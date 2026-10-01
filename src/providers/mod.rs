pub mod anthropic;
pub mod openai;
// Framing decoder private to the provider adapters.
pub(crate) mod sse;

use std::{pin::Pin, str::FromStr};

use axum::{
    body::Body,
    http::{StatusCode, header},
    response::Response,
};
use bytes::Bytes;
use futures_util::{Stream, StreamExt};
use secrecy::{ExposeSecret, SecretString};
use serde_json::Value;

use crate::{
    crypto::Crypto,
    db::{
        connection_models::{ModelField, StoredModel},
        providers::Provider,
    },
    error::AppError,
};

const MAX_RESPONSE_BYTES: usize = 16 * 1024 * 1024;
type ResponseStream = Pin<Box<dyn Stream<Item = Result<Bytes, AttemptError>> + Send>>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Protocol {
    OpenAi,
    Anthropic,
}

impl Protocol {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::OpenAi => "openai",
            Self::Anthropic => "anthropic",
        }
    }
}

impl FromStr for Protocol {
    type Err = AppError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "openai" => Ok(Self::OpenAi),
            "anthropic" => Ok(Self::Anthropic),
            _ => Err(AppError::InvalidRequest(
                "Unsupported provider protocol".into(),
            )),
        }
    }
}

pub fn normalize_base_url(input: &str) -> Result<String, AppError> {
    let invalid = || {
        AppError::InvalidRequest(
            "Provider URL must be an HTTP(S) URL without credentials, query, or fragment".into(),
        )
    };
    let url = reqwest::Url::parse(input.trim()).map_err(|_| invalid())?;
    if !matches!(url.scheme(), "http" | "https")
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(invalid());
    }
    // Local HTTP endpoints and custom paths (including /v1) are intentional.
    Ok(url.as_str().trim_end_matches('/').to_owned())
}

pub fn replace_model(body: &Value, model: &str) -> Result<Value, AppError> {
    let mut body = body
        .as_object()
        .cloned()
        .ok_or_else(|| AppError::InvalidRequest("Request body must be an object".into()))?;
    body.insert("model".into(), Value::String(model.into()));
    Ok(Value::Object(body))
}

#[derive(Debug)]
pub enum AttemptError {
    Retryable,
    Permanent(AppError),
}

impl From<AppError> for AttemptError {
    fn from(error: AppError) -> Self {
        Self::Permanent(error)
    }
}

pub async fn call(
    client: &reqwest::Client,
    crypto: &Crypto,
    provider: &Provider,
    model: &str,
    body: &Value,
) -> Result<Response, AttemptError> {
    match provider.protocol {
        Protocol::OpenAi => openai::call(client, crypto, provider, model, body).await,
        Protocol::Anthropic => anthropic::call(client, crypto, provider, model, body).await,
    }
}

fn request(
    client: &reqwest::Client,
    crypto: &Crypto,
    provider: &Provider,
    method: reqwest::Method,
    endpoint: &str,
) -> Result<reqwest::RequestBuilder, AppError> {
    let base = normalize_base_url(&provider.base_url)?;
    // An empty blob means no credential was configured (local no-auth servers).
    // Non-empty envelopes must always authenticate successfully before use.
    let key = if provider.encrypted_api_key.is_empty() {
        SecretString::from("")
    } else {
        crypto.decrypt_api_key(&provider.id, &provider.encrypted_api_key)?
    };
    let mut request = client.request(method, format!("{base}/{endpoint}"));
    if provider.protocol == Protocol::Anthropic {
        request = request.header("anthropic-version", "2023-06-01");
        if !key.expose_secret().is_empty() {
            let mut value = reqwest::header::HeaderValue::from_str(key.expose_secret())
                .map_err(|_| AppError::Provider)?;
            value.set_sensitive(true);
            request = request.header("x-api-key", value);
        }
    } else if !key.expose_secret().is_empty() {
        request = request.bearer_auth(key.expose_secret());
    }
    Ok(request)
}

const MAX_LISTED_MODELS: usize = 500;
const MAX_MODEL_PAGES: usize = 5;
const MAX_MODEL_LIST_BYTES: usize = 1024 * 1024;

