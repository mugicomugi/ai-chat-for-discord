//! Knowledge base tables. Vectors are bound as their raw VECTOR bytes (little-endian f32), which
//! MariaDB 12.3 accepts directly as a VECTOR value and as VEC_DISTANCE_COSINE's argument.

use std::collections::HashMap;

use chrono::{DateTime, NaiveDateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::{MySql, QueryBuilder, Row, mysql::MySqlRow};

use super::chunk::Chunk;
use crate::db::Database;

/// A document as listed (without its text).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DocumentRow {
    pub id: u64,
    pub guild_id: u64,
    pub title: String,
    pub file_name: String,
    pub media_type: String,
    pub byte_size: u32,
    pub char_count: u32,
    pub chunk_count: u32,
    pub status: Status,
    pub attempts: u32,
    pub error_code: Option<String>,
    pub uploaded_by_name: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Processing,
    Ready,
    Failed,
}

impl Status {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Processing => "processing",
            Self::Ready => "ready",
            Self::Failed => "failed",
        }
    }

    fn parse(value: &str) -> Self {
        match value {
            "ready" => Self::Ready,
            "failed" => Self::Failed,
            _ => Self::Processing,
        }
    }
}

pub struct NewDocument<'a> {
    pub guild_id: u64,
    pub title: &'a str,
    pub file_name: &'a str,
    pub media_type: &'a str,
    pub byte_size: u32,
    pub sha256: [u8; 32],
    pub content: &'a str,
    pub char_count: u32,
    pub chunk_count: u32,
    pub uploaded_by: u64,
    pub uploaded_by_name: &'a str,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Quotas {
    pub max_docs_per_guild: u32,
    pub max_chunks_per_guild: u32,
    pub max_chunks_total: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Insert {
    Created(u64),
    /// The guild already has a document with the same content.
    Duplicate {
        title: String,
    },
    TooManyDocuments,
    TooManyChunks,
    TooManyChunksTotal,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Usage {
    pub documents: u64,
    pub chunks: u64,
    /// All guilds together.
    pub total_chunks: u64,
}

/// A document's id and title as given to the AI and saved with a /talk run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KbSource {
    pub id: u64,
    pub title: String,
}

/// One chunk found by a search.
#[derive(Debug, Clone, PartialEq)]
pub struct Hit {
    pub document_id: u64,
    pub title: String,
    pub seq: u32,
    pub heading: Option<String>,
    pub content: String,
    /// Cosine distance: 0 is the same direction, 2 the opposite.
    pub distance: f64,
}

/// What the providers' vectors cover of a guild's searchable chunks.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Coverage {
    /// Chunks of the guild's ready documents.
    pub chunks: u64,
    /// How many of them have a vector, per provider key.
    pub vectors: HashMap<String, u64>,
}

/// A chunk that still needs a vector.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingChunk {
    pub id: u64,
    pub heading: Option<String>,
    pub content: String,
}

/// What the worker needs of a document.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkDocument {
    pub id: u64,
    pub guild_id: u64,
    pub title: String,
    pub media_type: String,
    pub chunk_count: u32,
    pub attempts: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Retry {
    Restarted,
    NotFailed,
    NotFound,
}

const DOCUMENT_COLUMNS: &str = "id,guild_id,title,file_name,media_type,byte_size,char_count,chunk_count,status,attempts,error_code,uploaded_by_name,created_at,updated_at";

fn document(row: &MySqlRow) -> Result<DocumentRow, sqlx::Error> {
    Ok(DocumentRow {
        id: row.try_get("id")?,
        guild_id: row.try_get("guild_id")?,
        title: row.try_get("title")?,
        file_name: row.try_get("file_name")?,
        media_type: row.try_get("media_type")?,
        byte_size: row.try_get("byte_size")?,
        char_count: row.try_get("char_count")?,
        chunk_count: row.try_get("chunk_count")?,
        status: Status::parse(&row.try_get::<String, _>("status")?),
        attempts: row.try_get("attempts")?,
        error_code: row.try_get("error_code")?,
        uploaded_by_name: row.try_get("uploaded_by_name")?,
        created_at: row.try_get::<NaiveDateTime, _>("created_at")?.and_utc(),
        updated_at: row.try_get::<NaiveDateTime, _>("updated_at")?.and_utc(),
    })
}

