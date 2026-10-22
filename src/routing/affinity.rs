//! Bounded, ephemeral placement hints. Never stores prompts or raw session IDs.
use std::{
    collections::HashMap,
    sync::Mutex,
    time::{Duration, Instant},
};

use axum::{
    body::Body,
    http::{HeaderMap, HeaderValue},
    response::Response,
};
use futures_util::StreamExt;
use serde_json::Value;
use zeroize::Zeroizing;

use crate::{
    crypto::Crypto,
    db::combos::{AffinityMode, Combo, ComboStep},
    error::AppError,
};

const MAX_ENTRIES: usize = 4096;
const MAX_SESSION_BYTES: usize = 256;

/// The JSON API a non-media request arrived on.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum Endpoint {
    #[default]
    Chat,
    SystemOne,
    Responses,
    Embeddings,
    /// Anthropic's native `/v1/messages`, passed through to Anthropic connections.
    Messages,
}

// No Debug: the transient header may contain a private session identifier.
pub(crate) struct RequestContext {
    scope: String,
    session_header: Option<HeaderValue>,
    pub(crate) endpoint: Endpoint,
    pub(crate) media: Option<crate::providers::media::Kind>,
    pub(crate) upload: Option<crate::providers::media::Upload>,
    /// Client protocol headers forwarded upstream (Anthropic's version and
    /// beta flags on `/v1/messages`); never credentials.
    pub(crate) upstream_headers: HeaderMap,
    /// The connection holding the response a Responses request continues
    /// (`previous_response_id`); tried first when the chain includes it.
    pub(crate) preferred_connection: Option<String>,
}

impl Default for RequestContext {
    fn default() -> Self {
        Self {
            scope: "anonymous".into(),
            session_header: None,
            endpoint: Endpoint::Chat,
            media: None,
            upload: None,
            upstream_headers: HeaderMap::new(),
            preferred_connection: None,
        }
    }
}

impl Endpoint {
    /// The `endpoint` label in metrics.
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Chat => "chat",
            Self::SystemOne => "systemone",
            Self::Responses => "responses",
            Self::Embeddings => "embeddings",
            Self::Messages => "messages",
        }
    }
}

impl RequestContext {
    /// Endpoints keep separate placement hints: their eligible targets differ.
    pub(crate) fn for_endpoint(mut self, endpoint: Endpoint) -> Self {
        self.endpoint = endpoint;
        self.scope.push_str(match endpoint {
            Endpoint::Chat => "",
            Endpoint::SystemOne => ":systemone",
            Endpoint::Responses => ":responses",
            Endpoint::Embeddings => ":embeddings",
            Endpoint::Messages => ":messages",
        });
        self
    }

    pub(crate) fn for_media(
        mut self,
        kind: crate::providers::media::Kind,
        upload: Option<crate::providers::media::Upload>,
    ) -> Self {
        self.media = Some(kind);
        self.upload = upload;
        self.scope.push(':');
        self.scope.push_str(kind.name());
        self
    }

    pub(crate) fn category(&self) -> crate::db::combos::Category {
        use crate::{db::combos::Category, providers::media::Kind};
        match (self.media, self.endpoint) {
            (Some(Kind::Image | Kind::ImageEdit), _) => Category::Image,
            (Some(Kind::Video), _) => Category::Video,
            (Some(Kind::Music), _) => Category::Music,
            (Some(Kind::Speech), _) => Category::Tts,
            (Some(Kind::Transcription | Kind::Translation), _) => Category::Stt,
            (None, Endpoint::Chat | Endpoint::Responses | Endpoint::Messages) => Category::Text,
            (None, Endpoint::SystemOne) => Category::Decision,
            (None, Endpoint::Embeddings) => Category::Embedding,
        }
    }

