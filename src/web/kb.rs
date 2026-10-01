//! The knowledge base API: list, upload, preview, retry and delete a guild's documents. Every
//! route needs the knowledge-manager right in the guild (`access::decide`), and answers 404
//! while the knowledge base is disabled.

use std::{collections::HashMap, sync::Arc, time::Duration};

use axum::{
    Json,
    body::Bytes,
    extract::{FromRequest, Path, Request, State},
    http::{HeaderMap, StatusCode, header},
};
use chrono::{DateTime, Utc};
use serde::Serialize;

use super::{ApiError, AppState, auth::Session, authz};
use crate::{
    ids::parse_snowflake,
    knowledge::{
        IngestError, Knowledge, MAX_FILE_NAME_CHARS, Upload,
        embed::EmbedError,
        extract::{self, ExtractError, MAX_PDF_BYTES, MAX_PDF_PAGES, MAX_TEXT_CHARS, MediaType},
        store::{DocumentRow, Retry, Status},
        worker::MAX_ATTEMPTS,
    },
    output::reorders_text,
};

/// Characters the preview shows (enough to spot garbled text).
const PREVIEW_CHARS: u32 = 2_000;
/// How long an upload may take to arrive. It holds an upload slot meanwhile, so a client that
/// sends slowly or stalls cannot keep one for ever.
const UPLOAD_READ_TIMEOUT: Duration = Duration::from_secs(120);

#[derive(Serialize)]
pub struct DocumentList {
    documents: Vec<DocumentView>,
    usage: UsageView,
    providers: Vec<ProviderView>,
    limits: LimitsView,
}

#[derive(Serialize)]
pub struct DocumentView {
    id: u64,
    title: String,
    file_name: String,
    /// `text`, `markdown` or `pdf`.
    kind: &'static str,
    byte_size: u32,
    char_count: u32,
    chunk_count: u32,
    /// `processing`, `ready` or `failed`.
    status: &'static str,
    attempts: u32,
    max_attempts: u32,
    error_code: Option<String>,
    /// Why it failed (or is retrying), for the user.
    error: Option<String>,
    uploaded_by: Option<String>,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
    /// Chunks with a vector, per configured provider (in priority order).
    progress: Vec<ProgressView>,
}

#[derive(Serialize)]
struct ProgressView {
    provider: String,
    embedded: u32,
}

#[derive(Serialize)]
struct UsageView {
    documents: u64,
    max_documents: u32,
    chunks: u64,
    max_chunks: u32,
    total_chunks: u64,
    max_total_chunks: u32,
}

#[derive(Serialize)]
struct ProviderView {
    key: String,
    label: String,
    /// `ok`, `rate_limited`, `auth_error` or `daily_limit`.
    state: &'static str,
    /// When a pause ends.
    until: Option<DateTime<Utc>>,
}

#[derive(Serialize)]
struct LimitsView {
    max_upload_bytes: usize,
    max_text_chars: usize,
    /// `.txt` and so on; `.pdf` only in a build with PDF support.
    extensions: Vec<String>,
    max_pdf_bytes: usize,
    max_pdf_pages: usize,
}

#[derive(Serialize)]
pub struct Preview {
    id: u64,
    title: String,
    text: String,
    char_count: u32,
    truncated: bool,
}

/// The knowledge base and the guild ID, if the session user may manage the guild's documents.
async fn manageable(
    state: &AppState,
    session: &Session,
    guild: &str,
) -> Result<(Arc<Knowledge>, u64), ApiError> {
    let knowledge = state.knowledge.clone().ok_or(ApiError::NotFound)?;
    let guild = parse_snowflake(guild).ok_or(ApiError::NotFound)?;
    let resolved = authz::resolve(state, session, guild).await?;
    if !resolved.access.manage_knowledge {
        return Err(ApiError::Forbidden);
    }
    Ok((knowledge, guild))
}

fn document_id(value: &str) -> Result<u64, ApiError> {
    parse_snowflake(value).ok_or(ApiError::NotFound)
}

fn database(_: sqlx::Error) -> ApiError {
    ApiError::Database
}

fn refused(status: StatusCode, code: &'static str, message: impl Into<String>) -> ApiError {
    ApiError::Refused {
        status,
        code,
        message: message.into(),
    }
}