/// Model cards returned by a provider's `/models` endpoint, in upstream order.
pub struct ListedModels {
    pub models: Vec<StoredModel>,
    pub truncated: bool,
}

pub async fn list_models(
    client: &reqwest::Client,
    crypto: &Crypto,
    provider: &Provider,
) -> Result<ListedModels, AppError> {
    let mut models = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let mut after: Option<String> = None;
    let mut truncated = false;
    for _ in 0..MAX_MODEL_PAGES {
        if models.len() >= MAX_LISTED_MODELS {
            truncated = true;
            break;
        }
        let endpoint = match &after {
            Some(cursor) => format!("models?limit=100&after_id={}", query_component(cursor)),
            None => "models".to_owned(),
        };
        let response = request(client, crypto, provider, reqwest::Method::GET, &endpoint)?
            .timeout(std::time::Duration::from_secs(15))
            .send()
            .await
            .map_err(|_| AppError::HttpClient)?;
        if !response.status().is_success() {
            return Err(AppError::Upstream(response.status()));
        }
        let bytes = read_model_body(response).await?;
        let body: Value = serde_json::from_slice(&bytes)
            .map_err(|_| AppError::InvalidRequest("provider did not return a model list".into()))?;
        let (page, next) = parse_model_page(&body)?;
        for model in page {
            if seen.insert(model.id.clone()) {
                models.push(model);
                if models.len() == MAX_LISTED_MODELS {
                    truncated = true;
                    break;
                }
            }
        }
        match next {
            Some(cursor) if !truncated && models.len() < MAX_LISTED_MODELS => after = Some(cursor),
            Some(_) => {
                truncated = true;
                break;
            }
            None => break,
        }
    }
    Ok(ListedModels { models, truncated })
}

/// Reads one OpenAI- or Anthropic-shaped models page. Unknown entries are
/// skipped; a missing `data` array is an error so a success page is not stored
/// as an empty catalog.
pub fn parse_model_page(body: &Value) -> Result<(Vec<StoredModel>, Option<String>), AppError> {
    let data = body
        .get("data")
        .and_then(Value::as_array)
        .ok_or_else(|| AppError::InvalidRequest("provider did not return a model list".into()))?;
    let mut models = Vec::new();
    for item in data {
        let raw = item.get("id").and_then(Value::as_str).unwrap_or("");
        let id = raw.trim();
        if id.is_empty() || id.chars().count() > 256 || id.chars().any(char::is_control) {
            continue;
        }
        models.push(model_card(item, id));
    }
    let next = if body.get("has_more").and_then(Value::as_bool) == Some(true) {
        body.get("last_id")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|id| !id.is_empty())
            .map(str::to_owned)
    } else {
        None
    };
    Ok((models, next))
}

/// Card title plus at most two useful facts: context size and output limit.
/// Modality strings, dates, owners, pricing, and descriptions stay off the card.
fn model_card(item: &Value, id: &str) -> StoredModel {
    let name = ["display_name", "name", "title"]
        .iter()
        .find_map(|key| clean_text(item.get(*key).and_then(Value::as_str).unwrap_or(""), 80))
        .unwrap_or_else(|| id.to_owned());
    let mut fields = Vec::new();
    collect_useful(item, &mut fields);
    collect_details(item, "", 0, &mut fields);
    StoredModel {
        id: id.to_owned(),
        name,
        fields,
        free: model_is_free(item, id),
    }
}

/// A model is free only when the provider says so: a free flag, an id ending
/// in `:free`, or a pricing object whose prices are all zero. Missing prices
/// are not treated as free.
fn model_is_free(item: &Value, id: &str) -> bool {
    if ["free", "is_free", "isFree"]
        .iter()
        .any(|key| item.get(*key).is_some_and(flag_is_true))
    {
        return true;
    }
    if id
        .rsplit([':', '/'])
        .next()
        .is_some_and(|part| part.eq_ignore_ascii_case("free"))
    {
        return true;
    }
    let Some(prices) = item.get("pricing").and_then(Value::as_object) else {
        return false;
    };
    let mut saw_price = false;
    for value in prices.values() {
        let Some(amount) = price_amount(value) else {
            continue;
        };
        saw_price = true;
        if amount != 0.0 {
            return false;
        }
    }
    saw_price
}

