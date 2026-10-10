//! Shared media admission and routing. Uploads stay in memory, bounded and replayable.
use crate::{
    app::{AppState, Authorization},
    error::AppError,
    providers::media::{BinaryStream, Kind, Upload, UploadPart},
    routing,
};
use axum::{
    Extension, Json, Router,
    extract::{
        DefaultBodyLimit, Multipart, State, multipart::MultipartRejection, rejection::JsonRejection,
    },
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::post,
};
use serde_json::Value;
use std::sync::Arc;

pub(crate) fn router() -> Router<AppState> {
    Router::new()
        .route("/images/generations", post(images))
        .route("/videos/generations", post(videos))
        .route("/music/generations", post(music))
        .route("/audio/speech", post(speech))
        .merge(
            Router::new()
                .route("/audio/transcriptions", post(transcriptions))
                .route("/audio/translations", post(translations))
                .route("/images/edits", post(image_edits))
                .layer(DefaultBodyLimit::max(32 * 1024 * 1024)),
        )
}
macro_rules! json_handler {
    ($name:ident, $kind:expr) => {
        async fn $name(
            State(state): State<AppState>,
            Extension(auth): Extension<Authorization>,
            headers: HeaderMap,
            payload: Result<Json<Value>, JsonRejection>,
        ) -> Result<Response, AppError> {
            let Json(body) = payload.map_err(|e| AppError::RequestBody(e.status()))?;
            execute(state, auth, headers, body, $kind, None, false).await
        }
    };
}
json_handler!(images, Kind::Image);
json_handler!(videos, Kind::Video);
json_handler!(music, Kind::Music);
json_handler!(speech, Kind::Speech);
macro_rules! upload_handler {
    ($name:ident, $kind:expr) => {
        async fn $name(
            State(state): State<AppState>,
            Extension(auth): Extension<Authorization>,
            headers: HeaderMap,
            payload: Result<Multipart, MultipartRejection>,
        ) -> Result<Response, AppError> {
            let multipart = payload.map_err(|e| AppError::RequestBody(e.status()))?;
            let (body, upload) = parse_upload(multipart, $kind).await?;
            execute(state, auth, headers, body, $kind, Some(upload), false).await
        }
    };
}
upload_handler!(transcriptions, Kind::Transcription);
upload_handler!(translations, Kind::Translation);
upload_handler!(image_edits, Kind::ImageEdit);

fn invalid(message: &str) -> AppError {
    AppError::InvalidRequest(message.into())
}
pub(crate) async fn parse_upload(
    mut multipart: Multipart,
    kind: Kind,
) -> Result<(Value, Upload), AppError> {
    let mut parts = Vec::new();
    let mut body = serde_json::Map::new();
    let mut files = 0;
    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|e| AppError::RequestBody(e.status()))?
    {
        if parts.len() >= 64 {
            return Err(invalid("Too many multipart fields"));
        }
        let name = field
            .name()
            .filter(|name| {
                !name.is_empty() && name.len() <= 100 && !name.chars().any(char::is_control)
            })
            .ok_or_else(|| invalid("Multipart field name required"))?
            .to_owned();
        let filename = field.file_name().map(str::to_owned);
        let content_type = field.content_type().map(str::to_owned);
        if filename
            .as_ref()
            .is_some_and(|s| s.len() > 256 || s.chars().any(char::is_control))
        {
            return Err(invalid("Invalid upload filename"));
        }
        let bytes = field
            .bytes()
            .await
            .map_err(|e| AppError::RequestBody(e.status()))?;
        if filename.is_some() {
            let allowed = if kind == Kind::ImageEdit {
                matches!(name.as_str(), "image" | "image[]" | "mask")
            } else {
                name == "file"
            };
            if !allowed || bytes.is_empty() {
                return Err(invalid("Unexpected or empty file field"));
            }
            files += usize::from(name != "mask");
            if kind != Kind::ImageEdit && files > 1 {
                return Err(invalid("Exactly one audio file is required"));
            }
        } else {
            if bytes.len() > 64 * 1024 {
                return Err(AppError::RequestBody(StatusCode::PAYLOAD_TOO_LARGE));
            }
            let text = std::str::from_utf8(&bytes)
                .map_err(|_| invalid("Multipart text fields must be UTF-8"))?;
            if name == "model" && body.contains_key("model") {
                return Err(invalid("Exactly one model field is required"));
            }
            body.insert(name.clone(), Value::String(text.to_owned()));
        }
        parts.push(UploadPart {
            name,
            filename,
            content_type,
            bytes,
        });
    }
    if files == 0 {
        return Err(invalid("A file upload is required"));
    }
    Ok((Value::Object(body), Arc::new(parts)))
}

