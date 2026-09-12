use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

use discord_discussion_bot::agent::{Agent, AgentError};
use serde_json::{Value, json};
use wiremock::{
    Mock, MockServer, Request, ResponseTemplate,
    matchers::{header, method, path},
};

fn reply(content: &str) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(
        json!({"message":{"role":"assistant","content":content,"thinking":"PRIVATE_THINKING"}}),
    )
}

fn tool(name: &str, arguments: Value) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(json!({"message":{"role":"assistant","content":"","thinking":"PRIVATE_THINKING","tool_calls":[{"function":{"name":name,"arguments":arguments}}]}}))
}

fn agent(server: &MockServer) -> Agent {
    Agent::new(&server.uri(), "test_key".into(), "gpt-oss:120b".into()).unwrap()
}

#[tokio::test]
async fn disabled_search_never_exposes_or_executes_tools() {
    let server = MockServer::start().await;
    Mock::given(method("POST")).and(path("/api/chat")).and(header("authorization", "Bearer test_key"))
        .respond_with(|req: &Request| {
            let body: Value = req.body_json().unwrap();
            assert!(body.get("tools").is_none());
            assert_eq!(body["model"], "gpt-oss:120b");
            assert_eq!(body["stream"], false);
            // Even an unsolicited tool call must not execute when search is disabled.
            ResponseTemplate::new(200).set_body_json(json!({"message":{"role":"assistant","content":"回答", "tool_calls":[{"function":{"name":"web_search","arguments":{"query":"ignored"}}}]}}))
        }).expect(1).mount(&server).await;
    let answer = agent(&server).answer("検索して", &[], false).await.unwrap();
    assert_eq!(answer.tool_count, 0);
    assert!(answer.sources.is_empty());
    assert_eq!(server.received_requests().await.unwrap().len(), 1);
}

#[tokio::test]
async fn search_fetch_roundtrip_sources_and_no_thinking() {
    let server = MockServer::start().await;
    let turns = Arc::new(AtomicUsize::new(0));
    Mock::given(path("/api/chat"))
        .respond_with(move |req: &Request| {
            let body: Value = req.body_json().unwrap();
            assert!(!String::from_utf8_lossy(&req.body).contains("PRIVATE_THINKING"));
            assert_eq!(body["tools"].as_array().unwrap().len(), 2);
            match turns.fetch_add(1, Ordering::SeqCst) {
                0 => tool("web_search", json!({"query":"Rust release"})),
                1 => {
                    assert_eq!(body["messages"][3]["role"], "tool");
                    tool("web_fetch", json!({"url":"https://www.rust-lang.org/"}))
                }
                _ => reply("取得した情報から回答します。"),
            }
        })
        .expect(3)
        .mount(&server)
        .await;
    Mock::given(path("/api/web_search")).respond_with(|req: &Request| {
        assert_eq!(req.body_json::<Value>().unwrap()["max_results"], 5);
        ResponseTemplate::new(200).set_body_json(json!({"results":[{"title":"Rust", "url":"https://www.rust-lang.org/", "content":"result"}]}))
    }).expect(1).mount(&server).await;
    Mock::given(path("/api/web_fetch"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"title":"Rust", "content":"fetched content", "links":[]})),
        )
        .expect(1)
        .mount(&server)
        .await;
    let result = agent(&server).answer("最新情報", &[], true).await.unwrap();
    assert_eq!(result.tool_count, 2);
    assert_eq!(result.sources.len(), 1);
    assert!(result.content.contains("https://www.rust-lang.org/"));
    assert!(!result.content.contains("PRIVATE_THINKING"));
}

#[tokio::test]
async fn five_calls_is_a_hard_limit() {
    let server = MockServer::start().await;
    Mock::given(path("/api/chat"))
        .respond_with(|req: &Request| {
            let body: Value = req.body_json().unwrap();
            if body.get("tools").is_none() {
                return reply("上限内の資料で回答");
            }
            let calls: Vec<_> = (0..7)
                .map(|_| json!({"function":{"name":"web_search","arguments":{"query":"query"}}}))
                .collect();
            ResponseTemplate::new(200)
                .set_body_json(json!({"message":{"role":"assistant","tool_calls":calls}}))
        })
        .expect(2)
        .mount(&server)
        .await;
    Mock::given(path("/api/web_search"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"results":[]})))
        .expect(5)
        .mount(&server)
        .await;
    let answer = agent(&server).answer("調べて", &[], true).await.unwrap();
    assert_eq!(answer.tool_count, 5);
    assert!(answer.content.contains("回数上限"));
}

