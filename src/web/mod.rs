//! The web UI: Discord login, the server list, role settings, the knowledge base, the chat and
//! the user's own data (privacy).
//! It runs in the bot's process and shares its database pool, Discord REST client (and rate
//! limits), generation limits and knowledge base.

pub mod admin;
pub mod assets;
pub mod auth;
pub mod authz;
pub mod chat;
pub mod chat_store;
pub mod kb;
pub mod privacy;
pub mod security;

use std::{
    collections::HashSet,
    io::{Read, Write},
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, TcpStream},
    sync::{
        Arc, RwLock,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, FromRequest, Request, State, rejection::JsonRejection},
    http::{StatusCode, Uri, header},
    middleware,
    response::{IntoResponse, Response},
    routing::{delete, get, post, put},
};
use serde::de::DeserializeOwned;
use serenity::http::Http;
use tokio::{net::TcpListener, sync::watch};

use crate::{agent::Agent, config::WebConfig, db::Database, knowledge::Knowledge, limits::Limits};

/// Guilds the bot is currently in, kept from gateway events (serenity's cache is disabled).
pub type BotGuilds = Arc<RwLock<HashSet<u64>>>;

/// Largest request body; the settings API only takes small JSON documents.
const MAX_BODY_BYTES: usize = 16 * 1024;

/// Shared by the API and the login pages.
const DATABASE_UNAVAILABLE: &str =
    "データベースに接続できませんでした。時間をおいて再試行してください。";
const INTERNAL_ERROR: &str = "内部エラーが発生しました。時間をおいて再試行してください。";

/// What the web server shares with the Discord side of the process.
pub struct Shared {
    pub db: Database,
    pub agent: Agent,
    pub limits: Arc<Limits>,
    pub http: Arc<Http>,
    pub bot_guilds: BotGuilds,
    /// True while every gateway shard is connected (for /healthz); see
    /// `discord::watch_gateway`.
    pub discord_ready: Arc<AtomicBool>,
    pub discord_cache: Arc<authz::DiscordCache>,
    /// `None` when the knowledge base is disabled.
    pub knowledge: Option<Arc<Knowledge>>,
    /// The web chat's answers being generated; /privacy delete stops the user's.
    pub chat: Arc<chat::Chat>,
}

pub struct Web {
    pub config: WebConfig,
    pub db: Database,
    /// For the web chat.
    pub agent: Agent,
    /// Shared with /talk; the chat uses `Key::User`.
    pub limits: Arc<Limits>,
    pub bot_guilds: BotGuilds,
    pub discord_ready: Arc<AtomicBool>,
    pub authz: authz::Authz,
    pub oauth: auth::OAuthClient,
    pub cookies: security::Cookies,
    pub pages: assets::Pages,
    /// `None` when the knowledge base is disabled; its pages then answer 404.
    pub knowledge: Option<Arc<Knowledge>>,
    /// Answers being generated and recent questions (in memory).
    pub chat: Arc<chat::Chat>,
}

pub type AppState = Arc<Web>;

impl Web {
    pub fn new(config: WebConfig, shared: Shared) -> Result<AppState, reqwest::Error> {
        Ok(Arc::new(Self {
            knowledge: shared.knowledge,
            oauth: auth::OAuthClient::new(&config)?,
            cookies: security::Cookies::new(config.secure()),
            authz: authz::Authz::new(shared.http, shared.discord_cache),
            pages: assets::Pages::render(),
            config,
            db: shared.db,
            agent: shared.agent,
            limits: shared.limits,
            bot_guilds: shared.bot_guilds,
            discord_ready: shared.discord_ready,
            chat: shared.chat,
        }))
    }

    pub fn bot_in_guild(&self, guild: u64) -> bool {
        self.bot_guilds
            .read()
            .expect("bot guild lock poisoned")
            .contains(&guild)
    }
}