pub async fn list(
    State(state): State<AppState>,
    session: Session,
    Path(guild): Path<String>,
) -> Result<Json<DocumentList>, ApiError> {
    let (knowledge, guild) = manageable(&state, &session, &guild).await?;
    let db = &knowledge.db;
    let documents = db.kb_documents(guild).await.map_err(database)?;
    let mut progress: HashMap<u64, HashMap<String, u32>> = HashMap::new();
    for (document, provider, count) in db.kb_progress(guild).await.map_err(database)? {
        progress
            .entry(document)
            .or_default()
            .insert(provider, count);
    }
    let usage = db.kb_usage(guild).await.map_err(database)?;
    let config = &knowledge.config;
    Ok(Json(DocumentList {
        documents: documents
            .iter()
            .map(|row| view(row, progress.get(&row.id), &knowledge))
            .collect(),
        usage: UsageView {
            documents: usage.documents,
            max_documents: config.max_docs_per_guild,
            chunks: usage.chunks,
            max_chunks: config.max_chunks_per_guild,
            total_chunks: usage.total_chunks,
            max_total_chunks: config.max_chunks_total,
        },
        providers: knowledge
            .provider_statuses()
            .into_iter()
            .map(|status| ProviderView {
                key: status.key,
                label: status.label,
                state: status.state.as_str(),
                until: status.until,
            })
            .collect(),
        limits: LimitsView {
            max_upload_bytes: config.max_upload_bytes,
            max_text_chars: MAX_TEXT_CHARS,
            extensions: extract::extensions()
                .map(|(extension, _)| format!(".{extension}"))
                .collect(),
            max_pdf_bytes: MAX_PDF_BYTES.min(config.max_upload_bytes),
            max_pdf_pages: MAX_PDF_PAGES,
        },
    }))
}

fn view(
    row: &DocumentRow,
    progress: Option<&HashMap<String, u32>>,
    knowledge: &Knowledge,
) -> DocumentView {
    DocumentView {
        id: row.id,
        title: row.title.clone(),
        file_name: row.file_name.clone(),
        kind: match MediaType::parse(&row.media_type) {
            Some(MediaType::Pdf) => "pdf",
            Some(MediaType::Markdown) => "markdown",
            _ => "text",
        },
        byte_size: row.byte_size,
        char_count: row.char_count,
        chunk_count: row.chunk_count,
        status: row.status.as_str(),
        attempts: row.attempts,
        max_attempts: MAX_ATTEMPTS,
        error: row
            .error_code
            .as_deref()
            .map(|code| error_message(row.status, code, row.attempts)),
        error_code: row.error_code.clone(),
        uploaded_by: row.uploaded_by_name.clone(),
        created_at: row.created_at,
        updated_at: row.updated_at,
        progress: knowledge
            .embedder
            .providers
            .iter()
            .map(|provider| ProgressView {
                provider: provider.label(),
                embedded: progress
                    .and_then(|counts| counts.get(provider.key()))
                    .copied()
                    .unwrap_or(0),
            })
            .collect(),
    }
}

/// The user's explanation of a document's error code (one the worker stores).
fn error_message(status: Status, code: &str, attempts: u32) -> String {
    let reason = [
        (
            EmbedError::Upstream.to_string(),
            "埋め込みプロバイダーとの通信に失敗しました",
        ),
        (
            EmbedError::Invalid.to_string(),
            "埋め込みプロバイダーから想定外の応答が返りました（モデルの設定を確認してください）",
        ),
        (
            EmbedError::BadInput.to_string(),
            "埋め込みプロバイダーが資料の内容を受け付けませんでした",
        ),
        (
            ExtractError::Empty.code().to_owned(),
            "本文を取り出せませんでした",
        ),
    ]
    .into_iter()
    .find_map(|(known, reason)| (known == code).then_some(reason))
    .unwrap_or("処理に失敗しました");
    match status {
        Status::Processing => {
            format!("{reason}。自動で再試行しています（失敗 {attempts}/{MAX_ATTEMPTS} 回）。")
        }
        _ => format!("{reason}。「再試行」で処理をやり直せます。"),
    }
}

/// `5 MiB`, `1.5 MiB`.
fn size_label(bytes: usize) -> String {
    let mib = bytes as f64 / (1024.0 * 1024.0);
    if bytes.is_multiple_of(1024 * 1024) {
        format!("{mib:.0} MiB")
    } else {
        format!("{mib:.1} MiB")
    }
}

