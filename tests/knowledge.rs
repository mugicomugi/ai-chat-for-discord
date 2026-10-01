//! Knowledge base tests. Without a database: PDF extraction through the bot binary's
//! `extract-pdf` child process. The `#[ignore]` tests need compose.test.yaml and
//! TEST_DATABASE_URL: vector search with bound VECTOR bytes, the choice of the search provider,
//! quotas, ingestion, the background worker against wiremock providers and pruning old
//! vectors. Each uses guild and user IDs of its own, and they take turns (`SERIAL`).

use std::{
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};

use chrono::Utc;
use discord_discussion_bot::{
    config::{KbConfig, Pacing, ProviderConfig, ProviderKind},
    db::{Database, NewRun},
    knowledge::{
        IngestError, Knowledge, SearchError, SearchOutcome, Upload, chunk,
        embed::{DIMENSIONS, unit_vector, vector_bytes},
        extract::{self, ExtractError},
        store::{Insert, KbSource, NewDocument, Quotas, Retry},
        worker::{MAX_ATTEMPTS, Timing},
    },
    ops::prune_embeddings,
};
use serde_json::{Value, json};
use sqlx::Row;
use tokio::{
    sync::{Mutex, watch},
    task::JoinHandle,
};
use wiremock::{
    Mock, MockServer, Request, ResponseTemplate,
    matchers::{path, path_regex},
};

mod common;
use common::database;

/// The worker processes and backfills the documents of every guild, and pruning looks at every
/// stored vector, so the database tests of this file run one at a time even when cargo runs
/// them in parallel.
static SERIAL: Mutex<()> = Mutex::const_new(());

/// A user ID of this file's range (the IDs of tests/database.rs are not used elsewhere).
const UPLOADER: u64 = 960_099;

fn bot() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_discord-discussion-bot"))
}

/// A minimal PDF with one page per text, set in Helvetica (a standard font every reader has).
#[cfg(feature = "pdf")]
fn pdf(pages: &[&str]) -> Vec<u8> {
    let count = pages.len();
    let kids: Vec<String> = (0..count).map(|i| format!("{} 0 R", 4 + 2 * i)).collect();
    let mut objects = vec![
        "<< /Type /Catalog /Pages 2 0 R >>".to_owned(),
        format!(
            "<< /Type /Pages /Kids [{}] /Count {count} >>",
            kids.join(" ")
        ),
        "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".to_owned(),
    ];
    for (i, text) in pages.iter().enumerate() {
        let content = if text.is_empty() {
            String::new()
        } else {
            format!("BT /F1 18 Tf 72 700 Td ({text}) Tj ET")
        };
        objects.push(format!(
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Resources << /Font << /F1 3 0 R >> >> /Contents {} 0 R >>",
            5 + 2 * i
        ));
        objects.push(format!(
            "<< /Length {} >>\nstream\n{content}\nendstream",
            content.len()
        ));
    }
    let mut out = b"%PDF-1.4\n".to_vec();
    let mut offsets = Vec::new();
    for (i, object) in objects.iter().enumerate() {
        offsets.push(out.len());
        out.extend_from_slice(format!("{} 0 obj\n{object}\nendobj\n", i + 1).as_bytes());
    }
    let xref = out.len();
    out.extend_from_slice(
        format!("xref\n0 {}\n0000000000 65535 f \n", objects.len() + 1).as_bytes(),
    );
    for offset in offsets {
        out.extend_from_slice(format!("{offset:010} 00000 n \n").as_bytes());
    }
    out.extend_from_slice(
        format!(
            "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n",
            objects.len() + 1
        )
        .as_bytes(),
    );
    out
}

#[cfg(feature = "pdf")]
#[tokio::test]
async fn pdf_text_is_extracted_in_a_child_process() {
    use extract::MediaType;
    let bytes = pdf(&["Hello knowledge base", "Second page text"]);
    assert_eq!(extract::detect("a.pdf", &bytes), Ok(MediaType::Pdf));
    let text = extract::extract(MediaType::Pdf, &bytes, Some(bot()))
        .await
        .unwrap();
    assert!(text.contains("Hello knowledge base"), "{text:?}");
    assert!(text.contains("Second page text"), "{text:?}");

    // A page without text (a scan, say) has nothing to extract.
    assert_eq!(
        extract::extract(MediaType::Pdf, &pdf(&[""]), Some(bot())).await,
        Err(ExtractError::Empty)
    );
    assert_eq!(
        extract::extract(MediaType::Pdf, b"%PDF-1.4\nnot really a pdf", Some(bot())).await,
        Err(ExtractError::PdfInvalid)
    );
    let pages = vec!["x"; extract::MAX_PDF_PAGES + 1];
    assert_eq!(
        extract::extract(MediaType::Pdf, &pdf(&pages), Some(bot())).await,
        Err(ExtractError::PdfTooManyPages)
    );
    assert!(
        extract::extract(
            MediaType::Pdf,
            &pdf(&vec!["x"; extract::MAX_PDF_PAGES]),
            Some(bot())
        )
        .await
        .is_ok()
    );
}

// ---- Database tests (compose.test.yaml) ----

fn provider(kind: ProviderKind, model: &str, server: &str) -> ProviderConfig {
    let mut config = ProviderConfig::new(kind, model, "test-key", server);
    // Tests do not wait for pacing.
    config.pacing = Pacing {
        requests_per_minute: 1_000_000,
        tokens_per_minute: 100_000_000,
        requests_per_day: 0,
    };
    config
}

fn kb_config(providers: Vec<ProviderConfig>) -> KbConfig {
    KbConfig {
        providers,
        max_upload_bytes: 5 * 1024 * 1024,
        max_docs_per_guild: 50,
        max_chunks_per_guild: 5_000,
        max_chunks_total: 10_000_000,
    }
}

const QUOTAS: Quotas = Quotas {
    max_docs_per_guild: 1_000,
    max_chunks_per_guild: 1_000_000,
    max_chunks_total: 100_000_000,
};

async fn clear_guilds(db: &Database, guilds: &[u64]) {
    for guild in guilds {
        sqlx::query("DELETE FROM kb_documents WHERE guild_id=?")
            .bind(guild)
            .execute(&db.pool)
            .await
            .unwrap();
    }
}

