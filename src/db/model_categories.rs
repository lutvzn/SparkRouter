//! Explicit connection/model classifications, never inferred from model names.
use crate::{
    db::{connection_models::ModelField, providers::Provider},
    error::AppError,
    providers::{
        Protocol,
        media::{self, Kind},
    },
};
use sqlx::{Row, SqliteConnection};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum Category {
    #[default]
    Text,
    Decision,
    Image,
    Video,
    Music,
    Tts,
    Stt,
    Embedding,
}
fn endpoint_fields(fields: &[ModelField]) -> impl Iterator<Item = &ModelField> {
    fields.iter().filter(|field| {
        field
            .label
            .to_ascii_lowercase()
            .replace([' ', '_', '-'], "")
            .ends_with("supportedendpoints")
    })
}

impl Category {
    fn matches_endpoint(self, endpoint: &str) -> bool {
        let endpoint = endpoint.trim().to_ascii_lowercase();
        let endpoint = endpoint.trim_start_matches('/');
        let endpoint = endpoint
            .strip_prefix("v1/")
            .unwrap_or(endpoint)
            .replace(['-', '_'], "/");
        match self {
            Self::Text => matches!(
                endpoint.as_str(),
                "chat" | "chat/completions" | "chatcompletions" | "messages" | "responses"
            ),
            Self::Decision => endpoint == "systemone",
            Self::Image => matches!(
                endpoint.as_str(),
                "image" | "images" | "images/generations" | "images/edits"
            ),
            Self::Video => matches!(endpoint.as_str(), "video" | "videos" | "videos/generations"),
            Self::Music => matches!(endpoint.as_str(), "music" | "music/generations"),
            Self::Tts => matches!(endpoint.as_str(), "audio/speech" | "speech"),
            Self::Stt => matches!(
                endpoint.as_str(),
                "audio/transcriptions" | "audio/translations" | "transcriptions" | "translations"
            ),
            Self::Embedding => matches!(endpoint.as_str(), "embeddings" | "embedding"),
        }
    }

    /// Shared policy for picker suggestions, mutations, import and routing.
    /// Explicit endpoint declarations take precedence over advisory catalog IDs;
    /// contradictory modality metadata always fails closed.
    pub(crate) fn matches_model(
        self,
        provider: &Provider,
        model: &str,
        fields: &[ModelField],
    ) -> bool {
        self.matches_fields(fields)
            && (endpoint_fields(fields).next().is_some()
                || crate::providers::model_catalog::category(provider, model)
                    .is_none_or(|known| known == self))
    }