fn flag_is_true(value: &Value) -> bool {
    match value {
        Value::Bool(flag) => *flag,
        Value::Number(number) => number.as_i64() == Some(1),
        Value::String(text) => matches!(
            text.trim().to_ascii_lowercase().as_str(),
            "true" | "yes" | "1" | "free"
        ),
        _ => false,
    }
}

fn price_amount(value: &Value) -> Option<f64> {
    match value {
        Value::Number(number) => number.as_f64(),
        Value::String(text) => text.trim().parse().ok(),
        _ => None,
    }
}

fn collect_useful(value: &Value, out: &mut Vec<ModelField>) {
    for key in [
        "context_length",
        "context_window",
        "max_context_length",
        "max_completion_tokens",
        "max_output_tokens",
    ] {
        if out.len() == 2 {
            return;
        }
        let Some(child) = value.get(key) else {
            continue;
        };
        let Some(field) = useful_field(key, child) else {
            continue;
        };
        if out.iter().any(|existing| existing.label == field.label) {
            continue;
        }
        out.push(field);
    }
}

fn useful_field(key: &str, value: &Value) -> Option<ModelField> {
    match key {
        "context_length" | "context_window" | "max_context_length" => Some(ModelField {
            label: "Context".into(),
            value: format!("{} ctx", compact_count(value)?),
        }),
        "max_completion_tokens" | "max_output_tokens" => Some(ModelField {
            label: "Output".into(),
            value: format!("{} output", compact_count(value)?),
        }),
        _ => None,
    }
}

const MAX_INFO_FIELDS: usize = 80;
const MAX_INFO_CHARS: usize = 500;

/// Extra endpoint fields for the details popup. Secrets and the id/name keys
/// already shown in the header are omitted. Nested objects are flattened once.
fn collect_details(value: &Value, prefix: &str, depth: usize, out: &mut Vec<ModelField>) {
    let Some(object) = value.as_object() else {
        return;
    };
    for (key, child) in object {
        if out.len() >= MAX_INFO_FIELDS {
            return;
        }
        if skipped_detail_key(key) || sensitive_key(key) {
            continue;
        }
        let label = detail_label(prefix, key);
        if out.iter().any(|field| field.label == label) {
            continue;
        }
        match child {
            Value::Object(_) if depth < 2 => collect_details(child, &label, depth + 1, out),
            Value::Array(items) => {
                if let Some(text) = detail_list(items) {
                    out.push(ModelField { label, value: text });
                }
            }
            other => {
                if let Some(text) = detail_scalar(other) {
                    out.push(ModelField { label, value: text });
                }
            }
        }
    }
}

fn skipped_detail_key(key: &str) -> bool {
    matches!(
        key,
        "id" | "object"
            | "display_name"
            | "name"
            | "title"
            | "context_length"
            | "context_window"
            | "max_context_length"
            | "max_completion_tokens"
            | "max_output_tokens"
    )
}

fn sensitive_key(key: &str) -> bool {
    let key = key.to_ascii_lowercase();
    matches!(
        key.as_str(),
        "key"
            | "token"
            | "secret"
            | "password"
            | "authorization"
            | "credential"
            | "api_key"
            | "apikey"
            | "access_token"
    ) || key.contains("secret")
        || key.contains("password")
        || key.ends_with("_token")
        || key.ends_with("_key")
}

fn detail_label(prefix: &str, key: &str) -> String {
    let part = humanize(key);
    if prefix.is_empty() {
        part
    } else {
        format!("{prefix} · {part}")
    }
}

fn humanize(key: &str) -> String {
    let mut words = key.split('_').filter(|word| !word.is_empty());
    let Some(first) = words.next() else {
        return String::new();
    };
    let mut label = capitalize(first);
    for word in words {
        label.push(' ');
        label.push_str(&word.to_ascii_lowercase());
    }
    label
}

