//! The background worker: one task that turns uploaded documents into chunks and vectors, one
//! batch at a time, then fills in the vectors of the other providers. Guilds take turns, and a
//! guild's documents go oldest first. The queue is the database (documents in `processing`), so
//! a restart continues where it stopped; waits and failure counts of the current run live in
//! memory.
//!
//! Rate limits (429, used-up quotas) pause the provider and are never counted as failures, so a
//! low free-tier quota only makes ingestion slow. A refused key pauses the provider too. Other
//! failed batches count against the document (after `MAX_ATTEMPTS` it fails), except while
//! another provider is only rate-limited: that one takes the document later.

use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
    time::{Duration, Instant},
};

use tokio::sync::watch;

use super::{
    Knowledge, chunk,
    embed::{BlockReason, DocumentText, EmbedError, text_tokens, vector_bytes},
    extract::{ExtractError, MediaType},
    store::{PendingChunk, WorkDocument},
};

pub const MAX_ATTEMPTS: u32 = 5;
/// Failures that do not count wait at most `retry_base` × 2^this.
const MAX_UNCOUNTED_DOUBLINGS: u32 = 6;
/// The pause after the database failed.
const DATABASE_RETRY: Duration = Duration::from_secs(30);
/// Processing documents looked at per pass.
const QUEUE_PAGE: u32 = 100;

#[derive(Debug, Clone, Copy)]
pub struct Timing {
    /// The longest sleep without a wake-up.
    pub idle: Duration,
    /// How often ready documents are checked for missing vectors of other providers.
    pub backfill_every: Duration,
    /// After a failed batch the provider waits this long for the document, doubled per
    /// attempt.
    pub retry_base: Duration,
}

impl Default for Timing {
    fn default() -> Self {
        Self {
            idle: Duration::from_secs(60),
            backfill_every: Duration::from_secs(3600),
            retry_base: Duration::from_secs(60),
        }
    }
}

#[derive(Default)]
struct State {
    /// (document, provider index): no new try before then.
    waits: HashMap<(u64, usize), Instant>,
    /// (document, provider index): the provider refused the document's text.
    refused: HashSet<(u64, usize)>,
    /// (document, provider index): failed batches not counted against the document, because
    /// another provider was only rate-limited.
    uncounted: HashMap<(u64, usize), u32>,
    /// The guild of the last processed batch; the next batch goes to the next guild.
    last_guild: Option<u64>,
    /// When to look for missing vectors next (`None`: at once).
    backfill_at: Option<Instant>,
}

impl State {
    /// A document that failed or is gone leaves the queue; a retry (the only way back) starts
    /// clean instead of being refused at once by what this run remembers.
    fn forget(&mut self, document: u64) {
        self.waits.retain(|(id, _), _| *id != document);
        self.refused.retain(|(id, _)| *id != document);
        self.uncounted.retain(|(id, _), _| *id != document);
    }
}

enum Step {
    /// Something was done; look for more at once.
    Again,
    /// Nothing to do for this long (or until woken).
    Sleep(Duration),
}

enum Choice {
    Use(usize),
    /// Every provider that may still embed the document is waiting until then.
    Later(Instant),
    /// Every provider refused the document.
    Refused,
}

enum Batch {
    Stored,
    /// The provider has vectors for every chunk of the document.
    Complete,
    /// The document was deleted (or re-chunked) meanwhile.
    Gone,
    Failed(EmbedError),
}

pub async fn run(knowledge: Arc<Knowledge>, timing: Timing, mut stop: watch::Receiver<bool>) {
    let mut state = State::default();
    tracing::info!(
        providers = knowledge.embedder.providers.len(),
        "knowledge_worker_started"
    );
    loop {
        if *stop.borrow() {
            break;
        }
        // A stop interrupts a request or a pacing pause; what was stored stays stored.
        let step = tokio::select! {
            step = step(&knowledge, &timing, &mut state) => step,
            _ = stop.wait_for(|stop| *stop) => break,
        };
        match step {
            Step::Again => tokio::task::yield_now().await,
            Step::Sleep(wait) => {
                tokio::select! {
                    _ = tokio::time::sleep(wait) => {}
                    _ = knowledge.wake.notified() => {}
                    _ = stop.wait_for(|stop| *stop) => break,
                }
            }
        }
    }
    tracing::info!("knowledge_worker_stopped");
}