fn work_document(row: &MySqlRow) -> Result<WorkDocument, sqlx::Error> {
    Ok(WorkDocument {
        id: row.try_get("id")?,
        guild_id: row.try_get("guild_id")?,
        title: row.try_get("title")?,
        media_type: row.try_get("media_type")?,
        chunk_count: row.try_get("chunk_count")?,
        attempts: row.try_get("attempts")?,
    })
}

impl Database {
    /// Checks the duplicate and the quotas and stores the document, in one transaction. The
    /// caller serializes uploads (one process), so two uploads cannot both pass the checks.
    pub async fn kb_insert(
        &self,
        document: &NewDocument<'_>,
        quotas: &Quotas,
    ) -> Result<Insert, sqlx::Error> {
        let mut tx = self.pool.begin().await?;
        let duplicate: Option<String> =
            sqlx::query_scalar("SELECT title FROM kb_documents WHERE guild_id=? AND sha256=?")
                .bind(document.guild_id)
                .bind(document.sha256.as_slice())
                .fetch_optional(&mut *tx)
                .await?;
        if let Some(title) = duplicate {
            return Ok(Insert::Duplicate { title });
        }
        let usage = usage(&mut tx, document.guild_id).await?;
        let chunks = u64::from(document.chunk_count);
        if usage.documents >= u64::from(quotas.max_docs_per_guild) {
            return Ok(Insert::TooManyDocuments);
        }
        if usage.chunks + chunks > u64::from(quotas.max_chunks_per_guild) {
            return Ok(Insert::TooManyChunks);
        }
        if usage.total_chunks + chunks > u64::from(quotas.max_chunks_total) {
            return Ok(Insert::TooManyChunksTotal);
        }
        let result = sqlx::query("INSERT INTO kb_documents (guild_id,title,file_name,media_type,byte_size,sha256,content,char_count,chunk_count,status,uploaded_by,uploaded_by_name,created_at,updated_at) VALUES (?,?,?,?,?,?,?,?,?,'processing',?,?,UTC_TIMESTAMP(3),UTC_TIMESTAMP(3))")
            .bind(document.guild_id).bind(document.title).bind(document.file_name)
            .bind(document.media_type).bind(document.byte_size).bind(document.sha256.as_slice())
            .bind(document.content).bind(document.char_count).bind(document.chunk_count)
            .bind(document.uploaded_by).bind(document.uploaded_by_name)
            .execute(&mut *tx).await;
        let id = match result {
            Ok(result) => result.last_insert_id(),
            Err(sqlx::Error::Database(error)) if error.is_unique_violation() => {
                return Ok(Insert::Duplicate {
                    title: document.title.to_owned(),
                });
            }
            Err(error) => return Err(error),
        };
        tx.commit().await?;
        Ok(Insert::Created(id))
    }

    /// The title of the guild's document with this content, if there is one.
    pub async fn kb_duplicate(
        &self,
        guild: u64,
        sha256: &[u8; 32],
    ) -> Result<Option<String>, sqlx::Error> {
        sqlx::query_scalar("SELECT title FROM kb_documents WHERE guild_id=? AND sha256=?")
            .bind(guild)
            .bind(sha256.as_slice())
            .fetch_optional(&self.pool)
            .await
    }

    pub async fn kb_usage(&self, guild: u64) -> Result<Usage, sqlx::Error> {
        let mut connection = self.pool.acquire().await?;
        usage(&mut connection, guild).await
    }

    /// The guild's documents, newest first.
    pub async fn kb_documents(&self, guild: u64) -> Result<Vec<DocumentRow>, sqlx::Error> {
        sqlx::query(&format!(
            "SELECT {DOCUMENT_COLUMNS} FROM kb_documents WHERE guild_id=? ORDER BY id DESC"
        ))
        .bind(guild)
        .fetch_all(&self.pool)
        .await?
        .iter()
        .map(document)
        .collect()
    }

