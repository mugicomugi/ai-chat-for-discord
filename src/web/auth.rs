//! Discord OAuth2 login and web sessions. The user's Discord token is used only to read who
//! they are and which guilds they are in, then revoked at once; it is never stored.

use std::{
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use axum::{
    extract::{FromRequestParts, Query, State},
    http::{HeaderMap, HeaderValue, StatusCode, header, request::Parts},
    response::{IntoResponse, Redirect, Response},
};
use chrono::Utc;
use reqwest::{Client, StatusCode as UpstreamStatus};
use serde::{Deserialize, de::DeserializeOwned};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use url::Url;

use super::{
    ApiError, AppState, DATABASE_UNAVAILABLE, INTERNAL_ERROR, Web, assets,
    security::{Token, cookie, decode, encode, equal, hash, new_token},
};
use crate::{
    bounded::{self, BodyError},
    config::WebConfig,
    db::{SESSION_DAYS, SessionGuild},
    ids::parse_snowflake,
};

const AUTHORIZE_URL: &str = "https://discord.com/oauth2/authorize";
const STATE_COOKIE_SECONDS: u64 = 600;
/// Discord's page size for /users/@me/guilds; users are in at most a few hundred guilds.
const GUILD_PAGE: usize = 200;
const MAX_GUILD_PAGES: usize = 5;
const MAX_RESPONSE_BYTES: usize = 1_000_000;
/// Logins that may talk to Discord at the same time, and per minute. The callback needs no
/// account and its state check is a double submit any script can pass, while every login calls
/// Discord from the bot's IP: enough 429s there get the IP banned from the whole Discord API
/// (gateway and /talk included), so a flood is refused here instead.
const LOGIN_CONCURRENCY: usize = 2;
const LOGINS_PER_MINUTE: u32 = 20;
/// The pause after a 429 without a usable Retry-After, and the longest one honoured.
const DEFAULT_BACKOFF: Duration = Duration::from_secs(60);
const MAX_BACKOFF: Duration = Duration::from_secs(600);

/// A logged-in user, from the session cookie. Rejects with 401 otherwise.
#[derive(Debug, Clone)]
pub struct Session {
    pub user_id: u64,
    pub user_name: String,
    /// Allowlisted guilds the user was in at login.
    pub guilds: Vec<SessionGuild>,
}

impl FromRequestParts<AppState> for Session {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, ApiError> {
        let token = cookie(&parts.headers, state.cookies.session())
            .and_then(decode)
            .ok_or(ApiError::Unauthenticated)?;
        let session = state
            .db
            .session(&hash(&token), Utc::now())
            .await
            .map_err(|_| ApiError::Database)?
            .ok_or(ApiError::Unauthenticated)?;
        Ok(Self {
            user_id: session.user_id,
            user_name: session.user_name,
            guilds: session.guilds,
        })
    }
}

/// Starts the login: a random state in a short-lived cookie, then Discord's consent page.
pub async fn login(State(state): State<AppState>) -> Response {
    let token = encode(&new_token());
    let mut url = Url::parse(AUTHORIZE_URL).expect("valid constant URL");
    url.query_pairs_mut()
        .append_pair("response_type", "code")
        .append_pair("client_id", &state.config.client_id)
        .append_pair("scope", "identify guilds")
        .append_pair("redirect_uri", &state.config.redirect_uri())
        .append_pair("state", &token);
    (
        [(
            header::SET_COOKIE,
            state
                .cookies
                .set(state.cookies.oauth_state(), &token, STATE_COOKIE_SECONDS),
        )],
        Redirect::to(url.as_str()),
    )
        .into_response()
}

#[derive(Deserialize)]
pub struct CallbackParams {
    code: Option<String>,
    state: Option<String>,
    error: Option<String>,
}

const LOGIN_EXPIRED: &str =
    "ログインの有効期限が切れたか、リクエストが正しくありません。もう一度ログインしてください。";

pub async fn callback(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(params): Query<CallbackParams>,
) -> Response {
    let clear_state = state.cookies.clear(state.cookies.oauth_state());
    let fail = |status: StatusCode, message: &'static str| {
        (
            status,
            [(header::SET_COOKIE, clear_state.clone())],
            assets::message_page("ログインできませんでした", message),
        )
            .into_response()
    };
    if params.error.is_some() {
        return fail(
            StatusCode::BAD_REQUEST,
            "Discord でのログインがキャンセルされました。",
        );
    }
    let expected = cookie(&headers, state.cookies.oauth_state());
    let (Some(expected), Some(received), Some(code)) = (expected, &params.state, &params.code)
    else {
        return fail(StatusCode::BAD_REQUEST, LOGIN_EXPIRED);
    };
    if expected.is_empty() || !equal(expected.as_bytes(), received.as_bytes()) || code.len() > 512 {
        return fail(StatusCode::BAD_REQUEST, LOGIN_EXPIRED);
    }
    let result = match state.oauth.admit() {
        // Refusals are not logged one by one: the gate reports floods itself.
        Err(wait) => Err(LoginError::Throttled(wait)),
        Ok(permit) => {
            // Detached from the request, so a browser that goes away mid-login does not stop
            // the revocation of the user's Discord token.
            let (state, code) = (state.clone(), code.clone());
            tokio::spawn(async move {
                let _permit = permit;
                start_session(&state, &code).await
            })
            .await
            .unwrap_or(Err(LoginError::Internal))
            .inspect_err(|error| tracing::warn!(error_code = %error, "web_login_failed"))
        }
    };
    let token = match result {
        Ok(token) => token,
        Err(error) => {
            let mut response = fail(error.status(), error.user_message());
            if let Some(wait) = error.retry_after() {
                // Whole seconds, rounded up.
                let seconds = wait.as_secs() + u64::from(wait.subsec_nanos() > 0);
                response
                    .headers_mut()
                    .insert(header::RETRY_AFTER, HeaderValue::from(seconds.max(1)));
            }
            return response;
        }
    };
    // A new token on every login (session fixation); the session it replaces is dropped.
    if let Some(previous) = cookie(&headers, state.cookies.session()).and_then(decode)
        && state.db.delete_session(&hash(&previous)).await.is_err()
    {
        tracing::warn!("previous_session_delete_failed");
    }
    let session_cookie = state.cookies.set(
        state.cookies.session(),
        &encode(&token),
        SESSION_DAYS as u64 * 86_400,
    );
    let mut response = Redirect::to("/").into_response();
    let headers = response.headers_mut();
    headers.append(header::SET_COOKIE, clear_state);
    headers.append(header::SET_COOKIE, session_cookie);
    response
}

