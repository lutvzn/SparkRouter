use crate::{app::AppState, error::AppError, routing};
use axum::{
    Extension, Json,
    extract::{State, rejection::JsonRejection},
    response::{IntoResponse, Response},
};
use serde_json::Value;

pub(crate) async fn complete(
    State(state): State<AppState>,
    Extension(auth): Extension<crate::app::Authorization>,
    headers: axum::http::HeaderMap,
    payload: Result<Json<Value>, JsonRejection>,
) -> Result<Response, AppError> {
    execute(state, auth, headers, payload, false).await
}

pub(crate) async fn systemone(
    State(state): State<AppState>,
    Extension(auth): Extension<crate::app::Authorization>,
    headers: axum::http::HeaderMap,
    payload: Result<Json<Value>, JsonRejection>,
) -> Result<Response, AppError> {
    execute(state, auth, headers, payload, true).await
}

async fn execute(
    state: AppState,
    auth: crate::app::Authorization,
    headers: axum::http::HeaderMap,
    payload: Result<Json<Value>, JsonRejection>,
    systemone: bool,
) -> Result<Response, AppError> {
    let started = std::time::Instant::now();
    let Json(body) = payload.map_err(|error| AppError::RequestBody(error.status()))?;
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
    if systemone {
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
    if !auth.access.allows(model) {
        return Err(AppError::Forbidden);
    }
    // Bound the entire pre-commit fallback phase as well as individual attempts.
    // A committed streaming body instead uses the client's idle read timeout.
    // `combo/` is reserved. A bare prefix is not a combo name and not a direct ID.
    if model == "combo/" {
        return Err(AppError::InvalidRequest("Use model: combo/<name>".into()));
    }
    let context = routing::affinity::RequestContext::new(auth.affinity_scope(&state), &headers);
    let context = if systemone {
        context.for_systemone()
    } else {
        context
    };
    let permit = auth.admit(&state)?;
    let capture = crate::history_payload::Capture::start(&state, &body).await;
    let mut attempts = Vec::new();
    let result = routing::route_with_trace(&state, model, &body, &context, &mut attempts).await;
    let mut response = result.unwrap_or_else(IntoResponse::into_response);
    response.extensions_mut().insert(capture);
    let response = crate::history::record_response(
        &state,
        "api",
        model,
        body.get("stream").and_then(Value::as_bool).unwrap_or(false),
        started,
        &attempts,
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
