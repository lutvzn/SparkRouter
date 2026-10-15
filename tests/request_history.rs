//! Integration tests for default Basic metadata and opt-in request-body recording.
//!
//! These drive `app::router(state).oneshot(request)` against real local Axum
//! upstream mocks (bound to 127.0.0.1:0), following the gateway suite's RAII
//! server pattern. Every assertion reads rows back through the real SQLite
//! database behind the assembled router, so the recording path exercised is
//! exactly the one production uses (`history::record_response` wrapping the
//! routed response), not a unit-level call.
//!
//! Basic metadata rows/schema/pages exclude prompts, completions, credentials,
//! endpoint URLs and upstream errors. Explicit Extended tests additionally check
//! sensitive body capture, limits, and lifecycle without changing response bytes. The insert is asynchronous, so readers yield until the expected
//! row exists. Stream and payload finalization are spawned too, so the end of a
//! body never waits on SQLite; tests observe them with bounded yield-and-query
//! loops, with no sleeps anywhere.

mod common;
use common::{body_bytes, json_body, spawn_upstream};

use std::sync::Arc;

use axum::{
    Router,
    body::Body,
    extract::Request,
    http::{Method, StatusCode, header},
    response::Response,
    routing::post,
};
use base64::{Engine, engine::general_purpose::STANDARD};
use bytes::Bytes;
use futures_util::StreamExt;
use secrecy::SecretString;
use serde_json::{Value, json};
use sqlx::Row;
use tower::ServiceExt;

use sparkrouter::{
    app::{self, AppState},
    config::Config,
    crypto::Crypto,
    db::{
        combos,
        connection_models::{self, StoredModel},
        providers as provider_db,
        request_history::{self, Record},
    },
    providers::Protocol,
};

type BoxError = Box<dyn std::error::Error + Send + Sync>;
type TestResult = Result<(), BoxError>;

const MASTER_KEY: &[u8; 32] = &[91; 32];

// Distinctive markers that must transit the gateway but never reach a row,
// a column name, or the rendered history page.
const PROMPT_MARKER: &str = "top-secret-prompt-marker-8675309";
const REPLY_MARKER: &str = "mock-reply-marker-246810";
const UPSTREAM_KEY_A: &str = "sk-history-secret-alpha";
const UPSTREAM_KEY_B: &str = "sk-history-secret-beta";

#[tokio::test]
async fn unavailable_history_does_not_fail_or_modify_a_generation() -> TestResult {
    let state = test_state().await;
    let expected = "{\"choices\":[],\"usage\":{\"prompt_tokens\":1}}";
    let upstream = spawn_upstream(captured_json_router(StatusCode::OK, expected).0).await;
    save_provider(
        &state,
        "provider",
        "Provider",
        &upstream.url(),
        Protocol::OpenAi,
        true,
        UPSTREAM_KEY_A,
    )
    .await?;
    combo_with_steps(&state, "coding", &[("provider", "model")]).await?;
    sqlx::query("CREATE TRIGGER reject_history BEFORE INSERT ON request_history BEGIN SELECT RAISE(ABORT, 'test history unavailable'); END").execute(&state.db).await?;
    let response = send(
        &state,
        chat_request(
            &json!({"model":"combo/coding","messages":[{"role":"user","content":"hello"}]}),
        ),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        body_bytes(response).await,
        Bytes::from_static(expected.as_bytes())
    );
    assert_eq!(request_history::count(&state.db).await?, 0);
    assert_eq!(upstream.hits.count(), 1);
    Ok(())
}

#[tokio::test]
async fn rotated_combo_history_preserves_actual_attempt_order() -> TestResult {
    let state = test_state().await;
    let healthy = spawn_upstream(captured_json_router(StatusCode::OK, "{\"choices\":[]}").0).await;
    let retry = spawn_upstream(error_status_router(StatusCode::SERVICE_UNAVAILABLE)).await;
    for (id, endpoint) in [
        ("healthy", healthy.url()),
        ("retry-b", retry.url()),
        ("retry-c", retry.url()),
    ] {
        save_provider(
            &state,
            id,
            id,
            &endpoint,
            Protocol::OpenAi,
            true,
            UPSTREAM_KEY_A,
        )
        .await?;
    }
    combo_with_steps(
        &state,
        "rotate",
        &[("healthy", "a"), ("retry-b", "b"), ("retry-c", "c")],
    )
    .await?;
    let combo = combos::by_name(&state.db, "rotate").await?;
    combos::set_strategy(&state.db, &combo.id, combos::Strategy::RoundRobin).await?;
    let body = json!({"model":"combo/rotate", "messages":[{"role":"user","content":"hello"}]});
    let response = send(&state, chat_request(&body)).await;
    assert_eq!(response.status(), StatusCode::OK);
    let _ = body_bytes(response).await;
    let _ = single_record(&state).await;
    sqlx::query("DELETE FROM request_history")
        .execute(&state.db)
        .await?;
    let response = send(&state, chat_request(&body)).await;
    assert_eq!(response.status(), StatusCode::OK);
    let _ = body_bytes(response).await;
    let row = single_record(&state).await;
    assert_eq!(
        row.attempts
            .iter()
            .map(|attempt| attempt.position)
            .collect::<Vec<_>>(),
        [2, 3, 1]
    );
    assert_eq!(
        row.attempts
            .iter()
            .map(|attempt| attempt.outcome.as_str())
            .collect::<Vec<_>>(),
        ["retryable_failure", "retryable_failure", "selected"]
    );
    assert_eq!(healthy.hits.count(), 2);
    assert_eq!(retry.hits.count(), 2);
    Ok(())
}

#[tokio::test]
async fn history_survives_restart_and_orphaned_streams_are_interrupted() -> TestResult {
    let directory = tempfile::tempdir()?;
    let database_url = format!(
        "sqlite://{}",
        directory
            .path()
            .join("history.db")
            .to_string_lossy()
            .replace('\\', "/")
    );
    let state = AppState::from_config(test_config(&database_url)).await?;
    let record = Record {
        id: "orphan".into(),
        source: "api".into(),
        model: "combo/coding".into(),
        streaming: true,
        outcome: "streaming".into(),
        status: 200,
        latency_ms: 12,
        input_tokens: None,
        output_tokens: None,
        attempts: vec![],
    };
    request_history::save(&state.db, &record).await?;
    state.db.close().await;
    drop(state);
    let reopened = AppState::from_config(test_config(&database_url)).await?;
    let row = single_record(&reopened).await;
    assert_eq!(row.id, "orphan");
    assert_eq!(row.outcome, "interrupted");
    reopened.db.close().await;
    Ok(())
}

