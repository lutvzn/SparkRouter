use std::{collections::HashMap, ops::ControlFlow};

use axum::{http::StatusCode, response::Response};
use bytes::Bytes;
use futures_util::{Stream, StreamExt};
use serde_json::{Map, Value, json};

use super::sse::{SseDecoder, SseEvent};
use super::transport_error;
use super::{
    AttemptError, ResponseStream, check_sse, check_status, json_response, read_bounded,
    replace_model, request, streaming_response,
};
use crate::{crypto::Crypto, db::providers::Provider, error::AppError};

/// Anthropic requires `max_tokens`. Requests that omit both OpenAI budgets get
/// this ceiling instead of a silent 1024-token cut. 8192 matches the output
/// limit of current Claude models that reject a higher value with a permanent
/// 400. Set `max_tokens` or `max_completion_tokens` for a different ceiling.
const DEFAULT_MAX_TOKENS: u64 = 8192;

pub async fn call(
    client: &reqwest::Client,
    crypto: &Crypto,
    provider: &Provider,
    model: &str,
    body: &Value,
) -> Result<Response, AttemptError> {
    let translated = translate_request(body, model)?;
    let streaming = translated
        .get("stream")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let include_usage = body
        .pointer("/stream_options/include_usage")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let response = request(client, crypto, provider, reqwest::Method::POST, "messages")?
        .json(&translated)
        .send()
        .await
        .map_err(transport_error)?;
    check_status(&response)?;
    if streaming {
        check_sse(&response)?;
        let source = response
            .bytes_stream()
            .map(|chunk| chunk.map_err(transport_error));
        streaming_response(translate_stream(source, include_usage)).await
    } else {
        let bytes = read_bounded(response).await?;
        let message: Value = serde_json::from_slice(&bytes).map_err(|_| AttemptError::Retryable)?;
        if message.get("type").and_then(Value::as_str) == Some("error") {
            return Err(upstream_error(&message));
        }
        let translated = translate_response(&message)?;
        Ok(json_response(
            StatusCode::OK,
            Bytes::from(translated.to_string()),
        ))
    }
}

fn invalid(message: &str) -> AppError {
    AppError::InvalidRequest(message.into())
}

fn required_string<'a>(value: &'a Value, field: &str) -> Result<&'a str, AppError> {
    value
        .get(field)
        .and_then(Value::as_str)
        .ok_or_else(|| invalid("Missing or invalid message/tool field"))
}

/// Copy a client's prompt-cache breakpoint (`cache_control`, as OpenRouter and
/// LiteLLM accept it on OpenAI-format parts, messages, and tools) onto the
/// translated block. Anthropic caches only up to marked blocks, so dropping it
/// would silently disable prompt caching. The value is forwarded unchanged.
fn keep_cache_control(mut block: Value, source: &Value) -> Result<Value, AppError> {
    if let Some(marker) = source.get("cache_control").filter(|value| !value.is_null()) {
        if !marker.is_object() {
            return Err(invalid("cache_control must be an object"));
        }
        block["cache_control"] = marker.clone();
    }
    Ok(block)
}

/// Apply a message-level `cache_control` to the last block that message produced.
fn mark_last(parts: &mut [Value], message: &Value) -> Result<(), AppError> {
    if let Some(last) = parts.last_mut() {
        *last = keep_cache_control(std::mem::take(last), message)?;
    }
    Ok(())
}

fn content_parts(content: &Value, images: bool) -> Result<Vec<Value>, AppError> {
    if let Some(text) = content.as_str() {
        return Ok(vec![json!({"type":"text", "text":text})]);
    }
    let parts = content
        .as_array()
        .ok_or_else(|| invalid("Message content must be text or an array of supported parts"))?;
    parts
        .iter()
        .map(|part| {
            let block = match part.get("type").and_then(Value::as_str) {
                Some("text") => Ok(json!({"type":"text", "text":required_string(part, "text")?})),
                Some("image_url") if images => {
                    let image = part
                        .get("image_url")
                        .ok_or_else(|| invalid("Invalid image content"))?;
                    let url = required_string(image, "url")?;
                    let source = if let Some(data) = url.strip_prefix("data:") {
                        let (media, encoded) = data.split_once(";base64,").ok_or_else(|| {
                            invalid("Images must use a base64 data URL or HTTP(S) URL")
                        })?;
                        if !matches!(
                            media,
                            "image/jpeg" | "image/png" | "image/gif" | "image/webp"
                        ) {
                            return Err(invalid("Unsupported image media type"));
                        }
                        use base64::Engine;
                        base64::engine::general_purpose::STANDARD
                            .decode(encoded)
                            .map_err(|_| invalid("Invalid base64 image"))?;
                        json!({"type":"base64", "media_type":media, "data":encoded})
                    } else {
                        let parsed =
                            reqwest::Url::parse(url).map_err(|_| invalid("Invalid image URL"))?;
                        if !matches!(parsed.scheme(), "http" | "https")
                            || !parsed.username().is_empty()
                            || parsed.password().is_some()
                        {
                            return Err(invalid(
                                "Images must use a base64 data URL or HTTP(S) URL",
                            ));
                        }
                        json!({"type":"url", "url":url})
                    };
                    // OpenAI's image detail hint has no Anthropic equivalent.
                    Ok(json!({"type":"image", "source":source}))
                }
                _ => Err(invalid("Unsupported message content part for Anthropic")),
            }?;
            keep_cache_control(block, part)
        })
        .collect()
}