#[derive(Debug, thiserror::Error)]
enum LoginError {
    /// Refused locally by the login gate.
    #[error("login_throttled")]
    Throttled(Duration),
    #[error(transparent)]
    OAuth(#[from] OAuthError),
    #[error("database")]
    Database,
    #[error("internal")]
    Internal,
}

impl LoginError {
    fn status(&self) -> StatusCode {
        match self {
            Self::Throttled(_) | Self::OAuth(OAuthError::RateLimit(_)) => {
                StatusCode::SERVICE_UNAVAILABLE
            }
            Self::OAuth(OAuthError::Rejected) => StatusCode::BAD_REQUEST,
            Self::OAuth(_) => StatusCode::BAD_GATEWAY,
            Self::Database | Self::Internal => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }

    fn user_message(&self) -> &'static str {
        match self {
            Self::Throttled(_) | Self::OAuth(OAuthError::RateLimit(_)) => {
                "ログインが混み合っています。しばらくしてから再試行してください。"
            }
            Self::OAuth(OAuthError::Rejected) => {
                "Discord でのログインに失敗しました。もう一度ログインしてください。"
            }
            Self::OAuth(_) => "Discord と通信できませんでした。時間をおいて再試行してください。",
            Self::Database => DATABASE_UNAVAILABLE,
            Self::Internal => INTERNAL_ERROR,
        }
    }

