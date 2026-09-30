use crate::{app::AppState, db::combos, error::AppError};
use axum::{Json, extract::State};
use serde_json::{Value, json};

pub async fn list(State(state): State<AppState>) -> Result<Json<Value>, AppError> {
    let models: Vec<Value> = combos::list(&state.db).await?.into_iter()
        .map(|combo| json!({"id":format!("combo/{}",combo.name),"object":"model","created":0,"owned_by":"sparkrouter"}))
        .collect();
    Ok(Json(json!({"object":"list","data":models})))
}