fn translate_request(body: &Value, model: &str) -> Result<Value, AppError> {
    let mut translated = replace_model(body, model)?;
    let output = translated
        .as_object_mut()
        .expect("replace_model returns an object");
    if body
        .get("n")
        .is_some_and(|n| !n.is_null() && n.as_u64() != Some(1))
    {
        return Err(invalid("Anthropic supports only n=1"));
    }
    // These change generation semantics and cannot be faithfully translated.
    // Bookkeeping fields (user/store/metadata) and the image detail hint are
    // intentionally omitted; unknown fields survive for Anthropic extensions.
    for field in [
        "functions",
        "function_call",
        "audio",
        "prediction",
        "reasoning_effort",
        "seed",
        "logit_bias",
    ] {
        if body.get(field).is_some_and(|value| !value.is_null()) {
            return Err(invalid(
                "Request contains an unsupported OpenAI option for Anthropic",
            ));
        }
    }
    for field in ["frequency_penalty", "presence_penalty"] {
        if body
            .get(field)
            .is_some_and(|value| !value.is_null() && value.as_f64() != Some(0.0))
        {
            return Err(invalid(
                "Anthropic does not support frequency or presence penalties",
            ));
        }
    }
    if body
        .get("logprobs")
        .is_some_and(|value| !value.is_null() && value != false)
        || body
            .get("top_logprobs")
            .is_some_and(|value| !value.is_null() && value.as_u64() != Some(0))
    {
        return Err(invalid("Anthropic does not support log probabilities"));
    }
    if body.get("response_format").is_some_and(|value| {
        !value.is_null() && value.get("type").and_then(Value::as_str) != Some("text")
    }) {
        return Err(invalid(
            "Anthropic adapter does not support response_format other than text",
        ));
    }
    if body
        .get("modalities")
        .is_some_and(|value| !value.is_null() && value != &json!(["text"]))
    {
        return Err(invalid("Anthropic adapter supports only text output"));
    }
    if body
        .get("service_tier")
        .is_some_and(|value| !value.is_null() && value.as_str() != Some("auto"))
    {
        return Err(invalid("Unsupported service_tier for Anthropic"));
    }
    if body.get("stream").is_some_and(|value| !value.is_boolean()) {
        return Err(invalid("stream must be a boolean"));
    }
    for field in [
        "n",
        "user",
        "store",
        "metadata",
        "functions",
        "function_call",
        "audio",
        "prediction",
        "reasoning_effort",
        "seed",
        "logit_bias",
        "frequency_penalty",
        "presence_penalty",
        "logprobs",
        "top_logprobs",
        "response_format",
        "modalities",
        "max_completion_tokens",
        "stream_options",
        "parallel_tool_calls",
        "stop",
    ] {
        output.remove(field);
    }
    let max_tokens = body
        .get("max_completion_tokens")
        .filter(|value| !value.is_null())
        .or_else(|| body.get("max_tokens").filter(|value| !value.is_null()))
        .cloned()
        .unwrap_or(json!(DEFAULT_MAX_TOKENS));
    if !max_tokens.as_u64().is_some_and(|value| value > 0) {
        return Err(invalid("max_tokens must be a positive integer"));
    }
    output.insert("max_tokens".into(), max_tokens);
    if let Some(stop) = body.get("stop").filter(|value| !value.is_null()) {
        let sequences = if stop.is_string() {
            vec![stop.clone()]
        } else {
            stop.as_array()
                .filter(|values| values.iter().all(Value::is_string))
                .cloned()
                .ok_or_else(|| invalid("stop must be a string or string array"))?
        };
        output.insert("stop_sequences".into(), Value::Array(sequences));
    }

    let messages = body
        .get("messages")
        .and_then(Value::as_array)
        .ok_or_else(|| invalid("messages must be an array"))?;
    let mut system = Vec::new();
    let mut converted: Vec<Value> = Vec::new();
    for message in messages {
        let role = required_string(message, "role")?;
        if message.get("name").is_some_and(|value| !value.is_null()) {
            return Err(invalid(
                "Named messages are not supported by the Anthropic adapter",
            ));
        }
        if message
            .get("function_call")
            .is_some_and(|value| !value.is_null())
        {
            return Err(invalid(
                "Legacy function_call messages are not supported; use tool_calls",
            ));
        }
        if message
            .get("tool_calls")
            .is_some_and(|value| !value.is_null())
            && role != "assistant"
        {
            return Err(invalid("Only assistant messages may contain tool_calls"));
        }
        if message.get("refusal").is_some_and(|value| !value.is_null()) {
            return Err(invalid(
                "Refusal message fields are not supported by the Anthropic adapter",
            ));
        }
        let content = message.get("content").unwrap_or(&Value::Null);
        if role == "system" {
            let mut parts = content_parts(content, false)?;
            mark_last(&mut parts, message)?;
            system.extend(parts);
            continue;
        }
        let (role, mut parts) = match role {
            "user" => ("user", content_parts(content, true)?),
            "assistant" => (
                "assistant",
                if content.is_null() {
                    Vec::new()
                } else {
                    content_parts(content, false)?
                },
            ),
            "tool" => (
                "user",
                vec![json!({
                    "type":"tool_result", "tool_use_id":required_string(message, "tool_call_id")?,
                    "content":content_parts(content, true)?
                })],
            ),
            _ => return Err(invalid("Unsupported message role for Anthropic")),
        };
        if let Some(calls) = message.get("tool_calls").filter(|value| !value.is_null()) {
            if role != "assistant" {
                return Err(invalid("Only assistant messages may contain tool_calls"));
            }
            for call in calls
                .as_array()
                .ok_or_else(|| invalid("tool_calls must be an array"))?
            {
                if required_string(call, "type")? != "function" {
                    return Err(invalid("Unsupported tool call type"));
                }
                let function = call
                    .get("function")
                    .ok_or_else(|| invalid("Missing tool function"))?;
                let input: Value = serde_json::from_str(required_string(function, "arguments")?)
                    .map_err(|_| invalid("Tool arguments must be a JSON object"))?;
                if !input.is_object() {
                    return Err(invalid("Tool arguments must be a JSON object"));
                }
                parts.push(json!({"type":"tool_use", "id":required_string(call, "id")?, "name":required_string(function, "name")?, "input":input}));
            }
        }
        mark_last(&mut parts, message)?;
        if let Some(last) = converted.last_mut().filter(|last| last["role"] == role) {
            last["content"]
                .as_array_mut()
                .expect("content array")
                .extend(parts);
        } else {
            converted.push(json!({"role":role, "content":parts}));
        }
    }
    if converted.is_empty() {
        return Err(invalid("At least one non-system message is required"));
    }
    output.insert("messages".into(), Value::Array(converted));
    if !system.is_empty() {
        if body.get("system").is_some_and(|value| !value.is_null()) {
            return Err(invalid(
                "Use system messages or the system extension, not both",
            ));
        }
        output.insert("system".into(), Value::Array(system));
    }
    if let Some(tools) = body.get("tools").filter(|value| !value.is_null()) {
        let mut converted = Vec::new();
        for tool in tools
            .as_array()
            .ok_or_else(|| invalid("tools must be an array"))?
        {
            if required_string(tool, "type")? != "function" {
                return Err(invalid("Only function tools are supported"));
            }
            let function = tool
                .get("function")
                .ok_or_else(|| invalid("Missing tool function"))?;
            if function.get("strict").and_then(Value::as_bool) == Some(true) {
                return Err(invalid(
                    "Strict function schemas are not supported by the Anthropic adapter",
                ));
            }
            let mut target = Map::new();
            target.insert("name".into(), json!(required_string(function, "name")?));
            let schema = function
                .get("parameters")
                .cloned()
                .unwrap_or(json!({"type":"object", "properties":{}}));
            if !schema.is_object() {
                return Err(invalid("Tool parameters must be an object"));
            }
            target.insert("input_schema".into(), schema);
            if let Some(description) = function.get("description") {
                target.insert("description".into(), description.clone());
            }
            converted.push(keep_cache_control(Value::Object(target), tool)?);
        }
        output.insert("tools".into(), Value::Array(converted));
    }
    if let Some(choice) = body.get("tool_choice").filter(|value| !value.is_null()) {
        let choice = match choice.as_str() {
            Some("auto") => json!({"type":"auto"}),
            Some("required") => json!({"type":"any"}),
            Some("none") => json!({"type":"none"}),
            None if choice.get("type").and_then(Value::as_str) == Some("function") => {
                json!({"type":"tool", "name":required_string(&choice["function"], "name")?})
            }
            _ => return Err(invalid("Unsupported tool_choice")),
        };
        output.insert("tool_choice".into(), choice);
    }
    if let Some(parallel) = body
        .get("parallel_tool_calls")
        .filter(|value| !value.is_null())
    {
        let parallel = parallel
            .as_bool()
            .ok_or_else(|| invalid("parallel_tool_calls must be a boolean"))?;
        if !parallel
            && output
                .get("tool_choice")
                .and_then(|choice| choice.get("type"))
                .and_then(Value::as_str)
                != Some("none")
        {
            let choice = output
                .entry("tool_choice")
                .or_insert(json!({"type":"auto"}));
            choice["disable_parallel_tool_use"] = json!(true);
        }
    }
    // OpenAI accepts temperatures up to 2; Anthropic's ceiling is 1 and a
    // higher value is a permanent 400 that would stop a mixed combo. Clamp
    // rather than reject so the same request stays valid across steps.
    if output
        .get("temperature")
        .and_then(Value::as_f64)
        .is_some_and(|temperature| temperature > 1.0)
    {
        output.insert("temperature".into(), json!(1.0));
    }
    // Anthropic expects optional parameters to be omitted rather than sent as
    // explicit nulls. Strip the known optional fields when null; unknown
    // extension fields (null or not) pass through untouched.
    for field in [
        "tools",
        "tool_choice",
        "temperature",
        "top_p",
        "top_k",
        "system",
        "stop_sequences",
        "service_tier",
    ] {
        if output.get(field) == Some(&Value::Null) {
            output.remove(field);
        }
    }
    Ok(translated)
}

