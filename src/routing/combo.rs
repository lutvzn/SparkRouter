use crate::{app::AppState, db::combos, error::AppError, providers::AttemptError};
use aes_gcm::aead::{OsRng, rand_core::RngCore};
use axum::{http::StatusCode, response::Response};
use serde_json::Value;
use std::{collections::HashMap, sync::Mutex};

/// Ephemeral per-combo cursors shared by every request handler in this process.
/// No lock is held during database or upstream I/O; restarts reset the sequence.
#[derive(Default)]
pub(crate) struct RoundRobin {
    cursors: Mutex<HashMap<String, usize>>,
}

impl RoundRobin {
    fn next(&self, id: &str, count: usize) -> Result<usize, AppError> {
        let mut cursors = self.cursors.lock().map_err(|_| AppError::Routing)?;
        let cursor = cursors.entry(id.to_owned()).or_default();
        let start = *cursor % count;
        *cursor = (start + 1) % count;
        Ok(start)
    }

    pub(crate) fn forget(&self, id: &str) {
        if let Ok(mut cursors) = self.cursors.lock() {
            cursors.remove(id);
        }
    }
}

/// Rejection sampling avoids modulo bias when selecting a random starting step.
fn random_start(count: usize, mut draw: impl FnMut() -> u64) -> usize {
    let count = count as u64;
    let limit = u64::MAX - u64::MAX % count;
    loop {
        let value = draw();
        if value < limit {
            return (value % count) as usize;
        }
    }
}

/// Deliberately small policy: transient transport failures are handled separately.
///
/// 401, 402, 404, and 529 describe that target (bad credential, billing, missing
/// model, Anthropic overload), so another connection or combo step may still
/// succeed. 400 and 422 are properties of the request. 403 stays permanent
/// because it can be a policy rejection rather than a missing credential.
pub fn is_retryable_status(status: StatusCode) -> bool {
    matches!(
        status.as_u16(),
        401 | 402 | 404 | 408 | 429 | 500 | 502 | 503 | 504 | 529
    )
}

/// Request-local diagnostics for the dashboard; never contains credentials,
/// endpoint URLs, request content, or upstream error bodies.
#[derive(serde::Serialize)]
pub(crate) struct AttemptTrace {
    pub provider_id: String,
    pub provider_name: String,
    pub model: String,
    pub position: i64,
    pub outcome: &'static str,
    pub latency_ms: u128,
    pub reason: String,
    pub routing: Option<RoutingSelection>,
    #[serde(skip)]
    pub provider_snapshot: Option<crate::db::request_history::ProviderSnapshot>,
    #[serde(skip)]
    started: std::time::Instant,
}

#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub(crate) struct RoutingSelection {
    pub strategy: String,
    pub affinity_mode: String,
    pub affinity: String,
    pub strategy_position: i64,
    pub starting_position: i64,
    /// Cooling targets moved to the end of this request's order.
    #[serde(default)]
    pub cooling_deferred: i64,
}

impl AttemptTrace {
    pub(crate) fn new(
        provider_id: String,
        provider_name: String,
        model: String,
        position: i64,
        started: std::time::Instant,
    ) -> Self {
        Self {
            provider_id,
            provider_name,
            model,
            position,
            outcome: "in_progress",
            latency_ms: 0,
            reason: "Preparation not completed".into(),
            routing: None,
            provider_snapshot: None,
            started,
        }
    }
    pub(crate) fn finish(&mut self, outcome: &'static str) {
        self.outcome = outcome;
        self.latency_ms = self.started.elapsed().as_millis();
    }
}