    pub const ALL: [Self; 8] = [
        Self::Text,
        Self::Decision,
        Self::Image,
        Self::Video,
        Self::Music,
        Self::Tts,
        Self::Stt,
        Self::Embedding,
    ];
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Text => "text",
            Self::Decision => "decision",
            Self::Image => "image",
            Self::Video => "video",
            Self::Music => "music",
            Self::Tts => "tts",
            Self::Stt => "stt",
            Self::Embedding => "embedding",
        }
    }
    pub fn label(self) -> &'static str {
        match self {
            Self::Text => "Text",
            Self::Decision => "Decision / SystemOne",
            Self::Image => "Image",
            Self::Video => "Video",
            Self::Music => "Music",
            Self::Tts => "TTS",
            Self::Stt => "STT",
            Self::Embedding => "Embedding",
        }
    }
    pub fn parse(value: &str) -> Result<Self, AppError> {
        Self::ALL
            .into_iter()
            .find(|c| c.as_str() == value)
            .ok_or_else(|| AppError::InvalidRequest("Unknown combo category".into()))
    }
    pub(crate) fn accepts(self, provider: &Provider) -> bool {
        let kind = match self {
            Self::Text => {
                return matches!(
                    provider.protocol,
                    Protocol::OpenAi | Protocol::Anthropic | Protocol::Xai | Protocol::MiniMax
                );
            }
            Self::Decision => return provider.protocol == Protocol::SystemOne,
            Self::Embedding => return provider.protocol == Protocol::OpenAi,
            Self::Image => Kind::Image,
            Self::Video => Kind::Video,
            Self::Music => Kind::Music,
            Self::Tts => Kind::Speech,
            Self::Stt => Kind::Transcription,
        };
        kind.accepts(media::dialect(provider))
    }
    /// Positive capability evidence for picker suggestions, unlike validation
    /// which deliberately permits manually declared IDs with unknown metadata.
    pub(crate) fn suggests_fields(self, fields: &[ModelField]) -> bool {
        let has = |suffix: &str| {
            fields.iter().any(|field| {
                field
                    .label
                    .to_ascii_lowercase()
                    .replace('_', " ")
                    .ends_with(suffix)
            })
        };
        if endpoint_fields(fields).next().is_some() {
            return self.matches_fields(fields);
        }
        self.matches_fields(fields)
            && has("output modalities")
            && match self {
                Self::Text => {
                    !has("input modalities")
                        || fields.iter().any(|field| {
                            field
                                .label
                                .to_ascii_lowercase()
                                .replace('_', " ")
                                .ends_with("input modalities")
                                && field
                                    .value
                                    .split([',', ' ', ';'])
                                    .any(|part| part.eq_ignore_ascii_case("text"))
                        })
                }
                Self::Image | Self::Video => true,
                Self::Stt | Self::Tts => has("input modalities"),
                // Audio output alone cannot distinguish music from speech, and
                // text output alone cannot distinguish embeddings from chat.
                Self::Music | Self::Decision | Self::Embedding => false,
            }
    }

    /// Explicit output modalities constrain categories; image *input* does
    /// not turn a vision-capable text model into an image generator. Known
    /// speech inputs must also match the requested direction. Unknown
    /// metadata is not a guessed model classification.
    pub(crate) fn matches_fields(self, fields: &[ModelField]) -> bool {
        if endpoint_fields(fields).next().is_some()
            && !endpoint_fields(fields).any(|field| {
                field
                    .value
                    .split([',', ' ', ';'])
                    .any(|endpoint| self.matches_endpoint(endpoint))
            })
        {
            return false;
        }
        if matches!(self, Self::Stt | Self::Tts) {
            let inputs: Vec<_> = fields
                .iter()
                .filter(|field| {
                    field
                        .label
                        .to_ascii_lowercase()
                        .replace('_', " ")
                        .ends_with("input modalities")
                })
                .collect();
            if !inputs.is_empty()
                && !inputs.iter().any(|field| {
                    field.value.split([',', ' ', ';']).any(|part| {
                        if self == Self::Stt {
                            part.eq_ignore_ascii_case("audio") || part.eq_ignore_ascii_case("video")
                        } else {
                            part.eq_ignore_ascii_case("text")
                        }
                    })
                })
            {
                return false;
            }
        }
        let outputs: Vec<_> = fields
            .iter()
            .filter(|f| {
                f.label
                    .to_ascii_lowercase()
                    .replace('_', " ")
                    .ends_with("output modalities")
            })
            .collect();
        if outputs.is_empty() || self == Self::Decision {
            return true;
        }
        outputs.iter().any(|field| {
            field.value.split([',', ' ', ';']).any(|part| match self {
                Self::Text | Self::Stt => part.eq_ignore_ascii_case("text"),
                Self::Image => part.eq_ignore_ascii_case("image"),
                Self::Video => part.eq_ignore_ascii_case("video"),
                Self::Music | Self::Tts => part.eq_ignore_ascii_case("audio"),
                // Listings without an embedding modality describe vectors as text.
                Self::Embedding => ["text", "embedding", "embeddings", "vector"]
                    .iter()
                    .any(|name| part.eq_ignore_ascii_case(name)),
                Self::Decision => false,
            })
        })
    }
}

/// Display positive evidence without guessing names. The same validation policy
/// gates every result; contradictions and incompatible declarations need review.
pub(crate) fn evidence(
    provider: &Provider,
    model: &str,
    fields: &[ModelField],
    declared: Option<Category>,
) -> (Option<Category>, &'static str) {
    let known = known_evidence(provider, model, fields, declared);
    match known {
        Some((category, source))
            if category.accepts(provider)
                && category.matches_model(provider, model, fields)
                && declared.is_none_or(|value| value == category) =>
        {
            (Some(category), source)
        }
        Some(_) => (None, "Needs review: conflicting evidence"),
        None => (None, "Unclassified"),
    }
}

fn known_evidence(
    provider: &Provider,
    model: &str,
    fields: &[ModelField],
    declared: Option<Category>,
) -> Option<(Category, &'static str)> {
    // A multi-endpoint model may legitimately support more than one category.
    // Prefer its saved category when that category has positive upstream evidence.
    let upstream = declared.into_iter().chain(Category::ALL).find(|category| {
        category.accepts(provider)
            && category.suggests_fields(fields)
            && category.matches_model(provider, model, fields)
    });
    upstream
        .map(|c| (c, "Upstream metadata"))
        .or_else(|| crate::providers::model_catalog::evidence(provider, model))
        .or_else(|| declared.map(|c| (c, "Manual declaration")))
        .or_else(|| {
            let mut accepted = Category::ALL
                .into_iter()
                .filter(|category| category.accepts(provider));
            let only = accepted.next()?;
            accepted
                .next()
                .is_none()
                .then_some((only, "Single-category protocol"))
        })
}