    pub(crate) fn accepts(&self, provider: &crate::db::providers::Provider) -> bool {
        use crate::providers::Protocol;
        if let Some(kind) = self.media {
            return kind.accepts(crate::providers::media::dialect(provider));
        }
        match self.endpoint {
            Endpoint::SystemOne => provider.protocol == Protocol::SystemOne,
            Endpoint::Chat => matches!(
                provider.protocol,
                Protocol::OpenAi | Protocol::Anthropic | Protocol::MiniMax | Protocol::Xai
            ),
            // Only these protocols implement OpenAI's `/responses` wire format.
            Endpoint::Responses => matches!(provider.protocol, Protocol::OpenAi | Protocol::Xai),
            Endpoint::Embeddings => provider.protocol == Protocol::OpenAi,
            Endpoint::Messages => provider.protocol == Protocol::Anthropic,
        }
    }

    pub(crate) fn new(scope: String, headers: &HeaderMap) -> Self {
        let session_header = [
            "x-sparkrouter-session-id",
            "x-sparkrouter-conversation-id",
            "session-id",
            "x-session-id",
        ]
        .iter()
        .find_map(|name| headers.get(*name))
        .cloned();
        Self {
            scope,
            session_header,
            endpoint: Endpoint::Chat,
            media: None,
            upload: None,
            upstream_headers: HeaderMap::new(),
            preferred_connection: None,
        }
    }
}

#[derive(Clone, Hash, PartialEq, Eq)]
struct Key {
    combo: String,
    revision: i64,
    scope: String,
    identity: String,
}

pub(crate) struct Binding {
    key: Key,
    ttl: Duration,
}

/// Keep the exact step/connection/model, not merely a vendor with multiple keys.
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct Target {
    step: String,
    connection: String,
    model: String,
}

impl Target {
    pub(crate) fn from_step(step: &ComboStep) -> Self {
        Self {
            step: step.id.clone(),
            connection: step.provider_id.clone(),
            model: step.model.clone(),
        }
    }

    pub(crate) fn matches(&self, step: &ComboStep) -> bool {
        self.step == step.id && self.connection == step.provider_id && self.model == step.model
    }

    /// A direct-route connection; direct models have no combo step.
    pub(crate) fn for_connection(connection: &str, model: &str) -> Self {
        Self {
            step: String::new(),
            connection: connection.into(),
            model: model.into(),
        }
    }

    pub(crate) fn connection(&self) -> &str {
        &self.connection
    }
}

struct Entry {
    target: Target,
    used: Instant,
    expires: Instant,
}

#[derive(Default)]
pub(crate) struct Affinity {
    entries: Mutex<HashMap<Key, Entry>>,
}

fn session_value(value: &str) -> Result<Option<&str>, AppError> {
    let has_controls = value.chars().any(char::is_control);
    let value = value.trim();
    if value.len() > MAX_SESSION_BYTES || has_controls {
        return Err(AppError::InvalidRequest(
            "session ID must be at most 256 bytes without control characters".into(),
        ));
    }
    Ok((!value.is_empty()).then_some(value))
}

fn explicit_session<'a>(
    context: &'a RequestContext,
    body: &'a Value,
) -> Result<Option<&'a str>, AppError> {
    if let Some(header) = &context.session_header {
        let value = header
            .to_str()
            .map_err(|_| AppError::InvalidRequest("session ID header must be text".into()))?;
        if let Some(value) = session_value(value)? {
            return Ok(Some(value));
        }
    }
    for object in [Some(body), body.get("metadata")].into_iter().flatten() {
        for field in ["session_id", "conversation_id", "chat_id"] {
            if let Some(value) = object.get(field).filter(|value| !value.is_null()) {
                let value = value.as_str().ok_or_else(|| {
                    AppError::InvalidRequest("session ID must be a string".into())
                })?;
                if let Some(value) = session_value(value)? {
                    return Ok(Some(value));
                }
            }
        }
    }
    // Provider-native conversation keys: OpenAI's `prompt_cache_key` and the
    // Responses `conversation` (an ID or `{"id": ...}`). The upstream validates
    // its own fields, so an unusable value is ignored here rather than rejected.
    let conversation = body
        .get("conversation")
        .and_then(|value| value.as_str().or_else(|| value.get("id")?.as_str()));
    for value in [
        body.get("prompt_cache_key").and_then(Value::as_str),
        conversation,
    ]
    .into_iter()
    .flatten()
    {
        if let Ok(Some(value)) = session_value(value) {
            return Ok(Some(value));
        }
    }
    Ok(None)
}

