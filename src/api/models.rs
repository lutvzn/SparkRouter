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
    let combo_details = connection_models::first_combo_model_details(&state.db).await?;
    let mut models: Vec<Value> = combos::list(&state.db)
        .await?
        .into_iter()
        .map(|combo| {
            let mut entry = model_entry(
                format!("combo/{}", combo.name),
                combo_details.get(&combo.id),
            );
            entry["category"] = json!(combo.category.as_str());
            entry
        })
        .collect();
    for (id, details) in connection_models::public_model_details(&state.db).await? {
        models.push(model_entry(id, Some(&details)));
    }
    models.retain(|model| model["id"].as_str().is_some_and(|id| access.allows(id)));
    Ok(Json(json!({"object":"list","data":models})))
}

fn model_entry(id: String, details: Option<&connection_models::StoredModel>) -> Value {
    let mut model = json!({"id":id,"object":"model","created":0,"owned_by":"sparkrouter"});
    if let Some(details) = details {
        let (context, output) = details.token_limits();
        let mut top_provider = serde_json::Map::new();
        if let Some(context) = context {
            model["context_length"] = json!(context);
            top_provider.insert("context_length".into(), json!(context));
        }
        if let Some(output) = output {
            model["max_output_tokens"] = json!(output);
            // OpenRouter-style discovery used by clients displaying output limits.
            top_provider.insert("max_completion_tokens".into(), json!(output));
        }
        if !top_provider.is_empty() {
            model["top_provider"] = Value::Object(top_provider);
        }
    }
    model
}
