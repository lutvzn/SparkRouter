pub mod chat;
pub(crate) mod media;
pub mod models;

use crate::{app::AppState, error::AppError};
use axum::{
    Router,
    extract::rejection::BytesRejection,
    http::{HeaderMap, StatusCode, header},
    routing::{get, post},
};
use bytes::Bytes;
use serde_json::Value;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/chat/completions", post(chat::complete))
        .route("/systemone", post(chat::systemone))
        .route("/models", get(models::list))
        .merge(media::router())
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
