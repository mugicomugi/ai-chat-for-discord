//! Web chat tests. Without a database: the routes need a session and same-origin writes. The
//! `#[ignore]` tests need compose.test.yaml and TEST_DATABASE_URL, and run one at a time
//! (`--exact`): `web_chat_retention_and_recovery` marks every streaming answer interrupted, as
//! the bot does at startup.

use std::{sync::Arc, sync::atomic::AtomicBool, time::Duration};

use axum::{
    body::{Body, Bytes},
    http::{HeaderMap, Request, StatusCode, header},
};
use chrono::Utc;
use discord_discussion_bot::{
    access::RoleKind,
    agent::Agent,
    config::{KbConfig, ProviderConfig, ProviderKind, WebConfig},
    db::{Database, SessionGuild},
    knowledge::{
        Knowledge,
        consult::{KNOWLEDGE_EMPTY, KNOWLEDGE_FAILED},
    },
    limits::{Key, Limits},
    web::{
        self, AppState, Shared, Web,
        chat_store::{NewTurn, Posted, Status},
        security,
    },
};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use serenity::http::HttpBuilder;
use sqlx::mysql::MySqlPoolOptions;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tower::ServiceExt;
use wiremock::{
    Mock, MockServer, Request as MockRequest, ResponseTemplate,
    matchers::{method, path},
};

mod common;
use common::database;

const ORIGIN: &str = "https://bot.example";

fn config(discord: &str, daily_messages: u32) -> WebConfig {
    WebConfig {
        client_id: "4242".into(),
        client_secret: "test-client-secret".into(),
        public_origin: ORIGIN.into(),
        bind: "127.0.0.1:0".parse().unwrap(),
        discord_api: format!("{discord}/api/v10"),
        daily_messages,
        request_timeout: Duration::from_secs(30),
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

/// The web state with Discord at `discord` and Ollama at `ollama`; with `knowledge`, a
/// knowledge base whose only provider cannot be reached.
fn state(
    db: Database,
    discord: &str,
    ollama: &str,
    daily_messages: u32,
    knowledge: bool,
) -> AppState {
    let http = HttpBuilder::new("bot-token")
        .proxy(discord)
        .ratelimiter_disabled(true)
        .build();
    let knowledge = knowledge.then(|| {
        Arc::new(
            Knowledge::new(
                KbConfig {
                    providers: vec![ProviderConfig::new(
                        ProviderKind::Gemini,
                        "chat-test",
                        "unused",
                        "http://127.0.0.1:9",
                    )],
                    max_upload_bytes: 65_536,
                    max_docs_per_guild: 50,
                    max_chunks_per_guild: 5_000,
                    max_chunks_total: 10_000_000,
                },
                db.clone(),
            )
            .unwrap(),
        )
    });
    Web::new(
        config(discord, daily_messages),
        Shared {
            db,
            agent: Agent::new(ollama, "test_key".into(), "gpt-oss:120b".into()).unwrap(),
            limits: Arc::new(Limits::new(4)),
            http: Arc::new(http),
            bot_guilds: Default::default(),
            discord_ready: Arc::new(AtomicBool::new(true)),
            discord_cache: Default::default(),
            knowledge,
        },
    )
    .unwrap()
}

async fn send(state: &AppState, request: Request<Body>) -> (StatusCode, HeaderMap, Bytes) {
    let response = web::router(state.clone()).oneshot(request).await.unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let body = tokio::time::timeout(Duration::from_secs(30), response.into_body().collect())
        .await
        .expect("the response ends")
        .unwrap()
        .to_bytes();
    (status, headers, body)
}

fn json_body(body: &Bytes) -> Value {
    serde_json::from_slice(body).unwrap()
}

fn read(cookie: &str, uri: &str) -> Request<Body> {
    Request::get(uri)
        .header(header::COOKIE, cookie)
        .body(Body::empty())
        .unwrap()
}

fn write(method: &str, cookie: &str, uri: &str, body: Option<Value>) -> Request<Body> {
    let request = Request::builder()
        .method(method)
        .uri(uri)
        .header(header::COOKIE, cookie)
        .header(header::ORIGIN, ORIGIN);
    match body {
        Some(body) => request
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body.to_string()))
            .unwrap(),
        None => request.body(Body::empty()).unwrap(),
    }
}

fn message(cookie: &str, conversation: u64, body: Value) -> Request<Body> {
    write(
        "POST",
        cookie,
        &format!("/api/conversations/{conversation}/messages"),
        Some(body),
    )
}

