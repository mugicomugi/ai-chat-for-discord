//! Streamed answers (the web chat): Ollama's NDJSON lines turned into `StreamEvent`s, with the
//! tool loop and limits of the non-streamed path (tests/agent.rs).

use std::{
    sync::atomic::{AtomicUsize, Ordering},
    time::Duration,
};

use discord_discussion_bot::agent::{
    Agent, AgentError, Answer, ChatRequest, Excerpt, StreamEvent, Turn,
};
use serde_json::{Value, json};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    sync::mpsc,
};
use wiremock::{
    Mock, MockServer, Request, ResponseTemplate,
    matchers::{header, method, path},
};

fn agent(uri: &str) -> Agent {
    Agent::new(uri, "test_key".into(), "gpt-oss:120b".into()).unwrap()
}

fn line(content: &str) -> Value {
    json!({"model":"gpt-oss:120b","created_at":"2026-10-01T00:00:00Z","message":{"role":"assistant","content":content},"done":false})
}

fn thinking(text: &str) -> Value {
    json!({"model":"gpt-oss:120b","message":{"role":"assistant","content":"","thinking":text},"done":false})
}

fn tool_call(name: &str, arguments: Value) -> Value {
    json!({"model":"gpt-oss:120b","message":{"role":"assistant","content":"","tool_calls":[{"function":{"name":name,"arguments":arguments}}]},"done":false})
}

fn end() -> Value {
    json!({"model":"gpt-oss:120b","message":{"role":"assistant","content":""},"done":true,"done_reason":"stop","eval_count":10})
}

fn body(lines: &[Value]) -> String {
    lines.iter().map(|line| format!("{line}\n")).collect()
}

fn ndjson(lines: &[Value]) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_raw(body(lines).into_bytes(), "application/x-ndjson")
}

fn request<'a>(question: &'a str, web_search: bool) -> ChatRequest<'a> {
    ChatRequest {
        question,
        history: &[],
        knowledge: &[],
        turns: &[],
        web_search,
    }
}

/// Runs a streamed answer and returns it with every event it sent.
async fn run(
    agent: &Agent,
    request: &ChatRequest<'_>,
) -> (Result<Answer, AgentError>, Vec<StreamEvent>) {
    let (sender, mut receiver) = mpsc::channel(4096);
    let result = agent.respond(request, Some(&sender)).await;
    drop(sender);
    let mut events = Vec::new();
    while let Some(event) = receiver.recv().await {
        events.push(event);
    }
    (result, events)
}

fn text(events: &[StreamEvent]) -> String {
    let mut text = String::new();
    for event in events {
        match event {
            StreamEvent::Delta(delta) => text.push_str(delta),
            StreamEvent::Reset => text.clear(),
            StreamEvent::Tool { .. } => {}
        }
    }
    text
}

#[tokio::test]
async fn deltas_are_streamed_without_thinking() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/chat"))
        .and(header("authorization", "Bearer test_key"))
        .respond_with(|req: &Request| {
            let body: Value = req.body_json().unwrap();
            assert_eq!(body["stream"], true);
            assert_eq!(body["think"], "low");
            assert!(body.get("tools").is_none());
            // Earlier turns come as messages of their own, before the question.
            let roles: Vec<_> = body["messages"]
                .as_array()
                .unwrap()
                .iter()
                .map(|message| message["role"].as_str().unwrap().to_owned())
                .collect();
            assert_eq!(roles, ["system", "user", "user", "assistant", "user"]);
            assert_eq!(body["messages"][4]["content"], "続きは？");
            ndjson(&[
                thinking("PRIVATE_THINKING"),
                line("こんに"),
                thinking("PRIVATE_THINKING"),
                line("ちは"),
                end(),
            ])
        })
        .expect(1)
        .mount(&server)
        .await;
    let knowledge = [Excerpt {
        document_id: 1,
        title: "手順書".into(),
        text: "抜粋".into(),
    }];
    let turns = [Turn {
        question: "最初の質問".into(),
        answer: "最初の回答".into(),
    }];
    let request = ChatRequest {
        question: "続きは？",
        history: &[],
        knowledge: &knowledge,
        turns: &turns,
        web_search: false,
    };
    let (answer, events) = run(&agent(&server.uri()), &request).await;
    let answer = answer.unwrap();
    assert_eq!(
        events,
        [
            StreamEvent::Delta("こんに".into()),
            StreamEvent::Delta("ちは".into())
        ]
    );
    // The caller lists the knowledge documents; the text has no footer.
    assert_eq!(answer.content, "こんにちは");
    assert!(answer.sources.is_empty());
    assert_eq!(answer.tool_count, 0);
}

