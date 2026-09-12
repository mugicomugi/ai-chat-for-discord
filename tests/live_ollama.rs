use std::time::Duration;

use discord_discussion_bot::agent::Agent;

/// Opt-in smoke test. Sends only the fixed public test prompts below, never Discord history.
#[tokio::test]
#[ignore = "uses real Ollama credentials and API quota from .env"]
async fn live_chat_and_search() {
    dotenvy::dotenv().ok();
    let key = std::env::var("OLLAMA_API_KEY").expect("OLLAMA_API_KEY is required");
    let model = std::env::var("OLLAMA_MODEL").unwrap_or_else(|_| "gpt-oss:120b".into());
    let agent = Agent::new("https://ollama.com", key, model).unwrap();
    let chat = tokio::time::timeout(
        Duration::from_secs(180),
        agent.answer("接続テストです。OKとだけ回答してください。", &[], false),
    )
    .await
    .expect("chat timed out")
    .expect("chat failed");
    assert!(!chat.content.is_empty());
    assert_eq!(chat.tool_count, 0);
    let search = tokio::time::timeout(
        Duration::from_secs(180),
        agent.answer("接続テストです。必ずweb_searchツールでRustプログラミング言語の公式サイトを検索し、そのURLを1つ示してください。", &[], true),
    ).await.expect("search timed out").expect("search failed");
    assert!(search.tool_count > 0, "model did not exercise a tool");
    assert!(!search.sources.is_empty(), "no verified source returned");
}
