//! Route a request by its provider-prefixed public model ID.
//!
//! `combo/<name>` stays on the combo path. Any other model is looked up in the
//! last refreshed connection lists within that provider. Only the upstream
//! suffix is sent to enabled connections, oldest first. The retry policy matches
//! combos: each connection at most once, and a permanent error stops the chain.
//! While Settings' direct-route affinity is on, the connection that last
//! succeeded for this caller, endpoint, and model goes first until it expires.

use crate::{app::AppState, error::AppError, providers::AttemptError};
use axum::response::Response;
use serde_json::Value;

const MAX_MODEL_CHARS: usize = crate::db::provider_slugs::MAX_PUBLIC_MODEL_CHARS;

pub(crate) async fn route_with_trace(
    state: &AppState,
    model: &str,
    body: &Value,
    context: &super::affinity::RequestContext,
    started: tokio::time::Instant,
    attempts: &mut Vec<super::combo::AttemptTrace>,
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
    let provider_ids = state.config.direct_targets(&state.db, slug, &model).await?;
    if provider_ids.is_empty() {
        return Err(AppError::NotFound(format!(
            "no connection lists public model '{public_model}'"
        )));
    }
    let cooldown = state.config.cooldown(&state.db).await?;
    let affinity_minutes = state.config.direct_affinity(&state.db).await?;
    let binding = super::affinity::direct_binding(&public_model, affinity_minutes, context);
    let hint = binding
        .as_ref()
        .and_then(|binding| state.affinity.preferred(binding));
    let hinted = hint
        .as_ref()
        .map(super::affinity::Target::connection)
        .filter(|connection| provider_ids.iter().any(|id| id == connection));
    let preferred = context
        .preferred_connection
        .as_ref()
        .filter(|connection| provider_ids.contains(connection));
    let cooling = |id: &String| state.cooldown.is_cooling(id, &model);
    let mut cooling_deferred = 0;
    // Copy the cached listing only when its order changes.
    let ordered: std::borrow::Cow<[String]> = if hinted.is_some()
        || preferred.is_some()
        || (cooldown && provider_ids.iter().any(cooling))
    {
        let mut ids = provider_ids.to_vec();
        if let Some(hinted) = hinted {
            super::cooldown::prefer(&mut ids, |id| id == hinted);
        }
        if cooldown {
            cooling_deferred = super::cooldown::defer(&mut ids, cooling);
            state.metrics.deferred(cooling_deferred);
        }
        // A continued stored response only exists on the connection that made it.
        if let Some(preferred) = preferred {
            super::cooldown::prefer(&mut ids, |id| id == preferred);
        }
        ids.into()
    } else {
        (&*provider_ids).into()
    };
    let selection = super::combo::RoutingSelection {
        strategy: "Oldest connection first".into(),
        affinity_mode: if binding.is_some() {
            format!("{affinity_minutes} min")
        } else {
            "off".into()
        },
        affinity: if preferred.is_some() {
            "Stored response's connection selected"
        } else if hinted.is_some() {
            "Hit: last successful connection selected"
        } else if hint.is_some() {
            "Last successful connection unavailable; oldest first used"
        } else if binding.is_some() {
            "Miss or expired"
        } else {
            "Off"
        }
        .into(),
        strategy_position: 1,
        starting_position: ordered.first().map_or(0, |first| {
            provider_ids.iter().position(|id| id == first).unwrap_or(0) as i64 + 1
        }),
        cooling_deferred: cooling_deferred as i64,
    };
    let provider_ids = ordered;
    // Listed connections bound the chain; ineligible ones only add headroom.
    let remaining = provider_ids.len();
    let deadline = super::Budget::for_context(context).deadline(started, remaining);
    for (index, provider_id) in provider_ids.iter().enumerate() {
        if super::deadline_exhausted(deadline, remaining - index) {
            break;
        }
        let connection = match state.config.connection(&state.db, provider_id).await? {
            Some(connection) => connection,
            // A refresh snapshot can outlive a connection deleted mid-request.
            None => continue,
        };
        let (provider, namespace, group_name) = &*connection;
        // Endpoint edits can change an unassigned connection's namespace.
        // Validate the namespace from the same snapshot as its credentials/URL.
        if namespace != slug || !context.accepts(provider) {
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
        trace.routing = Some(selection.clone());
        trace.provider_snapshot = Some(crate::db::request_history::ProviderSnapshot {
            slug: namespace.clone(),
            name: group_name.clone(),
        });
        let attempt = super::attempt(
            state, provider, &model, body, context, deadline, trace, attempts,
        )
        .await;
        if cooldown {
            state.cooldown.record(&provider.id, &model, &attempt);
        }
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
                if let Some(binding) = binding {
                    return Ok(super::affinity::remember_on_success(
                        &state.affinity,
                        binding,
                        super::affinity::Target::for_connection(&provider.id, &model),
                        body,
                        response,
                    ));
                }
                return Ok(response);
            }
            Err(AttemptError::Retryable | AttemptError::RetryableStatus(..)) => {
                if let Some(binding) = &binding {
                    state.affinity.forget_target(
                        binding,
                        &super::affinity::Target::for_connection(&provider.id, &model),
                    );
                }
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