pub fn router(state: AppState) -> Router {
    // The upload route takes a whole file as its body, chat messages up to 64 KiB, every other
    // route small JSON documents.
    // Caddy's request_body limit uses the same KB_MAX_UPLOAD_BYTES (deploy/caddy/Caddyfile).
    let upload_limit = state
        .knowledge
        .as_ref()
        .map_or(MAX_BODY_BYTES, |knowledge| {
            knowledge.config.max_upload_bytes
        });
    Router::new()
        .route("/", get(assets::index))
        .route("/static/{*path}", get(assets::file))
        .route("/privacy", get(assets::privacy))
        .route("/terms", get(assets::terms))
        .route("/healthz", get(healthz))
        .route("/auth/login", get(auth::login))
        .route("/auth/callback", get(auth::callback))
        .route("/auth/logout", post(auth::logout))
        .route("/api/me", get(admin::me))
        .route("/api/guilds/{guild}/roles", get(admin::roles))
        .route("/api/guilds/{guild}/config/roles", put(admin::put_roles))
        .route(
            "/api/guilds/{guild}/kb/documents",
            get(kb::list)
                .post(kb::upload)
                .layer(DefaultBodyLimit::max(upload_limit)),
        )
        .route("/api/guilds/{guild}/kb/documents/{id}", delete(kb::remove))
        .route(
            "/api/guilds/{guild}/kb/documents/{id}/preview",
            get(kb::preview),
        )
        .route(
            "/api/guilds/{guild}/kb/documents/{id}/retry",
            post(kb::retry),
        )
        .route("/api/conversations", get(chat::list).post(chat::start))
        .route(
            "/api/conversations/{id}",
            get(chat::show).patch(chat::rename).delete(chat::remove),
        )
        .route(
            "/api/conversations/{id}/messages",
            post(chat::send).layer(DefaultBodyLimit::max(chat::MAX_MESSAGE_BODY_BYTES)),
        )
        .route("/api/chat/stop", post(chat::stop))
        .route("/api/privacy", get(privacy::show))
        .route("/api/privacy/delete", post(privacy::delete))
        .fallback(not_found)
        .layer(DefaultBodyLimit::max(MAX_BODY_BYTES))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            security::origin_guard,
        ))
        // Outermost, so rejections by the layers above get the headers too.
        .layer(middleware::from_fn(security::headers))
        .with_state(state)
}

/// Serves until `stop` becomes true, then lets open requests finish (the caller bounds the wait).
/// Chat answers being streamed end then, saving what they have, so their responses finish too.
pub async fn serve(
    listener: TcpListener,
    state: AppState,
    mut stop: watch::Receiver<bool>,
) -> std::io::Result<()> {
    let chat = state.clone();
    axum::serve(listener, router(state))
        .with_graceful_shutdown(async move {
            let _ = stop.wait_for(|stop| *stop).await;
            chat.chat.shut_down();
        })
        .await
}

/// 200 only when the database answers within 2 seconds and the gateway is connected. The body
/// never says which part failed.
async fn healthz(State(state): State<AppState>) -> Response {
    let database = matches!(
        tokio::time::timeout(Duration::from_secs(2), state.db.ping()).await,
        Ok(Ok(()))
    );
    let discord = state.discord_ready.load(Ordering::Relaxed);
    let (status, body) = if database && discord {
        (StatusCode::OK, "ok")
    } else {
        (StatusCode::SERVICE_UNAVAILABLE, "unavailable")
    };
    (status, [(header::CACHE_CONTROL, "no-store")], body).into_response()
}

async fn not_found(uri: Uri) -> Response {
    if uri.path().starts_with("/api/") {
        ApiError::NotFound.into_response()
    } else {
        (StatusCode::NOT_FOUND, "見つかりません。").into_response()
    }
}

/// API failures: a kind code for the UI and logs, plus a message for the user.
#[derive(Debug, thiserror::Error)]
pub enum ApiError {
    #[error("unauthenticated")]
    Unauthenticated,
    #[error("forbidden")]
    Forbidden,
    #[error("cross_origin")]
    CrossOrigin,
    #[error("not_found")]
    NotFound,
    #[error("unsupported_media_type")]
    UnsupportedMediaType,
    #[error("payload_too_large")]
    PayloadTooLarge,
    #[error("invalid_request")]
    Invalid(String),
    #[error("discord_unavailable")]
    DiscordUnavailable,
    #[error("database")]
    Database,
    #[error("internal")]
    Internal,
    /// Refusals with their own status, kind code and message (the knowledge base API).
    #[error("{code}")]
    Refused {
        status: StatusCode,
        code: &'static str,
        message: String,
    },
}

