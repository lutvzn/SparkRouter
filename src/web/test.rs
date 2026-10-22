use askama::Template;
use axum::{
    Json,
    body::to_bytes,
    extract::{Multipart, Path, State, multipart::MultipartRejection, rejection::JsonRejection},
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
    execute(state, headers, payload, false).await
}

/// Native evaluation tests share chat's authorization, admission and trace wrapper.
pub async fn systemone(
    State(state): State<AppState>,
    headers: HeaderMap,
    payload: Result<Json<Value>, JsonRejection>,
) -> Result<Response, AppError> {
    execute(state, headers, payload, true).await
}

fn media_kind(mode: &str, upload: bool) -> Result<crate::providers::media::Kind, AppError> {
    use crate::providers::media::Kind;
    match (mode, upload) {
        ("image", false) => Ok(Kind::Image),
        ("video", false) => Ok(Kind::Video),
        ("music", false) => Ok(Kind::Music),
        ("speech", false) => Ok(Kind::Speech),
        ("image-edit", true) => Ok(Kind::ImageEdit),
        ("transcription", true) => Ok(Kind::Transcription),
        ("translation", true) => Ok(Kind::Translation),
        _ => Err(AppError::InvalidRequest(
            "Select a supported media mode".into(),
        )),
    }
}

fn check_media_model(body: &mut Value) -> Result<(), AppError> {
    if !body
        .get("model")
        .and_then(Value::as_str)
        .is_some_and(|model| {
            model
                .trim()
                .strip_prefix("combo/")
                .is_some_and(|name| !name.is_empty())
        })
    {
        return Err(AppError::InvalidRequest("Select a combo to test".into()));
    }
    if let Some(object) = body.as_object_mut() {
        object.remove("_csrf");
    }
    Ok(())
}

// Media tests use the public API's validation/admission/history/stream wrappers.
// CSRF lives in a header, never in upstream JSON or multipart fields.
pub async fn media(
    State(state): State<AppState>,
    Path(mode): Path<String>,
    headers: HeaderMap,
    payload: Result<Json<Value>, JsonRejection>,
) -> Result<Response, AppError> {
    super::check_csrf(
        &state,
        headers
            .get("x-sparkrouter-csrf")
            .and_then(|v| v.to_str().ok())
            .unwrap_or(""),
    )?;
    let auth = app::authorize(&state, &headers).await?;
    let kind = media_kind(&mode, false)?;
    let Json(mut body) = payload.map_err(|error| AppError::RequestBody(error.status()))?;
    check_media_model(&mut body)?;
    crate::api::media::execute(state, auth, headers, body, kind, None, true).await
}

pub async fn media_upload(
    State(state): State<AppState>,
    Path(mode): Path<String>,
    headers: HeaderMap,
    payload: Result<Multipart, MultipartRejection>,
) -> Result<Response, AppError> {
    super::check_csrf(
        &state,
        headers
            .get("x-sparkrouter-csrf")
            .and_then(|v| v.to_str().ok())
            .unwrap_or(""),
    )?;
    let auth = app::authorize(&state, &headers).await?;
    let kind = media_kind(&mode, true)?;
    let multipart = payload.map_err(|error| AppError::RequestBody(error.status()))?;
    let (mut body, upload) = crate::api::media::parse_upload(multipart, kind).await?;
    check_media_model(&mut body)?;
    let upload = std::sync::Arc::new(
        upload
            .iter()
            .filter(|part| part.name != "_csrf")
            .cloned()
            .collect(),
    );
    crate::api::media::execute(state, auth, headers, body, kind, Some(upload), true).await
}

async fn execute(
    state: AppState,
    headers: HeaderMap,
    payload: Result<Json<Value>, JsonRejection>,
    systemone: bool,
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
    if systemone {
        if !body
            .get("state")
            .is_some_and(|value| value.is_string() || value.is_object() || value.is_array())
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
        if body
            .get("stream")
            .is_some_and(|value| value != &Value::Bool(false))
        {
            return Err(AppError::InvalidRequest(
                "SystemOne does not support streaming".into(),
            ));
        }
    } else {
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
    }
    let object = body
        .as_object_mut()
        .ok_or_else(|| AppError::InvalidRequest("Request must be a JSON object".into()))?;
    object.remove("_csrf");
    if !systemone {
        object.insert("stream".into(), Value::Bool(false));
    }
    let context = routing::affinity::RequestContext::new(access.affinity_scope(&state), &headers);
    let context = if systemone {
        context.for_endpoint(crate::routing::affinity::Endpoint::SystemOne)
    } else {
        context
    };
    routing::validate_combo_category(&state, &format!("combo/{name}"), context.category()).await?;
    let admission = access.admit(&state).await?;
    let _permit = admission.permit;
    let capture = crate::history_payload::Capture::start(&state, &body).await;
    let mut attempts = Vec::new();
    let model = format!("combo/{name}");
    let result = routing::route_with_trace(&state, &model, &body, &context, &mut attempts).await;
    let mut response = match result {
        Ok(response) => response,
        Err(error) => error.into_response(),
    };
    response.extensions_mut().insert(capture);
    let meter = crate::history::Meter {
        source: "test",
        endpoint: context.endpoint.label(),
        charge: admission.charge,
    };
    let response =
        crate::history::record_response(&state, &model, false, started, &attempts, meter, response)
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
