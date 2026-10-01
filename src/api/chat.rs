use crate::{
    app::AppState,
    error::AppError,
    routing::{combo, direct},
};
use axum::{
    Json,
    extract::{State, rejection::JsonRejection},
    response::Response,
};
use serde_json::Value;

pub async fn complete(
    State(state): State<AppState>,
    payload: Result<Json<Value>, JsonRejection>,
) -> Result<Response, AppError> {
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
    // Bound the entire pre-commit fallback phase as well as individual attempts.
    // A committed streaming body instead uses the client's idle read timeout.
    // `combo/` is reserved. A bare prefix is not a combo name and not a direct ID.
    let deadline = std::time::Duration::from_secs(180);
    let routed = if let Some(name) = model.strip_prefix("combo/") {
        if name.is_empty() {
            return Err(AppError::InvalidRequest("Use model: combo/<name>".into()));
        }
        tokio::time::timeout(deadline, combo::route(&state, name, &body)).await
    } else {
        tokio::time::timeout(deadline, direct::route(&state, model, &body)).await
    };
    routed.map_err(|_| AppError::Routing)?
}
