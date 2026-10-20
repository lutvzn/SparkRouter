//! Best-effort request history; never changes response bytes or retry policy.
//! Basic is metadata-only; opt-in Extended bodies use `history_payload`.
//!
//! Inserts and streaming finalization are queued for the batch
//! [`HistoryWriter`], so neither the first response byte nor the end of the
//! body waits on SQLite. The queue is ordered, so a fast client cannot leave a
//! completed stream recorded as still streaming.
use std::{ops::ControlFlow, time::Instant};

use axum::{body::Body, response::Response};
use futures_util::StreamExt;
use serde_json::Value;

use crate::{
    app::AppState,
    db::{
        history_writer::HistoryWriter,
        request_history::{self, Attempt, NewRequest, Record, Terminal, UsageMetrics},
    },
    providers::sse::SseDecoder,
    routing::combo::AttemptTrace,
};

#[derive(Clone, Copy, Default)]
pub(crate) struct Usage {
    input: Option<i64>,
    output: Option<i64>,
    cached: Option<i64>,
    cost_usd: Option<f64>,
}

/// A streamed event with every field except `usage` skipped unparsed.
#[derive(serde::Deserialize)]
struct UsageEvent {
    #[serde(default)]
    usage: Value,
}

impl Usage {
    pub(crate) fn from_value(value: &Value) -> Self {
        Self::from_usage(&value["usage"])
    }

    /// Usage reported by one streamed event. Most events carry none, so only
    /// those mentioning the key are parsed; a key spelled with JSON escapes
    /// is not recognized.
    fn from_event(data: &str) -> Self {
        if !data.contains("\"usage\"") {
            return Self::default();
        }
        serde_json::from_str::<UsageEvent>(data)
            .map(|event| Self::from_usage(&event.usage))
            .unwrap_or_default()
    }

    fn from_usage(usage: &Value) -> Self {
        let mut result = Self {
            input: usage
                .get("prompt_tokens")
                .or_else(|| usage.get("input_tokens"))
                .and_then(token_count),
            cached: usage
                .pointer("/prompt_tokens_details/cached_tokens")
                .or_else(|| usage.pointer("/input_tokens_details/cached_tokens"))
                .or_else(|| usage.get("cache_read_input_tokens"))
                .and_then(token_count),
            cost_usd: usage
                .get("cost_usd")
                .or_else(|| {
                    // `usage.cost` is the USD field used by OpenRouter. Never
                    // silently mix explicitly non-USD amounts into that total.
                    let is_usd = usage.get("currency").is_none_or(|currency| {
                        currency
                            .as_str()
                            .is_some_and(|value| value.eq_ignore_ascii_case("USD"))
                    });
                    is_usd.then(|| usage.get("cost")).flatten()
                })
                .and_then(|cost| cost.as_f64())
                .filter(|cost| cost.is_finite() && (0.0..=1_000_000.0).contains(cost)),
            output: usage
                .get("completion_tokens")
                .or_else(|| usage.get("output_tokens"))
                .and_then(token_count),
        };
        result.validate_cache();
        result
    }

    fn merge(&mut self, other: Self) {
        self.input = other.input.or(self.input);
        self.output = other.output.or(self.output);
        self.cached = other.cached.or(self.cached);
        self.cost_usd = other.cost_usd.or(self.cost_usd);
        self.validate_cache();
    }

    fn validate_cache(&mut self) {
        if self
            .cached
            .zip(self.input)
            .is_some_and(|(cached, input)| cached > input)
        {
            self.cached = None;
        }
    }

    fn metrics(self) -> UsageMetrics {
        UsageMetrics {
            cached_tokens: self.cached,
            cost_usd: self.cost_usd,
        }
    }
}

fn token_count(value: &Value) -> Option<i64> {
    // Reject implausible upstream counters instead of risking aggregate overflow.
    value.as_i64().filter(|n| (0..=1_000_000_000).contains(n))
}

/// A new history record ID. Time-ordered (UUIDv7), so inserts append to the
/// right edge of every index keyed by it and retention deletes from the left
/// edge. Random IDs dirtied a random page per index on each insert and prune,
/// roughly doubling the bytes written per record.
fn history_id() -> String {
    uuid::Uuid::now_v7().to_string()
}

fn millis(started: Instant) -> i64 {
    started.elapsed().as_millis().min(i64::MAX as u128) as i64
}