fn capitalize(word: &str) -> String {
    let mut chars = word.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}

fn detail_scalar(value: &Value) -> Option<String> {
    match value {
        Value::String(text) => clean_text(text, MAX_INFO_CHARS),
        Value::Bool(flag) => Some(if *flag { "yes" } else { "no" }.to_owned()),
        Value::Number(number) => {
            if let Some(whole) = number.as_i64() {
                Some(crate::db::connection_models::group_plain_number(
                    &whole.to_string(),
                ))
            } else {
                number.as_f64().map(format_decimal)
            }
        }
        _ => None,
    }
}

fn detail_list(items: &[Value]) -> Option<String> {
    let mut values = Vec::new();
    for item in items.iter().take(16) {
        if let Some(text) = item.as_str().and_then(|text| clean_text(text, 80)) {
            values.push(text);
        } else if let Some(text) = item
            .get("id")
            .or_else(|| item.get("name"))
            .and_then(Value::as_str)
            .and_then(|text| clean_text(text, 80))
        {
            values.push(text);
        }
    }
    if values.is_empty() {
        return None;
    }
    let mut text = values.join(", ");
    if items.len() > values.len() {
        text.push('…');
    }
    clean_text(&text, MAX_INFO_CHARS)
}

fn format_decimal(value: f64) -> String {
    let text = format!("{value:.6}");
    text.trim_end_matches('0').trim_end_matches('.').to_owned()
}

fn compact_count(value: &Value) -> Option<String> {
    let whole = value.as_i64().filter(|n| *n > 0)?;
    Some(crate::db::connection_models::group_plain_number(
        &whole.to_string(),
    ))
}

fn clean_text(text: &str, max_chars: usize) -> Option<String> {
    let text = text.trim();
    if text.is_empty() || text.chars().any(char::is_control) {
        return None;
    }
    let mut out = String::new();
    for (index, ch) in text.chars().enumerate() {
        if index == max_chars {
            out.push('…');
            break;
        }
        out.push(ch);
    }
    Some(out)
}

fn query_component(value: &str) -> String {
    let mut encoded = String::new();
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                encoded.push(byte as char);
            }
            _ => encoded.push_str(&format!("%{byte:02X}")),
        }
    }
    encoded
}

async fn read_model_body(response: reqwest::Response) -> Result<Bytes, AppError> {
    if response
        .content_length()
        .is_some_and(|size| size > MAX_MODEL_LIST_BYTES as u64)
    {
        return Err(AppError::InvalidRequest(
            "provider model list is too large".into(),
        ));
    }
    let mut stream = response.bytes_stream();
    let mut body = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|_| AppError::HttpClient)?;
        if chunk.len() > MAX_MODEL_LIST_BYTES.saturating_sub(body.len()) {
            return Err(AppError::InvalidRequest(
                "provider model list is too large".into(),
            ));
        }
        body.extend_from_slice(&chunk);
    }
    Ok(Bytes::from(body))
}

pub async fn test_connection(
    client: &reqwest::Client,
    crypto: &Crypto,
    provider: &Provider,
) -> Result<(), AppError> {
    let response = request(client, crypto, provider, reqwest::Method::GET, "models")?
        .timeout(std::time::Duration::from_secs(15))
        .send()
        .await
        .map_err(|_| AppError::HttpClient)?;
    if response.status().is_success() {
        Ok(())
    } else {
        Err(AppError::Upstream(response.status()))
    }
}

fn check_status(response: &reqwest::Response) -> Result<(), AttemptError> {
    let status = response.status();
    if !status.is_success() {
        tracing::warn!(
            status = status.as_u16(),
            "upstream returned an unsuccessful status"
        );
    }
    if status.is_success() {
        Ok(())
    } else if crate::routing::combo::is_retryable_status(status) {
        Err(AttemptError::Retryable)
    } else {
        Err(AttemptError::Permanent(AppError::Upstream(status)))
    }
}

fn transport_error(error: reqwest::Error) -> AttemptError {
    // Never format the error itself: reqwest errors can contain request URLs.
    tracing::warn!(
        timeout = error.is_timeout(),
        connect = error.is_connect(),
        body = error.is_body(),
        "upstream transport failed"
    );
    if error.is_builder() {
        AttemptError::Permanent(AppError::Provider)
    } else {
        AttemptError::Retryable
    }
}