fn earliest(current: Option<Instant>, at: Instant) -> Option<Instant> {
    Some(current.map_or(at, |current| current.min(at)))
}

async fn step(knowledge: &Knowledge, timing: &Timing, state: &mut State) -> Step {
    let now = Instant::now();
    state.waits.retain(|_, until| *until > now);
    let mut documents = match knowledge.db.kb_processing(QUEUE_PAGE).await {
        Ok(documents) => documents,
        Err(_) => {
            tracing::warn!("knowledge_queue_read_failed");
            return Step::Sleep(DATABASE_RETRY);
        }
    };
    // Guilds take turns, a batch each, so one guild's large upload does not hold up the others.
    // The sort is stable: a guild's documents stay oldest first.
    if let Some(last) = state.last_guild {
        documents.sort_by_key(|document| (document.guild_id <= last, document.guild_id));
    }
    let mut next = None;
    for document in &documents {
        match choose(knowledge, state, document.id, now) {
            Choice::Use(index) => {
                state.last_guild = Some(document.guild_id);
                return process(knowledge, timing, state, document, index).await;
            }
            Choice::Later(at) => next = earliest(next, at),
            Choice::Refused => {
                let code = EmbedError::BadInput.to_string();
                return match knowledge.db.kb_mark_failed(document.id, &code).await {
                    Ok(()) => {
                        tracing::warn!(
                            document_id = document.id,
                            error_code = %code,
                            "kb_document_failed"
                        );
                        state.forget(document.id);
                        Step::Again
                    }
                    Err(_) => Step::Sleep(DATABASE_RETRY),
                };
            }
        }
    }
    // New documents come first; vectors of the other providers are filled in afterwards.
    if state.backfill_at.is_none_or(|at| at <= now) {
        match backfill(knowledge, timing, state, now).await {
            Ok(Some(step)) => return step,
            Ok(None) => {}
            Err(()) => state.backfill_at = Some(now + DATABASE_RETRY),
        }
    }
    let mut wake = now + timing.idle;
    for at in next.into_iter().chain(state.backfill_at) {
        wake = wake.min(at);
    }
    // Never spin: a deadline that already passed still sleeps a little.
    Step::Sleep(
        wake.saturating_duration_since(now)
            .max(Duration::from_millis(50)),
    )
}

/// The first provider (in priority order) that may embed the document now. Short waits for a
/// minute's pacing do not count here; they are waited out in `embed_batch`.
fn choose(knowledge: &Knowledge, state: &State, document: u64, now: Instant) -> Choice {
    let mut later = None;
    let mut usable = false;
    for index in 0..knowledge.embedder.providers.len() {
        if state.refused.contains(&(document, index)) {
            continue;
        }
        usable = true;
        match unavailable_until(knowledge, state, document, index, now) {
            None => return Choice::Use(index),
            Some(at) => later = earliest(later, at),
        }
    }
    match later {
        Some(at) => Choice::Later(at),
        None if usable => Choice::Later(now),
        None => Choice::Refused,
    }
}

/// When the provider can work on the document again: after a pause, the daily amount or a
/// failed attempt. `None` if it can now.
fn unavailable_until(
    knowledge: &Knowledge,
    state: &State,
    document: u64,
    index: usize,
    now: Instant,
) -> Option<Instant> {
    let mut pacer = knowledge.embedder.providers[index].pacer();
    [
        pacer.block(now).map(|block| block.until),
        pacer.daily_wait(now),
        state.waits.get(&(document, index)).copied(),
    ]
    .into_iter()
    .flatten()
    .max()
}