impl ApiError {
    pub fn status(&self) -> StatusCode {
        match self {
            Self::Unauthenticated => StatusCode::UNAUTHORIZED,
            Self::Forbidden | Self::CrossOrigin => StatusCode::FORBIDDEN,
            Self::NotFound => StatusCode::NOT_FOUND,
            Self::UnsupportedMediaType => StatusCode::UNSUPPORTED_MEDIA_TYPE,
            Self::PayloadTooLarge => StatusCode::PAYLOAD_TOO_LARGE,
            Self::Invalid(_) => StatusCode::BAD_REQUEST,
            Self::DiscordUnavailable => StatusCode::SERVICE_UNAVAILABLE,
            Self::Database | Self::Internal => StatusCode::INTERNAL_SERVER_ERROR,
            Self::Refused { status, .. } => *status,
        }
    }

    pub fn user_message(&self) -> &str {
        match self {
            Self::Unauthenticated => "ログインしてください。",
            Self::Forbidden => "この操作を行う権限がありません。",
            Self::CrossOrigin => "不正なリクエストです。ページを再読み込みしてください。",
            Self::NotFound => "見つからないか、利用できません。",
            Self::UnsupportedMediaType => "リクエストの形式（Content-Type）が正しくありません。",
            Self::PayloadTooLarge => "リクエストが大きすぎます。",
            Self::Invalid(message) => message,
            Self::DiscordUnavailable => {
                "Discord から情報を取得できませんでした。時間をおいて再試行してください。"
            }
            Self::Database => DATABASE_UNAVAILABLE,
            Self::Internal => INTERNAL_ERROR,
            Self::Refused { message, .. } => message,
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (
            self.status(),
            Json(serde_json::json!({"error": self.to_string(), "message": self.user_message()})),
        )
            .into_response()
    }
}

/// `Json` with this API's error format. Requires `Content-Type: application/json`, which also
/// makes cross-site form posts impossible without a CORS preflight.
pub struct JsonBody<T>(pub T);

impl<S: Send + Sync, T: DeserializeOwned> FromRequest<S> for JsonBody<T> {
    type Rejection = ApiError;

    async fn from_request(request: Request, state: &S) -> Result<Self, Self::Rejection> {
        match Json::<T>::from_request(request, state).await {
            Ok(Json(value)) => Ok(Self(value)),
            Err(JsonRejection::MissingJsonContentType(_)) => Err(ApiError::UnsupportedMediaType),
            Err(JsonRejection::BytesRejection(rejection))
                if rejection.status() == StatusCode::PAYLOAD_TOO_LARGE =>
            {
                Err(ApiError::PayloadTooLarge)
            }
            Err(_) => Err(ApiError::Invalid(
                "リクエストの内容が正しくありません。".into(),
            )),
        }
    }
}

/// `bot healthcheck` for Docker: GET /healthz on the local listener with std networking only (no
/// runtime, no logging). Returns the exit code; 0 when the web UI is disabled, since then there
/// is no listener to check.
pub fn healthcheck() -> i32 {
    let config = match WebConfig::from_env() {
        Ok(Some(config)) => config,
        Ok(None) => return 0,
        Err(error) => {
            eprintln!("{error:#}");
            return 1;
        }
    };
    let ip = match config.bind.ip() {
        IpAddr::V4(ip) if ip.is_unspecified() => IpAddr::V4(Ipv4Addr::LOCALHOST),
        IpAddr::V6(ip) if ip.is_unspecified() => IpAddr::V6(Ipv6Addr::LOCALHOST),
        ip => ip,
    };
    match probe(SocketAddr::new(ip, config.bind.port())) {
        Ok(true) => 0,
        Ok(false) => {
            eprintln!("unhealthy");
            1
        }
        Err(_) => {
            eprintln!("web listener unreachable");
            1
        }
    }
}

/// True when `GET /healthz` answers 200.
pub fn probe(address: SocketAddr) -> std::io::Result<bool> {
    let timeout = Duration::from_secs(3);
    let mut stream = TcpStream::connect_timeout(&address, timeout)?;
    stream.set_read_timeout(Some(timeout))?;
    stream.set_write_timeout(Some(timeout))?;
    stream.write_all(b"GET /healthz HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")?;
    let mut status = [0_u8; 12];
    stream.read_exact(&mut status)?;
    Ok(&status == b"HTTP/1.1 200")
}
