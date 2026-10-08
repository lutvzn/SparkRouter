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
    payload: Result<Json<Value>, JsonRejection>,
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
    if !auth.access.allows(model) {
        return Err(AppError::Forbidden);
    }
    // Bound the entire pre-commit fallback phase as well as individual attempts.
    // A committed streaming body instead uses the client's idle read timeout.
    // `combo/` is reserved. A bare prefix is not a combo name and not a direct ID.
    if model == "combo/" {
        return Err(AppError::InvalidRequest("Use model: combo/<name>".into()));
    }
    let permit = auth.admit(&state)?;
    let capture = crate::history_payload::Capture::start(&state, &body).await;
    let mut attempts = Vec::new();
    let result = routing::route_with_trace(&state, model, &body, &mut attempts).await;
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