    pub async fn kb_document(
        &self,
        guild: u64,
        id: u64,
    ) -> Result<Option<DocumentRow>, sqlx::Error> {
        sqlx::query(&format!(
            "SELECT {DOCUMENT_COLUMNS} FROM kb_documents WHERE guild_id=? AND id=?"
        ))
        .bind(guild)
        .bind(id)
        .fetch_optional(&self.pool)
        .await?
        .as_ref()
        .map(document)
        .transpose()
    }

    /// Stored vectors per document and provider key, for the guild's progress display.
    pub async fn kb_progress(&self, guild: u64) -> Result<Vec<(u64, String, u32)>, sqlx::Error> {
        let rows = sqlx::query("SELECT c.document_id,e.provider,COUNT(*) AS n FROM kb_embeddings e JOIN kb_chunks c ON c.id=e.chunk_id WHERE e.guild_id=? GROUP BY c.document_id,e.provider")
            .bind(guild).fetch_all(&self.pool).await?;
        rows.iter()
            .map(|row| {
                Ok((
                    row.try_get("document_id")?,
                    row.try_get("provider")?,
                    u32::try_from(row.try_get::<i64, _>("n")?).unwrap_or(u32::MAX),
                ))
            })
            .collect()
    }

    /// The start of a document's text (`chars` characters) and its full length in characters.
    pub async fn kb_preview(
        &self,
        guild: u64,
        id: u64,
        chars: u32,
    ) -> Result<Option<(String, u32)>, sqlx::Error> {
        let row = sqlx::query(
            "SELECT LEFT(content,?) AS head,char_count FROM kb_documents WHERE guild_id=? AND id=?",
        )
        .bind(chars)
        .bind(guild)
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;
        row.map(|row| Ok((row.try_get("head")?, row.try_get("char_count")?)))
            .transpose()
    }

    /// Puts a failed document back into the queue with a fresh attempt count.
    pub async fn kb_retry(&self, guild: u64, id: u64) -> Result<Retry, sqlx::Error> {
        let updated = sqlx::query("UPDATE kb_documents SET status='processing',attempts=0,error_code=NULL,updated_at=UTC_TIMESTAMP(3) WHERE guild_id=? AND id=? AND status='failed'")
            .bind(guild).bind(id).execute(&self.pool).await?.rows_affected();
        if updated == 1 {
            return Ok(Retry::Restarted);
        }
        Ok(match self.kb_document(guild, id).await? {
            Some(_) => Retry::NotFailed,
            None => Retry::NotFound,
        })
    }

    /// Deletes a document with its chunks and vectors. Returns false if there was none.
    pub async fn kb_delete(&self, guild: u64, id: u64) -> Result<bool, sqlx::Error> {
        Ok(
            sqlx::query("DELETE FROM kb_documents WHERE guild_id=? AND id=?")
                .bind(guild)
                .bind(id)
                .execute(&self.pool)
                .await?
                .rows_affected()
                == 1,
        )
    }