pub(crate) async fn route_with_trace(
    state: &AppState,
    name: &str,
    body: &Value,
    context: &super::affinity::RequestContext,
    started: tokio::time::Instant,
    attempts: &mut Vec<AttemptTrace>,
) -> Result<Response, AppError> {
    let routing = state
        .config
        .combo(&state.db, name, context.category())
        .await?;
    let (combo, snapshot) = &*routing;
    let mut steps: Vec<_> = snapshot
        .iter()
        .filter(|entry| entry.provider.enabled && context.accepts(&entry.provider))
        .map(|entry| entry.step.clone())
        .collect();
    let binding = super::affinity::binding(combo, context, &state.crypto, body)?;
    {
        // Disabled connections do not consume turns. Snapshot eligible steps
        // before selecting a start, but recheck providers below before calls so
        // concurrent deletion/disable edits retain the existing safety behavior.
        if steps.is_empty() {
            return Err(AppError::Routing);
        }
        let start = match combo.strategy {
            combos::Strategy::RoundRobin => state.round_robin.next(&combo.id, steps.len())?,
            combos::Strategy::Random => random_start(steps.len(), || OsRng.next_u64()),
            combos::Strategy::Priority => 0,
        };
        // Each step is attempted at most once; fallback follows configured order
        // cyclically from the selected start without advancing the cursor again.
        steps.rotate_left(start);
    }
    let deadline = super::Budget::for_context(context).deadline(started, steps.len());
    let strategy_position = steps.first().map_or(0, |step| step.position + 1);
    let mut affinity = if binding.is_some() {
        "Miss or expired"
    } else {
        "Off or no usable identity"
    };
    // Reserve the normal strategy once even on affinity hits. Override only
    // the start; cyclic configured fallback still visits every step once.
    if let Some(binding) = &binding
        && let Some(target) = state.affinity.preferred(binding)
    {
        if let Some(start) = steps.iter().position(|step| target.matches(step)) {
            steps.rotate_left(start);
            affinity = "Hit: preferred target selected";
        } else {
            affinity = "Preferred target unavailable; strategy used";
        }
    }
    let cooldown = state.config.cooldown(&state.db).await?;
    let cooling_deferred = if cooldown {
        super::cooldown::defer(&mut steps, |step| {
            state.cooldown.is_cooling(&step.provider_id, &step.model)
        })
    } else {
        0
    };
    state.metrics.deferred(cooling_deferred);
    // A continued stored response only exists on the connection that made it.
    if let Some(connection) = &context.preferred_connection
        && super::cooldown::prefer(&mut steps, |step| &step.provider_id == connection)
    {
        affinity = "Stored response's connection selected";
    }
    let selection = RoutingSelection {
        strategy: combo.strategy.label().into(),
        affinity_mode: combo.affinity.mode.label().into(),
        affinity: affinity.into(),
        strategy_position,
        starting_position: steps.first().map_or(0, |step| step.position + 1),
        cooling_deferred: cooling_deferred as i64,
    };
    let remaining = steps.len();
    for (index, step) in steps.into_iter().enumerate() {
        if super::deadline_exhausted(deadline, remaining - index) {
            break;
        }
        let connection = match state
            .config
            .connection(&state.db, &step.provider_id)
            .await?
        {
            Some(connection) => connection,
            // A dashboard edit can remove this snapshotted step and then its
            // provider while an earlier upstream attempt is still in progress.
            None => continue,
        };
        let (provider, slug, group_name) = &*connection;
        if !context.accepts(provider) {
            continue;
        }
        if !provider.enabled {
            tracing::debug!(combo = name, provider = %provider.id, "disabled provider skipped");
            continue;
        }
        let started = std::time::Instant::now();
        let mut trace = AttemptTrace::new(
            provider.id.clone(),
            provider.name.clone(),
            step.model.clone(),
            step.position + 1,
            started,
        );
        trace.routing = Some(selection.clone());
        trace.provider_snapshot = Some(crate::db::request_history::ProviderSnapshot {
            slug: slug.clone(),
            name: group_name.clone(),
        });
        let attempt = super::attempt(
            state,
            provider,
            &step.model,
            body,
            context,
            deadline,
            trace,
            attempts,
        )
        .await;
        if cooldown {
            state.cooldown.record(&provider.id, &step.model, &attempt);
        }
        match attempt {
            Ok(response) => {
                tracing::info!(combo = name, provider = %provider.id, model = %step.model,
                    status = response.status().as_u16(), latency_ms = started.elapsed().as_millis(), "upstream selected");
                // The response body now exclusively owns this attempt. Stream errors
                // MUST terminate that body; they cannot re-enter this fallback loop.
                if let Some(binding) = binding {
                    return Ok(super::affinity::remember_on_success(
                        &state.affinity,
                        binding,
                        super::affinity::Target::from_step(&step),
                        body,
                        response,
                    ));
                }
                return Ok(response);
            }
            Err(AttemptError::Retryable | AttemptError::RetryableStatus(..)) => {
                if let Some(binding) = &binding {
                    state
                        .affinity
                        .forget_target(binding, &super::affinity::Target::from_step(&step));
                }
                tracing::warn!(combo = name, provider = %provider.id, model = %step.model,
                    latency_ms = started.elapsed().as_millis(), "retryable upstream failure; trying next combo step");
            }
            Err(AttemptError::Permanent(error)) => {
                tracing::warn!(combo = name, provider = %provider.id, model = %step.model,
                    latency_ms = started.elapsed().as_millis(), error = %error,
                    "upstream attempt stopped the combo");
                return Err(error);
            }
        }
    }
    Err(AppError::Routing)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn attempt_trace_contains_only_routing_metadata() {
        let mut trace = AttemptTrace {
            provider_id: "connection-id".into(),
            provider_name: "Local connection".into(),
            model: "upstream-model".into(),
            position: 2,
            outcome: "in_progress",
            latency_ms: 0,
            reason: "Preparation not completed".into(),
            routing: None,
            provider_snapshot: None,
            started: std::time::Instant::now(),
        };
        trace.finish("timed_out");
        let value = serde_json::to_value(trace).unwrap();
        assert_eq!(value["outcome"], "timed_out");
        assert_eq!(value["position"], 2);
        assert_eq!(value.as_object().unwrap().len(), 8);
        assert_eq!(value["reason"], "Preparation not completed");
        assert!(value["routing"].is_null());
        assert!(value.get("started").is_none());
    }

    #[test]
    fn round_robin_wraps_and_keeps_combos_independent() {
        let cursors = RoundRobin::default();
        assert_eq!(cursors.next("a", 3).unwrap(), 0);
        assert_eq!(cursors.next("a", 3).unwrap(), 1);
        assert_eq!(cursors.next("b", 3).unwrap(), 0);
        assert_eq!(cursors.next("a", 3).unwrap(), 2);
        assert_eq!(cursors.next("a", 3).unwrap(), 0);
        assert_eq!(cursors.next("a", 2).unwrap(), 1);
        cursors.forget("a");
        assert_eq!(cursors.next("a", 2).unwrap(), 0);
        assert_eq!(cursors.next("b", 3).unwrap(), 1);
    }

    #[test]
    fn round_robin_concurrent_requests_reserve_one_turn_each() {
        let cursors = std::sync::Arc::new(RoundRobin::default());
        let threads: Vec<_> = (0..12)
            .map(|_| {
                let cursors = cursors.clone();
                std::thread::spawn(move || cursors.next("shared", 3).unwrap())
            })
            .collect();
        let mut counts = [0; 3];
        for thread in threads {
            counts[thread.join().unwrap()] += 1;
        }
        assert_eq!(counts, [4, 4, 4]);
    }

    #[test]
    fn random_start_covers_every_step_and_rejects_biased_tail() {
        for count in [1, 2, 3, 7] {
            for index in 0..count {
                assert_eq!(random_start(count, || index as u64), index);
            }
        }
        let mut draws = [u64::MAX, 5].into_iter();
        assert_eq!(random_start(3, || draws.next().unwrap()), 2);
        assert!(draws.next().is_none());
    }

    #[test]
    fn retry_policy_is_explicit() {
        for status in [401, 402, 404, 408, 429, 500, 502, 503, 504, 529] {
            assert!(is_retryable_status(StatusCode::from_u16(status).unwrap()));
        }
        for status in [200, 301, 400, 403, 422, 501] {
            assert!(!is_retryable_status(StatusCode::from_u16(status).unwrap()));
        }
    }
}
