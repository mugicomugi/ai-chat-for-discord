//! Embedding providers (Gemini, OpenAI, Ollama): request shapes, error classes, vector checks,
//! query failover, and the per-provider pacing that keeps background ingestion within each
//! provider's limits. Request and response bodies, keys and texts are never logged.

use std::{
    collections::VecDeque,
    sync::{Mutex, MutexGuard},
    time::{Duration, Instant},
};

use chrono::{DateTime, Utc};
use reqwest::{Client, header::HeaderMap};
use serde::{Deserialize, de::DeserializeOwned};
use serde_json::{Value, json};

use super::{SearchError, store::Hit};
use crate::{
    bounded::{self, BodyError},
    config::{Pacing, ProviderConfig, ProviderKind},
};

/// Every stored vector has this many dimensions (the VECTOR(768) column).
pub const DIMENSIONS: usize = 768;
/// The longest pause after a rate limit; quotas that last longer are probed once an hour.
pub const MAX_BACKOFF: Duration = Duration::from_secs(3600);
/// The first pause after a rate limit without a usable Retry-After; it doubles each time.
const FIRST_BACKOFF: Duration = Duration::from_secs(60);
/// The pause after the provider refused the API key.
const AUTH_BACKOFF: Duration = Duration::from_secs(600);
const MINUTE: Duration = Duration::from_secs(60);
const DAY: Duration = Duration::from_secs(86_400);
const MAX_ERROR_BODY: usize = 64 * 1024;
/// Queries are cut to this many characters (Gemini accepts about 2,048 tokens).
pub const MAX_QUERY_CHARS: usize = 1_500;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum EmbedError {
    /// The key was refused, or the project may not use the API.
    #[error("embedding_auth")]
    Auth,
    /// 429, or a used-up quota. Never counts as a failure of a document.
    #[error("embedding_rate_limit")]
    RateLimit { retry_after: Option<Duration> },
    /// Network problems, timeouts and server errors.
    #[error("embedding_upstream")]
    Upstream,
    /// The provider refused the text itself.
    #[error("embedding_rejected")]
    BadInput,
    /// A response with the wrong shape, count or dimension, or non-finite values.
    #[error("embedding_invalid")]
    Invalid,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Task {
    Document,
    Query,
}

/// A text to embed for storage, with its document's title.
#[derive(Debug, Clone, Copy)]
pub struct DocumentText<'a> {
    pub title: &'a str,
    pub text: &'a str,
}

pub struct Provider {
    config: ProviderConfig,
    key: String,
    client: Client,
    pacer: Mutex<Pacer>,
}

/// The configured providers, in priority order.
pub struct Embedder {
    pub providers: Vec<Provider>,
}

impl Embedder {
    pub fn new(configs: &[ProviderConfig]) -> Result<Self, reqwest::Error> {
        let client = Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(60))
            .redirect(reqwest::redirect::Policy::none())
            .build()?;
        Ok(Self {
            providers: configs
                .iter()
                .map(|config| Provider::new(config.clone(), client.clone()))
                .collect(),
        })
    }
}

impl Embedder {
    /// Asks the providers at `order` (indices into `providers`) in turn for a query vector and
    /// passes it (as VECTOR bytes) with the provider key to `lookup`, until one lookup finds
    /// something. A provider that is paused, fails or takes longer than `timeout` is skipped.
    /// Empty when providers answered but no lookup found anything; `Unavailable` when none
    /// answered.
    pub async fn search<F, Fut>(
        &self,
        query: &str,
        order: &[usize],
        timeout: Duration,
        mut lookup: F,
    ) -> Result<Vec<Hit>, SearchError>
    where
        F: FnMut(String, Vec<u8>) -> Fut,
        Fut: Future<Output = Result<Vec<Hit>, sqlx::Error>>,
    {
        let mut answered = false;
        for provider in order.iter().filter_map(|index| self.providers.get(*index)) {
            let vector = match tokio::time::timeout(timeout, provider.embed_query(query)).await {
                Ok(Ok(vector)) => vector,
                Ok(Err(error)) => {
                    tracing::info!(provider = provider.kind().as_str(), error_code = %error, "knowledge_query_provider_skipped");
                    continue;
                }
                Err(_) => {
                    tracing::warn!(
                        provider = provider.kind().as_str(),
                        "knowledge_query_timeout"
                    );
                    continue;
                }
            };
            answered = true;
            let hits = lookup(provider.key().to_owned(), vector_bytes(&vector))
                .await
                .map_err(|_| SearchError::Database)?;
            if !hits.is_empty() {
                return Ok(hits);
            }
        }
        if answered {
            Ok(Vec::new())
        } else {
            Err(SearchError::Unavailable)
        }
    }
}