fn sha(name: &str) -> [u8; 32] {
    let mut sha = [0_u8; 32];
    for (i, byte) in name.bytes().enumerate() {
        sha[i % 32] ^= byte.wrapping_add(i as u8);
    }
    sha[31] ^= name.len() as u8;
    sha
}

fn new_document<'a>(guild: u64, name: &'a str, content: &'a str, chunks: u32) -> NewDocument<'a> {
    NewDocument {
        guild_id: guild,
        title: name,
        file_name: name,
        media_type: "text/plain",
        byte_size: content.len() as u32,
        sha256: sha(name),
        content,
        char_count: content.chars().count() as u32,
        chunk_count: chunks,
        uploaded_by: UPLOADER,
        uploaded_by_name: "テスト",
    }
}

/// A document with the given chunks, still processing. Returns its ID and chunk IDs.
async fn document(db: &Database, guild: u64, name: &str, chunks: &[&str]) -> (u64, Vec<u64>) {
    let content = chunks.join("\n\n");
    let Insert::Created(id) = db
        .kb_insert(
            &new_document(guild, name, &content, chunks.len() as u32),
            &QUOTAS,
        )
        .await
        .unwrap()
    else {
        panic!("not created");
    };
    let pieces: Vec<chunk::Chunk> = chunks
        .iter()
        .map(|content| chunk::Chunk {
            heading: Some("見出し".into()),
            content: (*content).into(),
        })
        .collect();
    assert!(db.kb_replace_chunks(id, guild, &pieces).await.unwrap());
    let ids = sqlx::query_scalar("SELECT id FROM kb_chunks WHERE document_id=? ORDER BY seq")
        .bind(id)
        .fetch_all(&db.pool)
        .await
        .unwrap();
    (id, ids)
}

/// A unit vector from a few nonzero components.
fn vector(components: &[(usize, f64)]) -> Vec<u8> {
    let mut values = vec![0.0; DIMENSIONS];
    for (index, value) in components {
        values[*index] = *value;
    }
    vector_bytes(&unit_vector(&values).unwrap())
}

async fn count(db: &Database, sql: &str, id: u64) -> i64 {
    sqlx::query_scalar(sql)
        .bind(id)
        .fetch_one(&db.pool)
        .await
        .unwrap()
}

#[tokio::test]
#[ignore = "requires compose.test.yaml and TEST_DATABASE_URL"]
async fn knowledge_vectors() {
    let _serial = SERIAL.lock().await;
    let db = database().await;
    let (guild, other, fixture) = (960_001_u64, 960_002_u64, 960_003_u64);
    clear_guilds(&db, &[guild, other, fixture]).await;
    let (a, chunks) = document(&db, guild, "a", &["チャンク0", "チャンク1", "チャンク2"]).await;
    let x = vector(&[(0, 1.0)]);
    let y = vector(&[(1, 1.0)]);
    let xy = vector(&[(0, 1.0), (1, 1.0)]);
    assert!(
        db.kb_store_vectors(
            guild,
            "test:a",
            &[
                (chunks[0], x.clone()),
                (chunks[1], y.clone()),
                (chunks[2], xy)
            ]
        )
        .await
        .unwrap()
    );
    assert!(
        db.kb_store_vectors(guild, "test:b", &[(chunks[0], y.clone())])
            .await
            .unwrap()
    );
    // Not searchable while processing.
    assert!(
        db.kb_search(guild, "test:a", &x, 12)
            .await
            .unwrap()
            .is_empty()
    );
    assert!(!db.kb_has_ready(guild).await.unwrap());
    assert!(db.kb_mark_ready(a).await.unwrap());
    assert!(!db.kb_mark_ready(a).await.unwrap());
    assert!(db.kb_has_ready(guild).await.unwrap());

    // A processing document of the guild and a ready one of another guild, both nearest.
    let (_, pending) = document(&db, guild, "b", &["処理中"]).await;
    db.kb_store_vectors(guild, "test:a", &[(pending[0], x.clone())])
        .await
        .unwrap();
    let (c, foreign) = document(&db, other, "c", &["他のサーバー"]).await;
    db.kb_store_vectors(other, "test:a", &[(foreign[0], x.clone())])
        .await
        .unwrap();
    db.kb_mark_ready(c).await.unwrap();

    // What each provider covers of the guild's ready documents (not of processing ones).
    let coverage = db.kb_coverage(guild).await.unwrap();
    assert_eq!(coverage.chunks, 3);
    assert_eq!(coverage.vectors.len(), 2);
    assert_eq!(coverage.vectors["test:a"], 3);
    assert_eq!(coverage.vectors["test:b"], 1);
    assert_eq!(db.kb_coverage(other).await.unwrap().chunks, 1);

    // Exact cosine order with the query bound as VECTOR bytes.
    let hits = db.kb_search(guild, "test:a", &x, 12).await.unwrap();
    let found: Vec<(u64, u32)> = hits.iter().map(|hit| (hit.document_id, hit.seq)).collect();
    assert_eq!(found, [(a, 0), (a, 2), (a, 1)]);
    let distances: Vec<f64> = hits.iter().map(|hit| hit.distance).collect();
    assert!(distances[0].abs() < 1e-6, "{distances:?}");
    assert!(
        (distances[1] - (1.0 - 0.5_f64.sqrt())).abs() < 1e-5,
        "{distances:?}"
    );
    assert!((distances[2] - 1.0).abs() < 1e-6, "{distances:?}");
    assert_eq!(hits[0].title, "a");
    assert_eq!(hits[0].heading.as_deref(), Some("見出し"));
    assert_eq!(hits[0].content, "チャンク0");
    assert_eq!(db.kb_search(guild, "test:a", &x, 1).await.unwrap().len(), 1);
    // Providers are separate spaces.
    let hits = db.kb_search(guild, "test:b", &x, 12).await.unwrap();
    assert_eq!(hits.len(), 1);
    assert!((hits[0].distance - 1.0).abs() < 1e-6);
    assert!(
        db.kb_search(guild, "test:none", &x, 12)
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        db.kb_search(other, "test:a", &x, 12).await.unwrap()[0].document_id,
        c
    );

    // Storing again replaces the vector; MariaDB itself refuses a wrong length.
    db.kb_store_vectors(guild, "test:b", &[(chunks[0], x.clone())])
        .await
        .unwrap();
    assert!(
        db.kb_search(guild, "test:b", &x, 12).await.unwrap()[0]
            .distance
            .abs()
            < 1e-6
    );
    assert!(
        db.kb_store_vectors(
            guild,
            "test:a",
            &[(chunks[0], vec![0; (DIMENSIONS - 1) * 4])]
        )
        .await
        .is_err()
    );
    // A chunk that no longer exists stores nothing.
    assert!(
        !db.kb_store_vectors(guild, "test:a", &[(u64::MAX, x.clone())])
            .await
            .unwrap()
    );

    let mut progress = db.kb_progress(guild).await.unwrap();
    progress.sort();
    let pending_id = progress.iter().find(|(id, _, _)| *id != a).unwrap().0;
    assert_eq!(
        progress,
        [
            (a, "test:a".to_owned(), 3),
            (a, "test:b".to_owned(), 1),
            (pending_id, "test:a".to_owned(), 1)
        ]
    );

    // Deleting a document takes its chunks and vectors with it, and only in its own guild.
    assert!(!db.kb_delete(other, a).await.unwrap());
    assert!(db.kb_delete(guild, a).await.unwrap());
    assert_eq!(
        count(&db, "SELECT COUNT(*) FROM kb_chunks WHERE document_id=?", a).await,
        0
    );
    for chunk in &chunks {
        assert_eq!(
            count(
                &db,
                "SELECT COUNT(*) FROM kb_embeddings WHERE chunk_id=?",
                *chunk
            )
            .await,
            0
        );
    }
    assert!(
        db.kb_search(guild, "test:a", &x, 12)
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        db.kb_vector_counts()
            .await
            .unwrap()
            .iter()
            .filter(|(key, _)| key == "test:b")
            .count(),
        0
    );

    // What /talk records in talk_runs.
    let run = 960_500_u64;
    sqlx::query("DELETE FROM talk_runs WHERE interaction_id=?")
        .bind(run)
        .execute(&db.pool)
        .await
        .unwrap();
    db.begin(&NewRun {
        interaction_id: run,
        guild_id: guild,
        channel_id: 1,
        user_id: 2,
        user_name: "u",
        question: "q",
        web_search: false,
        history_seconds: 0,
        invoked_at: Utc::now(),
    })
    .await
    .unwrap();
    let stored = |db: Database| async move {
        let row = sqlx::query("SELECT knowledge,kb_sources FROM talk_runs WHERE interaction_id=?")
            .bind(run)
            .fetch_one(&db.pool)
            .await
            .unwrap();
        (
            row.get::<bool, _>("knowledge"),
            row.get::<Option<sqlx::types::Json<Vec<KbSource>>>, _>("kb_sources")
                .map(|json| json.0),
        )
    };
    assert_eq!(stored(db.clone()).await, (false, None));
    let sources = [KbSource {
        id: 7,
        title: "設計書😀".into(),
    }];
    db.record_knowledge(run, Some(&sources)).await.unwrap();
    assert_eq!(stored(db.clone()).await, (true, Some(sources.to_vec())));
    db.record_knowledge(run, Some(&[])).await.unwrap();
    assert_eq!(stored(db.clone()).await, (true, Some(vec![])));
    sqlx::query("DELETE FROM talk_runs WHERE interaction_id=?")
        .bind(run)
        .execute(&db.pool)
        .await
        .unwrap();

    // Left in place for scripts/check-vector-dump.sh (cleared by the next run of this test).
    let (kept, kept_chunks) = document(&db, fixture, "dump-fixture", &["保存", "復元"]).await;
    db.kb_store_vectors(
        fixture,
        "test:dump",
        &[
            (kept_chunks[0], vector(&[(0, 0.25), (767, -1.5)])),
            (kept_chunks[1], vector(&[(3, 1e-3), (400, 7.0)])),
        ],
    )
    .await
    .unwrap();
    db.kb_mark_ready(kept).await.unwrap();
    clear_guilds(&db, &[guild, other]).await;
}