/// The events of a complete server-sent event stream, as (name, data).
fn events(body: &[u8]) -> Vec<(String, Value)> {
    std::str::from_utf8(body)
        .unwrap()
        .split("\n\n")
        .filter_map(parse_event)
        .collect()
}

fn parse_event(block: &str) -> Option<(String, Value)> {
    let mut name = None;
    let mut data = String::new();
    for line in block.lines() {
        if let Some(value) = line.strip_prefix("event:") {
            name = Some(value.trim().to_owned());
        } else if let Some(value) = line.strip_prefix("data:") {
            data.push_str(value.trim_start());
        }
    }
    Some((name?, serde_json::from_str(&data).unwrap()))
}

/// Reads server-sent events as they arrive.
struct EventReader {
    body: Body,
    buffer: String,
}

impl EventReader {
    async fn open(state: &AppState, request: Request<Body>) -> Self {
        let response = web::router(state.clone()).oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers()[header::CONTENT_TYPE],
            "text/event-stream"
        );
        Self {
            body: response.into_body(),
            buffer: String::new(),
        }
    }

    /// The next event; `None` when the stream ends.
    async fn next(&mut self) -> Option<(String, Value)> {
        loop {
            if let Some(end) = self.buffer.find("\n\n") {
                let block: String = self.buffer.drain(..end + 2).collect();
                match parse_event(&block) {
                    Some(event) => return Some(event),
                    // A keep-alive comment.
                    None => continue,
                }
            }
            let frame = tokio::time::timeout(Duration::from_secs(20), self.body.frame())
                .await
                .expect("an event in time")?
                .unwrap();
            if let Ok(data) = frame.into_data() {
                self.buffer.push_str(std::str::from_utf8(&data).unwrap());
            }
        }
    }
}

#[tokio::test]
async fn chat_routes_need_a_session_and_same_origin_writes() {
    let state = state(
        offline_database(),
        "http://127.0.0.1:9",
        "http://127.0.0.1:9",
        100,
        false,
    );
    let writes = [
        ("POST", "/api/conversations?guild=1", None),
        ("PATCH", "/api/conversations/1", Some(json!({"title": "x"}))),
        ("DELETE", "/api/conversations/1", None),
        (
            "POST",
            "/api/conversations/1/messages",
            Some(json!({"content": "x"})),
        ),
        ("POST", "/api/chat/stop", None),
    ];
    for (method, uri, body) in &writes {
        for origin in [None, Some("https://evil.example"), Some("null")] {
            let mut request = Request::builder().method(*method).uri(*uri);
            if let Some(origin) = origin {
                request = request.header(header::ORIGIN, origin);
            }
            let request = match body {
                Some(body) => request
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(body.to_string())),
                None => request.body(Body::empty()),
            }
            .unwrap();
            let (status, _, response) = send(&state, request).await;
            assert_eq!(status, StatusCode::FORBIDDEN, "{method} {uri} {origin:?}");
            assert_eq!(json_body(&response)["error"], "cross_origin");
        }
        // Same origin, but nobody logged in.
        let (status, headers, response) =
            send(&state, write(method, "a=b", uri, body.clone())).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{method} {uri}");
        assert_eq!(json_body(&response)["error"], "unauthenticated");
        assert_eq!(headers[header::CACHE_CONTROL], "no-store");
    }
    for uri in ["/api/conversations?guild=1", "/api/conversations/1"] {
        let (status, _, _) = send(&state, read("a=b", uri)).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{uri}");
    }
}

// ---- Database tests (compose.test.yaml) ----

const ROLE: u64 = 941_201;

async fn clear(db: &Database, users: &[u64], guilds: &[u64]) {
    for user in users {
        for table in ["web_conversations", "web_sessions"] {
            sqlx::query(&format!("DELETE FROM {table} WHERE user_id=?"))
                .bind(user)
                .execute(&db.pool)
                .await
                .unwrap();
        }
    }
    for guild in guilds {
        for table in ["guild_roles", "guilds", "kb_documents"] {
            sqlx::query(&format!("DELETE FROM {table} WHERE guild_id=?"))
                .bind(guild)
                .execute(&db.pool)
                .await
                .unwrap();
        }
    }
}

/// A Discord role without permissions.
fn role(id: u64, name: &str, position: u16) -> Value {
    json!({
        "id": id.to_string(), "name": name, "color": 0,
        "colors": {"primary_color": 0, "secondary_color": null, "tertiary_color": null},
        "hoist": false, "icon": null, "unicode_emoji": null, "position": position,
        "permissions": "0", "managed": false, "mentionable": false, "flags": 0
    })
}