pub(crate) async fn record_response(
    state: &AppState,
    source: &str,
    model: &str,
    streaming: bool,
    started: Instant,
    attempts: &[AttemptTrace],
    mut response: Response,
) -> Response {
    let capture = response
        .extensions_mut()
        .remove::<crate::history_payload::Capture>();
    if capture
        .as_ref()
        .is_some_and(|c| c.mode == crate::db::settings::LoggingMode::Off)
    {
        return response;
    }
    let extended = capture
        .as_ref()
        .is_some_and(|c| c.mode == crate::db::settings::LoggingMode::Extended);
    let success = response.status().is_success();
    let is_stream = success && streaming;
    let is_binary = response
        .extensions()
        .get::<crate::providers::media::BinaryStream>()
        .is_some();
    let usage = response
        .extensions()
        .get::<Usage>()
        .copied()
        .unwrap_or_default();
    let record = Record {
        id: history_id(),
        source: source.into(),
        model: model.chars().take(321).collect(),
        streaming,
        outcome: if is_stream {
            "streaming"
        } else if success {
            "success"
        } else {
            "failure"
        }
        .into(),
        status: i64::from(response.status().as_u16()),
        latency_ms: millis(started),
        input_tokens: usage.input,
        output_tokens: usage.output,
        attempts: attempts
            .iter()
            .map(|trace| Attempt {
                provider_id: trace.provider_id.clone(),
                provider_name: trace.provider_name.clone(),
                model: trace.model.clone(),
                position: trace.position,
                outcome: trace.outcome.into(),
                latency_ms: trace.latency_ms.min(i64::MAX as u128) as i64,
            })
            .collect(),
    };
    let provider_snapshots: Vec<_> = attempts
        .iter()
        .map(|trace| trace.provider_snapshot.clone())
        .collect();
    let diagnostics = request_history::RoutingDiagnostics {
        selection: attempts.first().and_then(|trace| trace.routing.clone()),
        reasons: attempts.iter().map(|trace| trace.reason.clone()).collect(),
    };
    // Recording failures must never fail a generation, and queuing never
    // delays the first response byte.
    let id = record.id.clone();
    let request_body = capture
        .filter(|_| extended)
        .map(|capture| (capture.request, capture.truncated));
    let queued = match NewRequest::new(
        record,
        usage.metrics(),
        provider_snapshots,
        Some(&diagnostics),
    ) {
        Ok(request) => state.history.insert(request, request_body),
        Err(_) => {
            tracing::warn!("request history write unavailable");
            false
        }
    };
    // Without a queued insert there is nothing to finalize.
    if !queued {
        return response;
    }
    let response = if extended {
        crate::history_payload::wrap(response, state.history.clone(), id.clone())
    } else {
        response
    };
    if !is_stream {
        return response;
    }
    let guard = StreamRecord {
        history: state.history.clone(),
        id,
        started,
        usage,
        done: false,
        finalized: false,
    };
    let (parts, body) = response.into_parts();
    // Construct the guard outside the generator: even an entirely unpolled
    // dropped body records cancellation. Bytes/errors are forwarded unchanged.
    let stream = async_stream::stream! {
        let mut guard = guard;
        let mut decoder = if is_binary { None } else { Some(SseDecoder::default()) };
        let mut source = body.into_data_stream();
        while let Some(chunk) = source.next().await {
            match &chunk {
                Ok(bytes) => {
                    if let Some(observer) = decoder.as_mut() {
                        let observed = observer.push_chunk(bytes, |event| {
                            if event.data.trim() == "[DONE]" {
                                guard.done = true;
                                return ControlFlow::Break(());
                            }
                            guard.usage.merge(Usage::from_event(&event.data));
                            ControlFlow::Continue(())
                        });
                        if observed.is_err() { decoder = None; }
                    }
                }
                Err(_) => guard.finish("interrupted"),
            }
            let failed = chunk.is_err();
            yield chunk;
            if failed { return; }
        }
        guard.finish(if guard.done || is_binary { "success" } else { "interrupted" });
    };
    Response::from_parts(parts, Body::from_stream(stream))
}

struct StreamRecord {
    history: HistoryWriter,
    id: String,
    started: Instant,
    usage: Usage,
    done: bool,
    finalized: bool,
}

impl StreamRecord {
    /// Queue the terminal outcome once; it is written after the insert.
    fn finish(&mut self, outcome: &'static str) {
        if std::mem::replace(&mut self.finalized, true) {
            return;
        }
        self.history.finish(Terminal {
            id: self.id.clone(),
            outcome,
            latency_ms: millis(self.started),
            input_tokens: self.usage.input,
            output_tokens: self.usage.output,
            usage: self.usage.metrics(),
        });
    }
}

