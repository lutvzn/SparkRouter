//! Media protocol adapters. No remote asset downloads, workers, or per-call clients.
use std::{sync::Arc, time::Duration};

use axum::{
    body::Body,
    http::{StatusCode, header},
    response::Response,
};
use base64::{Engine, engine::general_purpose::STANDARD};
use bytes::Bytes;
use futures_util::StreamExt;
use serde_json::{Value, json};

use super::{
    AttemptError, Protocol, check_status, json_response, read_bounded, replace_model, request,
    transport_error,
};
use crate::{crypto::Crypto, db::providers::Provider, error::AppError};

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Kind {
    Image,
    ImageEdit,
    Video,
    Music,
    Speech,
    Transcription,
    Translation,
}
impl Kind {
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Image => "image",
            Self::ImageEdit => "image_edit",
            Self::Video => "video",
            Self::Music => "music",
            Self::Speech => "speech",
            Self::Transcription => "transcription",
            Self::Translation => "translation",
        }
    }
    pub(crate) fn accepts(self, protocol: Protocol) -> bool {
        match protocol {
            Protocol::OpenAi => true,
            Protocol::MiniMax => {
                matches!(
                    self,
                    Self::Image | Self::Video | Self::Music | Self::Speech | Self::Transcription
                )
            }
            Protocol::Xai => matches!(self, Self::Image | Self::Video),
            Protocol::Fal => matches!(self, Self::Image | Self::Video | Self::Music),
            Protocol::Deepgram => matches!(self, Self::Speech | Self::Transcription),
            Protocol::ElevenLabs => {
                matches!(self, Self::Speech | Self::Transcription | Self::Music)
            }
            _ => false,
        }
    }
}

// Upload buffers are shared across sequential attempts; files are not copied or base64 encoded.
#[derive(Clone)]
pub(crate) struct UploadPart {
    pub name: String,
    pub filename: Option<String>,
    pub content_type: Option<String>,
    pub bytes: Bytes,
}
pub(crate) type Upload = Arc<Vec<UploadPart>>;

#[derive(Clone, Copy)]
pub(crate) struct BinaryStream;
const MAX_BINARY_BYTES: usize = 64 * 1024 * 1024;

fn invalid(message: &str) -> AttemptError {
    AppError::InvalidRequest(message.into()).into()
}
fn text<'a>(body: &'a Value, key: &str) -> Result<&'a str, AttemptError> {
    body.get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| invalid(&format!("{key} must be a nonempty string")))
}
fn object_response(value: Value) -> Result<Response, AttemptError> {
    Ok(json_response(
        StatusCode::OK,
        Bytes::from(serde_json::to_vec(&value).map_err(|_| AttemptError::Retryable)?),
    ))
}
fn created() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
fn media_data(data: Vec<Value>) -> Result<Response, AttemptError> {
    object_response(json!({"created": created(), "data": data}))
}
fn has_error(value: &Value) -> bool {
    value.get("error").is_some_and(|v| !v.is_null())
}
fn valid_url(value: &Value) -> Option<&str> {
    let value = value.as_str()?;
    let url = reqwest::Url::parse(value).ok()?;
    (matches!(url.scheme(), "http" | "https")
        && url.host_str().is_some()
        && url.username().is_empty()
        && url.password().is_none())
    .then_some(value)
}
fn path_id(value: &str) -> Result<&str, AttemptError> {
    if value.is_empty()
        || value.len() > 256
        || !value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
        || value == "."
        || value == ".."
    {
        return Err(invalid("Invalid upstream path identifier"));
    }
    Ok(value)
}
fn fal_path(model: &str) -> Result<String, AttemptError> {
    // Model IDs become a path only for fal. Never allow URL/query/traversal syntax.
    let path = model
        .split('/')
        .map(path_id)
        .collect::<Result<Vec<_>, _>>()?
        .join("/");
    Ok(path)
}

pub(crate) fn dialect(provider: &Provider) -> Protocol {
    if provider.protocol != Protocol::OpenAi {
        return provider.protocol;
    }
    // Existing MiniMax connections retain their protocol and provider grouping.
    match reqwest::Url::parse(&provider.base_url)
        .ok()
        .and_then(|u| u.host_str().map(str::to_owned))
        .as_deref()
    {
        Some("api.minimax.io" | "api.minimaxi.com") => Protocol::MiniMax,
        Some("api.x.ai") => Protocol::Xai,
        _ => Protocol::OpenAi,
    }
}

