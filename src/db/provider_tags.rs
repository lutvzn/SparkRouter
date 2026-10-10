//! Dashboard-only capability labels, independent of routing protocols.
use std::collections::HashMap;

use sqlx::SqlitePool;

use crate::{error::AppError, providers::Protocol};

pub const TAGS: &[(&str, &str)] = &[
    ("text", "Text"),
    ("image", "Image"),
    ("video", "Video"),
    ("music", "Music"),
    ("tts", "TTS"),
    ("stt", "STT"),
    ("decision", "Decision"),
];

pub fn defaults(protocol: &Protocol) -> Vec<String> {
    let tags: &[&str] = match protocol {
        Protocol::SystemOne => &["decision"],
        Protocol::Fal => &["image", "video", "music"],
        Protocol::Xai => &["text", "image", "video"],
        Protocol::MiniMax => &["text", "image", "video", "music", "tts", "stt"],
        Protocol::Deepgram => &["tts", "stt"],
        Protocol::ElevenLabs => &["music", "tts", "stt"],
        _ => &["text"],
    };
    tags.iter().map(|tag| (*tag).into()).collect()
}

pub async fn list(pool: &SqlitePool) -> Result<HashMap<String, Vec<String>>, AppError> {
    let rows: Vec<(String, String)> = sqlx::query_as("SELECT identity, tags FROM provider_tags")
        .fetch_all(pool)
        .await?;
    rows.into_iter()
        .map(|(id, json)| {
            let tags = serde_json::from_str(&json)
                .map_err(|_| AppError::Configuration("invalid stored provider tags".into()))?;
            Ok((id, tags))
        })
        .collect()
}

pub async fn save(
    pool: &SqlitePool,
    identity: &str,
    group_id: Option<&str>,
    tags: &[String],
) -> Result<(), AppError> {
    if tags
        .iter()
        .any(|tag| !TAGS.iter().any(|(value, _)| value == tag))
    {
        return Err(AppError::InvalidRequest("unknown provider tag".into()));
    }
    sqlx::query("INSERT INTO provider_tags(identity, group_id, tags) VALUES (?, ?, ?) ON CONFLICT(identity) DO UPDATE SET tags = excluded.tags")
        .bind(identity).bind(group_id).bind(serde_json::to_string(tags).expect("string tags serialize"))
        .execute(pool).await?;
    Ok(())
}