impl Drop for StreamRecord {
    fn drop(&mut self) {
        self.finish(if self.done { "success" } else { "cancelled" });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn history_ids_sort_in_creation_order() {
        let ids: Vec<String> = (0..1000).map(|_| history_id()).collect();
        assert!(ids.windows(2).all(|pair| pair[0] < pair[1]));
        let parsed = uuid::Uuid::parse_str(&ids[0]).unwrap();
        assert_eq!(parsed.get_version_num(), 7);
    }

    #[test]
    fn unknown_and_invalid_usage_stays_unknown() {
        let absent = Usage::from_value(&serde_json::json!({"choices": []}));
        assert_eq!(absent.input, None);
        assert_eq!(absent.output, None);
        let invalid = Usage::from_value(
            &serde_json::json!({"usage": {"prompt_tokens": -1, "completion_tokens": i64::MAX}}),
        );
        assert_eq!(invalid.input, None);
        assert_eq!(invalid.output, None);
        let zero = Usage::from_value(
            &serde_json::json!({"usage": {"prompt_tokens": 0, "completion_tokens": 0}}),
        );
        assert_eq!(zero.input, Some(0));
        assert_eq!(zero.output, Some(0));
    }

    #[test]
    fn cache_and_usd_cost_keep_unknown_zero_and_invalid_values_distinct() {
        use serde_json::json;
        let absent = Usage::from_value(&json!({"usage":{"prompt_tokens":10}}));
        assert_eq!(absent.cached, None);
        assert_eq!(absent.cost_usd, None);
        let known = Usage::from_value(
            &json!({"usage":{"prompt_tokens":10,"prompt_tokens_details":{"cached_tokens":8},"cost":0.0012}}),
        );
        assert_eq!(known.cached, Some(8));
        assert_eq!(known.cost_usd, Some(0.0012));
        let zero = Usage::from_value(
            &json!({"usage":{"prompt_tokens":0,"input_tokens_details":{"cached_tokens":0},"cost_usd":0}}),
        );
        assert_eq!(zero.cached, Some(0));
        assert_eq!(zero.cost_usd, Some(0.0));
        for usage in [
            json!({"prompt_tokens":10,"prompt_tokens_details":{"cached_tokens":11},"cost":-1}),
            json!({"cache_read_input_tokens":-1,"cost":1_000_001}),
            json!({"cache_read_input_tokens":1_000_000_001,"cost":"1.0"}),
            json!({"prompt_tokens_details":{"cached_tokens":"8"},"cost":1,"currency":"EUR"}),
        ] {
            let invalid = Usage::from_value(&json!({"usage":usage}));
            assert_eq!(invalid.cached, None);
            assert_eq!(invalid.cost_usd, None);
        }
    }

    #[test]
    fn streamed_events_read_usage_like_the_full_value() {
        let events = [
            r#"{"choices":[{"delta":{"content":"usage"}}]}"#,
            r#"{"choices":[],"usage":{"prompt_tokens":12,"completion_tokens":3,"prompt_tokens_details":{"cached_tokens":4},"cost":0.5}}"#,
            r#"{"type":"message_delta","usage":{"input_tokens":7,"output_tokens":2,"cache_read_input_tokens":1,"cost_usd":0.25}}"#,
            r#"{"usage":{"cost":1.5,"currency":"EUR","prompt_tokens":-1}}"#,
            r#"{"usage":null}"#,
            r#"["usage"]"#,
            "not json \"usage\"",
        ];
        for data in events {
            let expected = serde_json::from_str::<Value>(data)
                .map(|value| Usage::from_value(&value))
                .unwrap_or_default();
            let actual = Usage::from_event(data);
            assert_eq!(
                (actual.input, actual.output, actual.cached, actual.cost_usd),
                (
                    expected.input,
                    expected.output,
                    expected.cached,
                    expected.cost_usd
                ),
                "{data}"
            );
        }
        assert_eq!(
            Usage::from_event(r#"{"usage":{"prompt_tokens":5}}"#).input,
            Some(5)
        );
    }

    #[test]
    fn streamed_usage_is_a_snapshot_not_an_increment() {
        use serde_json::json;
        let first = Usage::from_value(
            &json!({"usage":{"prompt_tokens":20,"completion_tokens":2,"prompt_tokens_details":{"cached_tokens":5},"cost":0.01}}),
        );
        let mut total = first;
        total.merge(first);
        total.merge(Usage::from_value(
            &json!({"usage":{"completion_tokens":6,"cost":0.02}}),
        ));
        total.merge(Usage::from_value(&json!({"usage":null})));
        assert_eq!(total.input, Some(20));
        assert_eq!(total.output, Some(6));
        assert_eq!(total.cached, Some(5));
        assert_eq!(total.cost_usd, Some(0.02));
    }

    fn streaming_record(id: &str) -> Record {
        Record {
            id: id.into(),
            source: "api".into(),
            model: "test".into(),
            streaming: true,
            outcome: "streaming".into(),
            status: 200,
            latency_ms: 1,
            input_tokens: None,
            output_tokens: None,
            attempts: vec![],
        }
    }

    fn guard(history: &HistoryWriter, id: &str) -> StreamRecord {
        StreamRecord {
            history: history.clone(),
            id: id.into(),
            started: Instant::now(),
            usage: Usage::default(),
            done: false,
            finalized: false,
        }
    }

    async fn outcome(pool: &sqlx::SqlitePool, id: &str) -> String {
        sqlx::query_scalar("SELECT outcome FROM request_history WHERE id = ?")
            .bind(id)
            .fetch_one(pool)
            .await
            .unwrap()
    }

    fn test_state(database_url: String) -> crate::config::Config {
        crate::config::Config {
            database_url,
            bind: "127.0.0.1:0".parse().unwrap(),
            crypto: crate::crypto::Crypto::from_base64(&secrecy::SecretString::from(
                base64::Engine::encode(&base64::engine::general_purpose::STANDARD, [9u8; 32]),
            ))
            .unwrap(),
            api_key: None,
            dashboard_password: None,
            dashboard_secure_cookie: false,
        }
    }

    #[tokio::test]
    async fn finalization_does_not_block_and_drop_keeps_the_terminal_outcome() {
        let pool = crate::db::connect("sqlite::memory:").await.unwrap();
        let id = uuid::Uuid::new_v4().to_string();
        request_history::save(&pool, &streaming_record(&id))
            .await
            .unwrap();
        let history = HistoryWriter::start(pool.clone());
        // Hold the only connection: finishing must still return immediately,
        // and the later drop must not overwrite the outcome with cancellation.
        let connection = pool.acquire().await.unwrap();
        let mut guard = guard(&history, &id);
        guard.finish("interrupted");
        drop(guard);
        drop(connection);
        history.flush().await;
        assert_eq!(outcome(&pool, &id).await, "interrupted");
    }

    #[tokio::test]
    async fn a_started_history_write_waits_for_the_lock_instead_of_being_cancelled() {
        let directory = tempfile::tempdir().unwrap();
        let state = crate::app::AppState::from_config(test_state(format!(
            "sqlite://{}",
            directory.path().join("lock.db").display()
        )))
        .await
        .unwrap();
        // Another connection holds the write lock past the 2 s acquire bound.
        // The writer already has its connection, so it must wait (within the
        // busy timeout) and commit rather than be abandoned mid-transaction.
        let lock = state.db.begin_with("BEGIN IMMEDIATE").await.unwrap();
        let response = record_response(
            &state,
            "api",
            "combo/coding",
            false,
            Instant::now(),
            &[],
            axum::response::Response::new(axum::body::Body::from("ok")),
        )
        .await;
        assert_eq!(response.status(), axum::http::StatusCode::OK);
        tokio::time::sleep(std::time::Duration::from_millis(2500)).await;
        assert_eq!(request_history::count(&state.db).await.unwrap(), 0);
        lock.commit().await.unwrap();
        state.flush_history().await;
        assert_eq!(
            request_history::count(&state.db).await.unwrap(),
            1,
            "the delayed write lands after the lock is released"
        );
    }

    #[tokio::test]
    async fn history_insert_does_not_block_the_response() {
        let state = crate::app::AppState::from_config(test_state("sqlite::memory:".into()))
            .await
            .unwrap();
        let held = state.db.acquire().await.unwrap();
        let started = Instant::now();
        let response = record_response(
            &state,
            "api",
            "combo/coding",
            false,
            Instant::now(),
            &[],
            axum::response::Response::new(axum::body::Body::from("ok")),
        )
        .await;
        assert!(
            started.elapsed() < std::time::Duration::from_millis(500),
            "history insert delayed the response by {:?}",
            started.elapsed()
        );
        assert_eq!(response.status(), axum::http::StatusCode::OK);
        drop(held);
        state.flush_history().await;
        assert_eq!(request_history::count(&state.db).await.unwrap(), 1);
    }

    #[tokio::test]
    async fn stream_finalization_is_written_after_its_insert() {
        let pool = crate::db::connect("sqlite::memory:").await.unwrap();
        let history = HistoryWriter::start(pool.clone());
        let id = uuid::Uuid::new_v4().to_string();
        // Hold the connection so the insert and its terminal outcome share
        // one batch, where they must still apply in order.
        let connection = pool.acquire().await.unwrap();
        let request =
            NewRequest::new(streaming_record(&id), UsageMetrics::default(), vec![], None).unwrap();
        assert!(history.insert(request, None));
        guard(&history, &id).finish("interrupted");
        drop(connection);
        history.flush().await;
        assert_eq!(outcome(&pool, &id).await, "interrupted");
    }
}
