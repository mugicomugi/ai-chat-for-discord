//! Embedding providers against wiremock: the request each provider gets, response checks, error
//! classes, pauses after rate limits, and the failover order of query searches.

use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

use discord_discussion_bot::{
    config::{ProviderConfig, ProviderKind},
    knowledge::{
        SearchError,
        embed::{
            DIMENSIONS, DocumentText, EmbedError, Embedder, Provider, ProviderState, vector_bytes,
        },
        store::Hit,
    },
};
use serde_json::{Value, json};
use wiremock::{
    Mock, MockServer, Request, ResponseTemplate,
    matchers::{header, method, path},
};

const GEMINI_PATH: &str = "/v1beta/models/gemini-embedding-001:batchEmbedContents";

/// A vector pointing along axis `axis`, not normalized (length 2).
fn axis(axis: usize) -> Vec<f64> {
    let mut values = vec![0.0; DIMENSIONS];
    values[axis] = 2.0;
    values
}

fn provider(kind: ProviderKind, model: &str, server: &MockServer) -> Provider {
    Provider::new(
        ProviderConfig::new(kind, model, "test-key", &server.uri()),
        reqwest::Client::new(),
    )
}

fn texts() -> [DocumentText<'static>; 2] {
    [
        DocumentText {
            title: "手順書",
            text: "一つ目",
        },
        DocumentText {
            title: "手順書",
            text: "二つ目",
        },
    ]
}

fn assert_axis(vector: &[f32], expected: usize) {
    assert_eq!(vector.len(), DIMENSIONS);
    for (i, value) in vector.iter().enumerate() {
        let want = if i == expected { 1.0 } else { 0.0 };
        assert!((value - want).abs() < 1e-6, "{i}: {value}");
    }
}

#[tokio::test]
async fn gemini_requests_documents_and_queries_with_task_types() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(GEMINI_PATH))
        .and(header("x-goog-api-key", "test-key"))
        .respond_with(|request: &Request| {
            assert!(request.headers.get("authorization").is_none());
            let body: Value = request.body_json().unwrap();
            let requests = body["requests"].as_array().unwrap();
            let task = requests[0]["taskType"].as_str().unwrap();
            for item in requests {
                assert_eq!(item["model"], "models/gemini-embedding-001");
                assert_eq!(item["outputDimensionality"], 768);
                assert_eq!(item["taskType"], task);
                // A title only for documents.
                assert_eq!(item.get("title").is_some(), task == "RETRIEVAL_DOCUMENT");
            }
            let embeddings: Vec<Value> = (0..requests.len())
                .map(|i| json!({"values": axis(i + 1)}))
                .collect();
            if task == "RETRIEVAL_DOCUMENT" {
                assert_eq!(requests[0]["title"], "手順書");
                assert_eq!(requests[1]["content"]["parts"][0]["text"], "二つ目");
            } else {
                assert_eq!(task, "RETRIEVAL_QUERY");
                assert_eq!(requests[0]["content"]["parts"][0]["text"], "質問");
            }
            ResponseTemplate::new(200).set_body_json(json!({"embeddings": embeddings}))
        })
        .expect(2)
        .mount(&server)
        .await;
    let gemini = provider(ProviderKind::Gemini, "gemini-embedding-001", &server);
    assert_eq!(gemini.key(), "gemini:gemini-embedding-001");
    let vectors = gemini.embed_documents(&texts()).await.unwrap();
    assert_eq!(vectors.len(), 2);
    // Gemini's shortened vectors are normalized here.
    assert_axis(&vectors[0], 1);
    assert_axis(&vectors[1], 2);
    assert_axis(&gemini.embed_query("質問").await.unwrap(), 1);
}

#[tokio::test]
async fn openai_requests_768_float_dimensions_and_sorts_by_index() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/embeddings"))
        .and(header("authorization", "Bearer test-key"))
        .respond_with(|request: &Request| {
            let body: Value = request.body_json().unwrap();
            assert_eq!(body["model"], "text-embedding-3-small");
            assert_eq!(body["dimensions"], 768);
            assert_eq!(body["encoding_format"], "float");
            // OpenAI has no title field; it leads the text.
            assert_eq!(body["input"], json!(["手順書\n一つ目", "手順書\n二つ目"]));
            ResponseTemplate::new(200).set_body_json(json!({"object": "list", "data": [
                {"object": "embedding", "index": 1, "embedding": axis(2)},
                {"object": "embedding", "index": 0, "embedding": axis(1)}
            ]}))
        })
        .expect(1)
        .mount(&server)
        .await;
    let openai = provider(ProviderKind::OpenAi, "text-embedding-3-small", &server);
    let vectors = openai.embed_documents(&texts()).await.unwrap();
    assert_axis(&vectors[0], 1);
    assert_axis(&vectors[1], 2);
}

