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
    pub(crate) input: Option<i64>,
    pub(crate) output: Option<i64>,
    pub(crate) cached: Option<i64>,
    pub(crate) cost_usd: Option<f64>,
}

/// A streamed event with every field except `usage` skipped unparsed. The
/// Responses API reports it inside the final event's `response` object, and
/// Anthropic's `message_start` inside its `message`.
#[derive(serde::Deserialize)]
struct UsageEvent {
    #[serde(default)]
    usage: Value,
    #[serde(default)]
    response: Option<UsageEnvelope>,
    #[serde(default)]
    message: Option<UsageEnvelope>,
}

#[derive(serde::Deserialize)]
struct UsageEnvelope {
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
            .map(|event| match event.response.or(event.message) {
                Some(envelope) if event.usage.is_null() => Self::from_usage(&envelope.usage),
                _ => Self::from_usage(&event.usage),
            })
            .unwrap_or_default()
    }

    pub(crate) fn from_usage(usage: &Value) -> Self {
        let mut result = Self {
            input: usage
                .get("prompt_tokens")
                .and_then(token_count)
                .or_else(|| anthropic_input(usage)),
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

    /// Input plus output tokens, when either was reported.
    fn total_tokens(self) -> Option<i64> {
        match (self.input, self.output) {
            (None, None) => None,
            (input, output) => Some(input.unwrap_or(0).saturating_add(output.unwrap_or(0))),
        }
    }

    fn metrics(self) -> UsageMetrics {
        UsageMetrics {
            cached_tokens: self.cached,
            cost_usd: self.cost_usd,
        }
    }
}

/// `input_tokens`, plus Anthropic's cache reads and writes when present: its
/// native count leaves them out, while the recorded input includes them, as
/// for every other protocol. An invalid cache counter leaves input unknown.
fn anthropic_input(usage: &Value) -> Option<i64> {
    crate::providers::anthropic::input_tokens(usage)
        .and_then(|input| i64::try_from(input).ok())
        .filter(|input| *input <= 1_000_000_000)
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

/// Who made a routed request: its history source (`api` or `test`), its
/// metrics endpoint label, and the budget its usage is charged to. Budgets
/// and metrics do not depend on the logging mode.
pub(crate) struct Meter {
    pub source: &'static str,
    pub endpoint: &'static str,
    pub charge: Option<crate::key_limits::Charge>,
}

/// Metrics for one request while its body is still open.
struct Measured {
    metrics: std::sync::Arc<crate::metrics::Metrics>,
    request: crate::metrics::Request,
}

pub(crate) async fn record_response(
    state: &AppState,
    model: &str,
    streaming: bool,
    started: Instant,
    attempts: &[AttemptTrace],
    meter: Meter,
    mut response: Response,
) -> Response {
    let capture = response
        .extensions_mut()
        .remove::<crate::history_payload::Capture>();
    let logging = capture
        .as_ref()
        .is_none_or(|c| c.mode != crate::db::settings::LoggingMode::Off);
    let metrics = state.metrics.enabled();
    if !logging && meter.charge.is_none() && !metrics {
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
    let mut guard = StreamRecord {
        writer: state.history.clone(),
        id: None,
        charge: meter.charge,
        measured: metrics
            .then(|| measure(state, meter.endpoint, model, started, attempts, is_stream)),
        started,
        usage,
        streamed: is_stream,
        done: false,
        finalized: false,
    };
    // A finished response reports now, before its history insert; a stream
    // reports when its body ends.
    if !is_stream {
        guard.finish(if success { "success" } else { "failure" });
    }
    if !logging {
        return if is_stream {
            observe_stream(response, guard, is_binary)
        } else {
            response
        };
    }
    let record = Record {
        id: history_id(),
        source: meter.source.into(),
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
    // Without a queued insert there is no history to finalize.
    if queued {
        guard.id = Some(id.clone());
    }
    let response = if extended && queued {
        crate::history_payload::wrap(response, state.history.clone(), id)
    } else {
        response
    };
    if !is_stream {
        return response;
    }
    observe_stream(response, guard, is_binary)
}

/// Labels for a request that reached routing. Only names from configuration
/// the request found become label values.
fn measure(
    state: &AppState,
    endpoint: &'static str,
    model: &str,
    started: Instant,
    attempts: &[AttemptTrace],
    is_stream: bool,
) -> Measured {
    let slug = |trace: &AttemptTrace| {
        trace
            .provider_snapshot
            .as_ref()
            .map(|snapshot| snapshot.slug.clone())
    };
    let combo = match model.strip_prefix("combo/") {
        Some(name) if !attempts.is_empty() => name.to_owned(),
        _ => String::new(),
    };
    let request = crate::metrics::Request {
        endpoint,
        combo,
        provider: attempts
            .iter()
            .rev()
            .find(|trace| trace.outcome == "selected")
            .and_then(slug),
    };
    let attempted: Vec<_> = attempts
        .iter()
        .map(|trace| (slug(trace), trace.outcome))
        .collect();
    state
        .metrics
        .responded(&request, started.elapsed(), &attempted, is_stream);
    Measured {
        metrics: state.metrics.clone(),
        request,
    }
}

/// Watch a committed stream for its end and usage. Construct the guard
/// outside the generator: even an entirely unpolled dropped body records
/// cancellation. Bytes and errors are forwarded unchanged.
fn observe_stream(response: Response, guard: StreamRecord, is_binary: bool) -> Response {
    let (parts, body) = response.into_parts();
    let stream = async_stream::stream! {
        let mut guard = guard;
        let mut decoder = if is_binary { None } else { Some(SseDecoder::default()) };
        let mut source = body.into_data_stream();
        while let Some(chunk) = source.next().await {
            match &chunk {
                Ok(bytes) => {
                    if let Some(observer) = decoder.as_mut() {
                        let observed = observer.push_chunk(bytes, |event| {
                            // The final Responses event also carries the usage.
                            guard.usage.merge(Usage::from_event(&event.data));
                            if crate::providers::openai::is_terminal(&event) {
                                guard.done = true;
                                return ControlFlow::Break(());
                            }
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
    writer: HistoryWriter,
    /// The history record to finalize, when one was queued.
    id: Option<String>,
    charge: Option<crate::key_limits::Charge>,
    measured: Option<Measured>,
    started: Instant,
    usage: Usage,
    /// A streaming body, counted as active until it ends.
    streamed: bool,
    done: bool,
    finalized: bool,
}

impl StreamRecord {
    /// Report the terminal outcome once; history is written after its insert.
    fn finish(&mut self, outcome: &'static str) {
        if std::mem::replace(&mut self.finalized, true) {
            return;
        }
        if let Some(charge) = self.charge.take() {
            charge.add(&self.writer, self.usage.total_tokens(), self.usage.cost_usd);
        }
        if let Some(id) = &self.id {
            self.writer.finish(Terminal {
                id: id.clone(),
                outcome,
                latency_ms: millis(self.started),
                input_tokens: self.usage.input,
                output_tokens: self.usage.output,
                usage: self.usage.metrics(),
            });
        }
        if let Some(measured) = &self.measured {
            measured
                .metrics
                .finished(&measured.request, outcome, self.usage, self.streamed);
        }
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
    fn native_anthropic_usage_counts_cache_tokens_as_input() {
        use serde_json::json;
        let usage = Usage::from_usage(&json!({"input_tokens":4, "cache_creation_input_tokens":3,
            "cache_read_input_tokens":20, "output_tokens":5}));
        assert_eq!(
            (usage.input, usage.cached, usage.output),
            (Some(27), Some(20), Some(5))
        );
        // Responses usage already includes cached input in `input_tokens`.
        let responses = Usage::from_usage(
            &json!({"input_tokens":30, "input_tokens_details":{"cached_tokens":20}}),
        );
        assert_eq!((responses.input, responses.cached), (Some(30), Some(20)));
        // `message_start` carries usage inside its message.
        let start = Usage::from_event(
            r#"{"type":"message_start","message":{"id":"m","usage":{"input_tokens":2,"cache_read_input_tokens":8,"output_tokens":1}}}"#,
        );
        assert_eq!((start.input, start.cached), (Some(10), Some(8)));
        let invalid = Usage::from_usage(&json!({"input_tokens":4, "cache_read_input_tokens":-1}));
        assert_eq!(invalid.input, None);
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

    fn api_meter() -> Meter {
        Meter {
            source: "api",
            endpoint: "chat",
            charge: None,
        }
    }

    fn guard(history: &HistoryWriter, id: &str) -> StreamRecord {
        StreamRecord {
            writer: history.clone(),
            id: Some(id.into()),
            charge: None,
            measured: None,
            started: Instant::now(),
            usage: Usage::default(),
            streamed: true,
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
            dashboard_login: crate::config::DashboardLogin::Disabled,
            dashboard_secure_cookie: false,
            metrics_token: None,
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
            "combo/coding",
            false,
            Instant::now(),
            &[],
            api_meter(),
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
            "combo/coding",
            false,
            Instant::now(),
            &[],
            api_meter(),
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
