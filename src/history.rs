//! Best-effort metadata recording; never changes response bytes or retry policy.
use std::time::{Duration, Instant};

use axum::{body::Body, response::Response};
use futures_util::StreamExt;
use serde_json::Value;
use sqlx::SqlitePool;

use crate::{
    app::AppState,
    db::request_history::{self, Attempt, Record},
    providers::sse::SseDecoder,
    routing::combo::AttemptTrace,
};

#[derive(Clone, Copy, Default)]
pub(crate) struct Usage {
    input: Option<i64>,
    output: Option<i64>,
}

impl Usage {
    pub(crate) fn from_value(value: &Value) -> Self {
        let usage = &value["usage"];
        Self {
            input: usage
                .get("prompt_tokens")
                .or_else(|| usage.get("input_tokens"))
                .and_then(token_count),
            output: usage
                .get("completion_tokens")
                .or_else(|| usage.get("output_tokens"))
                .and_then(token_count),
        }
    }

    fn merge(&mut self, other: Self) {
        self.input = other.input.or(self.input);
        self.output = other.output.or(self.output);
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
    response: Response,
) -> Response {
    let success = response.status().is_success();
    let is_stream = success && streaming;
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
    // Database contention or recording failures must never fail a generation.
    if !matches!(
        tokio::time::timeout(
            Duration::from_secs(2),
            request_history::save(&state.db, &record)
        )
        .await,
        Ok(Ok(()))
    ) {
        tracing::warn!("request history write unavailable");
        return response;
    }
    if !is_stream {
        return response;
    }
    let guard = StreamRecord {
        pool: state.db.clone(),
        id: record.id,
        started,
        usage,
        done: false,
        finalized: false,
        final_outcome: None,
    };
    let (parts, body) = response.into_parts();
    // Construct the guard outside the generator: even an entirely unpolled
    // dropped body records cancellation. Bytes/errors are forwarded unchanged.
    let stream = async_stream::stream! {
        let mut guard = guard;
        let mut decoder = Some(SseDecoder::default());
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
        guard.finish(if guard.done { "success" } else { "interrupted" }).await;
    };
    Response::from_parts(parts, Body::from_stream(stream))
}

struct StreamRecord {
    pool: SqlitePool,
    id: String,
    started: Instant,
    usage: Usage,
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
        if matches!(
            tokio::time::timeout(
                Duration::from_secs(2),
                request_history::finish(
                    &self.pool,
                    &self.id,
                    outcome,
                    millis(self.started),
                    self.usage.input,
                    self.usage.output
                )
            )
            .await,
            Ok(Ok(()))
        ) {
            self.finalized = true;
        } else {
            tracing::warn!("request history finalization unavailable");
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
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(async move {
                if !matches!(
                    tokio::time::timeout(
                        Duration::from_secs(2),
                        request_history::finish(
                            &pool,
                            &id,
                            outcome,
                            latency,
                            usage.input,
                            usage.output
                        )
                    )
                    .await,
                    Ok(Ok(()))
                ) {
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
                let mut guard = StreamRecord {
                    pool,
                    id,
                    started: Instant::now(),
                    usage: Usage::default(),
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
}
