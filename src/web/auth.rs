//! Discord OAuth2 login and web sessions. The user's Discord token is used only to read who
//! they are and which guilds they are in, then revoked at once; it is never stored.

use std::time::Duration;

use axum::{
    extract::{FromRequestParts, Query, State},
    http::{HeaderMap, StatusCode, header, request::Parts},
    response::{IntoResponse, Redirect, Response},
};
use chrono::Utc;
use reqwest::{Client, StatusCode as UpstreamStatus};
use serde::{Deserialize, de::DeserializeOwned};
use url::Url;

use super::{
    ApiError, AppState, Web, assets,
    security::{cookie, decode, encode, equal, hash, new_token},
};
use crate::{
    config::WebConfig,
    db::{SESSION_DAYS, SessionGuild, parse_snowflake},
};

const AUTHORIZE_URL: &str = "https://discord.com/oauth2/authorize";
const STATE_COOKIE_SECONDS: u64 = 600;
/// Discord's page size for /users/@me/guilds; users are in at most a few hundred guilds.
const GUILD_PAGE: usize = 200;
const MAX_GUILD_PAGES: usize = 5;
const MAX_RESPONSE_BYTES: usize = 1_000_000;

/// A logged-in user, from the session cookie. Rejects with 401 otherwise.
#[derive(Debug, Clone)]
pub struct Session {
    pub token_hash: [u8; 32],
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
        let token_hash = hash(&token);
        let session = state
            .db
            .session(&token_hash, Utc::now())
            .await
            .map_err(|_| ApiError::Database)?
            .ok_or(ApiError::Unauthenticated)?;
        Ok(Self {
            token_hash,
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
    let token = match start_session(&state, code).await {
        Ok(token) => token,
        Err(error) => {
            tracing::warn!(error_code = %error, "web_login_failed");
            return match error {
                LoginError::OAuth(OAuthError::Rejected) => fail(
                    StatusCode::BAD_REQUEST,
                    "Discord でのログインに失敗しました。もう一度ログインしてください。",
                ),
                LoginError::OAuth(_) => fail(
                    StatusCode::BAD_GATEWAY,
                    "Discord と通信できませんでした。時間をおいて再試行してください。",
                ),
                LoginError::Database => fail(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "データベースに接続できませんでした。時間をおいて再試行してください。",
                ),
            };
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
    #[error(transparent)]
    OAuth(#[from] OAuthError),
    #[error("database")]
    Database,
}

/// Exchanges the code, reads the user and their guilds, revokes the user token and stores a
/// session limited to allowlisted guilds. Returns the new session token.
async fn start_session(state: &Web, code: &str) -> Result<super::security::Token, LoginError> {
    let token = state
        .oauth
        .exchange(code, &state.config.redirect_uri())
        .await?;
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
    #[error("discord_rate_limit")]
    RateLimit,
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

/// Discord's OAuth2 endpoints, called with the application's client credentials.
pub struct OAuthClient {
    client: Client,
    api: String,
    client_id: String,
    client_secret: String,
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
        })
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
        let token: TokenResponse = read_json(response).await?;
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
        check(response.status())
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
        read_json(response).await
    }
}

fn check(status: UpstreamStatus) -> Result<(), OAuthError> {
    match status {
        status if status.is_success() => Ok(()),
        UpstreamStatus::TOO_MANY_REQUESTS => Err(OAuthError::RateLimit),
        status if status.is_client_error() => Err(OAuthError::Rejected),
        _ => Err(OAuthError::Upstream),
    }
}

async fn read_json<T: DeserializeOwned>(mut response: reqwest::Response) -> Result<T, OAuthError> {
    if let Err(error) = check(response.status()) {
        tracing::warn!(
            http_status = response.status().as_u16(),
            "discord_oauth_request_failed"
        );
        return Err(error);
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|_| OAuthError::Network)? {
        if bytes.len() + chunk.len() > MAX_RESPONSE_BYTES {
            return Err(OAuthError::InvalidResponse);
        }
        bytes.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&bytes).map_err(|_| OAuthError::InvalidResponse)
}