#[tokio::test]
#[ignore = "requires compose.test.yaml and TEST_DATABASE_URL"]
async fn knowledge_quotas() {
    let _serial = SERIAL.lock().await;
    let db = database().await;
    let (guild, second, concurrent) = (961_001_u64, 961_002_u64, 961_003_u64);
    clear_guilds(&db, &[guild, second, concurrent]).await;
    let base = db.kb_usage(guild).await.unwrap().total_chunks;
    let quotas = Quotas {
        max_docs_per_guild: 2,
        max_chunks_per_guild: 10,
        max_chunks_total: u32::try_from(base).unwrap() + 15,
    };
    let insert = |guild, name: &'static str, chunks| {
        let db = db.clone();
        async move {
            db.kb_insert(&new_document(guild, name, "本文", chunks), &quotas)
                .await
                .unwrap()
        }
    };
    assert!(matches!(insert(guild, "one", 4).await, Insert::Created(_)));
    // The same content again in the same guild.
    assert_eq!(
        db.kb_insert(
            &NewDocument {
                title: "別名",
                ..new_document(guild, "one", "本文", 1)
            },
            &quotas
        )
        .await
        .unwrap(),
        Insert::Duplicate {
            title: "one".into()
        }
    );
    assert_eq!(insert(guild, "two", 7).await, Insert::TooManyChunks);
    assert!(matches!(insert(guild, "two", 6).await, Insert::Created(_)));
    assert_eq!(insert(guild, "three", 1).await, Insert::TooManyDocuments);
    let usage = db.kb_usage(guild).await.unwrap();
    assert_eq!((usage.documents, usage.chunks), (2, 10));
    // Another guild: the same content is fine, the total over all guilds is not.
    assert_eq!(insert(second, "big", 6).await, Insert::TooManyChunksTotal);
    assert!(matches!(insert(second, "one", 5).await, Insert::Created(_)));

    // Concurrent uploads cannot pass the document limit together.
    let knowledge = Arc::new(
        Knowledge::new(
            KbConfig {
                max_docs_per_guild: 2,
                ..kb_config(vec![provider(
                    ProviderKind::Gemini,
                    "quota-test",
                    "http://127.0.0.1:1",
                )])
            },
            db.clone(),
        )
        .unwrap(),
    );
    let mut uploads = tokio::task::JoinSet::new();
    for i in 0..6 {
        let knowledge = knowledge.clone();
        uploads.spawn(async move {
            let (name, text) = (
                format!("同時{i}.txt"),
                format!("同時にアップロードした資料{i}"),
            );
            knowledge
                .ingest(Upload {
                    guild_id: concurrent,
                    file_name: &name,
                    bytes: text.as_bytes(),
                    uploaded_by: 1,
                    uploaded_by_name: "u",
                })
                .await
        });
    }
    let results = uploads.join_all().await;
    assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 2);
    assert!(
        results
            .iter()
            .filter_map(|result| result.as_ref().err())
            .all(
                |error| matches!(error, IngestError::Quota(message) if message.contains("2件まで"))
            )
    );
    clear_guilds(&db, &[guild, second, concurrent]).await;
}

