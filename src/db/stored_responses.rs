//! Which connection created each stored Responses API response.
//!
//! Written through the history writer, so recording a response never waits
//! on SQLite or invalidates the configuration cache. Lookups are exact by ID
//! and only succeed for the caller that created the response.

use sqlx::{Row, SqliteConnection, SqlitePool};

use crate::error::AppError;

/// OpenAI keeps stored responses for 30 days by default.
const RETENTION_SECONDS: i64 = 30 * 86_400;

pub(crate) struct StoredResponse {
    pub provider_id: String,
    pub model: String,
}

/// IDs as providers issue them (`resp_…`): letters, digits, `-` and `_`.
pub(crate) fn valid_id(id: &str) -> bool {
    (1..=128).contains(&id.len())
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}

/// The response's connection and public model, if `scope` created it.
pub(crate) async fn get(
    pool: &SqlitePool,
    id: &str,
    scope: &str,
) -> Result<Option<StoredResponse>, AppError> {
    let row = sqlx::query(
        "SELECT provider_id, model FROM stored_responses
         WHERE id = ? AND scope = ? AND created_at > unixepoch() - ?",
    )
    .bind(id)
    .bind(scope)
    .bind(RETENTION_SECONDS)
    .fetch_optional(pool)
    .await?;
    row.map(|row| {
        Ok(StoredResponse {
            provider_id: row.try_get("provider_id")?,
            model: row.try_get("model")?,
        })
    })
    .transpose()
}

pub(crate) async fn insert(
    connection: &mut SqliteConnection,
    id: &str,
    provider_id: &str,
    scope: &str,
    model: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO stored_responses (id, provider_id, scope, model) VALUES (?, ?, ?, ?)
         ON CONFLICT (id) DO UPDATE SET provider_id = excluded.provider_id,
             scope = excluded.scope, model = excluded.model, created_at = excluded.created_at",
    )
    .bind(id)
    .bind(provider_id)
    .bind(scope)
    .bind(model)
    .execute(connection)
    .await?;
    Ok(())
}

pub(crate) async fn delete(connection: &mut SqliteConnection, id: &str) -> Result<(), sqlx::Error> {
    sqlx::query("DELETE FROM stored_responses WHERE id = ?")
        .bind(id)
        .execute(connection)
        .await?;
    Ok(())
}

pub(crate) async fn prune(connection: &mut SqliteConnection) -> Result<(), sqlx::Error> {
    sqlx::query("DELETE FROM stored_responses WHERE created_at <= unixepoch() - ?")
        .bind(RETENTION_SECONDS)
        .execute(connection)
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_are_bounded_url_safe_tokens() {
        assert!(valid_id(
            "resp_67ccd2bed1ec8190b14f964abc0542670bb6a6b452d3795b"
        ));
        assert!(valid_id("a-b_C9"));
        for id in ["", "../x", "resp 1", "resp/1", "résp", &"x".repeat(129)] {
            assert!(!valid_id(id), "{id}");
        }
    }
}