    pub async fn kb_has_ready(&self, guild: u64) -> Result<bool, sqlx::Error> {
        sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM kb_documents WHERE guild_id=? AND status='ready')",
        )
        .bind(guild)
        .fetch_one(&self.pool)
        .await
    }

    /// How many chunks the guild's ready documents have, and how many of them have a vector of
    /// each provider key. A document is ready once one provider covers it, so the others may
    /// cover only part of what is searchable.
    pub async fn kb_coverage(&self, guild: u64) -> Result<Coverage, sqlx::Error> {
        let chunks: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM kb_documents d JOIN kb_chunks c ON c.document_id=d.id WHERE d.guild_id=? AND d.status='ready'")
            .bind(guild).fetch_one(&self.pool).await?;
        let rows = sqlx::query("SELECT e.provider,COUNT(*) AS n FROM kb_documents d JOIN kb_chunks c ON c.document_id=d.id JOIN kb_embeddings e ON e.chunk_id=c.id WHERE d.guild_id=? AND d.status='ready' GROUP BY e.provider")
            .bind(guild).fetch_all(&self.pool).await?;
        Ok(Coverage {
            chunks: chunks as u64,
            vectors: rows
                .iter()
                .map(|row| Ok((row.try_get("provider")?, row.try_get::<i64, _>("n")? as u64)))
                .collect::<Result<_, sqlx::Error>>()?,
        })
    }

    /// The nearest chunks of the guild's ready documents by exact comparison with every vector
    /// of this provider key (no vector index; see migrations/0007). The texts are read only for
    /// the rows found, so the sort never carries them.
    pub async fn kb_search(
        &self,
        guild: u64,
        provider: &str,
        vector: &[u8],
        limit: u32,
    ) -> Result<Vec<Hit>, sqlx::Error> {
        let rows = sqlx::query("SELECT n.document_id,d.title,n.seq,c.heading,c.content,n.distance FROM (SELECT e.chunk_id,c.document_id,c.seq,VEC_DISTANCE_COSINE(e.embedding,?) AS distance FROM kb_embeddings e JOIN kb_chunks c ON c.id=e.chunk_id JOIN kb_documents d ON d.id=c.document_id WHERE e.guild_id=? AND e.provider=? AND d.guild_id=? AND d.status='ready' ORDER BY distance,c.document_id,c.seq LIMIT ?) n JOIN kb_chunks c ON c.id=n.chunk_id JOIN kb_documents d ON d.id=n.document_id ORDER BY n.distance,n.document_id,n.seq")
            .bind(vector).bind(guild).bind(provider).bind(guild).bind(limit)
            .fetch_all(&self.pool).await?;
        rows.iter()
            .map(|row| {
                Ok(Hit {
                    document_id: row.try_get("document_id")?,
                    title: row.try_get("title")?,
                    seq: row.try_get("seq")?,
                    heading: row.try_get("heading")?,
                    content: row.try_get("content")?,
                    distance: row.try_get("distance")?,
                })
            })
            .collect()
    }

    /// Records what /talk gave the AI from the knowledge base: `None` when it was not used.
    pub async fn record_knowledge(
        &self,
        interaction: u64,
        sources: Option<&[KbSource]>,
    ) -> Result<(), sqlx::Error> {
        sqlx::query("UPDATE talk_runs SET knowledge=?,kb_sources=? WHERE interaction_id=?")
            .bind(sources.is_some())
            .bind(sources.map(sqlx::types::Json))
            .bind(interaction)
            .execute(&self.pool)
            .await?;
        Ok(())
    }
}

/// Worker queries.
impl Database {
    /// Documents waiting for chunks or vectors: every guild's oldest first, then every guild's
    /// second oldest and so on, so the page holds work of every guild however much one has.
    pub async fn kb_processing(&self, limit: u32) -> Result<Vec<WorkDocument>, sqlx::Error> {
        sqlx::query("SELECT id,guild_id,title,media_type,chunk_count,attempts FROM (SELECT id,guild_id,title,media_type,chunk_count,attempts,ROW_NUMBER() OVER (PARTITION BY guild_id ORDER BY id) AS turn FROM kb_documents WHERE status='processing') q ORDER BY turn,id LIMIT ?")
            .bind(limit).fetch_all(&self.pool).await?.iter().map(work_document).collect()
    }

    /// Ready documents with chunks that have no vector of this provider key yet, oldest first.
    pub async fn kb_backfill(
        &self,
        provider: &str,
        limit: u32,
    ) -> Result<Vec<WorkDocument>, sqlx::Error> {
        sqlx::query("SELECT d.id,d.guild_id,d.title,d.media_type,d.chunk_count,d.attempts FROM kb_documents d WHERE d.status='ready' AND EXISTS (SELECT 1 FROM kb_chunks c WHERE c.document_id=d.id AND NOT EXISTS (SELECT 1 FROM kb_embeddings e WHERE e.chunk_id=c.id AND e.provider=?)) ORDER BY d.id LIMIT ?")
            .bind(provider).bind(limit).fetch_all(&self.pool).await?.iter().map(work_document).collect()
    }

