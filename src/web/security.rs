//! Response headers, the cross-site request guard, cookies and random tokens.

use std::sync::LazyLock;

use axum::{
    extract::{Request, State},
    http::{HeaderMap, HeaderValue, Method, header},
    middleware::Next,
    response::{IntoResponse, Response},
};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use ring::{
    digest, hmac,
    rand::{SecureRandom, SystemRandom},
};

use super::{ApiError, AppState};

/// No inline scripts or styles anywhere, and nothing loaded from other origins (images included,
/// so injected markup cannot send data out through image URLs).
const CSP: &str = "default-src 'none'; script-src 'self'; style-src 'self'; img-src 'self'; connect-src 'self'; form-action 'self'; frame-ancestors 'none'; base-uri 'none'";

pub async fn headers(request: Request, next: Next) -> Response {
    let private = {
        let path = request.uri().path();
        path.starts_with("/api/") || path.starts_with("/auth/")
    };
    let mut response = next.run(request).await;
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(CSP),
    );
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(
        "cross-origin-opener-policy",
        HeaderValue::from_static("same-origin"),
    );
    // Not no-referrer: browsers then send `Origin: null` on POST, which the guard below rejects.
    headers.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("same-origin"),
    );
    if private && !headers.contains_key(header::CACHE_CONTROL) {
        headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    }
    response
}

/// CSRF protection: every request that can change something must come from our own pages.
pub async fn origin_guard(State(state): State<AppState>, request: Request, next: Next) -> Response {
    if matches!(*request.method(), Method::GET | Method::HEAD)
        || same_origin(request.headers(), &state.config.public_origin)
    {
        next.run(request).await
    } else {
        tracing::info!(method = %request.method(), "cross_origin_request_rejected");
        ApiError::CrossOrigin.into_response()
    }
}

/// Exactly one `Origin` header, equal to the configured origin.
pub fn same_origin(headers: &HeaderMap, origin: &str) -> bool {
    let mut values = headers.get_all(header::ORIGIN).iter();
    matches!(
        (values.next(), values.next()),
        (Some(value), None) if value.as_bytes() == origin.as_bytes()
    )
}

/// Cookie names and attributes. `__Host-` (which requires `Secure`, `Path=/` and no `Domain`)
/// pins a cookie to this exact origin; browsers refuse both over plain http://localhost.
#[derive(Debug, Clone, Copy)]
pub struct Cookies {
    secure: bool,
}

impl Cookies {
    pub fn new(secure: bool) -> Self {
        Self { secure }
    }

    pub fn session(&self) -> &'static str {
        if self.secure {
            "__Host-session"
        } else {
            "session"
        }
    }

    pub fn oauth_state(&self) -> &'static str {
        if self.secure { "__Host-oauth" } else { "oauth" }
    }

    /// `value` must be cookie-safe (our tokens are base64url).
    pub fn set(&self, name: &str, value: &str, max_age_seconds: u64) -> HeaderValue {
        let secure = if self.secure { "; Secure" } else { "" };
        HeaderValue::try_from(format!(
            "{name}={value}; Max-Age={max_age_seconds}; Path=/; HttpOnly; SameSite=Lax{secure}"
        ))
        .expect("cookie names and values are ASCII")
    }

    pub fn clear(&self, name: &str) -> HeaderValue {
        self.set(name, "", 0)
    }
}

/// The value of the first cookie called `name`.
pub fn cookie<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(';'))
        .filter_map(|pair| pair.trim().split_once('='))
        .find(|(key, _)| *key == name)
        .map(|(_, value)| value)
}

pub type Token = [u8; 32];

pub fn new_token() -> Token {
    let mut token = [0_u8; 32];
    SystemRandom::new()
        .fill(&mut token)
        .expect("system randomness is available");
    token
}

pub fn encode(token: &Token) -> String {
    URL_SAFE_NO_PAD.encode(token)
}

/// Only the exact encoding `encode` produces.
pub fn decode(value: &str) -> Option<Token> {
    if value.len() != 43 {
        return None;
    }
    URL_SAFE_NO_PAD.decode(value).ok()?.try_into().ok()
}