fn check_sse(response: &reqwest::Response) -> Result<(), AttemptError> {
    let is_sse = response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(';').next())
        .is_some_and(|value| value.trim().eq_ignore_ascii_case("text/event-stream"));
    if is_sse {
        Ok(())
    } else {
        Err(AttemptError::Retryable)
    }
}

async fn read_bounded(response: reqwest::Response) -> Result<Bytes, AttemptError> {
    if response
        .content_length()
        .is_some_and(|size| size > MAX_RESPONSE_BYTES as u64)
    {
        return Err(AttemptError::Retryable);
    }
    let mut stream = response.bytes_stream();
    let mut body = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(transport_error)?;
        if chunk.len() > MAX_RESPONSE_BYTES - body.len() {
            return Err(AttemptError::Retryable);
        }
        body.extend_from_slice(&chunk);
    }
    Ok(Bytes::from(body))
}

fn json_response(status: StatusCode, body: Bytes) -> Response {
    let usage = serde_json::from_slice::<Value>(&body)
        .ok()
        .map(|value| crate::history::Usage::from_value(&value))
        .unwrap_or_default();
    let mut response = Response::new(Body::from(body));
    response.extensions_mut().insert(usage);
    *response.status_mut() = status;
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        header::HeaderValue::from_static("application/json"),
    );
    response
}

