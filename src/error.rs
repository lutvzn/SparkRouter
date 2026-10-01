use axum::{
    Json,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde_json::json;

#[derive(Debug, thiserror::Error)]
pub enum AppError {
    #[error("database operation failed: {0}")]
    Database(#[from] sqlx::Error),
    #[error("migration failed: {0}")]
    Migration(#[from] sqlx::migrate::MigrateError),
    #[error("configuration: {0}")]
    Configuration(String),
    #[error("credential encryption or decryption failed")]
    Encryption,
    #[error("provider request failed")]
    Provider,
    #[error("all available targets failed")]
    Routing,
    // Do not retain reqwest errors: their URLs can contain sensitive query values.
    #[error("HTTP client operation failed")]
    HttpClient,
    #[error("template rendering failed: {0}")]
    Template(#[from] askama::Error),
    #[error("{0}")]
    InvalidRequest(String),
    #[error("request body rejected (HTTP {0})")]
    RequestBody(StatusCode),
    #[error("{0}")]
    NotFound(String),
    #[error("gateway API authentication required")]
    Unauthorized,
    #[error("upstream rejected the request (HTTP {0})")]
    Upstream(StatusCode),
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let (status, message, kind) = match &self {
            Self::InvalidRequest(message) => (
                StatusCode::BAD_REQUEST,
                message.clone(),
                "invalid_request_error",
            ),
            Self::RequestBody(status) => (
                *status,
                match *status {
                    StatusCode::PAYLOAD_TOO_LARGE => "Request body exceeds the 4 MiB limit",
                    StatusCode::UNSUPPORTED_MEDIA_TYPE => "Content-Type must be application/json",
                    _ => "Request body must contain valid JSON",
                }
                .into(),
                "invalid_request_error",
            ),
            Self::NotFound(message) => (StatusCode::NOT_FOUND, message.clone(), "not_found_error"),
            Self::Unauthorized => (
                StatusCode::UNAUTHORIZED,
                self.to_string(),
                "authentication_error",
            ),
            Self::Upstream(status) => (
                if status.is_client_error() || status.is_server_error() {
                    *status
                } else {
                    StatusCode::BAD_GATEWAY
                },
                self.to_string(),
                "upstream_error",
            ),
            Self::Routing | Self::Provider | Self::HttpClient => (
                StatusCode::BAD_GATEWAY,
                "No upstream provider could complete the request".into(),
                "upstream_error",
            ),
            _ => (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Internal server error".into(),
                "server_error",
            ),
        };
        if status.is_server_error() {
            tracing::error!(error = %self, "request failed");
        }
        (
            status,
            Json(json!({"error": {"message": message, "type": kind, "code": status.as_u16()}})),
        )
            .into_response()
    }
}