#[tokio::test]
async fn a_tool_call_in_a_stream_resets_the_text_and_the_answer_follows() {
    let server = MockServer::start().await;
    let turns = AtomicUsize::new(0);
    Mock::given(path("/api/chat"))
        .respond_with(move |req: &Request| {
            let body: Value = req.body_json().unwrap();
            assert_eq!(body["stream"], true);
            match turns.fetch_add(1, Ordering::SeqCst) {
                0 => {
                    assert_eq!(body["tools"].as_array().unwrap().len(), 2);
                    ndjson(&[
                        line("調べます"),
                        tool_call("web_search", json!({"query":"Rust release"})),
                        end(),
                    ])
                }
                _ => {
                    let messages = body["messages"].as_array().unwrap();
                    assert_eq!(messages[2]["role"], "assistant");
                    assert_eq!(
                        messages[2]["tool_calls"][0]["function"]["name"],
                        "web_search"
                    );
                    assert_eq!(messages[3]["role"], "tool");
                    ndjson(&[line("Rust 1.98 が"), line("最新です。"), end()])
                }
            }
        })
        .expect(2)
        .mount(&server)
        .await;
    Mock::given(path("/api/web_search"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"results":[
            {"title":"Rust","url":"https://www.rust-lang.org/","content":"result"}
        ]})))
        .expect(1)
        .mount(&server)
        .await;
    let (answer, events) = run(&agent(&server.uri()), &request("最新版は？", true)).await;
    let answer = answer.unwrap();
    assert_eq!(
        events,
        [
            StreamEvent::Delta("調べます".into()),
            StreamEvent::Reset,
            StreamEvent::Tool {
                name: "web_search",
                detail: "Rust release".into()
            },
            StreamEvent::Delta("Rust 1.98 が".into()),
            StreamEvent::Delta("最新です。".into()),
        ]
    );
    assert_eq!(answer.content, "Rust 1.98 が最新です。");
    assert_eq!(text(&events), answer.content);
    assert_eq!(answer.tool_count, 1);
    assert_eq!(answer.sources.len(), 1);
    assert_eq!(answer.sources[0].url, "https://www.rust-lang.org/");
}

#[tokio::test]
async fn notes_about_failed_searches_are_streamed_last() {
    let server = MockServer::start().await;
    let turns = AtomicUsize::new(0);
    Mock::given(path("/api/chat"))
        .respond_with(move |_: &Request| {
            if turns.fetch_add(1, Ordering::SeqCst) == 0 {
                ndjson(&[tool_call("web_search", json!({"query":"q"})), end()])
            } else {
                ndjson(&[line("情報不足です。"), end()])
            }
        })
        .expect(2)
        .mount(&server)
        .await;
    Mock::given(path("/api/web_search"))
        .respond_with(ResponseTemplate::new(429))
        .expect(1)
        .mount(&server)
        .await;
    let (answer, events) = run(&agent(&server.uri()), &request("質問", true)).await;
    let answer = answer.unwrap();
    assert!(answer.content.starts_with("情報不足です。"));
    assert!(answer.content.contains("一部に失敗"));
    // What the browser assembled is what is saved.
    assert_eq!(text(&events), answer.content);
}