pub(crate) async fn call(
    client: &reqwest::Client,
    crypto: &Crypto,
    provider: &Provider,
    model: &str,
    body: &Value,
    kind: Kind,
    upload: Option<&Upload>,
) -> Result<Response, AttemptError> {
    if kind == Kind::Transcription
        && matches!(dialect(provider), Protocol::Deepgram | Protocol::ElevenLabs)
        && !matches!(
            body.get("response_format")
                .and_then(Value::as_str)
                .unwrap_or("json"),
            "json" | "text"
        )
    {
        return Err(invalid(
            "This provider supports only json or text transcription responses",
        ));
    }
    match dialect(provider) {
        Protocol::MiniMax if kind == Kind::Transcription => {
            compatible(client, crypto, provider, model, body, kind, upload).await
        }
        Protocol::Fal => fal(client, crypto, provider, model, body, kind).await,
        Protocol::MiniMax => minimax(client, crypto, provider, model, body, kind).await,
        Protocol::Deepgram => deepgram(client, crypto, provider, model, body, kind, upload).await,
        Protocol::ElevenLabs => {
            elevenlabs(client, crypto, provider, model, body, kind, upload).await
        }
        Protocol::Xai if kind == Kind::Video => {
            xai_video(client, crypto, provider, model, body).await
        }
        Protocol::OpenAi | Protocol::Xai => {
            compatible(client, crypto, provider, model, body, kind, upload).await
        }
        _ => Err(invalid("Provider does not support this media endpoint")),
    }
}

async fn send_json(
    builder: reqwest::RequestBuilder,
    body: &Value,
) -> Result<(Bytes, Value), AttemptError> {
    let response = builder.json(body).send().await.map_err(transport_error)?;
    check_status(&response)?;
    let bytes = read_bounded(response).await?;
    let value: Value = serde_json::from_slice(&bytes).map_err(|_| AttemptError::Retryable)?;
    if !value.is_object() || has_error(&value) {
        return Err(AttemptError::Retryable);
    }
    Ok((bytes, value))
}

pub(crate) fn multipart(
    upload: &Upload,
    model: &str,
    model_field: &str,
) -> Result<reqwest::multipart::Form, AttemptError> {
    let mut form = reqwest::multipart::Form::new().text(model_field.to_owned(), model.to_owned());
    for part in upload.iter().filter(|part| part.name != "model") {
        let mut value = reqwest::multipart::Part::stream_with_length(
            reqwest::Body::from(part.bytes.clone()),
            part.bytes.len() as u64,
        );
        if let Some(filename) = &part.filename {
            value = value.file_name(filename.clone());
        }
        if let Some(content_type) = &part.content_type {
            value = value
                .mime_str(content_type)
                .map_err(|_| invalid("Invalid upload content type"))?;
        }
        form = form.part(part.name.clone(), value);
    }
    Ok(form)
}

async fn compatible(
    client: &reqwest::Client,
    crypto: &Crypto,
    provider: &Provider,
    model: &str,
    body: &Value,
    kind: Kind,
    upload: Option<&Upload>,
) -> Result<Response, AttemptError> {
    let endpoint = match kind {
        Kind::Image => "images/generations",
        Kind::ImageEdit => "images/edits",
        Kind::Video => "videos/generations",
        Kind::Music => "music/generations",
        Kind::Speech => "audio/speech",
        Kind::Transcription if dialect(provider) == Protocol::MiniMax => "speech_to_text",
        Kind::Transcription => "audio/transcriptions",
        Kind::Translation => "audio/translations",
    };
    let mut builder = request(client, crypto, provider, reqwest::Method::POST, endpoint)?;
    let minimax_stt = kind == Kind::Transcription && dialect(provider) == Protocol::MiniMax;
    if minimax_stt && let Some(language) = body.get("language").and_then(Value::as_str) {
        builder = builder.header("language", language);
    }
    let builder = if let Some(upload) = upload {
        let filtered = minimax_stt.then(|| {
            Arc::new(
                upload
                    .iter()
                    .filter(|p| p.name != "language")
                    .cloned()
                    .collect(),
            )
        });
        builder.multipart(multipart(
            filtered.as_ref().unwrap_or(upload),
            model,
            "model",
        )?)
    } else {
        builder.json(&replace_model(body, model)?)
    };
    let response = builder.send().await.map_err(transport_error)?;
    check_status(&response)?;
    // Generic compatible routes return completed outputs, not resumable jobs.
    // A known accepted asynchronous request must not trigger a backup submission.
    if response.status() == StatusCode::ACCEPTED {
        return Err(AttemptError::Permanent(AppError::Routing));
    }
    if kind == Kind::Speech {
        return binary(response, audio_mime(body)).await;
    }
    let content_type = response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("application/json")
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_owned();
    let bytes = read_bounded(response).await?;
    if matches!(kind, Kind::Transcription | Kind::Translation)
        && matches!(
            content_type.as_str(),
            "text/plain" | "application/x-subrip" | "text/vtt"
        )
    {
        if bytes.is_empty() || std::str::from_utf8(&bytes).is_err() {
            return Err(AttemptError::Retryable);
        }
        let mut response = Response::new(Body::from(bytes));
        response.headers_mut().insert(
            header::CONTENT_TYPE,
            header::HeaderValue::from_str(&content_type).map_err(|_| AttemptError::Retryable)?,
        );
        return Ok(response);
    }
    let value: Value = serde_json::from_slice(&bytes).map_err(|_| AttemptError::Retryable)?;
    let valid = match kind {
        Kind::Transcription | Kind::Translation => value.get("text").is_some_and(Value::is_string),
        _ => value
            .get("data")
            .and_then(Value::as_array)
            .is_some_and(|data| {
                !data.is_empty()
                    && data.iter().all(|item| {
                        item.get("url").and_then(valid_url).is_some()
                            || item
                                .get("b64_json")
                                .and_then(Value::as_str)
                                .is_some_and(|v| !v.is_empty())
                    })
            }),
    };
    if !valid || has_error(&value) {
        let job = value.get("task_id").is_some()
            || value.get("request_id").is_some()
            || matches!(
                value.get("status").and_then(Value::as_str),
                Some("queued" | "pending" | "in_progress")
            );
        return Err(if job {
            AttemptError::Permanent(AppError::Routing)
        } else {
            AttemptError::Retryable
        });
    }
    Ok(json_response(StatusCode::OK, bytes))
}

