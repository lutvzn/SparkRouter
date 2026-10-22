pub(crate) mod affinity;
pub mod combo;
pub(crate) mod cooldown;
pub mod direct;

use axum::response::Response;
use serde_json::Value;
use std::time::Duration;
use tokio::time::Instant;

use crate::{
    app::AppState,
    db::providers::Provider,
    error::AppError,
    providers::{self, AttemptError},
};
use combo::AttemptTrace;

/// API boundary check before quota admission; direct model IDs are unchanged.
pub(crate) async fn validate_combo_category(
    state: &AppState,
    model: &str,
    category: crate::db::combos::Category,
) -> Result<(), AppError> {
    if let Some(name) = model.strip_prefix("combo/") {
        if state.config.has_combo(name, category) {
            return Ok(());
        }
        let combo = match crate::db::combos::by_name(&state.db, name).await {
            Ok(combo) => combo,
            Err(AppError::NotFound(_)) => return Ok(()), // Preserve normal not-found traces/history.
            Err(error) => return Err(error),
        };
        if combo.category != category {
            return Err(AppError::InvalidRequest(
                "Combo category does not match this API mode".into(),
            ));
        }
    }
    Ok(())
}

/// Pre-commit timing. Every attempt keeps its full deadline; the ceiling for
/// the whole fallback phase grows with the eligible chain (plus slack for
/// database preparation) so later steps are not silently skipped, up to a cap.
#[derive(Clone, Copy)]
pub(crate) struct Budget {
    attempt: Duration,
    slack: Duration,
    cap: Duration,
}

impl Budget {
    pub(crate) fn for_context(context: &affinity::RequestContext) -> Self {
        if context.media.is_some() {
            Self {
                attempt: Duration::from_secs(600),
                slack: Duration::from_secs(10),
                cap: Duration::from_secs(30 * 60),
            }
        } else {
            Self {
                attempt: Duration::from_secs(120),
                slack: Duration::from_secs(60),
                cap: Duration::from_secs(10 * 60),
            }
        }
    }

    /// Deadline for a chain of `steps` eligible attempts started at `started`.
    pub(crate) fn deadline(self, started: Instant, steps: usize) -> Instant {
        let steps = u32::try_from(steps.max(1)).unwrap_or(u32::MAX);
        started
            + self
                .attempt
                .saturating_mul(steps)
                .saturating_add(self.slack)
                .min(self.cap)
    }
}

/// True when the chain's deadline leaves no time for another attempt.
pub(crate) fn deadline_exhausted(deadline: Instant, remaining_steps: usize) -> bool {
    if Instant::now() < deadline {
        return false;
    }
    tracing::warn!(
        skipped = remaining_steps,
        "pre-commit deadline exhausted; remaining targets skipped"
    );
    true
}

