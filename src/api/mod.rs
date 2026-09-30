pub mod chat;
pub mod models;

use crate::app::AppState;
use axum::{
    Router,
    routing::{get, post},
};

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/chat/completions", post(chat::complete))
        .route("/models", get(models::list))
}