pub(crate) async fn execute(
    state: AppState,
    auth: Authorization,
    headers: HeaderMap,
    body: Value,
    kind: Kind,
    upload: Option<Upload>,
    dashboard: bool,
) -> Result<Response, AppError> {
    let started = std::time::Instant::now();
    if !body.is_object() {
        return Err(invalid("Request must be an object"));
    }
    let model = body
        .get("model")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty() && s.chars().count() <= 321 && !s.chars().any(char::is_control))
        .ok_or_else(|| {
            invalid("model must be combo/<name> or <provider-slug>/<upstream-model-id>")
        })?;
    if model == "combo/" {
        return Err(invalid("Use model: combo/<name>"));
    }
    if body
        .get("stream")
        .is_some_and(|v| v != false && (upload.is_none() || v != "false"))
    {
        return Err(invalid(
            "Media SSE streaming is not supported; speech returns binary audio",
        ));
    }
    let key = match kind {
        Kind::Speech => Some("input"),
        Kind::Image | Kind::ImageEdit | Kind::Video => Some("prompt"),
        _ => None,
    };
    if let Some(key) = key
        && !body
            .get(key)
            .and_then(Value::as_str)
            .is_some_and(|s| !s.trim().is_empty())
    {
        return Err(invalid(&format!("{key} must be a nonempty string")));
    }
    if kind == Kind::Music
        && ![
            "prompt",
            "lyrics",
            "tags",
            "audio_url",
            "audio_base64",
            "cover_feature_id",
        ]
        .iter()
        .any(|key| {
            body.get(*key)
                .and_then(Value::as_str)
                .is_some_and(|s| !s.trim().is_empty())
        })
        && !body.get("composition_plan").is_some_and(Value::is_object)
    {
        return Err(invalid(
            "Music requires a prompt, lyrics, tags, reference audio, or composition plan",
        ));
    }
    if !auth.access.allows(model) {
        return Err(AppError::Forbidden);
    }
    let context = routing::affinity::RequestContext::new(auth.affinity_scope(&state), &headers)
        .for_media(kind, upload);
    routing::validate_combo_category(&state, model, context.category()).await?;
    let permit = auth.admit(&state)?;
    // Media is metadata-only even in Extended mode: no audio, images, URLs or prompts in SQLite.
    let mut capture = crate::history_payload::Capture::start(&state, &Value::Null).await;
    if capture.mode == crate::db::settings::LoggingMode::Extended {
        capture.mode = crate::db::settings::LoggingMode::Basic;
        capture.request.clear();
    }
    let mut attempts = Vec::new();
    let result = routing::route_with_trace(&state, model, &body, &context, &mut attempts).await;
    let mut response = result.unwrap_or_else(IntoResponse::into_response);
    if dashboard {
        // Keep binary output streaming rather than wrapping it in base64 JSON.
        // Bound diagnostics below typical header limits, dropping whole tail
        // attempts only. This header is never added to the public API.
        use base64::Engine as _;
        let mut shown = attempts.len();
        loop {
            let bytes = serde_json::to_vec(&serde_json::json!({
                "attempts": &attempts[..shown], "truncated": shown < attempts.len(),
            }))
            .map_err(|_| AppError::Provider)?;
            if bytes.len() <= 4096 {
                let encoded = base64::engine::general_purpose::STANDARD.encode(bytes);
                let value =
                    axum::http::HeaderValue::from_str(&encoded).map_err(|_| AppError::Provider)?;
                response
                    .headers_mut()
                    .insert("x-sparkrouter-attempts", value);
                break;
            }
            shown -= 1;
        }
    }
    let streaming = response.extensions().get::<BinaryStream>().is_some();
    response.extensions_mut().insert(capture);
    let response = crate::history::record_response(
        &state,
        if dashboard { "test" } else { "api" },
        model,
        streaming,
        started,
        &attempts,
        response,
    )
    .await;
    if streaming && let Some(permit) = permit {
        return Ok(permit.wrap(response));
    }
    Ok(response)
}