#[tokio::test]
#[ignore = "requires compose.test.yaml and TEST_DATABASE_URL"]
async fn knowledge_ingest() {
    let _serial = SERIAL.lock().await;
    let db = database().await;
    let guild = 962_001_u64;
    clear_guilds(&db, &[guild]).await;
    let knowledge = Knowledge::new(
        kb_config(vec![provider(
            ProviderKind::Gemini,
            "ingest-test",
            "http://127.0.0.1:1",
        )]),
        db.clone(),
    )
    .unwrap()
    .with_pdf_program(bot());
    let upload = |file_name: &'static str, bytes: Vec<u8>| {
        let knowledge = &knowledge;
        async move {
            knowledge
                .ingest(Upload {
                    guild_id: guild,
                    file_name,
                    bytes: &bytes,
                    uploaded_by: 42,
                    uploaded_by_name: "管理者",
                })
                .await
        }
    };
    let markdown = format!(
        "\u{FEFF}# 手順\r\n\r\n{}\r\n\r\n## 補足\r\n\r\n{}",
        "準備をします。".repeat(120),
        "注意点です。".repeat(10)
    );
    let id = upload("手順書.md", markdown.clone().into_bytes())
        .await
        .unwrap();
    let row = db.kb_document(guild, id).await.unwrap().unwrap();
    let text = extract::normalize(&markdown);
    assert_eq!(row.title, "手順書");
    assert_eq!(row.file_name, "手順書.md");
    assert_eq!(row.media_type, "text/markdown");
    assert_eq!(row.byte_size as usize, markdown.len());
    assert_eq!(row.char_count as usize, text.chars().count());
    assert_eq!(row.chunk_count as usize, chunk::chunk(&text, true).len());
    assert_eq!(row.status.as_str(), "processing");
    assert_eq!(row.uploaded_by_name.as_deref(), Some("管理者"));
    let (preview, length) = db.kb_preview(guild, id, 10).await.unwrap().unwrap();
    assert_eq!(preview, "# 手順\n\n準備をし");
    assert_eq!(length as usize, text.chars().count());

    // The same bytes again; a different name does not matter.
    assert_eq!(
        upload("copy.md", markdown.into_bytes()).await,
        Err(IngestError::Duplicate {
            title: "手順書".into()
        })
    );
    // Only the extracted text is kept, never the PDF itself.
    #[cfg(feature = "pdf")]
    {
        let id = upload("report.pdf", pdf(&["Quarterly report text"]))
            .await
            .unwrap();
        let (preview, _) = db.kb_preview(guild, id, 2_000).await.unwrap().unwrap();
        assert!(preview.contains("Quarterly report text") && !preview.contains("%PDF"));
        let stored: String = sqlx::query_scalar("SELECT content FROM kb_documents WHERE id=?")
            .bind(id)
            .fetch_one(&db.pool)
            .await
            .unwrap();
        assert!(!stored.contains("endstream"));
    }
    for (name, bytes, expected) in [
        (
            "garbled.txt",
            "\u{FFFD}".repeat(10).into_bytes(),
            ExtractError::Garbled,
        ),
        (
            "image.png",
            vec![0x89, b'P', b'N', b'G'],
            ExtractError::Unsupported,
        ),
        (
            "renamed.pdf",
            b"plain text".to_vec(),
            if cfg!(feature = "pdf") {
                ExtractError::Mismatch
            } else {
                ExtractError::Unsupported
            },
        ),
        (
            "sjis.txt",
            vec![0x93, 0xfa, 0x96, 0x7b],
            ExtractError::NotUtf8,
        ),
    ] {
        assert_eq!(
            upload(name, bytes).await,
            Err(IngestError::File(expected)),
            "{name}"
        );
    }
    assert_eq!(
        db.kb_usage(guild).await.unwrap().documents,
        1 + u64::from(cfg!(feature = "pdf"))
    );
    clear_guilds(&db, &[guild]).await;
}

// ---- The worker ----

const TIMING: Timing = Timing {
    idle: Duration::from_millis(200),
    backfill_every: Duration::from_secs(3600),
    retry_base: Duration::from_millis(50),
};

fn values(count: usize) -> Vec<Vec<f64>> {
    (0..count)
        .map(|i| {
            let mut values = vec![0.1; DIMENSIONS];
            values[i % DIMENSIONS] = 1.0;
            values
        })
        .collect()
}

/// Gemini's answer with one vector per text of the request.
fn gemini_vectors(request: &Request) -> ResponseTemplate {
    let body: Value = request.body_json().unwrap();
    let count = body["requests"].as_array().unwrap().len();
    let embeddings: Vec<Value> = values(count)
        .into_iter()
        .map(|values| json!({"values": values}))
        .collect();
    ResponseTemplate::new(200).set_body_json(json!({"embeddings": embeddings}))
}

fn openai_vectors(request: &Request) -> ResponseTemplate {
    let body: Value = request.body_json().unwrap();
    let count = body["input"].as_array().unwrap().len();
    let data: Vec<Value> = values(count)
        .into_iter()
        .enumerate()
        .map(|(index, embedding)| json!({"index": index, "embedding": embedding}))
        .collect();
    ResponseTemplate::new(200).set_body_json(json!({"data": data}))
}

const GEMINI_PATH: &str = r"^/v1beta/models/[a-z-]+:batchEmbedContents$";

/// A unit vector along one axis, as a provider returns it.
fn axis(index: usize) -> Vec<f64> {
    let mut values = vec![0.0; DIMENSIONS];
    values[index] = 1.0;
    values
}