#[tokio::test]
async fn ollama_requests_dimensions_and_checks_the_length() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/embed"))
        .and(header("authorization", "Bearer test-key"))
        .respond_with(|request: &Request| {
            let body: Value = request.body_json().unwrap();
            assert_eq!(body["model"], "embeddinggemma");
            assert_eq!(body["dimensions"], 768);
            let inputs = body["input"].as_array().unwrap().len();
            let embeddings: Vec<Vec<f64>> = (0..inputs).map(axis).collect();
            ResponseTemplate::new(200)
                .set_body_json(json!({"model": "embeddinggemma", "embeddings": embeddings}))
        })
        .expect(2)
        .mount(&server)
        .await;
    let ollama = provider(ProviderKind::Ollama, "embeddinggemma", &server);
    let vectors = ollama.embed_documents(&texts()).await.unwrap();
    assert_axis(&vectors[1], 1);
    assert_axis(&ollama.embed_query("質問").await.unwrap(), 0);
}

/// Finite in JSON, infinite once converted to f32.
fn beyond_f32() -> Vec<f64> {
    let mut values = axis(2);
    values[0] = 1e39;
    values
}

#[tokio::test]
async fn wrong_dimensions_counts_and_values_are_rejected() {
    for body in [
        json!({"embeddings": [{"values": vec![1.0; DIMENSIONS - 1]}, {"values": axis(1)}]}),
        json!({"embeddings": [{"values": axis(1)}]}),
        json!({"embeddings": [{"values": axis(1)}, {"values": beyond_f32()}]}),
        json!({"embeddings": [{"values": axis(1)}, {"values": vec![0.0; DIMENSIONS]}]}),
        json!({"unexpected": true}),
    ] {
        let server = MockServer::start().await;
        Mock::given(path(GEMINI_PATH))
            .respond_with(ResponseTemplate::new(200).set_body_json(body.clone()))
            .mount(&server)
            .await;
        let gemini = provider(ProviderKind::Gemini, "gemini-embedding-001", &server);
        assert_eq!(
            gemini.embed_documents(&texts()).await,
            Err(EmbedError::Invalid),
            "{body}"
        );
        // Not a reason to pause the provider.
        assert_eq!(gemini.status().state, ProviderState::Ok);
    }
    // OpenAI indices must be exactly 0..n.
    let server = MockServer::start().await;
    Mock::given(path("/v1/embeddings"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": [
            {"index": 0, "embedding": axis(1)}, {"index": 0, "embedding": axis(2)}
        ]})))
        .mount(&server)
        .await;
    let openai = provider(ProviderKind::OpenAi, "text-embedding-3-small", &server);
    assert_eq!(
        openai.embed_documents(&texts()).await,
        Err(EmbedError::Invalid)
    );
}

#[tokio::test]
async fn rate_limits_pause_the_provider_without_further_requests() {
    let server = MockServer::start().await;
    Mock::given(path(GEMINI_PATH))
        .respond_with(
            ResponseTemplate::new(429)
                .insert_header("retry-after", "30")
                .set_body_string("secret upstream detail"),
        )
        .expect(1)
        .mount(&server)
        .await;
    let gemini = provider(ProviderKind::Gemini, "gemini-embedding-001", &server);
    assert_eq!(
        gemini.embed_documents(&texts()).await,
        Err(EmbedError::RateLimit {
            retry_after: Some(Duration::from_secs(30))
        })
    );
    let status = gemini.status();
    assert_eq!(status.state, ProviderState::RateLimited);
    let until = status.until.unwrap() - chrono::Utc::now();
    assert!((28..=30).contains(&until.num_seconds()), "{until}");
    // Queries skip a paused provider at once (the mock's count proves no second request).
    match gemini.embed_query("質問").await {
        Err(EmbedError::RateLimit {
            retry_after: Some(wait),
        }) => assert!(wait <= Duration::from_secs(30)),
        other => panic!("{other:?}"),
    }
    server.verify().await;
}