fn audio_mime(body: &Value) -> &'static str {
    match body
        .get("response_format")
        .or_else(|| body.pointer("/audio_setting/format"))
        .and_then(Value::as_str)
        .unwrap_or("mp3")
    {
        "wav" => "audio/wav",
        "opus" => "audio/ogg",
        "aac" => "audio/aac",
        "flac" => "audio/flac",
        "pcm" => "application/octet-stream",
        _ => "audio/mpeg",
    }
}

async fn binary(
    response: reqwest::Response,
    fallback_mime: &'static str,
) -> Result<Response, AttemptError> {
    let expected = response.content_length();
    if expected.is_some_and(|v| v == 0 || v > MAX_BINARY_BYTES as u64) {
        return Err(AttemptError::Retryable);
    }
    let mime = response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or(fallback_mime)
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_owned();
    if !mime.starts_with("audio/") && mime != "application/octet-stream" {
        return Err(AttemptError::Retryable);
    }
    let mut source = response.bytes_stream();
    let first = loop {
        match source.next().await {
            Some(Ok(bytes)) if bytes.is_empty() => continue,
            Some(Ok(bytes)) if bytes.len() <= MAX_BINARY_BYTES => break bytes,
            _ => return Err(AttemptError::Retryable),
        }
    };
    let stream: std::pin::Pin<
        Box<dyn futures_util::Stream<Item = Result<Bytes, std::io::Error>> + Send>,
    > = Box::pin(async_stream::try_stream! {
        let mut total = first.len();
        yield first;
        while let Some(chunk) = source.next().await {
            let chunk = chunk.map_err(|_| std::io::Error::other("Upstream media interrupted"))?;
            if chunk.len() > MAX_BINARY_BYTES - total { Err(std::io::Error::other("Upstream media size limit"))?; }
            total += chunk.len();
            yield chunk;
        }
        if expected.is_some_and(|n| n != total as u64) { Err(std::io::Error::other("Upstream media incomplete"))?; }
    });
    let mut response = Response::new(Body::from_stream(stream));
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        header::HeaderValue::from_str(&mime).map_err(|_| AttemptError::Retryable)?,
    );
    response.extensions_mut().insert(BinaryStream);
    Ok(response)
}

async fn fal(
    client: &reqwest::Client,
    crypto: &Crypto,
    provider: &Provider,
    model: &str,
    body: &Value,
    kind: Kind,
) -> Result<Response, AttemptError> {
    let model = fal_path(model)?;
    let mut input = replace_model(body, model.as_str())?;
    input.as_object_mut().unwrap().remove("model");
    input.as_object_mut().unwrap().remove("stream");
    if kind == Kind::Music && model.ends_with("ace-step") && input.get("tags").is_none() {
        input["tags"] = input["prompt"].clone();
        input.as_object_mut().unwrap().remove("prompt");
    }
    let value = submit_video(
        request(client, crypto, provider, reqwest::Method::POST, &model)?,
        &input,
    )
    .await?;
    let value = if value.get("request_id").is_some() || value.get("status_url").is_some() {
        fal_result(client, crypto, provider, &value).await?
    } else {
        value
    };
    normalize_fal(&value, kind).map_err(|_| AttemptError::Permanent(AppError::Routing))
}