/// Gemini's 429 for a used-up daily quota: the provider pauses for at least an hour.
fn gemini_daily_quota() -> ResponseTemplate {
    ResponseTemplate::new(429).set_body_json(json!({"error": {
        "code": 429,
        "status": "RESOURCE_EXHAUSTED",
        "details": [{
            "@type": "type.googleapis.com/google.rpc.QuotaFailure",
            "violations": [{"quotaId": "EmbedContentRequestsPerDayPerProjectPerModel-FreeTier"}]
        }]
    }}))
}

struct Running {
    stop: watch::Sender<bool>,
    handle: JoinHandle<()>,
}

fn start(knowledge: &Arc<Knowledge>) -> Running {
    let (stop, stopped) = watch::channel(false);
    Running {
        handle: knowledge.spawn_worker(TIMING, stopped),
        stop,
    }
}

impl Running {
    async fn halt(self) {
        self.stop.send(true).unwrap();
        tokio::time::timeout(Duration::from_secs(10), self.handle)
            .await
            .expect("the worker stops promptly")
            .unwrap();
    }
}

async fn wait_until<F, Fut>(what: &str, mut condition: F)
where
    F: FnMut() -> Fut,
    Fut: Future<Output = bool>,
{
    for _ in 0..300 {
        if condition().await {
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("timed out waiting until {what}");
}

async fn status(db: &Database, document: u64) -> (String, u32, Option<String>) {
    let row = sqlx::query("SELECT status,attempts,error_code FROM kb_documents WHERE id=?")
        .bind(document)
        .fetch_one(&db.pool)
        .await
        .unwrap();
    (
        row.get("status"),
        row.get("attempts"),
        row.get("error_code"),
    )
}

async fn vectors(db: &Database, document: u64, provider: &str) -> i64 {
    sqlx::query_scalar("SELECT COUNT(*) FROM kb_embeddings e JOIN kb_chunks c ON c.id=e.chunk_id WHERE c.document_id=? AND e.provider=?")
        .bind(document)
        .bind(provider)
        .fetch_one(&db.pool)
        .await
        .unwrap()
}

/// Vectors these tests stored for other tests' ready documents through backfill.
async fn clear_worker_vectors(db: &Database) {
    sqlx::query("DELETE FROM kb_embeddings WHERE provider LIKE '%:worker-%'")
        .execute(&db.pool)
        .await
        .unwrap();
}

async fn upload_text(knowledge: &Knowledge, guild: u64, name: &str, sentences: usize) -> u64 {
    let text: String = (0..sentences)
        .map(|i| format!("{name}の{i}番目の文です。資料の処理を確かめます。"))
        .collect();
    knowledge
        .ingest(Upload {
            guild_id: guild,
            file_name: &format!("{name}.txt"),
            bytes: text.as_bytes(),
            uploaded_by: 1,
            uploaded_by_name: "u",
        })
        .await
        .unwrap()
}

#[tokio::test]
#[ignore = "requires compose.test.yaml and TEST_DATABASE_URL"]
async fn knowledge_worker_failover_and_backfill() {
    let _serial = SERIAL.lock().await;
    let db = database().await;
    let guild = 963_001_u64;
    clear_guilds(&db, &[guild]).await;
    clear_worker_vectors(&db).await;
    let (gemini, openai) = (MockServer::start().await, MockServer::start().await);
    // The first provider is rate-limited (Gemini's free tier, say); the second works.
    let limited = Arc::new(AtomicBool::new(true));
    Mock::given(path_regex(GEMINI_PATH))
        .respond_with({
            let limited = limited.clone();
            move |request: &Request| {
                if limited.load(Ordering::SeqCst) {
                    ResponseTemplate::new(429)
                        .insert_header("retry-after", "1")
                        .set_body_json(
                            json!({"error": {"code": 429, "status": "RESOURCE_EXHAUSTED"}}),
                        )
                } else {
                    gemini_vectors(request)
                }
            }
        })
        .mount(&gemini)
        .await;
    Mock::given(path("/v1/embeddings"))
        .respond_with(openai_vectors)
        .mount(&openai)
        .await;
    let knowledge = Arc::new(
        Knowledge::new(
            kb_config(vec![
                provider(ProviderKind::Gemini, "worker-a", &gemini.uri()),
                provider(ProviderKind::OpenAi, "worker-b", &openai.uri()),
            ]),
            db.clone(),
        )
        .unwrap(),
    );
    let document = upload_text(&knowledge, guild, "切り替え", 60).await;
    let chunks = i64::from(
        db.kb_document(guild, document)
            .await
            .unwrap()
            .unwrap()
            .chunk_count,
    );
    assert!(chunks >= 3);
    let worker = start(&knowledge);
    wait_until("the second provider made the document ready", || async {
        status(&db, document).await.0 == "ready"
    })
    .await;
    // The rate limit was never counted against the document.
    assert_eq!(status(&db, document).await, ("ready".into(), 0, None));
    assert_eq!(vectors(&db, document, "openai:worker-b").await, chunks);
    assert_eq!(vectors(&db, document, "gemini:worker-a").await, 0);
    assert!(!gemini.received_requests().await.unwrap().is_empty());
    let statuses = knowledge.provider_statuses();
    assert_eq!(statuses[0].key, "gemini:worker-a");

    // Search uses the first provider that answers and has vectors.
    let SearchOutcome::Found(excerpts) = knowledge.search(guild, "切り替え").await.unwrap()
    else {
        panic!("documents expected");
    };
    assert!(!excerpts.is_empty() && excerpts.iter().all(|e| e.document_id == document));

    // Once the rate limit is over, the first provider's vectors are filled in.
    limited.store(false, Ordering::SeqCst);
    wait_until("the first provider was backfilled", || async {
        vectors(&db, document, "gemini:worker-a").await == chunks
    })
    .await;
    assert_eq!(status(&db, document).await.0, "ready");
    worker.halt().await;
    clear_guilds(&db, &[guild]).await;
    clear_worker_vectors(&db).await;
}

#[tokio::test]
#[ignore = "requires compose.test.yaml and TEST_DATABASE_URL"]
async fn knowledge_worker_restart_and_rate_limits() {
    let _serial = SERIAL.lock().await;
    let db = database().await;
    let (guild, limited_guild) = (963_002_u64, 963_003_u64);
    clear_guilds(&db, &[guild, limited_guild]).await;
    clear_worker_vectors(&db).await;

    // Recovery: a worker stopped half-way leaves the document processing; the next one
    // finishes it.
    let slow = MockServer::start().await;
    Mock::given(path_regex(GEMINI_PATH))
        .respond_with(move |request: &Request| {
            gemini_vectors(request).set_delay(Duration::from_millis(150))
        })
        .mount(&slow)
        .await;
    let mut config = provider(ProviderKind::Gemini, "worker-c", &slow.uri());
    config.batch_size = 1;
    let knowledge = Arc::new(Knowledge::new(kb_config(vec![config]), db.clone()).unwrap());
    let document = upload_text(&knowledge, guild, "再起動", 200).await;
    let chunks = i64::from(
        db.kb_document(guild, document)
            .await
            .unwrap()
            .unwrap()
            .chunk_count,
    );
    let worker = start(&knowledge);
    wait_until("some vectors were stored", || async {
        vectors(&db, document, "gemini:worker-c").await >= 2
    })
    .await;
    worker.halt().await;
    let stored = vectors(&db, document, "gemini:worker-c").await;
    assert!(stored < chunks, "{stored} of {chunks}");
    assert_eq!(status(&db, document).await.0, "processing");
    let worker = start(&knowledge);
    wait_until("the restarted worker finished", || async {
        status(&db, document).await.0 == "ready"
    })
    .await;
    assert_eq!(vectors(&db, document, "gemini:worker-c").await, chunks);
    worker.halt().await;

    // A provider that answers 429 a few times (the only one) slows the document down but
    // never fails it.
    let server = MockServer::start().await;
    let calls = Arc::new(AtomicUsize::new(0));
    Mock::given(path_regex(GEMINI_PATH))
        .respond_with({
            let calls = calls.clone();
            move |request: &Request| {
                if calls.fetch_add(1, Ordering::SeqCst) < 3 {
                    ResponseTemplate::new(429).insert_header("retry-after", "1")
                } else {
                    gemini_vectors(request)
                }
            }
        })
        .mount(&server)
        .await;
    let knowledge = Arc::new(
        Knowledge::new(
            kb_config(vec![provider(
                ProviderKind::Gemini,
                "worker-d",
                &server.uri(),
            )]),
            db.clone(),
        )
        .unwrap(),
    );
    let document = upload_text(&knowledge, limited_guild, "制限", 10).await;
    let worker = start(&knowledge);
    wait_until("the rate-limited document became ready", || async {
        let (state, attempts, _) = status(&db, document).await;
        assert_ne!(state, "failed");
        assert_eq!(attempts, 0, "rate limits are not failures");
        state == "ready"
    })
    .await;
    assert!(calls.load(Ordering::SeqCst) >= 4);
    worker.halt().await;
    clear_guilds(&db, &[guild, limited_guild]).await;
    clear_worker_vectors(&db).await;
}

#[tokio::test]
#[ignore = "requires compose.test.yaml and TEST_DATABASE_URL"]
async fn knowledge_worker_failures_and_deletion() {
    let _serial = SERIAL.lock().await;
    let db = database().await;
    let (guild, deleted_guild) = (963_004_u64, 963_005_u64);
    clear_guilds(&db, &[guild, deleted_guild]).await;
    clear_worker_vectors(&db).await;

    // Server errors count: after MAX_ATTEMPTS the document fails; a retry starts over.
    let server = MockServer::start().await;
    let failing = Arc::new(AtomicBool::new(true));
    Mock::given(path("/v1/embeddings"))
        .respond_with({
            let failing = failing.clone();
            move |request: &Request| {
                if failing.load(Ordering::SeqCst) {
                    ResponseTemplate::new(500)
                } else {
                    openai_vectors(request)
                }
            }
        })
        .mount(&server)
        .await;
    let knowledge = Arc::new(
        Knowledge::new(
            kb_config(vec![provider(
                ProviderKind::OpenAi,
                "worker-e",
                &server.uri(),
            )]),
            db.clone(),
        )
        .unwrap(),
    );
    let document = upload_text(&knowledge, guild, "失敗", 5).await;
    let worker = start(&knowledge);
    wait_until("the document failed", || async {
        status(&db, document).await.0 == "failed"
    })
    .await;
    assert_eq!(
        status(&db, document).await,
        (
            "failed".into(),
            MAX_ATTEMPTS,
            Some("embedding_upstream".into())
        )
    );
    failing.store(false, Ordering::SeqCst);
    assert_eq!(
        db.kb_retry(guild, document).await.unwrap(),
        Retry::Restarted
    );
    assert_eq!(
        db.kb_retry(guild, document).await.unwrap(),
        Retry::NotFailed
    );
    assert_eq!(db.kb_retry(guild, u64::MAX).await.unwrap(), Retry::NotFound);
    knowledge.wake_worker();
    wait_until("the retried document became ready", || async {
        status(&db, document).await.0 == "ready"
    })
    .await;
    assert_eq!(status(&db, document).await, ("ready".into(), 0, None));
    worker.halt().await;

    // A document deleted while its batch is on the way is skipped; the worker goes on.
    let slow = MockServer::start().await;
    Mock::given(path_regex(GEMINI_PATH))
        .respond_with(move |request: &Request| {
            gemini_vectors(request).set_delay(Duration::from_millis(700))
        })
        .mount(&slow)
        .await;
    let knowledge = Arc::new(
        Knowledge::new(
            kb_config(vec![provider(
                ProviderKind::Gemini,
                "worker-f",
                &slow.uri(),
            )]),
            db.clone(),
        )
        .unwrap(),
    );
    let doomed = upload_text(&knowledge, deleted_guild, "削除", 5).await;
    let worker = start(&knowledge);
    wait_until("the first request was sent", || async {
        !slow.received_requests().await.unwrap().is_empty()
    })
    .await;
    assert!(db.kb_delete(deleted_guild, doomed).await.unwrap());
    let next = upload_text(&knowledge, deleted_guild, "次の資料", 5).await;
    wait_until("the next document became ready", || async {
        status(&db, next).await.0 == "ready"
    })
    .await;
    assert!(!worker.handle.is_finished());
    assert_eq!(
        count(
            &db,
            "SELECT COUNT(*) FROM kb_chunks WHERE document_id=?",
            doomed
        )
        .await,
        0
    );
    worker.halt().await;
    clear_guilds(&db, &[guild, deleted_guild]).await;
    clear_worker_vectors(&db).await;
}

#[tokio::test]
#[ignore = "requires compose.test.yaml and TEST_DATABASE_URL"]
async fn knowledge_search_prefers_complete_providers() {
    let _serial = SERIAL.lock().await;
    let db = database().await;
    let guild = 964_001_u64;
    clear_guilds(&db, &[guild]).await;
    // The first provider's query vector points to document A, the second's to document B.
    let (first, second) = (MockServer::start().await, MockServer::start().await);
    Mock::given(path_regex(GEMINI_PATH))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({"embeddings": [{"values": axis(0)}]})),
        )
        .mount(&first)
        .await;
    let failing = Arc::new(AtomicBool::new(false));
    Mock::given(path("/v1/embeddings"))
        .respond_with({
            let failing = failing.clone();
            move |_: &Request| {
                if failing.load(Ordering::SeqCst) {
                    ResponseTemplate::new(500)
                } else {
                    ResponseTemplate::new(200)
                        .set_body_json(json!({"data": [{"index": 0, "embedding": axis(1)}]}))
                }
            }
        })
        .mount(&second)
        .await;
    let knowledge = Knowledge::new(
        kb_config(vec![
            provider(ProviderKind::Gemini, "search-a", &first.uri()),
            provider(ProviderKind::OpenAi, "search-b", &second.uri()),
        ]),
        db.clone(),
    )
    .unwrap();
    let (a, a_chunks) = document(&db, guild, "a", &["Aの前半", "Aの後半"]).await;
    let (b, b_chunks) = document(&db, guild, "b", &["Bの本文"]).await;
    // The first provider stopped after A (its daily limit, say); the second covers both, and B
    // became ready through it.
    db.kb_store_vectors(
        guild,
        "gemini:search-a",
        &[
            (a_chunks[0], vector(&[(0, 1.0)])),
            (a_chunks[1], vector(&[(0, 1.0), (2, 1.0)])),
        ],
    )
    .await
    .unwrap();
    db.kb_store_vectors(
        guild,
        "openai:search-b",
        &[
            (a_chunks[0], vector(&[(5, 1.0)])),
            (a_chunks[1], vector(&[(6, 1.0)])),
            (b_chunks[0], vector(&[(1, 1.0)])),
        ],
    )
    .await
    .unwrap();
    for document in [a, b] {
        assert!(db.kb_mark_ready(document).await.unwrap());
    }
    let documents = |outcome: Result<SearchOutcome, SearchError>| match outcome {
        Ok(SearchOutcome::Found(excerpts)) => excerpts
            .iter()
            .map(|excerpt| excerpt.document_id)
            .collect::<Vec<_>>(),
        other => panic!("{other:?}"),
    };

    // The provider with vectors for every ready document is asked first, so B is found,
    // although the first provider in the configuration answers too.
    let found = documents(knowledge.search(guild, "Bについて").await);
    assert_eq!(found[0], b);
    assert!(first.received_requests().await.unwrap().is_empty());

    // When it fails, the provider that covers part of the documents stands in.
    failing.store(true, Ordering::SeqCst);
    let found = documents(knowledge.search(guild, "Aについて").await);
    assert!(
        !found.is_empty() && found.iter().all(|id| *id == a),
        "{found:?}"
    );

    // Without the partial vectors, nothing could be searched: that is a failure (with its
    // notice in /talk), not "nothing found", and the first provider is not even asked.
    sqlx::query("DELETE FROM kb_embeddings WHERE guild_id=? AND provider='gemini:search-a'")
        .bind(guild)
        .execute(&db.pool)
        .await
        .unwrap();
    let asked = first.received_requests().await.unwrap().len();
    assert_eq!(
        knowledge.search(guild, "Aについて").await,
        Err(SearchError::Unavailable)
    );
    assert_eq!(first.received_requests().await.unwrap().len(), asked);
    clear_guilds(&db, &[guild]).await;
}

