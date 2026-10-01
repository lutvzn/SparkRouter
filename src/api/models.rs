use crate::{
    app::AppState,
    db::{combos, connection_models},
    error::AppError,
};
use axum::{Extension, Json, extract::State};
use serde_json::{Value, json};

pub async fn list(
    State(state): State<AppState>,
    Extension(access): Extension<crate::db::api_keys::KeyAccess>,
) -> Result<Json<Value>, AppError> {
    let mut models: Vec<Value> = combos::list(&state.db)
        .await?
        .into_iter()
        .map(|combo| {
            json!({"id":format!("combo/{}",combo.name),"object":"model","created":0,"owned_by":"sparkrouter"})
        })
        .collect();
    for id in connection_models::routable_model_ids(&state.db).await? {
        models.push(json!({"id":id,"object":"model","created":0,"owned_by":"sparkrouter"}));
    }
    models.retain(|model| model["id"].as_str().is_some_and(|id| access.allows(id)));
    Ok(Json(json!({"object":"list","data":models})))
}