/// An allowlisted guild whose `ROLE` may use the bot, with Discord answering for `users` (all
/// holding the role).
async fn discord(db: &Database, guild: u64, users: &[u64]) -> MockServer {
    db.allow_guild(guild, None).await.unwrap();
    db.add_guild_role(guild, RoleKind::Use, ROLE, None)
        .await
        .unwrap();
    let server = MockServer::start().await;
    for user in users {
        Mock::given(method("GET"))
            .and(path(format!("/api/v10/guilds/{guild}/members/{user}")))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "user": {"id": user.to_string(), "username": "member", "discriminator": "0",
                         "global_name": null, "avatar": null},
                "nick": null, "avatar": null, "roles": [ROLE.to_string()],
                "joined_at": "2024-01-01T00:00:00.000000+00:00", "premium_since": null,
                "deaf": false, "mute": false, "flags": 0, "pending": false,
                "communication_disabled_until": null
            })))
            .mount(&server)
            .await;
    }
    Mock::given(method("GET"))
        .and(path(format!("/api/v10/guilds/{guild}")))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": guild.to_string(), "name": "チャット", "icon": null, "icon_hash": null,
            "splash": null, "discovery_splash": null, "owner_id": "1",
            "afk_channel_id": null, "afk_timeout": 300, "widget_enabled": false,
            "widget_channel_id": null, "verification_level": 0,
            "default_message_notifications": 0, "explicit_content_filter": 0,
            "roles": [role(guild, "@everyone", 0), role(ROLE, "メンバー", 1)],
            "emojis": [], "features": [], "mfa_level": 0, "application_id": null,
            "system_channel_id": null, "system_channel_flags": 0, "rules_channel_id": null,
            "max_presences": null, "max_members": 500000, "vanity_url_code": null,
            "description": null, "banner": null, "premium_tier": 0,
            "premium_subscription_count": 0, "preferred_locale": "ja",
            "public_updates_channel_id": null, "max_video_channel_users": 25, "nsfw_level": 0,
            "stickers": [], "premium_progress_bar_enabled": false,
            "safety_alerts_channel_id": null
        })))
        .mount(&server)
        .await;
    server
}

async fn login(db: &Database, user: u64, guild: u64) -> String {
    let token = security::new_token();
    let guilds = [SessionGuild {
        id: guild,
        name: "チャット".into(),
    }];
    db.create_session(&security::hash(&token), user, "user", &guilds, Utc::now())
        .await
        .unwrap();
    format!("__Host-session={}", security::encode(&token))
}

fn ndjson(lines: &[Value]) -> ResponseTemplate {
    let body: String = lines.iter().map(|line| format!("{line}\n")).collect();
    ResponseTemplate::new(200).set_body_raw(body.into_bytes(), "application/x-ndjson")
}

fn piece(text: &str) -> Value {
    json!({"message":{"role":"assistant","content":text},"done":false})
}

fn end() -> Value {
    json!({"message":{"role":"assistant","content":""},"done":true})
}

/// Ollama answering "回答<n>" in two pieces, where n is the number of messages it was sent.
async fn ollama() -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/chat"))
        .respond_with(|request: &MockRequest| {
            let body: Value = request.body_json().unwrap();
            assert_eq!(body["stream"], true);
            let count = body["messages"].as_array().unwrap().len();
            ndjson(&[piece("回答"), piece(&count.to_string()), end()])
        })
        .mount(&server)
        .await;
    server
}

async fn start(state: &AppState, cookie: &str, guild: u64) -> (StatusCode, u64) {
    let (status, _, body) = send(
        state,
        write(
            "POST",
            cookie,
            &format!("/api/conversations?guild={guild}"),
            None,
        ),
    )
    .await;
    let id = if status.is_success() {
        json_body(&body)["id"].as_u64().unwrap()
    } else {
        0
    };
    (status, id)
}

async fn message_rows(db: &Database, conversation: u64) -> Vec<(String, String, String)> {
    sqlx::query_as(
        "SELECT role,status,content FROM web_messages WHERE conversation_id=? ORDER BY id",
    )
    .bind(conversation)
    .fetch_all(&db.pool)
    .await
    .unwrap()
}