#[tokio::test]
#[ignore = "requires compose.test.yaml and TEST_DATABASE_URL"]
async fn knowledge_worker_retries_a_rejected_document() {
    let _serial = SERIAL.lock().await;
    let db = database().await;
    let guild = 963_006_u64;
    clear_guilds(&db, &[guild]).await;
    clear_worker_vectors(&db).await;
    // The only provider refuses the text (a 400, say from a proxy), then accepts it.
    let server = MockServer::start().await;
    let rejecting = Arc::new(AtomicBool::new(true));
    Mock::given(path("/v1/embeddings"))
        .respond_with({
            let rejecting = rejecting.clone();
            move |request: &Request| {
                if rejecting.load(Ordering::SeqCst) {
                    ResponseTemplate::new(400).set_body_json(
                        json!({"error": {"message": "rejected", "type": "invalid_request_error"}}),
                    )
                } else {
                    openai_vectors(request)
                }
            }
        })
        .mount(&server)
        .await;
    let knowledge = Arc::new(
        Knowledge::new(
            kb_config(vec![provider(
                ProviderKind::OpenAi,
                "worker-g",
                &server.uri(),
            )]),
            db.clone(),
        )
        .unwrap(),
    );
    let document = upload_text(&knowledge, guild, "拒否", 5).await;
    let worker = start(&knowledge);
    wait_until("the document failed", || async {
        status(&db, document).await.0 == "failed"
    })
    .await;
    assert_eq!(
        status(&db, document).await,
        ("failed".into(), 0, Some("embedding_rejected".into()))
    );
    // "再試行" really tries again: the worker does not remember the refusal.
    let sent = server.received_requests().await.unwrap().len();
    rejecting.store(false, Ordering::SeqCst);
    assert_eq!(
        db.kb_retry(guild, document).await.unwrap(),
        Retry::Restarted
    );
    knowledge.wake_worker();
    wait_until("the retried document became ready", || async {
        let (state, _, code) = status(&db, document).await;
        assert_ne!(code.as_deref(), Some("embedding_rejected"), "refused again");
        state == "ready"
    })
    .await;
    assert!(server.received_requests().await.unwrap().len() > sent);
    worker.halt().await;
    clear_guilds(&db, &[guild]).await;
    clear_worker_vectors(&db).await;
}