#[tokio::test]
async fn auth_and_server_errors_are_classified() {
    for (status, body, expected, state) in [
        (401, json!({}), EmbedError::Auth, ProviderState::AuthError),
        (
            400,
            json!({"error": {"code": 400, "status": "INVALID_ARGUMENT", "details": [
                {"@type": "type.googleapis.com/google.rpc.ErrorInfo", "reason": "API_KEY_INVALID"}
            ]}}),
            EmbedError::Auth,
            ProviderState::AuthError,
        ),
        (
            400,
            json!({"error": {"code": 400, "status": "INVALID_ARGUMENT"}}),
            EmbedError::BadInput,
            ProviderState::Ok,
        ),
        (500, json!({}), EmbedError::Upstream, ProviderState::Ok),
    ] {
        let server = MockServer::start().await;
        Mock::given(path(GEMINI_PATH))
            .respond_with(ResponseTemplate::new(status).set_body_json(body))
            .mount(&server)
            .await;
        let gemini = provider(ProviderKind::Gemini, "gemini-embedding-001", &server);
        assert_eq!(
            gemini.embed_documents(&texts()).await,
            Err(expected),
            "{status}"
        );
        assert_eq!(gemini.status().state, state, "{status}");
    }
    // Nothing listening.
    let config = ProviderConfig::new(ProviderKind::OpenAi, "m", "k", "http://127.0.0.1:1");
    let unreachable = Provider::new(config, reqwest::Client::new());
    assert_eq!(
        unreachable.embed_query("質問").await,
        Err(EmbedError::Upstream)
    );
}

fn hit(provider: &str) -> Hit {
    Hit {
        document_id: 1,
        title: provider.into(),
        seq: 0,
        heading: None,
        content: "抜粋".into(),
        distance: 0.1,
    }
}

#[tokio::test]
async fn query_search_fails_over_in_the_configured_order() {
    let (gemini, openai, ollama) = (
        MockServer::start().await,
        MockServer::start().await,
        MockServer::start().await,
    );
    Mock::given(path(GEMINI_PATH))
        .respond_with(ResponseTemplate::new(429).insert_header("retry-after", "60"))
        .expect(1)
        .mount(&gemini)
        .await;
    Mock::given(path("/v1/embeddings"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"data": [{"index": 0, "embedding": axis(3)}]})),
        )
        .expect(2)
        .mount(&openai)
        .await;
    Mock::given(path("/api/embed"))
        .respond_with(ResponseTemplate::new(500))
        .expect(0)
        .mount(&ollama)
        .await;
    let embedder = Embedder::new(&[
        ProviderConfig::new(
            ProviderKind::Gemini,
            "gemini-embedding-001",
            "k",
            &gemini.uri(),
        ),
        ProviderConfig::new(
            ProviderKind::OpenAi,
            "text-embedding-3-small",
            "k",
            &openai.uri(),
        ),
        ProviderConfig::new(ProviderKind::Ollama, "embeddinggemma", "k", &ollama.uri()),
    ])
    .unwrap();
    let asked = Arc::new(Mutex::new(Vec::new()));
    for _ in 0..2 {
        let asked = asked.clone();
        let hits = embedder
            .search(
                "質問",
                &[0, 1, 2],
                Duration::from_secs(5),
                move |provider, vector| {
                    let asked = asked.clone();
                    async move {
                        assert_eq!(vector.len(), DIMENSIONS * 4);
                        // The query vector arrives as VECTOR bytes.
                        let mut expected = vec![0.0_f32; DIMENSIONS];
                        expected[3] = 1.0;
                        assert_eq!(vector, vector_bytes(&expected));
                        asked.lock().unwrap().push(provider.clone());
                        Ok(vec![hit(&provider)])
                    }
                },
            )
            .await
            .unwrap();
        assert_eq!(hits[0].title, "openai:text-embedding-3-small");
    }
    // Gemini failed before a lookup, the second time without even a request.
    assert_eq!(
        *asked.lock().unwrap(),
        [
            "openai:text-embedding-3-small",
            "openai:text-embedding-3-small"
        ]
    );
    gemini.verify().await;
    openai.verify().await;
    ollama.verify().await;
}