/// Explain the exact checks that made the classification fail closed. Only
/// classification metadata is included, never credentials or request content.
pub(crate) fn conflict_details(
    provider: &Provider,
    model: &str,
    fields: &[ModelField],
    declared: Option<Category>,
) -> String {
    if evidence(provider, model, fields, declared).1 != "Needs review: conflicting evidence" {
        return String::new();
    }
    let Some((category, source)) = known_evidence(provider, model, fields, declared) else {
        return String::new();
    };
    let mut reasons = vec![format!(
        "{source} identifies this model as {}.",
        category.label()
    )];
    if let Some(declared) = declared.filter(|value| *value != category) {
        reasons.push(format!("The saved declaration is {}, which conflicts with that category. Remove referencing combo steps before changing the declaration.", declared.label()));
    }
    if !category.accepts(provider) {
        reasons.push(format!(
            "The connection protocol ({}) does not support the {} category.",
            provider.protocol.as_str(),
            category.label()
        ));
    }
    if !category.matches_fields(fields) {
        reasons.push(format!(
            "Upstream endpoint or input/output modality metadata does not permit {}.",
            category.label()
        ));
        for field in fields.iter().filter(|field| {
            let label = field
                .label
                .to_ascii_lowercase()
                .replace([' ', '_', '-'], "");
            label.ends_with("supportedendpoints")
                || label.ends_with("inputmodalities")
                || label.ends_with("outputmodalities")
        }) {
            reasons.push(format!("{}: {}.", field.label, field.value));
        }
    } else if !category.matches_model(provider, model, fields)
        && let Some((catalog, source)) = crate::providers::model_catalog::evidence(provider, model)
    {
        reasons.push(format!(
            "{source} assigns {}, which conflicts with {}.",
            catalog.label(),
            category.label()
        ));
    }
    reasons.join(" ")
}

/// Save a declaration under the same write lock and constraints as combo edits.
pub(crate) async fn declare(
    pool: &sqlx::SqlitePool,
    provider_id: &str,
    model: &str,
    category: Category,
) -> Result<(), AppError> {
    if model.trim().is_empty() || model.chars().count() > 256 || model.chars().any(char::is_control)
    {
        return Err(AppError::InvalidRequest(
            "Model ID must be 1–256 characters without controls".into(),
        ));
    }
    let mut transaction = pool.begin_with("BEGIN IMMEDIATE").await?;
    validate(&mut transaction, provider_id, model, category, true).await?;
    transaction.commit().await?;
    Ok(())
}

/// Remove manual evidence only when the automatic default keeps all referenced
/// combos valid. Unknown defaults cannot replace required explicit declarations.
pub(crate) async fn reset(
    pool: &sqlx::SqlitePool,
    provider_id: &str,
    model: &str,
) -> Result<(), AppError> {
    if model.is_empty() || model.chars().count() > 256 || model.chars().any(char::is_control) {
        return Err(AppError::InvalidRequest("Invalid model ID".into()));
    }
    let mut transaction = pool.begin_with("BEGIN IMMEDIATE").await?;
    let row = sqlx::query("SELECT protocol, base_url FROM providers WHERE id = ?")
        .bind(provider_id)
        .fetch_optional(&mut *transaction)
        .await?
        .ok_or_else(|| AppError::NotFound("Connection was not found".into()))?;
    let provider = Provider {
        id: provider_id.into(),
        name: String::new(),
        base_url: row.try_get("base_url")?,
        protocol: row.try_get::<String, _>("protocol")?.parse()?,
        encrypted_api_key: Vec::new(),
        enabled: true,
    };
    let details: Option<String> = sqlx::query_scalar(
        "SELECT details FROM connection_models WHERE provider_id = ? AND model_id = ?",
    )
    .bind(provider_id)
    .bind(model)
    .fetch_optional(&mut *transaction)
    .await?;
    let fields: Vec<ModelField> = match details {
        Some(details) => {
            let value: serde_json::Value = serde_json::from_str(&details)
                .map_err(|_| AppError::Configuration("Invalid stored model metadata".into()))?;
            match value.get("fields") {
                Some(fields) => serde_json::from_value(fields.clone())
                    .map_err(|_| AppError::Configuration("Invalid stored model metadata".into()))?,
                None => Vec::new(),
            }
        }
        None => Vec::new(),
    };
    let default = evidence(&provider, model, &fields, None).0;
    let used_categories: Vec<String> = sqlx::query_scalar(
        "SELECT DISTINCT c.category FROM combo_steps s JOIN combos c ON c.id = s.combo_id
         WHERE s.provider_id = ? AND s.model = ?",
    )
    .bind(provider_id)
    .bind(model)
    .fetch_all(&mut *transaction)
    .await?;
    if used_categories
        .iter()
        .any(|used| default.is_none_or(|category| category.as_str() != used))
    {
        return Err(AppError::InvalidRequest(
            "Reset would invalidate a referencing combo: remove its steps first, or keep the explicit declaration".into()));
    }
    sqlx::query("DELETE FROM model_categories WHERE provider_id = ? AND model = ?")
        .bind(provider_id)
        .bind(model)
        .execute(&mut *transaction)
        .await?;
    transaction.commit().await?;
    Ok(())
}

