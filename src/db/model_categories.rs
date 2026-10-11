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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Category {
    #[default]
    Text,
    Decision,
    Image,
    Video,
    Music,
    Tts,
    Stt,
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

    pub const ALL: [Self; 7] = [
        Self::Text,
        Self::Decision,
        Self::Image,
        Self::Video,
        Self::Music,
        Self::Tts,
        Self::Stt,
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
                // Audio output alone cannot distinguish music from speech.
                Self::Music | Self::Decision => false,
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
                Self::Decision => false,
            })
        })
    }
}

/// Validate in the same transaction as a step mutation. Unknown IDs are an
/// explicit declaration in the combo's selected category; subsequent use in a
/// different category is rejected while any existing steps still reference it.
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
    let mut fields = Vec::new();
    if let Some(details) = details {
        let value: serde_json::Value = serde_json::from_str(&details)
            .map_err(|_| AppError::Configuration("Invalid stored model metadata".into()))?;
        fields = match value.get("fields") {
            Some(fields) => serde_json::from_value(fields.clone())
                .map_err(|_| AppError::Configuration("Invalid stored model metadata".into()))?,
            None => Vec::new(),
        };
    }
    if !category.matches_model(&provider, model, &fields) {
        return Err(AppError::InvalidRequest(
            "Model capabilities or known category do not match the combo category".into(),
        ));
    }
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
    } else {
        return Err(AppError::InvalidRequest(
            "Model category needs review: edit and save this combo step".into(),
        ));
    }
    Ok(())
}
