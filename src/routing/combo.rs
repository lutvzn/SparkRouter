use crate::{
    app::AppState,
    db::{combos, providers as provider_db},
    error::AppError,
    providers::AttemptError,
};
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

pub async fn route(state: &AppState, name: &str, body: &Value) -> Result<Response, AppError> {
    route_inner(
        state,
        name,
        body,
        &super::affinity::RequestContext::default(),
        None,
    )
    .await
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
    #[serde(skip)]
    pub provider_snapshot: Option<crate::db::request_history::ProviderSnapshot>,
    #[serde(skip)]
    started: std::time::Instant,
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
    attempts: &mut Vec<AttemptTrace>,
) -> Result<Response, AppError> {
    route_inner(state, name, body, context, Some(attempts)).await
}

async fn route_inner(
    state: &AppState,
    name: &str,
    body: &Value,
    context: &super::affinity::RequestContext,
    mut attempts: Option<&mut Vec<AttemptTrace>>,
) -> Result<Response, AppError> {
    let combo = combos::by_name(&state.db, name).await?;
    if combo.category != context.category() {
        return Err(AppError::InvalidRequest(
            "Combo category does not match this API mode".into(),
        ));
    }
    let mut steps = combos::steps(&state.db, &combo.id).await?;
    // Revalidate every step before reserving a turn or making any provider call.
    // This also fails closed after metadata/protocol edits or legacy imports.
    let mut connection = state.db.acquire().await?;
    for step in &steps {
        crate::db::model_categories::validate(
            &mut connection,
            &step.provider_id,
            &step.model,
            combo.category,
            false,
        )
        .await?;
    }
    drop(connection);
    let binding = super::affinity::binding(&combo, context, &state.crypto, body)?;
    if combo.strategy != combos::Strategy::Priority
        || binding.is_some()
        || context.systemone
        || context.media.is_some()
    {
        // Disabled connections do not consume turns. Snapshot eligible steps
        // before selecting a start, but recheck providers below before calls so
        // concurrent deletion/disable edits retain the existing safety behavior.
        let mut eligible = Vec::with_capacity(steps.len());
        for step in steps {
            match provider_db::get(&state.db, &step.provider_id).await {
                Ok(provider) if provider.enabled && context.accepts(&provider) => {
                    eligible.push(step)
                }
                Ok(_) | Err(AppError::NotFound(_)) => {}
                Err(error) => return Err(error),
            }
        }
        steps = eligible;
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
    // Reserve the normal strategy once even on affinity hits. Override only
    // the start; cyclic configured fallback still visits every step once.
    if let Some(binding) = &binding
        && let Some(target) = state.affinity.preferred(binding)
        && let Some(start) = steps.iter().position(|step| target.matches(step))
    {
        steps.rotate_left(start);
    }
    for step in steps {
        let (provider, slug, group_name) =
            match crate::db::provider_slugs::connection_with_name(&state.db, &step.provider_id)
                .await?
            {
                Some(provider) => provider,
                // A dashboard edit can remove this snapshotted step and then its
                // provider while an earlier upstream attempt is still in progress.
                None => continue,
            };
        if !context.accepts(&provider) {
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
        trace.provider_snapshot = Some(crate::db::request_history::ProviderSnapshot {
            slug,
            name: group_name,
        });
        let attempt = super::attempt(
            state,
            &provider,
            &step.model,
            body,
            context,
            trace,
            &mut attempts,
        )
        .await;
        match attempt {
            Ok(response) => {
                tracing::info!(combo = name, provider = %provider.id, model = %step.model,
                    status = response.status().as_u16(), latency_ms = started.elapsed().as_millis(), "upstream selected");
                // The response body now exclusively owns this attempt. Stream errors
                // MUST terminate that body; they cannot re-enter this fallback loop.
                if let Some(binding) = binding {
                    let target = super::affinity::Target::from_step(&step);
                    if body.get("stream").and_then(Value::as_bool) == Some(true)
                        || response
                            .extensions()
                            .get::<crate::providers::media::BinaryStream>()
                            .is_some()
                    {
                        return Ok(super::affinity::on_complete(
                            response,
                            state.affinity.clone(),
                            binding,
                            target,
                        ));
                    }
                    state.affinity.remember(binding, target);
                }
                return Ok(response);
            }
            Err(AttemptError::Retryable) => {
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
            provider_snapshot: None,
            started: std::time::Instant::now(),
        };
        trace.finish("timed_out");
        let value = serde_json::to_value(trace).unwrap();
        assert_eq!(value["outcome"], "timed_out");
        assert_eq!(value["position"], 2);
        assert_eq!(value.as_object().unwrap().len(), 6);
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
