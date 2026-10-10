//! Integration tests for the dashboard test-chat playground (`GET /test` and
//! `POST /test/chat`).
//!
//! These drive `app::router(state).oneshot(request)` against real local Axum
//! upstream mocks (bound to 127.0.0.1:0), following the gateway suite's RAII
//! server pattern. The chat endpoint runs the real combo fallback loop, so
//! these tests pin its trace envelope, its sanitization, and the dashboard
//! CSRF/origin plus gateway authentication layers wrapped around it.

mod common;
use common::{body_bytes, json_body, spawn_upstream};
use std::sync::Arc;

use axum::{
    Json, Router,
    body::Body,
    extract::Request,
    http::{Method, StatusCode, header, request::Builder},
    response::Response,
    routing::post,
};
use base64::{Engine, engine::general_purpose::STANDARD};
use bytes::Bytes;
use secrecy::{ExposeSecret, SecretString};
use serde_json::{Value, json};
use tower::ServiceExt;

use sparkrouter::{
    app::{self, AppState},
    config::Config,
    crypto::Crypto,
    db::{
        api_keys,
        combos::{self, Strategy},
        providers as provider_db,
    },
    providers::Protocol,
};

type BoxError = Box<dyn std::error::Error + Send + Sync>;
type TestResult = Result<(), BoxError>;

const MASTER_KEY: &[u8; 32] = &[57; 32];

// ---------------------------------------------------------------------------
// Fixtures and helpers
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

/// Store a connection row directly with an explicit display name, so trace
/// assertions can pin `provider_name` values that differ from the ids.
async fn save_provider(
    state: &AppState,
    id: &str,
    name: &str,
    base_url: &str,
    protocol: Protocol,
    enabled: bool,
    api_key: &str,
) -> Result<provider_db::Provider, BoxError> {
    let provider = provider_db::Provider {
        id: id.to_owned(),
        name: name.to_owned(),
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

/// Create a Priority combo with ordered steps.
async fn combo_with_steps(state: &AppState, name: &str, steps: &[(&str, &str)]) -> TestResult {
    let combo = combos::create(&state.db, name).await?;
    for (provider_id, model) in steps {
        combos::add_step(&state.db, &combo.id, provider_id, model).await?;
    }
    Ok(())
}

/// Create a named combo with ordered steps and an explicit strategy.
async fn combo_with_strategy(
    state: &AppState,
    name: &str,
    steps: &[(&str, &str)],
    strategy: Strategy,
) -> TestResult {
    let combo = combos::create_with_strategy(&state.db, name, strategy).await?;
    for (provider_id, model) in steps {
        combos::add_step(&state.db, &combo.id, provider_id, model).await?;
    }
    Ok(())
}

/// A dashboard test-chat payload with a valid CSRF token.
fn chat_body(state: &AppState, model: &str, messages: Value) -> Value {
    json!({
        "_csrf": state.csrf_token.as_str(),
        "model": model,
        "messages": messages,
    })
}

// ---------------------------------------------------------------------------
// Mock upstream servers (RAII: dropping aborts the server task)
// ---------------------------------------------------------------------------

/// OpenAI-compatible upstream that echoes the exact request it received
/// (including the model it was given) inside a minimal chat.completion reply.
fn openai_echo_router(reply: &'static str) -> Router {
    Router::new().route(
        "/chat/completions",
        post(move |body: Bytes| async move {
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
                "upstream_saw": received,
            }))
        }),
    )
}

/// Upstream that always answers with the given HTTP status and an error body
/// whose distinctive text must never leak into gateway responses.
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

// ---------------------------------------------------------------------------
// Request/response helpers
// ---------------------------------------------------------------------------

/// POST builder for /test/chat, so individual tests can append browser-like
/// headers (Authorization, Origin, Sec-Fetch-Site) before attaching the body.
fn test_chat_builder() -> Builder {
    Request::builder()
        .method(Method::POST)
        .uri("/test/chat")
        .header(header::CONTENT_TYPE, "application/json")
}

fn v1_chat_request(body: &Value) -> Request<Body> {
    Request::builder()
        .method(Method::POST)
        .uri("/v1/chat/completions")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(body.to_string()))
        .unwrap()
}

