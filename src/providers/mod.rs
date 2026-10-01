pub mod anthropic;
pub mod openai;
// Framing decoder private to the provider adapters.
mod sse;

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

const MAX_MODEL_FIELDS: usize = 6;
const MAX_FIELD_CHARS: usize = 120;

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

/// Card title plus a few short scalar fields. Nested objects are flattened one
/// level. Credential-like keys and long values are omitted.
fn model_card(item: &Value, id: &str) -> StoredModel {
    let name = ["display_name", "name", "title"]
        .iter()
        .find_map(|key| clean_text(item.get(*key).and_then(Value::as_str).unwrap_or(""), 256))
        .unwrap_or_else(|| id.to_owned());
    let mut fields = Vec::new();
    collect_fields(item, "", 0, &mut fields);
    fields.sort_by_key(|field| field.0);
    StoredModel {
        id: id.to_owned(),
        name,
        fields: fields
            .into_iter()
            .take(MAX_MODEL_FIELDS)
            .map(|(_, label, value)| ModelField { label, value })
            .collect(),
    }
}

fn collect_fields(value: &Value, prefix: &str, depth: usize, out: &mut Vec<(u8, String, String)>) {
    let Some(object) = value.as_object() else {
        return;
    };
    for (key, child) in object {
        if skipped_key(key) || sensitive_key(key) {
            continue;
        }
        let label = field_label(prefix, key);
        if let Some(text) = scalar_text(key, child) {
            out.push((field_rank(key, depth), label, text));
            continue;
        }
        if depth == 0 && child.is_object() {
            collect_fields(child, &label, 1, out);
        }
    }
}

fn skipped_key(key: &str) -> bool {
    matches!(
        key,
        "id" | "object" | "type" | "display_name" | "name" | "title"
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
        || key.contains("token")
        || key.contains("password")
}

fn field_rank(key: &str, depth: usize) -> u8 {
    let preferred = match key {
        "owned_by" => 0,
        "created" | "created_at" => 1,
        "context_length" | "context_window" | "max_tokens" | "max_completion_tokens" => 2,
        "description" => 3,
        _ => 4,
    };
    preferred + u8::from(depth > 0) * 5
}

fn field_label(prefix: &str, key: &str) -> String {
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

fn scalar_text(key: &str, value: &Value) -> Option<String> {
    match value {
        Value::String(text) => clean_text(text, MAX_FIELD_CHARS).map(|text| format_time(key, text)),
        Value::Number(number) => number_text(key, number),
        Value::Bool(flag) => Some(if *flag { "yes" } else { "no" }.to_owned()),
        Value::Array(items) => string_list(items),
        _ => None,
    }
}

fn number_text(key: &str, number: &serde_json::Number) -> Option<String> {
    if let Some(whole) = number.as_i64() {
        if timestamp_key(key) && (1_000_000_000..4_000_000_000).contains(&whole) {
            return Some(format_unix_date(whole));
        }
        return Some(whole.to_string());
    }
    let value = number.as_f64()?;
    if value.fract() == 0.0 && value.abs() < 1e15 {
        Some((value as i64).to_string())
    } else {
        Some(
            format!("{value:.4}")
                .trim_end_matches('0')
                .trim_end_matches('.')
                .to_owned(),
        )
    }
}

fn string_list(items: &[Value]) -> Option<String> {
    let values: Vec<&str> = items.iter().filter_map(Value::as_str).take(4).collect();
    if values.is_empty() || values.len() != items.len().min(4) {
        return None;
    }
    let mut text = values[..values.len().min(3)].join(", ");
    if items.len() > 3 {
        text.push('…');
    }
    clean_text(&text, MAX_FIELD_CHARS)
}

fn format_time(key: &str, text: String) -> String {
    if timestamp_key(key)
        && text.len() >= 10
        && text.as_bytes().get(4) == Some(&b'-')
        && text.as_bytes().get(7) == Some(&b'-')
    {
        text[..10].to_owned()
    } else {
        text
    }
}

fn timestamp_key(key: &str) -> bool {
    key == "created" || key.ends_with("_at") || key.ends_with("_time")
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

fn format_unix_date(secs: i64) -> String {
    let days = secs.div_euclid(86_400);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if month <= 2 { year + 1 } else { year };
    format!("{year:04}-{month:02}-{day:02}")
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
    let mut response = Response::new(Body::from(body));
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
                    "api_key": "sk-should-not-appear",
                    "pricing": {"prompt": "0.005", "secret": "nope"}
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
        assert!(
            models[0]
                .fields
                .iter()
                .any(|field| field.label == "Owned by" && field.value == "openai")
        );
        assert!(
            models[0]
                .fields
                .iter()
                .any(|field| field.label == "Created" && field.value == "2024-01-01")
        );
        assert!(
            models[0]
                .fields
                .iter()
                .any(|field| field.label == "Pricing · Prompt")
        );
        assert!(models[0].search_text().contains("openai"));
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