fn finish_reason(reason: Option<&str>) -> &'static str {
    match reason {
        Some("max_tokens") => "length",
        Some("tool_use") => "tool_calls",
        Some("refusal") => "content_filter",
        _ => "stop",
    }
}

fn usage_with_cache(input: Option<u64>, output: Option<u64>, cached: Option<u64>) -> Value {
    let mut value = json!({});
    if let Some(input) = input {
        value["prompt_tokens"] = json!(input);
    }
    if let Some(output) = output {
        value["completion_tokens"] = json!(output);
    }
    if let Some((input, output)) = input.zip(output) {
        value["total_tokens"] = json!(input.saturating_add(output));
    }
    if let Some(cached) = cached {
        value["prompt_tokens_details"] = json!({"cached_tokens":cached});
    }
    value
}

fn cache_read_tokens(value: &Value) -> Option<u64> {
    value.get("cache_read_input_tokens").and_then(Value::as_u64)
}

fn input_tokens(value: &Value) -> Option<u64> {
    let mut input = value.get("input_tokens")?.as_u64()?;
    // Cache counters are optional, but a missing base count is not zero.
    for key in ["cache_creation_input_tokens", "cache_read_input_tokens"] {
        if let Some(tokens) = value.get(key) {
            input = input.checked_add(tokens.as_u64()?)?;
        }
    }
    Some(input)
}

fn translate_response(message: &Value) -> Result<Value, AttemptError> {
    let id = message
        .get("id")
        .and_then(Value::as_str)
        .ok_or(AttemptError::Retryable)?;
    let model = message
        .get("model")
        .and_then(Value::as_str)
        .ok_or(AttemptError::Retryable)?;
    let content = message
        .get("content")
        .and_then(Value::as_array)
        .ok_or(AttemptError::Retryable)?;
    let mut text = String::new();
    let mut calls = Vec::new();
    for block in content {
        match block.get("type").and_then(Value::as_str) {
            Some("text") => text.push_str(
                block
                    .get("text")
                    .and_then(Value::as_str)
                    .ok_or(AttemptError::Retryable)?,
            ),
            Some("tool_use") => {
                let id = block
                    .get("id")
                    .and_then(Value::as_str)
                    .ok_or(AttemptError::Retryable)?;
                let name = block
                    .get("name")
                    .and_then(Value::as_str)
                    .ok_or(AttemptError::Retryable)?;
                let input = block
                    .get("input")
                    .filter(|value| value.is_object())
                    .ok_or(AttemptError::Retryable)?;
                calls.push(json!({"id":id, "type":"function", "function":{"name":name, "arguments":input.to_string()}}));
            }
            // Thinking, server-tool and other newer block types are auxiliary
            // Anthropic data, not answer text. A block without a type is corrupt.
            Some(_) => {}
            None => return Err(AttemptError::Retryable),
        }
    }
    let mut assistant = json!({"role":"assistant", "content":if text.is_empty() && !calls.is_empty() { Value::Null } else { json!(text) }});
    if !calls.is_empty() {
        assistant["tool_calls"] = Value::Array(calls);
    }
    let tokens = &message["usage"];
    Ok(json!({
        "id":id, "object":"chat.completion", "created":created(), "model":model,
        "choices":[{"index":0, "message":assistant, "finish_reason":finish_reason(message.get("stop_reason").and_then(Value::as_str))}],
        "usage":usage_with_cache(input_tokens(tokens), tokens.get("output_tokens").and_then(Value::as_u64), cache_read_tokens(tokens))
    }))
}