async fn send(state: &AppState, request: Request<Body>) -> Response {
    app::router(state.clone())
        .oneshot(request)
        .await
        .expect("router call is infallible")
}

/// POST a JSON body to /test/chat and return the status with the parsed body.
async fn send_test_chat(state: &AppState, body: &Value) -> (StatusCode, Value) {
    let request = test_chat_builder()
        .body(Body::from(body.to_string()))
        .unwrap();
    let response = send(state, request).await;
    let status = response.status();
    (status, json_body(response).await)
}

/// GETs a dashboard page, asserting it renders successfully, and returns HTML.
async fn get_page(state: &AppState, path: &str) -> String {
    let response = send(state, Request::get(path).body(Body::empty()).unwrap()).await;
    assert_eq!(response.status(), StatusCode::OK, "GET {path} must render");
    String::from_utf8(body_bytes(response).await.to_vec()).unwrap()
}

/// Removes markup so anchor labels can be compared as plain text.
fn strip_tags(fragment: &str) -> String {
    let mut out = String::new();
    let mut inside_tag = false;
    for character in fragment.chars() {
        match character {
            '<' => inside_tag = true,
            '>' => inside_tag = false,
            _ if !inside_tag => out.push(character),
            _ => {}
        }
    }
    out
}

/// The full `<a ...>...</a>` element containing `marker`, for label checks.
fn anchor_fragment<'a>(html: &'a str, marker: &str) -> Option<&'a str> {
    let at = html.find(marker)?;
    let start = html[..at].rfind("<a ")?;
    let end = start + html[start..].find("</a>")?;
    Some(&html[start..end])
}