/// The file as the request body; its name, percent-encoded, in `X-File-Name` (a custom header
/// a cross-site form cannot send). Responds 202: the document is processed in the background.
pub async fn upload(
    State(state): State<AppState>,
    session: Session,
    Path(guild): Path<String>,
    request: Request,
) -> Result<(StatusCode, Json<DocumentView>), ApiError> {
    let (knowledge, guild) = manageable(&state, &session, &guild).await?;
    let file_name = file_name(request.headers())?;
    let limit = knowledge.config.max_upload_bytes;
    let too_large = || {
        refused(
            StatusCode::PAYLOAD_TOO_LARGE,
            "payload_too_large",
            format!("ファイルは{}までです。", size_label(limit)),
        )
    };
    let declared = request
        .headers()
        .get(header::CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok()?.parse::<usize>().ok());
    if declared.is_some_and(|length| length > limit) {
        return Err(too_large());
    }
    // Held until the document is stored or refused: the file stays in memory until then.
    let Some(_slot) = knowledge.upload_slot() else {
        tracing::info!(guild_id = guild, "kb_upload_busy");
        return Err(refused(
            StatusCode::SERVICE_UNAVAILABLE,
            "upload_busy",
            "ほかの資料の登録を処理中です。しばらくしてから再試行してください。",
        ));
    };
    // The route's DefaultBodyLimit stops reading beyond the limit.
    let bytes = tokio::time::timeout(UPLOAD_READ_TIMEOUT, Bytes::from_request(request, &state))
        .await
        .map_err(|_| {
            ApiError::Invalid(
                "ファイルを時間内に受け取れませんでした。もう一度お試しください。".into(),
            )
        })?
        .map_err(|rejection| {
            if rejection.status() == StatusCode::PAYLOAD_TOO_LARGE {
                too_large()
            } else {
                ApiError::Invalid("ファイルを受け取れませんでした。もう一度お試しください。".into())
            }
        })?;
    if bytes.is_empty() {
        return Err(ApiError::Invalid("ファイルが空です。".into()));
    }
    let id = knowledge
        .ingest(Upload {
            guild_id: guild,
            file_name: &file_name,
            bytes: &bytes,
            uploaded_by: session.user_id,
            uploaded_by_name: &session.user_name,
        })
        .await
        .map_err(|error| {
            tracing::info!(guild_id = guild, error_code = %error, "kb_upload_rejected");
            ingest_error(error)
        })?;
    let row = knowledge
        .db
        .kb_document(guild, id)
        .await
        .map_err(database)?
        .ok_or(ApiError::Internal)?;
    Ok((StatusCode::ACCEPTED, Json(view(&row, None, &knowledge))))
}

fn ingest_error(error: IngestError) -> ApiError {
    let message = error.user_message();
    match error {
        IngestError::File(error) => {
            let status = match error {
                ExtractError::Unsupported | ExtractError::Mismatch => {
                    StatusCode::UNSUPPORTED_MEDIA_TYPE
                }
                ExtractError::PdfTooLarge => StatusCode::PAYLOAD_TOO_LARGE,
                ExtractError::PdfBusy | ExtractError::PdfUnavailable => {
                    StatusCode::SERVICE_UNAVAILABLE
                }
                _ => StatusCode::UNPROCESSABLE_ENTITY,
            };
            refused(status, error.code(), message)
        }
        IngestError::Duplicate { .. } => refused(StatusCode::CONFLICT, "duplicate", message),
        IngestError::Quota(_) => refused(StatusCode::CONFLICT, "quota_exceeded", message),
        IngestError::Database => ApiError::Database,
    }
}

/// The file's name from `X-File-Name` (percent-encoded UTF-8), without any directories. Names
/// with control or bidirectional formatting characters are refused: the title is shown in the
/// UI and in public /talk answers, where such characters could make it read as another name.
pub fn file_name(headers: &HeaderMap) -> Result<String, ApiError> {
    let mut values = headers.get_all("x-file-name").iter();
    let (Some(value), None) = (values.next(), values.next()) else {
        return Err(ApiError::Invalid(
            "ファイル名（X-File-Name ヘッダー）がありません。ページを再読み込みしてください。"
                .into(),
        ));
    };
    let invalid = || {
        ApiError::Invalid(format!(
            "ファイル名が正しくありません（{MAX_FILE_NAME_CHARS}文字まで）。"
        ))
    };
    let decoded = percent_decode(value.as_bytes()).ok_or_else(invalid)?;
    let name = decoded
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or_default()
        .trim();
    if name.is_empty()
        || name.chars().any(|c| c.is_control() || reorders_text(c))
        || name.chars().count() > MAX_FILE_NAME_CHARS
    {
        return Err(invalid());
    }
    Ok(name.to_owned())
}

/// `%XX` escapes to bytes, the result as UTF-8. `None` for broken escapes or invalid UTF-8.
fn percent_decode(raw: &[u8]) -> Option<String> {
    let mut bytes = Vec::with_capacity(raw.len());
    let mut i = 0;
    while i < raw.len() {
        if raw[i] == b'%' {
            let hex = raw.get(i + 1..i + 3)?;
            if !hex.iter().all(u8::is_ascii_hexdigit) {
                return None;
            }
            bytes.push(u8::from_str_radix(std::str::from_utf8(hex).ok()?, 16).ok()?);
            i += 3;
        } else {
            bytes.push(raw[i]);
            i += 1;
        }
    }
    String::from_utf8(bytes).ok()
}

