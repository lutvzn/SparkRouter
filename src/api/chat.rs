use crate::{
    app::AppState,
    error::AppError,
    routing::{combo, direct},
};
use axum::{
    Extension, Json,
    extract::{State, rejection::JsonRejection},
    response::{IntoResponse, Response},
};
use serde_json::Value;

pub async fn complete(
    State(state): State<AppState>,
    Extension(access): Extension<crate::db::api_keys::KeyAccess>,
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
    if !access.allows(model) {
        return Err(AppError::Forbidden);
    }
    // Bound the entire pre-commit fallback phase as well as individual attempts.
    // A committed streaming body instead uses the client's idle read timeout.
    // `combo/` is reserved. A bare prefix is not a combo name and not a direct ID.
    let deadline = std::time::Duration::from_secs(180);
    let mut attempts = Vec::new();
    let routed = if let Some(name) = model.strip_prefix("combo/") {
        if name.is_empty() {
            return Err(AppError::InvalidRequest("Use model: combo/<name>".into()));
        }
        tokio::time::timeout(
            deadline,
            combo::route_with_trace(&state, name, &body, &mut attempts),
        )
        .await
    } else {
        tokio::time::timeout(
            deadline,
            direct::route_with_trace(&state, model, &body, &mut attempts),
        )
        .await
    };
    let result = routed.unwrap_or_else(|_| {
        if let Some(trace) = attempts
            .last_mut()
            .filter(|trace| trace.outcome == "in_progress")
        {
            trace.finish("timed_out");
        }
        Err(AppError::Routing)
    });
    let response = result.unwrap_or_else(IntoResponse::into_response);
    Ok(crate::history::record_response(
        &state,
        "api",
        model,
        body.get("stream").and_then(Value::as_bool).unwrap_or(false),
        started,
        &attempts,
        response,
    )
    .await)
}