fn normalize_fal(value: &Value, kind: Kind) -> Result<Response, AttemptError> {
    let data = match kind {
        Kind::Image => value
            .get("images")
            .and_then(Value::as_array)
            .cloned()
            .ok_or(AttemptError::Retryable)?,
        Kind::Video => vec![value.get("video").cloned().ok_or(AttemptError::Retryable)?],
        Kind::Music => vec![value.get("audio").cloned().ok_or(AttemptError::Retryable)?],
        _ => return Err(invalid("Unsupported fal media endpoint")),
    };
    if data.is_empty()
        || data
            .iter()
            .any(|item| item.get("url").and_then(valid_url).is_none())
    {
        return Err(AttemptError::Retryable);
    }
    media_data(data)
}

// Poll URLs come from an upstream response, not trusted configuration. Only
// authenticated GETs under the same configured origin/path and request ID are allowed.
async fn fal_result(
    client: &reqwest::Client,
    crypto: &Crypto,
    provider: &Provider,
    submitted: &Value,
) -> Result<Value, AttemptError> {
    let fail = || AttemptError::Permanent(AppError::Routing);
    let task = submitted
        .get("request_id")
        .and_then(Value::as_str)
        .and_then(|id| path_id(id).ok())
        .ok_or_else(fail)?;
    let base = super::normalize_base_url(&provider.base_url)?;
    let base_url = reqwest::Url::parse(&base).map_err(|_| fail())?;
    let endpoint = |key: &str| -> Result<String, AttemptError> {
        let raw = submitted
            .get(key)
            .and_then(Value::as_str)
            .ok_or_else(fail)?;
        let url = reqwest::Url::parse(raw).map_err(|_| fail())?;
        if url.origin() != base_url.origin()
            || !url.username().is_empty()
            || url.password().is_some()
            || url.fragment().is_some()
            || !url
                .path()
                .split('/')
                .collect::<Vec<_>>()
                .windows(2)
                .any(|pair| pair[0] == "requests" && pair[1] == task)
        {
            return Err(fail());
        }
        url.as_str()
            .strip_prefix(&format!("{base}/"))
            .map(str::to_owned)
            .ok_or_else(fail)
    };
    let status = endpoint("status_url")?;
    let result = endpoint("response_url")?;
    let polling = tokio::time::timeout(Duration::from_secs(570), async {
        loop {
            let response = request(client, crypto, provider, reqwest::Method::GET, &status)?
                .send()
                .await
                .map_err(transport_error)?;
            check_status(&response)?;
            let value: Value = serde_json::from_slice(&read_bounded(response).await?)
                .map_err(|_| AttemptError::Retryable)?;
            match value.get("status").and_then(Value::as_str) {
                Some("COMPLETED") => {
                    let response =
                        request(client, crypto, provider, reqwest::Method::GET, &result)?
                            .send()
                            .await
                            .map_err(transport_error)?;
                    check_status(&response)?;
                    let value: Value = serde_json::from_slice(&read_bounded(response).await?)
                        .map_err(|_| AttemptError::Retryable)?;
                    return Ok(value);
                }
                Some("IN_QUEUE" | "IN_PROGRESS") => {
                    tokio::time::sleep(Duration::from_secs(2)).await
                }
                _ => return Err(AttemptError::Retryable),
            }
        }
    })
    .await;
    polling
        .unwrap_or(Err(AttemptError::Retryable))
        .map_err(|_| fail())
}

