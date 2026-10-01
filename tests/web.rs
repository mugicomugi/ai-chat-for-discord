//! Web UI tests. Without a database: security headers, the CSRF guard, the login redirect and
//! state check, the login limits, the revocation of the user token when a login fails, the
//! session requirement (the privacy API included), the Discord OAuth2 client and the cached
//! Discord REST lookups (wiremock). The `#[ignore]` tests need compose.test.yaml and TEST_DATABASE_URL.

use std::{
    io::{Read, Write},
    net::TcpListener,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use axum::{
    Router,
    body::{Body, Bytes},
    extract::DefaultBodyLimit,
    http::{HeaderMap, Request, StatusCode, header},
    routing::put,
};
use chrono::Utc;
use discord_discussion_bot::{
    access::{Access, GuildAccess, RoleKind},
    agent::Agent,
    config::{KbConfig, ProviderConfig, ProviderKind, WebConfig},
    db::{Database, MAX_SESSIONS_PER_USER, NewRun, SessionGuild},
    knowledge::{Knowledge, UPLOAD_SLOTS},
    limits::Limits,
    privacy,
    web::{
        self, AppState, JsonBody, Shared, Web,
        auth::{OAuthClient, OAuthError},
        authz::{self, Authz, DiscordCache, Unavailable},
        security,
    },
};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use serenity::{
    all::{Member, PartialGuild},
    http::HttpBuilder,
};
use sqlx::{Row, mysql::MySqlPoolOptions};
use tower::ServiceExt;
use url::Url;
use wiremock::{
    Mock, MockServer, Request as MockRequest, ResponseTemplate,
    matchers::{
        body_string_contains, header as mock_header, method, path, path_regex, query_param,
    },
};

mod common;
use common::database;

const ORIGIN: &str = "https://bot.example";
/// The Content-Security-Policy of docs/roadmap.md, written out rather than taken from the crate,
/// so that a loosened policy fails here.
const POLICY: &str = "default-src 'none'; script-src 'self'; style-src 'self'; img-src 'self'; connect-src 'self'; form-action 'self'; frame-ancestors 'none'; base-uri 'none'";
const ADMINISTRATOR: u64 = 1 << 3;
const MANAGE_GUILD: u64 = 1 << 5;

fn config(discord: &str) -> WebConfig {
    WebConfig {
        client_id: "4242".into(),
        client_secret: "test-client-secret".into(),
        public_origin: ORIGIN.into(),
        bind: "127.0.0.1:0".parse().unwrap(),
        discord_api: format!("{discord}/api/v10"),
        daily_messages: 100,
        request_timeout: Duration::from_secs(180),
    }
}

/// A pool that never connects: requests that reach the database fail instead of hanging.
fn offline_database() -> Database {
    Database {
        pool: MySqlPoolOptions::new()
            .acquire_timeout(Duration::from_secs(1))
            .connect_lazy("mysql://nobody:nothing@127.0.0.1:1/none")
            .unwrap(),
    }
}

/// The web state with Discord (OAuth2 and the bot's REST client) pointed at `discord`.
fn state(db: Database, discord: &str) -> AppState {
    let http = HttpBuilder::new("bot-token")
        .proxy(discord)
        .ratelimiter_disabled(true)
        .build();
    Web::new(
        config(discord),
        Shared {
            db,
            agent: Agent::new("http://127.0.0.1:9", "unused".into(), "unused".into()).unwrap(),
            limits: Arc::new(Limits::new(4)),
            http: Arc::new(http),
            bot_guilds: Default::default(),
            discord_ready: Arc::new(AtomicBool::new(true)),
            discord_cache: Default::default(),
            knowledge: None,
            chat: Default::default(),
        },
    )
    .unwrap()
}

async fn send(state: &AppState, request: Request<Body>) -> (StatusCode, HeaderMap, Bytes) {
    let response = web::router(state.clone()).oneshot(request).await.unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let body = response.into_body().collect().await.unwrap().to_bytes();
    (status, headers, body)
}

fn get(uri: &str) -> Request<Body> {
    Request::get(uri).body(Body::empty()).unwrap()
}

fn json_body(body: &Bytes) -> Value {
    serde_json::from_slice(body).unwrap()
}

fn set_cookies(headers: &HeaderMap) -> Vec<String> {
    headers
        .get_all(header::SET_COOKIE)
        .iter()
        .map(|value| value.to_str().unwrap().to_owned())
        .collect()
}

#[tokio::test]
async fn pages_have_security_headers_and_cacheable_assets() {
    let state = state(offline_database(), "http://127.0.0.1:9");
    for uri in [
        "/",
        "/static/app.js",
        "/privacy",
        "/terms",
        "/nowhere",
        "/api/nowhere",
    ] {
        let (_, headers, _) = send(&state, get(uri)).await;
        assert_eq!(headers[header::CONTENT_SECURITY_POLICY], POLICY, "{uri}");
        assert_eq!(headers[header::X_CONTENT_TYPE_OPTIONS], "nosniff", "{uri}");
        assert_eq!(
            headers["cross-origin-opener-policy"], "same-origin",
            "{uri}"
        );
        assert_eq!(headers[header::REFERRER_POLICY], "same-origin", "{uri}");
    }

    let (status, headers, body) = send(&state, get("/")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers[header::CONTENT_TYPE], "text/html; charset=utf-8");
    assert_eq!(headers[header::CACHE_CONTROL], "no-cache");
    let html = std::str::from_utf8(&body).unwrap();
    assert!(html.contains("/static/app.js"));
    // The CSP forbids inline code; the page must not depend on it.
    assert!(!html.contains("<script>") && !html.contains("style="));
    let etag = headers[header::ETAG].clone();
    let (status, headers, body) = send(
        &state,
        Request::get("/")
            .header(header::IF_NONE_MATCH, etag.clone())
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_MODIFIED);
    assert_eq!(headers[header::ETAG], etag);
    assert!(body.is_empty());

    for (uri, content_type) in [
        ("/static/app.js", "text/javascript; charset=utf-8"),
        ("/static/app.css", "text/css; charset=utf-8"),
        ("/static/favicon.svg", "image/svg+xml"),
        (
            "/static/vendor/purify.min.js",
            "text/javascript; charset=utf-8",
        ),
        (
            "/static/vendor/marked.min.js",
            "text/javascript; charset=utf-8",
        ),
        ("/static/vendor/LICENSES.txt", "text/plain; charset=utf-8"),
    ] {
        let (status, headers, _) = send(&state, get(uri)).await;
        assert_eq!(status, StatusCode::OK, "{uri}");
        assert_eq!(headers[header::CONTENT_TYPE], content_type, "{uri}");
    }
    for uri in ["/static/missing.js", "/static/../Cargo.toml", "/static/"] {
        assert_eq!(
            send(&state, get(uri)).await.0,
            StatusCode::NOT_FOUND,
            "{uri}"
        );
    }
    let (status, headers, body) = send(&state, get("/privacy")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers[header::CONTENT_TYPE], "text/html; charset=utf-8");
    assert!(
        std::str::from_utf8(&body)
            .unwrap()
            .contains("<h1>プライバシーポリシー</h1>")
    );
    let (status, _, body) = send(&state, get("/api/nowhere")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(json_body(&body)["error"], "not_found");
}

#[tokio::test]
async fn writes_from_other_origins_are_rejected() {
    let state = state(offline_database(), "http://127.0.0.1:9");
    let post = |origin: Option<&str>| {
        let mut request = Request::post("/auth/logout");
        if let Some(origin) = origin {
            request = request.header(header::ORIGIN, origin);
        }
        request.body(Body::empty()).unwrap()
    };
    for origin in [
        None,
        Some("https://evil.example"),
        Some("null"),
        Some("http://bot.example"),
    ] {
        let (status, headers, body) = send(&state, post(origin)).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{origin:?}");
        assert_eq!(json_body(&body)["error"], "cross_origin");
        assert_eq!(headers[header::CONTENT_SECURITY_POLICY], POLICY);
        assert_eq!(headers[header::CACHE_CONTROL], "no-store");
    }
    let (status, _, body) = send(
        &state,
        Request::put("/api/guilds/1/config/roles")
            .header(header::ORIGIN, "https://evil.example")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(r#"{"use":[],"manage":[]}"#))
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(json_body(&body)["error"], "cross_origin");

    // Same origin without a session: nothing to delete, the cookie is cleared anyway.
    let (status, headers, _) = send(&state, post(Some(ORIGIN))).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert_eq!(
        set_cookies(&headers),
        ["__Host-session=; Max-Age=0; Path=/; HttpOnly; SameSite=Lax; Secure"]
    );
}

#[tokio::test]
async fn login_redirects_to_discord_with_a_state_cookie() {
    let state = state(offline_database(), "http://127.0.0.1:9");
    let (status, headers, _) = send(&state, get("/auth/login")).await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    let location = Url::parse(headers[header::LOCATION].to_str().unwrap()).unwrap();
    assert_eq!(
        location.origin().ascii_serialization(),
        "https://discord.com"
    );
    assert_eq!(location.path(), "/oauth2/authorize");
    let query: std::collections::HashMap<_, _> = location.query_pairs().into_owned().collect();
    assert_eq!(query["response_type"], "code");
    assert_eq!(query["client_id"], "4242");
    assert_eq!(query["scope"], "identify guilds");
    assert_eq!(query["redirect_uri"], "https://bot.example/auth/callback");
    let cookies = set_cookies(&headers);
    assert_eq!(cookies.len(), 1);
    let state_value = &query["state"];
    assert_eq!(state_value.len(), 43);
    assert_eq!(
        cookies[0],
        format!("__Host-oauth={state_value}; Max-Age=600; Path=/; HttpOnly; SameSite=Lax; Secure")
    );
    // Every login gets a new state.
    let (_, again, _) = send(&state, get("/auth/login")).await;
    assert_ne!(set_cookies(&again), cookies);
    assert_eq!(headers[header::CACHE_CONTROL], "no-store");
}

#[tokio::test]
async fn callback_rejects_missing_or_wrong_state_without_calling_discord() {
    let discord = MockServer::start().await;
    Mock::given(wiremock::matchers::any())
        .respond_with(ResponseTemplate::new(500))
        .expect(0)
        .mount(&discord)
        .await;
    let state = state(offline_database(), &discord.uri());
    let good = security::encode(&security::new_token());
    let other = security::encode(&security::new_token());
    let callback = |query: String, cookie: Option<String>| {
        let mut request = Request::get(format!("/auth/callback?{query}"));
        if let Some(cookie) = cookie {
            request = request.header(header::COOKIE, cookie);
        }
        request.body(Body::empty()).unwrap()
    };
    for (query, cookie) in [
        (format!("code=abc&state={good}"), None),
        (
            format!("code=abc&state={good}"),
            Some(format!("__Host-oauth={other}")),
        ),
        ("code=abc".to_owned(), Some(format!("__Host-oauth={good}"))),
        (
            format!("state={good}"),
            Some(format!("__Host-oauth={good}")),
        ),
        (
            format!("code=abc&state={good}"),
            Some(format!("oauth={good}")),
        ),
        (
            "code=abc&state=".to_owned(),
            Some("__Host-oauth=".to_owned()),
        ),
        (
            format!("error=access_denied&state={good}"),
            Some(format!("__Host-oauth={good}")),
        ),
    ] {
        let (status, headers, body) = send(&state, callback(query.clone(), cookie)).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{query}");
        assert!(
            std::str::from_utf8(&body)
                .unwrap()
                .contains("ログインできませんでした")
        );
        assert_eq!(
            set_cookies(&headers),
            ["__Host-oauth=; Max-Age=0; Path=/; HttpOnly; SameSite=Lax; Secure"]
        );
        assert_eq!(headers[header::CONTENT_SECURITY_POLICY], POLICY);
    }
}

/// A callback whose state cookie and parameter match, as any script can send.
fn forged_callback() -> Request<Body> {
    let value = security::encode(&security::new_token());
    Request::get(format!("/auth/callback?code=c&state={value}"))
        .header(header::COOKIE, format!("__Host-oauth={value}"))
        .body(Body::empty())
        .unwrap()
}

fn retry_after(headers: &HeaderMap) -> u64 {
    headers[header::RETRY_AFTER]
        .to_str()
        .unwrap()
        .parse()
        .unwrap()
}

/// Token exchanges that Discord refuses (a wrong code), counted by wiremock.
async fn mock_rejected_exchange(discord: &MockServer, delay: Duration, expect: u64) {
    Mock::given(method("POST"))
        .and(path("/api/v10/oauth2/token"))
        .respond_with(
            ResponseTemplate::new(400)
                .set_body_json(json!({"error": "invalid_grant"}))
                .set_delay(delay),
        )
        .expect(expect)
        .mount(discord)
        .await;
}

#[tokio::test]
async fn login_floods_are_refused_before_reaching_discord() {
    // At most 20 logins a minute reach Discord, however fast they arrive.
    let discord = MockServer::start().await;
    mock_rejected_exchange(&discord, Duration::ZERO, 20).await;
    let state = state(offline_database(), &discord.uri());
    for attempt in 0..30 {
        let (status, headers, body) = send(&state, forged_callback()).await;
        if attempt < 20 {
            assert_eq!(status, StatusCode::BAD_REQUEST, "{attempt}");
            continue;
        }
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{attempt}");
        assert!((1..=60).contains(&retry_after(&headers)));
        assert!(
            std::str::from_utf8(&body)
                .unwrap()
                .contains("混み合っています")
        );
        assert_eq!(
            set_cookies(&headers),
            ["__Host-oauth=; Max-Age=0; Path=/; HttpOnly; SameSite=Lax; Secure"]
        );
    }
    discord.verify().await;

    // And at most two at a time; the others are refused at once instead of queueing.
    let discord = MockServer::start().await;
    mock_rejected_exchange(&discord, Duration::from_millis(500), 2).await;
    let state = self::state(offline_database(), &discord.uri());
    let mut logins = tokio::task::JoinSet::new();
    for _ in 0..6 {
        let state = state.clone();
        logins.spawn(async move { send(&state, forged_callback()).await.0 });
    }
    let mut statuses = logins.join_all().await;
    statuses.sort();
    assert_eq!(
        statuses,
        [
            StatusCode::BAD_REQUEST,
            StatusCode::BAD_REQUEST,
            StatusCode::SERVICE_UNAVAILABLE,
            StatusCode::SERVICE_UNAVAILABLE,
            StatusCode::SERVICE_UNAVAILABLE,
            StatusCode::SERVICE_UNAVAILABLE,
        ]
    );
    discord.verify().await;
}

#[tokio::test]
async fn a_discord_rate_limit_pauses_logins() {
    let discord = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/v10/oauth2/token"))
        .respond_with(
            ResponseTemplate::new(429)
                .insert_header("retry-after", "30")
                .set_body_json(json!({"retry_after": 29.5, "global": false})),
        )
        .expect(1)
        .mount(&discord)
        .await;
    let state = state(offline_database(), &discord.uri());
    let (status, headers, _) = send(&state, forged_callback()).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(retry_after(&headers), 30);
    for _ in 0..3 {
        let (status, headers, _) = send(&state, forged_callback()).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert!((29..=30).contains(&retry_after(&headers)));
    }
    discord.verify().await;
}

/// A code exchange and user lookup that succeed; the guild list answers `guilds_status`.
async fn mock_login(discord: &MockServer, guilds_status: u16) {
    Mock::given(method("POST"))
        .and(path("/api/v10/oauth2/token"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({"access_token": "user-token"})),
        )
        .expect(1)
        .mount(discord)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v10/users/@me"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"id": "940001", "username": "u", "global_name": null})),
        )
        .expect(1)
        .mount(discord)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v10/users/@me/guilds"))
        .respond_with(ResponseTemplate::new(guilds_status).set_body_json(json!([])))
        .expect(1)
        .mount(discord)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/v10/oauth2/token/revoke"))
        .and(body_string_contains("token=user-token"))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(discord)
        .await;
}

#[tokio::test]
async fn failed_logins_still_revoke_the_user_token() {
    // Discord fails after the exchange; then the database fails after Discord answered.
    for (guilds_status, expected) in [
        (500, StatusCode::BAD_GATEWAY),
        (200, StatusCode::INTERNAL_SERVER_ERROR),
    ] {
        let discord = MockServer::start().await;
        mock_login(&discord, guilds_status).await;
        let state = state(offline_database(), &discord.uri());
        let (status, headers, _) = send(&state, forged_callback()).await;
        assert_eq!(status, expected, "{guilds_status}");
        // No session cookie.
        assert_eq!(
            set_cookies(&headers),
            ["__Host-oauth=; Max-Age=0; Path=/; HttpOnly; SameSite=Lax; Secure"]
        );
        discord.verify().await;
    }
}

#[tokio::test]
async fn api_requires_a_session() {
    let state = state(offline_database(), "http://127.0.0.1:9");
    for cookie in [None, Some("__Host-session=short"), Some("session=x")] {
        for uri in ["/api/me", "/api/guilds/1/roles"] {
            let mut request = Request::get(uri);
            if let Some(cookie) = cookie {
                request = request.header(header::COOKIE, cookie);
            }
            let (status, headers, body) = send(&state, request.body(Body::empty()).unwrap()).await;
            assert_eq!(status, StatusCode::UNAUTHORIZED, "{uri} {cookie:?}");
            let body = json_body(&body);
            assert_eq!(body["error"], "unauthenticated");
            assert_eq!(body["message"], "ログインしてください。");
            assert_eq!(headers[header::CACHE_CONTROL], "no-store");
        }
    }
    // A well-formed token is looked up; an unreachable database is reported, not a login.
    let token = security::encode(&security::new_token());
    let (status, _, body) = send(
        &state,
        Request::get("/api/me")
            .header(header::COOKIE, format!("__Host-session={token}"))
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(json_body(&body)["error"], "database");
}

#[tokio::test]
async fn json_bodies_need_the_json_content_type_and_a_small_size() {
    let app = Router::new()
        .route(
            "/",
            put(|JsonBody(value): JsonBody<Value>| async move { value.to_string() }),
        )
        .layer(DefaultBodyLimit::max(64));
    let call = |content_type: Option<&str>, body: &str| {
        let mut request = Request::put("/");
        if let Some(content_type) = content_type {
            request = request.header(header::CONTENT_TYPE, content_type);
        }
        app.clone()
            .oneshot(request.body(Body::from(body.to_owned())).unwrap())
    };
    let status = |response: axum::response::Response| response.status();
    assert_eq!(
        status(call(Some("application/json"), "[1]").await.unwrap()),
        StatusCode::OK
    );
    for content_type in [
        None,
        Some("text/plain"),
        Some("application/x-www-form-urlencoded"),
    ] {
        let response = call(content_type, "[1]").await.unwrap();
        assert_eq!(response.status(), StatusCode::UNSUPPORTED_MEDIA_TYPE);
        let body = response.into_body().collect().await.unwrap().to_bytes();
        assert_eq!(json_body(&body)["error"], "unsupported_media_type");
    }
    let response = call(
        Some("application/json"),
        &format!("[{}]", "1,".repeat(40) + "1"),
    )
    .await;
    assert_eq!(status(response.unwrap()), StatusCode::PAYLOAD_TOO_LARGE);
    let response = call(Some("application/json"), "{").await.unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body = response.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(json_body(&body)["error"], "invalid_request");
}

#[tokio::test]
async fn healthz_needs_database_and_gateway() {
    let state = state(offline_database(), "http://127.0.0.1:9");
    let (status, headers, body) = send(&state, get("/healthz")).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(&body[..], b"unavailable");
    assert_eq!(headers[header::CACHE_CONTROL], "no-store");

    // The std-only client used by `bot healthcheck`.
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let server = std::thread::spawn(move || {
        for reply in [
            "HTTP/1.1 200 OK\r\n\r\nok",
            "HTTP/1.1 503 Service Unavailable\r\n\r\n",
        ] {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0_u8; 256];
            let read = stream.read(&mut request).unwrap();
            assert!(request[..read].starts_with(b"GET /healthz HTTP/1.1\r\n"));
            stream.write_all(reply.as_bytes()).unwrap();
        }
    });
    assert!(web::probe(address).unwrap());
    assert!(!web::probe(address).unwrap());
    server.join().unwrap();
    assert!(web::probe(address).is_err());
}

#[tokio::test]
async fn oauth_client_exchanges_reads_paginates_and_revokes() {
    let discord = MockServer::start().await;
    // Basic auth of 4242:test-client-secret.
    let basic = "Basic NDI0Mjp0ZXN0LWNsaWVudC1zZWNyZXQ=";
    Mock::given(method("POST"))
        .and(path("/api/v10/oauth2/token"))
        .and(mock_header("authorization", basic))
        .and(mock_header(
            "content-type",
            "application/x-www-form-urlencoded",
        ))
        .and(body_string_contains("grant_type=authorization_code"))
        .and(body_string_contains("code=the-code"))
        .and(body_string_contains(
            "redirect_uri=https%3A%2F%2Fbot.example%2Fauth%2Fcallback",
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "access_token": "user-token", "token_type": "Bearer", "expires_in": 604800,
            "refresh_token": "refresh", "scope": "identify guilds"
        })))
        .expect(1)
        .mount(&discord)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v10/users/@me"))
        .and(mock_header("authorization", "Bearer user-token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "123456789012345678", "username": "tester", "global_name": "テスター",
            "discriminator": "0", "avatar": null
        })))
        .expect(1)
        .mount(&discord)
        .await;
    let page = |range: std::ops::RangeInclusive<u64>| {
        Value::Array(
            range
                .map(|id| json!({"id": id.to_string(), "name": format!("guild {id}"), "icon": null, "owner": false, "permissions": "0", "features": []}))
                .collect(),
        )
    };
    Mock::given(method("GET"))
        .and(path("/api/v10/users/@me/guilds"))
        .and(query_param("limit", "200"))
        .respond_with(move |request: &MockRequest| {
            let after = request
                .url
                .query_pairs()
                .find(|(key, _)| key == "after")
                .map(|(_, value)| value.into_owned());
            match after.as_deref() {
                None => ResponseTemplate::new(200).set_body_json(page(1..=200)),
                Some("200") => ResponseTemplate::new(200).set_body_json(page(201..=203)),
                other => panic!("unexpected page after {other:?}"),
            }
        })
        .expect(2)
        .mount(&discord)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/v10/oauth2/token/revoke"))
        .and(mock_header("authorization", basic))
        .and(body_string_contains("token=user-token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
        .expect(1)
        .mount(&discord)
        .await;

    let client = OAuthClient::new(&config(&discord.uri())).unwrap();
    let token = client
        .exchange("the-code", "https://bot.example/auth/callback")
        .await
        .unwrap();
    let user = client.user(&token).await.unwrap();
    assert_eq!(user.id, 123_456_789_012_345_678);
    assert_eq!(user.name, "テスター");
    let guilds = client.guilds(&token).await.unwrap();
    assert_eq!(guilds.len(), 203);
    assert_eq!(
        guilds.last().unwrap(),
        &SessionGuild {
            id: 203,
            name: "guild 203".into()
        }
    );
    client.revoke(&token).await.unwrap();
}

#[tokio::test]
async fn oauth_errors_are_classified() {
    for (status, body, expected) in [
        (400, json!({"error": "invalid_grant"}), "oauth_rejected"),
        (401, json!({"error": "invalid_client"}), "oauth_rejected"),
        (429, json!({"retry_after": 1}), "discord_rate_limit"),
        (502, json!({}), "discord_upstream"),
        (200, json!({"unexpected": true}), "discord_invalid_response"),
    ] {
        let discord = MockServer::start().await;
        Mock::given(path("/api/v10/oauth2/token"))
            .respond_with(ResponseTemplate::new(status).set_body_json(body))
            .mount(&discord)
            .await;
        let client = OAuthClient::new(&config(&discord.uri())).unwrap();
        let error: OAuthError = client
            .exchange("code", "https://bot.example/auth/callback")
            .await
            .err()
            .unwrap();
        assert_eq!(error.to_string(), expected, "{status}");
    }
    let client = OAuthClient::new(&config("http://127.0.0.1:1")).unwrap();
    let error = client.exchange("code", "x").await.err().unwrap();
    assert!(matches!(error, OAuthError::Network));
}

fn role(id: u64, name: &str, position: u16, permissions: u64) -> Value {
    json!({
        "id": id.to_string(), "name": name, "color": 0x3498db,
        "colors": {"primary_color": 0x3498db, "secondary_color": null, "tertiary_color": null},
        "hoist": false, "icon": null,
        "unicode_emoji": null, "position": position, "permissions": permissions.to_string(),
        "managed": false, "mentionable": false, "flags": 0
    })
}

/// A Discord guild object with @everyone (no permissions) plus `roles`.
fn guild_json(guild: u64, owner: u64, roles: &[Value]) -> Value {
    let mut all = vec![role(guild, "@everyone", 0, 0)];
    all.extend_from_slice(roles);
    json!({
        "id": guild.to_string(), "name": "テストサーバー", "icon": null, "icon_hash": null,
        "splash": null, "discovery_splash": null, "owner_id": owner.to_string(),
        "afk_channel_id": null, "afk_timeout": 300, "widget_enabled": false,
        "widget_channel_id": null, "verification_level": 0, "default_message_notifications": 0,
        "explicit_content_filter": 0, "roles": all, "emojis": [], "features": [], "mfa_level": 0,
        "application_id": null, "system_channel_id": null, "system_channel_flags": 0,
        "rules_channel_id": null, "max_presences": null, "max_members": 500000,
        "vanity_url_code": null, "description": null, "banner": null, "premium_tier": 0,
        "premium_subscription_count": 0, "preferred_locale": "ja",
        "public_updates_channel_id": null, "max_video_channel_users": 25, "nsfw_level": 0,
        "stickers": [], "premium_progress_bar_enabled": false, "safety_alerts_channel_id": null
    })
}

fn member_json(user: u64, roles: &[u64]) -> Value {
    json!({
        "user": {"id": user.to_string(), "username": "member", "discriminator": "0",
                 "global_name": null, "avatar": null},
        "nick": null, "avatar": null,
        "roles": roles.iter().map(u64::to_string).collect::<Vec<_>>(),
        "joined_at": "2024-01-01T00:00:00.000000+00:00", "premium_since": null,
        "deaf": false, "mute": false, "flags": 0, "pending": false,
        "communication_disabled_until": null
    })
}

async fn mock_member(discord: &MockServer, guild: u64, user: u64, roles: &[u64], expect: u64) {
    Mock::given(method("GET"))
        .and(path(format!("/api/v10/guilds/{guild}/members/{user}")))
        .and(mock_header("authorization", "Bot bot-token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(member_json(user, roles)))
        .expect(expect)
        .mount(discord)
        .await;
}

async fn mock_guild(discord: &MockServer, guild: u64, owner: u64, roles: &[Value], expect: u64) {
    Mock::given(method("GET"))
        .and(path(format!("/api/v10/guilds/{guild}")))
        .respond_with(ResponseTemplate::new(200).set_body_json(guild_json(guild, owner, roles)))
        .expect(expect)
        .mount(discord)
        .await;
}

fn bot_http(discord: &MockServer) -> Arc<serenity::http::Http> {
    Arc::new(
        HttpBuilder::new("bot-token")
            .proxy(discord.uri())
            .ratelimiter_disabled(true)
            .build(),
    )
}

#[tokio::test]
async fn discord_lookups_are_cached_and_decided_by_access_rules() {
    let discord = MockServer::start().await;
    let (guild, owner, user, outsider) = (500_u64, 501_u64, 502_u64, 503_u64);
    let (members_role, admins_role) = (510_u64, 511_u64);
    mock_member(&discord, guild, user, &[members_role], 1).await;
    Mock::given(method("GET"))
        .and(path(format!("/api/v10/guilds/{guild}/members/{outsider}")))
        .respond_with(
            ResponseTemplate::new(404)
                .set_body_json(json!({"message": "Unknown Member", "code": 10007})),
        )
        .expect(1)
        .mount(&discord)
        .await;
    // Fetched again after the cache entry is dropped by a role event.
    mock_guild(
        &discord,
        guild,
        owner,
        &[
            role(members_role, "メンバー", 1, 0),
            role(admins_role, "管理", 2, MANAGE_GUILD),
        ],
        2,
    )
    .await;
    let cache = Arc::new(DiscordCache::default());
    let authz = Authz::new(bot_http(&discord), cache.clone());
    let settings = GuildAccess {
        allowed: true,
        use_roles: vec![members_role],
        manage_roles: vec![],
    };
    for _ in 0..2 {
        let member = authz.member(guild, user).await.unwrap().unwrap();
        let partial = authz.guild(guild).await.unwrap();
        let access = authz::decide(&settings, &partial, &member);
        assert!(access.use_bot && !access.manage_knowledge && !access.configure);
        assert!(authz.member(guild, outsider).await.unwrap().is_none());
    }
    // The mocks' call counts show the second round came from the cache.
    cache.forget_guild(guild);
    let partial = authz.guild(guild).await.unwrap();
    assert!(partial.emojis.is_empty());
    assert_eq!(partial.roles.len(), 3);
}

#[tokio::test]
async fn owners_and_administrators_are_recognized() {
    let discord = MockServer::start().await;
    let (guild, owner, admin) = (600_u64, 601_u64, 602_u64);
    let admin_role = 610_u64;
    mock_member(&discord, guild, owner, &[], 1).await;
    mock_member(&discord, guild, admin, &[admin_role], 1).await;
    mock_guild(
        &discord,
        guild,
        owner,
        &[role(admin_role, "Admin", 1, ADMINISTRATOR)],
        1,
    )
    .await;
    let authz = Authz::new(bot_http(&discord), Arc::default());
    let settings = GuildAccess {
        allowed: true,
        use_roles: vec![],
        manage_roles: vec![],
    };
    let partial = authz.guild(guild).await.unwrap();
    for user in [owner, admin] {
        let member = authz.member(guild, user).await.unwrap().unwrap();
        let access = authz::decide(&settings, &partial, &member);
        // May configure, but using the bot still needs a configured role.
        assert!(access.configure && access.manage_knowledge && !access.use_bot);
    }
}

#[tokio::test]
async fn a_guild_event_during_a_lookup_keeps_its_answer_out_of_the_cache() {
    let discord = MockServer::start().await;
    let guild = 650_u64;
    Mock::given(method("GET"))
        .and(path(format!("/api/v10/guilds/{guild}")))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(guild_json(guild, 651, &[]))
                .set_delay(Duration::from_millis(400)),
        )
        .expect(2)
        .mount(&discord)
        .await;
    let cache = Arc::new(DiscordCache::default());
    let authz = Arc::new(Authz::new(bot_http(&discord), cache.clone()));
    let lookup = tokio::spawn({
        let authz = authz.clone();
        async move { authz.guild(guild).await.map(|partial| partial.id.get()) }
    });
    tokio::time::sleep(Duration::from_millis(100)).await;
    // A role change arrives while Discord is still answering with the old state.
    cache.forget_guild(guild);
    assert_eq!(lookup.await.unwrap(), Ok(guild));
    // So the next request asks again instead of using that answer for five minutes.
    authz.guild(guild).await.unwrap();
    discord.verify().await;
}

/// A member as Discord REST returns it (serenity adds the guild ID).
fn member(guild: u64, user: u64, roles: &[u64], change: impl FnOnce(&mut Value)) -> Member {
    let mut value = member_json(user, roles);
    value["guild_id"] = json!(guild);
    change(&mut value);
    serde_json::from_value(value).unwrap()
}

#[test]
fn timed_out_and_pending_members_get_no_rights() {
    let (guild, owner, user) = (660_u64, 661_u64, 662_u64);
    let (members_role, staff_role, admin_role) = (670_u64, 671_u64, 672_u64);
    let partial: PartialGuild = serde_json::from_value(guild_json(
        guild,
        owner,
        &[
            role(members_role, "メンバー", 1, 0),
            role(staff_role, "運営", 2, MANAGE_GUILD),
            role(admin_role, "管理者", 3, ADMINISTRATOR),
        ],
    ))
    .unwrap();
    let settings = GuildAccess {
        allowed: true,
        use_roles: vec![members_role],
        manage_roles: vec![],
    };
    let decide = |roles: &[u64], change: fn(&mut Value)| {
        authz::decide(&settings, &partial, &member(guild, user, roles, change))
    };
    let everything = Access {
        use_bot: true,
        manage_knowledge: true,
        configure: true,
    };
    let staff = [members_role, staff_role];
    assert_eq!(decide(&staff, |_| {}), everything);
    let timed_out = |member: &mut Value| {
        member["communication_disabled_until"] = json!("2099-01-01T00:00:00.000000+00:00")
    };
    let pending = |member: &mut Value| member["pending"] = json!(true);
    let ended = |member: &mut Value| {
        member["communication_disabled_until"] = json!("2020-01-01T00:00:00.000000+00:00")
    };
    assert_eq!(decide(&staff, timed_out), Access::default());
    assert_eq!(decide(&staff, pending), Access::default());
    assert_eq!(decide(&staff, ended), everything);
    // Discord does not apply timeouts to administrators.
    let admin = [members_role, admin_role];
    assert_eq!(decide(&admin, timed_out), everything);
}

#[tokio::test]
async fn slow_or_failing_discord_is_unavailable_and_not_cached() {
    let discord = MockServer::start().await;
    Mock::given(path("/api/v10/guilds/700/members/701"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(member_json(701, &[]))
                .set_delay(Duration::from_secs(2)),
        )
        .mount(&discord)
        .await;
    Mock::given(path("/api/v10/guilds/700"))
        .respond_with(ResponseTemplate::new(500).set_body_json(json!({"message": "x", "code": 0})))
        .expect(2)
        .mount(&discord)
        .await;
    let authz =
        Authz::new(bot_http(&discord), Arc::default()).with_timeout(Duration::from_millis(200));
    assert_eq!(authz.member(700, 701).await.err(), Some(Unavailable));
    assert!(authz.guild(700).await.is_err());
    assert!(authz.guild(700).await.is_err());
}

// ---- Database tests (compose.test.yaml) ----

async fn clear_user(db: &Database, user: u64) {
    sqlx::query("DELETE FROM web_sessions WHERE user_id=?")
        .bind(user)
        .execute(&db.pool)
        .await
        .unwrap();
}

async fn clear_guilds(db: &Database, guilds: &[u64]) {
    for guild in guilds {
        for table in ["guild_roles", "guilds"] {
            sqlx::query(&format!("DELETE FROM {table} WHERE guild_id=?"))
                .bind(guild)
                .execute(&db.pool)
                .await
                .unwrap();
        }
    }
}

#[tokio::test]
#[ignore = "requires compose.test.yaml and TEST_DATABASE_URL"]
async fn web_sessions() {
    let db = database().await;
    let (user, other) = (910_001_u64, 910_002_u64);
    clear_user(&db, user).await;
    clear_user(&db, other).await;
    let guilds = vec![SessionGuild {
        id: 910_100,
        name: "サーバー😀".into(),
    }];
    let now = Utc::now();
    let token = security::new_token();
    let hash = security::hash(&token);
    db.create_session(&hash, user, "テスト", &guilds, now)
        .await
        .unwrap();
    let session = db.session(&hash, now).await.unwrap().unwrap();
    assert_eq!(session.user_id, user);
    assert_eq!(session.user_name, "テスト");
    assert_eq!(session.guilds, guilds);
    let expires_at: chrono::NaiveDateTime =
        sqlx::query_scalar("SELECT expires_at FROM web_sessions WHERE user_id=?")
            .bind(user)
            .fetch_one(&db.pool)
            .await
            .unwrap();
    assert_eq!(
        expires_at.and_utc().timestamp_millis(),
        (now + chrono::Duration::days(7)).timestamp_millis()
    );
    // Only the hash is stored.
    let stored: Vec<u8> = sqlx::query_scalar("SELECT token_hash FROM web_sessions WHERE user_id=?")
        .bind(user)
        .fetch_one(&db.pool)
        .await
        .unwrap();
    assert_eq!(stored, hash);
    assert!(db.session(&token, now).await.unwrap().is_none());
    // No sliding renewal: the session ends 7 days after login however much it is used.
    assert!(
        db.session(&hash, now + chrono::Duration::days(7))
            .await
            .unwrap()
            .is_none()
    );

    // At most ten sessions per user; the oldest go first, other users are untouched.
    let other_hash = security::hash(&security::new_token());
    db.create_session(&other_hash, other, "other", &[], now)
        .await
        .unwrap();
    let mut hashes = vec![hash];
    for minute in 1..=MAX_SESSIONS_PER_USER as i64 {
        let hash = security::hash(&security::new_token());
        db.create_session(
            &hash,
            user,
            "テスト",
            &[],
            now + chrono::Duration::minutes(minute),
        )
        .await
        .unwrap();
        hashes.push(hash);
    }
    let later = now + chrono::Duration::hours(1);
    assert!(db.session(&hashes[0], later).await.unwrap().is_none());
    for hash in &hashes[1..] {
        assert!(db.session(hash, later).await.unwrap().is_some());
    }
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM web_sessions WHERE user_id=?")
        .bind(user)
        .fetch_one(&db.pool)
        .await
        .unwrap();
    assert_eq!(count, MAX_SESSIONS_PER_USER as i64);
    assert!(db.session(&other_hash, later).await.unwrap().is_some());

    assert!(db.delete_session(&hashes[1]).await.unwrap());
    assert!(!db.delete_session(&hashes[1]).await.unwrap());

    // The hourly purge removes sessions that have expired by now, and only those.
    let expired = security::hash(&security::new_token());
    db.create_session(
        &expired,
        other,
        "other",
        &[],
        now - chrono::Duration::days(8),
    )
    .await
    .unwrap();
    assert!(db.purge_sessions(Utc::now()).await.unwrap() >= 1);
    let remaining: Vec<Vec<u8>> =
        sqlx::query_scalar("SELECT token_hash FROM web_sessions WHERE user_id IN (?,?)")
            .bind(user)
            .bind(other)
            .fetch_all(&db.pool)
            .await
            .unwrap();
    assert_eq!(remaining.len(), MAX_SESSIONS_PER_USER);
    assert!(!remaining.contains(&expired.to_vec()));
    clear_user(&db, user).await;
    clear_user(&db, other).await;
}

#[tokio::test]
#[ignore = "requires compose.test.yaml and TEST_DATABASE_URL"]
async fn concurrent_logins_of_one_user_all_succeed() {
    let db = database().await;
    let user = 950_001_u64;
    clear_user(&db, user).await;
    let mut logins = tokio::task::JoinSet::new();
    for _ in 0..12 {
        let db = db.clone();
        logins.spawn(async move {
            let hash = security::hash(&security::new_token());
            db.create_session(&hash, user, "u", &[], Utc::now()).await
        });
    }
    for result in logins.join_all().await {
        result.unwrap();
    }
    // Trimming may lag behind concurrent logins, but the next login catches up.
    db.create_session(
        &security::hash(&security::new_token()),
        user,
        "u",
        &[],
        Utc::now(),
    )
    .await
    .unwrap();
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM web_sessions WHERE user_id=?")
        .bind(user)
        .fetch_one(&db.pool)
        .await
        .unwrap();
    assert_eq!(count, MAX_SESSIONS_PER_USER as i64);
    clear_user(&db, user).await;
}

#[tokio::test]
#[ignore = "requires compose.test.yaml and TEST_DATABASE_URL"]
async fn healthz_reports_database_and_gateway() {
    let state = state(database().await, "http://127.0.0.1:9");
    let (status, _, body) = send(&state, get("/healthz")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(&body[..], b"ok");
    state.discord_ready.store(false, Ordering::Relaxed);
    let (status, _, body) = send(&state, get("/healthz")).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(&body[..], b"unavailable");
}

#[tokio::test]
#[ignore = "requires compose.test.yaml and TEST_DATABASE_URL"]
async fn web_login_keeps_only_allowlisted_guilds() {
    let db = database().await;
    let user = 920_001_u64;
    let (allowed, denied, unknown) = (920_101_u64, 920_102_u64, 920_103_u64);
    clear_user(&db, user).await;
    clear_guilds(&db, &[allowed, denied, unknown]).await;
    db.allow_guild(allowed, None).await.unwrap();
    db.allow_guild(denied, None).await.unwrap();
    assert!(db.deny_guild(denied).await.unwrap());
    db.add_guild_role(
        allowed,
        discord_discussion_bot::access::RoleKind::Use,
        allowed,
        None,
    )
    .await
    .unwrap();

    let discord = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/v10/oauth2/token"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({"access_token": "user-token"})),
        )
        .expect(2)
        .mount(&discord)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v10/users/@me"))
        .respond_with(ResponseTemplate::new(200).set_body_json(
            json!({"id": user.to_string(), "username": "login-user", "global_name": null}),
        ))
        .expect(2)
        .mount(&discord)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v10/users/@me/guilds"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([
            {"id": allowed.to_string(), "name": "許可済み"},
            {"id": denied.to_string(), "name": "取り消し済み"},
            {"id": unknown.to_string(), "name": "未登録"}
        ])))
        .expect(2)
        .mount(&discord)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/v10/oauth2/token/revoke"))
        .and(body_string_contains("token=user-token"))
        .respond_with(ResponseTemplate::new(200))
        .expect(2)
        .mount(&discord)
        .await;
    mock_member(&discord, allowed, user, &[], 1).await;
    mock_guild(&discord, allowed, user, &[], 1).await;
    let state = state(db.clone(), &discord.uri());
    state.bot_guilds.write().unwrap().insert(allowed);

    let login = |previous: Option<String>| {
        let state = state.clone();
        async move {
            let (_, headers, _) = send(&state, get("/auth/login")).await;
            let oauth_state = set_cookies(&headers)[0]
                .split(';')
                .next()
                .unwrap()
                .to_owned();
            let value = oauth_state.split_once('=').unwrap().1.to_owned();
            let mut cookie = oauth_state;
            if let Some(previous) = previous {
                cookie = format!("{cookie}; {previous}");
            }
            send(
                &state,
                Request::get(format!("/auth/callback?code=c&state={value}"))
                    .header(header::COOKIE, cookie)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
        }
    };
    let (status, headers, _) = login(None).await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert_eq!(headers[header::LOCATION], "/");
    let cookies = set_cookies(&headers);
    assert_eq!(
        cookies[0],
        "__Host-oauth=; Max-Age=0; Path=/; HttpOnly; SameSite=Lax; Secure"
    );
    let session_cookie = cookies[1].split(';').next().unwrap().to_owned();
    assert!(session_cookie.starts_with("__Host-session="));
    assert!(cookies[1].ends_with("; Max-Age=604800; Path=/; HttpOnly; SameSite=Lax; Secure"));
    // What the session stores, not what /api/me shows: /api/me hides guilds the bot is not in
    // or that are not allowlisted anyway, so only this shows that login filtered them.
    let token = security::decode(session_cookie.split_once('=').unwrap().1).unwrap();
    let stored = db
        .session(&security::hash(&token), Utc::now())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        stored.guilds,
        [SessionGuild {
            id: allowed,
            name: "許可済み".into()
        }]
    );

    let (status, _, body) = send(
        &state,
        Request::get("/api/me")
            .header(header::COOKIE, &session_cookie)
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        json_body(&body),
        json!({
            "user": {"id": user.to_string(), "name": "login-user"},
            "guilds": [{"id": allowed.to_string(), "name": "許可済み",
                        "access": {"use": true, "manage_kb": true, "configure": true}}]
        })
    );

    // Logging in again replaces the previous session instead of adding one.
    let (status, headers, _) = login(Some(session_cookie.clone())).await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    let new_cookie = set_cookies(&headers)[1]
        .split(';')
        .next()
        .unwrap()
        .to_owned();
    assert_ne!(new_cookie, session_cookie);
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM web_sessions WHERE user_id=?")
        .bind(user)
        .fetch_one(&db.pool)
        .await
        .unwrap();
    assert_eq!(count, 1);
    let (status, _, _) = send(
        &state,
        Request::get("/api/me")
            .header(header::COOKIE, &session_cookie)
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    // Logout ends the session.
    let (status, _, _) = send(
        &state,
        Request::post("/auth/logout")
            .header(header::ORIGIN, ORIGIN)
            .header(header::COOKIE, &new_cookie)
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM web_sessions WHERE user_id=?")
        .bind(user)
        .fetch_one(&db.pool)
        .await
        .unwrap();
    assert_eq!(count, 0);
}

#[tokio::test]
#[ignore = "requires compose.test.yaml and TEST_DATABASE_URL"]
async fn web_role_settings() {
    let db = database().await;
    let (manager, member) = (930_001_u64, 930_002_u64);
    let (guild, absent) = (930_101_u64, 930_102_u64);
    let (staff, readers, mods) = (930_201_u64, 930_202_u64, 930_203_u64);
    for user in [manager, member] {
        clear_user(&db, user).await;
    }
    clear_guilds(&db, &[guild, absent]).await;
    db.allow_guild(guild, None).await.unwrap();
    db.allow_guild(absent, None).await.unwrap();
    // Configured earlier from Discord; keeps its creator when it stays selected.
    db.add_guild_role(
        guild,
        discord_discussion_bot::access::RoleKind::Use,
        readers,
        Some(1),
    )
    .await
    .unwrap();

    let discord = MockServer::start().await;
    mock_member(&discord, guild, manager, &[staff], 1).await;
    mock_member(&discord, guild, member, &[readers], 1).await;
    // Nothing about the absent guild (bot not in it) or one outside the session may reach
    // Discord: neither the guild nor any member lookup.
    Mock::given(path_regex(format!(
        "^/api/v10/guilds/({absent}|930999)(/.*)?$"
    )))
    .respond_with(ResponseTemplate::new(500))
    .expect(0)
    .mount(&discord)
    .await;
    let roles = [
        role(staff, "運営", 3, MANAGE_GUILD),
        role(readers, "読者", 2, 0),
        role(mods, "モデレーター", 1, 0),
    ];
    mock_guild(&discord, guild, 1, &roles, 1).await;
    let state = state(db.clone(), &discord.uri());
    state.bot_guilds.write().unwrap().insert(guild);

    let guilds = vec![
        SessionGuild {
            id: guild,
            name: "設定テスト".into(),
        },
        SessionGuild {
            id: absent,
            name: "Bot不在".into(),
        },
    ];
    let mut cookies = Vec::new();
    for user in [manager, member] {
        let token = security::new_token();
        db.create_session(&security::hash(&token), user, "user", &guilds, Utc::now())
            .await
            .unwrap();
        cookies.push(format!("__Host-session={}", security::encode(&token)));
    }
    let (manager_cookie, member_cookie) = (&cookies[0], &cookies[1]);
    let get_roles = |guild: u64, cookie: &str| {
        Request::get(format!("/api/guilds/{guild}/roles"))
            .header(header::COOKIE, cookie)
            .body(Body::empty())
            .unwrap()
    };
    let put_roles = |guild: u64, cookie: &str, body: Value| {
        Request::put(format!("/api/guilds/{guild}/config/roles"))
            .header(header::COOKIE, cookie)
            .header(header::ORIGIN, ORIGIN)
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body.to_string()))
            .unwrap()
    };

    let (status, _, body) = send(&state, get_roles(guild, manager_cookie)).await;
    assert_eq!(status, StatusCode::OK);
    let body = json_body(&body);
    assert_eq!(
        body["guild"],
        json!({"id": guild.to_string(), "name": "テストサーバー"})
    );
    let names: Vec<_> = body["roles"]
        .as_array()
        .unwrap()
        .iter()
        .map(|role| role["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["運営", "読者", "モデレーター", "@everyone"]);
    assert_eq!(body["roles"][0]["color"], 0x3498db);
    assert_eq!(body["use"], json!([readers.to_string()]));
    assert_eq!(body["manage"], json!([]));

    // Members without MANAGE_GUILD may not read or change the settings.
    let (status, _, body) = send(&state, get_roles(guild, member_cookie)).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(json_body(&body)["error"], "forbidden");
    let request = put_roles(guild, member_cookie, json!({"use": [], "manage": []}));
    assert_eq!(send(&state, request).await.0, StatusCode::FORBIDDEN);
    // Guilds the bot is not in, or not in the session, are 404 without asking Discord.
    for target in [absent, 930_999] {
        let (status, _, _) = send(&state, get_roles(target, manager_cookie)).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{target}");
    }
    let (status, _, _) = send(&state, get_roles(guild, "__Host-session=x")).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    // Invalid requests change nothing.
    let distinct: Vec<String> = (1..=26).map(|id| (930_300 + id).to_string()).collect();
    for (body, message) in [
        (json!({"use": ["930299"], "manage": []}), "存在しない"),
        (json!({"use": ["abc"], "manage": []}), "形式"),
        (json!({"use": [], "manage": distinct}), "25個まで"),
        (json!({"use": [staff.to_string()]}), "内容"),
    ] {
        let (status, _, response) = send(&state, put_roles(guild, manager_cookie, body)).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        let response = json_body(&response);
        assert_eq!(response["error"], "invalid_request");
        assert!(
            response["message"].as_str().unwrap().contains(message),
            "{response}"
        );
    }
    assert_eq!(db.guild_access(guild).await.unwrap().use_roles, [readers]);
    // Repeating one role is not "too many".
    let repeated: Vec<String> = (0..30).map(|_| readers.to_string()).collect();
    let request = put_roles(
        guild,
        manager_cookie,
        json!({"use": repeated, "manage": []}),
    );
    let (status, _, body) = send(&state, request).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(json_body(&body)["use"], json!([readers.to_string()]));
    let request = Request::put(format!("/api/guilds/{guild}/config/roles"))
        .header(header::COOKIE, manager_cookie)
        .header(header::ORIGIN, ORIGIN)
        .header(header::CONTENT_TYPE, "text/plain")
        .body(Body::from(r#"{"use":[],"manage":[]}"#))
        .unwrap();
    assert_eq!(
        send(&state, request).await.0,
        StatusCode::UNSUPPORTED_MEDIA_TYPE
    );

    // Both kinds are replaced together; kept roles keep their original creator.
    let request = put_roles(
        guild,
        manager_cookie,
        json!({"use": [readers.to_string(), guild.to_string()], "manage": [mods.to_string()]}),
    );
    let (status, _, body) = send(&state, request).await;
    assert_eq!(status, StatusCode::OK);
    let body = json_body(&body);
    assert_eq!(body["use"], json!([readers.to_string(), guild.to_string()]));
    assert_eq!(body["manage"], json!([mods.to_string()]));
    let settings = db.guild_access(guild).await.unwrap();
    assert_eq!(settings.use_roles, [readers, guild]);
    assert_eq!(settings.manage_roles, [mods]);
    let creators = sqlx::query(
        "SELECT role_id,created_by FROM guild_roles WHERE guild_id=? ORDER BY kind,role_id",
    )
    .bind(guild)
    .fetch_all(&db.pool)
    .await
    .unwrap();
    let creators: Vec<(u64, Option<u64>)> = creators
        .iter()
        .map(|row| (row.get("role_id"), row.get("created_by")))
        .collect();
    assert_eq!(
        creators,
        [
            (mods, Some(manager)),
            (guild, Some(manager)),
            (readers, Some(1))
        ]
    );

    let request = put_roles(guild, manager_cookie, json!({"use": [], "manage": []}));
    let (status, _, _) = send(&state, request).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        db.guild_access(guild).await.unwrap(),
        GuildAccess {
            allowed: true,
            use_roles: vec![],
            manage_roles: vec![],
        }
    );

    // `ops guild deny` cuts off sessions that already exist, without asking Discord (the
    // mocks' call counts are checked when the server is dropped).
    assert!(db.deny_guild(guild).await.unwrap());
    let (status, _, _) = send(&state, get_roles(guild, manager_cookie)).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let request = put_roles(guild, manager_cookie, json!({"use": [], "manage": []}));
    assert_eq!(send(&state, request).await.0, StatusCode::NOT_FOUND);
    let (status, _, body) = send(
        &state,
        Request::get("/api/me")
            .header(header::COOKIE, manager_cookie)
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(json_body(&body)["guilds"], json!([]));
    for user in [manager, member] {
        clear_user(&db, user).await;
    }
}

// ---- Knowledge base API ----

/// The web state with a knowledge base whose provider is never called (no worker runs here).
fn knowledge_state(db: Database, discord: &str, max_upload_bytes: usize) -> AppState {
    let http = HttpBuilder::new("bot-token")
        .proxy(discord)
        .ratelimiter_disabled(true)
        .build();
    let knowledge = Knowledge::new(
        KbConfig {
            providers: vec![ProviderConfig::new(
                ProviderKind::Gemini,
                "web-test",
                "unused",
                "http://127.0.0.1:9",
            )],
            max_upload_bytes,
            max_docs_per_guild: 50,
            max_chunks_per_guild: 5_000,
            max_chunks_total: 10_000_000,
        },
        db.clone(),
    )
    .unwrap();
    Web::new(
        config(discord),
        Shared {
            db,
            agent: Agent::new("http://127.0.0.1:9", "unused".into(), "unused".into()).unwrap(),
            limits: Arc::new(Limits::new(4)),
            http: Arc::new(http),
            bot_guilds: Default::default(),
            discord_ready: Arc::new(AtomicBool::new(true)),
            discord_cache: Default::default(),
            knowledge: Some(Arc::new(knowledge)),
            chat: Default::default(),
        },
    )
    .unwrap()
}

#[tokio::test]
async fn knowledge_writes_need_same_origin_and_a_session() {
    let state = knowledge_state(offline_database(), "http://127.0.0.1:9", 65_536);
    let documents = "/api/guilds/1/kb/documents";
    for (method, uri) in [
        ("POST", documents.to_owned()),
        ("POST", format!("{documents}/1/retry")),
        ("DELETE", format!("{documents}/1")),
    ] {
        let request = Request::builder()
            .method(method)
            .uri(&uri)
            .header("x-file-name", "a.txt")
            .header(header::ORIGIN, "https://evil.example")
            .body(Body::from("text"))
            .unwrap();
        let (status, _, body) = send(&state, request).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{method} {uri}");
        assert_eq!(json_body(&body)["error"], "cross_origin");
    }
    for request in [
        get(documents),
        Request::post(documents)
            .header(header::ORIGIN, ORIGIN)
            .header("x-file-name", "a.txt")
            .body(Body::from("text"))
            .unwrap(),
    ] {
        let (status, _, body) = send(&state, request).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_eq!(json_body(&body)["error"], "unauthenticated");
    }
}

#[tokio::test]
#[ignore = "requires compose.test.yaml and TEST_DATABASE_URL"]
async fn knowledge_web_api() {
    let db = database().await;
    let (manager, member) = (971_001_u64, 971_002_u64);
    let guild = 971_101_u64;
    let (managers, members) = (971_201_u64, 971_202_u64);
    for user in [manager, member] {
        clear_user(&db, user).await;
    }
    clear_guilds(&db, &[guild]).await;
    sqlx::query("DELETE FROM kb_documents WHERE guild_id=?")
        .bind(guild)
        .execute(&db.pool)
        .await
        .unwrap();
    db.allow_guild(guild, None).await.unwrap();
    use discord_discussion_bot::access::RoleKind;
    db.add_guild_role(guild, RoleKind::Use, members, None)
        .await
        .unwrap();
    db.add_guild_role(guild, RoleKind::Manage, managers, None)
        .await
        .unwrap();

    let discord = MockServer::start().await;
    // The manager is looked up again by the second state (knowledge base disabled).
    mock_member(&discord, guild, manager, &[managers], 2).await;
    mock_member(&discord, guild, member, &[members], 1).await;
    let roles = [
        role(managers, "資料係", 2, 0),
        role(members, "メンバー", 1, 0),
    ];
    mock_guild(&discord, guild, 1, &roles, 2).await;
    let limit = 65_536;
    let state = knowledge_state(db.clone(), &discord.uri(), limit);
    state.bot_guilds.write().unwrap().insert(guild);
    let guilds = vec![SessionGuild {
        id: guild,
        name: "ナレッジ".into(),
    }];
    let mut cookies = Vec::new();
    for user in [manager, member] {
        let token = security::new_token();
        db.create_session(
            &security::hash(&token),
            user,
            "管理する人",
            &guilds,
            Utc::now(),
        )
        .await
        .unwrap();
        cookies.push(format!("__Host-session={}", security::encode(&token)));
    }
    let (manager_cookie, member_cookie) = (cookies[0].clone(), cookies[1].clone());
    let base = format!("/api/guilds/{guild}/kb/documents");
    let read = |cookie: &str, uri: &str| {
        Request::get(uri)
            .header(header::COOKIE, cookie)
            .body(Body::empty())
            .unwrap()
    };
    let write = |method: &str, cookie: &str, uri: &str| {
        Request::builder()
            .method(method)
            .uri(uri)
            .header(header::COOKIE, cookie)
            .header(header::ORIGIN, ORIGIN)
            .body(Body::empty())
            .unwrap()
    };
    let upload = |cookie: &str, name: Option<&str>, body: Vec<u8>| {
        let mut request = Request::post(&base)
            .header(header::COOKIE, cookie)
            .header(header::ORIGIN, ORIGIN)
            .header(header::CONTENT_TYPE, "application/octet-stream");
        if let Some(name) = name {
            request = request.header("x-file-name", name);
        }
        request.body(Body::from(body)).unwrap()
    };

    let (status, _, body) = send(&state, read(&manager_cookie, "/api/me")).await;
    assert_eq!(status, StatusCode::OK);
    let me = json_body(&body);
    assert_eq!(me["knowledge"], true);
    assert_eq!(
        me["guilds"][0]["access"],
        json!({"use": false, "manage_kb": true, "configure": false})
    );

    // Members who may only use the bot get nothing of the knowledge base.
    let (status, _, _) = send(&state, read(&member_cookie, &base)).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let request = upload(&member_cookie, Some("a.txt"), b"text".to_vec());
    assert_eq!(send(&state, request).await.0, StatusCode::FORBIDDEN);

    // Refused uploads.
    let text = "社内手順の資料です。".repeat(30);
    let mut too_large = Request::post(&base)
        .header(header::COOKIE, &manager_cookie)
        .header(header::ORIGIN, ORIGIN)
        .header("x-file-name", "big.txt")
        .header(header::CONTENT_LENGTH, limit + 1)
        .body(Body::from(vec![b'a'; limit + 1]))
        .unwrap();
    let (status, _, body) = send(&state, too_large).await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(json_body(&body)["error"], "payload_too_large");
    assert!(
        json_body(&body)["message"]
            .as_str()
            .unwrap()
            .contains("ファイルは")
    );
    // Without Content-Length the body limit of the route stops the read.
    too_large = upload(&manager_cookie, Some("big.txt"), vec![b'a'; limit + 1]);
    assert_eq!(
        send(&state, too_large).await.0,
        StatusCode::PAYLOAD_TOO_LARGE
    );
    for (name, body, expected_status, code) in [
        (
            None,
            text.clone().into_bytes(),
            StatusCode::BAD_REQUEST,
            "invalid_request",
        ),
        (
            Some("a.docx"),
            b"PK".to_vec(),
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "unsupported_type",
        ),
        (
            Some("fake.pdf"),
            b"text".to_vec(),
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "type_mismatch",
        ),
        (
            Some("sjis.txt"),
            vec![0x93, 0xfa],
            StatusCode::UNPROCESSABLE_ENTITY,
            "not_utf8",
        ),
        (
            Some("empty.txt"),
            Vec::new(),
            StatusCode::BAD_REQUEST,
            "invalid_request",
        ),
    ] {
        let (status, _, response) = send(&state, upload(&manager_cookie, name, body)).await;
        assert_eq!(status, expected_status, "{name:?}");
        assert_eq!(json_body(&response)["error"], code, "{name:?}");
    }
    assert_eq!(db.kb_usage(guild).await.unwrap().documents, 0);

    // メモ.txt, percent-encoded.
    let (status, _, body) = send(
        &state,
        upload(
            &manager_cookie,
            Some("%E3%83%A1%E3%83%A2.txt"),
            text.clone().into_bytes(),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    let document = json_body(&body);
    let id = document["id"].as_u64().unwrap();
    assert_eq!(document["title"], "メモ");
    assert_eq!(document["file_name"], "メモ.txt");
    assert_eq!(document["kind"], "text");
    assert_eq!(document["status"], "processing");
    assert_eq!(document["uploaded_by"], "管理する人");
    assert_eq!(
        document["progress"],
        json!([{"provider": "Gemini (web-test)", "embedded": 0}])
    );
    // Larger than the JSON API's 16 KiB: the upload route's own limit applies.
    let larger = "大きめの資料です。".repeat(2_000);
    assert!(larger.len() > 16 * 1024 && larger.len() < limit);
    let (status, _, body) = send(
        &state,
        upload(&manager_cookie, Some("larger.txt"), larger.into_bytes()),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{:?}", json_body(&body));
    let larger_id = json_body(&body)["id"].as_u64().unwrap();
    let request = write("DELETE", &manager_cookie, &format!("{base}/{larger_id}"));
    assert_eq!(send(&state, request).await.0, StatusCode::NO_CONTENT);
    let (status, _, body) = send(
        &state,
        upload(&manager_cookie, Some("copy.txt"), text.clone().into_bytes()),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(json_body(&body)["error"], "duplicate");
    assert!(
        json_body(&body)["message"]
            .as_str()
            .unwrap()
            .contains("メモ")
    );
    // While every upload slot is taken (files being received or extracted), further uploads
    // are turned away before their body is read.
    let knowledge = state.knowledge.clone().unwrap();
    let held: Vec<_> = (0..UPLOAD_SLOTS)
        .map(|_| knowledge.upload_slot().unwrap())
        .collect();
    let (status, _, body) = send(
        &state,
        upload(&manager_cookie, Some("busy.txt"), b"busy".to_vec()),
    )
    .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(json_body(&body)["error"], "upload_busy");
    drop(held);
    // A slot is given back however the upload ends.
    let (status, _, _) = send(
        &state,
        upload(&manager_cookie, Some("copy.txt"), text.clone().into_bytes()),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert!(knowledge.upload_slot().is_some());

    let (status, _, body) = send(&state, read(&manager_cookie, &base)).await;
    assert_eq!(status, StatusCode::OK);
    let list = json_body(&body);
    assert_eq!(list["documents"].as_array().unwrap().len(), 1);
    assert_eq!(list["documents"][0]["id"], id);
    assert_eq!(list["usage"]["documents"], 1);
    assert_eq!(list["usage"]["max_documents"], 50);
    assert_eq!(list["limits"]["max_upload_bytes"], limit);
    let mut extensions = vec![".txt", ".text", ".md", ".markdown"];
    if cfg!(feature = "pdf") {
        extensions.push(".pdf");
    }
    assert_eq!(list["limits"]["extensions"], json!(extensions));
    assert_eq!(list["limits"]["max_pdf_bytes"], limit);
    assert_eq!(list["limits"]["max_pdf_pages"], 300);
    assert_eq!(list["providers"][0]["key"], "gemini:web-test");
    assert_eq!(list["providers"][0]["state"], "ok");

    let (status, _, body) = send(
        &state,
        read(&manager_cookie, &format!("{base}/{id}/preview")),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let preview = json_body(&body);
    assert_eq!(preview["text"], text);
    assert_eq!(preview["truncated"], false);

    // Only failed documents can be retried.
    let (status, _, body) = send(
        &state,
        write("POST", &manager_cookie, &format!("{base}/{id}/retry")),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(json_body(&body)["error"], "not_failed");
    sqlx::query("UPDATE kb_documents SET status='failed',attempts=5,error_code='embedding_upstream' WHERE id=?")
        .bind(id)
        .execute(&db.pool)
        .await
        .unwrap();
    let (_, _, body) = send(&state, read(&manager_cookie, &base)).await;
    let failed = &json_body(&body)["documents"][0];
    assert_eq!(failed["status"], "failed");
    assert!(failed["error"].as_str().unwrap().contains("再試行"));
    let (status, _, body) = send(
        &state,
        write("POST", &manager_cookie, &format!("{base}/{id}/retry")),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(json_body(&body)["status"], "processing");
    assert_eq!(json_body(&body)["attempts"], 0);

    // Deleting.
    let request = Request::delete(format!("{base}/{id}"))
        .header(header::COOKIE, &manager_cookie)
        .body(Body::empty())
        .unwrap();
    assert_eq!(send(&state, request).await.0, StatusCode::FORBIDDEN);
    let request = write("DELETE", &member_cookie, &format!("{base}/{id}"));
    assert_eq!(send(&state, request).await.0, StatusCode::FORBIDDEN);
    let request = write("DELETE", &manager_cookie, &format!("{base}/{id}"));
    assert_eq!(send(&state, request).await.0, StatusCode::NO_CONTENT);
    for request in [
        write("DELETE", &manager_cookie, &format!("{base}/{id}")),
        read(&manager_cookie, &format!("{base}/{id}/preview")),
        write("POST", &manager_cookie, &format!("{base}/{id}/retry")),
        read(&manager_cookie, &format!("{base}/abc/preview")),
    ] {
        assert_eq!(send(&state, request).await.0, StatusCode::NOT_FOUND);
    }

    // The upload route's larger limit does not apply to the JSON API.
    let big = json!({"use": vec!["1"; 5_000], "manage": []});
    let request = Request::put(format!("/api/guilds/{guild}/config/roles"))
        .header(header::COOKIE, &manager_cookie)
        .header(header::ORIGIN, ORIGIN)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(big.to_string()))
        .unwrap();
    assert_eq!(send(&state, request).await.0, StatusCode::PAYLOAD_TOO_LARGE);

    // With the knowledge base disabled its API is 404 and /api/me does not offer it.
    let disabled = self::state(db.clone(), &discord.uri());
    disabled.bot_guilds.write().unwrap().insert(guild);
    assert_eq!(
        send(&disabled, read(&manager_cookie, &base)).await.0,
        StatusCode::NOT_FOUND
    );
    let (_, _, body) = send(&disabled, read(&manager_cookie, "/api/me")).await;
    assert!(json_body(&body).get("knowledge").is_none());
    for user in [manager, member] {
        clear_user(&db, user).await;
    }
    clear_guilds(&db, &[guild]).await;
}

#[tokio::test]
async fn privacy_api_needs_a_session_and_same_origin() {
    let state = state(offline_database(), "http://127.0.0.1:9");
    let (status, _, body) = send(&state, get("/api/privacy")).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(json_body(&body)["error"], "unauthenticated");
    let delete = |origin: Option<&str>| {
        let mut request =
            Request::post("/api/privacy/delete").header(header::CONTENT_TYPE, "application/json");
        if let Some(origin) = origin {
            request = request.header(header::ORIGIN, origin);
        }
        request.body(Body::from(r#"{"confirm":"DELETE"}"#)).unwrap()
    };
    for origin in [None, Some("https://evil.example")] {
        let (status, _, body) = send(&state, delete(origin)).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{origin:?}");
        assert_eq!(json_body(&body)["error"], "cross_origin");
    }
    let (status, headers, body) = send(&state, delete(Some(ORIGIN))).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(json_body(&body)["error"], "unauthenticated");
    assert!(set_cookies(&headers).is_empty());
}

/// Rows of the privacy test, removed before and after it so that it can run again.
async fn clear_privacy_test(db: &Database, users: &[u64], guilds: &[u64]) {
    for user in users {
        for statement in [
            "DELETE FROM talk_runs WHERE user_id=?",
            "DELETE FROM web_conversations WHERE user_id=?",
            "DELETE FROM web_sessions WHERE user_id=?",
            "DELETE FROM privacy_erasures WHERE user_id=?",
        ] {
            sqlx::query(statement)
                .bind(user)
                .execute(&db.pool)
                .await
                .unwrap();
        }
    }
    for guild in guilds {
        for statement in [
            "DELETE FROM talk_runs WHERE guild_id=?",
            "DELETE FROM web_conversations WHERE guild_id=?",
            "DELETE FROM kb_documents WHERE guild_id=?",
        ] {
            sqlx::query(statement)
                .bind(guild)
                .execute(&db.pool)
                .await
                .unwrap();
        }
    }
    clear_guilds(db, guilds).await;
}

async fn count(db: &Database, sql: &str, id: u64) -> i64 {
    sqlx::query_scalar(sql)
        .bind(id)
        .fetch_one(&db.pool)
        .await
        .unwrap()
}

async fn add_talk_run(db: &Database, id: u64, guild: u64, user: u64, finished: bool) {
    add_talk_run_at(db, id, guild, user, finished, Utc::now()).await;
}

async fn add_talk_run_at(
    db: &Database,
    id: u64,
    guild: u64,
    user: u64,
    finished: bool,
    now: chrono::DateTime<Utc>,
) {
    assert!(
        db.begin(&NewRun {
            interaction_id: id,
            guild_id: guild,
            channel_id: guild + 1,
            user_id: user,
            user_name: "テスト",
            question: "質問",
            web_search: false,
            history_seconds: 0,
            invoked_at: now,
        })
        .await
        .unwrap()
    );
    if finished {
        db.reply(id, id + 50, 0, "回答", now).await.unwrap();
        db.finish(id, None).await.unwrap();
    }
}

async fn add_conversation(db: &Database, guild: u64, user: u64) -> u64 {
    add_conversation_at(db, guild, user, Utc::now()).await
}

async fn add_conversation_at(
    db: &Database,
    guild: u64,
    user: u64,
    at: chrono::DateTime<Utc>,
) -> u64 {
    let id = sqlx::query("INSERT INTO web_conversations (user_id,guild_id,title,created_at,updated_at) VALUES (?,?,'会話',?,?)")
        .bind(user).bind(guild).bind(at.naive_utc()).bind(at.naive_utc())
        .execute(&db.pool).await.unwrap().last_insert_id();
    sqlx::query("INSERT INTO web_messages (conversation_id,role,content,created_at) VALUES (?,'user','こんにちは',UTC_TIMESTAMP(3))")
        .bind(id).execute(&db.pool).await.unwrap();
    id
}

async fn add_document(db: &Database, guild: u64, user: u64, name: &str) -> u64 {
    let id = sqlx::query("INSERT INTO kb_documents (guild_id,title,file_name,media_type,byte_size,sha256,content,char_count,chunk_count,status,uploaded_by,uploaded_by_name,created_at,updated_at) VALUES (?,?,'a.md','text/markdown',3,UNHEX(SHA2(?,256)),'本文',2,1,'ready',?,?,UTC_TIMESTAMP(3),UTC_TIMESTAMP(3))")
        .bind(guild).bind(name).bind(format!("{guild}-{name}")).bind(user).bind(name)
        .execute(&db.pool).await.unwrap().last_insert_id();
    sqlx::query("INSERT INTO kb_chunks (document_id,guild_id,seq,content) VALUES (?,?,0,'本文')")
        .bind(id)
        .bind(guild)
        .execute(&db.pool)
        .await
        .unwrap();
    id
}

async fn login(db: &Database, user: u64) -> String {
    login_at(db, user, Utc::now()).await
}

async fn login_at(db: &Database, user: u64, at: chrono::DateTime<Utc>) -> String {
    let token = security::new_token();
    db.create_session(&security::hash(&token), user, "テスト", &[], at)
        .await
        .unwrap();
    format!("__Host-session={}", security::encode(&token))
}

#[tokio::test]
#[ignore = "requires compose.test.yaml and TEST_DATABASE_URL"]
async fn privacy_and_guild_purge() {
    let db = database().await;
    let (user, other) = (975_001_u64, 975_002_u64);
    // `kept` stays in use; `left` is purged after the grace period; `away` leaves and returns;
    // `unknown` has no guilds row until the bot leaves it.
    let (kept, left, away, unknown) = (975_100_u64, 975_200_u64, 975_300_u64, 975_400_u64);
    let guilds = [kept, left, away, unknown];
    clear_privacy_test(&db, &[user, other], &guilds).await;
    for guild in [kept, left, away] {
        db.allow_guild(guild, Some("テスト")).await.unwrap();
    }

    add_talk_run(&db, 975_011, kept, user, true).await;
    // Still being answered: kept by the erasure.
    add_talk_run(&db, 975_012, kept, user, false).await;
    add_talk_run(&db, 975_013, kept, other, true).await;
    add_talk_run(&db, 975_014, left, other, true).await;
    let mine = add_conversation(&db, kept, user).await;
    let theirs = add_conversation(&db, kept, other).await;
    add_conversation(&db, left, other).await;
    let cookie = login(&db, user).await;
    login(&db, user).await;
    login(&db, other).await;
    let my_document = add_document(&db, kept, user, "mine").await;
    let their_document = add_document(&db, kept, other, "theirs").await;
    let left_document = add_document(&db, left, other, "left").await;
    db.add_guild_role(kept, RoleKind::Use, 975_501, Some(user))
        .await
        .unwrap();
    db.add_guild_role(kept, RoleKind::Manage, 975_502, Some(other))
        .await
        .unwrap();
    db.add_guild_role(left, RoleKind::Use, 975_503, Some(other))
        .await
        .unwrap();

    // The web page: counts, a confirmation, then erasure with the cookie cleared.
    let state = state(db.clone(), "http://127.0.0.1:9");
    let (status, _, body) = send(
        &state,
        Request::get("/api/privacy")
            .header(header::COOKIE, &cookie)
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        json_body(&body),
        json!({"talk_runs": 2, "web_conversations": 1, "web_sessions": 2, "kb_documents": 1, "guild_roles": 1})
    );
    let delete = |confirm: &str| {
        Request::post("/api/privacy/delete")
            .header(header::ORIGIN, ORIGIN)
            .header(header::CONTENT_TYPE, "application/json")
            .header(header::COOKIE, &cookie)
            .body(Body::from(json!({ "confirm": confirm }).to_string()))
            .unwrap()
    };
    let (status, _, body) = send(&state, delete("yes")).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(json_body(&body)["error"], "invalid_request");
    assert_eq!(
        count(
            &db,
            "SELECT COUNT(*) FROM web_sessions WHERE user_id=?",
            user
        )
        .await,
        2
    );
    let (status, headers, body) = send(&state, delete("DELETE")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        json_body(&body),
        json!({"talk_runs": 1, "talk_runs_in_progress": 1, "web_conversations": 1, "web_sessions": 2, "kb_documents": 1, "guild_roles": 1})
    );
    assert_eq!(
        set_cookies(&headers),
        ["__Host-session=; Max-Age=0; Path=/; HttpOnly; SameSite=Lax; Secure"]
    );
    let (status, _, _) = send(
        &state,
        Request::get("/api/privacy")
            .header(header::COOKIE, &cookie)
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    // Deleted: the finished run with its reply, the conversation with its messages, sessions.
    let rows = |sql: &'static str, id: u64| count(&db, sql, id);
    assert_eq!(
        rows(
            "SELECT COUNT(*) FROM talk_runs WHERE interaction_id=?",
            975_011
        )
        .await,
        0
    );
    assert_eq!(
        rows(
            "SELECT COUNT(*) FROM talk_replies WHERE interaction_id=?",
            975_011
        )
        .await,
        0
    );
    assert_eq!(
        rows(
            "SELECT COUNT(*) FROM talk_runs WHERE interaction_id=?",
            975_012
        )
        .await,
        1
    );
    assert_eq!(
        rows(
            "SELECT COUNT(*) FROM web_messages WHERE conversation_id=?",
            mine
        )
        .await,
        0
    );
    // Anonymized, not deleted: the guild's document and role setting.
    let row = sqlx::query("SELECT uploaded_by,uploaded_by_name FROM kb_documents WHERE id=?")
        .bind(my_document)
        .fetch_one(&db.pool)
        .await
        .unwrap();
    assert_eq!(row.get::<Option<u64>, _>("uploaded_by"), None);
    assert_eq!(row.get::<Option<String>, _>("uploaded_by_name"), None);
    let creator: Option<u64> =
        sqlx::query_scalar("SELECT created_by FROM guild_roles WHERE guild_id=? AND role_id=?")
            .bind(kept)
            .bind(975_501_u64)
            .fetch_one(&db.pool)
            .await
            .unwrap();
    assert_eq!(creator, None);
    // Other users' data is untouched.
    assert_eq!(
        db.privacy_holdings(other).await.unwrap(),
        privacy::Holdings {
            talk_runs: 2,
            web_conversations: 2,
            web_sessions: 1,
            kb_documents: 2,
            guild_roles: 2,
        }
    );
    assert_eq!(
        rows(
            "SELECT COUNT(*) FROM web_messages WHERE conversation_id=?",
            theirs
        )
        .await,
        1
    );
    let uploader: Option<u64> =
        sqlx::query_scalar("SELECT uploaded_by FROM kb_documents WHERE id=?")
            .bind(their_document)
            .fetch_one(&db.pool)
            .await
            .unwrap();
    assert_eq!(uploader, Some(other));

    // Erasing again is harmless; the run being answered is still reported as kept.
    let again = privacy::erase_user(&db, None, user).await.unwrap();
    assert_eq!(
        again,
        privacy::Erasure {
            talk_runs_in_progress: 1,
            ..Default::default()
        }
    );

    // The ledger survives a restore: data from before the erasure that comes back is erased
    // again, what the user created since stays, and the original erasure time is kept.
    let erased_at = db
        .erasure_ledger()
        .await
        .unwrap()
        .into_iter()
        .find(|(id, _)| *id == user)
        .expect("the erasure is in the ledger")
        .1;
    let text = privacy::format_ledger(&[(user, erased_at)]);
    sqlx::query("DELETE FROM privacy_erasures WHERE user_id=?")
        .bind(user)
        .execute(&db.pool)
        .await
        .unwrap();
    let restored = erased_at - chrono::Duration::minutes(1);
    add_talk_run_at(&db, 975_015, kept, user, true, restored).await;
    add_conversation_at(&db, kept, user, restored).await;
    login_at(&db, user, restored).await;
    add_talk_run(&db, 975_016, kept, user, true).await;
    let since = add_conversation(&db, kept, user).await;
    // The backup also caught a run of the user mid-answer, long before the erasure.
    add_talk_run_at(
        &db,
        975_017,
        kept,
        user,
        false,
        erased_at - chrono::Duration::hours(2),
    )
    .await;
    let restored_document = add_document(&db, kept, other, "restored").await;
    // A document line older than the document: after a restore the id can be reused by a
    // later upload, which must survive.
    let reused_id = add_document(&db, kept, other, "reused id").await;
    let mut entries = privacy::parse_ledger(&text).unwrap();
    let today = Utc::now().format("%Y-%m-%d");
    entries.extend(
        privacy::parse_ledger(&format!(
            "{today} kb_document={restored_document}\n2026-09-01 kb_document={reused_id}"
        ))
        .unwrap(),
    );
    let applied = privacy::apply_ledger(&db, &entries).await.unwrap();
    assert_eq!(
        applied,
        privacy::Applied {
            users: 1,
            rows: 4,
            kb_documents: 1
        }
    );
    assert_eq!(
        rows("SELECT COUNT(*) FROM kb_documents WHERE id=?", reused_id).await,
        1
    );
    assert_eq!(
        rows(
            "SELECT COUNT(*) FROM talk_runs WHERE interaction_id=?",
            975_017
        )
        .await,
        0
    );
    let holdings = db.privacy_holdings(user).await.unwrap();
    assert_eq!(
        (
            holdings.talk_runs,
            holdings.web_conversations,
            holdings.web_sessions
        ),
        (2, 1, 0)
    );
    assert_eq!(
        rows(
            "SELECT COUNT(*) FROM talk_runs WHERE interaction_id=?",
            975_016
        )
        .await,
        1
    );
    assert_eq!(
        rows(
            "SELECT COUNT(*) FROM web_messages WHERE conversation_id=?",
            since
        )
        .await,
        1
    );
    assert!(
        db.erasure_ledger()
            .await
            .unwrap()
            .contains(&(user, erased_at))
    );
    assert_eq!(
        privacy::apply_ledger(&db, &entries).await.unwrap(),
        privacy::Applied {
            users: 1,
            rows: 0,
            kb_documents: 0
        }
    );
    // Ledger entries go after 40 days.
    sqlx::query("UPDATE privacy_erasures SET erased_at=? WHERE user_id=?")
        .bind((Utc::now() - chrono::Duration::days(41)).naive_utc())
        .bind(user)
        .execute(&db.pool)
        .await
        .unwrap();
    assert!(
        db.purge_erasures(Utc::now() - chrono::Duration::days(privacy::LEDGER_DAYS))
            .await
            .unwrap()
            >= 1
    );
    assert!(
        !db.erasure_ledger()
            .await
            .unwrap()
            .iter()
            .any(|(id, _)| *id == user)
    );

    // Leaving and returning. The first departure time is kept.
    let left_at = |guild: u64| {
        let db = db.clone();
        async move {
            db.guilds()
                .await
                .unwrap()
                .into_iter()
                .find(|row| row.guild_id == guild)
                .map(|row| row.left_at)
        }
    };
    db.mark_guild_left(away).await.unwrap();
    let first = left_at(away).await.unwrap().expect("left_at set");
    tokio::time::sleep(Duration::from_millis(20)).await;
    db.mark_guild_left(away).await.unwrap();
    assert_eq!(left_at(away).await.unwrap(), Some(first));
    assert!(db.mark_guild_present(away).await.unwrap());
    assert!(!db.mark_guild_present(away).await.unwrap());
    assert_eq!(left_at(away).await.unwrap(), None);
    // A guild without a row gets one, not allowed.
    db.mark_guild_left(unknown).await.unwrap();
    let row = db
        .guilds()
        .await
        .unwrap()
        .into_iter()
        .find(|row| row.guild_id == unknown)
        .unwrap();
    assert!(row.left_at.is_some() && !row.allowed());
    // READY without `away`: recorded as left, but only by the shard that owns it. Every other
    // guild of the test database counts as present, so other tests' rows stay untouched.
    let mut present: std::collections::HashSet<u64> = db
        .guilds()
        .await
        .unwrap()
        .into_iter()
        .map(|row| row.guild_id)
        .collect();
    present.remove(&away);
    // Allowlisted before the bot was ever invited: not counted as left.
    let never_seen = 975_500_u64;
    sqlx::query("DELETE FROM guilds WHERE guild_id=?")
        .bind(never_seen)
        .execute(&db.pool)
        .await
        .unwrap();
    db.allow_guild(never_seen, None).await.unwrap();
    let other_shard = (privacy::shard_of(away, 2) + 1) % 2;
    assert!(
        db.reconcile_guilds(&present, (other_shard, 2))
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(db.reconcile_guilds(&present, (0, 1)).await.unwrap(), [away]);
    assert_eq!(left_at(never_seen).await.unwrap(), None);
    assert!(left_at(away).await.unwrap().is_some());
    assert!(db.mark_guild_present(away).await.unwrap());

    // The grace period: only guilds left before the cutoff with data are purged.
    db.mark_guild_left(left).await.unwrap();
    sqlx::query("UPDATE guilds SET left_at=? WHERE guild_id=?")
        .bind((Utc::now() - chrono::Duration::days(15)).naive_utc())
        .bind(left)
        .execute(&db.pool)
        .await
        .unwrap();
    let cutoff = Utc::now() - chrono::Duration::days(14);
    let due = db.guilds_to_purge(cutoff).await.unwrap();
    assert!(due.contains(&left));
    assert!(!due.contains(&kept) && !due.contains(&away) && !due.contains(&unknown));
    // Only a guild that left before the cutoff is purged.
    assert_eq!(db.purge_guild(kept, Some(cutoff)).await.unwrap(), None);
    assert_eq!(
        db.purge_guild(left, Some(cutoff)).await.unwrap(),
        Some(privacy::GuildPurge {
            talk_runs: 1,
            web_conversations: 1,
            kb_documents: 1,
            guild_roles: 1,
        })
    );
    assert_eq!(
        rows(
            "SELECT COUNT(*) FROM kb_chunks WHERE document_id=?",
            left_document
        )
        .await,
        0
    );
    let row = db
        .guilds()
        .await
        .unwrap()
        .into_iter()
        .find(|row| row.guild_id == left)
        .unwrap();
    assert!(!row.allowed() && row.left_at.is_some());
    assert!(row.note.unwrap().starts_with("purged "));
    assert!(!db.guild_access(left).await.unwrap().allowed);
    assert!(!db.guilds_to_purge(cutoff).await.unwrap().contains(&left));
    // The guild that stayed keeps everything.
    assert_eq!(
        rows("SELECT COUNT(*) FROM talk_runs WHERE guild_id=?", kept).await,
        3
    );
    assert_eq!(
        rows("SELECT COUNT(*) FROM kb_documents WHERE guild_id=?", kept).await,
        3
    );
    assert!(db.guild_access(kept).await.unwrap().allowed);

    clear_privacy_test(&db, &[user, other], &guilds).await;
}
