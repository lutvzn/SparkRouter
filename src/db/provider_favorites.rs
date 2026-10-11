//! Persisted dashboard-only preferences for preset and custom provider tiles.
use crate::error::AppError;
use sqlx::SqlitePool;
use std::collections::HashMap;

pub async fn list(pool: &SqlitePool) -> Result<HashMap<String, bool>, AppError> {
    let rows: Vec<(String, bool)> =
        sqlx::query_as("SELECT identity, favorite FROM provider_favorites")
            .fetch_all(pool)
            .await?;
    Ok(rows.into_iter().collect())
}

pub async fn save(
    pool: &SqlitePool,
    identity: &str,
    group_id: Option<&str>,
    favorite: bool,
) -> Result<(), AppError> {
    sqlx::query("INSERT INTO provider_favorites(identity, group_id, favorite) VALUES (?, ?, ?) ON CONFLICT(identity) DO UPDATE SET favorite = excluded.favorite")
        .bind(identity).bind(group_id).bind(favorite).execute(pool).await?;
    Ok(())
}