impl Provider {
    pub fn new(config: ProviderConfig, client: Client) -> Self {
        Self {
            key: config.key(),
            pacer: Mutex::new(Pacer::new(config.pacing)),
            config,
            client,
        }
    }

    /// The label stored with this provider's vectors.
    pub fn key(&self) -> &str {
        &self.key
    }

    pub fn kind(&self) -> ProviderKind {
        self.config.kind
    }

    pub fn label(&self) -> String {
        format!("{} ({})", self.config.kind.label(), self.config.model)
    }

    pub fn batch_size(&self) -> usize {
        self.config.batch_size
    }

    pub fn pacer(&self) -> MutexGuard<'_, Pacer> {
        self.pacer.lock().expect("pacer mutex poisoned")
    }

    /// Embeds texts for storage. The caller paces; this records the use and pauses the
    /// provider after a rate limit or a refused key.
    pub async fn embed_documents(
        &self,
        texts: &[DocumentText<'_>],
    ) -> Result<Vec<Vec<f32>>, EmbedError> {
        let tokens = texts.iter().map(|text| text_tokens(text)).sum();
        self.pacer()
            .record(texts.len() as u32, tokens, Instant::now());
        let result = self.request(Task::Document, texts).await;
        self.settle(&result);
        result
    }

    /// One query vector for a search. While the provider is paused this fails at once,
    /// without a request, so the search moves on to the next provider.
    pub async fn embed_query(&self, text: &str) -> Result<Vec<f32>, EmbedError> {
        let now = Instant::now();
        if let Some(block) = self.pacer().block(now) {
            return Err(match block.reason {
                BlockReason::Auth => EmbedError::Auth,
                BlockReason::RateLimit => EmbedError::RateLimit {
                    retry_after: Some(block.until - now),
                },
            });
        }
        let text: String = text.chars().take(MAX_QUERY_CHARS).collect();
        let input = [DocumentText {
            title: "",
            text: &text,
        }];
        self.pacer().record(1, text_tokens(&input[0]), now);
        let result = self
            .request(Task::Query, &input)
            .await
            .map(|mut vectors| vectors.remove(0));
        self.settle(&result);
        result
    }

    fn settle<T>(&self, result: &Result<T, EmbedError>) {
        let now = Instant::now();
        let mut pacer = self.pacer();
        match result {
            Ok(_) => pacer.succeeded(),
            Err(EmbedError::RateLimit { retry_after }) => {
                let wait = pacer.rate_limited(*retry_after, now);
                tracing::info!(
                    provider = self.config.kind.as_str(),
                    wait_seconds = wait.as_secs(),
                    "embedding_rate_limited"
                );
            }
            Err(EmbedError::Auth) => {
                pacer.auth_failed(now);
                tracing::error!(
                    provider = self.config.kind.as_str(),
                    "embedding_auth_failed"
                );
            }
            Err(error) => tracing::warn!(
                provider = self.config.kind.as_str(),
                error_code = %error,
                "embedding_request_failed"
            ),
        }
    }

    async fn request(
        &self,
        task: Task,
        texts: &[DocumentText<'_>],
    ) -> Result<Vec<Vec<f32>>, EmbedError> {
        let config = &self.config;
        let base = &config.base_url;
        // OpenAI and Ollama have no title field; the title leads the text instead.
        let inputs = || -> Vec<String> {
            texts
                .iter()
                .map(|text| {
                    if text.title.is_empty() {
                        text.text.to_owned()
                    } else {
                        format!("{}\n{}", text.title, text.text)
                    }
                })
                .collect()
        };
        let request = match config.kind {
            ProviderKind::Gemini => {
                let model = format!("models/{}", config.model);
                let requests: Vec<Value> = texts
                    .iter()
                    .map(|text| {
                        let mut request = json!({
                            "model": model,
                            "content": {"parts": [{"text": text.text}]},
                            "taskType": match task {
                                Task::Document => "RETRIEVAL_DOCUMENT",
                                Task::Query => "RETRIEVAL_QUERY",
                            },
                            "outputDimensionality": DIMENSIONS,
                        });
                        // Gemini takes a title only for documents.
                        if task == Task::Document && !text.title.is_empty() {
                            request["title"] = json!(text.title);
                        }
                        request
                    })
                    .collect();
                self.client
                    .post(format!("{base}/v1beta/{model}:batchEmbedContents"))
                    .header("x-goog-api-key", &config.api_key)
                    .json(&json!({"requests": requests}))
            }
            ProviderKind::OpenAi => self
                .client
                .post(format!("{base}/v1/embeddings"))
                .bearer_auth(&config.api_key)
                .json(&json!({
                    "model": config.model,
                    "input": inputs(),
                    "dimensions": DIMENSIONS,
                    "encoding_format": "float",
                })),
            ProviderKind::Ollama => self
                .client
                .post(format!("{base}/api/embed"))
                .bearer_auth(&config.api_key)
                .json(&json!({
                    "model": config.model,
                    "input": inputs(),
                    "dimensions": DIMENSIONS,
                })),
        };
        let response = request.send().await.map_err(|_| EmbedError::Upstream)?;
        let status = response.status();
        if !status.is_success() {
            let headers = response.headers().clone();
            let body = error_body(response).await;
            let error = classify(status.as_u16(), &headers, &body, Utc::now());
            tracing::debug!(http_status = status.as_u16(), error_code = %error, "embedding_http_error");
            return Err(error);
        }
        // Pretty-printed JSON takes about 20 bytes per number.
        let limit = 64 * 1024 + texts.len() * 48 * 1024;
        let raw: Vec<Vec<f64>> = match config.kind {
            ProviderKind::Gemini => {
                #[derive(Deserialize)]
                struct Response {
                    embeddings: Vec<Embedding>,
                }
                #[derive(Deserialize)]
                struct Embedding {
                    values: Vec<f64>,
                }
                read::<Response>(response, limit)
                    .await?
                    .embeddings
                    .into_iter()
                    .map(|embedding| embedding.values)
                    .collect()
            }
            ProviderKind::OpenAi => {
                #[derive(Deserialize)]
                struct Response {
                    data: Vec<Embedding>,
                }
                #[derive(Deserialize)]
                struct Embedding {
                    index: usize,
                    embedding: Vec<f64>,
                }
                let mut data = read::<Response>(response, limit).await?.data;
                data.sort_by_key(|embedding| embedding.index);
                if data
                    .iter()
                    .enumerate()
                    .any(|(i, embedding)| embedding.index != i)
                {
                    return Err(EmbedError::Invalid);
                }
                data.into_iter()
                    .map(|embedding| embedding.embedding)
                    .collect()
            }
            ProviderKind::Ollama => {
                #[derive(Deserialize)]
                struct Response {
                    embeddings: Vec<Vec<f64>>,
                }
                read::<Response>(response, limit).await?.embeddings
            }
        };
        if raw.len() != texts.len() {
            return Err(EmbedError::Invalid);
        }
        raw.iter().map(|values| unit_vector(values)).collect()
    }

    pub fn status(&self) -> ProviderStatus {
        let now = Instant::now();
        let mut pacer = self.pacer();
        let (state, until) = match pacer.block(now) {
            Some(block) => (
                match block.reason {
                    BlockReason::RateLimit => ProviderState::RateLimited,
                    BlockReason::Auth => ProviderState::AuthError,
                },
                Some(block.until),
            ),
            None => match pacer.daily_wait(now) {
                Some(until) => (ProviderState::DailyLimit, Some(until)),
                None => (ProviderState::Ok, None),
            },
        };
        ProviderStatus {
            key: self.key.clone(),
            label: self.label(),
            state,
            until: until.map(|until| {
                Utc::now() + chrono::Duration::from_std(until - now).unwrap_or_default()
            }),
        }
    }
}

async fn read<T: DeserializeOwned>(
    response: reqwest::Response,
    limit: usize,
) -> Result<T, EmbedError> {
    bounded::json(response, limit)
        .await
        .map_err(|error| match error {
            BodyError::Transport(_) => EmbedError::Upstream,
            BodyError::Invalid => EmbedError::Invalid,
        })
}

/// The start of an error response, for classifying it only.
async fn error_body(mut response: reqwest::Response) -> Vec<u8> {
    let mut body = Vec::new();
    while let Ok(Some(chunk)) = response.chunk().await {
        if body.len() + chunk.len() > MAX_ERROR_BODY {
            break;
        }
        body.extend_from_slice(&chunk);
    }
    body
}

/// What a provider's error response means for the caller.
pub fn classify(status: u16, headers: &HeaderMap, body: &[u8], now: DateTime<Utc>) -> EmbedError {
    let details = Details::parse(body);
    match status {
        429 | 402 => {
            let mut wait = retry_after(headers, now).or(details.retry_delay);
            // A daily quota or an empty balance does not come back within seconds, whatever
            // the response suggests.
            if details.long_quota || status == 402 {
                wait = Some(wait.unwrap_or_default().max(MAX_BACKOFF));
            }
            EmbedError::RateLimit { retry_after: wait }
        }
        401 | 403 => EmbedError::Auth,
        400 if details.refused_project => EmbedError::Auth,
        400 | 413 | 422 => EmbedError::BadInput,
        _ => EmbedError::Upstream,
    }
}

/// The parts of Gemini's and OpenAI's error bodies that matter.
#[derive(Default)]
struct Details {
    retry_delay: Option<Duration>,
    /// A daily quota (Gemini) or no credit left (OpenAI's insufficient_quota).
    long_quota: bool,
    /// Gemini answers an invalid key, or a project or region that may not use the API, with
    /// 400.
    refused_project: bool,
}

impl Details {
    fn parse(body: &[u8]) -> Self {
        let mut details = Self::default();
        let Ok(value) = serde_json::from_slice::<Value>(body) else {
            return details;
        };
        let error = &value["error"];
        if matches!(
            error["status"].as_str(),
            Some("FAILED_PRECONDITION" | "PERMISSION_DENIED" | "UNAUTHENTICATED")
        ) {
            details.refused_project = true;
        }
        if error["code"].as_str() == Some("insufficient_quota")
            || error["type"].as_str() == Some("insufficient_quota")
        {
            details.long_quota = true;
        }
        for detail in error["details"].as_array().into_iter().flatten() {
            let kind = detail["@type"].as_str().unwrap_or_default();
            if kind.ends_with("RetryInfo") {
                details.retry_delay = detail["retryDelay"]
                    .as_str()
                    .and_then(|delay| delay.strip_suffix('s'))
                    .and_then(|seconds| seconds.parse::<f64>().ok())
                    .filter(|seconds| seconds.is_finite() && *seconds >= 0.0)
                    .map(|seconds| Duration::from_secs_f64(seconds.min(1e6)));
            } else if kind.ends_with("QuotaFailure") {
                details.long_quota |=
                    detail["violations"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .any(|violation| {
                            violation["quotaId"]
                                .as_str()
                                .is_some_and(|id| id.contains("PerDay"))
                        });
            } else if kind.ends_with("ErrorInfo") && detail["reason"] == "API_KEY_INVALID" {
                details.refused_project = true;
            }
        }
        details
    }
}

/// `retry-after-ms` (OpenAI), or `Retry-After` in seconds or as an HTTP date.
fn retry_after(headers: &HeaderMap, now: DateTime<Utc>) -> Option<Duration> {
    let header = |name: &str| headers.get(name)?.to_str().ok().map(str::trim);
    if let Some(ms) = header("retry-after-ms").and_then(|v| v.parse::<f64>().ok())
        && ms.is_finite()
        && ms >= 0.0
    {
        return Some(Duration::from_secs_f64(ms.min(1e9) / 1000.0));
    }
    let value = header("retry-after")?;
    if let Ok(seconds) = value.parse::<f64>() {
        return (seconds.is_finite() && seconds >= 0.0)
            .then(|| Duration::from_secs_f64(seconds.min(1e6)));
    }
    let at = DateTime::parse_from_rfc2822(value)
        .ok()?
        .with_timezone(&Utc);
    Some((at - now).to_std().unwrap_or_default())
}

/// Checks a provider's vector and scales it to unit length (Gemini does not normalize vectors
/// it shortened to 768 dimensions), so the cosine distances of all providers are comparable.
pub fn unit_vector(values: &[f64]) -> Result<Vec<f32>, EmbedError> {
    if values.len() != DIMENSIONS {
        return Err(EmbedError::Invalid);
    }
    let values: Vec<f32> = values.iter().map(|value| *value as f32).collect();
    if values.iter().any(|value| !value.is_finite()) {
        return Err(EmbedError::Invalid);
    }
    let norm = values
        .iter()
        .map(|value| f64::from(*value).powi(2))
        .sum::<f64>()
        .sqrt();
    if !norm.is_finite() || norm < 1e-12 {
        return Err(EmbedError::Invalid);
    }
    Ok(values
        .iter()
        .map(|value| (f64::from(*value) / norm) as f32)
        .collect())
}

/// The bytes of a VECTOR value: little-endian 32-bit floats.
pub fn vector_bytes(vector: &[f32]) -> Vec<u8> {
    vector
        .iter()
        .flat_map(|value| value.to_le_bytes())
        .collect()
}

/// Tokens a text may cost, from its characters: one per non-ASCII character (Japanese is about
/// one each) and one per three ASCII characters (English is about four), plus a little per
/// request item. Deliberately on the high side.
pub fn estimate_tokens(text: &str) -> u32 {
    let (ascii, other) = text.chars().fold((0_u32, 0_u32), |(ascii, other), c| {
        if c.is_ascii() {
            (ascii.saturating_add(1), other)
        } else {
            (ascii, other.saturating_add(1))
        }
    });
    other.saturating_add(ascii.div_ceil(3)).saturating_add(4)
}

pub fn text_tokens(text: &DocumentText<'_>) -> u32 {
    estimate_tokens(text.title).saturating_add(estimate_tokens(text.text))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockReason {
    RateLimit,
    Auth,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Block {
    pub until: Instant,
    pub reason: BlockReason,
}

/// What the web UI shows about a provider.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderStatus {
    pub key: String,
    pub label: String,
    pub state: ProviderState,
    pub until: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderState {
    Ok,
    /// Paused after a 429 or a used-up quota.
    RateLimited,
    /// Paused after the key was refused.
    AuthError,
    /// The configured daily amount for ingestion is used up.
    DailyLimit,
}

impl ProviderState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::RateLimited => "rate_limited",
            Self::AuthError => "auth_error",
            Self::DailyLimit => "daily_limit",
        }
    }
}

/// Sliding windows of what was sent to one provider in the last minute and day (in memory: a
/// restart forgets them, and the provider's own 429s cover that), and the pause after a rate
/// limit or a refused key. Queries are recorded too, so ingestion leaves them room.
pub struct Pacer {
    pacing: Pacing,
    minute: VecDeque<(Instant, u32, u32)>,
    day: VecDeque<(Instant, u32)>,
    block: Option<Block>,
    /// Rate limits in a row, for the backoff without Retry-After.
    streak: u32,
}

impl Pacer {
    pub fn new(pacing: Pacing) -> Self {
        Self {
            pacing,
            minute: VecDeque::new(),
            day: VecDeque::new(),
            block: None,
            streak: 0,
        }
    }

    fn expire(&mut self, now: Instant) {
        let old = |at: Instant, window| now.saturating_duration_since(at) >= window;
        while self
            .minute
            .front()
            .is_some_and(|entry| old(entry.0, MINUTE))
        {
            self.minute.pop_front();
        }
        while self.day.front().is_some_and(|entry| old(entry.0, DAY)) {
            self.day.pop_front();
        }
    }

    /// The pause in force, if any.
    pub fn block(&self, now: Instant) -> Option<Block> {
        self.block.filter(|block| block.until > now)
    }

    /// When the daily amount lets another text through; `None` if it does now.
    pub fn daily_wait(&mut self, now: Instant) -> Option<Instant> {
        let cap = self.pacing.requests_per_day;
        if cap == 0 {
            return None;
        }
        self.expire(now);
        let mut used: u32 = self.day.iter().map(|entry| entry.1).sum();
        if used < cap {
            return None;
        }
        for (at, items) in &self.day {
            used -= items;
            if used < cap {
                return Some(*at + DAY);
            }
        }
        Some(now + DAY)
    }

    /// How many of the next texts (their token costs in order) one request may carry: within
    /// the batch size, a minute's amounts and what is left of today's. At least one.
    pub fn batch_len(&mut self, costs: &[u32], batch_size: usize, now: Instant) -> usize {
        self.expire(now);
        let mut limit = batch_size.min(self.pacing.requests_per_minute as usize);
        if self.pacing.requests_per_day > 0 {
            let used: u32 = self.day.iter().map(|entry| entry.1).sum();
            limit = limit.min(self.pacing.requests_per_day.saturating_sub(used) as usize);
        }
        let mut tokens = 0_u32;
        let mut count = 0;
        for cost in costs.iter().take(limit) {
            tokens = tokens.saturating_add(*cost);
            if tokens > self.pacing.tokens_per_minute {
                break;
            }
            count += 1;
        }
        count.max(1).min(costs.len())
    }

    /// How long to wait until `items` texts of `tokens` fit into the last minute's amounts.
    pub fn minute_wait(&mut self, items: u32, tokens: u32, now: Instant) -> Duration {
        self.expire(now);
        let (mut used_items, mut used_tokens) =
            self.minute.iter().fold((0_u32, 0_u32), |(i, t), entry| {
                (i + entry.1, t.saturating_add(entry.2))
            });
        let fits = |used_items: u32, used_tokens: u32| {
            used_items + items <= self.pacing.requests_per_minute
                && used_tokens.saturating_add(tokens) <= self.pacing.tokens_per_minute
        };
        if fits(used_items, used_tokens) {
            return Duration::ZERO;
        }
        for (at, sent_items, sent_tokens) in &self.minute {
            used_items -= sent_items;
            used_tokens -= sent_tokens;
            if fits(used_items, used_tokens) {
                return (*at + MINUTE).saturating_duration_since(now);
            }
        }
        MINUTE
    }

    pub fn record(&mut self, items: u32, tokens: u32, now: Instant) {
        self.expire(now);
        self.minute.push_back((now, items, tokens));
        if self.pacing.requests_per_day > 0 {
            self.day.push_back((now, items));
        }
    }

    /// Pauses after a rate limit: Retry-After if given (1 second to 1 hour), else 1, 2, 4 …
    /// minutes up to an hour. Returns the pause.
    pub fn rate_limited(&mut self, retry_after: Option<Duration>, now: Instant) -> Duration {
        self.streak = self.streak.saturating_add(1);
        let wait = match retry_after {
            Some(wait) => wait.clamp(Duration::from_secs(1), MAX_BACKOFF),
            None => FIRST_BACKOFF
                .saturating_mul(1 << (self.streak - 1).min(6))
                .min(MAX_BACKOFF),
        };
        self.pause(now + wait, BlockReason::RateLimit);
        wait
    }

    pub fn auth_failed(&mut self, now: Instant) {
        self.pause(now + AUTH_BACKOFF, BlockReason::Auth);
    }

    pub fn succeeded(&mut self) {
        self.streak = 0;
    }

    fn pause(&mut self, until: Instant, reason: BlockReason) {
        let until = match self.block {
            Some(block) if block.until > until => block.until,
            _ => until,
        };
        self.block = Some(Block { until, reason });
    }
}

#[cfg(test)]
mod tests {
    use reqwest::header::HeaderValue;

    use super::*;

    fn pacing(rpm: u32, tpm: u32, rpd: u32) -> Pacing {
        Pacing {
            requests_per_minute: rpm,
            tokens_per_minute: tpm,
            requests_per_day: rpd,
        }
    }

    #[test]
    fn vectors_are_checked_normalized_and_little_endian() {
        let mut values = vec![0.0; DIMENSIONS];
        values[0] = 3.0;
        values[1] = 4.0;
        let vector = unit_vector(&values).unwrap();
        assert_eq!(vector.len(), DIMENSIONS);
        assert!((vector[0] - 0.6).abs() < 1e-6 && (vector[1] - 0.8).abs() < 1e-6);
        let norm: f32 = vector.iter().map(|v| v * v).sum();
        assert!((norm - 1.0).abs() < 1e-5);
        for bad in [
            vec![1.0; DIMENSIONS - 1],
            vec![1.0; DIMENSIONS + 1],
            vec![0.0; DIMENSIONS],
            {
                let mut v = vec![1.0; DIMENSIONS];
                v[5] = f64::NAN;
                v
            },
            {
                // Beyond f32: infinite once converted.
                let mut v = vec![1.0; DIMENSIONS];
                v[5] = 1e39;
                v
            },
        ] {
            assert_eq!(unit_vector(&bad), Err(EmbedError::Invalid));
        }
        let bytes = vector_bytes(&[1.0, -2.0]);
        assert_eq!(bytes, [0, 0, 0x80, 0x3f, 0, 0, 0, 0xc0]);
        assert_eq!(vector_bytes(&vector).len(), DIMENSIONS * 4);
    }

    #[test]
    fn tokens_are_estimated_from_characters() {
        assert_eq!(estimate_tokens(""), 4);
        assert_eq!(estimate_tokens("abcdef"), 6);
        assert_eq!(estimate_tokens("日本語"), 7);
        assert_eq!(estimate_tokens("日本 abc"), 2 + 2 + 4);
        let text = DocumentText {
            title: "題",
            text: "本文",
        };
        assert_eq!(text_tokens(&text), 5 + 6);
    }

    fn headers(pairs: &[(&'static str, &str)]) -> HeaderMap {
        let mut headers = HeaderMap::new();
        for (name, value) in pairs {
            headers.insert(*name, HeaderValue::from_str(value).unwrap());
        }
        headers
    }

    #[test]
    fn rate_limits_and_errors_are_classified() {
        let now = Utc::now();
        let none = HeaderMap::new();
        let limited = |retry_after| EmbedError::RateLimit { retry_after };
        assert_eq!(
            classify(429, &headers(&[("retry-after", "30")]), b"", now),
            limited(Some(Duration::from_secs(30)))
        );
        assert_eq!(
            classify(429, &headers(&[("retry-after-ms", "1500")]), b"", now),
            limited(Some(Duration::from_millis(1500)))
        );
        let date = (now + chrono::Duration::seconds(120)).to_rfc2822();
        match classify(429, &headers(&[("retry-after", &date)]), b"", now) {
            EmbedError::RateLimit {
                retry_after: Some(wait),
            } => assert!((118..=120).contains(&wait.as_secs()), "{wait:?}"),
            other => panic!("{other:?}"),
        }
        assert_eq!(classify(429, &none, b"not json", now), limited(None));
        // Gemini: RESOURCE_EXHAUSTED with RetryInfo; a daily quota waits at least an hour.
        let gemini = |quota: &str| {
            format!(
                r#"{{"error":{{"code":429,"status":"RESOURCE_EXHAUSTED","message":"quota","details":[
                {{"@type":"type.googleapis.com/google.rpc.QuotaFailure","violations":[{{"quotaId":"{quota}"}}]}},
                {{"@type":"type.googleapis.com/google.rpc.RetryInfo","retryDelay":"20.5s"}}]}}}}"#
            )
        };
        assert_eq!(
            classify(
                429,
                &none,
                gemini("EmbedContentRequestsPerMinutePerProjectPerModel-FreeTier").as_bytes(),
                now
            ),
            limited(Some(Duration::from_secs_f64(20.5)))
        );
        assert_eq!(
            classify(
                429,
                &none,
                gemini("EmbedContentRequestsPerDayPerProjectPerModel-FreeTier").as_bytes(),
                now
            ),
            limited(Some(MAX_BACKOFF))
        );
        assert_eq!(
            classify(
                429,
                &none,
                br#"{"error":{"code":"insufficient_quota","type":"insufficient_quota"}}"#,
                now
            ),
            limited(Some(MAX_BACKOFF))
        );
        assert_eq!(classify(402, &none, b"", now), limited(Some(MAX_BACKOFF)));
        assert_eq!(classify(401, &none, b"", now), EmbedError::Auth);
        assert_eq!(classify(403, &none, b"", now), EmbedError::Auth);
        let invalid_key = br#"{"error":{"code":400,"status":"INVALID_ARGUMENT","details":[{"@type":"type.googleapis.com/google.rpc.ErrorInfo","reason":"API_KEY_INVALID"}]}}"#;
        assert_eq!(classify(400, &none, invalid_key, now), EmbedError::Auth);
        assert_eq!(
            classify(
                400,
                &none,
                br#"{"error":{"status":"FAILED_PRECONDITION"}}"#,
                now
            ),
            EmbedError::Auth
        );
        assert_eq!(
            classify(
                400,
                &none,
                br#"{"error":{"status":"INVALID_ARGUMENT"}}"#,
                now
            ),
            EmbedError::BadInput
        );
        for status in [413, 422] {
            assert_eq!(classify(status, &none, b"", now), EmbedError::BadInput);
        }
        for status in [404, 500, 502, 503] {
            assert_eq!(classify(status, &none, b"", now), EmbedError::Upstream);
        }
    }

    #[test]
    fn pacing_waits_for_the_minute_window() {
        let start = Instant::now();
        let mut pacer = Pacer::new(pacing(10, 1_000, 0));
        assert_eq!(pacer.minute_wait(10, 1_000, start), Duration::ZERO);
        pacer.record(6, 300, start);
        pacer.record(2, 600, start + Duration::from_secs(20));
        let now = start + Duration::from_secs(30);
        assert_eq!(pacer.minute_wait(2, 100, now), Duration::ZERO);
        // Five more texts fit only after the first record leaves the window.
        assert_eq!(pacer.minute_wait(5, 100, now), Duration::from_secs(30));
        // 200 more tokens fit only after both have left it.
        assert_eq!(pacer.minute_wait(1, 800, now), Duration::from_secs(50));
        assert_eq!(
            pacer.minute_wait(10, 1_000, start + Duration::from_secs(80)),
            Duration::ZERO
        );
    }

    #[test]
    fn batches_fit_the_limits() {
        let now = Instant::now();
        let mut pacer = Pacer::new(pacing(50, 2_000, 0));
        assert_eq!(pacer.batch_len(&[600; 10], 32, now), 3);
        assert_eq!(pacer.batch_len(&[10; 100], 32, now), 32);
        assert_eq!(pacer.batch_len(&[10; 5], 32, now), 5);
        // A single text always goes, even one larger than the token budget.
        assert_eq!(pacer.batch_len(&[5_000, 1], 32, now), 1);
        let mut pacer = Pacer::new(pacing(4, 100_000, 0));
        assert_eq!(pacer.batch_len(&[1; 40], 32, now), 4);
    }

    #[test]
    fn the_daily_amount_is_kept_and_released_after_a_day() {
        let start = Instant::now();
        let mut pacer = Pacer::new(pacing(1_000, 1_000_000, 10));
        assert_eq!(pacer.daily_wait(start), None);
        pacer.record(4, 10, start);
        pacer.record(4, 10, start + Duration::from_secs(3600));
        let now = start + Duration::from_secs(7200);
        assert_eq!(pacer.batch_len(&[1; 20], 32, now), 2);
        pacer.record(2, 10, now);
        assert_eq!(pacer.daily_wait(now), Some(start + DAY));
        assert_eq!(pacer.daily_wait(start + DAY), None);
        // Without a daily limit nothing is kept for the day.
        let mut unlimited = Pacer::new(pacing(1_000, 1_000_000, 0));
        unlimited.record(10_000, 10, start);
        assert_eq!(unlimited.daily_wait(start), None);
    }

    #[test]
    fn rate_limits_pause_with_backoff() {
        let now = Instant::now();
        let mut pacer = Pacer::new(pacing(10, 10_000, 0));
        assert_eq!(pacer.block(now), None);
        assert_eq!(pacer.rate_limited(None, now), Duration::from_secs(60));
        assert_eq!(pacer.rate_limited(None, now), Duration::from_secs(120));
        assert_eq!(pacer.block(now).unwrap().reason, BlockReason::RateLimit);
        assert_eq!(
            pacer.block(now).unwrap().until,
            now + Duration::from_secs(120)
        );
        for _ in 0..10 {
            pacer.rate_limited(None, now);
        }
        assert_eq!(pacer.rate_limited(None, now), MAX_BACKOFF);
        pacer.succeeded();
        // Retry-After is honoured within 1 second and 1 hour; a running longer pause stays.
        let later = now + MAX_BACKOFF;
        assert_eq!(
            pacer.rate_limited(Some(Duration::from_millis(10)), later),
            Duration::from_secs(1)
        );
        assert_eq!(
            pacer.rate_limited(Some(Duration::from_secs(86_400)), later),
            MAX_BACKOFF
        );
        assert_eq!(pacer.block(later + MAX_BACKOFF), None);
        pacer.auth_failed(later + MAX_BACKOFF);
        assert_eq!(
            pacer.block(later + MAX_BACKOFF).unwrap().reason,
            BlockReason::Auth
        );
    }
}