/// Validate in the same transaction as a step mutation. Unknown IDs are an
/// explicit declaration in the combo's selected category; subsequent use in a
/// different category is rejected while any existing steps still reference it.
/// Validate capability evidence without database I/O. Used by both transactional
/// mutations and the batched routing snapshot; malformed metadata fails closed.
fn validate_capabilities(
    provider: &Provider,
    model: &str,
    category: Category,
    details: Option<&str>,
) -> Result<Vec<ModelField>, AppError> {
    if !category.accepts(provider) {
        return Err(AppError::InvalidRequest(
            "Connection protocol is incompatible with the combo category".into(),
        ));
    }
    let mut fields = Vec::new();
    if let Some(details) = details {
        let value: serde_json::Value = serde_json::from_str(details)
            .map_err(|_| AppError::Configuration("Invalid stored model metadata".into()))?;
        fields = match value.get("fields") {
            Some(fields) => serde_json::from_value(fields.clone())
                .map_err(|_| AppError::Configuration("Invalid stored model metadata".into()))?,
            None => Vec::new(),
        };
    }
    if !category.matches_model(provider, model, &fields) {
        return Err(AppError::InvalidRequest(
            "Model capabilities or known category do not match the combo category".into(),
        ));
    }
    Ok(fields)
}

/// Read-only equivalent of validate(..., false), using already loaded evidence.
pub(crate) fn validate_snapshot(
    provider: &Provider,
    model: &str,
    category: Category,
    details: Option<&str>,
    assigned: Option<&str>,
) -> Result<(), AppError> {
    let fields = validate_capabilities(provider, model, category, details)?;
    if let Some(assigned) = assigned {
        if assigned != category.as_str() {
            return Err(AppError::InvalidRequest("Model is already classified in a different category for this connection; remove its existing steps before reclassifying".into()));
        }
    } else if evidence(provider, model, &fields, None).0 != Some(category) {
        return Err(AppError::InvalidRequest(
            "Model category needs review: edit and save this combo step".into(),
        ));
    }
    Ok(())
}