async fn minimax(
    client: &reqwest::Client,
    crypto: &Crypto,
    provider: &Provider,
    model: &str,
    body: &Value,
    kind: Kind,
) -> Result<Response, AttemptError> {
    if kind == Kind::Video {
        return minimax_video(client, crypto, provider, model, body).await;
    }
    let endpoint = match kind {
        Kind::Image => "image_generation",
        Kind::Music => "music_generation",
        Kind::Speech => "t2a_v2",
        _ => return Err(invalid("Unsupported MiniMax media endpoint")),
    };
    let mut input = replace_model(body, model)?;
    if kind == Kind::Image {
        input.as_object_mut().unwrap().remove("stream");
    } else {
        input["stream"] = json!(false);
    }
    if kind == Kind::Image && input["response_format"] == "b64_json" {
        input["response_format"] = json!("base64");
    }
    if kind == Kind::Speech {
        input["text"] = json!(text(body, "input")?);
        input.as_object_mut().unwrap().remove("input");
        if input.get("voice_setting").is_none() {
            input["voice_setting"] = json!({"voice_id": text(body, "voice")?});
        }
        input.as_object_mut().unwrap().remove("voice");
        if let Some(speed) = input.as_object_mut().unwrap().remove("speed") {
            let settings = input
                .get_mut("voice_setting")
                .and_then(Value::as_object_mut)
                .ok_or_else(|| invalid("voice_setting must be an object"))?;
            settings.insert("speed".into(), speed);
        }
        if input.get("audio_setting").is_none() {
            input["audio_setting"] = json!({"format": body.get("response_format").and_then(Value::as_str).unwrap_or("mp3")});
        }
        input.as_object_mut().unwrap().remove("response_format");
        input["output_format"] = json!("hex");
    } else if kind == Kind::Music && input.get("output_format").is_none() {
        input["output_format"] = json!("url");
    }
    let (_, value) = send_json(
        request(client, crypto, provider, reqwest::Method::POST, endpoint)?,
        &input,
    )
    .await?;
    check_minimax(&value)?;
    if kind == Kind::Image {
        if let Some(data) = value
            .pointer("/data/image_base64")
            .and_then(Value::as_array)
        {
            if data.is_empty()
                || data
                    .iter()
                    .any(|v| !v.as_str().is_some_and(|s| !s.is_empty()))
            {
                return Err(AttemptError::Retryable);
            }
            return media_data(data.iter().map(|b64| json!({"b64_json": b64})).collect());
        }
        let data = value
            .pointer("/data/image_urls")
            .and_then(Value::as_array)
            .ok_or(AttemptError::Retryable)?;
        if data.is_empty() || data.iter().any(|v| valid_url(v).is_none()) {
            return Err(AttemptError::Retryable);
        }
        return media_data(data.iter().map(|url| json!({"url": url})).collect());
    }
    if value.pointer("/data/status").and_then(Value::as_i64) != Some(2) {
        return Err(AttemptError::Retryable);
    }
    let audio = value
        .pointer("/data/audio")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or(AttemptError::Retryable)?;
    if kind == Kind::Speech {
        let bytes = decode_hex(audio)?;
        let mut response = Response::new(Body::from(bytes));
        response.headers_mut().insert(
            header::CONTENT_TYPE,
            header::HeaderValue::from_static(audio_mime(body)),
        );
        return Ok(response);
    }
    let format = input
        .pointer("/audio_setting/format")
        .and_then(Value::as_str)
        .unwrap_or("mp3");
    if input["output_format"] == "url" {
        if valid_url(&json!(audio)).is_none() {
            return Err(AttemptError::Retryable);
        }
        media_data(vec![json!({"url": audio, "format": format})])
    } else {
        media_data(vec![
            json!({"b64_json": STANDARD.encode(decode_hex(audio)?), "format": format}),
        ])
    }
}
fn decode_hex(value: &str) -> Result<Vec<u8>, AttemptError> {
    if !value.len().is_multiple_of(2) || !value.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(AttemptError::Retryable);
    }
    value
        .as_bytes()
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| {
            u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16)
                .map_err(|_| AttemptError::Retryable)
        })
        .collect()
}
fn check_minimax(value: &Value) -> Result<(), AttemptError> {
    match value
        .pointer("/base_resp/status_code")
        .and_then(Value::as_i64)
    {
        Some(0) => Ok(()),
        Some(1000 | 1001 | 1002 | 1004 | 1008 | 1024 | 1033 | 1039 | 1041 | 2045 | 2049 | 2056) => {
            Err(AttemptError::Retryable)
        }
        Some(_) => Err(AppError::Upstream(StatusCode::UNPROCESSABLE_ENTITY).into()),
        None => Err(AttemptError::Retryable),
    }
}

