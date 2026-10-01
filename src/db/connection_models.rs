//! Last refreshed model IDs for a provider connection.
//!
//! The combo editor reads this snapshot for suggestions. Routing still accepts
//! any model string; a missing or stale list never blocks a request.

use std::collections::HashMap;

use sqlx::{Row, SqlitePool};

use crate::error::AppError;

/// Models saved for one connection, in upstream order.
pub struct ModelList {
    pub ids: Vec<String>,
    pub truncated: bool,
}

pub async fn replace(
    pool: &SqlitePool,
    provider_id: &str,
    ids: &[String],
    truncated: bool,
) -> Result<(), AppError> {
    let mut transaction = pool.begin_with("BEGIN IMMEDIATE").await?;
    sqlx::query("DELETE FROM connection_model_lists WHERE provider_id = ?")
        .bind(provider_id)
        .execute(&mut *transaction)
        .await?;
    sqlx::query(
        "INSERT INTO connection_model_lists (provider_id, refreshed_at, truncated)
         VALUES (?, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'), ?)",
    )
    .bind(provider_id)
    .bind(i64::from(truncated))
    .execute(&mut *transaction)
    .await?;
    for (position, model_id) in ids.iter().enumerate() {
        sqlx::query(
            "INSERT INTO connection_models (provider_id, position, model_id) VALUES (?, ?, ?)",
        )
        .bind(provider_id)
        .bind(i64::try_from(position).unwrap_or(i64::MAX))
        .bind(model_id)
        .execute(&mut *transaction)
        .await?;
    }
    transaction.commit().await?;
    Ok(())
}

/// Lists for the given connections. Missing keys were never refreshed.
pub async fn for_connections(
    pool: &SqlitePool,
    provider_ids: &[String],
) -> Result<HashMap<String, ModelList>, AppError> {
    let mut lists = HashMap::new();
    if provider_ids.is_empty() {
        return Ok(lists);
    }
    let rows = sqlx::query("SELECT provider_id, truncated FROM connection_model_lists")
        .fetch_all(pool)
        .await?;
    for row in rows {
        let id: String = row.try_get("provider_id")?;
        if !provider_ids.iter().any(|candidate| candidate == &id) {
            continue;
        }
        let truncated: i64 = row.try_get("truncated")?;
        lists.insert(
            id,
            ModelList {
                ids: Vec::new(),
                truncated: truncated != 0,
            },
        );
    }
    let models = sqlx::query(
        "SELECT provider_id, model_id FROM connection_models ORDER BY provider_id, position",
    )
    .fetch_all(pool)
    .await?;
    for row in models {
        let id: String = row.try_get("provider_id")?;
        if let Some(list) = lists.get_mut(&id) {
            list.ids.push(row.try_get("model_id")?);
        }
    }
    Ok(lists)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::providers::{self, Provider};
    use crate::providers::Protocol;

    async fn pool() -> SqlitePool {
        crate::db::connect("sqlite::memory:")
            .await
            .expect("connect")
    }

    async fn provider(pool: &SqlitePool, id: &str) {
        providers::save(
            pool,
            &Provider {
                id: id.to_owned(),
                name: id.to_owned(),
                base_url: "https://example.test/v1".to_owned(),
                protocol: Protocol::OpenAi,
                encrypted_api_key: Vec::new(),
                enabled: true,
            },
        )
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn replace_keeps_order_and_cascades_with_the_connection() {
        let pool = pool().await;
        provider(&pool, "connection").await;
        replace(
            &pool,
            "connection",
            &["second".into(), "first".into()],
            true,
        )
        .await
        .unwrap();
        replace(&pool, "connection", &["only".into()], false)
            .await
            .unwrap();
        let lists = for_connections(&pool, &["connection".into()])
            .await
            .unwrap();
        let list = lists.get("connection").unwrap();
        assert_eq!(list.ids, vec!["only".to_owned()]);
        assert!(!list.truncated);
        providers::delete(&pool, "connection").await.unwrap();
        let remaining: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM connection_models")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(remaining, 0);
    }
}