/// Prepare one upstream response. Returning success ends fallback; the body
/// exclusively owns the committed attempt, including any subsequent failure.
#[allow(clippy::too_many_arguments)]
async fn attempt(
    state: &AppState,
    provider: &Provider,
    model: &str,
    body: &Value,
    context: &affinity::RequestContext,
    deadline: Instant,
    trace: AttemptTrace,
    attempts: &mut Vec<AttemptTrace>,
) -> Result<Response, AttemptError> {
    attempts.push(trace);
    let own_limit = Instant::now() + Budget::for_context(context).attempt;
    // Only a chain-level deadline (the cap) can end an attempt early.
    let capped = deadline < own_limit;
    let mut deadline_exceeded = false;
    let result = tokio::time::timeout_at(own_limit.min(deadline), async {
        if let Some(kind) = context.media {
            providers::media::call(
                &state.client,
                &state.crypto,
                provider,
                model,
                body,
                kind,
                context.upload.as_ref(),
            )
            .await
        } else {
            match context.endpoint {
                affinity::Endpoint::Responses => {
                    providers::openai::responses(
                        &state.client,
                        &state.crypto,
                        provider,
                        model,
                        body,
                    )
                    .await
                }
                affinity::Endpoint::Embeddings => {
                    providers::openai::embeddings(
                        &state.client,
                        &state.crypto,
                        provider,
                        model,
                        body,
                    )
                    .await
                }
                affinity::Endpoint::Messages => {
                    providers::anthropic::messages(
                        &state.client,
                        &state.crypto,
                        provider,
                        model,
                        body,
                        &context.upstream_headers,
                    )
                    .await
                }
                affinity::Endpoint::Chat | affinity::Endpoint::SystemOne => {
                    providers::call(&state.client, &state.crypto, provider, model, body).await
                }
            }
        }
    })
    .await
    .unwrap_or_else(|_| {
        Err(if capped {
            deadline_exceeded = true;
            AttemptError::Permanent(AppError::Routing)
        } else if context.media.is_some() {
            AttemptError::Permanent(AppError::Routing)
        } else {
            AttemptError::Retryable
        })
    });
    if let Some(trace) = attempts.last_mut() {
        if deadline_exceeded {
            trace.reason = "Gateway pre-commit deadline exceeded".into();
            trace.finish("timed_out");
            return result;
        }
        trace.reason = match &result {
            Ok(response) => format!("Upstream selected (HTTP {}); body owns any subsequent failure", response.status().as_u16()),
            Err(AttemptError::Retryable) => "Retryable upstream failure: transport or invalid/incomplete response".into(),
            Err(AttemptError::RetryableStatus(status, _)) => format!("Retryable upstream HTTP {}: {}", status.as_u16(), status.canonical_reason().unwrap_or("target unavailable")),
            Err(AttemptError::Permanent(AppError::Upstream(status))) => format!("Non-retryable upstream HTTP {}", status.as_u16()),
            Err(AttemptError::Permanent(_)) => "Non-retryable request/provider failure or possibly accepted media job; fallback stopped".into(),
        };
        trace.finish(match &result {
            Ok(_) => "selected",
            Err(AttemptError::Retryable | AttemptError::RetryableStatus(..)) => "retryable_failure",
            Err(AttemptError::Permanent(_)) => "permanent_failure",
        });
    }
    result
}