#[tokio::test]
#[ignore = "requires compose.test.yaml and TEST_DATABASE_URL"]
async fn knowledge_worker_waits_for_a_rate_limited_provider() {
    let _serial = SERIAL.lock().await;
    let db = database().await;
    let guild = 963_007_u64;
    clear_guilds(&db, &[guild]).await;
    clear_worker_vectors(&db).await;
    // The first provider's daily quota is used up; the second one fails (an outage, say).
    let (gemini, openai) = (MockServer::start().await, MockServer::start().await);
    Mock::given(path_regex(GEMINI_PATH))
        .respond_with(gemini_daily_quota())
        .expect(1)
        .mount(&gemini)
        .await;
    let failing = Arc::new(AtomicBool::new(true));
    Mock::given(path("/v1/embeddings"))
        .respond_with({
            let failing = failing.clone();
            move |request: &Request| {
                if failing.load(Ordering::SeqCst) {
                    ResponseTemplate::new(500)
                } else {
                    openai_vectors(request)
                }
            }
        })
        .mount(&openai)
        .await;
    let knowledge = Arc::new(
        Knowledge::new(
            kb_config(vec![
                provider(ProviderKind::Gemini, "worker-h", &gemini.uri()),
                provider(ProviderKind::OpenAi, "worker-i", &openai.uri()),
            ]),
            db.clone(),
        )
        .unwrap(),
    );
    let document = upload_text(&knowledge, guild, "待機", 5).await;
    let worker = start(&knowledge);
    wait_until(
        "the second provider failed more often than a document may",
        || async { openai.received_requests().await.unwrap().len() > MAX_ATTEMPTS as usize + 1 },
    )
    .await;
    // The document waits for the first provider instead of failing.
    assert_eq!(status(&db, document).await, ("processing".into(), 0, None));
    failing.store(false, Ordering::SeqCst);
    wait_until("the document became ready", || async {
        status(&db, document).await.0 == "ready"
    })
    .await;
    assert_eq!(status(&db, document).await, ("ready".into(), 0, None));
    worker.halt().await;
    gemini.verify().await;
    clear_guilds(&db, &[guild]).await;
    clear_worker_vectors(&db).await;
}

