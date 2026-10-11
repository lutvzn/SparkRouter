//! Best-effort request history; never changes response bytes or retry policy.
//! Basic is metadata-only; opt-in Extended bodies use `history_payload`.
//!
//! The insert is spawned so it does not delay the response. Streaming
//! finalization waits for that insert, then writes the terminal outcome, so a
//! fast client cannot leave a completed stream recorded as still streaming.
use std::{
    sync::Arc,
    time::{Duration, Instant},
};

use axum::{body::Body, response::Response};
use futures_util::StreamExt;
use serde_json::Value;
use sqlx::SqlitePool;

use crate::{
    app::AppState,
    db::request_history::{self, Attempt, Record, UsageMetrics},
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

impl Usage {
    pub(crate) fn from_value(value: &Value) -> Self {
        let usage = &value["usage"];
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
        id: uuid::Uuid::new_v4().to_string(),
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
    // Database contention or recording failures must never fail a generation,
    // and the insert must not delay the first response byte.
    let gate = InsertGate::pending();
    let insert = gate.clone();
    let pool = state.db.clone();
    let id = record.id.clone();
    let metrics = usage.metrics();
    drop(tokio::spawn(async move {
        let ok = matches!(
            tokio::time::timeout(
                Duration::from_secs(2),
                request_history::save_with_diagnostics(
                    &pool,
                    &record,
                    metrics,
                    &provider_snapshots,
                    Some(&diagnostics)
                )
            )
            .await,
            Ok(Ok(()))
        );
        if !ok {
            tracing::warn!("request history write unavailable");
        } else if let Some(capture) =
            capture.filter(|c| c.mode == crate::db::settings::LoggingMode::Extended)
        {
            let captured = matches!(
                tokio::time::timeout(
                    Duration::from_millis(200),
                    crate::db::request_payloads::insert(
                        &pool,
                        &record.id,
                        &capture.request,
                        capture.truncated
                    )
                )
                .await,
                Ok(Ok(()))
            );
            if !captured {
                tracing::warn!("request payload write unavailable");
            }
        }
        // Payload failure must not leave otherwise valid streaming metadata pending.
        insert.complete(ok);
    }));
    let response = if extended {
        crate::history_payload::wrap(response, state.db.clone(), id.clone(), gate.clone())
    } else {
        response
    };
    if !is_stream {
        return response;
    }
    let guard = StreamRecord {
        pool: state.db.clone(),
        id,
        started,
        usage,
        gate,
        done: false,
        finalized: false,
        final_outcome: None,
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
                        let mut invalid = false;
                        for byte in bytes {
                            match observer.push(*byte) {
                                Ok(Some(event)) if event.data.trim() == "[DONE]" => { guard.done = true; break; }
                                Ok(Some(event)) => {
                                    if let Ok(value) = serde_json::from_str::<Value>(&event.data) { guard.usage.merge(Usage::from_value(&value)); }
                                }
                                Ok(None) => {},
                                Err(_) => { invalid = true; break; }
                            }
                        }
                        if invalid { decoder = None; }
                    }
                }
                Err(_) => { guard.finish("interrupted").await; }
            }
            let failed = chunk.is_err();
            yield chunk;
            if failed { return; }
        }
        guard.finish(if guard.done || is_binary { "success" } else { "interrupted" }).await;
    };
    Response::from_parts(parts, Body::from_stream(stream))
}

struct StreamRecord {
    pool: SqlitePool,
    id: String,
    started: Instant,
    usage: Usage,
    gate: InsertGate,
    done: bool,
    finalized: bool,
    final_outcome: Option<&'static str>,
}

impl StreamRecord {
    async fn finish(&mut self, outcome: &'static str) {
        if self.finalized {
            return;
        }
        self.final_outcome = Some(outcome);
        if !self.wait_for_insert().await {
            return;
        }
        if write_terminal(
            &self.pool,
            &self.id,
            outcome,
            millis(self.started),
            self.usage,
        )
        .await
        {
            self.finalized = true;
        } else {
            tracing::warn!("request history finalization unavailable");
        }
    }

    /// Wait until the spawned insert finishes. A failed insert has nothing to
    /// update. A timed-out wait leaves `finalized` false so Drop can retry.
    async fn wait_for_insert(&mut self) -> bool {
        let inserted = tokio::time::timeout(Duration::from_secs(2), self.gate.wait())
            .await
            .unwrap_or_default();
        if !inserted && self.gate.outcome() == Some(false) {
            self.finalized = true;
        }
        inserted
    }
}

async fn write_terminal(
    pool: &SqlitePool,
    id: &str,
    outcome: &str,
    latency_ms: i64,
    usage: Usage,
) -> bool {
    matches!(
        tokio::time::timeout(
            Duration::from_secs(2),
            request_history::finish_with_usage(
                pool,
                id,
                outcome,
                latency_ms,
                usage.input,
                usage.output,
                usage.metrics()
            )
        )
        .await,
        Ok(Ok(()))
    )
}

/// Signals whether the background insert succeeded without losing a wakeup
/// when finalization starts before the insert task runs.
#[derive(Clone)]
pub(crate) struct InsertGate {
    tx: Arc<tokio::sync::watch::Sender<Option<bool>>>,
    rx: tokio::sync::watch::Receiver<Option<bool>>,
}

