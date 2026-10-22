pub mod chat;
pub(crate) mod media;
pub mod models;
mod responses;

use crate::{app::AppState, error::AppError};
use axum::{
    Router,
    extract::{Request, rejection::BytesRejection},
    http::{HeaderMap, StatusCode, header},
    middleware::Next,
    response::Response,
    routing::{get, post},
};
use bytes::Bytes;
use serde_json::Value;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/chat/completions", post(chat::complete))
        .route("/systemone", post(chat::systemone))
        .route("/responses", post(chat::responses))
        .route(
            "/responses/{id}",
            get(responses::retrieve).delete(responses::delete),
        )
        .route("/responses/{id}/cancel", post(responses::cancel))
        .route("/responses/{id}/input_items", get(responses::input_items))
        .route("/embeddings", post(chat::embeddings))
        .route("/models", get(models::list))
        .merge(media::router())
}

/// Anthropic's native API, whose errors [`anthropic_errors`] reshapes.
pub fn anthropic_router() -> Router<AppState> {
    Router::new().route("/messages", post(chat::messages))
}

/// Anthropic clients read errors as `{"type":"error","error":{...}}`, so the
/// gateway's own errors on its routes, authentication included, are
/// reshaped. Status codes, headers, and the sanitized message are kept.
pub(crate) async fn anthropic_errors(request: Request, next: Next) -> Response {
    let response = next.run(request).await;
    let status = response.status();
    if !(status.is_client_error() || status.is_server_error()) {
        return response;
    }
    let (mut parts, body) = response.into_parts();
    let Ok(bytes) = axum::body::to_bytes(body, 64 * 1024).await else {
        return Response::from_parts(parts, axum::body::Body::empty());
    };
    let message = serde_json::from_slice::<Value>(&bytes)
        .ok()
        .and_then(|value| value.pointer("/error/message")?.as_str().map(str::to_owned))
        .unwrap_or_else(|| "Request failed".into());
    let kind = match status.as_u16() {
        400 | 405 | 415 | 422 => "invalid_request_error",
        401 => "authentication_error",
        403 => "permission_error",
        404 => "not_found_error",
        413 => "request_too_large",
        429 => "rate_limit_error",
        529 => "overloaded_error",
        _ => "api_error",
    };
    let body = serde_json::json!({"type":"error", "error":{"type":kind, "message":message}});
    parts.headers.remove(header::CONTENT_LENGTH);
    parts.headers.insert(
        header::CONTENT_TYPE,
        header::HeaderValue::from_static("application/json"),
    );
    Response::from_parts(parts, axum::body::Body::from(body.to_string()))
}

/// The checks and rejection statuses of `axum::Json<Value>`, without its
/// `serde_path_to_error` wrapper, which copies every object key it parses.
pub(crate) fn json_body(
    headers: &HeaderMap,
    body: Result<Bytes, BytesRejection>,
) -> Result<Value, AppError> {
    let json = headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<mime::Mime>().ok())
        .is_some_and(|mime| {
            mime.type_() == "application"
                && (mime.subtype() == "json" || mime.suffix().is_some_and(|name| name == "json"))
        });
    if !json {
        return Err(AppError::RequestBody(StatusCode::UNSUPPORTED_MEDIA_TYPE));
    }
    let body = body.map_err(|error| AppError::RequestBody(error.status()))?;
    serde_json::from_slice(&body).map_err(|error| {
        AppError::RequestBody(match error.classify() {
            serde_json::error::Category::Data => StatusCode::UNPROCESSABLE_ENTITY,
            _ => StatusCode::BAD_REQUEST,
        })
    })
}
