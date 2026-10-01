//! Public local probes. Readiness checks SQLite, never billable providers.
use crate::app::AppState;
use axum::{
    Json, Router,
    extract::State,
    http::{StatusCode, header},
    response::{IntoResponse, Response},
    routing::get,
};
use serde_json::json;
use std::time::Duration;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/healthz", get(health))
        .route("/readyz", get(ready))
}

fn response(status: StatusCode, message: &'static str) -> Response {
    (
        status,
        [(header::CACHE_CONTROL, "no-store")],
        Json(json!({"status":message})),
    )
        .into_response()
}

async fn health() -> Response {
    response(StatusCode::OK, "ok")
}

async fn ready(State(state): State<AppState>) -> Response {
    match tokio::time::timeout(
        Duration::from_secs(2),
        sqlx::query_scalar::<_, i64>("SELECT 1").fetch_one(&state.db),
    )
    .await
    {
        Ok(Ok(1)) => response(StatusCode::OK, "ready"),
        _ => response(StatusCode::SERVICE_UNAVAILABLE, "unavailable"),
    }
}
