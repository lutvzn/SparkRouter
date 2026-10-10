pub mod chat;
pub(crate) mod media;
pub mod models;

use crate::app::AppState;
use axum::{
    Router,
    routing::{get, post},
};

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/chat/completions", post(chat::complete))
        .route("/systemone", post(chat::systemone))
        .route("/models", get(models::list))
        .merge(media::router())
}