    fn retry_after(&self) -> Option<Duration> {
        match self {
            Self::Throttled(wait) | Self::OAuth(OAuthError::RateLimit(wait)) => Some(*wait),
            _ => None,
        }
    }
}

/// Exchanges the code, reads the user and their guilds, revokes the user token and stores a
/// session limited to allowlisted guilds. Returns the new session token.
async fn start_session(state: &Web, code: &str) -> Result<Token, LoginError> {
    let token = state
        .oauth
        .exchange(code, &state.config.redirect_uri())
        .await?;
    // Revoked whatever happens next: the fetches and the database are only touched after it.
    let fetched = async {
        let user = state.oauth.user(&token).await?;
        let guilds = state.oauth.guilds(&token).await?;
        Ok::<_, OAuthError>((user, guilds))
    }
    .await;
    if let Err(error) = state.oauth.revoke(&token).await {
        tracing::warn!(error_code = %error, "oauth_token_revoke_failed");
    }
    drop(token);
    let (user, guilds) = fetched?;
    let allowed = state
        .db
        .allowed_guild_ids()
        .await
        .map_err(|_| LoginError::Database)?;
    let guilds: Vec<SessionGuild> = guilds
        .into_iter()
        .filter(|guild| allowed.contains(&guild.id))
        .collect();
    let session = new_token();
    state
        .db
        .create_session(&hash(&session), user.id, &user.name, &guilds, Utc::now())
        .await
        .map_err(|_| LoginError::Database)?;
    tracing::info!(guild_count = guilds.len(), "web_login");
    Ok(session)
}

/// Ends the session if there is one; always clears the cookie.
pub async fn logout(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Some(token) = cookie(&headers, state.cookies.session()).and_then(decode)
        && state.db.delete_session(&hash(&token)).await.is_err()
    {
        return ApiError::Database.into_response();
    }
    (
        StatusCode::NO_CONTENT,
        [(
            header::SET_COOKIE,
            state.cookies.clear(state.cookies.session()),
        )],
    )
        .into_response()
}

#[derive(Debug, thiserror::Error)]
pub enum OAuthError {
    /// Discord refused the code or the client credentials.
    #[error("oauth_rejected")]
    Rejected,
    /// Discord answered 429; carries how long to wait.
    #[error("discord_rate_limit")]
    RateLimit(Duration),
    #[error("discord_upstream")]
    Upstream,
    #[error("discord_network")]
    Network,
    #[error("discord_invalid_response")]
    InvalidResponse,
}

/// A user's OAuth2 access token. Deliberately not `Debug`, so it cannot end up in logs.
pub struct UserToken(String);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscordUser {
    pub id: u64,
    /// Display name, or the username when none is set.
    pub name: String,
}

/// Discord's OAuth2 endpoints, called with the application's client credentials. Logins go
/// through `admit` first, and any 429 pauses new ones.
pub struct OAuthClient {
    client: Client,
    api: String,
    client_id: String,
    client_secret: String,
    gate: LoginGate,
}

/// Process-wide limits on logins (single replica, so memory is enough).
struct LoginGate {
    permits: Arc<Semaphore>,
    state: Mutex<GateState>,
}

struct GateState {
    window_start: Instant,
    admitted: u32,
    blocked_until: Option<Instant>,
    refused: u64,
    reported_at: Option<Instant>,
}

impl LoginGate {
    fn new() -> Self {
        Self {
            permits: Arc::new(Semaphore::new(LOGIN_CONCURRENCY)),
            state: Mutex::new(GateState {
                window_start: Instant::now(),
                admitted: 0,
                blocked_until: None,
                refused: 0,
                reported_at: None,
            }),
        }
    }

    fn admit(&self) -> Result<OwnedSemaphorePermit, Duration> {
        let now = Instant::now();
        let mut state = self.state.lock().expect("login gate poisoned");
        let minute = Duration::from_secs(60);
        if now.duration_since(state.window_start) >= minute {
            state.window_start = now;
            state.admitted = 0;
        }
        let wait = if let Some(until) = state.blocked_until.filter(|until| *until > now) {
            until - now
        } else if state.admitted >= LOGINS_PER_MINUTE {
            state.window_start + minute - now
        } else {
            match self.permits.clone().try_acquire_owned() {
                Ok(permit) => {
                    state.admitted += 1;
                    return Ok(permit);
                }
                // A login takes about a second.
                Err(_) => Duration::from_secs(2),
            }
        };
        state.refused += 1;
        if state
            .reported_at
            .is_none_or(|at| now.duration_since(at) >= minute)
        {
            tracing::warn!(refused = state.refused, "web_login_throttled");
            state.refused = 0;
            state.reported_at = Some(now);
        }
        Err(wait)
    }

    fn back_off(&self, wait: Duration) {
        let until = Instant::now() + wait;
        let mut state = self.state.lock().expect("login gate poisoned");
        state.blocked_until = Some(
            state
                .blocked_until
                .map_or(until, |current| current.max(until)),
        );
    }
}

impl OAuthClient {
    pub fn new(config: &WebConfig) -> Result<Self, reqwest::Error> {
        Ok(Self {
            client: Client::builder()
                .connect_timeout(Duration::from_secs(5))
                .timeout(Duration::from_secs(10))
                .redirect(reqwest::redirect::Policy::none())
                .build()?,
            api: config.discord_api.trim_end_matches('/').to_owned(),
            client_id: config.client_id.clone(),
            client_secret: config.client_secret.clone(),
            gate: LoginGate::new(),
        })
    }

    /// A permit for one login's Discord calls (held until they finish), or how long to wait.
    pub fn admit(&self) -> Result<OwnedSemaphorePermit, Duration> {
        self.gate.admit()
    }

    pub async fn exchange(&self, code: &str, redirect_uri: &str) -> Result<UserToken, OAuthError> {
        #[derive(Deserialize)]
        struct TokenResponse {
            access_token: String,
        }
        let response = self
            .client
            .post(format!("{}/oauth2/token", self.api))
            .basic_auth(&self.client_id, Some(&self.client_secret))
            .form(&[
                ("grant_type", "authorization_code"),
                ("code", code),
                ("redirect_uri", redirect_uri),
            ])
            .send()
            .await
            .map_err(|_| OAuthError::Network)?;
        let token: TokenResponse = self.read_json(response).await?;
        Ok(UserToken(token.access_token))
    }