/// Waits until the user's generation slot is free again (its task has ended).
async fn wait_for_slot(state: &AppState, user: u64) {
    for _ in 0..200 {
        if state.limits.enter(Key::User(user)).is_ok() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("the generation slot was not released");
}

#[tokio::test]
#[ignore = "requires compose.test.yaml and TEST_DATABASE_URL"]
async fn web_chat() {
    let db = database().await;
    let (owner, other) = (941_001_u64, 941_002_u64);
    let guild = 941_101_u64;
    clear(&db, &[owner, other], &[guild]).await;
    let discord = discord(&db, guild, &[owner, other]).await;
    let ollama = ollama().await;
    let state = state(db.clone(), &discord.uri(), &ollama.uri(), 3, false);
    state.bot_guilds.write().unwrap().insert(guild);
    let owner_cookie = login(&db, owner, guild).await;
    let other_cookie = login(&db, other, guild).await;
    let list_uri = format!("/api/conversations?guild={guild}");

    let (status, _, body) = send(&state, read(&owner_cookie, &list_uri)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        json_body(&body),
        json!({"conversations": [], "knowledge": false, "daily_limit": 3, "daily_used": 0})
    );
    // An unknown or missing guild is 404 without asking Discord.
    for uri in ["/api/conversations?guild=941999", "/api/conversations"] {
        assert_eq!(
            send(&state, read(&owner_cookie, uri)).await.0,
            StatusCode::NOT_FOUND
        );
    }

    // Starting twice gives the same empty conversation.
    let (status, conversation) = start(&state, &owner_cookie, guild).await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(
        start(&state, &owner_cookie, guild).await,
        (StatusCode::OK, conversation)
    );

    // Another user's conversation does not exist for them, whatever they try.
    let path = format!("/api/conversations/{conversation}");
    for request in [
        read(&other_cookie, &path),
        write(
            "PATCH",
            &other_cookie,
            &path,
            Some(json!({"title": "乗っ取り"})),
        ),
        write("DELETE", &other_cookie, &path, None),
        message(
            &other_cookie,
            conversation,
            json!({"content": "こんにちは"}),
        ),
    ] {
        let (status, _, body) = send(&state, request).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(json_body(&body)["error"], "not_found");
    }
    let (_, _, body) = send(&state, read(&other_cookie, &list_uri)).await;
    assert_eq!(json_body(&body)["conversations"], json!([]));
    assert_eq!(message_rows(&db, conversation).await.len(), 0);

    // Invalid messages.
    for body in [
        json!({"content": " \n "}),
        json!({"content": "あ".repeat(4_001)}),
        json!({"content": "x", "unknown": true}),
    ] {
        let (status, _, _) = send(&state, message(&owner_cookie, conversation, body)).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }
    // 4,001 characters that JSON escapes to six bytes each (24 KiB): over the length limit,
    // but within this route's body limit (the other JSON routes take 16 KiB), so the answer is
    // the length message rather than "too large".
    let (status, _, body) = send(
        &state,
        message(
            &owner_cookie,
            conversation,
            json!({"content": "\u{7}".repeat(4_001)}),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(
        json_body(&body)["message"]
            .as_str()
            .unwrap()
            .contains("4,000文字")
    );

    // The first answer: the request holds the system prompt and the question.
    let (status, headers, body) = send(
        &state,
        message(
            &owner_cookie,
            conversation,
            json!({"content": "最初の質問です。\n二行目", "web_search": false}),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers[header::CONTENT_TYPE], "text/event-stream");
    let received = events(&body);
    let names: Vec<_> = received.iter().map(|(name, _)| name.as_str()).collect();
    assert_eq!(names, ["delta", "delta", "done"]);
    assert_eq!(received[0].1, json!({"text": "回答"}));
    let answer = received[2].1["message_id"].as_u64().unwrap();
    assert_eq!(received[2].1["status"], "completed");
    wait_for_slot(&state, owner).await;

    // The second answer gets the first turn as context: four messages.
    let (_, _, body) = send(
        &state,
        message(&owner_cookie, conversation, json!({"content": "次の質問"})),
    )
    .await;
    assert_eq!(events(&body).last().unwrap().0, "done");
    wait_for_slot(&state, owner).await;

    let (status, _, body) = send(&state, read(&owner_cookie, &path)).await;
    assert_eq!(status, StatusCode::OK);
    let detail = json_body(&body);
    assert_eq!(detail["title"], "最初の質問です。 二行目");
    assert_eq!(detail["guild_id"], guild.to_string());
    let summary: Vec<_> = detail["messages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| {
            (
                m["role"].as_str().unwrap(),
                m["status"].as_str().unwrap(),
                m["content"].as_str().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        summary,
        [
            ("user", "completed", "最初の質問です。\n二行目"),
            ("assistant", "completed", "回答2"),
            ("user", "completed", "次の質問"),
            ("assistant", "completed", "回答4"),
        ]
    );
    assert_eq!(detail["messages"][1]["id"], answer);
    assert_eq!(detail["messages"][1]["error"], Value::Null);

    // Renaming.
    let (status, _, body) = send(
        &state,
        write(
            "PATCH",
            &owner_cookie,
            &path,
            Some(json!({"title": " 議事録 "})),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(json_body(&body)["title"], "議事録");
    for title in ["", "改行\nあり", &"長".repeat(101)] {
        let request = write("PATCH", &owner_cookie, &path, Some(json!({"title": title})));
        assert_eq!(send(&state, request).await.0, StatusCode::BAD_REQUEST);
    }
    let (_, _, body) = send(&state, read(&owner_cookie, &list_uri)).await;
    let list = json_body(&body);
    assert_eq!(list["conversations"][0]["title"], "議事録");
    assert_eq!(list["daily_used"], 2);

    // Busy: this user is already generating (another tab), or every slot is taken.
    let held = state.limits.enter(Key::User(owner)).unwrap();
    let (status, _, body) = send(
        &state,
        message(
            &owner_cookie,
            conversation,
            json!({"content": "二つ目のタブ"}),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(json_body(&body)["error"], "busy");
    drop(held);
    let held: Vec<_> = (0..4)
        .map(|i| state.limits.enter(Key::Channel(941_900 + i)).unwrap())
        .collect();
    let (status, _, body) = send(
        &state,
        message(&owner_cookie, conversation, json!({"content": "混雑"})),
    )
    .await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(json_body(&body)["error"], "server_busy");
    drop(held);
    // Refused messages are not stored.
    assert_eq!(message_rows(&db, conversation).await.len(), 4);

    // The daily limit (3 here): the third message goes through, the fourth does not.
    let (_, _, body) = send(
        &state,
        message(&owner_cookie, conversation, json!({"content": "三つ目"})),
    )
    .await;
    assert_eq!(events(&body).last().unwrap().0, "done");
    wait_for_slot(&state, owner).await;
    let (status, _, body) = send(
        &state,
        message(&owner_cookie, conversation, json!({"content": "四つ目"})),
    )
    .await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
    let refusal = json_body(&body);
    assert_eq!(refusal["error"], "daily_limit");
    assert!(refusal["message"].as_str().unwrap().contains("3件"));

    // Deleting removes the messages too, but does not reset the daily limit.
    let (status, _, _) = send(&state, write("DELETE", &owner_cookie, &path, None)).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert!(message_rows(&db, conversation).await.is_empty());
    assert_eq!(
        send(&state, read(&owner_cookie, &path)).await.0,
        StatusCode::NOT_FOUND
    );
    let (status, second) = start(&state, &owner_cookie, guild).await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, _, body) = send(
        &state,
        message(&owner_cookie, second, json!({"content": "削除後"})),
    )
    .await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(json_body(&body)["error"], "daily_limit");

    // Without the use role, nothing can be started, sent or listed.
    db.remove_guild_role(guild, RoleKind::Use, ROLE)
        .await
        .unwrap();
    assert_eq!(
        start(&state, &other_cookie, guild).await.0,
        StatusCode::FORBIDDEN
    );
    let (status, _, body) = send(&state, read(&other_cookie, &list_uri)).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(json_body(&body)["error"], "no_use_role");
    // A denied guild disappears altogether.
    db.add_guild_role(guild, RoleKind::Use, ROLE, None)
        .await
        .unwrap();
    assert!(db.deny_guild(guild).await.unwrap());
    assert_eq!(
        start(&state, &other_cookie, guild).await.0,
        StatusCode::NOT_FOUND
    );
    clear(&db, &[owner, other], &[guild]).await;
}

#[tokio::test]
#[ignore = "requires compose.test.yaml and TEST_DATABASE_URL"]
async fn web_chat_knowledge_notices() {
    let db = database().await;
    let user = 942_001_u64;
    let guild = 942_101_u64;
    clear(&db, &[user], &[guild]).await;
    let discord = discord(&db, guild, &[user]).await;
    let ollama = ollama().await;
    let state = state(db.clone(), &discord.uri(), &ollama.uri(), 100, true);
    state.bot_guilds.write().unwrap().insert(guild);
    let cookie = login(&db, user, guild).await;
    let (_, _, body) = send(
        &state,
        read(&cookie, &format!("/api/conversations?guild={guild}")),
    )
    .await;
    // Enabled, but the guild has no ready documents: the composer does not offer it.
    assert_eq!(json_body(&body)["knowledge"], false);
    let (_, conversation) = start(&state, &cookie, guild).await;

    // Asked for, without documents: answered without them and said so, as /talk does.
    let (_, _, body) = send(
        &state,
        message(
            &cookie,
            conversation,
            json!({"content": "資料は？", "knowledge": true}),
        ),
    )
    .await;
    let received = events(&body);
    let streamed: String = received
        .iter()
        .filter(|(name, _)| name == "delta")
        .map(|(_, data)| data["text"].as_str().unwrap())
        .collect();
    assert_eq!(streamed, format!("回答2{KNOWLEDGE_EMPTY}"));
    wait_for_slot(&state, user).await;

    // A ready document that no provider can search: the search fails, the answer says so and
    // records an empty document list.
    sqlx::query("INSERT INTO kb_documents (guild_id,title,file_name,media_type,byte_size,sha256,content,char_count,chunk_count,status,created_at,updated_at) VALUES (?,'手順書','手順書.txt','text/plain',3,UNHEX(SHA2('chat',256)),'abc',3,1,'ready',UTC_TIMESTAMP(3),UTC_TIMESTAMP(3))")
        .bind(guild)
        .execute(&db.pool)
        .await
        .unwrap();
    let document: u64 = sqlx::query_scalar("SELECT id FROM kb_documents WHERE guild_id=?")
        .bind(guild)
        .fetch_one(&db.pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO kb_chunks (document_id,guild_id,seq,content) VALUES (?,?,0,'abc')")
        .bind(document)
        .bind(guild)
        .execute(&db.pool)
        .await
        .unwrap();
    let chunk: u64 = sqlx::query_scalar("SELECT id FROM kb_chunks WHERE document_id=?")
        .bind(document)
        .fetch_one(&db.pool)
        .await
        .unwrap();
    let vector: Vec<u8> = (0..768)
        .flat_map(|i| if i == 0 { 1.0_f32 } else { 0.0 }.to_le_bytes())
        .collect();
    sqlx::query(
        "INSERT INTO kb_embeddings (chunk_id,provider,guild_id,embedding) VALUES (?,?,?,?)",
    )
    .bind(chunk)
    .bind("gemini:chat-test")
    .bind(guild)
    .bind(vector)
    .execute(&db.pool)
    .await
    .unwrap();
    let (_, _, body) = send(
        &state,
        read(&cookie, &format!("/api/conversations?guild={guild}")),
    )
    .await;
    assert_eq!(json_body(&body)["knowledge"], true);
    let (_, _, body) = send(
        &state,
        message(&cookie, conversation, json!({"content": "手順は？"})),
    )
    .await;
    let received = events(&body);
    assert_eq!(received.last().unwrap().0, "done");
    let (_, _, body) = send(
        &state,
        read(&cookie, &format!("/api/conversations/{conversation}")),
    )
    .await;
    let detail = json_body(&body);
    let answer = &detail["messages"][3];
    assert!(
        answer["content"]
            .as_str()
            .unwrap()
            .ends_with(KNOWLEDGE_FAILED)
    );
    assert_eq!(answer["knowledge"], true);
    assert_eq!(answer["kb_sources"], json!([]));
    // knowledge:false never searches.
    wait_for_slot(&state, user).await;
    let (_, _, body) = send(
        &state,
        message(
            &cookie,
            conversation,
            json!({"content": "資料なしで", "knowledge": false}),
        ),
    )
    .await;
    assert_eq!(events(&body).last().unwrap().0, "done");
    let (_, _, body) = send(
        &state,
        read(&cookie, &format!("/api/conversations/{conversation}")),
    )
    .await;
    let answer = &json_body(&body)["messages"][5];
    assert_eq!(answer["knowledge"], false);
    assert!(!answer["content"].as_str().unwrap().contains('※'));
    clear(&db, &[user], &[guild]).await;
}

/// An Ollama stand-in that streams `lines` and then keeps every answer open without finishing.
async fn stalling_ollama(lines: Vec<Value>) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            let lines = lines.clone();
            tokio::spawn(async move {
                let mut request = Vec::new();
                let mut buffer = [0_u8; 8192];
                loop {
                    let Ok(read) = stream.read(&mut buffer).await else {
                        return;
                    };
                    if read == 0 {
                        return;
                    }
                    request.extend_from_slice(&buffer[..read]);
                    let Some(head) = request.windows(4).position(|w| w == b"\r\n\r\n") else {
                        continue;
                    };
                    let length = String::from_utf8_lossy(&request[..head])
                        .to_ascii_lowercase()
                        .lines()
                        .find_map(|line| line.strip_prefix("content-length:")?.trim().parse().ok())
                        .unwrap_or(0_usize);
                    if request.len() >= head + 4 + length {
                        break;
                    }
                }
                let _ = stream
                    .write_all(b"HTTP/1.1 200 OK\r\ncontent-type: application/x-ndjson\r\ntransfer-encoding: chunked\r\n\r\n")
                    .await;
                for line in lines {
                    let line = format!("{line}\n");
                    let chunk = format!("{:x}\r\n{line}\r\n", line.len());
                    let _ = stream.write_all(chunk.as_bytes()).await;
                    let _ = stream.flush().await;
                }
                tokio::time::sleep(Duration::from_secs(600)).await;
            });
        }
    });
    format!("http://{address}")
}

async fn answer_row(db: &Database, conversation: u64) -> (String, String) {
    sqlx::query_as("SELECT status,content FROM web_messages WHERE conversation_id=? AND role='assistant' ORDER BY id DESC LIMIT 1")
        .bind(conversation)
        .fetch_one(&db.pool)
        .await
        .unwrap()
}

async fn wait_for_status(db: &Database, conversation: u64, status: &str) -> String {
    for _ in 0..200 {
        let (current, content) = answer_row(db, conversation).await;
        if current == status {
            return content;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("the answer did not become {status}");
}

/// Opens an answer and reads its first two pieces.
async fn begin_answer(state: &AppState, cookie: &str, conversation: u64) -> EventReader {
    let mut reader = EventReader::open(
        state,
        message(cookie, conversation, json!({"content": "長い話をして"})),
    )
    .await;
    for expected in ["前半", "の続き"] {
        let (name, data) = reader.next().await.unwrap();
        assert_eq!(
            (name.as_str(), data["text"].as_str()),
            ("delta", Some(expected))
        );
    }
    reader
}

#[tokio::test]
#[ignore = "requires compose.test.yaml and TEST_DATABASE_URL"]
async fn web_chat_stops_with_partial_text() {
    let db = database().await;
    let user = 943_001_u64;
    let guild = 943_101_u64;
    clear(&db, &[user], &[guild]).await;
    let discord = discord(&db, guild, &[user]).await;
    let ollama = stalling_ollama(vec![piece("前半"), piece("の続き")]).await;
    let state = state(db.clone(), &discord.uri(), &ollama, 100, false);
    state.bot_guilds.write().unwrap().insert(guild);
    let cookie = login(&db, user, guild).await;
    let (_, conversation) = start(&state, &cookie, guild).await;

    // Nothing to stop yet.
    let (status, _, body) = send(&state, write("POST", &cookie, "/api/chat/stop", None)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(json_body(&body), json!({"stopped": false}));

    // The stop button: the stream ends with `done` and the text so far is kept.
    let mut reader = begin_answer(&state, &cookie, conversation).await;
    assert_eq!(answer_row(&db, conversation).await.0, "streaming");
    let (_, _, body) = send(
        &state,
        read(&cookie, &format!("/api/conversations/{conversation}")),
    )
    .await;
    assert_eq!(json_body(&body)["messages"][1]["status"], "streaming");
    let (_, _, body) = send(&state, write("POST", &cookie, "/api/chat/stop", None)).await;
    assert_eq!(json_body(&body), json!({"stopped": true}));
    let (name, data) = reader.next().await.unwrap();
    assert_eq!(name, "done");
    assert_eq!(data["status"], "stopped");
    assert!(reader.next().await.is_none());
    assert_eq!(
        answer_row(&db, conversation).await,
        ("stopped".into(), "前半の続き".into())
    );
    wait_for_slot(&state, user).await;

    // Closing the tab: the generation stops too and keeps its text.
    let reader = begin_answer(&state, &cookie, conversation).await;
    drop(reader);
    assert_eq!(
        wait_for_status(&db, conversation, "stopped").await,
        "前半の続き"
    );
    wait_for_slot(&state, user).await;
    // Both questions are kept with their partial answers.
    let rows = message_rows(&db, conversation).await;
    assert_eq!(rows.len(), 4);

    // Deleting the conversation stops its answer.
    let reader = begin_answer(&state, &cookie, conversation).await;
    let path = format!("/api/conversations/{conversation}");
    let (status, _, _) = send(&state, write("DELETE", &cookie, &path, None)).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let mut reader = reader;
    // Nothing is left to save or report: the stream just ends.
    assert!(reader.next().await.is_none());
    wait_for_slot(&state, user).await;
    assert!(message_rows(&db, conversation).await.is_empty());

    // Shutting down: the answer is saved as interrupted and the stream says so.
    let (_, conversation) = start(&state, &cookie, guild).await;
    let mut reader = begin_answer(&state, &cookie, conversation).await;
    state.chat.shut_down();
    let (name, data) = reader.next().await.unwrap();
    assert_eq!(name, "error");
    assert!(data["message"].as_str().unwrap().contains("中断"));
    assert!(reader.next().await.is_none());
    assert_eq!(
        answer_row(&db, conversation).await,
        ("interrupted".into(), "前半の続き".into())
    );
    clear(&db, &[user], &[guild]).await;
}

#[tokio::test]
#[ignore = "requires compose.test.yaml and TEST_DATABASE_URL"]
async fn web_chat_retention_and_recovery() {
    let db = database().await;
    let (user, other) = (944_001_u64, 944_002_u64);
    let guild = 944_101_u64;
    clear(&db, &[user, other], &[guild]).await;
    let now = Utc::now();
    let post = |conversation: u64, content: &'static str, at| NewTurn {
        user_id: user,
        conversation_id: conversation,
        content,
        web_search: false,
        knowledge: false,
        now: at,
    };

    // Retention counts from the last update: an old conversation updated recently stays.
    let old = now - chrono::Duration::days(40);
    let (stale, _) = db.start_conversation(user, guild, old).await.unwrap();
    db.post_question(&post(stale.id, "古い質問", old))
        .await
        .unwrap();
    let (revived, _) = db.start_conversation(user, guild + 1, old).await.unwrap();
    db.post_question(&post(revived.id, "古い会話", old))
        .await
        .unwrap();
    db.post_question(&post(revived.id, "最近の続き", now))
        .await
        .unwrap();
    let (fresh, _) = db.start_conversation(user, guild + 2, now).await.unwrap();
    assert!(
        db.purge_conversations(now - chrono::Duration::days(30))
            .await
            .unwrap()
            >= 1
    );
    assert!(db.conversation(user, stale.id).await.unwrap().is_none());
    assert!(message_rows(&db, stale.id).await.is_empty());
    assert!(db.conversation(user, revived.id).await.unwrap().is_some());
    assert_eq!(message_rows(&db, revived.id).await.len(), 4);
    assert!(db.conversation(user, fresh.id).await.unwrap().is_some());
    // The daily count sees only this user's questions of the last 24 hours.
    assert_eq!(
        db.chat_questions_since(user, now - chrono::Duration::days(1))
            .await
            .unwrap(),
        1
    );
    assert_eq!(
        db.chat_questions_since(other, now - chrono::Duration::days(1))
            .await
            .unwrap(),
        0
    );
    // The owner filter: another user cannot post into, rename or delete it.
    let foreign = NewTurn {
        user_id: other,
        ..post(fresh.id, "他人", now)
    };
    assert_eq!(db.post_question(&foreign).await.unwrap(), Posted::NotFound);
    assert!(!db.rename_conversation(other, fresh.id, "x").await.unwrap());
    assert!(!db.delete_conversation(other, fresh.id).await.unwrap());
    assert!(
        db.conversation_messages(other, revived.id)
            .await
            .unwrap()
            .is_empty()
    );

    // Startup recovery: answers left streaming become interrupted; finished ones stay.
    let Posted::Saved { answer, .. } = db
        .post_question(&post(fresh.id, "質問", now))
        .await
        .unwrap()
    else {
        panic!("not saved");
    };
    assert!(db.recover_web_messages().await.unwrap() >= 1);
    let rows = db.conversation_messages(user, fresh.id).await.unwrap();
    let recovered = rows.iter().find(|row| row.id == answer).unwrap();
    assert_eq!(recovered.status, Status::Interrupted);
    assert_eq!(rows[0].status, Status::Completed);
    clear(&db, &[user, other], &[guild]).await;
}