async fn process(
    knowledge: &Knowledge,
    timing: &Timing,
    state: &mut State,
    document: &WorkDocument,
    index: usize,
) -> Step {
    match ensure_chunks(knowledge, document).await {
        Ok(true) => {}
        Ok(false) => return Step::Again,
        Err(()) => return Step::Sleep(DATABASE_RETRY),
    }
    let result = match embed_batch(knowledge, document, index).await {
        Ok(result) => result,
        Err(()) => return Step::Sleep(DATABASE_RETRY),
    };
    let database = &knowledge.db;
    if matches!(result, Batch::Stored | Batch::Complete) {
        state.uncounted.retain(|(id, _), _| *id != document.id);
    }
    match result {
        Batch::Gone => state.forget(document.id),
        Batch::Stored => {
            if document.attempts > 0 && database.kb_reset_attempts(document.id).await.is_err() {
                return Step::Sleep(DATABASE_RETRY);
            }
        }
        Batch::Complete => match database.kb_mark_ready(document.id).await {
            Ok(true) => {
                tracing::info!(
                    document_id = document.id,
                    provider = knowledge.embedder.providers[index].kind().as_str(),
                    "kb_document_ready"
                );
                state.waits.retain(|(id, _), _| *id != document.id);
                // The other providers fill in its vectors as soon as the queue is idle, so
                // searches find it with whichever provider they use.
                state.backfill_at = None;
            }
            Ok(false) => {}
            Err(_) => return Step::Sleep(DATABASE_RETRY),
        },
        // The provider is paused (Provider::settle); another one or a later pass continues.
        Batch::Failed(EmbedError::RateLimit { .. } | EmbedError::Auth) => {}
        Batch::Failed(EmbedError::BadInput) => {
            state.refused.insert((document.id, index));
        }
        Batch::Failed(_) if rate_limited_elsewhere(knowledge, state, document.id, index) => {
            let failures = state.uncounted.entry((document.id, index)).or_insert(0);
            *failures += 1;
            let wait = backoff(timing, *failures, MAX_UNCOUNTED_DOUBLINGS);
            state
                .waits
                .insert((document.id, index), Instant::now() + wait);
        }
        Batch::Failed(error) => {
            let code = error.to_string();
            match database
                .kb_record_failure(document.id, &code, MAX_ATTEMPTS)
                .await
            {
                Ok(Some(attempts)) if attempts >= MAX_ATTEMPTS => {
                    tracing::warn!(
                        document_id = document.id,
                        error_code = %code,
                        attempts,
                        "kb_document_failed"
                    );
                    state.forget(document.id);
                }
                Ok(Some(attempts)) => {
                    let wait = backoff(timing, attempts, 10);
                    state
                        .waits
                        .insert((document.id, index), Instant::now() + wait);
                }
                // Deleted, or no longer processing.
                Ok(None) => state.forget(document.id),
                Err(_) => return Step::Sleep(DATABASE_RETRY),
            }
        }
    }
    Step::Again
}

/// `retry_base` doubled per failure after the first, at most `max_doublings` times.
fn backoff(timing: &Timing, failures: u32, max_doublings: u32) -> Duration {
    timing
        .retry_base
        .saturating_mul(1 << failures.saturating_sub(1).min(max_doublings))
}

/// Whether another provider that may still embed the document is paused only by a rate limit
/// (a 429 or the daily amount): it takes the document once that ends, so the failures of the
/// provider standing in for it do not count, just as the rate limit itself does not.
fn rate_limited_elsewhere(
    knowledge: &Knowledge,
    state: &State,
    document: u64,
    index: usize,
) -> bool {
    let now = Instant::now();
    knowledge
        .embedder
        .providers
        .iter()
        .enumerate()
        .filter(|(other, _)| *other != index && !state.refused.contains(&(document, *other)))
        .any(|(_, provider)| {
            let mut pacer = provider.pacer();
            pacer
                .block(now)
                .is_some_and(|block| block.reason == BlockReason::RateLimit)
                || pacer.daily_wait(now).is_some()
        })
}

/// Makes sure the document's chunks exist, rebuilding them in one transaction if they are
/// missing or incomplete. False if the document is gone or has no text.
async fn ensure_chunks(knowledge: &Knowledge, document: &WorkDocument) -> Result<bool, ()> {
    let database = &knowledge.db;
    let rows = database.kb_chunk_rows(document.id).await.map_err(|_| ())?;
    if rows > 0 && rows == document.chunk_count {
        return Ok(true);
    }
    let Some(content) = database.kb_content(document.id).await.map_err(|_| ())? else {
        return Ok(false);
    };
    let markdown = MediaType::parse(&document.media_type).is_some_and(MediaType::is_markdown);
    let chunks = chunk::chunk(&content, markdown);
    if chunks.is_empty() {
        database
            .kb_mark_failed(document.id, ExtractError::Empty.code())
            .await
            .map_err(|_| ())?;
        return Ok(false);
    }
    let stored = database
        .kb_replace_chunks(document.id, document.guild_id, &chunks)
        .await
        .map_err(|_| ())?;
    if stored {
        tracing::info!(
            document_id = document.id,
            chunks = chunks.len(),
            "kb_document_chunked"
        );
    }
    Ok(stored)
}

