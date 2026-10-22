use crate::{
    app::AppState,
    error::AppError,
    routing::{self, affinity::Endpoint},
};
use axum::{
    Extension,
    extract::{State, rejection::BytesRejection},
    response::{IntoResponse, Response},
};
use bytes::Bytes;
use serde_json::Value;

pub(crate) async fn complete(
    State(state): State<AppState>,
    Extension(auth): Extension<crate::app::Authorization>,
    headers: axum::http::HeaderMap,
    payload: Result<Bytes, BytesRejection>,
) -> Result<Response, AppError> {
    execute(state, auth, headers, payload, Endpoint::Chat).await
}

pub(crate) async fn systemone(
    State(state): State<AppState>,
    Extension(auth): Extension<crate::app::Authorization>,
    headers: axum::http::HeaderMap,
    payload: Result<Bytes, BytesRejection>,
) -> Result<Response, AppError> {
    execute(state, auth, headers, payload, Endpoint::SystemOne).await
}

pub(crate) async fn responses(
    State(state): State<AppState>,
    Extension(auth): Extension<crate::app::Authorization>,
    headers: axum::http::HeaderMap,
    payload: Result<Bytes, BytesRejection>,
) -> Result<Response, AppError> {
    execute(state, auth, headers, payload, Endpoint::Responses).await
}

pub(crate) async fn embeddings(
    State(state): State<AppState>,
    Extension(auth): Extension<crate::app::Authorization>,
    headers: axum::http::HeaderMap,
    payload: Result<Bytes, BytesRejection>,
) -> Result<Response, AppError> {
    execute(state, auth, headers, payload, Endpoint::Embeddings).await
}

pub(crate) async fn messages(
    State(state): State<AppState>,
    Extension(auth): Extension<crate::app::Authorization>,
    headers: axum::http::HeaderMap,
    payload: Result<Bytes, BytesRejection>,
) -> Result<Response, AppError> {
    execute(state, auth, headers, payload, Endpoint::Messages).await
}

/// The client's Anthropic protocol headers, checked before they go upstream:
/// a dated `anthropic-version` and plain `anthropic-beta` flags, combined
/// into one comma-separated value.
fn anthropic_headers(headers: &axum::http::HeaderMap) -> Result<axum::http::HeaderMap, AppError> {
    let mut forwarded = axum::http::HeaderMap::new();
    if let Some(version) = headers.get("anthropic-version") {
        let dated = version.as_bytes().len() == 10
            && version.as_bytes().iter().enumerate().all(|(index, byte)| {
                if index == 4 || index == 7 {
                    *byte == b'-'
                } else {
                    byte.is_ascii_digit()
                }
            });
        if !dated {
            return Err(AppError::InvalidRequest(
                "anthropic-version must be a date such as 2023-06-01".into(),
            ));
        }
        forwarded.insert("anthropic-version", version.clone());
    }
    let mut flags = Vec::new();
    for value in headers.get_all("anthropic-beta") {
        let value = value.to_str().map_err(|_| {
            AppError::InvalidRequest("anthropic-beta must be comma-separated flags".into())
        })?;
        flags.extend(
            value
                .split(',')
                .map(str::trim)
                .filter(|flag| !flag.is_empty()),
        );
    }
    if flags.len() > 32
        || flags.iter().any(|flag| {
            flag.len() > 64
                || !flag
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
        })
    {
        return Err(AppError::InvalidRequest(
            "anthropic-beta must be at most 32 comma-separated flags of letters, digits, '-', '_' and '.'"
                .into(),
        ));
    }
    if !flags.is_empty() {
        forwarded.insert(
            "anthropic-beta",
            axum::http::HeaderValue::from_str(&flags.join(",")).map_err(|_| AppError::Routing)?,
        );
    }
    Ok(forwarded)
}