// ---------------------------------------------------------------------------
// GET /test: rendering, sidebar navigation, assets, and secret hygiene
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_page_renders_empty_and_populated_without_leaking_credentials() -> TestResult {
    let state = test_state().await;
    let upstream = spawn_upstream(openai_echo_router("unused reply")).await;

    // Empty database: the page explains that a combo is needed and ships no
    // chat form or script.
    let empty_response = send(&state, Request::get("/test").body(Body::empty()).unwrap()).await;
    assert_eq!(empty_response.status(), StatusCode::OK);
    let csp = empty_response
        .headers()
        .get(header::CONTENT_SECURITY_POLICY)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_owned();
    assert!(
        csp.contains("script-src 'self'"),
        "script-src must stay 'self'"
    );
    assert!(
        !csp.contains("unsafe-inline"),
        "CSP must not allow unsafe-inline"
    );
    let empty = String::from_utf8(body_bytes(empty_response).await.to_vec()).unwrap();
    assert!(
        empty.contains("No combos configured yet."),
        "the empty page must explain that a combo is needed"
    );
    assert!(
        !empty.contains("id=\"test-form\""),
        "no compose form renders before any combo exists"
    );
    assert!(
        !empty.contains("/static/test-chat.js"),
        "the chat script is only shipped when a combo exists"
    );

    // Populated: one connection with a stored secret plus two combos.
    save_provider(
        &state,
        "prov-page",
        "Page Connection",
        &upstream.url(),
        Protocol::OpenAi,
        true,
        "sk-page-secret-999",
    )
    .await?;
    let coding = combos::create(&state.db, "coding").await?;
    combos::add_step(&state.db, &coding.id, "prov-page", "model-page").await?;
    combos::create(&state.db, "writing").await?;

    let page = get_page(&state, "/test").await;
    assert!(page.contains("href=\"#content\""));
    assert!(page.contains("<main id=\"content\" tabindex=\"-1\""));
    assert_eq!(page.matches("<h1>").count(), 1);
    let topbar_start = page.find("<header class=\"topbar\">").expect("topbar");
    let topbar_end = topbar_start
        + page[topbar_start..]
            .find("</header>")
            .expect("topbar closing");
    assert!(page[topbar_start..topbar_end].contains("<h1>Test</h1>"));
    assert!(page.contains("<title>SparkRouter :: Test</title>"));
    assert!(page.contains("class=\"test-chat-shell\""));
    assert!(page.contains("What would you like to try?"));
    assert!(page.contains("switching combos or modes clears results."));
    assert!(page.contains("<option value=\"systemone\">SystemOne</option>"));
    assert!(!page.contains("Follow-up messages keep the context."));
    assert!(page.contains("<details class=\"test-routing\">"));
    assert!(!page.contains("Enter to send"));
    assert!(!page.contains("test-compose-hint"));
    assert!(!page.contains("<footer"));
    assert!(!page.contains("self-hosted LLM gateway with ordered fallback combos"));
    assert!(page.contains("<input id=\"test-draft\" type=\"text\""));
    assert!(page.contains("class=\"test-compose-row\""));
    assert!(!page.contains("<textarea id=\"test-draft\""));
    assert!(page.contains("id=\"test-evaluation\" class=\"test-evaluation\" hidden"));
    assert!(page.contains("<textarea id=\"test-state\""));
    assert!(page.contains("<textarea id=\"test-questions\""));
    assert!(page.find("id=\"test-log\"").unwrap() < page.find("id=\"test-form\"").unwrap());
    let nav_start = page
        .find("<nav aria-label=\"Dashboard\">")
        .expect("dashboard nav");
    let nav_end = nav_start + page[nav_start..].find("</nav>").expect("nav closing");
    let sidebar_nav = &page[nav_start..nav_end];
    assert_eq!(sidebar_nav.matches("aria-current=\"page\"").count(), 1);

    // The sidebar links the playground with a visible "Test" label and the
    // local icon sprite.
    let sidebar_open = page
        .find("<aside class=\"sidebar\">")
        .expect("sidebar opening");
    let sidebar_close = sidebar_open
        + page[sidebar_open..]
            .find("</aside>")
            .expect("sidebar closing");
    let sidebar = &page[sidebar_open..sidebar_close];
    let test_link = anchor_fragment(sidebar, "href=\"/test\"")
        .expect("the sidebar must link the test playground");
    let label = strip_tags(test_link);
    assert!(label.trim() == "Test", "sidebar link label was {label:?}");
    assert!(
        test_link.contains("/static/icons.svg#flask-conical"),
        "the test link uses the bundled local sprite"
    );

    // The combo picker lists every combo and embeds the CSRF token for the
    // script; no inline scripts run against the strict CSP.
    assert!(page.contains("id=\"test-combo\""));
    assert!(page.contains("<option value=\"coding\" data-category=\"text\">"));
    assert!(page.contains("coding &middot; Text &middot; Priority"));
    assert!(page.contains("<option value=\"writing\" data-category=\"text\">"));
    assert!(page.contains(&format!("value=\"{}\"", state.csrf_token)));
    assert!(page.contains("src=\"/static/test-chat.js\""));
    assert!(
        !page.contains("<script>"),
        "no inline scripts: CSP only allows 'self'"
    );
    // No gateway key is configured, so no key field renders.
    assert!(
        !page.contains("id=\"test-key\""),
        "the key field appears only when gateway authentication is configured"
    );

    // Credentials and endpoint URLs never reach the page.
    assert!(!page.contains("sk-page-secret-999"));
    assert!(
        !page.contains(&upstream.url()),
        "the page must not reveal the connection's endpoint URL"
    );

    // The referenced script is served locally as real JavaScript.
    let script = send(
        &state,
        Request::get("/static/test-chat.js")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(script.status(), StatusCode::OK);
    let served_type = script
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_owned();
    assert!(
        served_type.starts_with("text/javascript"),
        "/static/test-chat.js served as {served_type}"
    );
    assert!(!body_bytes(script).await.is_empty());

    // Rendering the page never contacts the configured upstream.
    assert_eq!(upstream.hits.count(), 0);
    Ok(())
}

// ---------------------------------------------------------------------------
// POST /test/chat: fallback tracing and request rewriting
// ---------------------------------------------------------------------------

#[tokio::test]
async fn retryable_failure_falls_back_with_an_ordered_sanitized_trace() -> TestResult {
    let state = test_state().await;
    let first = spawn_upstream(error_status_router(StatusCode::SERVICE_UNAVAILABLE)).await;
    let second = spawn_upstream(openai_echo_router("fallback reply")).await;

    save_provider(
        &state,
        "prov-first",
        "First Connection",
        &first.url(),
        Protocol::OpenAi,
        true,
        "sk-first-secret",
    )
    .await?;
    save_provider(
        &state,
        "prov-second",
        "Second Connection",
        &second.url(),
        Protocol::OpenAi,
        true,
        "sk-second-secret",
    )
    .await?;
    combo_with_steps(
        &state,
        "coding",
        &[
            ("prov-first", "model-first"),
            ("prov-second", "model-second"),
        ],
    )
    .await?;

    // A multi-turn history plus unknown extension fields; streaming is
    // requested but must be forced off before the upstream call.
    let mut body = chat_body(
        &state,
        "combo/coding",
        json!([
            {"role": "user", "content": "Hi"},
            {"role": "assistant", "content": "Hello!"},
            {"role": "user", "content": "What else?"},
        ]),
    );
    body["stream"] = json!(true);
    body["temperature"] = json!(0.5);
    body["user_extension"] = json!({"nested": {"keep": [1, 2, 3]}});

    let (status, payload) = send_test_chat(&state, &body).await;
    assert_eq!(status, StatusCode::OK);

    // The completion is the healthy upstream's reply, verbatim.
    assert!(payload["error"].is_null());
    let completion = &payload["completion"];
    assert_eq!(
        completion["choices"][0]["message"]["content"],
        "fallback reply"
    );
    assert_eq!(completion["model"], "model-second");

    // The trace records both attempts in request order, with the configured
    // 1-based step positions, display names, and outcomes.
    let attempts = payload["attempts"].as_array().expect("attempts array");
    assert_eq!(attempts.len(), 2);
    assert_eq!(attempts[0]["provider_id"], "prov-first");
    assert_eq!(attempts[0]["provider_name"], "First Connection");
    assert_eq!(attempts[0]["model"], "model-first");
    assert_eq!(attempts[0]["position"], 1);
    assert_eq!(attempts[0]["outcome"], "retryable_failure");
    assert!(
        attempts[0]["latency_ms"].is_u64(),
        "latency must serialize as a number"
    );
    assert_eq!(attempts[1]["provider_id"], "prov-second");
    assert_eq!(attempts[1]["provider_name"], "Second Connection");
    assert_eq!(attempts[1]["model"], "model-second");
    assert_eq!(attempts[1]["position"], 2);
    assert_eq!(attempts[1]["outcome"], "selected");
    assert!(attempts[1]["latency_ms"].is_u64());

    // The healthy upstream received the rewritten request: the CSRF token
    // stripped, streaming forced off, the step model substituted, and the
    // full history plus unknown extension fields preserved.
    let seen = &completion["upstream_saw"];
    assert!(
        seen.get("_csrf").is_none(),
        "the CSRF token must never be forwarded upstream"
    );
    assert_eq!(seen["stream"], false, "streaming must be forced off");
    assert_eq!(seen["model"], "model-second");
    assert_eq!(
        seen["messages"],
        json!([
            {"role": "user", "content": "Hi"},
            {"role": "assistant", "content": "Hello!"},
            {"role": "user", "content": "What else?"},
        ])
    );
    assert_eq!(seen["temperature"], 0.5);
    assert_eq!(seen["user_extension"]["nested"]["keep"], json!([1, 2, 3]));

    // The trace stays sanitized: no credentials, endpoint URLs, or upstream
    // error bodies are added to it.
    let attempts_text = payload["attempts"].to_string();
    assert!(!attempts_text.contains("sk-first-secret"));
    assert!(!attempts_text.contains("sk-second-secret"));
    assert!(
        !attempts_text.contains("http"),
        "the trace must not carry endpoint URLs"
    );
    assert!(
        !attempts_text.contains("mock status"),
        "the trace must not carry upstream error bodies"
    );

    assert_eq!(first.hits.count(), 1);
    assert_eq!(second.hits.count(), 1);
    Ok(())
}

#[tokio::test]
async fn permanent_failure_stops_the_chain_with_one_sanitized_attempt() -> TestResult {
    let state = test_state().await;
    let rejecting = spawn_upstream(error_status_router(StatusCode::BAD_REQUEST)).await;
    let healthy = spawn_upstream(openai_echo_router("must never be used")).await;

    save_provider(
        &state,
        "prov-reject",
        "Rejecting Connection",
        &rejecting.url(),
        Protocol::OpenAi,
        true,
        "sk-reject-secret",
    )
    .await?;
    save_provider(
        &state,
        "prov-healthy",
        "Healthy Connection",
        &healthy.url(),
        Protocol::OpenAi,
        true,
        "sk-healthy-secret",
    )
    .await?;
    combo_with_steps(
        &state,
        "coding",
        &[
            ("prov-reject", "model-reject"),
            ("prov-healthy", "model-healthy"),
        ],
    )
    .await?;

    let (status, payload) = send_test_chat(
        &state,
        &chat_body(
            &state,
            "combo/coding",
            json!([{"role": "user", "content": "Hi"}]),
        ),
    )
    .await;

    // The upstream's 400 status reaches the caller unchanged.
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(payload["completion"].is_null());
    let error = &payload["error"];
    assert_eq!(error["code"], 400);
    assert_eq!(error["type"], "upstream_error");
    let message = error["message"].as_str().unwrap_or_default();
    assert!(
        !message.contains("mock status 400"),
        "the raw upstream error body must stay hidden, got: {message}"
    );
    assert!(
        message.contains("400"),
        "the sanitized message still names the upstream status, got: {message}"
    );

    // A permanent client error stops the chain: exactly one trace entry and
    // no contact with the healthy second step.
    let attempts = payload["attempts"].as_array().expect("attempts array");
    assert_eq!(
        attempts.len(),
        1,
        "a permanent failure must stop the fallback chain"
    );
    assert_eq!(attempts[0]["provider_id"], "prov-reject");
    assert_eq!(attempts[0]["provider_name"], "Rejecting Connection");
    assert_eq!(attempts[0]["model"], "model-reject");
    assert_eq!(attempts[0]["position"], 1);
    assert_eq!(attempts[0]["outcome"], "permanent_failure");

    assert_eq!(rejecting.hits.count(), 1);
    assert_eq!(healthy.hits.count(), 0);
    Ok(())
}

#[tokio::test]
async fn disabled_steps_leave_no_trace_and_empty_combos_fail_without_attempts() -> TestResult {
    let state = test_state().await;
    let disabled = spawn_upstream(openai_echo_router("disabled reply")).await;
    let enabled = spawn_upstream(openai_echo_router("enabled reply")).await;

    save_provider(
        &state,
        "prov-off",
        "Disabled Connection",
        &disabled.url(),
        Protocol::OpenAi,
        false,
        "sk-off-secret",
    )
    .await?;
    save_provider(
        &state,
        "prov-on",
        "Enabled Connection",
        &enabled.url(),
        Protocol::OpenAi,
        true,
        "sk-on-secret",
    )
    .await?;
    combo_with_steps(
        &state,
        "coding",
        &[("prov-off", "model-off"), ("prov-on", "model-on")],
    )
    .await?;

    let (status, payload) = send_test_chat(
        &state,
        &chat_body(
            &state,
            "combo/coding",
            json!([{"role": "user", "content": "Hi"}]),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        payload["completion"]["choices"][0]["message"]["content"],
        "enabled reply"
    );

    // Disabled steps are skipped without a trace entry, and the surviving
    // attempt keeps its configured 1-based position.
    let attempts = payload["attempts"].as_array().expect("attempts array");
    assert_eq!(attempts.len(), 1, "disabled steps must not be traced");
    assert_eq!(attempts[0]["provider_id"], "prov-on");
    assert_eq!(attempts[0]["provider_name"], "Enabled Connection");
    assert_eq!(attempts[0]["model"], "model-on");
    assert_eq!(
        attempts[0]["position"], 2,
        "positions stay the configured step numbers, not attempt ordinals"
    );
    assert_eq!(attempts[0]["outcome"], "selected");
    assert_eq!(
        disabled.hits.count(),
        0,
        "a disabled step's server must never be contacted"
    );
    assert_eq!(enabled.hits.count(), 1);

    // A combo with no steps at all fails routing with an empty trace.
    combos::create(&state.db, "empty").await?;
    let (status, payload) = send_test_chat(
        &state,
        &chat_body(
            &state,
            "combo/empty",
            json!([{"role": "user", "content": "Hi"}]),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
    assert!(payload["completion"].is_null());
    assert_eq!(payload["error"]["code"], 502);
    assert_eq!(
        payload["attempts"].as_array().map(Vec::len),
        Some(0),
        "an empty combo has no attempts to trace"
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// Shared routing strategy state with the public API
// ---------------------------------------------------------------------------

#[tokio::test]
async fn round_robin_rotation_is_shared_between_test_chat_and_the_public_api() -> TestResult {
    let state = test_state().await;
    let first = spawn_upstream(openai_echo_router("first reply")).await;
    let second = spawn_upstream(openai_echo_router("second reply")).await;

    save_provider(
        &state,
        "prov-a",
        "Rotating One",
        &first.url(),
        Protocol::OpenAi,
        true,
        "sk-rotate-a",
    )
    .await?;
    save_provider(
        &state,
        "prov-b",
        "Rotating Two",
        &second.url(),
        Protocol::OpenAi,
        true,
        "sk-rotate-b",
    )
    .await?;
    combo_with_strategy(
        &state,
        "robin",
        &[("prov-a", "model-a"), ("prov-b", "model-b")],
        Strategy::RoundRobin,
    )
    .await?;

    // Dashboard test call #1 starts at the first step.
    let (status, payload) = send_test_chat(
        &state,
        &chat_body(
            &state,
            "combo/robin",
            json!([{"role": "user", "content": "Hi"}]),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload["attempts"][0]["model"], "model-a");
    assert_eq!(payload["completion"]["model"], "model-a");

    // A public /v1 call continues the same shared cursor at the second step.
    let response = send(
        &state,
        v1_chat_request(&json!({
            "model": "combo/robin",
            "messages": [{"role": "user", "content": "hello"}],
        })),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(json_body(response).await["model"], "model-b");

    // Dashboard test call #2 wraps back to the first step: one process-wide
    // cursor serves both entry points.
    let (status, payload) = send_test_chat(
        &state,
        &chat_body(
            &state,
            "combo/robin",
            json!([{"role": "user", "content": "Again"}]),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload["attempts"][0]["model"], "model-a");
    assert_eq!(payload["completion"]["model"], "model-a");

    assert_eq!(first.hits.count(), 2);
    assert_eq!(second.hits.count(), 1);
    Ok(())
}

// ---------------------------------------------------------------------------
// Security: CSRF, origin, and gateway authentication
// ---------------------------------------------------------------------------

#[tokio::test]
async fn csrf_and_cross_origin_submissions_are_rejected_before_any_upstream_call() -> TestResult {
    let state = test_state().await;
    let upstream = spawn_upstream(openai_echo_router("never used")).await;
    save_provider(
        &state,
        "prov-guarded",
        "Guarded Connection",
        &upstream.url(),
        Protocol::OpenAi,
        true,
        "sk-guarded",
    )
    .await?;
    combo_with_steps(&state, "coding", &[("prov-guarded", "model-guarded")]).await?;

    let messages = json!([{"role": "user", "content": "Hi"}]);

    // A missing CSRF token is rejected with the plain error shape.
    let mut missing = chat_body(&state, "combo/coding", messages.clone());
    missing.as_object_mut().unwrap().remove("_csrf");
    let (status, payload) = send_test_chat(&state, &missing).await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "a missing CSRF token must be rejected"
    );
    assert!(
        payload.get("attempts").is_none(),
        "security rejections use the plain error shape, not the trace envelope"
    );
    assert_eq!(payload["error"]["code"], 400);

    // So is a wrong token.
    let mut wrong = chat_body(&state, "combo/coding", messages.clone());
    wrong["_csrf"] = json!("definitely-not-the-token");
    let (status, _) = send_test_chat(&state, &wrong).await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "an invalid CSRF token must be rejected"
    );

    // Cross-site submissions are rejected by the dashboard origin middleware
    // even with a valid CSRF token.
    let valid = chat_body(&state, "combo/coding", messages.clone());
    let cross_site = test_chat_builder()
        .header(header::HOST, "127.0.0.1:8080")
        .header("sec-fetch-site", "cross-site")
        .body(Body::from(valid.to_string()))
        .unwrap();
    let response = send(&state, cross_site).await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let _ = body_bytes(response).await;

    let foreign_origin = test_chat_builder()
        .header(header::HOST, "127.0.0.1:8080")
        .header(header::ORIGIN, "https://attacker.example")
        .body(Body::from(valid.to_string()))
        .unwrap();
    let response = send(&state, foreign_origin).await;
    assert_eq!(
        response.status(),
        StatusCode::BAD_REQUEST,
        "a foreign Origin against our Host must be rejected"
    );
    let _ = body_bytes(response).await;

    // The very same payload from the dashboard origin succeeds, proving the
    // middleware distinguishes origins rather than rejecting all headers.
    let same_origin = test_chat_builder()
        .header(header::HOST, "127.0.0.1:8080")
        .header(header::ORIGIN, "http://127.0.0.1:8080")
        .header("sec-fetch-site", "same-origin")
        .body(Body::from(valid.to_string()))
        .unwrap();
    let response = send(&state, same_origin).await;
    assert_eq!(response.status(), StatusCode::OK);
    let _ = body_bytes(response).await;

    assert_eq!(
        upstream.hits.count(),
        1,
        "only the accepted request may reach the upstream"
    );
    Ok(())
}

#[tokio::test]
async fn test_chat_requires_gateway_authentication_even_though_it_lives_in_the_dashboard()
-> TestResult {
    let mut state = test_state().await;
    let upstream = spawn_upstream(openai_echo_router("authed reply")).await;
    save_provider(
        &state,
        "prov-auth",
        "Auth Connection",
        &upstream.url(),
        Protocol::OpenAi,
        true,
        "sk-auth-upstream",
    )
    .await?;
    combo_with_steps(&state, "coding", &[("prov-auth", "model-auth")]).await?;
    state.api_key = Some(Arc::new(SecretString::from("gateway-secret")));

    // The page itself stays reachable without credentials, but now offers a
    // gateway key field for the billable action.
    let page = get_page(&state, "/test").await;
    assert!(
        page.contains("id=\"test-key\""),
        "an authenticated gateway shows the key field on the test page"
    );
    let controls = &page[page.find("class=\"test-controls\"").unwrap()
        ..page.find("id=\"test-target-hint\"").unwrap()];
    assert!(
        controls.find("id=\"test-combo\"").unwrap() < controls.find("id=\"test-key\"").unwrap()
    );
    assert!(
        controls.find("id=\"test-key\"").unwrap() < controls.find("id=\"test-reset\"").unwrap()
    );
    assert_eq!(
        page.matches("aria-describedby=\"test-target-hint\"")
            .count(),
        2
    );
    assert!(
        page.contains("Gateway key required; sent with requests, never saved in browser storage.")
    );
    assert!(!page.contains("id=\"test-key-hint\""));
    assert!(!page.contains("Gateway authentication"));
    assert!(!page.contains("class=\"test-auth\""));

    let valid = chat_body(
        &state,
        "combo/coding",
        json!([{"role": "user", "content": "Hi"}]),
    );

    // A valid CSRF token alone is not enough: the gateway key is required
    // exactly like /v1, and dashboard access is not a credential.
    let (status, payload) = send_test_chat(&state, &valid).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert!(payload.get("attempts").is_none());
    assert_eq!(payload["error"]["code"], 401);

    let wrong = test_chat_builder()
        .header(header::AUTHORIZATION, "Bearer wrong-secret")
        .body(Body::from(valid.to_string()))
        .unwrap();
    let response = send(&state, wrong).await;
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    let _ = body_bytes(response).await;

    // The correct environment key unlocks the endpoint and still reports the
    // attempt trace alongside the completion.
    let authed = test_chat_builder()
        .header(header::AUTHORIZATION, "Bearer gateway-secret")
        .body(Body::from(valid.to_string()))
        .unwrap();
    let response = send(&state, authed).await;
    assert_eq!(response.status(), StatusCode::OK);
    let payload = json_body(response).await;
    assert!(payload["error"].is_null());
    assert_eq!(
        payload["completion"]["choices"][0]["message"]["content"],
        "authed reply"
    );
    assert_eq!(payload["attempts"][0]["provider_name"], "Auth Connection");
    assert_eq!(payload["attempts"][0]["outcome"], "selected");

    assert_eq!(
        upstream.hits.count(),
        1,
        "only the authenticated request may reach the upstream"
    );
    Ok(())
}

#[tokio::test]
async fn generated_api_keys_gate_test_chat_and_revocation_reopens_it() -> TestResult {
    let state = test_state().await;
    let upstream = spawn_upstream(openai_echo_router("generated reply")).await;
    save_provider(
        &state,
        "prov-gen",
        "Generated Connection",
        &upstream.url(),
        Protocol::OpenAi,
        true,
        "sk-gen-upstream",
    )
    .await?;
    combo_with_steps(&state, "coding", &[("prov-gen", "model-gen")]).await?;

    let valid = chat_body(
        &state,
        "combo/coding",
        json!([{"role": "user", "content": "Hi"}]),
    );

    // Open gateway before any key exists: the dashboard test call works
    // without an Authorization header.
    let (status, _) = send_test_chat(&state, &valid).await;
    assert_eq!(status, StatusCode::OK);

    // Issuing a generated key closes the gateway for /test/chat too.
    let issued = api_keys::create(&state.db, &state.crypto, "test client").await?;
    let (status, payload) = send_test_chat(&state, &valid).await;
    assert_eq!(
        status,
        StatusCode::UNAUTHORIZED,
        "dashboard access must not authorize billable test calls"
    );
    assert!(payload.get("attempts").is_none());

    // The generated secret authenticates like any /v1 key.
    let secret = issued.secret.expose_secret().to_owned();
    let authed = test_chat_builder()
        .header(header::AUTHORIZATION, format!("Bearer {secret}"))
        .body(Body::from(valid.to_string()))
        .unwrap();
    let response = send(&state, authed).await;
    assert_eq!(response.status(), StatusCode::OK);
    let payload = json_body(response).await;
    assert_eq!(
        payload["completion"]["choices"][0]["message"]["content"],
        "generated reply"
    );
    assert_eq!(payload["attempts"][0]["provider_id"], "prov-gen");

    // Revoking the last generated key reopens the endpoint.
    api_keys::delete(&state.db, &issued.id).await?;
    let (status, _) = send_test_chat(&state, &valid).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "revoking the last generated key reopens the gateway when no environment key is set"
    );

    assert_eq!(upstream.hits.count(), 3);
    Ok(())
}

// ---------------------------------------------------------------------------
// Validation: rejections never reach an upstream
// ---------------------------------------------------------------------------

#[tokio::test]
async fn model_and_message_validation_reject_without_touching_any_upstream() -> TestResult {
    let state = test_state().await;
    let upstream = spawn_upstream(openai_echo_router("never used")).await;
    save_provider(
        &state,
        "prov-validate",
        "Validation Connection",
        &upstream.url(),
        Protocol::OpenAi,
        true,
        "sk-validate",
    )
    .await?;
    combo_with_steps(&state, "coding", &[("prov-validate", "model-validate")]).await?;

    let valid_messages = json!([{"role": "user", "content": "Hi"}]);

    // The playground only routes combos: bare direct model ids and the empty
    // combo prefix are rejected before any routing work.
    for model in ["gpt-direct-model", "combo/"] {
        let (status, payload) =
            send_test_chat(&state, &chat_body(&state, model, valid_messages.clone())).await;
        assert_eq!(
            status,
            StatusCode::BAD_REQUEST,
            "model {model} must be rejected"
        );
        assert!(
            payload["error"]["message"]
                .as_str()
                .unwrap_or_default()
                .contains("combo"),
            "the rejection must point at the combo picker"
        );
        assert!(payload.get("attempts").is_none());
    }

    // Unknown combo names fail inside routing with an empty trace envelope.
    let (status, payload) = send_test_chat(
        &state,
        &chat_body(
            &state,
            "combo/no-such-combo",
            json!([{"role": "user", "content": "Hi"}]),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(payload["completion"].is_null());
    assert_eq!(payload["error"]["code"], 404);
    assert_eq!(payload["attempts"].as_array().map(Vec::len), Some(0));

    // Message shape validation: empty, missing, wrong roles, blank content,
    // and missing content are all rejected.
    for messages in [
        json!([]),
        json!([{"role": "system", "content": "no system role here"}]),
        json!([{"role": "user", "content": "   "}]),
        json!([{"role": "user"}]),
        Value::Null,
    ] {
        let (status, _) =
            send_test_chat(&state, &chat_body(&state, "combo/coding", messages)).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }

    // Malformed JSON bodies are rejected by the extractor.
    let malformed = test_chat_builder()
        .body(Body::from("{definitely not json"))
        .unwrap();
    let response = send(&state, malformed).await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let _ = body_bytes(response).await;

    assert_eq!(
        upstream.hits.count(),
        0,
        "validation rejections must never reach an upstream"
    );
    Ok(())
}