#[tokio::test]
async fn logging_modes_preserve_bytes_and_capture_only_when_extended() -> TestResult {
    use sparkrouter::db::{
        request_payloads,
        settings::{self, LoggingMode},
    };
    let state = test_state().await;
    let expected =
        r#"{"choices":[{"message":{"content":"<script>reply</script>"}}],"unknown":true}"#;
    let upstream = spawn_upstream(captured_json_router(StatusCode::OK, expected).0).await;
    save_provider(
        &state,
        "p",
        "p",
        &upstream.url(),
        Protocol::OpenAi,
        true,
        UPSTREAM_KEY_A,
    )
    .await?;
    combo_with_steps(&state, "coding", &[("p", "upstream/model")]).await?;
    let body = json!({"model":"combo/coding", "messages":[{"role":"user","content":PROMPT_MARKER}], "unknown":{"nested":true}});
    assert!(settings::logging_mode(&state.db).await? == LoggingMode::Basic);
    let response = send(&state, chat_request(&body)).await;
    assert_eq!(body_bytes(response).await, expected);
    let basic = single_record(&state).await;
    assert!(request_payloads::get(&state.db, &basic.id).await?.is_none());
    settings::set_logging_mode(&state.db, LoggingMode::Off).await?;
    assert_eq!(
        body_bytes(send(&state, chat_request(&body)).await).await,
        expected
    );
    assert_eq!(request_history::count(&state.db).await?, 1);
    settings::set_logging_mode(&state.db, LoggingMode::Extended).await?;
    let reply = json_body(send(&state, test_chat_request(&state, &body)).await).await;
    assert_eq!(
        reply["completion"],
        serde_json::from_str::<Value>(expected)?
    );
    let records = wait_for_count(&state, 2).await;
    let record = records.iter().find(|r| r.source == "test").unwrap();
    let payload = finished_payload(&state, &record.id).await;
    assert!(!payload.response_truncated);
    assert_eq!(payload.response_body, expected);
    let captured: Value = serde_json::from_str(&payload.request_body)?;
    assert_eq!(captured["model"], "combo/coding");
    assert_eq!(captured["unknown"], body["unknown"]);
    assert!(captured.get("_csrf").is_none());
    assert!(!payload.request_body.contains(UPSTREAM_KEY_A));
    let response = send(
        &state,
        Request::get(format!("/history/{}/payload", record.id))
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
    let html = String::from_utf8(body_bytes(response).await.to_vec())?;
    assert!(html.contains(PROMPT_MARKER));
    assert!(html.contains("&#60;script&#62;"));
    assert!(!html.contains("<script>reply</script>"));
    settings::set_history_retention(&state.db, 1).await?;
    assert!(request_payloads::get(&state.db, &basic.id).await?.is_none());
    request_history::clear(&state.db).await?;
    assert!(
        request_payloads::get(&state.db, &record.id)
            .await?
            .is_none()
    );
    assert_eq!(upstream.hits.count(), 3);
    Ok(())
}

#[tokio::test]
async fn extended_stream_capture_is_snapshot_based_and_byte_exact() -> TestResult {
    use sparkrouter::db::settings::{self, LoggingMode};
    let state = test_state().await;
    let chunks = vec![
        "data: {\"choices\":[{\"delta\":{\"content\":\"hello\"}}]}\n",
        "\ndata: [DO",
        "NE]\n\n",
    ];
    let expected = chunks.concat();
    let upstream = spawn_upstream(sse_router(chunks)).await;
    save_provider(
        &state,
        "p",
        "p",
        &upstream.url(),
        Protocol::OpenAi,
        true,
        UPSTREAM_KEY_A,
    )
    .await?;
    combo_with_steps(&state, "coding", &[("p", "m")]).await?;
    settings::set_logging_mode(&state.db, LoggingMode::Extended).await?;
    let response = send(
        &state,
        chat_request(&json!({"model":"combo/coding","stream":true,"messages":[]})),
    )
    .await;
    settings::set_logging_mode(&state.db, LoggingMode::Off).await?;
    assert_eq!(body_bytes(response).await, expected);
    let record = wait_until(&state, |r| r.outcome == "success").await;
    let payload = finished_payload(&state, &record.id).await;
    assert_eq!(payload.response_body, expected);
    Ok(())
}

#[tokio::test]
async fn extended_capture_is_bounded_without_truncating_client_response() -> TestResult {
    use sparkrouter::db::{
        request_payloads,
        settings::{self, LoggingMode},
    };
    let state = test_state().await;
    let expected = json!({"choices":[{"message":{"content":"x".repeat(100_000)}}]}).to_string();
    let upstream = spawn_upstream(captured_json_router(StatusCode::OK, &expected).0).await;
    save_provider(
        &state,
        "p",
        "p",
        &upstream.url(),
        Protocol::OpenAi,
        true,
        UPSTREAM_KEY_A,
    )
    .await?;
    combo_with_steps(&state, "coding", &[("p", "m")]).await?;
    settings::set_logging_mode(&state.db, LoggingMode::Extended).await?;
    let body =
        json!({"model":"combo/coding","messages":[{"role":"user","content":"é".repeat(50_000)}]});
    assert_eq!(
        body_bytes(send(&state, chat_request(&body)).await).await,
        expected
    );
    let record = single_record(&state).await;
    let payload = finished_payload(&state, &record.id).await;
    assert!(payload.request_truncated && payload.response_truncated);
    assert!(payload.request_body.len() <= request_payloads::BODY_LIMIT);
    assert_eq!(payload.response_body.len(), request_payloads::BODY_LIMIT);
    Ok(())
}

#[tokio::test]
async fn extended_cancelled_and_interrupted_streams_keep_partial_captures() -> TestResult {
    use sparkrouter::db::{
        request_payloads,
        settings::{self, LoggingMode},
    };
    let state = test_state().await;
    let upstream = spawn_upstream(sse_router(vec!["data: {\"choices\":[]}\n\n"])).await;
    save_provider(
        &state,
        "p",
        "p",
        &upstream.url(),
        Protocol::OpenAi,
        true,
        UPSTREAM_KEY_A,
    )
    .await?;
    combo_with_steps(&state, "coding", &[("p", "m")]).await?;
    settings::set_logging_mode(&state.db, LoggingMode::Extended).await?;
    let body = json!({"model":"combo/coding","stream":true,"messages":[]});
    let response = send(&state, chat_request(&body)).await;
    let mut stream = response.into_body().into_data_stream();
    assert!(stream.next().await.unwrap().is_ok());
    assert!(stream.next().await.unwrap().is_err());
    drop(stream);
    let record = wait_until(&state, |r| r.outcome == "interrupted").await;
    let payload = request_payloads::get(&state.db, &record.id).await?.unwrap();
    assert!(!payload.response_complete);
    assert_eq!(payload.response_body, "data: {\"choices\":[]}\n\n");
    request_history::clear(&state.db).await?;
    drop(send(&state, chat_request(&body)).await);
    let record = wait_until(&state, |r| r.outcome == "cancelled").await;
    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        loop {
            if let Some(payload) = request_payloads::get(&state.db, &record.id).await.unwrap() {
                assert!(!payload.response_complete);
                assert!(payload.response_body.is_empty());
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await?;
    assert_eq!(upstream.hits.count(), 2);
    Ok(())
}

#[tokio::test]
async fn extended_excludes_raw_upstream_errors_and_payload_failure_does_not_break_metadata()
-> TestResult {
    use sparkrouter::db::{
        request_payloads,
        settings::{self, LoggingMode},
    };
    let state = test_state().await;
    let upstream = spawn_upstream(error_status_router(StatusCode::BAD_REQUEST)).await;
    save_provider(
        &state,
        "p",
        "p",
        &upstream.url(),
        Protocol::OpenAi,
        true,
        UPSTREAM_KEY_A,
    )
    .await?;
    combo_with_steps(&state, "coding", &[("p", "m")]).await?;
    settings::set_logging_mode(&state.db, LoggingMode::Extended).await?;
    let response = send(
        &state,
        chat_request(&json!({"model":"combo/coding","messages":[]})),
    )
    .await;
    assert!(!response.status().is_success());
    let expected = String::from_utf8(body_bytes(response).await.to_vec())?;
    let record = single_record(&state).await;
    let payload = finished_payload(&state, &record.id).await;
    assert_eq!(payload.response_body, expected);
    assert!(!payload.response_body.contains("mock status 400"));
    assert!(!payload.response_body.contains("mock_error"));
    assert!(!payload.response_body.contains(UPSTREAM_KEY_A));
    request_history::clear(&state.db).await?;
    let chunks = vec!["data: {\"choices\":[]}\n\n", "data: [DONE]\n\n"];
    let expected = chunks.concat();
    let healthy = spawn_upstream(sse_router(chunks)).await;
    save_provider(
        &state,
        "p",
        "p",
        &healthy.url(),
        Protocol::OpenAi,
        true,
        UPSTREAM_KEY_A,
    )
    .await?;
    sqlx::query("CREATE TRIGGER reject_payload BEFORE INSERT ON request_payloads BEGIN SELECT RAISE(ABORT, 'payload unavailable'); END").execute(&state.db).await?;
    let response = send(
        &state,
        chat_request(&json!({"model":"combo/coding","stream":true,"messages":[]})),
    )
    .await;
    assert_eq!(body_bytes(response).await, expected);
    let record = wait_until(&state, |r| r.outcome == "success").await;
    assert!(
        request_payloads::get(&state.db, &record.id)
            .await?
            .is_none()
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// Fixtures
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

/// Store a connection row with an explicit display name, so attempt
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

// ---------------------------------------------------------------------------
// Mock upstream servers (RAII: dropping aborts the server task)
// ---------------------------------------------------------------------------

/// OpenAI-compatible JSON upstream that replies with the exact given body
/// bytes and records the request it received for unknown-field assertions.
fn captured_json_router(
    status: StatusCode,
    body: &str,
) -> (Router, Arc<std::sync::Mutex<Option<Value>>>) {
    let captured = Arc::new(std::sync::Mutex::new(None));
    let seen = captured.clone();
    let reply = body.to_owned();
    let router = Router::new().route(
        "/chat/completions",
        post(move |body_bytes: Bytes| {
            let seen = seen.clone();
            let reply = reply.clone();
            async move {
                *seen.lock().unwrap() =
                    Some(serde_json::from_slice(&body_bytes).unwrap_or(Value::Null));
                (status, [(header::CONTENT_TYPE, "application/json")], reply)
            }
        }),
    );
    (router, captured)
}

/// Upstream that always answers with the given HTTP status and an error body
/// whose distinctive text must never be recorded.
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

/// Upstream streaming a deterministic raw SSE body chunk by chunk; the chunk
/// boundaries deliberately split events (and the terminal `[DONE]` terminator)
/// so decoders must reassemble framing across reads.
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

/// POST a dashboard test-chat payload with the process CSRF token.
fn test_chat_request(state: &AppState, body: &Value) -> Request<Body> {
    let mut body = body.clone();
    body["_csrf"] = json!(state.csrf_token.as_str());
    Request::builder()
        .method(Method::POST)
        .uri("/test/chat")
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

// ---------------------------------------------------------------------------
// Reading recorded history back through the module the dashboard uses
// ---------------------------------------------------------------------------

/// The records recorded so far, newest first, with attempts attached in their
/// recorded order.
async fn history(state: &AppState) -> Vec<Record> {
    request_history::list_page(&state.db, 100, 0)
        .await
        .expect("list recorded history")
        .into_iter()
        .map(|stored| stored.record)
        .collect()
}

async fn wait_for_count(state: &AppState, count: usize) -> Vec<Record> {
    for _ in 0..2_000 {
        let records = history(state).await;
        if records.len() == count {
            return records;
        }
        tokio::task::yield_now().await;
    }
    panic!(
        "expected {count} history records, found {}",
        history(state).await.len()
    );
}

/// The single record recorded so far, asserting there is exactly one.
///
/// The insert no longer blocks the response, so this yields until that row
/// appears instead of racing the background write.
async fn single_record(state: &AppState) -> Record {
    for _ in 0..2_000 {
        let records = history(state).await;
        if records.len() == 1 {
            return records.into_iter().next().unwrap();
        }
        assert!(
            records.is_empty(),
            "expected exactly one history record, found {}",
            records.len()
        );
        tokio::task::yield_now().await;
    }
    panic!("expected exactly one history record, found none");
}

/// Drives the runtime with yields (never sleeps) while re-reading the record,
/// until `satisfied` holds or a bounded number of attempts elapse. This makes
/// waiting for the task spawned when an unpolled body is dropped deterministic.
async fn wait_until(state: &AppState, satisfied: impl Fn(&Record) -> bool) -> Record {
    for _ in 0..2_000 {
        let record = single_record(state).await;
        if satisfied(&record) {
            return record;
        }
        tokio::task::yield_now().await;
    }
    panic!(
        "history record never reached the expected outcome, still {:?}",
        single_record(state).await.outcome
    );
}

/// Payload captures are finalized in the background after the body ends, so
/// yield until the completed capture lands instead of racing that write.
async fn finished_payload(
    state: &AppState,
    id: &str,
) -> sparkrouter::db::request_payloads::Payload {
    for _ in 0..2_000 {
        if let Some(payload) = sparkrouter::db::request_payloads::get(&state.db, id)
            .await
            .expect("read payload")
            && payload.response_complete
        {
            return payload;
        }
        tokio::task::yield_now().await;
    }
    panic!("payload capture for {id} never completed");
}

/// Every value of every row of both history tables, stringified, so negative
/// assertions cover columns the typed reader above never parses.
async fn dump_history_text(state: &AppState) -> String {
    let mut dump = String::new();
    for table in ["request_history", "request_attempts"] {
        let columns: Vec<String> =
            sqlx::query_scalar(&format!("SELECT name FROM pragma_table_info('{table}')"))
                .fetch_all(&state.db)
                .await
                .expect("history table columns");
        assert!(!columns.is_empty(), "{table} must have columns");
        let selection = columns
            .iter()
            .map(|column| format!("CAST(\"{column}\" AS TEXT)"))
            .collect::<Vec<_>>()
            .join(", ");
        let rows = sqlx::query(&format!("SELECT {selection} FROM \"{table}\""))
            .fetch_all(&state.db)
            .await
            .expect("dump history table");
        for row in rows {
            for (index, column) in columns.iter().enumerate() {
                // Every selected expression is CAST(... AS TEXT), so plain
                // string decoding covers integers, booleans, and blobs too.
                let value: Option<String> =
                    row.try_get(index).expect("cast history column is text");
                dump.push_str(&format!("{table}.{column} = {value:?}\n"));
            }
        }
    }
    dump
}

// ---------------------------------------------------------------------------
// JSON fallback: usage, attempts, and byte-exact passthrough
// ---------------------------------------------------------------------------

#[tokio::test]
async fn json_fallback_records_success_usage_attempts_and_exact_bytes() -> TestResult {
    let state = test_state().await;
    let first = spawn_upstream(error_status_router(StatusCode::SERVICE_UNAVAILABLE)).await;
    let success_body = concat!(
        "{\"id\":\"chatcmpl-raw\",\"object\":\"chat.completion\",\"created\":1700000000,",
        "\"model\":\"model-second\",\"choices\":[{\"index\":0,\"message\":{\"role\":\"assistant\",",
        "\"content\":\"",
        "mock-reply-marker-246810",
        "\"},\"finish_reason\":\"stop\"}],",
        "\"usage\":{\"prompt_tokens\":100,\"completion_tokens\":20,\"total_tokens\":120,",
        "\"prompt_tokens_details\":{\"cached_tokens\":80},\"cost_usd\":0.012}}"
    );
    let (second_router, captured) = captured_json_router(StatusCode::OK, success_body);
    let second = spawn_upstream(second_router).await;

    save_provider(
        &state,
        "prov-first",
        "First Display",
        &first.url(),
        Protocol::OpenAi,
        true,
        UPSTREAM_KEY_A,
    )
    .await?;
    save_provider(
        &state,
        "prov-second",
        "Second Display",
        &second.url(),
        Protocol::OpenAi,
        true,
        UPSTREAM_KEY_B,
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

    let body = json!({
        "model": "combo/coding",
        "messages": [{"role": "user", "content": "top-secret-prompt-marker-8675309"}],
        "stream": false,
        "temperature": 0.5,
        "user_extension": {"nested": {"keep": [1, 2, 3]}},
    });
    let response = send(&state, chat_request(&body)).await;
    assert_eq!(response.status(), StatusCode::OK);

    // Non-streaming JSON responses pass through byte for byte: the body the
    // client collects is exactly the body the mock wrote.
    assert_eq!(body_bytes(response).await, Bytes::from(success_body));

    // The gateway really forwarded the prompt (and the replaced model), so the
    // metadata-only assertions below are not vacuous.
    let seen = captured.lock().unwrap().clone().expect("upstream called");
    assert_eq!(seen["model"], "model-second");
    assert_eq!(
        seen["messages"][0]["content"], PROMPT_MARKER,
        "the marked prompt must have reached the upstream"
    );
    assert_eq!(seen["temperature"], 0.5);
    assert_eq!(seen["user_extension"]["nested"]["keep"], json!([1, 2, 3]));

    // The record carries only metadata: source, requested model, streaming
    // flag, outcome, status, latency, and token counts.
    let row = single_record(&state).await;
    assert_eq!(row.source, "api");
    assert_eq!(row.model, "combo/coding");
    assert!(!row.streaming);
    assert_eq!(row.outcome, "success");
    assert_eq!(row.status, 200);
    assert!(row.latency_ms >= 0);
    assert_eq!(row.input_tokens, Some(100));
    assert_eq!(row.output_tokens, Some(20));
    let (cached, cost): (Option<i64>, Option<f64>) =
        sqlx::query_as("SELECT cached_tokens, cost_usd FROM request_history WHERE id = ?")
            .bind(&row.id)
            .fetch_one(&state.db)
            .await?;
    assert_eq!(cached, Some(80));
    assert_eq!(cost, Some(0.012));
    let overview = sparkrouter::db::usage::overview(&state.db).await?;
    assert_eq!(overview.input_tokens, Some(100));
    assert_eq!(overview.output_tokens, Some(20));
    assert_eq!(overview.cached_tokens, Some(80));
    assert_eq!(overview.cost_usd, Some(0.012));

    // Attempt snapshots: both steps, in request order, with configured
    // positions, display names, models, outcomes, and latencies.
    let attempts = &row.attempts;
    assert_eq!(attempts.len(), 2);
    assert_eq!(attempts[0].provider_id, "prov-first");
    assert_eq!(attempts[0].provider_name, "First Display");
    assert_eq!(attempts[0].model, "model-first");
    assert_eq!(attempts[0].position, 1);
    assert_eq!(attempts[0].outcome, "retryable_failure");
    assert!(attempts[0].latency_ms >= 0);
    assert_eq!(attempts[1].provider_id, "prov-second");
    assert_eq!(attempts[1].provider_name, "Second Display");
    assert_eq!(attempts[1].model, "model-second");
    assert_eq!(attempts[1].position, 2);
    assert_eq!(attempts[1].outcome, "selected");
    assert!(attempts[1].latency_ms >= 0);

    assert_eq!(first.hits.count(), 1);
    assert_eq!(second.hits.count(), 1);
    Ok(())
}

// ---------------------------------------------------------------------------
// Permanent errors stop the chain and record a failure
// ---------------------------------------------------------------------------

#[tokio::test]
async fn permanent_failure_stops_the_chain_and_records_failure() -> TestResult {
    let state = test_state().await;
    let first = spawn_upstream(error_status_router(StatusCode::BAD_REQUEST)).await;
    let second = spawn_upstream(captured_json_router(StatusCode::OK, "{\"usage\":{}}").0).await;

    save_provider(
        &state,
        "prov-bad",
        "Bad Display",
        &first.url(),
        Protocol::OpenAi,
        true,
        UPSTREAM_KEY_A,
    )
    .await?;
    save_provider(
        &state,
        "prov-good",
        "Good Display",
        &second.url(),
        Protocol::OpenAi,
        true,
        UPSTREAM_KEY_B,
    )
    .await?;
    combo_with_steps(
        &state,
        "coding",
        &[("prov-bad", "model-bad"), ("prov-good", "model-good")],
    )
    .await?;

    let body = json!({
        "model": "combo/coding",
        "messages": [{"role": "user", "content": "hi"}],
    });
    let response = send(&state, chat_request(&body)).await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let payload = json_body(response).await;
    assert_eq!(payload["error"]["code"], 400);

    // A permanent client error stops the chain: one recorded attempt, the
    // healthy second step is never contacted, and the record is a failure
    // carrying the upstream status and no token counts.
    let row = single_record(&state).await;
    assert_eq!(row.source, "api");
    assert_eq!(row.model, "combo/coding");
    assert!(!row.streaming);
    assert_eq!(row.outcome, "failure");
    assert_eq!(row.status, 400);
    assert_eq!(row.input_tokens, None);
    assert_eq!(row.output_tokens, None);

    let attempts = &row.attempts;
    assert_eq!(attempts.len(), 1, "a permanent failure must stop the chain");
    assert_eq!(attempts[0].provider_id, "prov-bad");
    assert_eq!(attempts[0].model, "model-bad");
    assert_eq!(attempts[0].position, 1);
    assert_eq!(attempts[0].outcome, "permanent_failure");

    assert_eq!(first.hits.count(), 1);
    assert_eq!(second.hits.count(), 0);
    Ok(())
}

// ---------------------------------------------------------------------------
// Direct model routing records each refreshed connection attempt
// ---------------------------------------------------------------------------

#[tokio::test]
async fn direct_model_records_each_refreshed_connection_attempt() -> TestResult {
    let state = test_state().await;
    let first = spawn_upstream(error_status_router(StatusCode::TOO_MANY_REQUESTS)).await;
    let second = spawn_upstream(
        captured_json_router(
            StatusCode::OK,
            "{\"id\":\"d\",\"choices\":[],\"usage\":{\"prompt_tokens\":4,\"completion_tokens\":2}}",
        )
        .0,
    )
    .await;

    save_provider(
        &state,
        "prov-one",
        "Oldest Connection",
        &first.url(),
        Protocol::OpenAi,
        true,
        UPSTREAM_KEY_A,
    )
    .await?;
    save_provider(
        &state,
        "prov-two",
        "Newest Connection",
        &second.url(),
        Protocol::OpenAi,
        true,
        UPSTREAM_KEY_B,
    )
    .await?;
    remember_model(&state, "prov-one", "openai/gpt-6-luna").await?;
    remember_model(&state, "prov-two", "openai/gpt-6-luna").await?;
    let group = sparkrouter::db::provider_groups::create_with_slug(
        &state.db,
        "History provider",
        &first.url(),
        &Protocol::OpenAi,
        Some("history-provider"),
    )
    .await?;
    for id in ["prov-one", "prov-two"] {
        sqlx::query("INSERT INTO provider_group_connections (provider_id, group_id) VALUES (?, ?)")
            .bind(id)
            .bind(&group.id)
            .execute(&state.db)
            .await?;
    }

    // Surrounding whitespace is trimmed by the endpoint; the record keeps the
    // trimmed model id the client asked for, not the step's substituted id.
    let body = json!({
        "model": " history-provider/openai/gpt-6-luna ",
        "messages": [{"role": "user", "content": "hello"}],
    });
    let response = send(&state, chat_request(&body)).await;
    assert_eq!(response.status(), StatusCode::OK);

    let row = single_record(&state).await;
    assert_eq!(row.source, "api");
    assert_eq!(row.model, "history-provider/openai/gpt-6-luna");
    assert!(!row.streaming);
    assert_eq!(row.outcome, "success");
    assert_eq!(row.status, 200);
    assert_eq!(row.input_tokens, Some(4));
    assert_eq!(row.output_tokens, Some(2));

    // Direct routing attempts every refreshed connection that lists the
    // model, oldest first, at most once each.
    let attempts = &row.attempts;
    assert_eq!(attempts.len(), 2);
    assert_eq!(attempts[0].provider_id, "prov-one");
    assert_eq!(attempts[0].provider_name, "Oldest Connection");
    assert_eq!(attempts[0].model, "openai/gpt-6-luna");
    assert_eq!(attempts[0].position, 1);
    assert_eq!(attempts[0].outcome, "retryable_failure");
    assert_eq!(attempts[1].provider_id, "prov-two");
    assert_eq!(attempts[1].provider_name, "Newest Connection");
    assert_eq!(attempts[1].model, "openai/gpt-6-luna");
    assert_eq!(attempts[1].position, 2);
    assert_eq!(attempts[1].outcome, "selected");

    assert_eq!(first.hits.count(), 1);
    assert_eq!(second.hits.count(), 1);
    Ok(())
}

// ---------------------------------------------------------------------------
// Dashboard test chat records the dashboard source, never streaming
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_chat_records_dashboard_source_without_streaming() -> TestResult {
    let state = test_state().await;
    let first = spawn_upstream(error_status_router(StatusCode::SERVICE_UNAVAILABLE)).await;
    let (second_router, captured) = captured_json_router(
        StatusCode::OK,
        "{\"id\":\"t\",\"choices\":[{\"index\":0,\"message\":{\"role\":\"assistant\",\"content\":\"ok\"},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":4,\"completion_tokens\":2}}",
    );
    let second = spawn_upstream(second_router).await;

    save_provider(
        &state,
        "prov-a",
        "Chat A",
        &first.url(),
        Protocol::OpenAi,
        true,
        UPSTREAM_KEY_A,
    )
    .await?;
    save_provider(
        &state,
        "prov-b",
        "Chat B",
        &second.url(),
        Protocol::OpenAi,
        true,
        UPSTREAM_KEY_B,
    )
    .await?;
    combo_with_steps(
        &state,
        "coding",
        &[("prov-a", "model-a"), ("prov-b", "model-b")],
    )
    .await?;

    // The playground payload asks for streaming, but the endpoint forces it
    // off before calling upstream; the record must say non-streaming.
    let response = send(
        &state,
        test_chat_request(
            &state,
            &json!({
                "model": "combo/coding",
                "messages": [{"role": "user", "content": "dashboard hello"}],
                "stream": true,
            }),
        ),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let payload = json_body(response).await;
    assert_eq!(
        payload["completion"]["choices"][0]["message"]["content"], "ok",
        "the completion is the healthy upstream's reply"
    );

    let row = single_record(&state).await;
    assert_eq!(row.source, "test", "test chat must record its own source");
    assert_eq!(row.model, "combo/coding");
    assert!(!row.streaming, "test chat is always non-streaming");
    assert_eq!(row.outcome, "success");
    assert_eq!(row.status, 200);
    assert_eq!(row.input_tokens, Some(4));
    assert_eq!(row.output_tokens, Some(2));

    let attempts = &row.attempts;
    assert_eq!(attempts.len(), 2);
    assert_eq!(attempts[0].provider_id, "prov-a");
    assert_eq!(attempts[0].outcome, "retryable_failure");
    assert_eq!(attempts[1].provider_id, "prov-b");
    assert_eq!(attempts[1].outcome, "selected");

    let seen = captured.lock().unwrap().clone().expect("upstream called");
    assert_eq!(seen["stream"], false, "streaming must be forced off");

    assert_eq!(first.hits.count(), 1);
    assert_eq!(second.hits.count(), 1);
    Ok(())
}

// ---------------------------------------------------------------------------
// Streaming: "streaming" while open, "success" once [DONE] is forwarded
// ---------------------------------------------------------------------------

#[tokio::test]
async fn streaming_records_streaming_then_success_with_fragmented_usage() -> TestResult {
    let state = test_state().await;
    // Chunk boundaries deliberately cut through JSON payloads and the SSE
    // terminators, and the two usage fields arrive in separate events, so the
    // recorded totals must be merged across fragmented frames.
    let chunks: Vec<&'static str> = vec![
        "data: {\"id\":\"c1\",\"choices\":[{\"index\":0,\"delta\":{\"conte",
        "nt\":\"He\"}}]}\n",
        "\ndata: {\"choices\":[],\"usage\":{\"prom",
        "pt_tokens\":9,\"prompt_tokens_details\":{\"cached_tokens\":5},\"cost\":0.001}}\n\n",
        "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":9,\"prompt_tokens_details\":{\"cached_tokens\":5},\"cost\":0.001}}\n\n",
        "data: {\"choices\":[],\"usage\":{\"compl",
        "etion_tokens\":6,\"total_tokens\":15,\"cost\":0.002}}\n",
        "\ndata: [DONE]",
        "\n\n",
    ];
    let expected: String = chunks.concat();
    let (router, captured) = {
        let captured = Arc::new(std::sync::Mutex::new(None::<Value>));
        let seen = captured.clone();
        let router = Router::new().route(
            "/chat/completions",
            post(move |body_bytes: Bytes| {
                let seen = seen.clone();
                let chunks = chunks.clone();
                async move {
                    *seen.lock().unwrap() =
                        Some(serde_json::from_slice(&body_bytes).unwrap_or(Value::Null));
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
        );
        (router, captured)
    };
    let upstream = spawn_upstream(router).await;

    save_provider(
        &state,
        "prov-sse",
        "SSE Display",
        &upstream.url(),
        Protocol::OpenAi,
        true,
        UPSTREAM_KEY_A,
    )
    .await?;
    combo_with_steps(&state, "coding", &[("prov-sse", "model-sse")]).await?;

    let body = json!({
        "model": "combo/coding",
        "stream": true,
        "stream_options": {"include_usage": true},
        "messages": [{"role": "user", "content": "stream please"}],
    });
    let response = send(&state, chat_request(&body)).await;
    assert_eq!(response.status(), StatusCode::OK);

    // While the body is still open the record exists and says "streaming",
    // with no token counts yet (usage has not been observed).
    let open = single_record(&state).await;
    assert_eq!(open.source, "api");
    assert_eq!(open.model, "combo/coding");
    assert!(open.streaming);
    assert_eq!(open.outcome, "streaming");
    assert_eq!(open.status, 200);
    assert_eq!(open.input_tokens, None);
    assert_eq!(open.output_tokens, None);
    assert_eq!(open.attempts.len(), 1);
    assert_eq!(open.attempts[0].provider_id, "prov-sse");
    assert_eq!(open.attempts[0].outcome, "selected");

    // Raw SSE bytes pass through unchanged regardless of fragmentation.
    let collected = body_bytes(response).await;
    assert_eq!(collected, Bytes::from(expected));

    // Completing the body finalizes the record as a success with usage
    // merged across the fragmented events, and a latency measured to the end.
    let done = wait_until(&state, |r| r.outcome != "streaming").await;
    assert_eq!(done.id, open.id);
    assert!(done.streaming);
    assert_eq!(done.outcome, "success");
    assert_eq!(done.status, 200);
    assert_eq!(done.input_tokens, Some(9));
    assert_eq!(done.output_tokens, Some(6));
    let totals = sparkrouter::db::usage::overview(&state.db).await?;
    assert_eq!(totals.cached_tokens, Some(5));
    assert_eq!(
        totals.cost_usd,
        Some(0.002),
        "stream usage snapshots are not cumulative charges"
    );
    assert_eq!(totals.providers[0].cached_tokens, Some(5));
    assert_eq!(totals.providers[0].cost_usd, Some(0.002));
    assert!(done.latency_ms >= open.latency_ms);

    // The unknown stream_options field survived to the upstream untouched.
    let seen = captured.lock().unwrap().clone().expect("upstream called");
    assert_eq!(seen["stream_options"]["include_usage"], true);
    assert_eq!(seen["model"], "model-sse");
    assert_eq!(seen["stream"], true);

    assert_eq!(upstream.hits.count(), 1);
    Ok(())
}

// ---------------------------------------------------------------------------
// Streaming: a truncated body records "interrupted" and never fails over
// ---------------------------------------------------------------------------

#[tokio::test]
async fn midstream_truncation_records_interrupted_without_failover() -> TestResult {
    let state = test_state().await;
    // Usage observed before a truncated EOF is still retained, without turning
    // the interruption into a success or attempting a fallback.
    const FIRST_FRAME: &str = "data: {\"first\":true,\"usage\":{\"prompt_tokens\":10,\"prompt_tokens_details\":{\"cached_tokens\":8},\"cost\":0.001}}\n\n";
    let first = spawn_upstream(sse_router(vec![FIRST_FRAME])).await;
    let second = spawn_upstream(captured_json_router(StatusCode::OK, "{\"usage\":{}}").0).await;

    save_provider(
        &state,
        "prov-flaky",
        "Flaky Display",
        &first.url(),
        Protocol::OpenAi,
        true,
        UPSTREAM_KEY_A,
    )
    .await?;
    save_provider(
        &state,
        "prov-backup",
        "Backup Display",
        &second.url(),
        Protocol::OpenAi,
        true,
        UPSTREAM_KEY_B,
    )
    .await?;
    combo_with_steps(
        &state,
        "coding",
        &[("prov-flaky", "model-a"), ("prov-backup", "model-b")],
    )
    .await?;

    let body = json!({
        "model": "combo/coding",
        "stream": true,
        "messages": [{"role": "user", "content": "stream please"}],
    });
    let response = send(&state, chat_request(&body)).await;
    assert_eq!(response.status(), StatusCode::OK);

    let mut frames = response.into_body().into_data_stream();
    let frame = tokio::time::timeout(std::time::Duration::from_secs(10), frames.next())
        .await
        .expect("first frame within timeout")
        .expect("stream contains a first frame")
        .expect("first frame is data");
    assert_eq!(frame, Bytes::from_static(FIRST_FRAME.as_bytes()));

    // The truncation surfaces as a body error; the row is then finalized as
    // "interrupted" in the background.
    let next = tokio::time::timeout(std::time::Duration::from_secs(10), frames.next())
        .await
        .expect("second frame within timeout");
    assert!(
        matches!(&next, Some(Err(_))),
        "a truncated stream must end in a body error, got {next:?}"
    );

    let row = wait_until(&state, |r| r.outcome != "streaming").await;
    assert!(row.streaming);
    assert_eq!(row.outcome, "interrupted");
    assert_eq!(row.status, 200);
    assert_eq!(row.input_tokens, Some(10));
    assert_eq!(row.output_tokens, None);
    let totals = sparkrouter::db::usage::overview(&state.db).await?;
    assert_eq!(totals.cached_tokens, Some(8));
    assert_eq!(totals.cost_usd, Some(0.001));

    let attempts = &row.attempts;
    assert_eq!(attempts.len(), 1);
    assert_eq!(attempts[0].provider_id, "prov-flaky");
    assert_eq!(attempts[0].outcome, "selected");

    assert_eq!(first.hits.count(), 1);
    assert_eq!(
        second.hits.count(),
        0,
        "a committed streaming body must not fail over"
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// Streaming: dropping an unpolled body records "cancelled"
// ---------------------------------------------------------------------------

#[tokio::test]
async fn unpolled_dropped_body_records_cancelled() -> TestResult {
    let state = test_state().await;
    let upstream = spawn_upstream(sse_router(vec![
        "data: {\"id\":\"c1\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"Hel\"}}]}\n\n",
        "data: {\"id\":\"c2\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"lo\"}}]}\n\n",
        "data: [DONE]\n\n",
    ]))
    .await;

    save_provider(
        &state,
        "prov-sse",
        "SSE Display",
        &upstream.url(),
        Protocol::OpenAi,
        true,
        UPSTREAM_KEY_A,
    )
    .await?;
    combo_with_steps(&state, "coding", &[("prov-sse", "model-sse")]).await?;

    let body = json!({
        "model": "combo/coding",
        "stream": true,
        "messages": [{"role": "user", "content": "stream please"}],
    });
    let response = send(&state, chat_request(&body)).await;
    assert_eq!(response.status(), StatusCode::OK);

    // The record exists and is open while the client holds the body...
    let open = single_record(&state).await;
    assert_eq!(open.outcome, "streaming");
    assert_eq!(open.attempts.len(), 1);
    assert_eq!(open.attempts[0].provider_id, "prov-sse");
    assert_eq!(open.attempts[0].outcome, "selected");

    // ...and abandoning it without ever polling the body records a
    // cancellation. The final write is spawned from Drop, so wait for it by
    // yielding to the runtime between bounded re-reads (never sleeping).
    drop(response);
    let cancelled = wait_until(&state, |row| row.outcome == "cancelled").await;
    assert!(cancelled.streaming);
    assert_eq!(cancelled.status, 200);
    assert_eq!(cancelled.input_tokens, None);
    assert_eq!(cancelled.output_tokens, None);

    assert_eq!(upstream.hits.count(), 1);
    Ok(())
}

// ---------------------------------------------------------------------------
// Metadata only: rows, schema, and the history page never carry content
// ---------------------------------------------------------------------------

#[tokio::test]
async fn history_rows_schema_and_page_store_metadata_only() -> TestResult {
    let state = test_state().await;
    let first = spawn_upstream(error_status_router(StatusCode::SERVICE_UNAVAILABLE)).await;
    let success_body = concat!(
        "{\"id\":\"chatcmpl-raw\",\"choices\":[{\"index\":0,\"message\":{\"role\":\"assistant\",",
        "\"content\":\"",
        "mock-reply-marker-246810",
        "\"},\"finish_reason\":\"stop\"}],",
        "\"usage\":{\"prompt_tokens\":11,\"completion_tokens\":7}}"
    );
    let (second_router, _captured) = captured_json_router(StatusCode::OK, success_body);
    let second = spawn_upstream(second_router).await;
    let bad = spawn_upstream(error_status_router(StatusCode::BAD_REQUEST)).await;

    save_provider(
        &state,
        "prov-first",
        "First Display",
        &first.url(),
        Protocol::OpenAi,
        true,
        UPSTREAM_KEY_A,
    )
    .await?;
    save_provider(
        &state,
        "prov-second",
        "Second Display",
        &second.url(),
        Protocol::OpenAi,
        true,
        UPSTREAM_KEY_B,
    )
    .await?;
    save_provider(
        &state,
        "prov-bad",
        "Bad Display",
        &bad.url(),
        Protocol::OpenAi,
        true,
        UPSTREAM_KEY_A,
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
    combo_with_steps(&state, "failing", &[("prov-bad", "model-bad")]).await?;

    // A successful fallback (prompt + completion + keys + 503 body transit
    // the gateway) and a permanent failure (upstream 400 body transits too).
    let good = json!({
        "model": "combo/coding",
        "messages": [{"role": "user", "content": "top-secret-prompt-marker-8675309"}],
    });
    let response = send(&state, chat_request(&good)).await;
    assert_eq!(response.status(), StatusCode::OK);
    let _ = body_bytes(response).await;

    let evil = json!({
        "model": "combo/failing",
        "messages": [{"role": "user", "content": "another prompt nobody should store"}],
    });
    let response = send(&state, chat_request(&evil)).await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let _ = body_bytes(response).await;
    assert_eq!(bad.hits.count(), 1);

    let rows = wait_for_count(&state, 2).await;
    assert_eq!(rows.len(), 2, "both requests must be recorded");
    assert_eq!(rows[0].model, "combo/failing", "newest record first");
    assert_eq!(rows[1].model, "combo/coding");

    // 1. No stored value anywhere in the history tables carries a prompt, a
    //    completion, a credential, an endpoint URL, or an upstream body.
    let dump = dump_history_text(&state).await;
    for marker in [
        PROMPT_MARKER,
        "another prompt nobody should store",
        REPLY_MARKER,
        UPSTREAM_KEY_A,
        UPSTREAM_KEY_B,
        "sk-history-secret",
        "mock status",
        "127.0.0.1",
        "http",
    ] {
        assert!(
            !dump.contains(marker),
            "history rows must not contain {marker:?}; dump:\n{dump}"
        );
    }
    // The dump is not vacuous: it really carries the metadata.
    assert!(dump.contains("combo/coding"));
    assert!(dump.contains("prov-second"));
    assert!(dump.contains("retryable_failure"));

    // 2. The schema itself has no column for content: no prompts, messages,
    //    bodies, keys, URLs, or endpoints may even exist.
    for table in ["request_history", "request_attempts"] {
        let columns: Vec<String> =
            sqlx::query_scalar(&format!("SELECT name FROM pragma_table_info('{table}')"))
                .fetch_all(&state.db)
                .await
                .expect("history table columns");
        assert!(!columns.is_empty(), "{table} must have columns");
        for column in &columns {
            let lower = column.to_ascii_lowercase();
            for banned in [
                "prompt",
                "message",
                "content",
                "body",
                "bodies",
                "key",
                "url",
                "endpoint",
                "secret",
                "credential",
            ] {
                assert!(
                    !lower.contains(banned),
                    "history column {table}.{column} must not describe {banned}"
                );
            }
        }
    }

    // 3. The dashboard history page renders the recorded metadata (proving
    //    the negative assertions are not vacuous) but never any of it.
    let response = send(
        &state,
        Request::builder()
            .method(Method::GET)
            .uri("/history")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let page = String::from_utf8(body_bytes(response).await.to_vec())?;
    assert!(page.contains("combo/coding"), "the page lists records");
    assert!(page.contains("First Display"));
    for marker in [
        PROMPT_MARKER,
        "another prompt nobody should store",
        REPLY_MARKER,
        UPSTREAM_KEY_A,
        UPSTREAM_KEY_B,
        "mock status",
        "127.0.0.1",
    ] {
        assert!(
            !page.contains(marker),
            "the history page must not contain {marker:?}"
        );
    }
    Ok(())
}