async fn execute(
    state: AppState,
    auth: crate::app::Authorization,
    headers: axum::http::HeaderMap,
    payload: Result<Bytes, BytesRejection>,
    endpoint: Endpoint,
) -> Result<Response, AppError> {
    let started = std::time::Instant::now();
    let body = super::json_body(&headers, payload)?;
    if !body.is_object() {
        return Err(AppError::InvalidRequest(
            "Request must be a JSON object".into(),
        ));
    }
    let model = body
        .get("model")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|model| !model.is_empty())
        .ok_or_else(|| {
            AppError::InvalidRequest("model must be combo/<name> or a provider model id".into())
        })?;
    if body.get("stream").is_some_and(|v| !v.is_boolean()) {
        return Err(AppError::InvalidRequest("stream must be a boolean".into()));
    }
    if endpoint == Endpoint::SystemOne {
        if !body
            .get("state")
            .is_some_and(|state| state.is_string() || state.is_object() || state.is_array())
        {
            return Err(AppError::InvalidRequest(
                "state must be a string, object, or array".into(),
            ));
        }
        if body.get("questions").and_then(Value::as_object).is_none() {
            return Err(AppError::InvalidRequest(
                "questions must be an object".into(),
            ));
        }
        if body.get("stream").and_then(Value::as_bool) == Some(true) {
            return Err(AppError::InvalidRequest(
                "SystemOne does not support streaming".into(),
            ));
        }
    }
    if endpoint == Endpoint::Embeddings {
        if body.get("input").is_none_or(Value::is_null) {
            return Err(AppError::InvalidRequest("input is required".into()));
        }
        if body.get("stream").and_then(Value::as_bool) == Some(true) {
            return Err(AppError::InvalidRequest(
                "Embeddings do not support streaming".into(),
            ));
        }
    }
    if endpoint == Endpoint::Messages && body.get("messages").and_then(Value::as_array).is_none() {
        return Err(AppError::InvalidRequest("messages must be an array".into()));
    }
    let upstream_headers = if endpoint == Endpoint::Messages {
        anthropic_headers(&headers)?
    } else {
        axum::http::HeaderMap::new()
    };
    if !auth.access.allows(model) {
        return Err(AppError::Forbidden);
    }
    // Bound the entire pre-commit fallback phase as well as individual attempts.
    // A committed streaming body instead uses the client's idle read timeout.
    // `combo/` is reserved. A bare prefix is not a combo name and not a direct ID.
    if model == "combo/" {
        return Err(AppError::InvalidRequest("Use model: combo/<name>".into()));
    }
    let caller = auth.affinity_scope(&state);
    let mut context =
        routing::affinity::RequestContext::new(caller.clone(), &headers).for_endpoint(endpoint);
    context.upstream_headers = upstream_headers;
    if endpoint == Endpoint::Responses
        && let Some(previous) = body.get("previous_response_id").and_then(Value::as_str)
        && crate::db::stored_responses::valid_id(previous)
    {
        context.preferred_connection =
            crate::db::stored_responses::get(&state.db, previous, &caller)
                .await?
                .map(|stored| stored.provider_id);
    }
    routing::validate_combo_category(&state, model, context.category()).await?;
    let admission = auth.admit(&state).await?;
    let permit = admission.permit;
    let capture = crate::history_payload::Capture::start(&state, &body).await;
    let mut attempts = Vec::new();
    let result = routing::route_with_trace(&state, model, &body, &context, &mut attempts).await;
    let mut response = result.unwrap_or_else(IntoResponse::into_response);
    if let Some(stored) = response
        .extensions_mut()
        .remove::<crate::providers::openai::StoredResponseId>()
        && let Some(selected) = attempts
            .iter()
            .rev()
            .find(|trace| trace.outcome == "selected")
    {
        state.history.remember_response(
            stored.0,
            selected.provider_id.clone(),
            caller,
            model.to_owned(),
        );
    }
    response.extensions_mut().insert(capture);
    let response = crate::history::record_response(
        &state,
        model,
        body.get("stream").and_then(Value::as_bool).unwrap_or(false),
        started,
        &attempts,
        crate::history::Meter {
            source: "api",
            endpoint: endpoint.label(),
            charge: admission.charge,
        },
        response,
    )
    .await;
    if body.get("stream").and_then(Value::as_bool) == Some(true)
        && response.status().is_success()
        && let Some(permit) = permit
    {
        return Ok(permit.wrap(response));
    }
    Ok(response)
}
