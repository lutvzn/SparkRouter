//! Route a request by its provider-prefixed public model ID.
//!
//! `combo/<name>` stays on the combo path. Any other model is looked up in the
//! last refreshed connection lists within that provider. Only the upstream
//! suffix is sent to enabled connections, oldest first. The retry policy matches
//! combos: each connection at most once, and a permanent error stops the chain.

use crate::{
    app::AppState,
    db::{connection_models, provider_slugs},
    error::AppError,
    providers::AttemptError,
};
use axum::response::Response;
use serde_json::Value;

const MAX_MODEL_CHARS: usize = crate::db::provider_slugs::MAX_PUBLIC_MODEL_CHARS;

pub async fn route(state: &AppState, model: &str, body: &Value) -> Result<Response, AppError> {
    route_inner(
        state,
        model,
        body,
        &super::affinity::RequestContext::default(),
        None,
    )
    .await
}

pub(crate) async fn route_with_trace(
    state: &AppState,
    model: &str,
    body: &Value,
    context: &super::affinity::RequestContext,
    attempts: &mut Vec<super::combo::AttemptTrace>,
) -> Result<Response, AppError> {
    route_inner(state, model, body, context, Some(attempts)).await
}

async fn route_inner(
    state: &AppState,
    model: &str,
    body: &Value,
    context: &super::affinity::RequestContext,
    mut attempts: Option<&mut Vec<super::combo::AttemptTrace>>,
) -> Result<Response, AppError> {
    let public_model = validate_model(model)?;
    let (slug, upstream_model) = public_model.split_once('/').ok_or_else(|| {
        AppError::NotFound(
            "direct models require a provider slug; use an ID from /v1/models".into(),
        )
    })?;
    if slug.is_empty()
        || slug.len() > crate::db::provider_slugs::MAX_SLUG_CHARS
        || upstream_model.is_empty()
        || upstream_model.chars().count() > 256
    {
        return Err(AppError::InvalidRequest(
            "model must be <provider-slug>/<upstream-model-id>".into(),
        ));
    }
    let model = upstream_model.to_owned();
    let provider_ids =
        connection_models::providers_for_public_model(&state.db, slug, &model).await?;
    if provider_ids.is_empty() {
        return Err(AppError::NotFound(format!(
            "no connection lists public model '{public_model}'"
        )));
    }
    for (index, provider_id) in provider_ids.into_iter().enumerate() {
        let (provider, namespace, group_name) =
            match provider_slugs::connection_with_name(&state.db, &provider_id).await? {
                Some(provider) => provider,
                // A refresh snapshot can outlive a connection deleted mid-request.
                None => continue,
            };
        // Endpoint edits can change an unassigned connection's namespace.
        // Validate the namespace from the same snapshot as its credentials/URL.
        if namespace != slug || !context.accepts(provider.protocol) {
            continue;
        }
        if !provider.enabled {
            tracing::debug!(provider = %provider.id, model = %model, "disabled provider skipped");
            continue;
        }
        let started = std::time::Instant::now();
        let mut trace = super::combo::AttemptTrace::new(
            provider.id.clone(),
            provider.name.clone(),
            model.clone(),
            index as i64 + 1,
            started,
        );
        trace.provider_snapshot = Some(crate::db::request_history::ProviderSnapshot {
            slug: namespace,
            name: group_name,
        });
        let attempt = super::attempt(state, &provider, &model, body, trace, &mut attempts).await;
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
            "model must be combo/<name> or <provider-slug>/<upstream-model-id> (up to 321 characters)".into(),
        ));
    }
    Ok(model.to_owned())
}