// Once a video task is accepted, failures are permanent for this gateway request:
// never submit a second billable job to a backup. Poll inline, without a worker.
async fn minimax_video(
    client: &reqwest::Client,
    crypto: &Crypto,
    provider: &Provider,
    model: &str,
    body: &Value,
) -> Result<Response, AttemptError> {
    let submitted = submit_video(
        request(
            client,
            crypto,
            provider,
            reqwest::Method::POST,
            "video_generation",
        )?,
        &replace_model(body, model)?,
    )
    .await?;
    // An explicit envelope rejection means no task was accepted, so normal
    // retry classification still applies. An absent envelope is ambiguous.
    if submitted
        .pointer("/base_resp/status_code")
        .and_then(Value::as_i64)
        .is_none()
    {
        return Err(AttemptError::Permanent(AppError::Routing));
    }
    check_minimax(&submitted)?;
    let task = path_id(
        submitted
            .get("task_id")
            .and_then(Value::as_str)
            .ok_or_else(|| AttemptError::Permanent(AppError::Routing))?,
    )
    .map_err(|_| AttemptError::Permanent(AppError::Routing))?;
    let result = tokio::time::timeout(Duration::from_secs(570), async {
        loop {
            let response = request(
                client,
                crypto,
                provider,
                reqwest::Method::GET,
                "query/video_generation",
            )?
            .query(&[("task_id", task)])
            .send()
            .await
            .map_err(transport_error)?;
            check_status(&response)?;
            let value: Value = serde_json::from_slice(&read_bounded(response).await?)
                .map_err(|_| AttemptError::Retryable)?;
            check_minimax(&value)?;
            match value.get("status").and_then(Value::as_str) {
                Some("Success") => {
                    let file_id = value
                        .get("file_id")
                        .and_then(Value::as_str)
                        .ok_or(AttemptError::Retryable)?;
                    let response = request(
                        client,
                        crypto,
                        provider,
                        reqwest::Method::GET,
                        "files/retrieve",
                    )?
                    .query(&[("file_id", file_id)])
                    .send()
                    .await
                    .map_err(transport_error)?;
                    check_status(&response)?;
                    let value: Value = serde_json::from_slice(&read_bounded(response).await?)
                        .map_err(|_| AttemptError::Retryable)?;
                    check_minimax(&value)?;
                    let url = value
                        .pointer("/file/download_url")
                        .and_then(valid_url)
                        .ok_or(AttemptError::Retryable)?;
                    return media_data(vec![json!({"url": url, "format": "mp4"})]);
                }
                Some("Preparing" | "Queueing" | "Processing") => {
                    tokio::time::sleep(Duration::from_secs(2)).await
                }
                _ => return Err(AttemptError::Retryable),
            }
        }
    })
    .await;
    result
        .unwrap_or(Err(AttemptError::Retryable))
        .map_err(|_| AttemptError::Permanent(AppError::Routing))
}

async fn deepgram(
    client: &reqwest::Client,
    crypto: &Crypto,
    provider: &Provider,
    model: &str,
    body: &Value,
    kind: Kind,
    upload: Option<&Upload>,
) -> Result<Response, AttemptError> {
    if kind == Kind::Speech {
        let mut query = vec![("model", model.to_owned())];
        let (encoding, container) = match body
            .get("response_format")
            .and_then(Value::as_str)
            .unwrap_or("mp3")
        {
            "mp3" => ("mp3", None),
            "wav" => ("linear16", Some("wav")),
            "pcm" => ("linear16", Some("none")),
            "opus" => ("opus", Some("ogg")),
            "flac" => ("flac", None),
            _ => return Err(invalid("Unsupported Deepgram audio format")),
        };
        query.push(("encoding", encoding.into()));
        if let Some(container) = container {
            query.push(("container", container.into()));
        }
        let response = request(client, crypto, provider, reqwest::Method::POST, "speak")?
            .query(&query)
            .json(&json!({"text": text(body, "input")?}))
            .send()
            .await
            .map_err(transport_error)?;
        check_status(&response)?;
        return binary(response, audio_mime(body)).await;
    }
    if kind != Kind::Transcription {
        return Err(invalid("Unsupported Deepgram endpoint"));
    }
    let upload = upload.ok_or_else(|| invalid("Audio upload required"))?;
    let file = upload
        .iter()
        .find(|p| p.name == "file")
        .ok_or_else(|| invalid("Audio file required"))?;
    let mut query = vec![("model", model.to_owned())];
    for field in upload
        .iter()
        .filter(|p| p.filename.is_none() && p.name != "model" && p.name != "response_format")
    {
        let value =
            std::str::from_utf8(&field.bytes).map_err(|_| invalid("Text field must be UTF-8"))?;
        query.push((field.name.as_str(), value.to_owned()));
    }
    let response = request(client, crypto, provider, reqwest::Method::POST, "listen")?
        .query(&query)
        .header(
            header::CONTENT_TYPE,
            file.content_type
                .as_deref()
                .unwrap_or("application/octet-stream"),
        )
        .body(file.bytes.clone())
        .send()
        .await
        .map_err(transport_error)?;
    check_status(&response)?;
    let value: Value = serde_json::from_slice(&read_bounded(response).await?)
        .map_err(|_| AttemptError::Retryable)?;
    let text = value
        .pointer("/results/channels/0/alternatives/0/transcript")
        .and_then(Value::as_str)
        .ok_or(AttemptError::Retryable)?;
    transcription_result(body, text)
}
fn transcription_result(body: &Value, text: &str) -> Result<Response, AttemptError> {
    match body
        .get("response_format")
        .and_then(Value::as_str)
        .unwrap_or("json")
    {
        "json" => object_response(json!({"text": text})),
        "text" => {
            let mut response = Response::new(Body::from(text.to_owned()));
            response.headers_mut().insert(
                header::CONTENT_TYPE,
                header::HeaderValue::from_static("text/plain; charset=utf-8"),
            );
            Ok(response)
        }
        _ => Err(invalid(
            "This provider supports only json or text transcription responses",
        )),
    }
}

