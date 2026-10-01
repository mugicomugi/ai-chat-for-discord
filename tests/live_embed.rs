//! Opt-in checks of the real embedding APIs with the keys from .env (docs/roadmap.md M3 §0).
//! They send only the fixed public text below, use a little of each provider's quota and print
//! whether the provider works. A provider whose key is not set is skipped.
//!
//! ```text
//! cargo test --locked --test live_embed -- --ignored --nocapture
//! ```

use std::env;

use discord_discussion_bot::{
    config::{ProviderConfig, ProviderKind},
    knowledge::embed::{DIMENSIONS, DocumentText, Provider},
};

const TEXT: &str =
    "Discord の議論を支援する Bot の、ナレッジベース接続テスト用の公開テキストです。";

async fn check(kind: ProviderKind, key_name: &str, model_name: &str, fallback_model: &str) {
    dotenvy::dotenv().ok();
    let set = |name: &str| env::var(name).ok().filter(|value| !value.trim().is_empty());
    let Some(key) = set(key_name) else {
        println!("{}: skipped ({key_name} is not set)", kind.label());
        return;
    };
    let model = set(model_name)
        .or(kind.default_model().map(String::from))
        .unwrap_or_else(|| fallback_model.to_owned());
    let provider = Provider::new(
        ProviderConfig::new(kind, &model, &key, kind.base_url()),
        reqwest::Client::new(),
    );
    let document = provider
        .embed_documents(&[DocumentText {
            title: "接続テスト",
            text: TEXT,
        }])
        .await;
    let query = provider.embed_query(TEXT).await;
    match (&document, &query) {
        (Ok(document), Ok(query)) => {
            let similarity: f32 = document[0].iter().zip(query).map(|(a, b)| a * b).sum();
            println!(
                "{}: WORKS with {model}: {} dimensions, document/query cosine similarity {similarity:.3}",
                kind.label(),
                document[0].len()
            );
            assert_eq!(query.len(), DIMENSIONS);
            assert!(similarity > 0.5, "the same text should be similar");
        }
        _ => {
            println!(
                "{}: DOES NOT WORK with {model}: document {:?}, query {:?} (embedding_auth: key refused; embedding_rate_limit: quota; embedding_invalid: not {DIMENSIONS} dimensions or bad response; embedding_upstream: model unknown, server or network error)",
                kind.label(),
                document.as_ref().err().map(ToString::to_string),
                query.as_ref().err().map(ToString::to_string),
            );
            panic!("{} embeddings are not usable", kind.label());
        }
    }
}

/// Ollama Cloud with the existing chat key; the model is OLLAMA_EMBEDDING_MODEL or
/// embeddinggemma. Record the result in docs/runbook.md.
#[tokio::test]
#[ignore = "uses real API credentials and quota from .env"]
async fn live_ollama_embed() {
    check(
        ProviderKind::Ollama,
        "OLLAMA_API_KEY",
        "OLLAMA_EMBEDDING_MODEL",
        "embeddinggemma",
    )
    .await;
}

#[tokio::test]
#[ignore = "uses real API credentials and quota from .env"]
async fn live_gemini_embed() {
    check(
        ProviderKind::Gemini,
        "GEMINI_API_KEY",
        "GEMINI_EMBEDDING_MODEL",
        "gemini-embedding-001",
    )
    .await;
}

#[tokio::test]
#[ignore = "uses real API credentials and quota from .env"]
async fn live_openai_embed() {
    check(
        ProviderKind::OpenAi,
        "OPENAI_API_KEY",
        "OPENAI_EMBEDDING_MODEL",
        "text-embedding-3-small",
    )
    .await;
}