pub async fn preview(
    State(state): State<AppState>,
    session: Session,
    Path((guild, id)): Path<(String, String)>,
) -> Result<Json<Preview>, ApiError> {
    let (knowledge, guild) = manageable(&state, &session, &guild).await?;
    let id = document_id(&id)?;
    let db = &knowledge.db;
    let row = db
        .kb_document(guild, id)
        .await
        .map_err(database)?
        .ok_or(ApiError::NotFound)?;
    let (text, char_count) = db
        .kb_preview(guild, id, PREVIEW_CHARS)
        .await
        .map_err(database)?
        .ok_or(ApiError::NotFound)?;
    Ok(Json(Preview {
        id,
        title: row.title,
        text,
        char_count,
        truncated: char_count > PREVIEW_CHARS,
    }))
}

/// Puts a failed document back into the queue.
pub async fn retry(
    State(state): State<AppState>,
    session: Session,
    Path((guild, id)): Path<(String, String)>,
) -> Result<Json<DocumentView>, ApiError> {
    let (knowledge, guild) = manageable(&state, &session, &guild).await?;
    let id = document_id(&id)?;
    match knowledge.db.kb_retry(guild, id).await.map_err(database)? {
        Retry::Restarted => {}
        Retry::NotFailed => {
            return Err(refused(
                StatusCode::CONFLICT,
                "not_failed",
                "再試行できるのは失敗した資料だけです。",
            ));
        }
        Retry::NotFound => return Err(ApiError::NotFound),
    }
    tracing::info!(guild_id = guild, document_id = id, "kb_document_retried");
    knowledge.wake_worker();
    let row = knowledge
        .db
        .kb_document(guild, id)
        .await
        .map_err(database)?
        .ok_or(ApiError::NotFound)?;
    Ok(Json(view(&row, None, &knowledge)))
}

/// Deletes a document with its chunks and vectors.
pub async fn remove(
    State(state): State<AppState>,
    session: Session,
    Path((guild, id)): Path<(String, String)>,
) -> Result<StatusCode, ApiError> {
    let (knowledge, guild) = manageable(&state, &session, &guild).await?;
    let id = document_id(&id)?;
    if !knowledge.db.kb_delete(guild, id).await.map_err(database)? {
        return Err(ApiError::NotFound);
    }
    tracing::info!(guild_id = guild, document_id = id, "kb_document_deleted");
    Ok(StatusCode::NO_CONTENT)
}

#[cfg(test)]
mod tests {
    use axum::http::HeaderValue;

    use super::*;

    fn named(values: &[&'static str]) -> Result<String, String> {
        let mut headers = HeaderMap::new();
        for value in values {
            headers.append("x-file-name", HeaderValue::from_static(value));
        }
        file_name(&headers).map_err(|error| error.to_string())
    }

    #[test]
    fn file_names_are_percent_decoded_without_directories() {
        assert_eq!(
            named(&["%E8%B3%87%E6%96%99%20v2.pdf"]),
            Ok("資料 v2.pdf".into())
        );
        assert_eq!(named(&["a%2Fb%5Cc.txt"]), Ok("c.txt".into()));
        assert_eq!(named(&["plain.md"]), Ok("plain.md".into()));
        for bad in [
            &[][..],
            &["a.txt", "b.txt"],
            &["%E8%B3"],
            &["%zz.txt"],
            &["%+f.txt"],
            &["dir%2F"],
            &["a%0Ab.txt"],
            // Right-to-left override and a directional isolate.
            &["invoice%E2%80%AEtxt.pdf"],
            &["%E2%81%A7a.txt"],
        ] {
            assert_eq!(named(bad), Err("invalid_request".into()), "{bad:?}");
        }
        let long = "a".repeat(252) + ".txt";
        let mut headers = HeaderMap::new();
        headers.insert("x-file-name", HeaderValue::from_str(&long).unwrap());
        assert!(file_name(&headers).is_err());
    }

    #[test]
    fn sizes_and_errors_read_well() {
        assert_eq!(size_label(5 * 1024 * 1024), "5 MiB");
        assert_eq!(size_label(1024 * 1024 + 512 * 1024), "1.5 MiB");
        assert!(error_message(Status::Processing, "embedding_upstream", 2).contains("2/5"));
        assert!(error_message(Status::Failed, "embedding_upstream", 5).contains("再試行"));
        // Every code the worker stores has its own reason.
        let generic = error_message(Status::Failed, "unknown", 0);
        for code in [
            EmbedError::Upstream.to_string(),
            EmbedError::Invalid.to_string(),
            EmbedError::BadInput.to_string(),
            ExtractError::Empty.code().to_owned(),
        ] {
            assert_ne!(error_message(Status::Failed, &code, 0), generic, "{code}");
        }
    }
}