/// Bound all pre-commit work, retaining an unfinished attempt on cancellation.
pub(crate) async fn route_with_trace(
    state: &AppState,
    model: &str,
    body: &Value,
    context: &affinity::RequestContext,
    attempts: &mut Vec<AttemptTrace>,
) -> Result<Response, AppError> {
    // Hard ceiling covering database stalls before the chain length is known;
    // the routers apply the tighter, chain-scaled deadline from the same start.
    let started = Instant::now();
    let result = tokio::time::timeout(Budget::for_context(context).cap, async {
        if let Some(name) = model.strip_prefix("combo/") {
            combo::route_with_trace(state, name, body, context, started, attempts).await
        } else {
            direct::route_with_trace(state, model, body, context, started, attempts).await
        }
    })
    .await;
    result.unwrap_or_else(|_| {
        if let Some(trace) = attempts
            .last_mut()
            .filter(|trace| trace.outcome == "in_progress")
        {
            trace.reason = "Gateway preparation deadline exceeded".into();
            trace.finish("timed_out");
        }
        Err(AppError::Routing)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        config::Config,
        crypto::Crypto,
        db::{combos, connection_models},
        providers::Protocol,
    };
    use base64::{Engine, engine::general_purpose::STANDARD};
    use secrecy::SecretString;

    #[test]
    fn chain_deadline_grows_with_eligible_steps_up_to_the_cap() {
        let started = Instant::now();
        let chat = Budget::for_context(&affinity::RequestContext::default());
        let total = |budget: Budget, steps| budget.deadline(started, steps) - started;
        // A single step keeps the previous 120 s attempt + 60 s ceiling.
        assert_eq!(total(chat, 0), Duration::from_secs(180));
        assert_eq!(total(chat, 1), Duration::from_secs(180));
        // Every hanging step of a three-step chain still gets its full attempt.
        assert_eq!(total(chat, 3), Duration::from_secs(420));
        assert_eq!(total(chat, 10), Duration::from_secs(600));
        assert_eq!(total(chat, usize::MAX), Duration::from_secs(600));
        let media = Budget::for_context(
            &affinity::RequestContext::default()
                .for_media(crate::providers::media::Kind::Image, None),
        );
        assert_eq!(total(media, 1), Duration::from_secs(610));
        assert_eq!(total(media, 2), Duration::from_secs(1210));
        assert_eq!(total(media, 5), Duration::from_secs(1800));
    }

    #[tokio::test]
    async fn chain_deadline_stops_an_attempt_and_records_why() {
        let state = AppState::from_config(Config {
            database_url: "sqlite::memory:".into(),
            bind: "127.0.0.1:0".parse().unwrap(),
            crypto: Crypto::from_base64(&SecretString::from(STANDARD.encode([7; 32]))).unwrap(),
            api_key: None,
            dashboard_login: crate::config::DashboardLogin::Disabled,
            dashboard_secure_cookie: false,
            metrics_token: None,
        })
        .await
        .unwrap();
        // Accepts but never answers, so only the chain deadline can end the attempt.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let provider = Provider {
            id: "hanging".into(),
            name: "Hanging connection".into(),
            base_url: format!("http://{}", listener.local_addr().unwrap()),
            protocol: Protocol::OpenAi,
            encrypted_api_key: vec![],
            enabled: true,
        };
        let accept = tokio::spawn(async move {
            let socket = listener.accept().await.unwrap().0;
            std::future::pending::<()>().await;
            drop(socket);
        });
        let context = affinity::RequestContext::default();
        let mut traces = Vec::new();
        let trace = AttemptTrace::new(
            provider.id.clone(),
            provider.name.clone(),
            "model".into(),
            1,
            std::time::Instant::now(),
        );
        let result = attempt(
            &state,
            &provider,
            "model",
            &serde_json::json!({"messages":[]}),
            &context,
            Instant::now() + Duration::from_millis(200),
            trace,
            &mut traces,
        )
        .await;
        accept.abort();
        // The chain deadline is final: no further fallback is attempted.
        assert!(matches!(
            result,
            Err(AttemptError::Permanent(AppError::Routing))
        ));
        assert_eq!(traces[0].outcome, "timed_out");
        assert_eq!(traces[0].reason, "Gateway pre-commit deadline exceeded");
        assert!(deadline_exhausted(Instant::now(), 1));
    }

    #[tokio::test]
    async fn cancelling_precommit_routing_keeps_the_in_progress_trace() {
        let state = AppState::from_config(Config {
            database_url: "sqlite::memory:".into(),
            bind: "127.0.0.1:0".parse().unwrap(),
            crypto: Crypto::from_base64(&SecretString::from(STANDARD.encode([7; 32]))).unwrap(),
            api_key: None,
            dashboard_login: crate::config::DashboardLogin::Disabled,
            dashboard_secure_cookie: false,
            metrics_token: None,
        })
        .await
        .unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let provider = Provider {
            id: "pending".into(),
            name: "Pending connection".into(),
            base_url: format!("http://{}", listener.local_addr().unwrap()),
            protocol: Protocol::OpenAi,
            encrypted_api_key: vec![],
            enabled: true,
        };
        crate::db::providers::save(&state.db, &provider)
            .await
            .unwrap();
        let combo = combos::create(&state.db, "pending").await.unwrap();
        combos::add_step(&state.db, &combo.id, &provider.id, "nested/model")
            .await
            .unwrap();
        connection_models::replace(
            &state.db,
            &provider.id,
            &[connection_models::StoredModel {
                id: "nested/model".into(),
                name: "Model".into(),
                fields: vec![],
                free: false,
            }],
            false,
        )
        .await
        .unwrap();
        let direct = connection_models::routable_model_ids(&state.db)
            .await
            .unwrap()
            .remove(0);
        for model in ["combo/pending", direct.as_str()] {
            let body = serde_json::json!({"model": model});
            let mut attempts = Vec::new();
            let context = affinity::RequestContext::default();
            {
                let routing = route_with_trace(&state, model, &body, &context, &mut attempts);
                tokio::pin!(routing);
                // Accept but never answer. Connection establishment proves the
                // attempt started, without sleeps or a billable upstream call.
                tokio::select! {
                    result = &mut routing => panic!("unexpected completion: {}", result.is_ok()),
                    accepted = tokio::time::timeout(Duration::from_secs(5), listener.accept()) => {
                        let _socket = accepted.unwrap().unwrap().0;
                    }
                }
            }
            assert_eq!(attempts.len(), 1);
            assert_eq!(attempts[0].outcome, "in_progress");
            assert_eq!(attempts[0].model, "nested/model");
            assert_eq!(attempts[0].position, 1);
            assert!(attempts[0].provider_snapshot.is_some());
        }
    }
}
