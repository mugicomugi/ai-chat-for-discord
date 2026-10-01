//! Web UI tests. Without a database: security headers, the CSRF guard, the login redirect and
//! state check, the session requirement, the Discord OAuth2 client and the cached Discord REST
//! lookups (wiremock). The `#[ignore]` tests need compose.test.yaml and TEST_DATABASE_URL.

use std::{
    io::{Read, Write},
    net::TcpListener,
    sync::{Arc, atomic::AtomicBool},
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
    access::GuildAccess,
    agent::Agent,
    config::WebConfig,
    db::{Database, MAX_SESSIONS_PER_USER, SessionGuild},
    limits::Limits,
    web::{
        self, AppState, JsonBody, Shared, Web,
        auth::{OAuthClient, OAuthError},
        authz::{self, Authz, DiscordCache, Unavailable},
        security::{self, CSP},
    },
};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use serenity::http::HttpBuilder;
use sqlx::{ConnectOptions, Row, mysql::MySqlConnectOptions, mysql::MySqlPoolOptions};
use tower::ServiceExt;
use url::Url;
use wiremock::{
    Mock, MockServer, Request as MockRequest, ResponseTemplate,
    matchers::{body_string_contains, header as mock_header, method, path, query_param},
};

const ORIGIN: &str = "https://bot.example";
const ADMINISTRATOR: u64 = 1 << 3;
const MANAGE_GUILD: u64 = 1 << 5;

fn config(discord: &str) -> WebConfig {
    WebConfig {
        client_id: "4242".into(),
        client_secret: "test-client-secret".into(),
        public_origin: ORIGIN.into(),
        bind: "127.0.0.1:0".parse().unwrap(),
        discord_api: format!("{discord}/api/v10"),
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
        assert_eq!(headers[header::CONTENT_SECURITY_POLICY], CSP, "{uri}");
        assert!(CSP.contains("frame-ancestors 'none'") && !CSP.contains("unsafe"));
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
        assert_eq!(headers[header::CONTENT_SECURITY_POLICY], CSP);
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
        assert_eq!(headers[header::CONTENT_SECURITY_POLICY], CSP);
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
    assert_eq!(cache.len(), 3);
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

async fn database() -> Database {
    let url = std::env::var("TEST_DATABASE_URL")
        .expect("set TEST_DATABASE_URL to the disposable discussion_test database");
    let options: MySqlConnectOptions = url.parse().unwrap();
    assert_eq!(
        options.get_database(),
        Some("discussion_test"),
        "never use a production database for these tests"
    );
    let pool = Database::pool_options()
        .max_connections(3)
        .connect_with(options.disable_statement_logging())
        .await
        .unwrap();
    let db = Database { pool };
    db.migrate().await.unwrap();
    db
}

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
    assert_eq!(
        session.expires_at.timestamp_millis(),
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
    // Neither absent guild (bot not in it) nor anything else may reach Discord.
    Mock::given(path(format!("/api/v10/guilds/{absent}")))
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
    for user in [manager, member] {
        clear_user(&db, user).await;
    }
}
