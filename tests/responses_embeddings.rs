//! `/v1/responses` and `/v1/embeddings` end to end against local upstream
//! mocks: category enforcement, protocol eligibility, ordered fallback before
//! commitment, raw SSE passthrough ending on Responses terminal events, no
//! fallback after commitment, unknown-field preservation, and recorded usage.

mod common;
use common::{body_bytes, json_body, spawn_upstream};

use axum::{
    Json, Router,
    body::Body,
    http::{Method, Request, StatusCode, header},
    response::Response,
    routing::post,
};
use base64::{Engine, engine::general_purpose::STANDARD};
use bytes::Bytes;
use secrecy::SecretString;
use serde_json::{Value, json};
use sqlx::Row;
use tower::ServiceExt;

use sparkrouter::{
    app::{self, AppState},
    config::{Config, DashboardLogin},
    crypto::Crypto,
    db::{
        combos::{self, Category, Strategy},
        connection_models::{self, StoredModel},
        providers as provider_db,
    },
    providers::Protocol,
};

async fn test_state() -> AppState {
    AppState::from_config(Config {
        database_url: "sqlite::memory:".into(),
        bind: "127.0.0.1:0".parse().unwrap(),
        crypto: Crypto::from_base64(&SecretString::from(STANDARD.encode([42u8; 32]))).unwrap(),
        api_key: None,
        dashboard_login: DashboardLogin::Disabled,
        dashboard_secure_cookie: false,
    })
    .await
    .unwrap()
}

async fn save_provider(state: &AppState, id: &str, base_url: &str, protocol: Protocol) {
    provider_db::save(
        &state.db,
        &provider_db::Provider {
            id: id.into(),
            name: format!("{id} display"),
            base_url: base_url.into(),
            protocol,
            encrypted_api_key: state
                .crypto
                .encrypt_api_key(id, &SecretString::from(format!("sk-{id}")))
                .unwrap(),
            enabled: true,
        },
    )
    .await
    .unwrap();
}

async fn combo(state: &AppState, name: &str, category: Category, steps: &[(&str, &str)]) {
    let combo = combos::create_typed(
        &state.db,
        name,
        category,
        Strategy::Priority,
        Default::default(),
    )
    .await
    .unwrap();
    for (provider, model) in steps {
        combos::add_step(&state.db, &combo.id, provider, model)
            .await
            .unwrap();
    }
}

fn post_json(path: &str, body: &Value) -> Request<Body> {
    Request::builder()
        .method(Method::POST)
        .uri(path)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(body.to_string()))
        .unwrap()
}

async fn send(state: &AppState, request: Request<Body>) -> Response {
    app::router(state.clone()).oneshot(request).await.unwrap()
}

/// Outcome and token counts of the only recorded request.
async fn history(state: &AppState) -> (String, Option<i64>, Option<i64>) {
    state.flush_history().await;
    let row = sqlx::query("SELECT outcome, input_tokens, output_tokens FROM request_history")
        .fetch_one(&state.db)
        .await
        .unwrap();
    (
        row.get("outcome"),
        row.get("input_tokens"),
        row.get("output_tokens"),
    )
}

fn failing(path: &'static str, status: StatusCode) -> Router {
    Router::new().route(
        path,
        post(move || async move { (status, Json(json!({"error": {"message": "mock"}}))) }),
    )
}

/// Answers `/responses` with a response object echoing what it received.
fn responses_echo() -> Router {
    Router::new().route(
        "/responses",
        post(|body: Bytes| async move {
            let received: Value = serde_json::from_slice(&body).unwrap();
            Json(json!({
                "id": "resp_mock",
                "object": "response",
                "status": "completed",
                "model": received["model"],
                "output": [{"type": "message", "content": [{"type": "output_text", "text": "hi"}]}],
                "error": null,
                "usage": {"input_tokens": 3, "output_tokens": 5, "total_tokens": 8},
                "upstream_saw": received,
            }))
        }),
    )
}

fn responses_stream(chunks: Vec<&'static str>) -> Router {
    Router::new().route(
        "/responses",
        post(move || {
            let chunks = chunks.clone();
            async move {
                let stream = async_stream::stream! {
                    for chunk in chunks {
                        yield Ok::<Bytes, std::io::Error>(Bytes::from_static(chunk.as_bytes()));
                    }
                };
                (
                    [(header::CONTENT_TYPE, "text/event-stream")],
                    Body::from_stream(stream),
                )
            }
        }),
    )
}

