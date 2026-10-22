//! `/v1/messages` end to end against local upstream mocks: Anthropic-only
//! eligibility, unchanged passthrough with only `model` replaced, forwarded
//! protocol headers, `x-api-key` authentication, fallback on a first-event
//! error before commitment, Anthropic-shaped gateway errors, and usage with
//! cache tokens recorded in history.

mod common;
use common::{body_bytes, json_body, spawn_upstream};

use axum::{
    Json, Router,
    body::Body,
    http::{HeaderMap, Request, StatusCode},
    response::{IntoResponse, Response},
    routing::post,
};
use base64::{Engine, engine::general_purpose::STANDARD};
use secrecy::SecretString;
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};
use tower::ServiceExt;

use sparkrouter::{
    app::{self, AppState},
    config::{Config, DashboardLogin},
    crypto::Crypto,
    db::{
        combos::{self, Category, Strategy},
        providers as provider_db,
    },
    providers::Protocol,
};

async fn test_state(api_key: Option<&str>) -> AppState {
    AppState::from_config(Config {
        database_url: "sqlite::memory:".into(),
        bind: "127.0.0.1:0".parse().unwrap(),
        crypto: Crypto::from_base64(&SecretString::from(STANDARD.encode([42u8; 32]))).unwrap(),
        api_key: api_key.map(SecretString::from),
        dashboard_login: DashboardLogin::Disabled,
        dashboard_secure_cookie: false,
        metrics_token: None,
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

async fn combo(state: &AppState, name: &str, steps: &[(&str, &str)]) {
    let combo = combos::create_typed(
        &state.db,
        name,
        Category::Text,
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

fn messages(body: Value, headers: &[(&str, &str)]) -> Request<Body> {
    let mut request = Request::builder()
        .method("POST")
        .uri("/v1/messages")
        .header("content-type", "application/json");
    for (name, value) in headers {
        request = request.header(*name, *value);
    }
    request.body(Body::from(body.to_string())).unwrap()
}

type Seen = Arc<Mutex<Vec<(HeaderMap, Value)>>>;

/// Answers like Anthropic: a message, or a stream ending with `message_stop`.
fn anthropic(seen: Seen) -> Router {
    Router::new().route(
        "/messages",
        post(move |headers: HeaderMap, Json(body): Json<Value>| {
            let seen = seen.clone();
            async move {
                seen.lock().unwrap().push((headers, body.clone()));
                if body["stream"] == true {
                    let wire = concat!(
                        "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_s\",\"usage\":{\"input_tokens\":3,\"cache_read_input_tokens\":7,\"output_tokens\":1}}}\n\n",
                        "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"hi\"}}\n\n",
                        "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":5}}\n\n",
                        "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
                    );
                    return ([("content-type", "text/event-stream")], wire).into_response();
                }
                Json(json!({"id":"msg_1", "type":"message", "role":"assistant", "model":body["model"],
                    "content":[{"type":"thinking", "thinking":"", "signature":"s"}, {"type":"text", "text":"hello"}],
                    "stop_reason":"end_turn", "native_extension":{"kept":true},
                    "usage":{"input_tokens":4, "cache_creation_input_tokens":2, "cache_read_input_tokens":10, "output_tokens":6}}))
                .into_response()
            }
        }),
    )
}

#[tokio::test]
async fn messages_pass_through_to_anthropic_connections_only() {
    let state = test_state(Some("environment-key")).await;
    let seen = Seen::default();
    let claude = spawn_upstream(anthropic(seen.clone())).await;
    let openai = spawn_upstream(Router::new().route(
        "/chat/completions",
        post(|| async { Json(json!({"choices":[]})) }),
    ))
    .await;
    save_provider(&state, "openai", &openai.url(), Protocol::OpenAi).await;
    save_provider(&state, "claude", &claude.url(), Protocol::Anthropic).await;
    combo(
        &state,
        "coding",
        &[("openai", "gpt"), ("claude", "claude-up")],
    )
    .await;

    let request = json!({"model":"combo/coding", "max_tokens":64, "system":"Be brief",
        "messages":[{"role":"user", "content":[{"type":"text", "text":"hi", "cache_control":{"type":"ephemeral"}}]}],
        "thinking":{"type":"adaptive"}, "unknown_field":[1, 2]});
    let response = app::router(state.clone())
        .oneshot(messages(
            request.clone(),
            &[
                ("x-api-key", "environment-key"),
                ("anthropic-version", "2026-10-21"),
                ("anthropic-beta", "beta-one, beta-two"),
                ("anthropic-beta", "beta-three"),
            ],
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = json_body(response).await;
    assert_eq!(body["model"], "claude-up");
    assert_eq!(body["native_extension"], json!({"kept":true}));
    assert_eq!(body["content"][0]["type"], "thinking");
    // The OpenAI-protocol step is never eligible for native Messages.
    assert_eq!(openai.hits.count(), 0);
    let (headers, upstream) = seen.lock().unwrap().remove(0);
    let mut expected = request.clone();
    expected["model"] = json!("claude-up");
    assert_eq!(upstream, expected);
    assert_eq!(headers["x-api-key"], "sk-claude");
    assert_eq!(headers["anthropic-version"], "2026-10-21");
    assert_eq!(headers["anthropic-beta"], "beta-one,beta-two,beta-three");
    assert!(headers.get("authorization").is_none());

    // Without client headers the adapter's default version is sent.
    let response = app::router(state.clone())
        .oneshot(messages(
            json!({"model":"claude/claude-direct", "max_tokens":8, "messages":[]}),
            &[("authorization", "Bearer environment-key")],
        ))
        .await
        .unwrap();
    // Never-refreshed connections are not direct targets.
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    let response = app::router(state.clone())
        .oneshot(messages(
            json!({"model":"combo/coding", "max_tokens":8, "messages":[]}),
            &[("authorization", "Bearer environment-key")],
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let (headers, _) = seen.lock().unwrap().remove(0);
    assert_eq!(headers["anthropic-version"], "2023-06-01");
    assert!(headers.get("anthropic-beta").is_none());

    state.flush_history().await;
    let (input, output, cached): (i64, i64, i64) = sqlx::query_as(
        "SELECT input_tokens, output_tokens, cached_tokens FROM request_history
         ORDER BY created_at, rowid LIMIT 1",
    )
    .fetch_one(&state.db)
    .await
    .unwrap();
    assert_eq!((input, output, cached), (16, 6, 10));
}

#[tokio::test]
async fn streams_fall_back_on_a_first_event_error_and_end_at_message_stop() {
    let state = test_state(None).await;
    let overloaded = spawn_upstream(Router::new().route(
        "/messages",
        post(|| async {
            (
                [("content-type", "text/event-stream")],
                "event: error\ndata: {\"type\":\"error\",\"error\":{\"type\":\"overloaded_error\",\"message\":\"secret detail\"}}\n\n",
            )
        }),
    ))
    .await;
    let seen = Seen::default();
    let claude = spawn_upstream(anthropic(seen.clone())).await;
    save_provider(&state, "busy", &overloaded.url(), Protocol::Anthropic).await;
    save_provider(&state, "claude", &claude.url(), Protocol::Anthropic).await;
    combo(&state, "coding", &[("busy", "a"), ("claude", "b")]).await;
    let response = app::router(state.clone())
        .oneshot(messages(
            json!({"model":"combo/coding", "max_tokens":8, "stream":true,
                "messages":[{"role":"user", "content":"hi"}]}),
            &[],
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let wire = String::from_utf8(body_bytes(response).await.to_vec()).unwrap();
    assert!(wire.starts_with("event: message_start\n"));
    assert!(wire.ends_with("data: {\"type\":\"message_stop\"}\n\n"));
    assert!(!wire.contains("secret detail"));
    assert_eq!(overloaded.hits.count(), 1);
    assert_eq!(claude.hits.count(), 1);
    state.flush_history().await;
    let (outcome, input, output): (String, i64, i64) =
        sqlx::query_as("SELECT outcome, input_tokens, output_tokens FROM request_history LIMIT 1")
            .fetch_one(&state.db)
            .await
            .unwrap();
    assert_eq!((outcome.as_str(), input, output), ("success", 10, 5));

    // An upstream EOF before message_stop is an interrupted stream.
    let truncated = spawn_upstream(Router::new().route(
        "/messages",
        post(|| async {
            (
                [("content-type", "text/event-stream")],
                "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"m\"}}\n\n",
            )
        }),
    ))
    .await;
    save_provider(&state, "short", &truncated.url(), Protocol::Anthropic).await;
    combo(&state, "short", &[("short", "a"), ("claude", "b")]).await;
    let response = app::router(state.clone())
        .oneshot(messages(
            json!({"model":"combo/short", "max_tokens":8, "stream":true, "messages":[]}),
            &[],
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    use http_body_util::BodyExt;
    let collected = response.into_body().collect().await;
    assert!(
        collected.is_err(),
        "a truncated stream must not end cleanly"
    );
    // Committed: no fallback to the healthy connection.
    assert_eq!(claude.hits.count(), 1);
}

#[tokio::test]
async fn gateway_errors_use_the_anthropic_error_shape() {
    let state = test_state(Some("environment-key")).await;
    let seen = Seen::default();
    let claude = spawn_upstream(anthropic(seen)).await;
    save_provider(&state, "claude", &claude.url(), Protocol::Anthropic).await;
    combo(&state, "coding", &[("claude", "b")]).await;
    let check = |response: Response, status: StatusCode, kind: &'static str| async move {
        assert_eq!(response.status(), status);
        let body = json_body(response).await;
        assert_eq!(body["type"], "error");
        assert_eq!(body["error"]["type"], kind, "{body}");
        assert!(body["error"]["message"].is_string());
    };
    let router = app::router(state.clone());
    let unauthenticated = router
        .clone()
        .oneshot(messages(
            json!({"model":"combo/coding", "messages":[]}),
            &[("x-api-key", "wrong")],
        ))
        .await
        .unwrap();
    check(
        unauthenticated,
        StatusCode::UNAUTHORIZED,
        "authentication_error",
    )
    .await;
    for (body, headers, status, kind) in [
        (
            json!({"model":"combo/coding"}),
            vec![],
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
        ),
        (
            json!({"model":"combo/missing", "messages":[]}),
            vec![],
            StatusCode::NOT_FOUND,
            "not_found_error",
        ),
        (
            json!({"model":"combo/coding", "messages":[]}),
            vec![("anthropic-version", "latest")],
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
        ),
        (
            json!({"model":"combo/coding", "messages":[]}),
            vec![("anthropic-beta", "bad flag")],
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
        ),
    ] {
        let mut headers = headers;
        headers.push(("x-api-key", "environment-key"));
        let response = router
            .clone()
            .oneshot(messages(body, &headers))
            .await
            .unwrap();
        check(response, status, kind).await;
    }
    assert_eq!(claude.hits.count(), 0);
    // Other endpoints keep the OpenAI error shape.
    let chat = router
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/chat/completions")
                .header("content-type", "application/json")
                .body(Body::from("{}"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(chat.status(), StatusCode::UNAUTHORIZED);
    let body = json_body(chat).await;
    assert_eq!(body["error"]["type"], "authentication_error");
    assert!(body.get("type").is_none());
}
