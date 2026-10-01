//! Route a request by the upstream model ID itself.
//!
//! `combo/<name>` stays on the combo path. Any other model is looked up in the
//! last refreshed connection lists and sent, unchanged, to each enabled
//! connection that listed it. Oldest connection first. The retry policy matches
//! combos: each connection at most once, and a permanent error stops the chain.

use crate::{
    app::AppState,
    db::{connection_models, providers as provider_db},
    error::AppError,
    providers::{self, AttemptError},
};
use axum::response::Response;
use serde_json::Value;

const MAX_MODEL_CHARS: usize = 256;

pub async fn route(state: &AppState, model: &str, body: &Value) -> Result<Response, AppError> {
    let model = validate_model(model)?;
    let provider_ids = connection_models::providers_for_model(&state.db, &model).await?;
    if provider_ids.is_empty() {
        return Err(AppError::NotFound(format!(
            "no connection lists model '{model}'"
        )));
    }
    for provider_id in provider_ids {
        let provider = match provider_db::get(&state.db, &provider_id).await {
            Ok(provider) => provider,
            // A refresh snapshot can outlive a connection deleted mid-request.
            Err(AppError::NotFound(_)) => continue,
            Err(error) => return Err(error),
        };
        if !provider.enabled {
            tracing::debug!(provider = %provider.id, model = %model, "disabled provider skipped");
            continue;
        }
        let started = std::time::Instant::now();
        let attempt = tokio::time::timeout(
            std::time::Duration::from_secs(120),
            providers::call(&state.client, &state.crypto, &provider, &model, body),
        )
        .await
        .unwrap_or(Err(AttemptError::Retryable));
        match attempt {
            Ok(response) => {
                tracing::info!(
                    provider = %provider.id,
                    model = %model,
                    status = response.status().as_u16(),
                    latency_ms = started.elapsed().as_millis(),
                    "upstream selected"
                );
                // The response body now exclusively owns this attempt.
                return Ok(response);
            }
            Err(AttemptError::Retryable) => {
                tracing::warn!(
                    provider = %provider.id,
                    model = %model,
                    latency_ms = started.elapsed().as_millis(),
                    "retryable upstream failure; trying next connection"
                );
            }
            Err(AttemptError::Permanent(error)) => {
                tracing::warn!(
                    provider = %provider.id,
                    model = %model,
                    latency_ms = started.elapsed().as_millis(),
                    error = %error,
                    "upstream attempt stopped direct routing"
                );
                return Err(error);
            }
        }
    }
    Err(AppError::Routing)
}

fn validate_model(model: &str) -> Result<String, AppError> {
    let model = model.trim();
    if model.is_empty()
        || model.chars().count() > MAX_MODEL_CHARS
        || model.chars().any(char::is_control)
    {
        return Err(AppError::InvalidRequest(
            "model must be combo/<name> or a provider model id of at most 256 characters".into(),
        ));
    }
    Ok(model.to_owned())
}