impl InsertGate {
    fn pending() -> Self {
        let (tx, rx) = tokio::sync::watch::channel(None);
        Self {
            tx: Arc::new(tx),
            rx,
        }
    }

    fn complete(&self, ok: bool) {
        let _ = self.tx.send(Some(ok));
    }

    fn outcome(&self) -> Option<bool> {
        *self.rx.borrow()
    }

    pub(crate) async fn wait(&self) -> bool {
        let mut rx = self.rx.clone();
        loop {
            if let Some(ok) = *rx.borrow_and_update() {
                return ok;
            }
            if rx.changed().await.is_err() {
                return false;
            }
        }
    }
}

impl Drop for StreamRecord {
    fn drop(&mut self) {
        if self.finalized {
            return;
        }
        let pool = self.pool.clone();
        let id = self.id.clone();
        let outcome = self
            .final_outcome
            .unwrap_or(if self.done { "success" } else { "cancelled" });
        let latency = millis(self.started);
        let usage = self.usage;
        let gate = self.gate.clone();
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(async move {
                let inserted = tokio::time::timeout(Duration::from_secs(2), gate.wait())
                    .await
                    .unwrap_or_default();
                if !inserted || !write_terminal(&pool, &id, outcome, latency, usage).await {
                    tracing::warn!("request history cancellation write unavailable");
                }
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[tokio::test]
    async fn cancelling_finalization_retries_without_losing_terminal_outcome() {
        let pool = crate::db::connect("sqlite::memory:").await.unwrap();
        let id = uuid::Uuid::new_v4().to_string();
        request_history::save(
            &pool,
            &Record {
                id: id.clone(),
                source: "api".into(),
                model: "test".into(),
                streaming: true,
                outcome: "streaming".into(),
                status: 200,
                latency_ms: 1,
                input_tokens: None,
                output_tokens: None,
                attempts: vec![],
            },
        )
        .await
        .unwrap();
        // Hold the only connection so finishing cannot complete before abort.
        let connection = pool.acquire().await.unwrap();
        let (ready, received) = tokio::sync::oneshot::channel();
        let task = tokio::spawn({
            let pool = pool.clone();
            let id = id.clone();
            async move {
                let gate = InsertGate::pending();
                gate.complete(true);
                let mut guard = StreamRecord {
                    pool,
                    id,
                    started: Instant::now(),
                    usage: Usage::default(),
                    gate,
                    done: false,
                    finalized: false,
                    final_outcome: None,
                };
                ready.send(()).unwrap();
                guard.finish("interrupted").await;
            }
        });
        received.await.unwrap();
        task.abort();
        let _ = task.await;
        drop(connection);
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let outcome: String =
                    sqlx::query_scalar("SELECT outcome FROM request_history WHERE id = ?")
                        .bind(&id)
                        .fetch_one(&pool)
                        .await
                        .unwrap();
                if outcome == "interrupted" {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn history_insert_does_not_block_the_response() {
        let state = crate::app::AppState::from_config(crate::config::Config {
            database_url: "sqlite::memory:".into(),
            bind: "127.0.0.1:0".parse().unwrap(),
            crypto: crate::crypto::Crypto::from_base64(&secrecy::SecretString::from(
                base64::Engine::encode(&base64::engine::general_purpose::STANDARD, [9u8; 32]),
            ))
            .unwrap(),
            api_key: None,
            dashboard_password: None,
            dashboard_secure_cookie: false,
        })
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
            started.elapsed() < Duration::from_millis(500),
            "history insert delayed the response by {:?}",
            started.elapsed()
        );
        assert_eq!(response.status(), axum::http::StatusCode::OK);
        drop(held);
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if request_history::count(&state.db).await.unwrap() == 1 {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("background insert should land after the connection is released");
    }

    #[tokio::test]
    async fn stream_finalization_waits_for_the_insert() {
        let pool = crate::db::connect("sqlite::memory:").await.unwrap();
        let id = uuid::Uuid::new_v4().to_string();
        let gate = InsertGate::pending();
        let mut guard = StreamRecord {
            pool: pool.clone(),
            id: id.clone(),
            started: Instant::now(),
            usage: Usage::default(),
            gate: gate.clone(),
            done: false,
            finalized: false,
            final_outcome: None,
        };
        let finished = tokio::spawn(async move {
            guard.finish("interrupted").await;
            guard.finalized
        });
        for _ in 0..8 {
            tokio::task::yield_now().await;
        }
        assert_eq!(request_history::count(&pool).await.unwrap(), 0);
        request_history::save(
            &pool,
            &Record {
                id: id.clone(),
                source: "api".into(),
                model: "test".into(),
                streaming: true,
                outcome: "streaming".into(),
                status: 200,
                latency_ms: 1,
                input_tokens: None,
                output_tokens: None,
                attempts: vec![],
            },
        )
        .await
        .unwrap();
        gate.complete(true);
        assert!(finished.await.unwrap());
        let outcome: String =
            sqlx::query_scalar("SELECT outcome FROM request_history WHERE id = ?")
                .bind(&id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(outcome, "interrupted");
    }
}