#[tokio::test]
async fn search_moves_on_from_empty_lookups_and_slow_providers() {
    let (slow, empty, last) = (
        MockServer::start().await,
        MockServer::start().await,
        MockServer::start().await,
    );
    let one = |axis_index| json!({"data": [{"index": 0, "embedding": axis(axis_index)}]});
    Mock::given(path("/v1/embeddings"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(one(1))
                .set_delay(Duration::from_secs(2)),
        )
        .mount(&slow)
        .await;
    for server in [&empty, &last] {
        Mock::given(path("/v1/embeddings"))
            .respond_with(ResponseTemplate::new(200).set_body_json(one(1)))
            .mount(server)
            .await;
    }
    let config = |model: &str, server: &MockServer| {
        ProviderConfig::new(ProviderKind::OpenAi, model, "k", &server.uri())
    };
    let embedder = Embedder::new(&[
        config("slow", &slow),
        config("empty", &empty),
        config("last", &last),
    ])
    .unwrap();
    let hits = embedder
        .search(
            "質問",
            &[0, 1, 2],
            Duration::from_millis(300),
            |provider, _| async move {
                // The provider without vectors for the guild finds nothing.
                Ok(if provider == "openai:empty" {
                    vec![]
                } else {
                    vec![hit(&provider)]
                })
            },
        )
        .await
        .unwrap();
    assert_eq!(hits[0].title, "openai:last");
    // Answers without hits anywhere are an empty result, not an error.
    let hits = embedder
        .search(
            "質問",
            &[0, 1, 2],
            Duration::from_millis(300),
            |_, _| async { Ok(vec![]) },
        )
        .await
        .unwrap();
    assert!(hits.is_empty());
    // No provider answering at all is an error.
    let down = Embedder::new(&[config("down", &slow)]).unwrap();
    assert_eq!(
        down.search("質問", &[0], Duration::from_millis(100), |_, _| async {
            Ok(vec![])
        })
        .await,
        Err(SearchError::Unavailable)
    );
}

/// Knowledge::search orders the providers by how much of the guild's documents their vectors
/// cover; providers left out of the order are never asked.
#[tokio::test]
async fn search_follows_the_given_order() {
    let servers = [
        MockServer::start().await,
        MockServer::start().await,
        MockServer::start().await,
    ];
    for (i, server) in servers.iter().enumerate() {
        Mock::given(path("/v1/embeddings"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({"data": [{"index": 0, "embedding": axis(i)}]})),
            )
            // The second provider is not in the order; the others answer twice each at most.
            .expect(if i == 1 { 0..=0 } else { 1..=2 })
            .mount(server)
            .await;
    }
    let embedder = Embedder::new(
        &servers
            .iter()
            .enumerate()
            .map(|(i, server)| {
                ProviderConfig::new(ProviderKind::OpenAi, &format!("m{i}"), "k", &server.uri())
            })
            .collect::<Vec<_>>(),
    )
    .unwrap();
    let asked = Arc::new(Mutex::new(Vec::new()));
    let search = |empty: &'static str| {
        let asked = asked.clone();
        embedder.search(
            "質問",
            &[2, 0],
            Duration::from_secs(5),
            move |provider, _| {
                asked.lock().unwrap().push(provider.clone());
                async move {
                    Ok(if provider == empty {
                        vec![]
                    } else {
                        vec![hit(&provider)]
                    })
                }
            },
        )
    };
    assert_eq!(search("none").await.unwrap()[0].title, "openai:m2");
    // A provider whose lookup finds nothing hands over to the next in the order.
    assert_eq!(search("openai:m2").await.unwrap()[0].title, "openai:m0");
    assert_eq!(
        *asked.lock().unwrap(),
        ["openai:m2", "openai:m2", "openai:m0"]
    );
    // Nobody to ask: nothing could be searched.
    assert_eq!(
        embedder
            .search("質問", &[], Duration::from_secs(5), |_, _| async {
                Ok(vec![])
            })
            .await,
        Err(SearchError::Unavailable)
    );
    for server in &servers {
        server.verify().await;
    }
}