#[tokio::test]
#[ignore = "requires compose.test.yaml and TEST_DATABASE_URL"]
async fn knowledge_worker_lets_guilds_take_turns() {
    let _serial = SERIAL.lock().await;
    let db = database().await;
    let (busy, other) = (963_010_u64, 963_011_u64);
    clear_guilds(&db, &[busy, other]).await;
    clear_worker_vectors(&db).await;
    let server = MockServer::start().await;
    Mock::given(path_regex(GEMINI_PATH))
        .respond_with(move |request: &Request| {
            gemini_vectors(request).set_delay(Duration::from_millis(100))
        })
        .mount(&server)
        .await;
    let mut config = provider(ProviderKind::Gemini, "worker-j", &server.uri());
    config.batch_size = 1;
    let knowledge = Arc::new(Knowledge::new(kb_config(vec![config]), db.clone()).unwrap());
    // One guild uploads a large document; another guild's small one comes after it.
    let large = upload_text(&knowledge, busy, "大きな資料", 200).await;
    let small = upload_text(&knowledge, other, "小さな資料", 3).await;
    let chunks = i64::from(
        db.kb_document(busy, large)
            .await
            .unwrap()
            .unwrap()
            .chunk_count,
    );
    assert!(chunks >= 8, "{chunks}");
    let worker = start(&knowledge);
    wait_until("the small document became ready", || async {
        status(&db, small).await.0 == "ready"
    })
    .await;
    // It did not wait for the whole large document.
    let done = vectors(&db, large, "gemini:worker-j").await;
    assert!(done < chunks, "{done} of {chunks}");
    assert_eq!(status(&db, large).await.0, "processing");
    wait_until("the large document became ready", || async {
        status(&db, large).await.0 == "ready"
    })
    .await;
    worker.halt().await;
    clear_guilds(&db, &[busy, other]).await;
    clear_worker_vectors(&db).await;
}

#[tokio::test]
#[ignore = "requires compose.test.yaml and TEST_DATABASE_URL"]
async fn knowledge_prune_embeddings() {
    let _serial = SERIAL.lock().await;
    let db = database().await;
    let guild = 965_001_u64;
    clear_guilds(&db, &[guild]).await;
    let texts: Vec<String> = (0..5).map(|i| format!("断片{i}")).collect();
    let texts: Vec<&str> = texts.iter().map(String::as_str).collect();
    let (document, chunks) = self::document(&db, guild, "prune", &texts).await;
    for (key, axis) in [("test:prune-old", 0), ("test:prune-new", 1)] {
        let rows: Vec<(u64, Vec<u8>)> = chunks
            .iter()
            .map(|chunk| (*chunk, vector(&[(axis, 1.0)])))
            .collect();
        db.kb_store_vectors(guild, key, &rows).await.unwrap();
    }
    db.kb_mark_ready(document).await.unwrap();
    let stored = |key: &'static str| {
        let db = db.clone();
        async move {
            db.kb_vector_counts()
                .await
                .unwrap()
                .into_iter()
                .find(|(stored, _)| stored == key)
                .map_or(0, |(_, count)| count)
        }
    };
    // Every other key in the test database counts as configured, so only ours is stale.
    let current: Vec<String> = db
        .kb_vector_counts()
        .await
        .unwrap()
        .into_iter()
        .map(|(key, _)| key)
        .filter(|key| key != "test:prune-old")
        .collect();
    assert!(current.iter().any(|key| key == "test:prune-new"));

    // A dry run deletes nothing.
    assert_eq!(prune_embeddings(&db, &current, false, 2).await.unwrap(), 0);
    assert_eq!(stored("test:prune-old").await, 5);
    // With no provider configured every key would be stale: refused, nothing deleted.
    assert!(prune_embeddings(&db, &[], true, 2).await.is_err());
    assert_eq!(stored("test:prune-old").await, 5);
    assert_eq!(stored("test:prune-new").await, 5);
    // --apply deletes the stale key only, in several statements.
    assert_eq!(prune_embeddings(&db, &current, true, 2).await.unwrap(), 5);
    assert_eq!(stored("test:prune-old").await, 0);
    assert_eq!(stored("test:prune-new").await, 5);
    assert_eq!(prune_embeddings(&db, &current, true, 2).await.unwrap(), 0);
    clear_guilds(&db, &[guild]).await;
}