/// The text embedded for a chunk: its heading path, then its content.
fn chunk_text(chunk: &PendingChunk) -> String {
    match &chunk.heading {
        Some(heading) => format!("{heading}\n{}", chunk.content),
        None => chunk.content.clone(),
    }
}

/// Embeds and stores the next batch of the document's chunks that lack this provider's vector,
/// after waiting for the provider's pacing. Err only for database failures.
async fn embed_batch(
    knowledge: &Knowledge,
    document: &WorkDocument,
    index: usize,
) -> Result<Batch, ()> {
    let provider = &knowledge.embedder.providers[index];
    let database = &knowledge.db;
    let pending = database
        .kb_pending_chunks(document.id, provider.key(), provider.batch_size() as u32)
        .await
        .map_err(|_| ())?;
    if pending.is_empty() {
        return Ok(Batch::Complete);
    }
    let texts: Vec<String> = pending.iter().map(chunk_text).collect();
    let inputs: Vec<DocumentText> = texts
        .iter()
        .map(|text| DocumentText {
            title: &document.title,
            text,
        })
        .collect();
    let costs: Vec<u32> = inputs.iter().map(text_tokens).collect();
    let (count, wait) = {
        let now = Instant::now();
        let mut pacer = provider.pacer();
        let count = pacer.batch_len(&costs, provider.batch_size(), now);
        let tokens = costs[..count].iter().sum();
        (count, pacer.minute_wait(count as u32, tokens, now))
    };
    if !wait.is_zero() {
        tokio::time::sleep(wait).await;
    }
    let vectors = match provider.embed_documents(&inputs[..count]).await {
        Ok(vectors) => vectors,
        Err(error) => return Ok(Batch::Failed(error)),
    };
    let rows: Vec<(u64, Vec<u8>)> = pending
        .iter()
        .zip(&vectors)
        .map(|(chunk, vector)| (chunk.id, vector_bytes(vector)))
        .collect();
    if !database
        .kb_store_vectors(document.guild_id, provider.key(), &rows)
        .await
        .map_err(|_| ())?
    {
        return Ok(Batch::Gone);
    }
    let rest = database
        .kb_pending_chunks(document.id, provider.key(), 1)
        .await
        .map_err(|_| ())?;
    Ok(if rest.is_empty() {
        Batch::Complete
    } else {
        Batch::Stored
    })
}

/// One batch of missing vectors for a ready document, if some provider can do one now.
/// `Ok(None)` when there is nothing to do now; the next check is then scheduled.
async fn backfill(
    knowledge: &Knowledge,
    timing: &Timing,
    state: &mut State,
    now: Instant,
) -> Result<Option<Step>, ()> {
    let mut later = None;
    for (index, provider) in knowledge.embedder.providers.iter().enumerate() {
        {
            let mut pacer = provider.pacer();
            if let Some(at) = [
                pacer.block(now).map(|block| block.until),
                pacer.daily_wait(now),
            ]
            .into_iter()
            .flatten()
            .max()
            {
                later = earliest(later, at);
                continue;
            }
        }
        let documents = knowledge
            .db
            .kb_backfill(provider.key(), QUEUE_PAGE)
            .await
            .map_err(|_| ())?;
        for document in &documents {
            if state.refused.contains(&(document.id, index)) {
                continue;
            }
            if let Some(until) = state.waits.get(&(document.id, index)) {
                later = earliest(later, *until);
                continue;
            }
            match embed_batch(knowledge, document, index).await? {
                Batch::Stored => {}
                Batch::Gone => state.forget(document.id),
                Batch::Complete => tracing::info!(
                    document_id = document.id,
                    provider = provider.kind().as_str(),
                    "kb_document_backfilled"
                ),
                Batch::Failed(EmbedError::RateLimit { .. } | EmbedError::Auth) => {}
                Batch::Failed(EmbedError::BadInput) => {
                    state.refused.insert((document.id, index));
                }
                // The document stays searchable through its other vectors; try again later.
                Batch::Failed(_) => {
                    state
                        .waits
                        .insert((document.id, index), now + timing.backfill_every);
                }
            }
            return Ok(Some(Step::Again));
        }
    }
    // Paused providers may still have work: look again once the first pause ends.
    state.backfill_at = Some(match later {
        Some(at) => at.min(now + timing.backfill_every),
        None => now + timing.backfill_every,
    });
    Ok(None)
}