async fn elevenlabs(
    client: &reqwest::Client,
    crypto: &Crypto,
    provider: &Provider,
    model: &str,
    body: &Value,
    kind: Kind,
    upload: Option<&Upload>,
) -> Result<Response, AttemptError> {
    if kind == Kind::Music {
        let mut input = replace_model(body, model)?;
        let object = input.as_object_mut().unwrap();
        object.remove("model");
        object.remove("stream");
        object.remove("response_format");
        object.insert("model_id".into(), json!(model));
        let response = request(client, crypto, provider, reqwest::Method::POST, "music")?
            .json(&input)
            .send()
            .await
            .map_err(transport_error)?;
        check_status(&response)?;
        return binary(response, "audio/mpeg").await;
    }
    if kind == Kind::Speech {
        let voice = path_id(text(body, "voice")?)?;
        let format = match body
            .get("response_format")
            .and_then(Value::as_str)
            .unwrap_or("mp3")
        {
            "mp3" => "mp3_44100_128",
            "pcm" => "pcm_44100",
            "opus" => "opus_48000_128",
            _ => return Err(invalid("Unsupported ElevenLabs audio format")),
        };
        let mut input = replace_model(body, model)?;
        let object = input.as_object_mut().unwrap();
        object.remove("model");
        object.remove("voice");
        object.remove("response_format");
        object.remove("stream");
        let text = object
            .remove("input")
            .ok_or_else(|| invalid("input required"))?;
        object.insert("text".into(), text);
        object.insert("model_id".into(), json!(model));
        let response = request(
            client,
            crypto,
            provider,
            reqwest::Method::POST,
            &format!("text-to-speech/{voice}"),
        )?
        .query(&[("output_format", format)])
        .json(&input)
        .send()
        .await
        .map_err(transport_error)?;
        check_status(&response)?;
        return binary(response, audio_mime(body)).await;
    }
    if kind != Kind::Transcription {
        return Err(invalid("Unsupported ElevenLabs endpoint"));
    }
    let upload = upload.ok_or_else(|| invalid("Audio upload required"))?;
    let mut form = reqwest::multipart::Form::new().text("model_id", model.to_owned());
    for part in upload
        .iter()
        .filter(|p| p.name != "model" && p.name != "model_id" && p.name != "response_format")
    {
        let mut value = reqwest::multipart::Part::stream_with_length(
            reqwest::Body::from(part.bytes.clone()),
            part.bytes.len() as u64,
        );
        if let Some(filename) = &part.filename {
            value = value.file_name(filename.clone());
        }
        if let Some(mime) = &part.content_type {
            value = value
                .mime_str(mime)
                .map_err(|_| invalid("Invalid upload type"))?;
        }
        let name = if part.name == "language" {
            "language_code"
        } else {
            &part.name
        };
        form = form.part(name.to_owned(), value);
    }
    let response = request(
        client,
        crypto,
        provider,
        reqwest::Method::POST,
        "speech-to-text",
    )?
    .multipart(form)
    .send()
    .await
    .map_err(transport_error)?;
    check_status(&response)?;
    let value: Value = serde_json::from_slice(&read_bounded(response).await?)
        .map_err(|_| AttemptError::Retryable)?;
    let text = value
        .get("text")
        .and_then(Value::as_str)
        .ok_or(AttemptError::Retryable)?;
    if body
        .get("response_format")
        .and_then(Value::as_str)
        .unwrap_or("json")
        == "json"
    {
        return object_response(value);
    }
    transcription_result(body, text)
}

