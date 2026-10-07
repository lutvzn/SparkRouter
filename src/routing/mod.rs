pub mod combo;
pub mod direct;

use axum::response::Response;
use serde_json::Value;
use std::time::Duration;

use crate::{
    app::AppState,
    db::providers::Provider,
    error::AppError,
    providers::{self, AttemptError},
};
use combo::AttemptTrace;

/// Prepare one upstream response. Returning success ends fallback; the body
/// exclusively owns the committed attempt, including any subsequent failure.
async fn attempt(
    state: &AppState,
    provider: &Provider,
    model: &str,
    body: &Value,
    trace: AttemptTrace,
    attempts: &mut Option<&mut Vec<AttemptTrace>>,
) -> Result<Response, AttemptError> {
    if let Some(attempts) = attempts.as_deref_mut() {
        attempts.push(trace);
    }
    let result = tokio::time::timeout(
        Duration::from_secs(120),
        providers::call(&state.client, &state.crypto, provider, model, body),
    )
    .await
    .unwrap_or(Err(AttemptError::Retryable));
    if let Some(trace) = attempts.as_deref_mut().and_then(|items| items.last_mut()) {
        trace.finish(match &result {
            Ok(_) => "selected",
            Err(AttemptError::Retryable) => "retryable_failure",
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
    attempts: &mut Vec<AttemptTrace>,
) -> Result<Response, AppError> {
    let result = tokio::time::timeout(Duration::from_secs(180), async {
        if let Some(name) = model.strip_prefix("combo/") {
            combo::route_with_trace(state, name, body, attempts).await
        } else {
            direct::route_with_trace(state, model, body, attempts).await
        }
    })
    .await;
    result.unwrap_or_else(|_| {
        if let Some(trace) = attempts
            .last_mut()
            .filter(|trace| trace.outcome == "in_progress")
        {
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

    #[tokio::test]
    async fn cancelling_precommit_routing_keeps_the_in_progress_trace() {
        let state = AppState::from_config(Config {
            database_url: "sqlite::memory:".into(),
            bind: "127.0.0.1:0".parse().unwrap(),
            crypto: Crypto::from_base64(&SecretString::from(STANDARD.encode([7; 32]))).unwrap(),
            api_key: None,
            dashboard_password: None,
            dashboard_secure_cookie: false,
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
            {
                let routing = route_with_trace(&state, model, &body, &mut attempts);
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