    pub async fn kb_chunk_rows(&self, document: u64) -> Result<u32, sqlx::Error> {
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM kb_chunks WHERE document_id=?")
            .bind(document)
            .fetch_one(&self.pool)
            .await?;
        Ok(u32::try_from(count).unwrap_or(u32::MAX))
    }

    /// The text of a document, `None` if it was deleted.
    pub async fn kb_content(&self, document: u64) -> Result<Option<String>, sqlx::Error> {
        sqlx::query_scalar("SELECT content FROM kb_documents WHERE id=?")
            .bind(document)
            .fetch_optional(&self.pool)
            .await
    }

    /// Replaces the document's chunks (and with them its vectors) in one transaction. Returns
    /// false if the document was deleted meanwhile.
    pub async fn kb_replace_chunks(
        &self,
        document: u64,
        guild: u64,
        chunks: &[Chunk],
    ) -> Result<bool, sqlx::Error> {
        let mut tx = self.pool.begin().await?;
        // Holds off a concurrent delete of the document until the chunks are complete.
        let exists = sqlx::query("SELECT id FROM kb_documents WHERE id=? FOR UPDATE")
            .bind(document)
            .fetch_optional(&mut *tx)
            .await?
            .is_some();
        if !exists {
            return Ok(false);
        }
        sqlx::query("DELETE FROM kb_chunks WHERE document_id=?")
            .bind(document)
            .execute(&mut *tx)
            .await?;
        for (batch_index, batch) in chunks.chunks(100).enumerate() {
            let mut insert: QueryBuilder<MySql> = QueryBuilder::new(
                "INSERT INTO kb_chunks (document_id,guild_id,seq,heading,content) ",
            );
            insert.push_values(batch.iter().enumerate(), |mut row, (i, chunk)| {
                row.push_bind(document)
                    .push_bind(guild)
                    .push_bind((batch_index * 100 + i) as u32)
                    .push_bind(chunk.heading.as_deref())
                    .push_bind(chunk.content.as_str());
            });
            insert.build().execute(&mut *tx).await?;
        }
        sqlx::query("UPDATE kb_documents SET chunk_count=?,updated_at=UTC_TIMESTAMP(3) WHERE id=?")
            .bind(chunks.len() as u32)
            .bind(document)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(true)
    }

    /// Chunks of the document without a vector of this provider key, in order.
    pub async fn kb_pending_chunks(
        &self,
        document: u64,
        provider: &str,
        limit: u32,
    ) -> Result<Vec<PendingChunk>, sqlx::Error> {
        let rows = sqlx::query("SELECT c.id,c.heading,c.content FROM kb_chunks c WHERE c.document_id=? AND NOT EXISTS (SELECT 1 FROM kb_embeddings e WHERE e.chunk_id=c.id AND e.provider=?) ORDER BY c.seq LIMIT ?")
            .bind(document).bind(provider).bind(limit).fetch_all(&self.pool).await?;
        rows.iter()
            .map(|row| {
                Ok(PendingChunk {
                    id: row.try_get("id")?,
                    heading: row.try_get("heading")?,
                    content: row.try_get("content")?,
                })
            })
            .collect()
    }

    /// Stores vectors (VECTOR bytes) of chunks. Returns false if a chunk no longer exists (its
    /// document was deleted or re-chunked meanwhile); then nothing is stored.
    pub async fn kb_store_vectors(
        &self,
        guild: u64,
        provider: &str,
        vectors: &[(u64, Vec<u8>)],
    ) -> Result<bool, sqlx::Error> {
        if vectors.is_empty() {
            return Ok(true);
        }
        let mut insert: QueryBuilder<MySql> =
            QueryBuilder::new("INSERT INTO kb_embeddings (chunk_id,provider,guild_id,embedding) ");
        insert.push_values(vectors, |mut row, (chunk, vector)| {
            row.push_bind(*chunk)
                .push_bind(provider)
                .push_bind(guild)
                .push_bind(vector.as_slice());
        });
        insert.push(" ON DUPLICATE KEY UPDATE embedding=VALUES(embedding)");
        match insert.build().execute(&self.pool).await {
            Ok(_) => Ok(true),
            Err(sqlx::Error::Database(error)) if error.is_foreign_key_violation() => Ok(false),
            Err(error) => Err(error),
        }
    }