    pub async fn user(&self, token: &UserToken) -> Result<DiscordUser, OAuthError> {
        #[derive(Deserialize)]
        struct User {
            id: String,
            username: String,
            global_name: Option<String>,
        }
        let user: User = self.get(token, "/users/@me").await?;
        let name = user
            .global_name
            .filter(|name| !name.trim().is_empty())
            .unwrap_or(user.username);
        Ok(DiscordUser {
            id: parse_snowflake(&user.id).ok_or(OAuthError::InvalidResponse)?,
            name: name.chars().take(128).collect(),
        })
    }

    /// Every guild the user is in, in Discord's order (by ID).
    pub async fn guilds(&self, token: &UserToken) -> Result<Vec<SessionGuild>, OAuthError> {
        #[derive(Deserialize)]
        struct Guild {
            id: String,
            name: String,
        }
        let mut guilds = Vec::new();
        let mut after = None;
        for _ in 0..MAX_GUILD_PAGES {
            let path = match after {
                Some(after) => format!("/users/@me/guilds?limit={GUILD_PAGE}&after={after}"),
                None => format!("/users/@me/guilds?limit={GUILD_PAGE}"),
            };
            let page: Vec<Guild> = self.get(token, &path).await?;
            let full = page.len() == GUILD_PAGE;
            for guild in page {
                let id = parse_snowflake(&guild.id).ok_or(OAuthError::InvalidResponse)?;
                after = Some(id);
                guilds.push(SessionGuild {
                    id,
                    name: guild.name.chars().take(100).collect(),
                });
            }
            if !full {
                break;
            }
        }
        Ok(guilds)
    }

    /// Revokes the whole authorization behind the token (Discord invalidates its refresh token
    /// with it).
    pub async fn revoke(&self, token: &UserToken) -> Result<(), OAuthError> {
        let response = self
            .client
            .post(format!("{}/oauth2/token/revoke", self.api))
            .basic_auth(&self.client_id, Some(&self.client_secret))
            .form(&[
                ("token", token.0.as_str()),
                ("token_type_hint", "access_token"),
            ])
            .send()
            .await
            .map_err(|_| OAuthError::Network)?;
        self.check(&response)
    }

    async fn get<T: DeserializeOwned>(
        &self,
        token: &UserToken,
        path: &str,
    ) -> Result<T, OAuthError> {
        let response = self
            .client
            .get(format!("{}{path}", self.api))
            .bearer_auth(&token.0)
            .send()
            .await
            .map_err(|_| OAuthError::Network)?;
        self.read_json(response).await
    }

    /// A 429 also pauses new logins for as long as Discord asks.
    fn check(&self, response: &reqwest::Response) -> Result<(), OAuthError> {
        let status = response.status();
        if status.is_success() {
            return Ok(());
        }
        tracing::warn!(
            http_status = status.as_u16(),
            "discord_oauth_request_failed"
        );
        Err(match status {
            UpstreamStatus::TOO_MANY_REQUESTS => {
                let wait = retry_after(response.headers());
                self.gate.back_off(wait);
                OAuthError::RateLimit(wait)
            }
            status if status.is_client_error() => OAuthError::Rejected,
            _ => OAuthError::Upstream,
        })
    }

    async fn read_json<T: DeserializeOwned>(
        &self,
        response: reqwest::Response,
    ) -> Result<T, OAuthError> {
        self.check(&response)?;
        bounded::json(response, MAX_RESPONSE_BYTES)
            .await
            .map_err(|error| match error {
                BodyError::Transport(_) => OAuthError::Network,
                BodyError::Invalid => OAuthError::InvalidResponse,
            })
    }
}

/// Discord's wait in seconds (`Retry-After`, else `X-RateLimit-Reset-After`), within bounds.
fn retry_after(headers: &reqwest::header::HeaderMap) -> Duration {
    [
        reqwest::header::RETRY_AFTER.as_str(),
        "x-ratelimit-reset-after",
    ]
    .into_iter()
    .filter_map(|name| headers.get(name)?.to_str().ok()?.trim().parse::<f64>().ok())
    .find(|seconds| seconds.is_finite() && *seconds >= 0.0)
    .map_or(DEFAULT_BACKOFF, |seconds| {
        Duration::from_secs_f64(seconds.min(MAX_BACKOFF.as_secs_f64()))
    })
    .clamp(Duration::from_secs(1), MAX_BACKOFF)
}
