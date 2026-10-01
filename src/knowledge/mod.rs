//! The per-guild knowledge base: documents uploaded on the web UI are reduced to their text,
//! split into chunks, embedded by every configured provider in the background and searched by
//! /talk. Optional: without EMBEDDING_PROVIDERS none of this runs.

pub mod chunk;
pub mod consult;
pub mod embed;
pub mod extract;
pub mod store;
pub mod worker;

use std::{cmp::Reverse, path::PathBuf, sync::Arc, time::Duration};

use ring::digest;
use tokio::{
    sync::{Mutex, Notify, Semaphore, SemaphorePermit, watch},
    task::JoinHandle,
};

use self::{
    embed::{Embedder, ProviderStatus},
    extract::ExtractError,
    store::{Coverage, Hit, Insert, KbSource, NewDocument, Quotas},
};
use crate::{agent::Excerpt, config::KbConfig, db::Database};

/// Each provider gets this long for a query vector before the next one is asked.
const QUERY_TIMEOUT: Duration = Duration::from_secs(8);
/// Nearest chunks fetched per search.
const SEARCH_LIMIT: u32 = 12;
/// Excerpts (runs of consecutive chunks) given to the AI, and their total length.
const MAX_EXCERPTS: usize = 5;
const MAX_EXCERPT_CHARS: usize = 8_000;
const MAX_TITLE_CHARS: usize = 200;
pub const MAX_FILE_NAME_CHARS: usize = 255;
/// Uploads received and extracted at once (each holds its file, up to KB_MAX_UPLOAD_BYTES, in
/// the bot's 256 MB). Two, so that one slow PDF does not hold up every other upload.
pub const UPLOAD_SLOTS: usize = 2;

pub struct Knowledge {
    pub config: KbConfig,
    pub db: Database,
    pub embedder: Embedder,
    /// Wakes the worker (a new or retried document).
    wake: Notify,
    upload_slots: Semaphore,
    /// One upload at a time does the quota check and insert (single process).
    uploads: Mutex<()>,
    /// The binary that runs `extract-pdf`; the running executable unless set (tests).
    pdf_program: Option<PathBuf>,
}

/// An uploaded file.
pub struct Upload<'a> {
    pub guild_id: u64,
    pub file_name: &'a str,
    pub bytes: &'a [u8],
    pub uploaded_by: u64,
    pub uploaded_by_name: &'a str,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum IngestError {
    #[error("{0}")]
    File(ExtractError),
    #[error("duplicate")]
    Duplicate { title: String },
    /// Carries the message for the user.
    #[error("quota_exceeded")]
    Quota(String),
    #[error("database")]
    Database,
}

