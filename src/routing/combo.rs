use crate::{
    app::AppState,
    db::{combos, providers as provider_db},
    error::AppError,
    providers::{self, AttemptError},
};
use axum::{http::StatusCode, response::Response};
use serde_json::Value;

/// Deliberately small policy: transient transport failures are handled separately.
/// Authentication, permissions, not-found and malformed requests stop the chain.
pub fn is_retryable_status(status: StatusCode) -> bool {
    matches!(status.as_u16(), 408 | 429 | 500 | 502 | 503 | 504)
}

pub async fn route(state: &AppState, name: &str, body: &Value) -> Result<Response, AppError> {
    let combo = combos::by_name(&state.db, name).await?;
    let steps = combos::steps(&state.db, &combo.id).await?;
    for step in steps {
        let provider = match provider_db::get(&state.db, &step.provider_id).await {
            Ok(provider) => provider,
            // A dashboard edit can remove this snapshotted step and then its
            // provider while an earlier upstream attempt is still in progress.
            Err(AppError::NotFound(_)) => continue,
            Err(error) => return Err(error),
        };
        if !provider.enabled {
            tracing::debug!(combo = name, provider = %provider.id, "disabled provider skipped");
            continue;
        }
        let started = std::time::Instant::now();
        let attempt = tokio::time::timeout(
            std::time::Duration::from_secs(120),
            providers::call(&state.client, &state.crypto, &provider, &step.model, body),
        )
        .await
        .unwrap_or(Err(AttemptError::Retryable));
        match attempt {
            Ok(response) => {
                tracing::info!(combo = name, provider = %provider.id, model = %step.model,
                    status = response.status().as_u16(), latency_ms = started.elapsed().as_millis(), "upstream selected");
                // The response body now exclusively owns this attempt. Stream errors
                // MUST terminate that body; they cannot re-enter this fallback loop.
                return Ok(response);
            }
            Err(AttemptError::Retryable) => {
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
    fn retry_policy_is_explicit() {
        for status in [408, 429, 500, 502, 503, 504] {
            assert!(is_retryable_status(StatusCode::from_u16(status).unwrap()));
        }
        for status in [200, 301, 400, 401, 403, 404, 422, 501] {
            assert!(!is_retryable_status(StatusCode::from_u16(status).unwrap()));
        }
    }
}