#[tokio::test]
async fn error_lines_and_statuses_fail_without_leaking_details() {
    let server = MockServer::start().await;
    Mock::given(path("/api/chat"))
        .respond_with(ndjson(&[
            line("途中まで"),
            json!({"error":"sensitive upstream detail"}),
        ]))
        .mount(&server)
        .await;
    let (result, events) = run(&agent(&server.uri()), &request("質問", false)).await;
    let error = result.err().unwrap();
    assert!(matches!(error, AgentError::Upstream));
    assert!(!error.user_message().contains("sensitive"));
    assert_eq!(events, [StreamEvent::Delta("途中まで".into())]);

    for (status, expected) in [
        (401, "authentication"),
        (429, "rate_limit"),
        (500, "upstream"),
    ] {
        let server = MockServer::start().await;
        Mock::given(path("/api/chat"))
            .respond_with(ResponseTemplate::new(status).set_body_string("sensitive"))
            .mount(&server)
            .await;
        let (result, events) = run(&agent(&server.uri()), &request("質問", false)).await;
        assert_eq!(result.err().unwrap().to_string(), expected);
        assert!(events.is_empty());
    }

    // A stream that ends without its final line, or that is not JSON, is not an answer.
    for broken in [
        body(&[line("途中まで")]),
        "not json\n".to_owned(),
        body(&[end()]),
    ] {
        let server = MockServer::start().await;
        Mock::given(path("/api/chat"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_raw(broken.clone().into_bytes(), "application/x-ndjson"),
            )
            .mount(&server)
            .await;
        let (result, _) = run(&agent(&server.uri()), &request("質問", false)).await;
        assert!(
            matches!(result, Err(AgentError::InvalidResponse)),
            "{broken}"
        );
    }
}

#[tokio::test]
async fn long_answers_stop_streaming_at_the_cap() {
    let server = MockServer::start().await;
    let mut lines: Vec<Value> = (0..13).map(|_| line(&"あ".repeat(1_000))).collect();
    lines.push(end());
    Mock::given(path("/api/chat"))
        .respond_with(ndjson(&lines))
        .mount(&server)
        .await;
    let (answer, events) = run(&agent(&server.uri()), &request("長く", false)).await;
    let answer = answer.unwrap();
    let streamed: usize = events
        .iter()
        .map(|event| match event {
            StreamEvent::Delta(text) if !text.contains('※') => text.chars().count(),
            _ => 0,
        })
        .sum();
    assert_eq!(streamed, 12_000);
    assert!(
        answer
            .content
            .ends_with("\n\n※回答が長いため一部を省略しました。")
    );
    assert_eq!(
        answer.content.chars().filter(|c| *c == 'あ').count(),
        12_000
    );
    assert_eq!(text(&events), answer.content);
}

/// Answers one request with `parts` written one at a time (chunked encoding, with pauses), so
/// that the client receives them as separate chunks.
async fn chunked_server(parts: Vec<Vec<u8>>) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut request = Vec::new();
        let mut buffer = [0_u8; 4096];
        loop {
            let read = stream.read(&mut buffer).await.unwrap();
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
        stream
            .write_all(b"HTTP/1.1 200 OK\r\ncontent-type: application/x-ndjson\r\ntransfer-encoding: chunked\r\n\r\n")
            .await
            .unwrap();
        for part in parts {
            stream
                .write_all(format!("{:x}\r\n", part.len()).as_bytes())
                .await
                .unwrap();
            stream.write_all(&part).await.unwrap();
            stream.write_all(b"\r\n").await.unwrap();
            stream.flush().await.unwrap();
            tokio::time::sleep(Duration::from_millis(30)).await;
        }
        stream.write_all(b"0\r\n\r\n").await.unwrap();
    });
    format!("http://{address}")
}

#[tokio::test]
async fn lines_split_across_chunks_are_put_back_together() {
    let all = body(&[line("こんにちは"), line("世界"), end()]).into_bytes();
    // Cut inside the first JSON line, inside a three-byte character of it, and right after a
    // newline.
    let first_char = all.windows(3).position(|w| w == "こ".as_bytes()).unwrap();
    let first_newline = all.iter().position(|b| *b == b'\n').unwrap();
    let cuts = [10, first_char + 1, first_char + 4, first_newline + 1];
    let mut parts = Vec::new();
    let mut start = 0;
    for cut in cuts {
        parts.push(all[start..cut].to_vec());
        start = cut;
    }
    parts.push(all[start..].to_vec());
    let uri = chunked_server(parts).await;
    let (answer, events) = run(&agent(&uri), &request("挨拶", false)).await;
    assert_eq!(answer.unwrap().content, "こんにちは世界");
    assert_eq!(
        events,
        [
            StreamEvent::Delta("こんにちは".into()),
            StreamEvent::Delta("世界".into())
        ]
    );
}
