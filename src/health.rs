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

pub(crate) async fn database_ready(pool: &sqlx::SqlitePool) -> bool {
    matches!(
        tokio::time::timeout(
            Duration::from_secs(2),
            sqlx::query_scalar::<_, i64>("SELECT 1").fetch_one(pool),
        )
        .await,
        Ok(Ok(1))
    )
}

async fn ready(State(state): State<AppState>) -> Response {
    if database_ready(&state.db).await {
        response(StatusCode::OK, "ready")
    } else {
        response(StatusCode::SERVICE_UNAVAILABLE, "unavailable")
    }
}