async fn streaming_response(mut stream: ResponseStream) -> Result<Response, AttemptError> {
    let first = loop {
        match stream.next().await {
            Some(Ok(bytes)) if bytes.is_empty() => continue,
            Some(Ok(bytes)) => break bytes,
            Some(Err(error)) => return Err(error),
            None => return Err(AttemptError::Retryable),
        }
    };
    // Commitment boundary: before this point an attempt can fail over. After we
    // return the response, the body owns the request and errors only close it.
    let body: Pin<Box<dyn Stream<Item = Result<Bytes, std::io::Error>> + Send>> =
        Box::pin(async_stream::try_stream! {
            yield first;
            while let Some(chunk) = stream.next().await {
                yield chunk.map_err(|_| std::io::Error::other("Upstream stream interrupted"))?;
            }
        });
    let mut response = Response::new(Body::from_stream(body));
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        header::HeaderValue::from_static("text/event-stream"),
    );
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        header::HeaderValue::from_static("no-cache, no-store"),
    );
    response
        .headers_mut()
        .insert("x-accel-buffering", header::HeaderValue::from_static("no"));
    Ok(response)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn normalizes_custom_and_local_urls_and_rejects_unsafe_components() {
        assert_eq!(
            normalize_base_url("  http://localhost:1234/custom/v1/// ").unwrap(),
            "http://localhost:1234/custom/v1"
        );
        assert_eq!(
            normalize_base_url("https://EXAMPLE.com/").unwrap(),
            "https://example.com"
        );
        for url in [
            "file:///tmp/model",
            "https://user:secret@example.com",
            "https://example.com?key=secret",
            "https://example.com#fragment",
            "relative/path",
        ] {
            assert!(normalize_base_url(url).is_err());
        }
    }

    #[test]
    fn parse_model_page_reads_ids_and_anthropic_pagination() {
        let (models, next) = parse_model_page(&json!({
            "data": [
                {
                    "id": " gpt-4o ",
                    "display_name": "GPT-4o",
                    "owned_by": "openai",
                    "created": 1704067200,
                    "context_length": 1050000,
                    "max_output_tokens": 16384,
                    "description": "A long description that should not appear on the card.",
                    "api_key": "sk-should-not-appear",
                    "pricing": {
                        "prompt": "0.005",
                        "input_per_million": 1.25,
                        "above_threshold": {"output_per_million": "12"},
                        "secret": "nope"
                    },
                    "aliases": ["anthropic/claude", "vendor/claude"],
                    "type": "text"
                },
                {"id": ""},
                {"id": "gpt-4o-mini"},
                {"name": "missing-id"}
            ]
        }))
        .unwrap();
        assert_eq!(models[0].id, "gpt-4o");
        assert_eq!(models[0].name, "GPT-4o");
        assert_eq!(models[1].id, "gpt-4o-mini");
        assert_eq!(
            models[0].display_fields(),
            vec![
                ModelField {
                    label: "Context".into(),
                    value: "1.050.000 ctx".into(),
                },
                ModelField {
                    label: "Output".into(),
                    value: "16.384 output".into(),
                },
            ]
        );
        assert!(!models[0].free);
        assert!(models[0].search_text().contains("1.050.000"));
        assert!(!models[0].search_text().contains("openai"));
        let info = models[0].info_fields();
        assert!(
            info.iter()
                .any(|field| field.label == "Type" && field.value == "text")
        );
        assert!(info.iter().any(|field| {
            field.label == "Pricing · Input per million" && field.value == "$1.25 / 1M tokens"
        }));
        assert!(
            info.iter()
                .any(|field| field.label == "Pricing · Above threshold · Output per million")
        );
        assert!(
            info.iter()
                .any(|field| field.label == "Aliases" && field.value.contains("anthropic/claude"))
        );
        let (free_models, _) = parse_model_page(&json!({
            "data": [
                {"id": "llama:free", "context_length": 8192},
                {"id": "paid", "pricing": {"prompt": "0", "completion": "0.0001"}},
                {"id": "gratis", "pricing": {"prompt": "0", "completion": "0"}}
            ]
        }))
        .unwrap();
        assert!(free_models[0].free && free_models[2].free);
        assert!(!free_models[1].free);
        let (flagged, _) = parse_model_page(&json!({
            "data": [{"id": "promo", "is_free": true, "context_length": 1000000}]
        }))
        .unwrap();
        assert!(flagged[0].is_free());
        assert_eq!(flagged[0].display_fields()[0].value, "1.000.000 ctx");
        let rendered = serde_json::to_string(&models[0].fields).unwrap();
        assert!(!rendered.contains("sk-should-not-appear"));
        assert!(!rendered.contains("nope"));
        assert!(next.is_none());
        let (_, next) = parse_model_page(&json!({
            "data": [{"id": "claude-sonnet"}],
            "has_more": true,
            "last_id": "claude-sonnet"
        }))
        .unwrap();
        assert_eq!(next.as_deref(), Some("claude-sonnet"));
        assert!(parse_model_page(&json!({"models": ["nope"]})).is_err());
    }

    #[test]
    fn replacing_model_preserves_extensions_and_original() {
        let original = json!({"model":"combo", "messages":[], "custom":{"value":42}});
        let replaced = replace_model(&original, "real-model").unwrap();
        assert_eq!(replaced["model"], "real-model");
        assert_eq!(replaced["custom"], original["custom"]);
        assert_eq!(original["model"], "combo");
        assert!(replace_model(&json!([]), "model").is_err());
    }

    #[tokio::test]
    async fn stream_prefetch_retries_before_commit_and_errors_after_commit() {
        use http_body_util::BodyExt;

        let early =
            futures_util::stream::iter(vec![Ok(Bytes::new()), Err(AttemptError::Retryable)]);
        assert!(matches!(
            streaming_response(Box::pin(early)).await,
            Err(AttemptError::Retryable)
        ));
        let late = futures_util::stream::iter(vec![
            Ok(Bytes::new()),
            Ok(Bytes::from_static(b"data: first\n\n")),
            Err(AttemptError::Permanent(AppError::InvalidRequest(
                "secret upstream body".into(),
            ))),
        ]);
        let mut response = streaming_response(Box::pin(late)).await.unwrap();
        let first = response
            .body_mut()
            .frame()
            .await
            .unwrap()
            .unwrap()
            .into_data()
            .unwrap();
        assert_eq!(first.as_ref(), b"data: first\n\n");
        let error = response.body_mut().frame().await.unwrap().unwrap_err();
        assert!(error.to_string().contains("Upstream stream interrupted"));
        assert!(!error.to_string().contains("secret"));
    }
}