#[tokio::test]
async fn responses_fall_back_skip_ineligible_protocols_and_preserve_fields() {
    let state = test_state().await;
    let limited = spawn_upstream(failing("/responses", StatusCode::TOO_MANY_REQUESTS)).await;
    let anthropic = spawn_upstream(failing("/messages", StatusCode::OK)).await;
    let good = spawn_upstream(responses_echo()).await;
    save_provider(&state, "limited", &limited.url(), Protocol::OpenAi).await;
    save_provider(&state, "claude", &anthropic.url(), Protocol::Anthropic).await;
    save_provider(&state, "good", &good.url(), Protocol::OpenAi).await;
    combo(
        &state,
        "coding",
        Category::Text,
        &[
            ("limited", "model-a"),
            ("claude", "claude-model"),
            ("good", "vendor/model-b"),
        ],
    )
    .await;

    let response = send(
        &state,
        post_json(
            "/v1/responses",
            &json!({
                "model": "combo/coding",
                "input": "hello",
                "reasoning": {"effort": "low"},
                "x_vendor_extension": {"keep": [1, 2]},
            }),
        ),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let body = json_body(response).await;
    assert_eq!(body["object"], "response");
    assert_eq!(body["upstream_saw"]["model"], "vendor/model-b");
    assert_eq!(
        body["upstream_saw"]["x_vendor_extension"],
        json!({"keep": [1, 2]})
    );
    assert_eq!(body["upstream_saw"]["reasoning"], json!({"effort": "low"}));
    assert_eq!(limited.hits.count(), 1);
    // Anthropic connections cannot serve the Responses wire format.
    assert_eq!(anthropic.hits.count(), 0);
    assert_eq!(history(&state).await, ("success".into(), Some(3), Some(5)));
}

#[tokio::test]
async fn responses_reject_wrong_categories_and_invalid_success_bodies_fall_back() {
    let state = test_state().await;
    let broken = spawn_upstream(Router::new().route(
        "/responses",
        post(|| async { Json(json!(["not", "a", "response"])) }),
    ))
    .await;
    let failed = spawn_upstream(Router::new().route(
        "/responses",
        post(|| async { Json(json!({"output": [], "error": {"code": "server_error"}})) }),
    ))
    .await;
    let good = spawn_upstream(responses_echo()).await;
    save_provider(&state, "broken", &broken.url(), Protocol::OpenAi).await;
    save_provider(&state, "failed", &failed.url(), Protocol::OpenAi).await;
    save_provider(&state, "good", &good.url(), Protocol::Xai).await;
    combo(
        &state,
        "chain",
        Category::Text,
        &[("broken", "m"), ("failed", "m"), ("good", "grok-model")],
    )
    .await;
    let response = send(
        &state,
        post_json(
            "/v1/responses",
            &json!({"model": "combo/chain", "input": "x"}),
        ),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        json_body(response).await["upstream_saw"]["model"],
        "grok-model"
    );
    assert_eq!((broken.hits.count(), failed.hits.count()), (1, 1));

    combo(
        &state,
        "vectors",
        Category::Embedding,
        &[("broken", "embed-model")],
    )
    .await;
    let wrong = send(
        &state,
        post_json(
            "/v1/responses",
            &json!({"model": "combo/vectors", "input": "x"}),
        ),
    )
    .await;
    assert_eq!(wrong.status(), StatusCode::BAD_REQUEST);
    assert_eq!(broken.hits.count(), 1);
    let not_boolean = send(
        &state,
        post_json(
            "/v1/responses",
            &json!({"model": "combo/chain", "input": "x", "stream": "yes"}),
        ),
    )
    .await;
    assert_eq!(not_boolean.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn targets_without_the_endpoint_fall_back_but_invalid_requests_stop() {
    let state = test_state().await;
    // OpenAI-compatible servers that only implement chat answer 405 or 501.
    let not_allowed = spawn_upstream(
        failing("/responses", StatusCode::METHOD_NOT_ALLOWED)
            .merge(failing("/embeddings", StatusCode::METHOD_NOT_ALLOWED)),
    )
    .await;
    let not_implemented = spawn_upstream(
        failing("/responses", StatusCode::NOT_IMPLEMENTED)
            .merge(failing("/embeddings", StatusCode::NOT_IMPLEMENTED)),
    )
    .await;
    let invalid = spawn_upstream(failing("/responses", StatusCode::BAD_REQUEST)).await;
    let good = spawn_upstream(responses_echo().merge(embeddings_echo())).await;
    save_provider(&state, "chat-only", &not_allowed.url(), Protocol::OpenAi).await;
    save_provider(&state, "partial", &not_implemented.url(), Protocol::OpenAi).await;
    save_provider(&state, "invalid", &invalid.url(), Protocol::OpenAi).await;
    save_provider(&state, "good", &good.url(), Protocol::OpenAi).await;
    combo(
        &state,
        "text",
        Category::Text,
        &[("chat-only", "m"), ("partial", "m"), ("good", "m")],
    )
    .await;
    combo(
        &state,
        "vectors",
        Category::Embedding,
        &[("chat-only", "e"), ("partial", "e"), ("good", "e")],
    )
    .await;
    combo(
        &state,
        "rejected",
        Category::Text,
        &[("invalid", "m"), ("good", "m")],
    )
    .await;

    for (path, model) in [
        ("/v1/responses", "combo/text"),
        ("/v1/embeddings", "combo/vectors"),
    ] {
        let response = send(
            &state,
            post_json(path, &json!({"model": model, "input": "x"})),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK, "{path}");
    }
    assert_eq!(not_allowed.hits.count(), 2);
    assert_eq!(not_implemented.hits.count(), 2);
    assert_eq!(good.hits.count(), 2);

    // A 400 still rejects the request itself and stops the chain.
    let response = send(
        &state,
        post_json(
            "/v1/responses",
            &json!({"model": "combo/rejected", "input": "x"}),
        ),
    )
    .await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(invalid.hits.count(), 1);
    assert_eq!(good.hits.count(), 2);
}

const STREAM: &str = "event: response.created\ndata: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_1\"}}\n\nevent: response.output_text.delta\ndata: {\"type\":\"response.output_text.delta\",\"delta\":\"Hel\"}\n\n";
const COMPLETED: &str = "event: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_1\",\"usage\":{\"input_tokens\":7,\"output_tokens\":2}}}\n\n";

#[tokio::test]
async fn responses_streams_pass_through_and_end_on_the_completed_event() {
    let state = test_state().await;
    let empty = spawn_upstream(responses_stream(vec![])).await;
    // The completed event ends the stream; later bytes in that chunk are kept,
    // and the upstream body is not read further.
    let good = spawn_upstream(responses_stream(vec![STREAM, COMPLETED])).await;
    save_provider(&state, "empty", &empty.url(), Protocol::OpenAi).await;
    save_provider(&state, "good", &good.url(), Protocol::OpenAi).await;
    combo(
        &state,
        "stream",
        Category::Text,
        &[("empty", "m"), ("good", "m")],
    )
    .await;
    let response = send(
        &state,
        post_json(
            "/v1/responses",
            &json!({"model": "combo/stream", "input": "x", "stream": true}),
        ),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers()[header::CONTENT_TYPE],
        "text/event-stream"
    );
    let bytes = body_bytes(response).await;
    assert_eq!(bytes, format!("{STREAM}{COMPLETED}").as_bytes());
    // An empty upstream stream had nothing committed, so it fell back.
    assert_eq!(empty.hits.count(), 1);
    assert_eq!(history(&state).await, ("success".into(), Some(7), Some(2)));
}

#[tokio::test]
async fn responses_streams_without_a_terminal_event_end_as_interrupted_without_fallback() {
    let state = test_state().await;
    let truncated = spawn_upstream(responses_stream(vec![STREAM])).await;
    let spare = spawn_upstream(responses_stream(vec![STREAM, COMPLETED])).await;
    save_provider(&state, "truncated", &truncated.url(), Protocol::OpenAi).await;
    save_provider(&state, "spare", &spare.url(), Protocol::OpenAi).await;
    combo(
        &state,
        "cut",
        Category::Text,
        &[("truncated", "m"), ("spare", "m")],
    )
    .await;
    let response = send(
        &state,
        post_json(
            "/v1/responses",
            &json!({"model": "combo/cut", "input": "x", "stream": true}),
        ),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let collected = http_body_util::BodyExt::collect(response.into_body()).await;
    assert!(
        collected.is_err(),
        "a truncated committed stream must error"
    );
    assert_eq!(spare.hits.count(), 0, "no fallback after commitment");
    assert_eq!(history(&state).await.0, "interrupted");
}

#[tokio::test]
async fn direct_ids_route_responses_and_embeddings_to_listing_connections() {
    let state = test_state().await;
    let upstream = spawn_upstream(responses_echo().merge(embeddings_echo())).await;
    save_provider(&state, "direct", &upstream.url(), Protocol::OpenAi).await;
    connection_models::replace(
        &state.db,
        "direct",
        &[StoredModel {
            id: "org/model".into(),
            name: "Model".into(),
            fields: vec![],
            free: false,
        }],
        false,
    )
    .await
    .unwrap();
    let public = connection_models::routable_model_ids(&state.db)
        .await
        .unwrap()
        .remove(0);
    let response = send(
        &state,
        post_json("/v1/responses", &json!({"model": public, "input": "x"})),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        json_body(response).await["upstream_saw"]["model"],
        "org/model"
    );
    let response = send(
        &state,
        post_json("/v1/embeddings", &json!({"model": public, "input": "x"})),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(json_body(response).await["model"], "org/model");
}

/// Answers `/embeddings` with one vector per input and usage.
fn embeddings_echo() -> Router {
    Router::new().route(
        "/embeddings",
        post(|body: Bytes| async move {
            let received: Value = serde_json::from_slice(&body).unwrap();
            Json(json!({
                "object": "list",
                "data": [{"object": "embedding", "index": 0, "embedding": [0.25, -0.5]}],
                "model": received["model"],
                "usage": {"prompt_tokens": 4, "total_tokens": 4},
                "upstream_saw": received,
            }))
        }),
    )
}

#[tokio::test]
async fn embeddings_route_only_embedding_combos_and_fall_back_before_returning_vectors() {
    let state = test_state().await;
    let down = spawn_upstream(failing("/embeddings", StatusCode::SERVICE_UNAVAILABLE)).await;
    let invalid = spawn_upstream(Router::new().route(
        "/embeddings",
        post(|| async { Json(json!({"object": "list"})) }),
    ))
    .await;
    let good = spawn_upstream(embeddings_echo()).await;
    save_provider(&state, "down", &down.url(), Protocol::OpenAi).await;
    save_provider(&state, "invalid", &invalid.url(), Protocol::OpenAi).await;
    save_provider(&state, "good", &good.url(), Protocol::OpenAi).await;
    combo(
        &state,
        "vectors",
        Category::Embedding,
        &[
            ("down", "e1"),
            ("invalid", "e2"),
            ("good", "text-embedding-3-small"),
        ],
    )
    .await;

    let response = send(
        &state,
        post_json(
            "/v1/embeddings",
            &json!({
                "model": "combo/vectors",
                "input": ["alpha", "beta"],
                "dimensions": 2,
                "encoding_format": "float",
            }),
        ),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let body = json_body(response).await;
    assert_eq!(body["data"][0]["embedding"], json!([0.25, -0.5]));
    assert_eq!(body["upstream_saw"]["model"], "text-embedding-3-small");
    assert_eq!(body["upstream_saw"]["input"], json!(["alpha", "beta"]));
    assert_eq!(body["upstream_saw"]["dimensions"], 2);
    assert_eq!((down.hits.count(), invalid.hits.count()), (1, 1));
    assert_eq!(history(&state).await, ("success".into(), Some(4), None));

    // An embedding combo never serves chat, and a text combo never serves embeddings.
    let chat = send(
        &state,
        post_json(
            "/v1/chat/completions",
            &json!({"model": "combo/vectors", "messages": [{"role": "user", "content": "x"}]}),
        ),
    )
    .await;
    assert_eq!(chat.status(), StatusCode::BAD_REQUEST);
    combo(&state, "chat", Category::Text, &[("good", "gpt-model")]).await;
    let wrong = send(
        &state,
        post_json(
            "/v1/embeddings",
            &json!({"model": "combo/chat", "input": "x"}),
        ),
    )
    .await;
    assert_eq!(wrong.status(), StatusCode::BAD_REQUEST);
    assert_eq!(good.hits.count(), 1);

    for invalid_body in [
        json!({"model": "combo/vectors"}),
        json!({"model": "combo/vectors", "input": null}),
        json!({"model": "combo/vectors", "input": "x", "stream": true}),
    ] {
        let response = send(&state, post_json("/v1/embeddings", &invalid_body)).await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{invalid_body}");
    }
    assert_eq!(good.hits.count(), 1);
}

#[tokio::test]
async fn embedding_combos_accept_only_openai_protocol_connections() {
    let state = test_state().await;
    save_provider(
        &state,
        "claude",
        "https://api.anthropic.com/v1",
        Protocol::Anthropic,
    )
    .await;
    save_provider(
        &state,
        "openai",
        "https://api.openai.com/v1",
        Protocol::OpenAi,
    )
    .await;
    let combo = combos::create_typed(
        &state.db,
        "vectors",
        Category::Embedding,
        Strategy::Priority,
        Default::default(),
    )
    .await
    .unwrap();
    assert!(
        combos::add_step(&state.db, &combo.id, "claude", "voyage-model")
            .await
            .is_err()
    );
    combos::add_step(&state.db, &combo.id, "openai", "text-embedding-3-large")
        .await
        .unwrap();
    // A model documented as a chat model cannot be declared an embedding model.
    assert!(
        combos::add_step(&state.db, &combo.id, "openai", "gpt-4o")
            .await
            .is_err()
    );
}
