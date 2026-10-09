//! Native SystemOne evaluation passthrough; no chat translation or SSE.
use axum::response::Response;
use serde_json::Value;

use super::{
    AttemptError, check_status, json_response, read_bounded, replace_model, request,
    transport_error,
};
use crate::{
    crypto::Crypto,
    db::{connection_models::StoredModel, providers::Provider},
    error::AppError,
};

pub async fn call(
    client: &reqwest::Client,
    crypto: &Crypto,
    provider: &Provider,
    model: &str,
    body: &Value,
) -> Result<Response, AttemptError> {
    let body = replace_model(body, model)?;
    let response = request(client, crypto, provider, reqwest::Method::POST, "systemone")?
        .json(&body)
        .send()
        .await
        .map_err(transport_error)?;
    check_status(&response)?;
    let status = response.status();
    let bytes = read_bounded(response).await?;
    let value: Value = serde_json::from_slice(&bytes).map_err(|_| AttemptError::Retryable)?;
    if value.get("model").and_then(Value::as_str).is_none()
        || value.get("answers").and_then(Value::as_object).is_none()
        || value.get("error").is_some_and(|error| !error.is_null())
    {
        return Err(AttemptError::Retryable);
    }
    Ok(json_response(status, bytes))
}

pub(super) fn parse_models(body: &Value) -> Result<Vec<StoredModel>, AppError> {
    let items = body
        .get("models")
        .and_then(Value::as_array)
        .ok_or_else(|| AppError::InvalidRequest("provider did not return a model list".into()))?;
    Ok(items
        .iter()
        .filter_map(|item| {
            let id = item.get("name")?.as_str()?.trim();
            if id.is_empty() || id.chars().count() > 256 || id.chars().any(char::is_control) {
                return None;
            }
            Some(super::model_card(item, id))
        })
        .collect())
}