fn created() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|value| value.as_secs())
        .unwrap_or(0)
}

fn upstream_error(event: &Value) -> AttemptError {
    let status = match event.pointer("/error/type").and_then(Value::as_str) {
        Some("invalid_request_error") => StatusCode::BAD_REQUEST,
        Some("authentication_error") => StatusCode::UNAUTHORIZED,
        Some("permission_error") => StatusCode::FORBIDDEN,
        Some("not_found_error") => StatusCode::NOT_FOUND,
        Some("request_too_large") => StatusCode::PAYLOAD_TOO_LARGE,
        _ => return AttemptError::Retryable,
    };
    if crate::routing::combo::is_retryable_status(status) {
        AttemptError::Retryable
    } else {
        AttemptError::Permanent(AppError::Upstream(status))
    }
}

#[derive(Clone, Copy)]
enum Block {
    Text,
    Tool {
        index: usize,
        has_arguments: bool,
    },
    /// Thinking or newer block types: consumed, never exposed as answer text.
    Auxiliary,
}

struct StreamState {
    id: String,
    model: String,
    created: u64,
    started: bool,
    finished: bool,
    stopped: bool,
    input_tokens: Option<u64>,
    output_tokens: Option<u64>,
    cached_tokens: Option<u64>,
    tool_count: usize,
    blocks: HashMap<u64, Block>,
    include_usage: bool,
}

impl StreamState {
    fn new(include_usage: bool) -> Self {
        Self {
            id: String::new(),
            model: String::new(),
            created: created(),
            started: false,
            finished: false,
            stopped: false,
            input_tokens: None,
            output_tokens: None,
            cached_tokens: None,
            tool_count: 0,
            blocks: HashMap::new(),
            include_usage,
        }
    }

    fn chunk(&self, delta: Value, finish: Option<&str>) -> Bytes {
        let mut value = json!({"id":self.id, "object":"chat.completion.chunk", "created":self.created, "model":self.model,
            "choices":[{"index":0, "delta":delta, "finish_reason":finish}]});
        if self.include_usage {
            value["usage"] = Value::Null;
        }
        sse_json(value)
    }

    fn event(&mut self, event: SseEvent) -> Result<Vec<Bytes>, AttemptError> {
        let value: Value =
            serde_json::from_str(&event.data).map_err(|_| AttemptError::Retryable)?;
        let kind = value
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or(&event.name);
        if event.name == "error" || kind == "error" {
            return Err(upstream_error(&value));
        }
        if kind == "ping" {
            return Ok(Vec::new());
        }
        let mut output = Vec::new();
        if kind == "message_start" {
            if self.started {
                return Err(AttemptError::Retryable);
            }
            let message = &value["message"];
            self.id = message
                .get("id")
                .and_then(Value::as_str)
                .ok_or(AttemptError::Retryable)?
                .to_owned();
            self.model = message
                .get("model")
                .and_then(Value::as_str)
                .ok_or(AttemptError::Retryable)?
                .to_owned();
            self.input_tokens = input_tokens(&message["usage"]);
            self.cached_tokens = cache_read_tokens(&message["usage"]);
            self.output_tokens = message
                .pointer("/usage/output_tokens")
                .and_then(Value::as_u64);
            self.started = true;
            output.push(self.chunk(json!({"role":"assistant", "content":""}), None));
            return Ok(output);
        }
        if !self.started || self.stopped {
            return Err(AttemptError::Retryable);
        }
        match kind {
            "content_block_start" => {
                if self.finished {
                    return Err(AttemptError::Retryable);
                }
                let index = value
                    .get("index")
                    .and_then(Value::as_u64)
                    .ok_or(AttemptError::Retryable)?;
                if self.blocks.contains_key(&index) {
                    return Err(AttemptError::Retryable);
                }
                let block = &value["content_block"];
                let block = match block.get("type").and_then(Value::as_str) {
                    Some("text") => {
                        let text = block
                            .get("text")
                            .and_then(Value::as_str)
                            .ok_or(AttemptError::Retryable)?;
                        if !text.is_empty() {
                            output.push(self.chunk(json!({"content":text}), None));
                        }
                        Block::Text
                    }
                    Some("tool_use") => {
                        let tool_index = self.tool_count;
                        self.tool_count += 1;
                        let id = block
                            .get("id")
                            .and_then(Value::as_str)
                            .ok_or(AttemptError::Retryable)?;
                        let name = block
                            .get("name")
                            .and_then(Value::as_str)
                            .ok_or(AttemptError::Retryable)?;
                        let input = block
                            .get("input")
                            .and_then(Value::as_object)
                            .ok_or(AttemptError::Retryable)?;
                        let arguments = if input.is_empty() {
                            String::new()
                        } else {
                            Value::Object(input.clone()).to_string()
                        };
                        output.push(self.chunk(json!({"tool_calls":[{"index":tool_index, "id":id, "type":"function", "function":{"name":name, "arguments":arguments}}]}), None));
                        Block::Tool {
                            index: tool_index,
                            has_arguments: !arguments.is_empty(),
                        }
                    }
                    Some(_) => Block::Auxiliary,
                    None => return Err(AttemptError::Retryable),
                };
                self.blocks.insert(index, block);
            }
            "content_block_delta" => {
                if self.finished {
                    return Err(AttemptError::Retryable);
                }
                let index = value
                    .get("index")
                    .and_then(Value::as_u64)
                    .ok_or(AttemptError::Retryable)?;
                let block = *self.blocks.get(&index).ok_or(AttemptError::Retryable)?;
                let delta = &value["delta"];
                match (block, delta.get("type").and_then(Value::as_str)) {
                    (Block::Text, Some("text_delta")) => {
                        let text = delta
                            .get("text")
                            .and_then(Value::as_str)
                            .ok_or(AttemptError::Retryable)?;
                        output.push(self.chunk(json!({"content":text}), None));
                    }
                    (
                        Block::Tool {
                            index: tool_index,
                            has_arguments,
                        },
                        Some("input_json_delta"),
                    ) => {
                        let arguments = delta
                            .get("partial_json")
                            .and_then(Value::as_str)
                            .ok_or(AttemptError::Retryable)?;
                        self.blocks.insert(
                            index,
                            Block::Tool {
                                index: tool_index,
                                has_arguments: has_arguments || !arguments.is_empty(),
                            },
                        );
                        output.push(self.chunk(json!({"tool_calls":[{"index":tool_index, "function":{"arguments":arguments}}]}), None));
                    }
                    // Citation metadata accompanies text; it is not answer text.
                    (Block::Text, Some("citations_delta")) | (Block::Auxiliary, Some(_)) => {}
                    _ => return Err(AttemptError::Retryable),
                }
            }
            "content_block_stop" => {
                let index = value
                    .get("index")
                    .and_then(Value::as_u64)
                    .ok_or(AttemptError::Retryable)?;
                let block = self.blocks.remove(&index).ok_or(AttemptError::Retryable)?;
                if let Block::Tool {
                    index,
                    has_arguments: false,
                } = block
                {
                    output.push(self.chunk(
                        json!({"tool_calls":[{"index":index, "function":{"arguments":"{}"}}]}),
                        None,
                    ));
                }
            }
            "message_delta" => {
                if let Some(tokens) = value
                    .pointer("/usage/cache_read_input_tokens")
                    .and_then(Value::as_u64)
                {
                    self.cached_tokens = Some(tokens);
                }
                if let Some(tokens) = value
                    .pointer("/usage/output_tokens")
                    .and_then(Value::as_u64)
                {
                    self.output_tokens = Some(tokens);
                }
                if let Some(reason) = value.pointer("/delta/stop_reason").and_then(Value::as_str) {
                    if self.finished || !self.blocks.is_empty() {
                        return Err(AttemptError::Retryable);
                    }
                    self.finished = true;
                    output.push(self.chunk(json!({}), Some(finish_reason(Some(reason)))));
                }
            }
            "message_stop" => {
                if !self.finished || !self.blocks.is_empty() {
                    return Err(AttemptError::Retryable);
                }
                self.stopped = true;
                if self.include_usage {
                    output.push(sse_json(json!({"id":self.id, "object":"chat.completion.chunk", "created":self.created, "model":self.model,
                        "choices":[], "usage":usage_with_cache(self.input_tokens, self.output_tokens, self.cached_tokens)})));
                }
                output.push(Bytes::from_static(b"data: [DONE]\n\n"));
            }
            // Anthropic may add event kinds. Ignore unknown events, but never
            // treat them as a successful completion or a commitment boundary.
            _ => {}
        }
        Ok(output)
    }
}