pub(crate) async fn validate(
    connection: &mut SqliteConnection,
    provider_id: &str,
    model: &str,
    category: Category,
    declare: bool,
) -> Result<(), AppError> {
    let row = sqlx::query("SELECT protocol, base_url FROM providers WHERE id = ?")
        .bind(provider_id)
        .fetch_optional(&mut *connection)
        .await?
        .ok_or_else(|| AppError::InvalidRequest("Selected connection does not exist".into()))?;
    let provider = Provider {
        id: provider_id.into(),
        name: String::new(),
        base_url: row.try_get("base_url")?,
        protocol: row.try_get::<String, _>("protocol")?.parse()?,
        encrypted_api_key: Vec::new(),
        enabled: true,
    };
    if !category.accepts(&provider) {
        return Err(AppError::InvalidRequest(
            "Connection protocol is incompatible with the combo category".into(),
        ));
    }
    let details: Option<String> = sqlx::query_scalar(
        "SELECT details FROM connection_models WHERE provider_id = ? AND model_id = ?",
    )
    .bind(provider_id)
    .bind(model)
    .fetch_optional(&mut *connection)
    .await?;
    if !declare {
        let assigned: Option<String> = sqlx::query_scalar(
            "SELECT category FROM model_categories WHERE provider_id = ? AND model = ?",
        )
        .bind(provider_id)
        .bind(model)
        .fetch_optional(&mut *connection)
        .await?;
        return validate_snapshot(
            &provider,
            model,
            category,
            details.as_deref(),
            assigned.as_deref(),
        );
    }
    validate_capabilities(&provider, model, category, details.as_deref())?;
    let assigned: Option<String> = sqlx::query_scalar(
        "SELECT category FROM model_categories WHERE provider_id = ? AND model = ?",
    )
    .bind(provider_id)
    .bind(model)
    .fetch_optional(&mut *connection)
    .await?;
    if let Some(assigned) = assigned {
        if assigned != category.as_str() {
            let uses: i64 = sqlx::query_scalar(
                "SELECT COUNT(*) FROM combo_steps WHERE provider_id = ? AND model = ?",
            )
            .bind(provider_id)
            .bind(model)
            .fetch_one(&mut *connection)
            .await?;
            if !declare || uses != 0 {
                return Err(AppError::InvalidRequest("Model is already classified in a different category for this connection; remove its existing steps before reclassifying".into()));
            }
            sqlx::query(
                "UPDATE model_categories SET category = ? WHERE provider_id = ? AND model = ?",
            )
            .bind(category.as_str())
            .bind(provider_id)
            .bind(model)
            .execute(&mut *connection)
            .await?;
        }
    } else if declare {
        sqlx::query("INSERT INTO model_categories(provider_id, model, category) VALUES (?, ?, ?)")
            .bind(provider_id)
            .bind(model)
            .bind(category.as_str())
            .execute(&mut *connection)
            .await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn conflict_details_name_the_disagreeing_sources_and_constraints() {
        let mut provider = Provider {
            id: "test".into(),
            name: "test".into(),
            base_url: "https://api.openai.com/v1".into(),
            protocol: Protocol::OpenAi,
            encrypted_api_key: vec![],
            enabled: true,
        };
        let fields = [ModelField {
            label: "output modalities".into(),
            value: "text".into(),
        }];
        let reason = conflict_details(&provider, "gpt-image-1", &fields, None);
        assert!(reason.contains("Built-in catalog identifies this model as Image"));
        assert!(reason.contains("metadata does not permit Image"));
        assert!(reason.contains("output modalities: text"));
        let reason = conflict_details(&provider, "gpt-image-1", &[], Some(Category::Text));
        assert!(reason.contains("saved declaration is Text"));
        assert!(conflict_details(&provider, "gpt-image-1", &[], Some(Category::Image)).is_empty());
        provider.protocol = Protocol::Anthropic;
        let reason = conflict_details(&provider, "unknown", &[], Some(Category::Image));
        assert!(
            reason.contains("connection protocol (anthropic) does not support the Image category")
        );
    }

    #[test]
    fn provenance_respects_scope_precedence_and_conflicts() {
        let mut provider = Provider {
            id: "test".into(),
            name: "test".into(),
            base_url: "https://api.openai.com/v1".into(),
            protocol: Protocol::OpenAi,
            encrypted_api_key: vec![],
            enabled: true,
        };
        assert_eq!(
            evidence(&provider, "gpt-image-1", &[], None),
            (Some(Category::Image), "Built-in catalog")
        );
        let fields = [ModelField {
            label: "supported_endpoints".into(),
            value: "chat".into(),
        }];
        assert_eq!(
            evidence(&provider, "gpt-image-1", &fields, None),
            (Some(Category::Text), "Upstream metadata")
        );
        assert_eq!(
            evidence(&provider, "gpt-image-1", &fields, Some(Category::Image)),
            (None, "Needs review: conflicting evidence")
        );
        let multiple = [ModelField {
            label: "supported_endpoints".into(),
            value: "chat, images".into(),
        }];
        assert_eq!(
            evidence(&provider, "gpt-image-1", &multiple, Some(Category::Image)),
            (Some(Category::Image), "Upstream metadata")
        );
        provider.base_url = "https://api.groq.com/openai/v1".into();
        assert_eq!(
            evidence(&provider, "whisper-large-v3", &[], None),
            (Some(Category::Stt), "models.dev")
        );
        provider.base_url = "https://custom.invalid/v1".into();
        assert_eq!(
            evidence(&provider, "whisper-large-v3", &[], None),
            (None, "Unclassified")
        );
        assert_eq!(
            evidence(&provider, "custom", &[], Some(Category::Music)),
            (Some(Category::Music), "Manual declaration")
        );
        provider.protocol = Protocol::SystemOne;
        assert_eq!(
            evidence(&provider, "custom", &[], None),
            (Some(Category::Decision), "Single-category protocol")
        );
    }
}
