use askama::Template;
use axum::{
    Json,
    body::to_bytes,
    extract::{State, rejection::JsonRejection},
    http::HeaderMap,
    response::{Html, IntoResponse, Response},
};
use serde_json::{Value, json};

use crate::{
    app::{self, AppState},
    db::{api_keys, combos},
    error::AppError,
    routing,
};

#[derive(Template)]
#[template(path = "test.html")]
struct TestTemplate {
    app_name: &'static str,
    csrf_token: String,
    combos: Vec<combos::Combo>,
    auth_required: bool,
}

pub async fn page(State(state): State<AppState>) -> Result<Html<String>, AppError> {
    super::render(TestTemplate {
        app_name: crate::APP_NAME,
        csrf_token: state.csrf_token.to_string(),
        combos: combos::list(&state.db).await?,
        auth_required: state.api_key.is_some() || api_keys::any(&state.db).await?,
    })
}

/// Non-streaming chat tests use the real combo loop and its shared strategy
/// state. Gateway authentication still applies; dashboard access is not a key.
pub async fn chat(
    State(state): State<AppState>,
    headers: HeaderMap,
    payload: Result<Json<Value>, JsonRejection>,
) -> Result<Response, AppError> {
    let started = std::time::Instant::now();
    let Json(mut body) = payload.map_err(|error| AppError::RequestBody(error.status()))?;
    super::check_csrf(
        &state,
        body.get("_csrf").and_then(Value::as_str).unwrap_or(""),
    )?;
    let access = app::authorize(&state, &headers).await?;
    let name = body
        .get("model")
        .and_then(Value::as_str)
        .and_then(|model| model.strip_prefix("combo/"))
        .filter(|name| !name.is_empty())
        .ok_or_else(|| AppError::InvalidRequest("Select a combo to test".into()))?
        .to_owned();
    if !access.access.allows(&format!("combo/{name}")) {
        return Err(AppError::Forbidden);
    }
    let messages = body
        .get("messages")
        .and_then(Value::as_array)
        .filter(|messages| !messages.is_empty())
        .ok_or_else(|| AppError::InvalidRequest("messages must be a nonempty array".into()))?;
    if messages.iter().any(|message| {
        !matches!(
            message.get("role").and_then(Value::as_str),
            Some("user" | "assistant")
        ) || message
            .get("content")
            .and_then(Value::as_str)
            .is_none_or(|content| content.trim().is_empty())
    }) {
        return Err(AppError::InvalidRequest(
            "Test messages need a user or assistant role and nonempty text".into(),
        ));
    }
    let object = body
        .as_object_mut()
        .ok_or_else(|| AppError::InvalidRequest("Request must be a JSON object".into()))?;
    object.remove("_csrf");
    object.insert("stream".into(), Value::Bool(false));
    let _permit = access.admit(&state)?;
    let capture = crate::history_payload::Capture::start(&state, &body).await;
    let mut attempts = Vec::new();
    let model = format!("combo/{name}");
    let result = routing::route_with_trace(&state, &model, &body, &mut attempts).await;
    let mut response = match result {
        Ok(response) => response,
        Err(error) => error.into_response(),
    };
    response.extensions_mut().insert(capture);
    let response = crate::history::record_response(
        &state, "test", &model, false, started, &attempts, response,
    )
    .await;
    let status = response.status();
    // Adapters already buffer non-streaming responses with the same limit.
    let bytes = to_bytes(response.into_body(), 16 * 1024 * 1024)
        .await
        .map_err(|_| AppError::Provider)?;
    let value: Value = serde_json::from_slice(&bytes).map_err(|_| AppError::Provider)?;
    Ok((
        status,
        Json(json!({
            "completion": if status.is_success() { Some(&value) } else { None },
            "error": if status.is_success() { None } else { value.get("error") },
            "attempts": attempts,
        })),
    )
        .into_response())
}