fn role(item: &Value) -> Option<&str> {
    item.get("role").and_then(Value::as_str)
}

/// The leading system/developer items and the first user item of a conversation.
fn conversation_start(items: &[Value]) -> Option<(Vec<&Value>, &Value)> {
    let initial = items
        .iter()
        .take_while(|item| matches!(role(item), Some("system" | "developer")))
        .collect();
    let user = items.iter().find(|item| role(item) == Some("user"))?;
    Some((initial, user))
}

/// The stable start of a prompt, serialized for Smart fingerprinting. Exact
/// content and all fields are kept; serde_json's default ordered object maps
/// make object key order immaterial.
fn prompt_start(endpoint: Endpoint, body: &Value) -> Result<Option<Zeroizing<Vec<u8>>>, AppError> {
    let tools = body.get("tools");
    let serialized = if endpoint == Endpoint::Responses {
        // Responses carry the prompt in `instructions` and `input`: the first
        // user message as a string, or a list of conversation items.
        let instructions = body.get("instructions");
        match body.get("input") {
            Some(Value::String(text)) => {
                serde_json::to_vec(&(instructions, Vec::<&Value>::new(), text, tools))
            }
            Some(Value::Array(items)) => {
                let Some((initial, user)) = conversation_start(items) else {
                    return Ok(None);
                };
                serde_json::to_vec(&(instructions, initial, user, tools))
            }
            _ => return Ok(None),
        }
    } else if endpoint == Endpoint::Messages {
        // Messages keep the system prompt in a top-level `system` field.
        let Some(user) = body
            .get("messages")
            .and_then(Value::as_array)
            .and_then(|messages| messages.iter().find(|item| role(item) == Some("user")))
        else {
            return Ok(None);
        };
        serde_json::to_vec(&(body.get("system"), user, tools))
    } else {
        let Some((initial, user)) = body
            .get("messages")
            .and_then(Value::as_array)
            .and_then(|messages| conversation_start(messages))
        else {
            return Ok(None);
        };
        serde_json::to_vec(&(initial, user, tools))
    };
    serialized
        .map(|bytes| Some(Zeroizing::new(bytes)))
        .map_err(|_| AppError::Routing)
}

pub(crate) fn binding(
    combo: &Combo,
    context: &RequestContext,
    crypto: &Crypto,
    body: &Value,
) -> Result<Option<Binding>, AppError> {
    // Every mode keeps a hint for the combo's timeout after its last success.
    let ttl = Duration::from_secs(u64::from(combo.affinity.sticky_minutes) * 60);
    let identity = match combo.affinity.mode {
        AffinityMode::Off => return Ok(None),
        AffinityMode::Sticky => "sticky".into(),
        mode => {
            if let Some(session) = explicit_session(context, body)? {
                format!(
                    "session:{}",
                    crypto.affinity_fingerprint(session.as_bytes())?
                )
            } else if mode == AffinityMode::Smart {
                let Some(start) = prompt_start(context.endpoint, body)? else {
                    return Ok(None);
                };
                format!("prompt:{}", crypto.affinity_fingerprint(&start)?)
            } else {
                return Ok(None);
            }
        }
    };
    Ok(Some(Binding {
        key: Key {
            combo: combo.id.clone(),
            revision: combo.routing_revision,
            scope: context.scope.clone(),
            identity,
        },
        ttl,
    }))
}

/// Direct model IDs remember the connection that last succeeded for this
/// caller, endpoint, and public model; `minutes` 0 is off. Every session shares
/// one hint because oldest-first already sends them all to the same connection.
pub(crate) fn direct_binding(
    public_model: &str,
    minutes: u32,
    context: &RequestContext,
) -> Option<Binding> {
    (minutes > 0).then(|| Binding {
        key: Key {
            // Combo IDs never contain '/', so this cannot collide with a combo.
            combo: format!("direct/{public_model}"),
            // A changed timeout starts fresh rather than keeping older hints.
            revision: i64::from(minutes),
            scope: context.scope.clone(),
            identity: "sticky".into(),
        },
        ttl: Duration::from_secs(u64::from(minutes) * 60),
    })
}

