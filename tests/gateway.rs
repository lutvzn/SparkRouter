//! End-to-end gateway tests.
//!
//! These drive `app::router(state).oneshot(request)` against real local Axum
//! HTTP servers acting as upstream provider mocks (bound to 127.0.0.1:0).
//! Mock servers are stopped deterministically through RAII task abortion, and
//! streaming sequences are ordered with channels rather than sleeps.

use std::{
    net::SocketAddr,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use axum::{
    Json, Router,
    body::Body,
    extract::Request,
    http::{HeaderMap, Method, StatusCode, header},
    middleware::Next,
    response::{IntoResponse, Response},
    routing::{get, post},
};
use base64::{Engine, engine::general_purpose::STANDARD};
use bytes::Bytes;
use futures_util::StreamExt;
use http_body_util::BodyExt;
use secrecy::{ExposeSecret, SecretString};
use serde_json::{Value, json};
use tower::ServiceExt;

use sparkrouter::{
    app::{self, AppState},
    config::Config,
    crypto::Crypto,
    db::{
        combos::{self, Strategy},
        connection_models::{self, StoredModel},
        providers as provider_db,
    },
    providers::Protocol,
};

type BoxError = Box<dyn std::error::Error + Send + Sync>;
type TestResult = Result<(), BoxError>;

const MASTER_KEY: &[u8; 32] = &[42; 32];

// ---------------------------------------------------------------------------
// Test state fixtures
// ---------------------------------------------------------------------------

fn test_config(database_url: &str) -> Config {
    Config {
        database_url: database_url.to_owned(),
        bind: "127.0.0.1:0".parse().unwrap(),
        crypto: Crypto::from_base64(&SecretString::from(STANDARD.encode(MASTER_KEY))).unwrap(),
        api_key: None,
        dashboard_password: None,
        dashboard_secure_cookie: false,
    }
}

async fn test_state() -> AppState {
    AppState::from_config(test_config("sqlite::memory:"))
        .await
        .unwrap()
}

async fn save_provider(
    state: &AppState,
    id: &str,
    base_url: &str,
    protocol: Protocol,
    enabled: bool,
    api_key: &str,
) -> Result<provider_db::Provider, BoxError> {
    let provider = provider_db::Provider {
        id: id.to_owned(),
        name: format!("{id} display"),
        base_url: base_url.to_owned(),
        protocol,
        encrypted_api_key: state
            .crypto
            .encrypt_api_key(id, &SecretString::from(api_key.to_owned()))?,
        enabled,
    };
    provider_db::save(&state.db, &provider).await?;
    Ok(provider)
}

async fn combo_with_steps(state: &AppState, steps: &[(&str, &str)]) -> TestResult {
    combos::create(&state.db, "coding").await?;
    let combo = combos::by_name(&state.db, "coding").await?;
    for (provider_id, model) in steps {
        combos::add_step(&state.db, &combo.id, provider_id, model).await?;
    }
    Ok(())
}

/// Create a named combo with ordered steps and a routing strategy, returning
/// the stored combo so tests can address it by id.
async fn combo_with_strategy(
    state: &AppState,
    name: &str,
    steps: &[(&str, &str)],
    strategy: Strategy,
) -> Result<combos::Combo, BoxError> {
    let combo = combos::create_with_strategy(&state.db, name, strategy).await?;
    for (provider_id, model) in steps {
        combos::add_step(&state.db, &combo.id, provider_id, model).await?;
    }
    Ok(combo)
}

/// Send one chat completion to `combo/<name>` and assert it succeeded and was
/// served by the step configured with `expected_model`.
async fn assert_combo_reply(state: &AppState, name: &str, expected_model: &str) -> TestResult {
    let body = json!({
        "model": format!("combo/{name}"),
        "messages": [{"role": "user", "content": "hello"}],
    });
    let response = send(state, chat_request(&body)).await;
    assert_eq!(
        response.status(),
        StatusCode::OK,
        "combo {name} expected a reply from {expected_model}"
    );
    let payload = json_body(response).await;
    assert_eq!(
        payload["model"], expected_model,
        "combo {name} was served by the wrong step"
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// Mock upstream servers (RAII: dropping aborts the server task)
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct Hits(Arc<AtomicUsize>);

impl Hits {
    fn new() -> Self {
        Self(Arc::new(AtomicUsize::new(0)))
    }

    fn count(&self) -> usize {
        self.0.load(Ordering::SeqCst)
    }
}

struct MockUpstream {
    addr: SocketAddr,
    hits: Hits,
    task: tokio::task::JoinHandle<()>,
}

impl MockUpstream {
    fn url(&self) -> String {
        format!("http://{}", self.addr)
    }
}

impl Drop for MockUpstream {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn spawn_upstream(router: Router) -> MockUpstream {
    let hits = Hits::new();
    let counted = hits.clone();
    let app = router.layer(axum::middleware::from_fn(
        move |request: Request, next: Next| {
            let counted = counted.clone();
            async move {
                counted.0.fetch_add(1, Ordering::SeqCst);
                next.run(request).await
            }
        },
    ));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind mock upstream");
    let addr = listener.local_addr().expect("mock upstream local address");
    let task = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    MockUpstream { addr, hits, task }
}

/// OpenAI-compatible upstream that echoes the exact request it received
/// (including the model it was given) inside a minimal chat.completion reply.
fn openai_echo_router(reply: &'static str) -> Router {
    Router::new().route(
        "/chat/completions",
        post(move |headers: HeaderMap, body: Bytes| async move {
            let received: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
            Json(json!({
                "id": "chatcmpl-mock",
                "object": "chat.completion",
                "created": 1_700_000_000i64,
                "model": received.get("model").cloned().unwrap_or(Value::Null),
                "choices": [{
                    "index": 0,
                    "message": {"role": "assistant", "content": reply},
                    "finish_reason": "stop",
                }],
                "usage": {"prompt_tokens": 4, "completion_tokens": 2, "total_tokens": 6},
                "provider_extension": {"keep": true},
                "upstream_auth": headers
                    .get(header::AUTHORIZATION)
                    .and_then(|value| value.to_str().ok())
                    .unwrap_or(""),
                "upstream_saw": received,
            }))
        }),
    )
}

/// Upstream that always answers with the given HTTP status and an error body.
fn error_status_router(status: StatusCode) -> Router {
    Router::new().route(
        "/chat/completions",
        post(move || async move {
            let code = status.as_u16();
            (
                status,
                [(header::CONTENT_TYPE, "application/json")],
                format!("{{\"error\":{{\"message\":\"mock status {code}\",\"type\":\"mock_error\",\"code\":{code}}}}}"),
            )
        }),
    )
}

/// Upstream streaming a deterministic raw SSE body, chunk by chunk.
fn sse_router(chunks: Vec<&'static str>) -> Router {
    Router::new().route(
        "/chat/completions",
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

/// Upstream that streams one SSE frame, then waits for the test's go-ahead
/// before failing mid-body. Ordering is channel-driven, never sleep-driven.
fn sse_disconnect_router() -> (Router, tokio::sync::mpsc::Sender<()>) {
    let (send, receive) = tokio::sync::mpsc::channel::<()>(1);
    let receive = Arc::new(tokio::sync::Mutex::new(Some(receive)));
    let router = Router::new().route(
        "/chat/completions",
        post(move || {
            let receive = receive.clone();
            async move {
                // Box the alternatives: each `stream!` expansion is its own type.
                let stream: std::pin::Pin<
                    Box<dyn futures_util::Stream<Item = Result<Bytes, std::io::Error>> + Send>,
                > = match receive.lock().await.take() {
                    Some(mut receive) => Box::pin(async_stream::stream! {
                        yield Ok::<Bytes, std::io::Error>(
                            Bytes::from_static(b"data: {\"first\":true}\n\n"),
                        );
                        // Park until the test has read the first frame above.
                        let _ = receive.recv().await;
                        yield Err(std::io::Error::other("mock upstream broke mid-stream"));
                    }),
                    None => Box::pin(async_stream::stream! {
                        yield Ok::<Bytes, std::io::Error>(Bytes::from_static(b"data: unexpected-second-request\n\n"));
                    }),
                };
                ([(header::CONTENT_TYPE, "text/event-stream")], Body::from_stream(stream))
            }
        }),
    );
    (router, send)
}

// ---------------------------------------------------------------------------
// Request/response helpers
// ---------------------------------------------------------------------------

fn chat_request(body: &Value) -> Request<Body> {
    Request::builder()
        .method(Method::POST)
        .uri("/v1/chat/completions")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(body.to_string()))
        .unwrap()
}

fn models_request(bearer: Option<&str>) -> Request<Body> {
    let mut builder = Request::builder().method(Method::GET).uri("/v1/models");
    if let Some(bearer) = bearer {
        builder = builder.header(header::AUTHORIZATION, format!("Bearer {bearer}"));
    }
    builder.body(Body::empty()).unwrap()
}

fn dashboard_edit_request(provider_id: &str, form: String) -> Request<Body> {
    Request::builder()
        .method(Method::POST)
        .uri(format!("/providers/{provider_id}"))
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .header(header::HOST, "127.0.0.1:8080")
        .header(header::ORIGIN, "http://127.0.0.1:8080")
        .body(Body::from(form))
        .unwrap()
}

/// Minimal application/x-www-form-urlencoded serializer (space -> `+`).
fn form_urlencoded(pairs: &[(&str, &str)]) -> String {
    fn encode(value: &str) -> String {
        let mut out = String::new();
        for byte in value.bytes() {
            match byte {
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                    out.push(byte as char);
                }
                b' ' => out.push('+'),
                _ => out.push_str(&format!("%{byte:02X}")),
            }
        }
        out
    }
    pairs
        .iter()
        .map(|(key, value)| format!("{}={}", encode(key), encode(value)))
        .collect::<Vec<_>>()
        .join("&")
}

async fn body_bytes(response: Response) -> Bytes {
    response
        .into_body()
        .collect()
        .await
        .expect("read gateway response body")
        .to_bytes()
}

async fn json_body(response: Response) -> Value {
    let bytes = body_bytes(response).await;
    serde_json::from_slice(&bytes).unwrap_or_else(|error| {
        panic!(
            "gateway response is not JSON ({error}): {}",
            String::from_utf8_lossy(&bytes)
        )
    })
}

async fn send(state: &AppState, request: Request<Body>) -> Response {
    app::router(state.clone())
        .oneshot(request)
        .await
        .expect("router call is infallible")
}

// ---------------------------------------------------------------------------
// Fallback chain behavior
// ---------------------------------------------------------------------------

#[tokio::test]
async fn retryable_first_upstream_falls_back_and_preserves_request() -> TestResult {
    let state = test_state().await;
    let first = spawn_upstream(error_status_router(StatusCode::TOO_MANY_REQUESTS)).await;
    let second = spawn_upstream(openai_echo_router("mock reply")).await;

    save_provider(
        &state,
        "prov-first",
        &first.url(),
        Protocol::OpenAi,
        true,
        "sk-first",
    )
    .await?;
    save_provider(
        &state,
        "prov-second",
        &second.url(),
        Protocol::OpenAi,
        true,
        "sk-second-key",
    )
    .await?;
    combo_with_steps(
        &state,
        &[
            ("prov-first", "first-model"),
            ("prov-second", "second-model"),
        ],
    )
    .await?;

    let body = json!({
        "model": "combo/coding",
        "messages": [{"role": "user", "content": "hello"}],
        "stream": false,
        "temperature": 0.25,
        "metadata": {"trace_id": "trace-1234"},
        "user_extension": {"nested": {"keep": [1, 2, 3]}},
    });
    let response = send(&state, chat_request(&body)).await;
    assert_eq!(response.status(), StatusCode::OK);
    let payload = json_body(response).await;

    // The reply comes from the second (healthy) upstream.
    assert_eq!(payload["id"], "chatcmpl-mock");
    assert_eq!(payload["choices"][0]["message"]["content"], "mock reply");
    assert_eq!(payload["provider_extension"]["keep"], true);

    // The combo model was replaced by the step's real model...
    assert_eq!(payload["model"], "second-model");
    let seen = &payload["upstream_saw"];
    assert_eq!(seen["model"], "second-model");
    // ...while every unknown nested field survived untouched.
    assert_eq!(
        seen["messages"],
        json!([{"role": "user", "content": "hello"}])
    );
    assert_eq!(seen["temperature"], 0.25);
    assert_eq!(seen["metadata"]["trace_id"], "trace-1234");
    assert_eq!(seen["user_extension"]["nested"]["keep"], json!([1, 2, 3]));

    // The stored provider credential reached the upstream as a bearer token.
    assert_eq!(payload["upstream_auth"], "Bearer sk-second-key");

    assert_eq!(first.hits.count(), 1);
    assert_eq!(second.hits.count(), 1);
    Ok(())
}

#[tokio::test]
async fn client_error_stops_the_chain() -> TestResult {
    let state = test_state().await;
    let first = spawn_upstream(error_status_router(StatusCode::BAD_REQUEST)).await;
    let second = spawn_upstream(openai_echo_router("should never be used")).await;

    save_provider(
        &state,
        "prov-bad",
        &first.url(),
        Protocol::OpenAi,
        true,
        "sk-bad",
    )
    .await?;
    save_provider(
        &state,
        "prov-good",
        &second.url(),
        Protocol::OpenAi,
        true,
        "sk-good",
    )
    .await?;
    combo_with_steps(&state, &[("prov-bad", "model-a"), ("prov-good", "model-b")]).await?;

    let body = json!({"model": "combo/coding", "messages": [{"role": "user", "content": "hi"}]});
    let response = send(&state, chat_request(&body)).await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let payload = json_body(response).await;
    assert_eq!(payload["error"]["code"], 400);

    // A 400 is permanent: the second upstream must not be contacted.
    assert_eq!(first.hits.count(), 1);
    assert_eq!(second.hits.count(), 0);
    Ok(())
}

#[tokio::test]
async fn exhausted_combo_returns_bad_gateway() -> TestResult {
    let state = test_state().await;
    let only = spawn_upstream(error_status_router(StatusCode::SERVICE_UNAVAILABLE)).await;

    save_provider(
        &state,
        "prov-down",
        &only.url(),
        Protocol::OpenAi,
        true,
        "sk-down",
    )
    .await?;
    combo_with_steps(&state, &[("prov-down", "model-a")]).await?;

    let body = json!({"model": "combo/coding", "messages": [{"role": "user", "content": "hi"}]});
    let response = send(&state, chat_request(&body)).await;
    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    let payload = json_body(response).await;
    assert_eq!(payload["error"]["code"], 502);
    assert_eq!(only.hits.count(), 1);
    Ok(())
}

#[tokio::test]
async fn disabled_provider_is_skipped() -> TestResult {
    let state = test_state().await;
    let disabled = spawn_upstream(openai_echo_router("disabled reply")).await;
    let enabled = spawn_upstream(openai_echo_router("enabled reply")).await;

    save_provider(
        &state,
        "prov-off",
        &disabled.url(),
        Protocol::OpenAi,
        false,
        "sk-off",
    )
    .await?;
    save_provider(
        &state,
        "prov-on",
        &enabled.url(),
        Protocol::OpenAi,
        true,
        "sk-on",
    )
    .await?;
    combo_with_steps(
        &state,
        &[("prov-off", "model-off"), ("prov-on", "model-on")],
    )
    .await?;

    let body = json!({"model": "combo/coding", "messages": [{"role": "user", "content": "hi"}]});
    let response = send(&state, chat_request(&body)).await;
    assert_eq!(response.status(), StatusCode::OK);
    let payload = json_body(response).await;
    assert_eq!(payload["choices"][0]["message"]["content"], "enabled reply");
    assert_eq!(payload["model"], "model-on");

    // The disabled step's server must never be contacted, even though it is
    // healthy and would happily answer.
    assert_eq!(disabled.hits.count(), 0);
    assert_eq!(enabled.hits.count(), 1);
    Ok(())
}

// ---------------------------------------------------------------------------
// Routing strategies: round robin and random step selection
// ---------------------------------------------------------------------------

#[tokio::test]
async fn round_robin_rotates_the_starting_step_once_per_request() -> TestResult {
    let state = test_state().await;
    let first = spawn_upstream(openai_echo_router("first reply")).await;
    let second = spawn_upstream(openai_echo_router("second reply")).await;
    let third = spawn_upstream(openai_echo_router("third reply")).await;

    save_provider(
        &state,
        "prov-first",
        &first.url(),
        Protocol::OpenAi,
        true,
        "sk-first",
    )
    .await?;
    save_provider(
        &state,
        "prov-second",
        &second.url(),
        Protocol::OpenAi,
        true,
        "sk-second",
    )
    .await?;
    save_provider(
        &state,
        "prov-third",
        &third.url(),
        Protocol::OpenAi,
        true,
        "sk-third",
    )
    .await?;
    combo_with_strategy(
        &state,
        "coding",
        &[
            ("prov-first", "model-first"),
            ("prov-second", "model-second"),
            ("prov-third", "model-third"),
        ],
        Strategy::RoundRobin,
    )
    .await?;

    // Each `send` builds a fresh router over a cloned AppState, so rotation
    // surviving across requests also proves the cursor is shared state behind
    // those clones rather than per-request data.
    assert_combo_reply(&state, "coding", "model-first").await?;
    assert_combo_reply(&state, "coding", "model-second").await?;
    assert_combo_reply(&state, "coding", "model-third").await?;
    assert_combo_reply(&state, "coding", "model-first").await?;

    assert_eq!(first.hits.count(), 2);
    assert_eq!(second.hits.count(), 1);
    assert_eq!(third.hits.count(), 1);
    Ok(())
}

#[tokio::test]
async fn round_robin_skips_disabled_steps_without_taking_rotation_turns() -> TestResult {
    let state = test_state().await;
    let off_first = spawn_upstream(openai_echo_router("off first reply")).await;
    let on_first = spawn_upstream(openai_echo_router("on first reply")).await;
    let off_middle = spawn_upstream(openai_echo_router("off middle reply")).await;
    let on_last = spawn_upstream(openai_echo_router("on last reply")).await;

    save_provider(
        &state,
        "prov-off-first",
        &off_first.url(),
        Protocol::OpenAi,
        false,
        "sk-1",
    )
    .await?;
    save_provider(
        &state,
        "prov-on-first",
        &on_first.url(),
        Protocol::OpenAi,
        true,
        "sk-2",
    )
    .await?;
    save_provider(
        &state,
        "prov-off-middle",
        &off_middle.url(),
        Protocol::OpenAi,
        false,
        "sk-3",
    )
    .await?;
    save_provider(
        &state,
        "prov-on-last",
        &on_last.url(),
        Protocol::OpenAi,
        true,
        "sk-4",
    )
    .await?;
    combo_with_strategy(
        &state,
        "coding",
        &[
            ("prov-off-first", "model-off-first"),
            ("prov-on-first", "model-on-first"),
            ("prov-off-middle", "model-off-middle"),
            ("prov-on-last", "model-on-last"),
        ],
        Strategy::RoundRobin,
    )
    .await?;

    // Disabled steps neither receive traffic nor consume a rotation turn:
    // the two eligible steps simply alternate.
    assert_combo_reply(&state, "coding", "model-on-first").await?;
    assert_combo_reply(&state, "coding", "model-on-last").await?;
    assert_combo_reply(&state, "coding", "model-on-first").await?;
    assert_combo_reply(&state, "coding", "model-on-last").await?;

    assert_eq!(
        off_first.hits.count(),
        0,
        "disabled first step must never be contacted"
    );
    assert_eq!(on_first.hits.count(), 2);
    assert_eq!(
        off_middle.hits.count(),
        0,
        "disabled middle step must never be contacted"
    );
    assert_eq!(on_last.hits.count(), 2);

    // With every step disabled there is nothing left to rotate to.
    let dead = spawn_upstream(openai_echo_router("never used")).await;
    save_provider(
        &state,
        "prov-dead-a",
        &dead.url(),
        Protocol::OpenAi,
        false,
        "sk-dead-a",
    )
    .await?;
    save_provider(
        &state,
        "prov-dead-b",
        &dead.url(),
        Protocol::OpenAi,
        false,
        "sk-dead-b",
    )
    .await?;
    combo_with_strategy(
        &state,
        "alldown",
        &[
            ("prov-dead-a", "model-dead-a"),
            ("prov-dead-b", "model-dead-b"),
        ],
        Strategy::RoundRobin,
    )
    .await?;

    let body = json!({"model": "combo/alldown", "messages": [{"role": "user", "content": "hi"}]});
    let response = send(&state, chat_request(&body)).await;
    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    let payload = json_body(response).await;
    assert_eq!(payload["error"]["code"], 502);
    assert_eq!(dead.hits.count(), 0);
    Ok(())
}

#[tokio::test]
async fn round_robin_falls_back_cyclically_and_advances_once_per_request() -> TestResult {
    let state = test_state().await;
    let flaky = spawn_upstream(error_status_router(StatusCode::TOO_MANY_REQUESTS)).await;
    let second = spawn_upstream(openai_echo_router("second reply")).await;
    let third = spawn_upstream(openai_echo_router("third reply")).await;

    save_provider(
        &state,
        "prov-flaky",
        &flaky.url(),
        Protocol::OpenAi,
        true,
        "sk-flaky",
    )
    .await?;
    save_provider(
        &state,
        "prov-second",
        &second.url(),
        Protocol::OpenAi,
        true,
        "sk-second",
    )
    .await?;
    save_provider(
        &state,
        "prov-third",
        &third.url(),
        Protocol::OpenAi,
        true,
        "sk-third",
    )
    .await?;
    combo_with_strategy(
        &state,
        "coding",
        &[
            ("prov-flaky", "model-flaky"),
            ("prov-second", "model-second"),
            ("prov-third", "model-third"),
        ],
        Strategy::RoundRobin,
    )
    .await?;

    // Request 1 starts at the flaky step; 429 is retryable, so the cyclic
    // in-request fallback moves on to the second step, which answers.
    assert_combo_reply(&state, "coding", "model-second").await?;
    assert_eq!(flaky.hits.count(), 1);
    assert_eq!(second.hits.count(), 1);
    assert_eq!(third.hits.count(), 0);

    // Request 2 must start at the second step directly: the cursor advanced
    // exactly once for request 1 as a whole, not once per upstream attempt.
    assert_combo_reply(&state, "coding", "model-second").await?;
    assert_eq!(flaky.hits.count(), 1);
    assert_eq!(second.hits.count(), 2);
    assert_eq!(third.hits.count(), 0);

    // Request 3 rotates on to the third step.
    assert_combo_reply(&state, "coding", "model-third").await?;
    assert_eq!(flaky.hits.count(), 1);
    assert_eq!(second.hits.count(), 2);
    assert_eq!(third.hits.count(), 1);

    // Request 4 wraps around to the flaky step, fails retryably again, and
    // falls back to the second step: fallback stays cyclic per request.
    assert_combo_reply(&state, "coding", "model-second").await?;
    assert_eq!(flaky.hits.count(), 2);
    assert_eq!(second.hits.count(), 3);
    assert_eq!(third.hits.count(), 1);
    Ok(())
}

#[tokio::test]
async fn permanent_error_on_the_selected_round_robin_step_stops_the_chain() -> TestResult {
    let state = test_state().await;
    let first = spawn_upstream(openai_echo_router("first reply")).await;
    let rejecting = spawn_upstream(error_status_router(StatusCode::BAD_REQUEST)).await;
    let last = spawn_upstream(openai_echo_router("last reply")).await;

    save_provider(
        &state,
        "prov-first",
        &first.url(),
        Protocol::OpenAi,
        true,
        "sk-first",
    )
    .await?;
    save_provider(
        &state,
        "prov-rejecting",
        &rejecting.url(),
        Protocol::OpenAi,
        true,
        "sk-bad",
    )
    .await?;
    save_provider(
        &state,
        "prov-last",
        &last.url(),
        Protocol::OpenAi,
        true,
        "sk-last",
    )
    .await?;
    combo_with_strategy(
        &state,
        "coding",
        &[
            ("prov-first", "model-first"),
            ("prov-rejecting", "model-bad"),
            ("prov-last", "model-last"),
        ],
        Strategy::RoundRobin,
    )
    .await?;

    // The first request starts at the healthy first step...
    assert_combo_reply(&state, "coding", "model-first").await?;

    // ...so the second request starts at the rejecting step. Its 400 is
    // permanent: the healthy third step must not be contacted and the client
    // sees the upstream status.
    let body = json!({"model": "combo/coding", "messages": [{"role": "user", "content": "hi"}]});
    let response = send(&state, chat_request(&body)).await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let payload = json_body(response).await;
    assert_eq!(payload["error"]["code"], 400);

    assert_eq!(first.hits.count(), 1);
    assert_eq!(rejecting.hits.count(), 1);
    assert_eq!(
        last.hits.count(),
        0,
        "a permanent error must stop the chain"
    );
    Ok(())
}

#[tokio::test]
async fn round_robin_cursors_are_independent_per_combo() -> TestResult {
    let state = test_state().await;
    let alpha_one = spawn_upstream(openai_echo_router("alpha one")).await;
    let alpha_two = spawn_upstream(openai_echo_router("alpha two")).await;
    let beta_one = spawn_upstream(openai_echo_router("beta one")).await;
    let beta_two = spawn_upstream(openai_echo_router("beta two")).await;

    save_provider(
        &state,
        "prov-alpha-one",
        &alpha_one.url(),
        Protocol::OpenAi,
        true,
        "sk-a1",
    )
    .await?;
    save_provider(
        &state,
        "prov-alpha-two",
        &alpha_two.url(),
        Protocol::OpenAi,
        true,
        "sk-a2",
    )
    .await?;
    save_provider(
        &state,
        "prov-beta-one",
        &beta_one.url(),
        Protocol::OpenAi,
        true,
        "sk-b1",
    )
    .await?;
    save_provider(
        &state,
        "prov-beta-two",
        &beta_two.url(),
        Protocol::OpenAi,
        true,
        "sk-b2",
    )
    .await?;
    combo_with_strategy(
        &state,
        "alpha",
        &[
            ("prov-alpha-one", "alpha-one"),
            ("prov-alpha-two", "alpha-two"),
        ],
        Strategy::RoundRobin,
    )
    .await?;
    combo_with_strategy(
        &state,
        "beta",
        &[("prov-beta-one", "beta-one"), ("prov-beta-two", "beta-two")],
        Strategy::RoundRobin,
    )
    .await?;

    // Interleaved requests over the shared state: each combo rotates through
    // its own steps. A single process-global cursor would make beta's first
    // request start at its second step instead of its first.
    assert_combo_reply(&state, "alpha", "alpha-one").await?;
    assert_combo_reply(&state, "beta", "beta-one").await?;
    assert_combo_reply(&state, "alpha", "alpha-two").await?;
    assert_combo_reply(&state, "beta", "beta-two").await?;
    assert_combo_reply(&state, "alpha", "alpha-one").await?;
    assert_combo_reply(&state, "beta", "beta-one").await?;

    assert_eq!(alpha_one.hits.count(), 2);
    assert_eq!(alpha_two.hits.count(), 1);
    assert_eq!(beta_one.hits.count(), 2);
    assert_eq!(beta_two.hits.count(), 1);
    Ok(())
}

#[tokio::test]
async fn concurrent_round_robin_requests_distribute_exactly_evenly() -> TestResult {
    let state = test_state().await;
    let first = spawn_upstream(openai_echo_router("first reply")).await;
    let second = spawn_upstream(openai_echo_router("second reply")).await;
    let third = spawn_upstream(openai_echo_router("third reply")).await;

    save_provider(
        &state,
        "prov-a",
        &first.url(),
        Protocol::OpenAi,
        true,
        "sk-a",
    )
    .await?;
    save_provider(
        &state,
        "prov-b",
        &second.url(),
        Protocol::OpenAi,
        true,
        "sk-b",
    )
    .await?;
    save_provider(
        &state,
        "prov-c",
        &third.url(),
        Protocol::OpenAi,
        true,
        "sk-c",
    )
    .await?;
    combo_with_strategy(
        &state,
        "coding",
        &[
            ("prov-a", "model-a"),
            ("prov-b", "model-b"),
            ("prov-c", "model-c"),
        ],
        Strategy::RoundRobin,
    )
    .await?;

    // Six requests in flight at once over cloned AppState handles. Completion
    // order is arbitrary, so the served models are sorted before comparing
    // and nothing relies on await ordering. Whatever the interleaving, each
    // request must claim exactly one rotation slot: two per step.
    let body = json!({"model": "combo/coding", "messages": [{"role": "user", "content": "hi"}]});
    let requests = (0..6).map(|_| send(&state, chat_request(&body)));
    let responses = futures_util::future::join_all(requests).await;

    let mut served = Vec::with_capacity(responses.len());
    for response in responses {
        assert_eq!(response.status(), StatusCode::OK);
        let payload = json_body(response).await;
        served.push(
            payload["model"]
                .as_str()
                .expect("echo upstream reports its model")
                .to_owned(),
        );
    }
    served.sort();
    assert_eq!(
        served,
        [
            "model-a", "model-a", "model-b", "model-b", "model-c", "model-c"
        ],
        "six concurrent requests must split two per step"
    );

    assert_eq!(first.hits.count(), 2);
    assert_eq!(second.hits.count(), 2);
    assert_eq!(third.hits.count(), 2);
    Ok(())
}

#[tokio::test]
async fn random_strategy_resolves_the_only_enabled_step() -> TestResult {
    let state = test_state().await;
    let off_a = spawn_upstream(openai_echo_router("off a reply")).await;
    let off_b = spawn_upstream(openai_echo_router("off b reply")).await;
    let on_c = spawn_upstream(openai_echo_router("on c reply")).await;

    save_provider(
        &state,
        "prov-off-a",
        &off_a.url(),
        Protocol::OpenAi,
        false,
        "sk-off-a",
    )
    .await?;
    save_provider(
        &state,
        "prov-off-b",
        &off_b.url(),
        Protocol::OpenAi,
        false,
        "sk-off-b",
    )
    .await?;
    save_provider(
        &state,
        "prov-on-c",
        &on_c.url(),
        Protocol::OpenAi,
        true,
        "sk-on-c",
    )
    .await?;
    combo_with_strategy(
        &state,
        "coding",
        &[
            ("prov-off-a", "model-off-a"),
            ("prov-off-b", "model-off-b"),
            ("prov-on-c", "model-on-c"),
        ],
        Strategy::Random,
    )
    .await?;

    // Uniform selection over the enabled steps only: with a single enabled
    // step the outcome is deterministic no matter where the random start
    // lands.
    assert_combo_reply(&state, "coding", "model-on-c").await?;
    assert_combo_reply(&state, "coding", "model-on-c").await?;
    assert_combo_reply(&state, "coding", "model-on-c").await?;

    assert_eq!(off_a.hits.count(), 0);
    assert_eq!(off_b.hits.count(), 0);
    assert_eq!(on_c.hits.count(), 3);
    Ok(())
}

#[tokio::test]
async fn random_strategy_attempts_each_step_at_most_once() -> TestResult {
    let state = test_state().await;
    let first = spawn_upstream(error_status_router(StatusCode::SERVICE_UNAVAILABLE)).await;
    let second = spawn_upstream(error_status_router(StatusCode::BAD_GATEWAY)).await;

    save_provider(
        &state,
        "prov-down-a",
        &first.url(),
        Protocol::OpenAi,
        true,
        "sk-down-a",
    )
    .await?;
    save_provider(
        &state,
        "prov-down-b",
        &second.url(),
        Protocol::OpenAi,
        true,
        "sk-down-b",
    )
    .await?;
    combo_with_strategy(
        &state,
        "coding",
        &[
            ("prov-down-a", "model-down-a"),
            ("prov-down-b", "model-down-b"),
        ],
        Strategy::Random,
    )
    .await?;

    // Whichever step the random start picks, the other is tried once as the
    // cyclic fallback and neither is ever retried: exactly one hit each.
    let body = json!({"model": "combo/coding", "messages": [{"role": "user", "content": "hi"}]});
    let response = send(&state, chat_request(&body)).await;
    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    let payload = json_body(response).await;
    assert_eq!(payload["error"]["code"], 502);

    assert_eq!(first.hits.count(), 1);
    assert_eq!(second.hits.count(), 1);
    Ok(())
}

#[tokio::test]
async fn random_strategy_sends_each_request_to_exactly_one_healthy_step() -> TestResult {
    let state = test_state().await;
    let first = spawn_upstream(openai_echo_router("first reply")).await;
    let second = spawn_upstream(openai_echo_router("second reply")).await;
    let third = spawn_upstream(openai_echo_router("third reply")).await;

    save_provider(
        &state,
        "prov-a",
        &first.url(),
        Protocol::OpenAi,
        true,
        "sk-a",
    )
    .await?;
    save_provider(
        &state,
        "prov-b",
        &second.url(),
        Protocol::OpenAi,
        true,
        "sk-b",
    )
    .await?;
    save_provider(
        &state,
        "prov-c",
        &third.url(),
        Protocol::OpenAi,
        true,
        "sk-c",
    )
    .await?;
    combo_with_strategy(
        &state,
        "coding",
        &[
            ("prov-a", "model-a"),
            ("prov-b", "model-b"),
            ("prov-c", "model-c"),
        ],
        Strategy::Random,
    )
    .await?;

    // Every response must come from a real configured upstream, and each
    // request must result in exactly one upstream attempt (no duplicated
    // successes). Whether all three upstreams receive traffic is
    // probabilistic under uniform random selection, so it is deliberately
    // not asserted.
    const REQUESTS: usize = 60;
    let body = json!({"model": "combo/coding", "messages": [{"role": "user", "content": "hi"}]});
    let valid = ["model-a", "model-b", "model-c"];
    for index in 0..REQUESTS {
        let response = send(&state, chat_request(&body)).await;
        assert_eq!(response.status(), StatusCode::OK, "random request {index}");
        let payload = json_body(response).await;
        let served = payload["model"].as_str().unwrap_or_default();
        assert!(
            valid.contains(&served),
            "random request {index} was served by unknown model {served}"
        );
    }

    assert_eq!(
        first.hits.count() + second.hits.count() + third.hits.count(),
        REQUESTS,
        "each request must be a single successful upstream attempt"
    );
    Ok(())
}

#[tokio::test]
async fn unknown_strategy_is_rejected_without_changing_routing() -> TestResult {
    let state = test_state().await;
    let first = spawn_upstream(openai_echo_router("first reply")).await;
    let second = spawn_upstream(openai_echo_router("second reply")).await;

    save_provider(
        &state,
        "prov-first",
        &first.url(),
        Protocol::OpenAi,
        true,
        "sk-first",
    )
    .await?;
    save_provider(
        &state,
        "prov-second",
        &second.url(),
        Protocol::OpenAi,
        true,
        "sk-second",
    )
    .await?;
    let combo = combo_with_strategy(
        &state,
        "coding",
        &[
            ("prov-first", "model-first"),
            ("prov-second", "model-second"),
        ],
        Strategy::RoundRobin,
    )
    .await?;

    // Unknown strategies are rejected, not silently treated as Priority.
    assert!(
        sqlx::query("UPDATE combos SET strategy = 'time-paradox' WHERE id = ?")
            .bind(&combo.id)
            .execute(&state.db)
            .await
            .is_err()
    );
    assert_eq!(
        sparkrouter::db::combos::get(&state.db, &combo.id)
            .await?
            .strategy,
        Strategy::RoundRobin
    );

    assert_combo_reply(&state, "coding", "model-first").await?;
    assert_eq!(first.hits.count(), 1);
    assert_eq!(second.hits.count(), 0);
    assert_combo_reply(&state, "coding", "model-second").await?;
    assert_eq!(first.hits.count(), 1);
    assert_eq!(second.hits.count(), 1);
    Ok(())
}

// ---------------------------------------------------------------------------
// Model listing and gateway authentication
// ---------------------------------------------------------------------------

#[tokio::test]
async fn models_endpoint_lists_configured_combos() -> TestResult {
    let state = test_state().await;
    combo_with_steps(&state, &[]).await?;

    // No api_key configured: the gateway API is open by default.
    let response = send(&state, models_request(None)).await;
    assert_eq!(response.status(), StatusCode::OK);
    let payload = json_body(response).await;
    assert_eq!(payload["object"], "list");
    let data = payload["data"].as_array().expect("models data array");
    let listed: Vec<&str> = data
        .iter()
        .filter_map(|model| model["id"].as_str())
        .collect();
    assert!(
        listed.contains(&"combo/coding"),
        "listed models: {listed:?}"
    );
    // Combo models are advertised as owned by the gateway itself.
    assert!(data.iter().all(|model| model["owned_by"] == "sparkrouter"));
    Ok(())
}

async fn remember_model(state: &AppState, provider_id: &str, model_id: &str) -> TestResult {
    connection_models::replace(
        &state.db,
        provider_id,
        &[StoredModel {
            id: model_id.to_owned(),
            name: model_id.to_owned(),
            fields: Vec::new(),
            free: false,
        }],
        false,
    )
    .await?;
    Ok(())
}

#[tokio::test]
async fn direct_model_routes_to_connections_that_list_it() -> TestResult {
    let state = test_state().await;
    let first = spawn_upstream(error_status_router(StatusCode::TOO_MANY_REQUESTS)).await;
    let second = spawn_upstream(openai_echo_router("direct reply")).await;
    let unused = spawn_upstream(openai_echo_router("other model")).await;

    save_provider(
        &state,
        "prov-first",
        &first.url(),
        Protocol::OpenAi,
        true,
        "sk-first",
    )
    .await?;
    save_provider(
        &state,
        "prov-second",
        &second.url(),
        Protocol::OpenAi,
        true,
        "sk-second",
    )
    .await?;
    save_provider(
        &state,
        "prov-other",
        &unused.url(),
        Protocol::OpenAi,
        true,
        "sk-other",
    )
    .await?;
    let group = sparkrouter::db::provider_groups::create_with_slug(
        &state.db,
        "Shared upstream",
        &second.url(),
        &Protocol::OpenAi,
        Some("shared"),
    )
    .await?;
    for provider_id in ["prov-first", "prov-second"] {
        sqlx::query("INSERT INTO provider_group_connections (provider_id, group_id) VALUES (?, ?)")
            .bind(provider_id)
            .bind(&group.id)
            .execute(&state.db)
            .await?;
    }
    remember_model(&state, "prov-first", "openai/gpt-6-luna").await?;
    remember_model(&state, "prov-second", "openai/gpt-6-luna").await?;
    remember_model(&state, "prov-other", "other-model").await?;

    let body = json!({
        "model": " shared/openai/gpt-6-luna ",
        "messages": [{"role": "user", "content": "hello"}],
        "metadata": {"trace_id": "direct-1"},
    });
    let response = send(&state, chat_request(&body)).await;
    assert_eq!(response.status(), StatusCode::OK);
    let payload = json_body(response).await;
    assert_eq!(payload["choices"][0]["message"]["content"], "direct reply");
    assert_eq!(payload["model"], "openai/gpt-6-luna");
    assert_eq!(payload["upstream_saw"]["model"], "openai/gpt-6-luna");
    assert_eq!(payload["upstream_saw"]["metadata"]["trace_id"], "direct-1");
    assert_eq!(payload["upstream_auth"], "Bearer sk-second");
    assert_eq!(first.hits.count(), 1);
    assert_eq!(second.hits.count(), 1);
    assert_eq!(unused.hits.count(), 0);

    let listed = json_body(send(&state, models_request(None)).await).await;
    let ids: Vec<&str> = listed["data"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|model| model["id"].as_str())
        .collect();
    assert!(ids.contains(&"shared/openai/gpt-6-luna"));
    assert!(!ids.contains(&"connection-prov-first/openai/gpt-6-luna"));
    assert!(ids.contains(&"connection-prov-other/other-model"));
    assert!(!ids.contains(&"combo/not-a-combo"));

    let missing = send(
        &state,
        chat_request(&json!({"model": "shared/openai/missing", "messages": []})),
    )
    .await;
    assert_eq!(missing.status(), StatusCode::NOT_FOUND);
    assert!(
        json_body(missing).await["error"]["message"]
            .as_str()
            .unwrap()
            .contains("shared/openai/missing")
    );
    Ok(())
}

#[tokio::test]
async fn direct_model_stops_on_permanent_errors_and_skips_disabled_connections() -> TestResult {
    let state = test_state().await;
    let rejecting = spawn_upstream(error_status_router(StatusCode::BAD_REQUEST)).await;
    let healthy = spawn_upstream(openai_echo_router("should not run")).await;
    let disabled = spawn_upstream(openai_echo_router("disabled")).await;

    save_provider(
        &state,
        "prov-reject",
        &rejecting.url(),
        Protocol::OpenAi,
        true,
        "sk-reject",
    )
    .await?;
    save_provider(
        &state,
        "prov-healthy",
        &healthy.url(),
        Protocol::OpenAi,
        true,
        "sk-healthy",
    )
    .await?;
    save_provider(
        &state,
        "prov-off",
        &disabled.url(),
        Protocol::OpenAi,
        false,
        "sk-off",
    )
    .await?;
    remember_model(&state, "prov-reject", "openai/gpt-6-luna").await?;
    remember_model(&state, "prov-healthy", "openai/gpt-6-luna").await?;
    remember_model(&state, "prov-off", "only-disabled").await?;

    let stopped = send(
        &state,
        chat_request(&json!({"model": "connection-prov-reject/openai/gpt-6-luna", "messages": []})),
    )
    .await;
    assert_eq!(stopped.status(), StatusCode::BAD_REQUEST);
    assert_eq!(rejecting.hits.count(), 1);
    assert_eq!(healthy.hits.count(), 0);

    let unavailable = send(
        &state,
        chat_request(&json!({"model": "connection-prov-off/only-disabled", "messages": []})),
    )
    .await;
    assert_eq!(unavailable.status(), StatusCode::BAD_GATEWAY);
    assert_eq!(disabled.hits.count(), 0);

    let listed = json_body(send(&state, models_request(None)).await).await["data"].to_string();
    assert!(listed.contains("connection-prov-reject/openai/gpt-6-luna"));
    assert!(listed.contains("connection-prov-healthy/openai/gpt-6-luna"));
    assert!(!listed.contains("connection-prov-off/only-disabled"));
    Ok(())
}

#[tokio::test]
async fn custom_group_slugs_isolate_identical_models_and_preserve_nested_ids() -> TestResult {
    let state = test_state().await;
    let fail_first = Arc::new(AtomicUsize::new(0));
    let failure_control = fail_first.clone();
    let first = spawn_upstream(openai_echo_router("provider one").layer(
        axum::middleware::from_fn(move |request: Request, next: Next| {
            let failure_control = failure_control.clone();
            async move {
                if failure_control.load(Ordering::SeqCst) != 0 {
                    StatusCode::SERVICE_UNAVAILABLE.into_response()
                } else {
                    next.run(request).await
                }
            }
        }),
    ))
    .await;
    let second = spawn_upstream(openai_echo_router("provider two")).await;
    let first_provider =
        save_provider(&state, "p-one", &first.url(), Protocol::OpenAi, true, "one").await?;
    let second_provider = save_provider(
        &state,
        "p-two",
        &second.url(),
        Protocol::OpenAi,
        true,
        "two",
    )
    .await?;
    for (slug, url, provider) in [
        ("provider1", first.url(), first_provider),
        ("provider2", second.url(), second_provider),
    ] {
        let group = sparkrouter::db::provider_groups::create_with_slug(
            &state.db,
            slug,
            &url,
            &Protocol::OpenAi,
            Some(slug),
        )
        .await?;
        sqlx::query("INSERT INTO provider_group_connections (provider_id, group_id) VALUES (?, ?)")
            .bind(provider.id)
            .bind(group.id)
            .execute(&state.db)
            .await?;
    }
    for id in ["p-one", "p-two"] {
        connection_models::replace(
            &state.db,
            id,
            &[
                StoredModel {
                    id: "openai/gpt-6-luna".into(),
                    name: "gpt".into(),
                    fields: vec![],
                    free: false,
                },
                StoredModel {
                    id: "combo/nested/model".into(),
                    name: "nested".into(),
                    fields: vec![],
                    free: false,
                },
            ],
            false,
        )
        .await?;
    }

    let listed = json_body(send(&state, models_request(None)).await).await;
    let ids: Vec<_> = listed["data"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|m| m["id"].as_str())
        .collect();
    for slug in ["provider1", "provider2"] {
        assert_eq!(
            ids.iter()
                .filter(|id| **id == format!("{slug}/openai/gpt-6-luna"))
                .count(),
            1
        );
        assert_eq!(
            ids.iter()
                .filter(|id| **id == format!("{slug}/combo/nested/model"))
                .count(),
            1
        );
    }
    assert!(!ids.contains(&"openai/gpt-6-luna"));

    for (public, raw, reply, expected_first, expected_second) in [
        (
            "provider1/openai/gpt-6-luna",
            "openai/gpt-6-luna",
            "provider one",
            1,
            0,
        ),
        (
            "provider2/openai/gpt-6-luna",
            "openai/gpt-6-luna",
            "provider two",
            1,
            1,
        ),
        (
            "provider1/combo/nested/model",
            "combo/nested/model",
            "provider one",
            2,
            1,
        ),
    ] {
        let response = send(
            &state,
            chat_request(&json!({"model":public,"messages":[],"extension":{"preserved":true}})),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        let payload = json_body(response).await;
        assert_eq!(payload["choices"][0]["message"]["content"], reply);
        assert_eq!(payload["upstream_saw"]["model"], raw);
        assert_eq!(payload["upstream_saw"]["extension"]["preserved"], true);
        assert_eq!(
            (first.hits.count(), second.hits.count()),
            (expected_first, expected_second)
        );
    }

    let raw = send(
        &state,
        chat_request(&json!({"model":"openai/gpt-6-luna","messages":[]})),
    )
    .await;
    assert_eq!(raw.status(), StatusCode::NOT_FOUND);
    assert_eq!((first.hits.count(), second.hits.count()), (2, 1));
    fail_first.store(1, Ordering::SeqCst);
    let failed = send(
        &state,
        chat_request(&json!({"model":"provider1/openai/gpt-6-luna","messages":[]})),
    )
    .await;
    assert_eq!(failed.status(), StatusCode::BAD_GATEWAY);
    assert_eq!(
        (first.hits.count(), second.hits.count()),
        (3, 1),
        "retryable failures must not cross provider namespaces"
    );
    Ok(())
}

#[tokio::test]
async fn optional_api_key_protects_models_and_chat() -> TestResult {
    let mut state = test_state().await;
    combo_with_steps(&state, &[]).await?;
    state.api_key = Some(Arc::new(SecretString::from("gateway-secret")));

    // Missing credentials on both endpoints.
    let response = send(&state, models_request(None)).await;
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    let body = json!({"model": "combo/coding", "messages": [{"role": "user", "content": "hi"}]});
    let response = send(&state, chat_request(&body)).await;
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);

    // Wrong bearer token is still rejected (constant-time comparison path).
    let response = send(&state, models_request(Some("wrong-secret"))).await;
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);

    // The correct bearer token unlocks the API.
    let response = send(&state, models_request(Some("gateway-secret"))).await;
    assert_eq!(response.status(), StatusCode::OK);
    let payload = json_body(response).await;
    assert!(payload["data"].to_string().contains("combo/coding"));
    Ok(())
}

#[tokio::test]
async fn generated_api_keys_are_unrestricted_and_close_the_open_gateway() -> TestResult {
    let state = test_state().await;
    combo_with_steps(&state, &[]).await?;
    combos::create(&state.db, "writing").await?;

    // No environment key and no generated key: the API stays open.
    assert_eq!(
        send(&state, models_request(None)).await.status(),
        StatusCode::OK
    );

    let issued = sparkrouter::db::api_keys::create(&state.db, &state.crypto, "client").await?;
    let secret = issued.secret.expose_secret();
    assert_eq!(
        send(&state, models_request(None)).await.status(),
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        send(&state, models_request(Some("not-the-key")))
            .await
            .status(),
        StatusCode::UNAUTHORIZED
    );

    let response = send(&state, models_request(Some(secret))).await;
    assert_eq!(response.status(), StatusCode::OK);
    let listed = json_body(response).await["data"].to_string();
    assert!(listed.contains("combo/coding") && listed.contains("combo/writing"));

    // Auth is not scoped to a combo: the same key is accepted for every name.
    // Empty combos fail upstream after authentication, which is not a 401.
    for name in ["coding", "writing"] {
        let response = send(&state, authed_chat(secret, name)).await;
        assert_ne!(response.status(), StatusCode::UNAUTHORIZED);
        let _ = body_bytes(response).await;
    }

    sparkrouter::db::api_keys::delete(&state.db, &issued.id).await?;
    assert_eq!(
        send(&state, models_request(None)).await.status(),
        StatusCode::OK,
        "revoking the last generated key reopens the gateway when no environment key is set"
    );
    Ok(())
}

#[tokio::test]
async fn refreshing_models_stores_ids_for_combo_suggestions() -> TestResult {
    let state = test_state().await;
    let upstream = spawn_upstream(Router::new().route(
        "/models",
        get(|| async {
            Json(json!({"data": [
                {"id": "alpha", "display_name": "Alpha", "context_length": 1050000, "pricing": {"prompt": "0", "completion": "0"}},
                {"id": " beta "}
            ]}))
        }),
    ))
    .await;
    save_provider(
        &state,
        "listed",
        &upstream.url(),
        Protocol::OpenAi,
        true,
        "sk-listed",
    )
    .await?;
    let response = send(
        &state,
        Request::builder()
            .method(Method::POST)
            .uri("/providers/listed/models")
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .header(header::HOST, "127.0.0.1:8080")
            .header(header::ORIGIN, "http://127.0.0.1:8080")
            .body(Body::from(form_urlencoded(&[(
                "_csrf",
                state.csrf_token.as_str(),
            )])))
            .unwrap(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let page = String::from_utf8(body_bytes(response).await.to_vec())?;
    assert!(page.contains("alpha") && page.contains("beta"));
    assert!(page.contains("Alpha"));
    assert!(page.contains("1.050.000 ctx"));
    assert!(page.contains("data-copy-model=\"connection-listed/alpha\""));
    assert!(page.contains("model-card-free"));
    assert!(!page.contains("Owned by"));
    assert!(page.contains("data-model-search"));
    let combo = combos::create(&state.db, "suggest").await?;
    let editor = send(
        &state,
        Request::builder()
            .uri(format!("/combos/{}", combo.id))
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(editor.status(), StatusCode::OK);
    let editor = String::from_utf8(body_bytes(editor).await.to_vec())?;
    assert!(editor.contains("id=\"models-listed\""));
    assert!(editor.contains("value=\"alpha\""));
    assert!(editor.contains("value=\"beta\""));
    Ok(())
}

fn authed_chat(secret: &str, combo: &str) -> Request<Body> {
    Request::builder()
        .method(Method::POST)
        .uri("/v1/chat/completions")
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::AUTHORIZATION, format!("Bearer {secret}"))
        .body(Body::from(
            json!({"model": format!("combo/{combo}"), "messages": [{"role": "user", "content": "hi"}]}).to_string(),
        ))
        .unwrap()
}

// ---------------------------------------------------------------------------
// Streaming
// ---------------------------------------------------------------------------

#[tokio::test]
async fn streaming_falls_back_before_start_and_passes_sse_through() -> TestResult {
    let state = test_state().await;
    let first = spawn_upstream(error_status_router(StatusCode::SERVICE_UNAVAILABLE)).await;
    let second = spawn_upstream(sse_router(vec![
        "data: {\"id\":\"chunk-1\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"Hel\"}}]}\n\n",
        "data: {\"id\":\"chunk-2\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"lo\"}}]}\n\n",
        "data: [DONE]\n\n",
    ]))
    .await;

    save_provider(
        &state,
        "prov-sse-down",
        &first.url(),
        Protocol::OpenAi,
        true,
        "sk-a",
    )
    .await?;
    save_provider(
        &state,
        "prov-sse-up",
        &second.url(),
        Protocol::OpenAi,
        true,
        "sk-b",
    )
    .await?;
    combo_with_steps(
        &state,
        &[("prov-sse-down", "model-a"), ("prov-sse-up", "model-b")],
    )
    .await?;

    let body = json!({
        "model": "combo/coding",
        "stream": true,
        "messages": [{"role": "user", "content": "stream please"}],
    });
    let response = send(&state, chat_request(&body)).await;
    assert_eq!(response.status(), StatusCode::OK);
    let content_type = response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_owned();
    assert!(
        content_type.starts_with("text/event-stream"),
        "unexpected content-type {content_type}"
    );

    // Raw SSE bytes must pass through byte-for-byte.
    let collected = body_bytes(response).await;
    let expected = [
        "data: {\"id\":\"chunk-1\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"Hel\"}}]}\n\n",
        "data: {\"id\":\"chunk-2\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"lo\"}}]}\n\n",
        "data: [DONE]\n\n",
    ]
    .concat();
    assert_eq!(collected, Bytes::from(expected));

    assert_eq!(first.hits.count(), 1);
    assert_eq!(second.hits.count(), 1);
    Ok(())
}

#[tokio::test]
async fn mid_stream_upstream_failure_does_not_fall_back() -> TestResult {
    // The committed-stream invariant must hold no matter how the starting
    // step is chosen: it is asserted for the ordered default and for round
    // robin, whose first request starts at the same first step via the
    // rotation path. Random starts at an unpredictable step, so it cannot be
    // checked deterministically here; the code path after the response is
    // committed is shared by every strategy.
    for strategy in [Strategy::Priority, Strategy::RoundRobin] {
        mid_stream_upstream_failure_does_not_fall_back_for(strategy).await?;
    }
    Ok(())
}

async fn mid_stream_upstream_failure_does_not_fall_back_for(strategy: Strategy) -> TestResult {
    let state = test_state().await;
    let (router, disconnect) = sse_disconnect_router();
    let first = spawn_upstream(router).await;
    let second = spawn_upstream(openai_echo_router("must not be used")).await;

    save_provider(
        &state,
        "prov-flaky",
        &first.url(),
        Protocol::OpenAi,
        true,
        "sk-flaky",
    )
    .await?;
    save_provider(
        &state,
        "prov-backup",
        &second.url(),
        Protocol::OpenAi,
        true,
        "sk-backup",
    )
    .await?;
    combo_with_steps(
        &state,
        &[("prov-flaky", "model-a"), ("prov-backup", "model-b")],
    )
    .await?;
    let combo = combos::by_name(&state.db, "coding").await?;
    combos::set_strategy(&state.db, &combo.id, strategy).await?;

    let body = json!({
        "model": "combo/coding",
        "stream": true,
        "messages": [{"role": "user", "content": "stream please"}],
    });
    let response = send(&state, chat_request(&body)).await;
    // The first SSE frame committed the response: 200 must reach the client.
    assert_eq!(response.status(), StatusCode::OK);
    assert!(
        response
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| value.starts_with("text/event-stream"))
    );

    let mut frames = response.into_body().into_data_stream();
    let first_frame = tokio::time::timeout(Duration::from_secs(10), frames.next())
        .await
        .expect("first SSE frame within timeout")
        .expect("stream contains a first frame")
        .expect("first frame is data");
    assert_eq!(
        first_frame,
        Bytes::from_static(b"data: {\"first\":true}\n\n")
    );

    // Only now break the upstream connection; the failure must surface as a
    // body error rather than triggering another combo step.
    disconnect
        .send(())
        .await
        .expect("signal mid-stream disconnect");
    let next_frame = tokio::time::timeout(Duration::from_secs(10), frames.next())
        .await
        .expect("second frame within timeout");
    assert!(
        matches!(&next_frame, Some(Err(_))),
        "mid-body failure must surface as a body error, got {next_frame:?}"
    );

    assert_eq!(first.hits.count(), 1);
    assert_eq!(
        second.hits.count(),
        0,
        "a committed streaming body must not fail over"
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// Anthropic protocol conversion
// ---------------------------------------------------------------------------

#[tokio::test]
async fn anthropic_provider_converts_tools_and_text() -> TestResult {
    let state = test_state().await;
    let captured = Arc::new(std::sync::Mutex::new(None::<Value>));
    let seen = captured.clone();
    let router = Router::new().route(
        "/messages",
        post(move |headers: HeaderMap, body: Bytes| {
            let seen = seen.clone();
            async move {
                let received: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
                *seen.lock().unwrap() = Some(json!({
                    "request": received,
                    "x_api_key": headers.get("x-api-key").and_then(|v| v.to_str().ok()).unwrap_or(""),
                    "anthropic_version": headers.get("anthropic-version").and_then(|v| v.to_str().ok()).unwrap_or(""),
                    "authorization": headers.get(header::AUTHORIZATION).and_then(|v| v.to_str().ok()).unwrap_or(""),
                }));
                // Anthropic Messages API shape: content blocks with text and tool_use.
                Json(json!({
                    "id": "msg_mock_01",
                    "type": "message",
                    "role": "assistant",
                    "model": "claude-mock-20260101",
                    "content": [
                        {"type": "text", "text": "Claude says hi"},
                        {"type": "tool_use", "id": "toolu_mock_01", "name": "get_weather", "input": {"city": "Oslo"}},
                    ],
                    "stop_reason": "tool_use",
                    "stop_sequence": null,
                    "usage": {"input_tokens": 12, "output_tokens": 34},
                }))
            }
        }),
    );
    let upstream = spawn_upstream(router).await;

    save_provider(
        &state,
        "prov-claude",
        &upstream.url(),
        Protocol::Anthropic,
        true,
        "sk-ant-mock",
    )
    .await?;
    combo_with_steps(&state, &[("prov-claude", "claude-mock-20260101")]).await?;

    let body = json!({
        "model": "combo/coding",
        "stream": false,
        "max_tokens": 512,
        "messages": [
            {"role": "system", "content": "Be terse."},
            {"role": "user", "content": "What is the weather in Oslo?"},
        ],
        "tools": [{
            "type": "function",
            "function": {
                "name": "get_weather",
                "description": "Get the current weather for a city",
                "parameters": {
                    "type": "object",
                    "properties": {"city": {"type": "string"}},
                    "required": ["city"],
                },
            },
        }],
    });
    let response = send(&state, chat_request(&body)).await;
    assert_eq!(response.status(), StatusCode::OK);
    let payload = json_body(response).await;

    // What the Anthropic upstream actually received.
    let seen = captured
        .lock()
        .unwrap()
        .clone()
        .expect("anthropic upstream was called");
    let request = &seen["request"];
    assert_eq!(request["model"], "claude-mock-20260101");
    assert_eq!(request["max_tokens"], 512);
    assert_eq!(
        request["system"],
        json!([{"type":"text","text":"Be terse."}])
    );
    assert_eq!(
        request["messages"],
        json!([{"role":"user","content":[{"type":"text","text":"What is the weather in Oslo?"}]}])
    );
    let tool = &request["tools"][0];
    assert_eq!(tool["name"], "get_weather");
    assert_eq!(
        tool["input_schema"],
        body["tools"][0]["function"]["parameters"]
    );
    assert!(
        tool.get("parameters").is_none(),
        "OpenAI tool shape must not leak upstream"
    );
    assert_eq!(seen["x_api_key"], "sk-ant-mock");
    assert_eq!(seen["anthropic_version"], "2023-06-01");
    assert_eq!(seen["authorization"], "");

    // What the OpenAI-compatible client got back.
    assert_eq!(payload["id"], "msg_mock_01");
    let message = &payload["choices"][0]["message"];
    assert_eq!(message["content"], "Claude says hi");
    let call = &message["tool_calls"][0];
    assert_eq!(call["type"], "function");
    assert_eq!(call["id"], "toolu_mock_01");
    assert_eq!(call["function"]["name"], "get_weather");
    assert_eq!(
        serde_json::from_str::<Value>(call["function"]["arguments"].as_str().unwrap())?,
        json!({"city":"Oslo"})
    );
    assert_eq!(
        payload["usage"],
        json!({"prompt_tokens":12,"completion_tokens":34,"total_tokens":46})
    );
    assert_eq!(payload["choices"][0]["finish_reason"], "tool_calls");
    Ok(())
}

// ---------------------------------------------------------------------------
// Dashboard UI: CSRF edit flow and cross-origin protection
// ---------------------------------------------------------------------------

#[tokio::test]
async fn provider_edit_with_blank_api_key_preserves_ciphertext() -> TestResult {
    let state = test_state().await;
    save_provider(
        &state,
        "prov-ui",
        "http://127.0.0.1:9",
        Protocol::OpenAi,
        true,
        "sk-original",
    )
    .await?;
    let before = provider_db::get(&state.db, "prov-ui").await?;

    let form = form_urlencoded(&[
        ("name", "Renamed Provider"),
        ("base_url", "http://127.0.0.1:9/v1"),
        ("protocol", "openai"),
        ("enabled", "on"),
        ("api_key", ""),
        ("_csrf", state.csrf_token.as_str()),
    ]);
    let response = send(&state, dashboard_edit_request("prov-ui", form)).await;
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    let _ = body_bytes(response).await;

    let after = provider_db::get(&state.db, "prov-ui").await?;
    assert_eq!(after.name, "Renamed Provider");
    assert_eq!(after.base_url, "http://127.0.0.1:9/v1");
    assert!(after.enabled);

    // A blank api_key on the edit form must keep the stored ciphertext intact,
    // byte for byte, and it must still decrypt to the original secret.
    assert_eq!(
        after.encrypted_api_key, before.encrypted_api_key,
        "blank api_key must preserve the exact stored ciphertext"
    );
    assert_eq!(
        state
            .crypto
            .decrypt_api_key("prov-ui", &after.encrypted_api_key)
            .unwrap()
            .expose_secret(),
        "sk-original"
    );
    Ok(())
}

#[tokio::test]
async fn dashboard_pages_send_same_origin_referrer_policy() -> TestResult {
    let state = test_state().await;

    for uri in ["/providers/new", "/combos/new"] {
        let request = Request::builder()
            .method(Method::GET)
            .uri(uri)
            .body(Body::empty())
            .unwrap();
        let response = send(&state, request).await;
        assert_eq!(response.status(), StatusCode::OK);
        // `no-referrer` made native form POSTs send Origin: null, which the
        // dashboard origin check rejects; same-origin keeps form POSTs
        // working while still withholding referrers from external sites.
        assert_eq!(
            response
                .headers()
                .get(header::REFERRER_POLICY)
                .and_then(|value| value.to_str().ok())
                .unwrap_or_default(),
            "same-origin",
            "unexpected Referrer-Policy on {uri}"
        );
        let _ = body_bytes(response).await;
    }
    Ok(())
}

#[tokio::test]
async fn native_form_navigation_creates_provider_and_combo() -> TestResult {
    let state = test_state().await;

    // Headers a real browser puts on a native (non-fetch) form submission.
    let navigation_post = |uri: &str, host: &str, body: String| {
        Request::builder()
            .method(Method::POST)
            .uri(uri)
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .header(header::HOST, host)
            .header(header::ORIGIN, format!("http://{host}"))
            .header("sec-fetch-site", "same-origin")
            .header("sec-fetch-mode", "navigate")
            .header("sec-fetch-dest", "document")
            .body(Body::from(body))
            .unwrap()
    };

    // Provider creation exactly as the dashboard form submits it. The
    // base_url points at a port nothing listens on; creating a provider must
    // not contact the upstream.
    let form = form_urlencoded(&[
        ("name", "Local Provider"),
        ("base_url", "http://127.0.0.1:9/v1"),
        ("protocol", "openai"),
        ("enabled", "1"),
        ("api_key", ""),
        ("_csrf", state.csrf_token.as_str()),
    ]);
    let response = send(
        &state,
        navigation_post("/providers", "127.0.0.1:60123", form),
    )
    .await;
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    let _ = body_bytes(response).await;

    let providers = provider_db::list(&state.db).await?;
    assert_eq!(providers.len(), 1, "exactly one provider must be saved");
    let provider = &providers[0];
    assert_eq!(provider.name, "Local Provider");
    assert_eq!(provider.base_url, "http://127.0.0.1:9/v1");
    assert_eq!(provider.protocol, Protocol::OpenAi);
    assert!(provider.enabled);
    // A blank key denotes an unauthenticated local provider: no ciphertext.
    assert!(provider.encrypted_api_key.is_empty());

    // Combo creation the same way; Host/Origin may use localhost too.
    let form = form_urlencoded(&[
        ("name", "native-flow"),
        ("_csrf", state.csrf_token.as_str()),
    ]);
    let response = send(&state, navigation_post("/combos", "localhost:60123", form)).await;
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    let _ = body_bytes(response).await;

    let combo = combos::by_name(&state.db, "native-flow").await?;
    assert_eq!(combo.name, "native-flow");
    assert!(combos::steps(&state.db, &combo.id).await?.is_empty());

    // Same-origin navigation headers do not bypass the CSRF check: a wrong
    // token is rejected and must not create any additional rows.
    let form = form_urlencoded(&[
        ("name", "Evil Provider"),
        ("base_url", "http://127.0.0.1:9/v1"),
        ("protocol", "openai"),
        ("enabled", "1"),
        ("api_key", ""),
        ("_csrf", "wrong-token"),
    ]);
    let response = send(
        &state,
        navigation_post("/providers", "127.0.0.1:60123", form),
    )
    .await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let _ = body_bytes(response).await;

    let form = form_urlencoded(&[("name", "evil-flow"), ("_csrf", "wrong-token")]);
    let response = send(&state, navigation_post("/combos", "localhost:60123", form)).await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let _ = body_bytes(response).await;

    assert_eq!(provider_db::list(&state.db).await?.len(), 1);
    assert_eq!(combos::list(&state.db).await?.len(), 1);
    Ok(())
}

#[tokio::test]
async fn grouped_connection_form_rejects_cross_origin() -> TestResult {
    let state = test_state().await;
    let form = form_urlencoded(&[
        ("name", "Unwanted connection"),
        ("base_url", "https://api.openai.com/v1"),
        ("protocol", "openai"),
        ("enabled", "1"),
        ("api_key", "fake-key"),
        ("_csrf", state.csrf_token.as_str()),
    ]);
    let response = send(
        &state,
        Request::builder()
            .method(Method::POST)
            .uri("/providers/catalog/openai")
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .header(header::HOST, "127.0.0.1:60123")
            .header(header::ORIGIN, "https://untrusted.example")
            .header("sec-fetch-site", "cross-site")
            .body(Body::from(form))
            .unwrap(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let _ = body_bytes(response).await;
    assert!(provider_db::list(&state.db).await?.is_empty());
    Ok(())
}

#[tokio::test]
async fn cross_origin_dashboard_mutation_rejected() -> TestResult {
    let state = test_state().await;
    save_provider(
        &state,
        "prov-ui",
        "http://127.0.0.1:9",
        Protocol::OpenAi,
        true,
        "sk-original",
    )
    .await?;

    let build = |name: &str, origin: Option<&str>, fetch_site: Option<&str>| {
        let body = form_urlencoded(&[
            ("name", name),
            ("base_url", "http://127.0.0.1:9"),
            ("protocol", "openai"),
            ("enabled", "on"),
            ("api_key", ""),
            ("_csrf", state.csrf_token.as_str()),
        ]);
        let mut builder = Request::builder()
            .method(Method::POST)
            .uri("/providers/prov-ui")
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .header(header::HOST, "127.0.0.1:8080");
        if let Some(origin) = origin {
            builder = builder.header(header::ORIGIN, origin);
        }
        if let Some(fetch_site) = fetch_site {
            builder = builder.header("sec-fetch-site", fetch_site);
        }
        builder.body(Body::from(body)).unwrap()
    };

    // A foreign Origin against our Host is a cross-site submission.
    let response = send(
        &state,
        build("Hijacked", Some("https://attacker.example"), None),
    )
    .await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let _ = body_bytes(response).await;

    // Browsers also announce cross-site navigations via Sec-Fetch-Site.
    let response = send(
        &state,
        build(
            "Hijacked",
            Some("http://127.0.0.1:8080"),
            Some("cross-site"),
        ),
    )
    .await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let _ = body_bytes(response).await;

    // The literal Origin "null" (as sent under a no-referrer/sandboxed
    // policy, which motivated switching to same-origin) is not our origin.
    let response = send(&state, build("Hijacked", Some("null"), None)).await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let _ = body_bytes(response).await;

    // A same-host Origin on a different port is still a cross-origin submit.
    let response = send(
        &state,
        build("Hijacked", Some("http://127.0.0.1:9999"), None),
    )
    .await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let _ = body_bytes(response).await;

    // None of the attempts may have mutated the stored provider.
    let provider = provider_db::get(&state.db, "prov-ui").await?;
    assert_eq!(provider.name, "prov-ui display");
    assert_eq!(provider.base_url, "http://127.0.0.1:9");
    Ok(())
}

// ---------------------------------------------------------------------------
// Persistence: disk database survives a restart
// ---------------------------------------------------------------------------

#[tokio::test]
async fn disk_database_survives_restart() -> TestResult {
    let directory = tempfile::tempdir()?;
    let db_path = directory.path().join("sparkrouter-gateway-test.db");
    // Pre-create the file so a read-write (non-creating) open works; forward
    // slashes keep the sqlite:// URL unambiguous on Windows.
    std::fs::File::create(&db_path)?;
    let database_url = format!("sqlite://{}", db_path.to_str().unwrap().replace('\\', "/"));

    // First "process": write configuration.
    let state = AppState::from_config(test_config(&database_url))
        .await
        .unwrap();
    save_provider(
        &state,
        "prov-disk",
        "http://127.0.0.1:9",
        Protocol::OpenAi,
        true,
        "sk-disk",
    )
    .await?;
    save_provider(
        &state,
        "prov-disk-b",
        "http://127.0.0.1:10",
        Protocol::Anthropic,
        false,
        "sk-disk-b",
    )
    .await?;
    combo_with_steps(
        &state,
        &[("prov-disk", "model-a"), ("prov-disk-b", "model-b")],
    )
    .await?;
    let provider_before = provider_db::get(&state.db, "prov-disk").await?;
    state.db.close().await;
    drop(state);

    // Second "process": a fresh AppState over the same database file.
    let reopened = AppState::from_config(test_config(&database_url))
        .await
        .unwrap();
    let provider = provider_db::get(&reopened.db, "prov-disk").await?;
    assert_eq!(provider.name, provider_before.name);
    assert_eq!(provider.base_url, "http://127.0.0.1:9");
    assert!(provider.enabled);
    assert_eq!(
        provider.encrypted_api_key,
        provider_before.encrypted_api_key
    );
    assert_eq!(
        reopened
            .crypto
            .decrypt_api_key("prov-disk", &provider.encrypted_api_key)
            .unwrap()
            .expose_secret(),
        "sk-disk"
    );

    // The combo and its ordered steps persisted too.
    let combo = combos::by_name(&reopened.db, "coding").await?;
    let steps = combos::steps(&reopened.db, &combo.id).await?;
    assert_eq!(steps.len(), 2);
    assert_eq!(steps[0].provider_id, "prov-disk");
    assert_eq!(steps[0].model, "model-a");
    assert_eq!(steps[1].provider_id, "prov-disk-b");
    assert_eq!(steps[1].model, "model-b");
    reopened.db.close().await;
    Ok(())
}

#[tokio::test]
async fn round_robin_strategy_persists_but_the_cursor_resets_after_restart() -> TestResult {
    let directory = tempfile::tempdir()?;
    let database_url = format!(
        "sqlite://{}",
        directory
            .path()
            .join("round-robin.db")
            .to_str()
            .unwrap()
            .replace('\\', "/")
    );
    let first = spawn_upstream(openai_echo_router("first reply")).await;
    let second = spawn_upstream(openai_echo_router("second reply")).await;
    let state = AppState::from_config(test_config(&database_url)).await?;
    save_provider(
        &state,
        "first",
        &first.url(),
        Protocol::OpenAi,
        true,
        "fake-first",
    )
    .await?;
    save_provider(
        &state,
        "second",
        &second.url(),
        Protocol::OpenAi,
        true,
        "fake-second",
    )
    .await?;
    combo_with_strategy(
        &state,
        "restart",
        &[("first", "model-first"), ("second", "model-second")],
        Strategy::RoundRobin,
    )
    .await?;
    assert_combo_reply(&state, "restart", "model-first").await?;
    state.db.close().await;
    drop(state);

    let reopened = AppState::from_config(test_config(&database_url)).await?;
    assert_eq!(
        combos::by_name(&reopened.db, "restart").await?.strategy,
        Strategy::RoundRobin
    );
    assert_combo_reply(&reopened, "restart", "model-first").await?;
    assert_combo_reply(&reopened, "restart", "model-second").await?;
    assert_eq!(first.hits.count(), 2);
    assert_eq!(second.hits.count(), 1);
    reopened.db.close().await;
    Ok(())
}

#[tokio::test]
async fn random_permanent_errors_stop_without_trying_other_steps() -> TestResult {
    let state = test_state().await;
    let first = spawn_upstream(error_status_router(StatusCode::BAD_REQUEST)).await;
    let second = spawn_upstream(error_status_router(StatusCode::BAD_REQUEST)).await;
    save_provider(
        &state,
        "first",
        &first.url(),
        Protocol::OpenAi,
        true,
        "fake-first",
    )
    .await?;
    save_provider(
        &state,
        "second",
        &second.url(),
        Protocol::OpenAi,
        true,
        "fake-second",
    )
    .await?;
    combo_with_strategy(
        &state,
        "random-stop",
        &[("first", "model-first"), ("second", "model-second")],
        Strategy::Random,
    )
    .await?;
    let response = send(&state, chat_request(&json!({"model": "combo/random-stop", "messages": [{"role": "user", "content": "hello"}]}))).await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let _ = body_bytes(response).await;
    assert_eq!(first.hits.count() + second.hits.count(), 1);
    Ok(())
}

#[tokio::test]
async fn random_stream_interruption_never_attempts_a_second_step() -> TestResult {
    let state = test_state().await;
    let (first_router, first_disconnect) = sse_disconnect_router();
    let (second_router, second_disconnect) = sse_disconnect_router();
    let first = spawn_upstream(first_router).await;
    let second = spawn_upstream(second_router).await;
    save_provider(
        &state,
        "first",
        &first.url(),
        Protocol::OpenAi,
        true,
        "fake-first",
    )
    .await?;
    save_provider(
        &state,
        "second",
        &second.url(),
        Protocol::OpenAi,
        true,
        "fake-second",
    )
    .await?;
    combo_with_strategy(
        &state,
        "random-stream",
        &[("first", "model-first"), ("second", "model-second")],
        Strategy::Random,
    )
    .await?;
    let response = send(&state, chat_request(&json!({"model": "combo/random-stream", "stream": true, "messages": [{"role": "user", "content": "hello"}]}))).await;
    assert_eq!(response.status(), StatusCode::OK);
    let mut frames = response.into_body().into_data_stream();
    let frame = tokio::time::timeout(Duration::from_secs(10), frames.next())
        .await?
        .expect("first frame")?;
    assert_eq!(frame, Bytes::from_static(b"data: {\"first\":true}\n\n"));
    assert_eq!(first.hits.count() + second.hits.count(), 1);
    // Either starting step may be selected. Signal only the committed upstream.
    let disconnect = if first.hits.count() == 1 {
        first_disconnect
    } else {
        second_disconnect
    };
    disconnect.send(()).await?;
    let interrupted = tokio::time::timeout(Duration::from_secs(10), frames.next()).await?;
    assert!(matches!(interrupted, Some(Err(_))));
    assert_eq!(first.hits.count() + second.hits.count(), 1);
    Ok(())
}