// A successful submission must not be retried even if its body is malformed.
// Transport failure may also mean an accepted task: fail closed, avoiding duplicate billing.
async fn submit_video(
    builder: reqwest::RequestBuilder,
    body: &Value,
) -> Result<Value, AttemptError> {
    let response = builder
        .json(body)
        .send()
        .await
        .map_err(|_| AttemptError::Permanent(AppError::Routing))?;
    check_status(&response)?;
    let bytes = read_bounded(response)
        .await
        .map_err(|_| AttemptError::Permanent(AppError::Routing))?;
    serde_json::from_slice(&bytes).map_err(|_| AttemptError::Permanent(AppError::Routing))
}

async fn xai_video(
    client: &reqwest::Client,
    crypto: &Crypto,
    provider: &Provider,
    model: &str,
    body: &Value,
) -> Result<Response, AttemptError> {
    let submitted = submit_video(
        request(
            client,
            crypto,
            provider,
            reqwest::Method::POST,
            "videos/generations",
        )?,
        &replace_model(body, model)?,
    )
    .await?;
    let task = submitted
        .get("request_id")
        .and_then(Value::as_str)
        .and_then(|id| path_id(id).ok())
        .ok_or_else(|| AttemptError::Permanent(AppError::Routing))?;
    let result = tokio::time::timeout(Duration::from_secs(570), async {
        loop {
            let response = request(
                client,
                crypto,
                provider,
                reqwest::Method::GET,
                &format!("videos/{task}"),
            )?
            .send()
            .await
            .map_err(transport_error)?;
            check_status(&response)?;
            let value: Value = serde_json::from_slice(&read_bounded(response).await?)
                .map_err(|_| AttemptError::Retryable)?;
            match value.get("status").and_then(Value::as_str) {
                Some("done") => {
                    let video = value
                        .get("video")
                        .filter(|v| v.get("url").and_then(valid_url).is_some())
                        .ok_or(AttemptError::Retryable)?;
                    return media_data(vec![video.clone()]);
                }
                Some("pending") => tokio::time::sleep(Duration::from_secs(2)).await,
                _ => return Err(AttemptError::Retryable),
            }
        }
    })
    .await;
    result
        .unwrap_or(Err(AttemptError::Retryable))
        .map_err(|_| AttemptError::Permanent(AppError::Routing))
}

pub(crate) fn parse_models(
    protocol: Protocol,
    value: &Value,
) -> Result<Vec<crate::db::connection_models::StoredModel>, AppError> {
    let items: Vec<&Value> = if protocol == Protocol::ElevenLabs {
        value
            .as_array()
            .ok_or_else(|| AppError::InvalidRequest("Provider did not return a model list".into()))?
            .iter()
            .collect()
    } else {
        let stt = value.get("stt").and_then(Value::as_array);
        let tts = value.get("tts").and_then(Value::as_array);
        if stt.is_none() && tts.is_none() {
            return Err(AppError::InvalidRequest(
                "Provider did not return a model list".into(),
            ));
        }
        stt.into_iter().chain(tts).flatten().collect()
    };
    let mut seen = std::collections::HashSet::new();
    Ok(items
        .into_iter()
        .filter_map(|item| {
            let id = item
                .get(if protocol == Protocol::ElevenLabs {
                    "model_id"
                } else {
                    "canonical_name"
                })
                .and_then(Value::as_str)?;
            if id.is_empty()
                || id.chars().count() > 256
                || id.chars().any(char::is_control)
                || !seen.insert(id.to_owned())
            {
                return None;
            }
            Some(crate::db::connection_models::StoredModel {
                id: id.into(),
                name: item
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or(id)
                    .into(),
                fields: vec![],
                free: false,
            })
        })
        .collect())
}

/// Advisory fallback catalogs for services without an OpenAI-shaped model list.
/// These are not proof of account/model access; operators can use any model in combos.
pub(crate) fn catalog(protocol: Protocol) -> &'static [&'static str] {
    match protocol {
        Protocol::Fal => &[
            "fal-ai/flux/dev",
            "fal-ai/wan/v2.2-a14b/text-to-video",
            "fal-ai/ace-step",
        ],
        Protocol::ElevenLabs => &[
            "scribe_v1",
            "scribe_v2",
            "music_v1",
            "music_v2",
            "music_v2_5",
        ],
        Protocol::MiniMax => &[
            "image-01",
            "MiniMax-Hailuo-2.3",
            "music-2.6",
            "music-3.0",
            "speech-2.8-hd",
            "speech-2.8-turbo",
            "asr-1.0",
        ],
        _ => &[],
    }
}