/// The reusable start every conversation of one agent shares: its system or
/// developer instructions and tools, without any user content.
fn shared_prefix(endpoint: Endpoint, body: &Value) -> Option<Zeroizing<Vec<u8>>> {
    fn leading(items: Option<&Value>) -> Vec<&Value> {
        let items = items
            .and_then(Value::as_array)
            .map_or(&[][..], Vec::as_slice);
        items
            .iter()
            .take_while(|item| matches!(role(item), Some("system" | "developer")))
            .collect()
    }
    let (instructions, initial) = match endpoint {
        Endpoint::Responses => (body.get("instructions"), leading(body.get("input"))),
        Endpoint::Chat => (None, leading(body.get("messages"))),
        _ => return None,
    };
    let instructions = instructions.filter(|value| !matches!(value, Value::Null) && *value != "");
    let tools = body
        .get("tools")
        .filter(|tools| tools.as_array().is_some_and(|tools| !tools.is_empty()));
    // An empty prefix would give unrelated requests one common key.
    if instructions.is_none() && initial.is_empty() && tools.is_none() {
        return None;
    }
    serde_json::to_vec(&(instructions, initial, tools))
        .ok()
        .map(Zeroizing::new)
}

/// A `prompt_cache_key` for a request that has none, sent only to upstreams
/// documented to accept it. Derived from the caller and the shared prefix, so
/// one agent's conversations share a cache placement while other agents and
/// callers do not. A client-supplied key, even null, is never replaced.
pub(crate) fn prompt_cache_key(
    context: &RequestContext,
    provider: &crate::db::providers::Provider,
    crypto: &Crypto,
    body: &Value,
) -> Option<String> {
    if context.media.is_some()
        || body.get("prompt_cache_key").is_some()
        || !crate::providers::openai::accepts_prompt_cache_key(provider)
    {
        return None;
    }
    let prefix = shared_prefix(context.endpoint, body)?;
    let mut input = Zeroizing::new(context.scope.as_bytes().to_vec());
    input.push(0);
    input.extend_from_slice(&prefix);
    let fingerprint = crypto.prompt_cache_fingerprint(&input).ok()?;
    Some(format!("sparkrouter-{fingerprint}"))
}

impl Affinity {
    pub(crate) fn preferred(&self, binding: &Binding) -> Option<Target> {
        self.preferred_at(binding, Instant::now())
    }

    fn preferred_at(&self, binding: &Binding, now: Instant) -> Option<Target> {
        let mut entries = self.entries.lock().ok()?;
        entries.retain(|_, entry| entry.expires > now);
        entries.get(&binding.key).map(|entry| entry.target.clone())
    }

    pub(crate) fn remember(&self, binding: Binding, target: Target) {
        self.remember_at(binding, target, Instant::now());
    }

    fn remember_at(&self, binding: Binding, target: Target, now: Instant) {
        let Ok(mut entries) = self.entries.lock() else {
            return;
        };
        // An older in-flight stream may finish after a configuration edit.
        // Keep revisions isolated; that completion must not evict newer hints.
        entries.retain(|_, entry| entry.expires > now);
        if entries.len() >= MAX_ENTRIES
            && !entries.contains_key(&binding.key)
            && let Some(oldest) = entries
                .iter()
                .min_by_key(|(_, entry)| entry.used)
                .map(|(key, _)| key.clone())
        {
            entries.remove(&oldest);
        }
        entries.insert(
            binding.key,
            Entry {
                target,
                used: now,
                expires: now + binding.ttl,
            },
        );
    }

    pub(crate) fn forget(&self, combo: &str) {
        if let Ok(mut entries) = self.entries.lock() {
            entries.retain(|key, _| key.combo != combo);
        }
    }

    pub(crate) fn forget_target(&self, binding: &Binding, target: &Target) {
        if let Ok(mut entries) = self.entries.lock()
            && entries
                .get(&binding.key)
                .is_some_and(|entry| entry.target == *target)
        {
            entries.remove(&binding.key);
        }
    }
}