#[tokio::test]
async fn api_errors_are_classified_without_leaking_response() {
    for (status, expected) in [
        (401, "authentication"),
        (403, "authentication"),
        (429, "rate_limit"),
        (500, "upstream"),
    ] {
        let server = MockServer::start().await;
        Mock::given(path("/api/chat"))
            .respond_with(
                ResponseTemplate::new(status).set_body_string("sensitive upstream detail"),
            )
            .mount(&server)
            .await;
        let error = agent(&server)
            .answer("質問", &[], false)
            .await
            .err()
            .unwrap();
        assert_eq!(error.to_string(), expected);
        assert!(!error.user_message().contains("sensitive"));
    }
}

#[tokio::test]
async fn failed_search_is_reported_and_results_are_bounded() {
    let server = MockServer::start().await;
    let turns = AtomicUsize::new(0);
    Mock::given(path("/api/chat"))
        .respond_with(move |_: &Request| {
            if turns.fetch_add(1, Ordering::SeqCst) == 0 {
                tool("web_search", json!({"query":"query"}))
            } else {
                reply("情報不足です。")
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
    let result = agent(&server).answer("質問", &[], true).await.unwrap();
    assert!(result.content.contains("一部に失敗"));
    assert!(result.sources.is_empty());
}

#[tokio::test]
async fn malformed_response_and_outer_timeout() {
    let server = MockServer::start().await;
    Mock::given(path("/api/chat"))
        .respond_with(ResponseTemplate::new(200).set_body_string("not json"))
        .mount(&server)
        .await;
    assert!(matches!(
        agent(&server).answer("質問", &[], false).await,
        Err(AgentError::InvalidResponse)
    ));
    server.reset().await;
    Mock::given(path("/api/chat"))
        .respond_with(reply("遅い回答").set_delay(std::time::Duration::from_secs(1)))
        .mount(&server)
        .await;
    assert!(
        tokio::time::timeout(
            std::time::Duration::from_millis(20),
            agent(&server).answer("質問", &[], false)
        )
        .await
        .is_err()
    );
}

#[tokio::test]
async fn retrieved_text_is_bounded_and_unusable_urls_are_excluded() {
    let server = MockServer::start().await;
    let turns = AtomicUsize::new(0);
    Mock::given(path("/api/chat"))
        .respond_with(move |req: &Request| {
            if turns.fetch_add(1, Ordering::SeqCst) == 0 {
                return tool("web_search", json!({"query":"query"}));
            }
            let body: Value = req.body_json().unwrap();
            let tool_result: Value =
                serde_json::from_str(body["messages"][3]["content"].as_str().unwrap()).unwrap();
            let results = tool_result["results"].as_array().unwrap();
            assert_eq!(results.len(), 1);
            assert_eq!(
                results[0]["content"].as_str().unwrap().chars().count(),
                4000
            );
            reply("資料を確認しました。")
        })
        .expect(2)
        .mount(&server)
        .await;
    Mock::given(path("/api/web_search"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"results":[
            {"title":"Example", "url":"https://example.com/", "content":"日".repeat(10_000)},
            {"title":"Unsafe", "url":"javascript:alert(1)", "content":"ignored"},
            {"title":"Local", "url":"http://127.0.0.1/", "content":"ignored"}
        ]})))
        .expect(1)
        .mount(&server)
        .await;
    let result = agent(&server).answer("質問", &[], true).await.unwrap();
    assert_eq!(result.sources.len(), 1);
    assert_eq!(result.sources[0].url, "https://example.com/");
}

#[tokio::test]
async fn unknown_tools_and_private_fetch_do_not_make_requests() {
    for (name, arguments) in [
        ("execute_shell", json!({"command":"ignored"})),
        ("web_fetch", json!({"url":"http://127.0.0.1/secret"})),
    ] {
        let server = MockServer::start().await;
        let turns = AtomicUsize::new(0);
        Mock::given(path("/api/chat"))
            .respond_with(move |_: &Request| {
                if turns.fetch_add(1, Ordering::SeqCst) == 0 {
                    tool(name, arguments.clone())
                } else {
                    reply("取得できませんでした。")
                }
            })
            .expect(2)
            .mount(&server)
            .await;
        let result = agent(&server).answer("質問", &[], true).await.unwrap();
        assert!(result.sources.is_empty());
        assert_eq!(server.received_requests().await.unwrap().len(), 2);
    }
}