fn sse_json(value: Value) -> Bytes {
    Bytes::from(format!("data: {value}\n\n"))
}

fn translate_stream<S>(source: S, include_usage: bool) -> ResponseStream
where
    S: Stream<Item = Result<Bytes, AttemptError>> + Send + 'static,
{
    Box::pin(async_stream::try_stream! {
        futures_util::pin_mut!(source);
        let mut decoder = SseDecoder::default();
        let mut state = StreamState::new(include_usage);
        while let Some(bytes) = source.next().await {
            let bytes = bytes?;
            // Output of the events before a failure is yielded first, as the
            // failure would surface after it event by event.
            let mut output = Vec::new();
            let mut failed = None;
            let decoded = decoder.push_chunk(&bytes, |event| match state.event(event) {
                Ok(chunks) => {
                    output.extend(chunks);
                    if state.stopped { ControlFlow::Break(()) } else { ControlFlow::Continue(()) }
                }
                Err(error) => {
                    failed = Some(error);
                    ControlFlow::Break(())
                }
            });
            for chunk in output { yield chunk; }
            if let Some(error) = failed { Err(error)?; }
            decoded?;
            if state.stopped { return; }
        }
        // An HTTP EOF is not an Anthropic message_stop; signal truncation.
        Err(AttemptError::Retryable)?;
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn omitted_token_budget_uses_the_documented_default() {
        let translated = translate_request(
            &json!({"model":"combo", "messages":[{"role":"user", "content":"hi"}]}),
            "claude",
        )
        .unwrap();
        assert_eq!(translated["max_tokens"], DEFAULT_MAX_TOKENS);
        assert_eq!(DEFAULT_MAX_TOKENS, 8192);
    }

    #[test]
    fn translates_system_images_tools_and_preserves_extensions() {
        let request = json!({"model":"combo", "max_completion_tokens":300, "stop":"END", "n":1,
            "custom_extension":{"enabled":true}, "parallel_tool_calls":false,
            "messages":[
                {"role":"system", "content":"Be helpful"},
                {"role":"user", "content":[{"type":"text", "text":"What?"}, {"type":"image_url", "image_url":{"url":"data:image/png;base64,aGk="}}]},
                {"role":"assistant", "content":null, "tool_calls":[{"id":"call_1", "type":"function", "function":{"name":"weather", "arguments":"{\"city\":\"Paris\"}"}}]},
                {"role":"tool", "tool_call_id":"call_1", "content":"Sunny"}
            ], "tools":[{"type":"function", "function":{"name":"weather", "parameters":{"type":"object"}}}], "tool_choice":"required"});
        let translated = translate_request(&request, "claude").unwrap();
        assert_eq!(translated["model"], "claude");
        assert_eq!(translated["max_tokens"], 300);
        assert!(translated.get("max_completion_tokens").is_none());
        assert_eq!(translated["stop_sequences"], json!(["END"]));
        assert_eq!(translated["system"][0]["text"], "Be helpful");
        assert_eq!(
            translated["messages"][0]["content"][1]["source"]["data"],
            "aGk="
        );
        assert_eq!(
            translated["messages"][1]["content"][0]["input"],
            json!({"city":"Paris"})
        );
        assert_eq!(
            translated["messages"][2]["content"][0]["tool_use_id"],
            "call_1"
        );
        assert_eq!(
            translated["tool_choice"],
            json!({"type":"any", "disable_parallel_tool_use":true})
        );
        assert_eq!(translated["custom_extension"], request["custom_extension"]);
    }

    #[test]
    fn keeps_prompt_cache_breakpoints_on_parts_messages_and_tools() {
        let hour = json!({"type":"ephemeral", "ttl":"1h"});
        let request = json!({"messages":[
            {"role":"system", "content":[{"type":"text", "text":"Rules", "cache_control":{"type":"ephemeral"}}]},
            {"role":"system", "content":"More rules", "cache_control":hour},
            {"role":"user", "content":[{"type":"text", "text":"Long document"}, {"type":"image_url", "image_url":{"url":"https://example.test/a.png"}, "cache_control":{"type":"ephemeral"}}]},
            {"role":"assistant", "content":"ok", "tool_calls":[{"id":"call_1", "type":"function", "function":{"name":"look", "arguments":"{}"}}], "cache_control":{"type":"ephemeral"}},
            {"role":"tool", "tool_call_id":"call_1", "content":"result", "cache_control":{"type":"ephemeral"}},
            {"role":"user", "content":"Question"}
        ], "tools":[
            {"type":"function", "function":{"name":"look"}},
            {"type":"function", "function":{"name":"read"}, "cache_control":{"type":"ephemeral"}}
        ]});
        let translated = translate_request(&request, "claude").unwrap();
        let ephemeral = json!({"type":"ephemeral"});
        assert_eq!(translated["system"][0]["cache_control"], ephemeral);
        assert_eq!(translated["system"][1]["cache_control"], hour);
        let messages = &translated["messages"];
        assert!(messages[0]["content"][0].get("cache_control").is_none());
        assert_eq!(messages[0]["content"][1]["type"], "image");
        assert_eq!(messages[0]["content"][1]["cache_control"], ephemeral);
        // A message-level marker lands on that message's last block.
        assert!(messages[1]["content"][0].get("cache_control").is_none());
        assert_eq!(messages[1]["content"][1]["type"], "tool_use");
        assert_eq!(messages[1]["content"][1]["cache_control"], ephemeral);
        assert_eq!(messages[2]["content"][0]["type"], "tool_result");
        assert_eq!(messages[2]["content"][0]["cache_control"], ephemeral);
        assert!(messages[2]["content"][1].get("cache_control").is_none());
        assert!(translated["tools"][0].get("cache_control").is_none());
        assert_eq!(translated["tools"][1]["cache_control"], ephemeral);
        // Unmarked requests gain no breakpoints.
        let plain = translate_request(
            &json!({"messages":[{"role":"user", "content":"hi"}]}),
            "claude",
        )
        .unwrap();
        assert!(!plain.to_string().contains("cache_control"));
        for marker in [json!("ephemeral"), json!(true)] {
            let body = json!({"messages":[{"role":"user", "content":[{"type":"text", "text":"x", "cache_control":marker}]}]});
            assert!(translate_request(&body, "claude").is_err());
        }
    }

    #[test]
    fn strips_null_optional_fields_but_keeps_values_and_extensions() {
        let request = json!({"messages":[{"role":"user", "content":"hi"}],
            "tools":null, "tool_choice":null, "temperature":null, "top_p":null, "top_k":null,
            "system":null, "stop_sequences":null, "service_tier":null, "x_extension":null});
        let translated = translate_request(&request, "claude").unwrap();
        for field in [
            "tools",
            "tool_choice",
            "temperature",
            "top_p",
            "top_k",
            "system",
            "stop_sequences",
            "service_tier",
        ] {
            assert!(
                translated.get(field).is_none(),
                "{field} should be stripped when null"
            );
        }
        // Unknown extension fields survive even when null.
        assert_eq!(translated.get("x_extension"), Some(&Value::Null));
        let kept = translate_request(
            &json!({"messages":[{"role":"user", "content":"hi"}], "temperature":0.5, "top_k":40}),
            "claude",
        )
        .unwrap();
        assert_eq!(kept["temperature"], 0.5);
        assert_eq!(kept["top_k"], 40);
    }

    #[test]
    fn clamps_openai_temperatures_above_anthropics_ceiling() {
        let translate = |temperature: Value| {
            translate_request(
                &json!({"messages":[{"role":"user", "content":"hi"}], "temperature":temperature}),
                "claude",
            )
            .unwrap()["temperature"]
                .clone()
        };
        assert_eq!(translate(json!(1.7)), 1.0);
        assert_eq!(translate(json!(2)), 1.0);
        assert_eq!(translate(json!(1)), 1);
        assert_eq!(translate(json!(0.2)), 0.2);
    }

    #[test]
    fn rejects_unsupported_semantics() {
        for extra in [
            json!({"n":2}),
            json!({"response_format":{"type":"json_object"}}),
            json!({"logprobs":true}),
        ] {
            let mut body = json!({"messages":[{"role":"user", "content":"hello"}]});
            body.as_object_mut()
                .unwrap()
                .extend(extra.as_object().unwrap().clone());
            assert!(translate_request(&body, "claude").is_err());
        }
        for message in [
            json!({"role":"developer", "content":"hello"}),
            json!({"role":"user", "content":[{"type":"input_audio", "data":"x"}]}),
        ] {
            assert!(translate_request(&json!({"messages":[message]}), "claude").is_err());
        }
    }

    #[test]
    fn converts_text_tool_calls_finish_and_cache_usage() {
        let value = translate_response(&json!({"id":"msg_1", "model":"claude", "content":[
            {"type":"text", "text":"Looking up"}, {"type":"tool_use", "id":"tool_1", "name":"weather", "input":{"city":"Paris"}}
        ], "stop_reason":"tool_use", "usage":{"input_tokens":2, "cache_read_input_tokens":8, "output_tokens":3}})).unwrap();
        assert_eq!(value["choices"][0]["message"]["content"], "Looking up");
        assert_eq!(value["choices"][0]["finish_reason"], "tool_calls");
        assert_eq!(
            value["choices"][0]["message"]["tool_calls"][0]["function"]["arguments"],
            "{\"city\":\"Paris\"}"
        );
        assert_eq!(value["usage"]["total_tokens"], 13);
        assert_eq!(value["usage"]["prompt_tokens_details"]["cached_tokens"], 8);
        assert_eq!(value["usage"]["prompt_tokens"], 10);
    }

    #[test]
    fn cache_usage_preserves_zero_and_absence() {
        let response = |usage| {
            translate_response(&json!({"id":"m", "model":"c", "content":[], "usage":usage}))
                .unwrap()
        };
        assert_eq!(
            response(json!({"input_tokens":10,"cache_read_input_tokens":0,"output_tokens":3}))["usage"]
                ["prompt_tokens_details"]["cached_tokens"],
            0
        );
        assert!(
            response(json!({"input_tokens":10,"output_tokens":3}))["usage"]
                .get("prompt_tokens_details")
                .is_none()
        );
    }

    #[test]
    fn missing_or_invalid_token_counts_are_not_reported_as_zero() {
        for (tokens, expected) in [
            (Value::Null, json!({})),
            (json!({}), json!({})),
            (json!({"input_tokens":4}), json!({"prompt_tokens":4})),
            (json!({"output_tokens":2}), json!({"completion_tokens":2})),
            (
                json!({"input_tokens":0,"output_tokens":0}),
                json!({"prompt_tokens":0,"completion_tokens":0,"total_tokens":0}),
            ),
            (
                json!({"cache_read_input_tokens":3}),
                json!({"prompt_tokens_details":{"cached_tokens":3}}),
            ),
            (json!({"input_tokens":-1,"output_tokens":"2"}), json!({})),
            (
                json!({"input_tokens":4,"cache_creation_input_tokens":-1}),
                json!({}),
            ),
            (
                json!({"input_tokens":u64::MAX,"cache_creation_input_tokens":1}),
                json!({}),
            ),
            (
                json!({"input_tokens":4,"cache_creation_input_tokens":3,"cache_read_input_tokens":2,"output_tokens":1}),
                json!({"prompt_tokens":9,"completion_tokens":1,"total_tokens":10,"prompt_tokens_details":{"cached_tokens":2}}),
            ),
        ] {
            let translated =
                translate_response(&json!({"id":"m","model":"claude","content":[],"usage":tokens}))
                    .unwrap();
            assert_eq!(translated["usage"], expected, "upstream usage: {tokens}");
        }
        let absent = translate_response(&json!({"id":"m","model":"claude","content":[]})).unwrap();
        assert_eq!(absent["usage"], json!({}));
    }

    #[tokio::test]
    async fn streamed_usage_preserves_missing_partial_and_zero_counts() {
        for (start, delta, expected) in [
            (Value::Null, Value::Null, json!({})),
            (
                json!({"input_tokens":4}),
                json!({"output_tokens":2}),
                json!({"prompt_tokens":4,"completion_tokens":2,"total_tokens":6}),
            ),
            (
                Value::Null,
                json!({"output_tokens":0}),
                json!({"completion_tokens":0}),
            ),
            (
                json!({"input_tokens":0,"output_tokens":0}),
                json!({}),
                json!({"prompt_tokens":0,"completion_tokens":0,"total_tokens":0}),
            ),
        ] {
            let wire: String = [
                json!({"type":"message_start","message":{"id":"m","model":"claude","usage":start}}),
                json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":delta}),
                json!({"type":"message_stop"}),
            ]
            .into_iter()
            .map(event)
            .collect();
            let chunks = translate_stream(
                futures_util::stream::iter(vec![Ok(Bytes::from(wire))]),
                true,
            )
            .collect::<Vec<_>>()
            .await
            .into_iter()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
            assert_eq!(chunks.last().unwrap().as_ref(), b"data: [DONE]\n\n");
            let usage_chunk = std::str::from_utf8(&chunks[chunks.len() - 2]).unwrap();
            let translated: Value =
                serde_json::from_str(usage_chunk.strip_prefix("data: ").unwrap().trim()).unwrap();
            assert_eq!(translated["usage"], expected);
        }
    }

    fn event(value: Value) -> String {
        format!(
            "event: {}\r\ndata: {value}\r\n\r\n",
            value["type"].as_str().unwrap()
        )
    }

    fn tool_stream() -> String {
        [
            json!({"type":"message_start", "message":{"id":"msg_1", "model":"claude", "usage":{"input_tokens":7,"cache_read_input_tokens":8,"output_tokens":0}}}),
            json!({"type":"content_block_start", "index":0, "content_block":{"type":"text", "text":""}}),
            json!({"type":"content_block_delta", "index":0, "delta":{"type":"text_delta", "text":"Héllo 🦀"}}),
            json!({"type":"content_block_stop", "index":0}),
            json!({"type":"content_block_start", "index":1, "content_block":{"type":"tool_use", "id":"tool_1", "name":"weather", "input":{}}}),
            json!({"type":"content_block_delta", "index":1, "delta":{"type":"input_json_delta", "partial_json":"{\"city\":"}}),
            json!({"type":"content_block_delta", "index":1, "delta":{"type":"input_json_delta", "partial_json":"\"Paris\"}"}}),
            json!({"type":"content_block_stop", "index":1}),
            json!({"type":"message_delta", "delta":{"stop_reason":"tool_use"}, "usage":{"output_tokens":12}}),
            json!({"type":"message_stop"}),
        ].into_iter().map(event).collect()
    }

    #[tokio::test]
    async fn fragmented_utf8_crlf_and_tool_stream_matches_single_chunk() {
        let wire = tool_stream();
        let whole = translate_stream(
            futures_util::stream::iter(vec![Ok(Bytes::from(wire.clone()))]),
            true,
        )
        .collect::<Vec<_>>()
        .await
        .into_iter()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
        let chunks = wire
            .bytes()
            .map(|byte| Ok(Bytes::from(vec![byte])))
            .collect::<Vec<_>>();
        let split = translate_stream(futures_util::stream::iter(chunks), true)
            .collect::<Vec<_>>()
            .await
            .into_iter()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        // Compare protocol fields rather than timestamps across a clock tick.
        let decode = |chunks: Vec<Bytes>| {
            chunks
                .into_iter()
                .filter_map(|chunk| {
                    let text = std::str::from_utf8(&chunk).unwrap();
                    if text == "data: [DONE]\n\n" {
                        return None;
                    }
                    let mut value: Value =
                        serde_json::from_str(text.strip_prefix("data: ").unwrap().trim()).unwrap();
                    value.as_object_mut().unwrap().remove("created");
                    Some(value)
                })
                .collect::<Vec<_>>()
        };
        assert_eq!(whole.last().unwrap().as_ref(), b"data: [DONE]\n\n");
        let whole = decode(whole);
        assert_eq!(whole, decode(split));
        assert_eq!(whole[1]["choices"][0]["delta"]["content"], "Héllo 🦀");
        assert_eq!(whole[2]["choices"][0]["delta"]["tool_calls"][0]["index"], 0);
        assert_eq!(
            whole[3]["choices"][0]["delta"]["tool_calls"][0]["function"]["arguments"],
            "{\"city\":"
        );
        assert_eq!(
            whole.last().unwrap()["usage"],
            usage_with_cache(Some(15), Some(12), Some(8))
        );
    }

    #[tokio::test]
    async fn detects_early_error_truncation_and_oversized_events() {
        let error = event(
            json!({"type":"error", "error":{"type":"invalid_request_error", "message":"secret"}}),
        );
        let mut stream = translate_stream(
            futures_util::stream::iter(vec![Ok(Bytes::from(error.clone()))]),
            false,
        );
        assert!(matches!(
            stream.next().await,
            Some(Err(AttemptError::Permanent(AppError::Upstream(
                StatusCode::BAD_REQUEST
            ))))
        ));
        let start =
            event(json!({"type":"message_start", "message":{"id":"msg", "model":"claude"}}));
        let mut stream = translate_stream(
            futures_util::stream::iter(vec![Ok(Bytes::from(start.clone()))]),
            false,
        );
        assert!(stream.next().await.unwrap().is_ok());
        assert!(matches!(
            stream.next().await,
            Some(Err(AttemptError::Retryable))
        ));
        // Output of an event precedes a failure later in the same chunk.
        let mut stream = translate_stream(
            futures_util::stream::iter(vec![Ok(Bytes::from(format!("{start}{error}")))]),
            false,
        );
        assert!(stream.next().await.unwrap().is_ok());
        assert!(matches!(
            stream.next().await,
            Some(Err(AttemptError::Permanent(AppError::Upstream(
                StatusCode::BAD_REQUEST
            ))))
        ));
        assert!(stream.next().await.is_none());
        let invalid_utf8 = [start.as_bytes(), b"data: \xff\n"].concat();
        let mut stream = translate_stream(
            futures_util::stream::iter(vec![Ok(Bytes::from(invalid_utf8))]),
            false,
        );
        assert!(stream.next().await.unwrap().is_ok());
        assert!(matches!(
            stream.next().await,
            Some(Err(AttemptError::Retryable))
        ));
        let huge = Bytes::from(vec![b'x'; crate::providers::sse::MAX_EVENT_BYTES + 1]);
        let mut stream = translate_stream(futures_util::stream::iter(vec![Ok(huge)]), false);
        assert!(matches!(
            stream.next().await,
            Some(Err(AttemptError::Retryable))
        ));
    }

    #[test]
    fn handles_multiline_data_and_comments() {
        let mut parser = SseDecoder::default();
        let mut events = Vec::new();
        for byte in b": comment\r\nevent: ping\r\ndata: {\r\ndata: \"type\":\"ping\"}\r\n\r\n" {
            if let Some(event) = parser.push(*byte).unwrap() {
                events.push(event);
            }
        }
        assert_eq!(events.len(), 1);
        assert_eq!(
            serde_json::from_str::<Value>(&events[0].data).unwrap(),
            json!({"type":"ping"})
        );
    }

    #[test]
    fn auxiliary_and_citation_blocks_are_skipped_not_failed() {
        let value = translate_response(&json!({"id":"m", "model":"c", "content":[
            {"type":"server_tool_use", "id":"s", "name":"web_search", "input":{}},
            {"type":"web_search_tool_result", "tool_use_id":"s", "content":[]},
            {"type":"text", "text":"Answer", "citations":[{"type":"web_search_result_location"}]}
        ]}))
        .unwrap();
        assert_eq!(value["choices"][0]["message"]["content"], "Answer");
        assert!(matches!(
            translate_response(&json!({"id":"m", "model":"c", "content":[{"text":"no type"}]})),
            Err(AttemptError::Retryable)
        ));

        let mut state = StreamState::new(false);
        let mut apply = |value: Value| {
            state.event(SseEvent {
                name: String::new(),
                data: value.to_string(),
            })
        };
        apply(json!({"type":"message_start", "message":{"id":"msg", "model":"claude"}})).unwrap();
        apply(json!({"type":"content_block_start", "index":0, "content_block":{"type":"server_tool_use", "id":"s", "name":"web_search", "input":{}}})).unwrap();
        let hidden = apply(json!({"type":"content_block_delta", "index":0, "delta":{"type":"input_json_delta", "partial_json":"{\"q\":1}"}})).unwrap();
        assert!(hidden.is_empty());
        apply(json!({"type":"content_block_stop", "index":0})).unwrap();
        apply(json!({"type":"content_block_start", "index":1, "content_block":{"type":"text", "text":""}})).unwrap();
        let citation = apply(json!({"type":"content_block_delta", "index":1, "delta":{"type":"citations_delta", "citation":{"type":"web_search_result_location"}}})).unwrap();
        assert!(citation.is_empty());
        let text = apply(json!({"type":"content_block_delta", "index":1, "delta":{"type":"text_delta", "text":"Answer"}})).unwrap();
        assert!(std::str::from_utf8(&text[0]).unwrap().contains("Answer"));
        // Unexpected delta kinds on answer blocks are still treated as corruption.
        assert!(apply(json!({"type":"content_block_delta", "index":1, "delta":{"type":"input_json_delta", "partial_json":"{}"}})).is_err());
        assert!(
            apply(
                json!({"type":"content_block_start", "index":2, "content_block":{"text":"no type"}})
            )
            .is_err()
        );
    }

    #[test]
    fn empty_tool_input_still_produces_valid_json_arguments() {
        let mut state = StreamState::new(false);
        let mut apply = |value: Value| {
            state
                .event(SseEvent {
                    name: String::new(),
                    data: value.to_string(),
                })
                .unwrap()
        };
        apply(json!({"type":"message_start", "message":{"id":"msg", "model":"claude"}}));
        apply(
            json!({"type":"content_block_start", "index":0, "content_block":{"type":"tool_use", "id":"call", "name":"now", "input":{}}}),
        );
        let chunks = apply(json!({"type":"content_block_stop", "index":0}));
        let text = std::str::from_utf8(&chunks[0]).unwrap();
        let value: Value =
            serde_json::from_str(text.strip_prefix("data: ").unwrap().trim()).unwrap();
        assert_eq!(
            value["choices"][0]["delta"]["tool_calls"][0]["function"]["arguments"],
            "{}"
        );
    }
}