/// Remember `target` once `response` has succeeded: now, or for a stream once
/// its body finishes.
pub(crate) fn remember_on_success(
    affinity: &std::sync::Arc<Affinity>,
    binding: Binding,
    target: Target,
    body: &Value,
    response: Response,
) -> Response {
    if body.get("stream").and_then(Value::as_bool) == Some(true)
        || response
            .extensions()
            .get::<crate::providers::media::BinaryStream>()
            .is_some()
    {
        return on_complete(response, affinity.clone(), binding, target);
    }
    affinity.remember(binding, target);
    response
}

/// A stream only establishes affinity when its body finishes successfully.
/// Errors and cancellation never warm a hint and can never re-enter routing.
pub(crate) fn on_complete(
    response: Response,
    affinity: std::sync::Arc<Affinity>,
    binding: Binding,
    target: Target,
) -> Response {
    let (parts, body) = response.into_parts();
    let stream = async_stream::stream! {
        let mut source = body.into_data_stream();
        while let Some(chunk) = source.next().await {
            let failed = chunk.is_err();
            yield chunk;
            if failed { return; }
        }
        affinity.remember(binding, target);
    };
    Response::from_parts(parts, Body::from_stream(stream))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::combos::{AffinityOptions, Strategy};
    use base64::{Engine, engine::general_purpose::STANDARD};
    use secrecy::SecretString;
    use serde_json::json;

    fn crypto() -> Crypto {
        Crypto::from_base64(&SecretString::from(STANDARD.encode([7; 32]))).unwrap()
    }
    fn combo(mode: AffinityMode) -> Combo {
        Combo {
            id: "combo".into(),
            name: "test".into(),
            category: crate::db::combos::Category::Text,
            strategy: Strategy::RoundRobin,
            affinity: AffinityOptions {
                mode,
                sticky_minutes: 3,
            },
            routing_revision: 0,
        }
    }
    fn target() -> Target {
        Target {
            step: "step".into(),
            connection: "connection".into(),
            model: "model".into(),
        }
    }

    #[test]
    fn modes_session_precedence_and_fingerprint_continuity() {
        let crypto = crypto();
        let context = RequestContext::default();
        let body = json!({"messages":[{"role":"system","content":"rules"},{"role":"user","content":"question"}], "tools":[{"type":"function","function":{"name":"a"}}]});
        assert!(
            binding(&combo(AffinityMode::Off), &context, &crypto, &body)
                .unwrap()
                .is_none()
        );
        assert!(
            binding(&combo(AffinityMode::Basic), &context, &crypto, &body)
                .unwrap()
                .is_none()
        );
        let smart = combo(AffinityMode::Smart);
        let first = binding(&smart, &context, &crypto, &body).unwrap().unwrap();
        assert!(!first.key.identity.contains("question"));
        let mut next = body.clone();
        next["messages"]
            .as_array_mut()
            .unwrap()
            .push(json!({"role":"assistant","content":"answer"}));
        next["messages"]
            .as_array_mut()
            .unwrap()
            .push(json!({"role":"user","content":"follow-up"}));
        assert!(
            first.key
                == binding(&smart, &context, &crypto, &next)
                    .unwrap()
                    .unwrap()
                    .key
        );
        for field in ["system", "user", "tools"] {
            let mut changed = next.clone();
            match field {
                "system" => changed["messages"][0]["content"] = json!("other"),
                "user" => changed["messages"][1]["content"] = json!("other"),
                _ => changed["tools"] = json!([]),
            }
            assert!(
                first.key
                    != binding(&smart, &context, &crypto, &changed)
                        .unwrap()
                        .unwrap()
                        .key
            );
        }
        next["session_id"] = json!("same-session");
        let session = binding(&smart, &context, &crypto, &next).unwrap().unwrap();
        next["messages"][0]["content"] = json!("new prompt");
        assert!(
            session.key
                == binding(&smart, &context, &crypto, &next)
                    .unwrap()
                    .unwrap()
                    .key
        );
        assert!(
            session.key
                == binding(&combo(AffinityMode::Basic), &context, &crypto, &next)
                    .unwrap()
                    .unwrap()
                    .key
        );
        let mut headers = HeaderMap::new();
        headers.insert("session-id", HeaderValue::from_static("header-session"));
        let header = RequestContext::new("anonymous".into(), &headers);
        assert!(
            session.key
                != binding(&smart, &header, &crypto, &next)
                    .unwrap()
                    .unwrap()
                    .key
        );
        assert!(
            binding(
                &smart,
                &context,
                &crypto,
                &json!({"messages":[],"session_id":12})
            )
            .is_err()
        );
        assert!(
            binding(
                &smart,
                &context,
                &crypto,
                &json!({"messages":[{"role":"system","content":"only rules"}]})
            )
            .unwrap()
            .is_none()
        );
    }

    #[test]
    fn provider_native_conversation_keys_are_session_ids() {
        let crypto = crypto();
        let context = RequestContext::default();
        let basic = combo(AffinityMode::Basic);
        let key = |body: Value| {
            binding(&basic, &context, &crypto, &body)
                .unwrap()
                .map(|b| b.key)
        };
        let session = key(json!({"session_id":"abc"})).unwrap();
        assert!(key(json!({"prompt_cache_key":"abc"})) == Some(session.clone()));
        assert!(key(json!({"conversation":"abc"})) == Some(session.clone()));
        assert!(key(json!({"conversation":{"id":"abc"}})) == Some(session.clone()));
        // Gateway fields win over provider-native ones, and the cache key
        // over the conversation.
        assert!(
            key(json!({"session_id":"abc", "prompt_cache_key":"other"})) == Some(session.clone())
        );
        assert!(key(json!({"prompt_cache_key":"abc", "conversation":"other"})) == Some(session));
        // Unusable provider-native values are ignored, not rejected.
        for body in [
            json!({"prompt_cache_key":12}),
            json!({"prompt_cache_key":"  "}),
            json!({"prompt_cache_key":"x".repeat(MAX_SESSION_BYTES + 1)}),
            json!({"conversation":{"id":7}}),
            json!({"conversation":null}),
        ] {
            assert!(
                binding(&basic, &context, &crypto, &body).unwrap().is_none(),
                "{body}"
            );
        }
    }

    #[test]
    fn smart_fingerprints_the_start_of_responses_prompts() {
        let crypto = crypto();
        let context = RequestContext::default().for_endpoint(Endpoint::Responses);
        let smart = combo(AffinityMode::Smart);
        let key = |body: &Value| {
            binding(&smart, &context, &crypto, body)
                .unwrap()
                .map(|b| b.key)
        };
        let body = json!({"instructions":"rules", "tools":[{"type":"function","name":"a"}], "input":[
            {"role":"developer","content":"style"},
            {"role":"user","content":[{"type":"input_text","text":"question"}]}
        ]});
        let first = key(&body).unwrap();
        let mut next = body.clone();
        next["input"].as_array_mut().unwrap().extend([
            json!({"type":"message","role":"assistant","content":[{"type":"output_text","text":"answer"}]}),
            json!({"type":"function_call_output","call_id":"c","output":"x"}),
            json!({"role":"user","content":"follow-up"}),
        ]);
        assert!(key(&next) == Some(first.clone()));
        for change in ["instructions", "developer", "user", "tools"] {
            let mut changed = next.clone();
            match change {
                "instructions" => changed["instructions"] = json!("other"),
                "developer" => changed["input"][0]["content"] = json!("other"),
                "user" => changed["input"][1]["content"] = json!("other"),
                _ => changed["tools"] = json!([]),
            }
            assert!(key(&changed) != Some(first.clone()), "{change}");
        }
        let text = key(&json!({"instructions":"rules", "input":"question"})).unwrap();
        assert!(key(&json!({"instructions":"rules", "input":"question"})) == Some(text.clone()));
        assert!(key(&json!({"instructions":"other", "input":"question"})) != Some(text));
        assert!(key(&json!({"input":[{"role":"developer","content":"only rules"}]})).is_none());
        assert!(key(&json!({"instructions":"rules"})).is_none());
        // The chat fingerprint still reads `messages`, not `input`.
        let chat = RequestContext::default();
        assert!(
            binding(&smart, &chat, &crypto, &json!({"input":"question"}))
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn successful_requests_renew_timeout_and_old_snapshots_cannot_clear_new_targets() {
        let crypto = crypto();
        let context = RequestContext::default();
        // Basic/Smart and Sticky all use the combo's timeout (3 minutes here).
        for mode in [
            AffinityMode::Basic,
            AffinityMode::Smart,
            AffinityMode::Sticky,
        ] {
            let memory = Affinity::default();
            let mut combo = combo(mode);
            let body = json!({"session_id":"id"});
            let make = |combo: &Combo| binding(combo, &context, &crypto, &body).unwrap().unwrap();
            let ttl = Duration::from_secs(180);
            let start = Instant::now();
            memory.remember_at(make(&combo), target(), start);
            let mut backup = target();
            backup.connection = "backup".into();
            memory.remember_at(make(&combo), backup.clone(), start + ttl / 2);
            memory.forget_target(&make(&combo), &target());
            assert!(
                memory
                    .preferred_at(&make(&combo), start + ttl)
                    .is_some_and(|target| target == backup)
            );
            assert!(
                memory
                    .preferred_at(&make(&combo), start + ttl + ttl / 2)
                    .is_none()
            );
            combo.routing_revision = 1;
            memory.remember_at(make(&combo), target(), start);
            combo.routing_revision = 0;
            memory.remember_at(make(&combo), backup, start);
            combo.routing_revision = 1;
            assert!(memory.preferred_at(&make(&combo), start).is_some());
        }
    }

    #[test]
    fn direct_bindings_are_per_model_scope_and_timeout() {
        let context = RequestContext::default();
        assert!(direct_binding("pool/m", 0, &context).is_none());
        let five = direct_binding("pool/m", 5, &context).unwrap();
        assert_eq!(five.ttl, Duration::from_secs(300));
        assert!(direct_binding("pool/m", 5, &context).unwrap().key == five.key);
        let responses = RequestContext::default().for_endpoint(Endpoint::Responses);
        for other in [
            direct_binding("pool/other", 5, &context),
            direct_binding("pool/m", 6, &context),
            direct_binding("pool/m", 5, &responses),
        ] {
            assert!(other.unwrap().key != five.key);
        }
    }

    #[test]
    fn generated_prompt_cache_keys_follow_the_shared_prefix() {
        use crate::{db::providers::Provider, providers::Protocol};
        let crypto = crypto();
        let openai = Provider {
            id: "openai".into(),
            name: "OpenAI".into(),
            base_url: "https://api.openai.com/v1".into(),
            protocol: Protocol::OpenAi,
            encrypted_api_key: vec![],
            enabled: true,
        };
        let context = RequestContext::default();
        let key = |context: &RequestContext, provider: &Provider, body: &Value| {
            prompt_cache_key(context, provider, &crypto, body)
        };
        let body = json!({"messages":[{"role":"system","content":"agent rules"},{"role":"user","content":"question"}], "tools":[{"type":"function","function":{"name":"a"}}]});
        let first = key(&context, &openai, &body).unwrap();
        assert!(first.starts_with("sparkrouter-") && first.len() <= 64);
        assert!(!first.contains("rules") && !first.contains("question"));
        // Another conversation of the same agent shares the key.
        let mut other = body.clone();
        other["messages"][1]["content"] = json!("different question");
        assert_eq!(
            key(&context, &openai, &other).as_deref(),
            Some(first.as_str())
        );
        // A different agent, toolset, or caller does not.
        let mut changed = body.clone();
        changed["messages"][0]["content"] = json!("other rules");
        assert_ne!(
            key(&context, &openai, &changed).as_deref(),
            Some(first.as_str())
        );
        changed = body.clone();
        changed["tools"] = json!([]);
        assert_ne!(
            key(&context, &openai, &changed).as_deref(),
            Some(first.as_str())
        );
        let caller = RequestContext {
            scope: "other-key".into(),
            ..RequestContext::default()
        };
        assert_ne!(
            key(&caller, &openai, &body).as_deref(),
            Some(first.as_str())
        );
        // No usable prefix, a client key (even null), or an unverified upstream
        // or endpoint: nothing is generated.
        for body in [
            json!({"messages":[{"role":"user","content":"question"}]}),
            json!({"messages":[{"role":"user","content":"q"}], "tools":[]}),
            json!({"messages":[{"role":"system","content":"r"}], "prompt_cache_key":"client"}),
            json!({"messages":[{"role":"system","content":"r"}], "prompt_cache_key":null}),
        ] {
            assert!(key(&context, &openai, &body).is_none(), "{body}");
        }
        for (base_url, protocol) in [
            ("https://api.openai.com.example/v1", Protocol::OpenAi),
            ("https://openrouter.ai/api/v1", Protocol::OpenAi),
            ("https://api.openai.com/v1", Protocol::Xai),
        ] {
            let provider = Provider {
                id: "other".into(),
                name: "Other".into(),
                base_url: base_url.into(),
                protocol,
                encrypted_api_key: vec![],
                enabled: true,
            };
            assert!(key(&context, &provider, &body).is_none(), "{base_url}");
        }
        for context in [
            RequestContext::default().for_endpoint(Endpoint::Embeddings),
            RequestContext::default().for_endpoint(Endpoint::SystemOne),
            RequestContext::default().for_media(crate::providers::media::Kind::Image, None),
        ] {
            assert!(key(&context, &openai, &body).is_none());
        }
        // Responses use `instructions` and leading developer items.
        let responses = RequestContext::default().for_endpoint(Endpoint::Responses);
        let instructed = key(
            &responses,
            &openai,
            &json!({"instructions":"agent rules", "input":"question"}),
        )
        .unwrap();
        assert_eq!(
            key(
                &responses,
                &openai,
                &json!({"instructions":"agent rules", "input":[{"role":"user","content":"other"}]})
            )
            .as_deref(),
            Some(instructed.as_str())
        );
        assert!(
            key(
                &responses,
                &openai,
                &json!({"input":[{"role":"developer","content":"style"}]})
            )
            .is_some()
        );
        assert!(
            key(
                &responses,
                &openai,
                &json!({"instructions":"", "input":"question"})
            )
            .is_none()
        );
    }

    #[test]
    fn expiry_refresh_scope_revision_and_capacity() {
        let crypto = crypto();
        let memory = Affinity::default();
        let context = RequestContext::default();
        let mut combo = combo(AffinityMode::Sticky);
        let now = Instant::now();
        let key = binding(&combo, &context, &crypto, &Value::Null)
            .unwrap()
            .unwrap();
        memory.remember_at(
            binding(&combo, &context, &crypto, &Value::Null)
                .unwrap()
                .unwrap(),
            target(),
            now,
        );
        assert!(
            memory
                .preferred_at(&key, now + Duration::from_secs(179))
                .is_some()
        );
        let other = RequestContext {
            scope: "other-key".into(),
            ..RequestContext::default()
        };
        assert!(
            memory
                .preferred_at(
                    &binding(&combo, &other, &crypto, &Value::Null)
                        .unwrap()
                        .unwrap(),
                    now
                )
                .is_none()
        );
        combo.routing_revision += 1;
        assert!(
            memory
                .preferred_at(
                    &binding(&combo, &context, &crypto, &Value::Null)
                        .unwrap()
                        .unwrap(),
                    now
                )
                .is_none()
        );
        assert!(
            memory
                .preferred_at(&key, now + Duration::from_secs(180))
                .is_none()
        );
        for i in 0..=MAX_ENTRIES {
            let context = RequestContext {
                scope: format!("key-{i}"),
                ..RequestContext::default()
            };
            memory.remember_at(
                binding(&combo, &context, &crypto, &Value::Null)
                    .unwrap()
                    .unwrap(),
                target(),
                now,
            );
        }
        assert_eq!(memory.entries.lock().unwrap().len(), MAX_ENTRIES);
        memory.forget(&combo.id);
        assert!(memory.entries.lock().unwrap().is_empty());
    }
}