impl IngestError {
    pub fn user_message(&self) -> String {
        match self {
            Self::File(error) => error.user_message(),
            Self::Duplicate { title } => {
                format!("同じ内容のファイルがすでに「{title}」として登録されています。")
            }
            Self::Quota(message) => message.clone(),
            Self::Database => {
                "データベースに保存できませんでした。時間をおいて再試行してください。".into()
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SearchOutcome {
    /// The guild has no searchable documents.
    NoDocuments,
    /// Possibly empty when no provider had vectors for the guild.
    Found(Vec<Excerpt>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum SearchError {
    #[error("database")]
    Database,
    /// No provider produced a query vector (rate limits, errors, timeouts).
    #[error("embedding_unavailable")]
    Unavailable,
}

impl Knowledge {
    pub fn new(config: KbConfig, db: Database) -> Result<Self, reqwest::Error> {
        Ok(Self {
            embedder: Embedder::new(&config.providers)?,
            config,
            db,
            wake: Notify::new(),
            upload_slots: Semaphore::new(UPLOAD_SLOTS),
            uploads: Mutex::new(()),
            pdf_program: None,
        })
    }

    /// For tests, whose own executable cannot run `extract-pdf`.
    pub fn with_pdf_program(mut self, program: PathBuf) -> Self {
        self.pdf_program = Some(program);
        self
    }

    pub fn quotas(&self) -> Quotas {
        Quotas {
            max_docs_per_guild: self.config.max_docs_per_guild,
            max_chunks_per_guild: self.config.max_chunks_per_guild,
            max_chunks_total: self.config.max_chunks_total,
        }
    }

    pub fn wake_worker(&self) {
        self.wake.notify_one();
    }

    pub fn spawn_worker(
        self: &Arc<Self>,
        timing: worker::Timing,
        stop: watch::Receiver<bool>,
    ) -> JoinHandle<()> {
        tokio::spawn(worker::run(self.clone(), timing, stop))
    }

    pub fn provider_statuses(&self) -> Vec<ProviderStatus> {
        self.embedder
            .providers
            .iter()
            .map(|provider| provider.status())
            .collect()
    }

    /// Checks, extracts and stores an upload, then wakes the worker. Returns the document ID.
    /// Only the extracted text is kept, never the file.
    pub async fn ingest(&self, upload: Upload<'_>) -> Result<u64, IngestError> {
        let kind = extract::detect(upload.file_name, upload.bytes).map_err(IngestError::File)?;
        let sha256: [u8; 32] = digest::digest(&digest::SHA256, upload.bytes)
            .as_ref()
            .try_into()
            .expect("SHA-256 digests are 32 bytes");
        // Cheap checks first, so a duplicate or a full guild does not wait for a PDF parse.
        // They are repeated in the insert's transaction.
        if let Some(title) = self
            .db
            .kb_duplicate(upload.guild_id, &sha256)
            .await
            .map_err(|_| IngestError::Database)?
        {
            return Err(IngestError::Duplicate { title });
        }
        let usage = self
            .db
            .kb_usage(upload.guild_id)
            .await
            .map_err(|_| IngestError::Database)?;
        if usage.documents >= u64::from(self.config.max_docs_per_guild) {
            return Err(self.quota_error(Insert::TooManyDocuments, 0));
        }
        let text = extract::extract(kind, upload.bytes, self.pdf_program.clone())
            .await
            .map_err(IngestError::File)?;
        let chunks = chunk::chunk(&text, kind.is_markdown()).len() as u32;
        if chunks == 0 {
            return Err(IngestError::File(ExtractError::Empty));
        }
        let title = title(upload.file_name);
        let file_name: String = upload.file_name.chars().take(MAX_FILE_NAME_CHARS).collect();
        let uploader: String = upload.uploaded_by_name.chars().take(128).collect();
        let document = NewDocument {
            guild_id: upload.guild_id,
            title: &title,
            file_name: &file_name,
            media_type: kind.as_str(),
            byte_size: upload.bytes.len() as u32,
            sha256,
            content: &text,
            char_count: text.chars().count() as u32,
            chunk_count: chunks,
            uploaded_by: upload.uploaded_by,
            uploaded_by_name: &uploader,
        };
        let inserted = {
            let _serialized = self.uploads.lock().await;
            self.db
                .kb_insert(&document, &self.quotas())
                .await
                .map_err(|_| IngestError::Database)?
        };
        match inserted {
            Insert::Created(id) => {
                tracing::info!(
                    guild_id = upload.guild_id,
                    document_id = id,
                    chunks,
                    "kb_document_uploaded"
                );
                self.wake_worker();
                Ok(id)
            }
            Insert::Duplicate { title } => Err(IngestError::Duplicate { title }),
            quota => Err(self.quota_error(quota, chunks)),
        }
    }

    fn quota_error(&self, insert: Insert, chunks: u32) -> IngestError {
        IngestError::Quota(match insert {
            Insert::TooManyDocuments => format!(
                "このサーバーに登録できる資料は{}件までです。不要な資料を削除してから登録してください。",
                self.config.max_docs_per_guild
            ),
            Insert::TooManyChunks => format!(
                "このサーバーの資料の量が上限（{}チャンク）を超えるため登録できません（この資料は{chunks}チャンクです）。不要な資料を削除するか、ファイルを分割してください。",
                self.config.max_chunks_per_guild
            ),
            _ => {
                "Bot 全体の資料の量が上限に達しているため登録できません。運営者に連絡してください。"
                    .into()
            }
        })
    }

    /// Excerpts of the guild's documents nearest to `query`, from the first provider in
    /// `search_order` that answers. Vectors of different models cannot be compared, so one
    /// provider's vectors are searched, preferably one that covers every ready document.
    pub async fn search(&self, guild: u64, query: &str) -> Result<SearchOutcome, SearchError> {
        let coverage = self
            .db
            .kb_coverage(guild)
            .await
            .map_err(|_| SearchError::Database)?;
        if coverage.chunks == 0 {
            return Ok(SearchOutcome::NoDocuments);
        }
        let keys: Vec<&str> = self
            .embedder
            .providers
            .iter()
            .map(|provider| provider.key())
            .collect();
        let order = search_order(&keys, &coverage);
        if order.is_empty() {
            // Ready documents whose vectors are all of providers no longer configured.
            return Ok(SearchOutcome::Found(Vec::new()));
        }
        let hits = self
            .embedder
            .search(
                query,
                &order,
                QUERY_TIMEOUT,
                |provider, vector| async move {
                    self.db
                        .kb_search(guild, &provider, &vector, SEARCH_LIMIT)
                        .await
                },
            )
            .await?;
        Ok(SearchOutcome::Found(excerpts(hits)))
    }

    /// Room for one more upload, if there is any. Each upload holds its whole file in memory
    /// while it is received and extracted.
    pub fn upload_slot(&self) -> Option<SemaphorePermit<'_>> {
        self.upload_slots.try_acquire().ok()
    }
}

/// The providers a search tries, as indices into `keys` (the configured order): those with a
/// vector for every chunk of the ready documents first, in the configured order, then those
/// with vectors for part of them, most first. Providers without any are left out.
pub fn search_order(keys: &[&str], coverage: &Coverage) -> Vec<usize> {
    let count = |index: usize| coverage.vectors.get(keys[index]).copied().unwrap_or(0);
    let mut order: Vec<usize> = (0..keys.len()).filter(|&index| count(index) > 0).collect();
    // A stable sort: ties keep the configured order.
    order.sort_by_key(|&index| (count(index) < coverage.chunks, Reverse(count(index))));
    order
}

/// A document's title: its file name without the extension.
pub fn title(file_name: &str) -> String {
    let stem = file_name
        .rsplit_once('.')
        .map_or(file_name, |(stem, _)| stem)
        .trim();
    let stem = if stem.is_empty() { file_name } else { stem };
    stem.chars().take(MAX_TITLE_CHARS).collect()
}

/// Joins neighbouring chunks of a document (under the same heading) into one excerpt, ranks the
/// excerpts by their best chunk and keeps the best `MAX_EXCERPTS`, `MAX_EXCERPT_CHARS`
/// characters in all.
pub fn excerpts(mut hits: Vec<Hit>) -> Vec<Excerpt> {
    struct Run {
        document_id: u64,
        title: String,
        heading: Option<String>,
        first_seq: u32,
        last_seq: u32,
        text: String,
        distance: f64,
    }
    hits.sort_by_key(|hit| (hit.document_id, hit.seq));
    let mut runs: Vec<Run> = Vec::new();
    for hit in hits {
        match runs.last_mut() {
            // An excerpt shows one heading, so sections stay apart.
            Some(run)
                if run.document_id == hit.document_id
                    && run.last_seq + 1 == hit.seq
                    && run.heading == hit.heading =>
            {
                run.text = chunk::join_overlapping(&run.text, &hit.content);
                run.last_seq = hit.seq;
                run.distance = run.distance.min(hit.distance);
            }
            _ => runs.push(Run {
                document_id: hit.document_id,
                title: hit.title,
                heading: hit.heading,
                first_seq: hit.seq,
                last_seq: hit.seq,
                text: hit.content,
                distance: hit.distance,
            }),
        }
    }
    runs.sort_by(|a, b| {
        a.distance
            .total_cmp(&b.distance)
            .then((a.document_id, a.first_seq).cmp(&(b.document_id, b.first_seq)))
    });
    let mut budget = MAX_EXCERPT_CHARS;
    let mut result = Vec::new();
    for run in runs.into_iter().take(MAX_EXCERPTS) {
        let mut text = match &run.heading {
            Some(heading) => format!("見出し: {heading}\n{}", run.text),
            None => run.text,
        };
        let length = text.chars().count();
        if length > budget {
            if budget < 200 {
                break;
            }
            text = text.chars().take(budget - 1).collect::<String>() + "…";
        }
        budget -= text.chars().count();
        result.push(Excerpt {
            document_id: run.document_id,
            title: run.title,
            text,
        });
    }
    result
}

/// The documents behind the excerpts, each once, in order.
pub fn sources(excerpts: &[Excerpt]) -> Vec<KbSource> {
    let mut sources: Vec<KbSource> = Vec::new();
    for excerpt in excerpts {
        if !sources
            .iter()
            .any(|source| source.id == excerpt.document_id)
        {
            sources.push(KbSource {
                id: excerpt.document_id,
                title: excerpt.title.clone(),
            });
        }
    }
    sources
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hit(document_id: u64, seq: u32, content: &str, distance: f64) -> Hit {
        Hit {
            document_id,
            title: format!("文書{document_id}"),
            seq,
            heading: (document_id == 2).then(|| "章 > 節".to_owned()),
            content: content.into(),
            distance,
        }
    }

    #[test]
    fn neighbouring_chunks_are_joined_and_ranked() {
        let excerpts = excerpts(vec![
            hit(1, 4, "後半の文章です。続きがあります。", 0.30),
            hit(2, 0, "見出し付きの文章。", 0.10),
            hit(1, 3, "前半の文章です。後半の文章です。", 0.20),
            hit(1, 9, "離れた部分。", 0.50),
        ]);
        let summary: Vec<(u64, &str)> = excerpts
            .iter()
            .map(|e| (e.document_id, e.text.as_str()))
            .collect();
        assert_eq!(
            summary,
            [
                (2, "見出し: 章 > 節\n見出し付きの文章。"),
                (1, "前半の文章です。後半の文章です。続きがあります。"),
                (1, "離れた部分。"),
            ]
        );
        assert_eq!(
            sources(&excerpts),
            [
                KbSource {
                    id: 2,
                    title: "文書2".into()
                },
                KbSource {
                    id: 1,
                    title: "文書1".into()
                }
            ]
        );
    }

    #[test]
    fn excerpts_are_limited_in_number_and_length() {
        let long = "あ".repeat(3_000);
        let hits = (0..8)
            .map(|i| hit(10 + i, 0, &long, f64::from(i as u32) / 10.0))
            .collect();
        let excerpts = excerpts(hits);
        assert_eq!(excerpts.len(), 3);
        let total: usize = excerpts.iter().map(|e| e.text.chars().count()).sum();
        assert_eq!(total, MAX_EXCERPT_CHARS);
        assert!(excerpts[2].text.ends_with('…'));
        let short: Vec<Hit> = (0..8).map(|i| hit(20 + i, 0, "短い", 0.1)).collect();
        assert_eq!(super::excerpts(short).len(), MAX_EXCERPTS);
    }

    #[test]
    fn neighbouring_chunks_of_different_sections_stay_apart() {
        let section = |seq: u32, heading: &str, content: &str| Hit {
            heading: Some(heading.to_owned()),
            ..hit(3, seq, content, 0.1)
        };
        let excerpts = excerpts(vec![
            section(7, "返品", "返品は7日以内です。"),
            section(8, "配送", "配送は3日かかります。"),
            section(9, "配送", "離島は5日かかります。"),
        ]);
        let texts: Vec<&str> = excerpts.iter().map(|e| e.text.as_str()).collect();
        assert_eq!(
            texts,
            [
                "見出し: 返品\n返品は7日以内です。",
                "見出し: 配送\n配送は3日かかります。\n離島は5日かかります。",
            ]
        );
    }

    #[test]
    fn providers_covering_every_ready_chunk_are_searched_first() {
        let keys = ["gemini:a", "openai:b", "ollama:c"];
        let coverage = |counts: &[(&str, u64)]| Coverage {
            chunks: 100,
            vectors: counts
                .iter()
                .map(|(key, count)| ((*key).to_owned(), *count))
                .collect(),
        };
        // Everything covered by everyone: the configured order.
        let full = coverage(&[("gemini:a", 100), ("openai:b", 100), ("ollama:c", 100)]);
        assert_eq!(search_order(&keys, &full), [0, 1, 2]);
        // The first provider covers part (its daily limit ran out): the complete one first.
        let partial = coverage(&[("gemini:a", 30), ("openai:b", 100), ("ollama:c", 60)]);
        assert_eq!(search_order(&keys, &partial), [1, 2, 0]);
        // Without vectors a provider is not asked at all; ties keep the configured order.
        let some = coverage(&[("gemini:a", 40), ("ollama:c", 40), ("old:model", 100)]);
        assert_eq!(search_order(&keys, &some), [0, 2]);
        assert!(search_order(&keys, &coverage(&[("old:model", 100)])).is_empty());
    }

    #[tokio::test]
    async fn uploads_are_limited_to_the_slots() {
        let db = Database {
            pool: sqlx::mysql::MySqlPoolOptions::new()
                .connect_lazy("mysql://nobody:nothing@127.0.0.1:1/none")
                .unwrap(),
        };
        let config = KbConfig {
            providers: Vec::new(),
            max_upload_bytes: 65_536,
            max_docs_per_guild: 1,
            max_chunks_per_guild: 1,
            max_chunks_total: 1,
        };
        let knowledge = Knowledge::new(config, db).unwrap();
        let held: Vec<_> = (0..UPLOAD_SLOTS)
            .map(|_| knowledge.upload_slot().expect("a free slot"))
            .collect();
        assert!(knowledge.upload_slot().is_none());
        drop(held);
        assert!(knowledge.upload_slot().is_some());
    }

    #[test]
    fn titles_come_from_file_names() {
        assert_eq!(title("議事録 2026-09.pdf"), "議事録 2026-09");
        assert_eq!(title("README"), "README");
        assert_eq!(title(".md"), ".md");
        assert_eq!(
            title(&format!("{}.txt", "長".repeat(300))).chars().count(),
            200
        );
    }
}
