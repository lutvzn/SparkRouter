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

use crate::{crypto::Crypto, db::providers::Provider, error::AppError};

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
