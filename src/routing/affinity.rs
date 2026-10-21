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

const CACHE_IDLE: Duration = Duration::from_secs(30 * 60);
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
}

// No Debug: the transient header may contain a private session identifier.
pub(crate) struct RequestContext {
    scope: String,
    session_header: Option<HeaderValue>,
    pub(crate) endpoint: Endpoint,
    pub(crate) media: Option<crate::providers::media::Kind>,
    pub(crate) upload: Option<crate::providers::media::Upload>,
}

impl Default for RequestContext {
    fn default() -> Self {
        Self {
            scope: "anonymous".into(),
            session_header: None,
            endpoint: Endpoint::Chat,
            media: None,
            upload: None,
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
            (None, Endpoint::Chat | Endpoint::Responses) => Category::Text,
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
    Ok(None)
}

pub(crate) fn binding(
    combo: &Combo,
    context: &RequestContext,
    crypto: &Crypto,
    body: &Value,
) -> Result<Option<Binding>, AppError> {
    let (identity, ttl) = match combo.affinity.mode {
        AffinityMode::Off => return Ok(None),
        AffinityMode::Sticky => (
            "sticky".into(),
            Duration::from_secs(u64::from(combo.affinity.sticky_minutes) * 60),
        ),
        mode => {
            let identity = if let Some(session) = explicit_session(context, body)? {
                format!(
                    "session:{}",
                    crypto.affinity_fingerprint(session.as_bytes())?
                )
            } else if mode == AffinityMode::Smart {
                let Some(messages) = body.get("messages").and_then(Value::as_array) else {
                    return Ok(None);
                };
                let initial: Vec<&Value> = messages
                    .iter()
                    .take_while(|message| {
                        matches!(
                            message.get("role").and_then(Value::as_str),
                            Some("system" | "developer")
                        )
                    })
                    .collect();
                let Some(user) = messages
                    .iter()
                    .find(|message| message.get("role").and_then(Value::as_str) == Some("user"))
                else {
                    return Ok(None);
                };
                // Preserve all message/tool fields and exact content. serde_json's
                // default ordered object maps make object key order immaterial.
                let bytes = Zeroizing::new(
                    serde_json::to_vec(&(initial, user, body.get("tools")))
                        .map_err(|_| AppError::Routing)?,
                );
                format!("prompt:{}", crypto.affinity_fingerprint(&bytes)?)
            } else {
                return Ok(None);
            };
            (identity, CACHE_IDLE)
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
    fn successful_requests_renew_timeout_and_old_snapshots_cannot_clear_new_targets() {
        let crypto = crypto();
        let context = RequestContext::default();
        for mode in [AffinityMode::Basic, AffinityMode::Sticky] {
            let memory = Affinity::default();
            let mut combo = combo(mode);
            let body = json!({"session_id":"id"});
            let make = |combo: &Combo| binding(combo, &context, &crypto, &body).unwrap().unwrap();
            let ttl = if mode == AffinityMode::Basic {
                CACHE_IDLE
            } else {
                Duration::from_secs(180)
            };
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