    /// Marks a processing document searchable. Returns false if it is gone or no longer
    /// processing.
    pub async fn kb_mark_ready(&self, document: u64) -> Result<bool, sqlx::Error> {
        Ok(sqlx::query("UPDATE kb_documents SET status='ready',attempts=0,error_code=NULL,updated_at=UTC_TIMESTAMP(3) WHERE id=? AND status='processing'")
            .bind(document).execute(&self.pool).await?.rows_affected() == 1)
    }

    pub async fn kb_mark_failed(&self, document: u64, code: &str) -> Result<(), sqlx::Error> {
        sqlx::query("UPDATE kb_documents SET status='failed',error_code=?,updated_at=UTC_TIMESTAMP(3) WHERE id=? AND status='processing'")
            .bind(code).bind(document).execute(&self.pool).await?;
        Ok(())
    }

    /// Counts a failed attempt of a processing document; at `max_attempts` it fails. Returns
    /// the new count, `None` if the document is gone or no longer processing.
    pub async fn kb_record_failure(
        &self,
        document: u64,
        code: &str,
        max_attempts: u32,
    ) -> Result<Option<u32>, sqlx::Error> {
        // status first: MariaDB evaluates assignments left to right.
        let updated = sqlx::query("UPDATE kb_documents SET status=IF(attempts+1>=?,'failed','processing'),attempts=attempts+1,error_code=?,updated_at=UTC_TIMESTAMP(3) WHERE id=? AND status='processing'")
            .bind(max_attempts).bind(code).bind(document).execute(&self.pool).await?.rows_affected();
        if updated == 0 {
            return Ok(None);
        }
        sqlx::query_scalar("SELECT attempts FROM kb_documents WHERE id=?")
            .bind(document)
            .fetch_optional(&self.pool)
            .await
    }

    /// Progress after failures: the count starts again.
    pub async fn kb_reset_attempts(&self, document: u64) -> Result<(), sqlx::Error> {
        sqlx::query("UPDATE kb_documents SET attempts=0,error_code=NULL WHERE id=? AND status='processing' AND attempts>0")
            .bind(document).execute(&self.pool).await?;
        Ok(())
    }
}

/// `ops kb prune-embeddings`.
impl Database {
    /// Stored vectors per provider key.
    pub async fn kb_vector_counts(&self) -> Result<Vec<(String, u64)>, sqlx::Error> {
        let rows = sqlx::query(
            "SELECT provider,COUNT(*) AS n FROM kb_embeddings GROUP BY provider ORDER BY provider",
        )
        .fetch_all(&self.pool)
        .await?;
        rows.iter()
            .map(|row| Ok((row.try_get("provider")?, row.try_get::<i64, _>("n")? as u64)))
            .collect()
    }

    /// Deletes up to `limit` vectors of a provider key (small batches keep transactions short).
    pub async fn kb_delete_vectors(&self, provider: &str, limit: u32) -> Result<u64, sqlx::Error> {
        Ok(
            sqlx::query("DELETE FROM kb_embeddings WHERE provider=? LIMIT ?")
                .bind(provider)
                .bind(limit)
                .execute(&self.pool)
                .await?
                .rows_affected(),
        )
    }
}

async fn usage(connection: &mut sqlx::MySqlConnection, guild: u64) -> Result<Usage, sqlx::Error> {
    let row = sqlx::query("SELECT COUNT(*) AS documents,CAST(COALESCE(SUM(chunk_count),0) AS UNSIGNED) AS chunks FROM kb_documents WHERE guild_id=?")
        .bind(guild).fetch_one(&mut *connection).await?;
    let total: u64 = sqlx::query_scalar(
        "SELECT CAST(COALESCE(SUM(chunk_count),0) AS UNSIGNED) FROM kb_documents",
    )
    .fetch_one(&mut *connection)
    .await?;
    Ok(Usage {
        documents: row.try_get::<i64, _>("documents")? as u64,
        chunks: row.try_get("chunks")?,
        total_chunks: total,
    })
}