/// What the database stores instead of a session token.
pub fn hash(token: &Token) -> [u8; 32] {
    digest::digest(&digest::SHA256, token)
        .as_ref()
        .try_into()
        .expect("SHA-256 digests are 32 bytes")
}

/// Constant-time equality. ring deprecated its `constant_time` module; HMAC verification under
/// a per-process random key is its supported constant-time comparison.
pub fn equal(a: &[u8], b: &[u8]) -> bool {
    static KEY: LazyLock<hmac::Key> = LazyLock::new(|| {
        hmac::Key::generate(hmac::HMAC_SHA256, &SystemRandom::new())
            .expect("system randomness is available")
    });
    let tag = hmac::sign(&KEY, a);
    hmac::verify(&KEY, b, tag.as_ref()).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cookies_follow_the_scheme() {
        let https = Cookies::new(true);
        assert_eq!(https.session(), "__Host-session");
        assert_eq!(https.oauth_state(), "__Host-oauth");
        assert_eq!(
            https.set("__Host-session", "abc", 604_800),
            "__Host-session=abc; Max-Age=604800; Path=/; HttpOnly; SameSite=Lax; Secure"
        );
        assert_eq!(
            https.clear("__Host-oauth"),
            "__Host-oauth=; Max-Age=0; Path=/; HttpOnly; SameSite=Lax; Secure"
        );
        let local = Cookies::new(false);
        assert_eq!(local.session(), "session");
        assert_eq!(
            local.set("session", "abc", 60),
            "session=abc; Max-Age=60; Path=/; HttpOnly; SameSite=Lax"
        );
    }

    #[test]
    fn cookies_are_parsed_from_every_header() {
        let mut headers = HeaderMap::new();
        headers.append(header::COOKIE, HeaderValue::from_static("a=1; session=x"));
        headers.append(
            header::COOKIE,
            HeaderValue::from_static("__Host-session=y;b=2"),
        );
        assert_eq!(cookie(&headers, "session"), Some("x"));
        assert_eq!(cookie(&headers, "__Host-session"), Some("y"));
        assert_eq!(cookie(&headers, "b"), Some("2"));
        assert_eq!(cookie(&headers, "sess"), None);
    }

    #[test]
    fn origin_must_match_exactly_once() {
        let origin = "https://bot.example";
        let with = |values: &[&'static str]| {
            let mut headers = HeaderMap::new();
            for value in values {
                headers.append(header::ORIGIN, HeaderValue::from_static(value));
            }
            same_origin(&headers, origin)
        };
        assert!(with(&["https://bot.example"]));
        assert!(!with(&[]));
        assert!(!with(&["null"]));
        assert!(!with(&["https://evil.example"]));
        assert!(!with(&["http://bot.example"]));
        assert!(!with(&["https://bot.example:443"]));
        assert!(!with(&["https://bot.example.evil.example"]));
        assert!(!with(&["https://bot.example", "https://bot.example"]));
    }

    #[test]
    fn tokens_are_random_hashed_and_strictly_encoded() {
        let token = new_token();
        assert_ne!(token, new_token());
        let encoded = encode(&token);
        assert_eq!(encoded.len(), 43);
        assert_eq!(decode(&encoded), Some(token));
        assert_eq!(decode(&encoded[..42]), None);
        assert_eq!(decode(&format!("{encoded}=")), None);
        assert_eq!(decode(&"+".repeat(43)), None);
        // SHA-256 of 32 zero bytes.
        assert_eq!(
            hash(&[0; 32])
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect::<String>(),
            "66687aadf862bd776c8fc18b8e9f8e20089714856ee233b3902a591d0d5f2925"
        );
        assert_ne!(hash(&token), token);
        assert!(equal(b"state", b"state"));
        assert!(!equal(b"state", b"statf"));
        assert!(!equal(b"state", b"state2"));
        assert!(!equal(b"", b"x"));
    }
}
