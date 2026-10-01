//! Last refreshed model list for a provider connection.
//!
//! The combo editor reads the IDs for suggestions. Routing still accepts any
//! model string; a missing or stale list never blocks a request.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use sqlx::{Row, SqlitePool};

use crate::error::AppError;

/// One scalar field shown on a model card. Labels are display text, not raw keys.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ModelField {
    pub label: String,
    pub value: String,
}

/// One model from a connection's last refresh, in upstream order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoredModel {
    pub id: String,
    pub name: String,
    pub fields: Vec<ModelField>,
}

impl StoredModel {
    /// Text the dashboard search box matches: name, id, and every displayed field.
    pub fn search_text(&self) -> String {
        let mut text = format!("{} {}", self.name, self.id);
        for field in &self.fields {
            text.push(' ');
            text.push_str(&field.label);
            text.push(' ');
            text.push_str(&field.value);
        }
        text
    }
}

/// Models saved for one connection, in upstream order.
pub struct ModelList {
    pub ids: Vec<String>,
    pub models: Vec<StoredModel>,
    pub truncated: bool,
}

pub async fn replace(
    pool: &SqlitePool,
    provider_id: &str,
    models: &[StoredModel],
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
    for (position, model) in models.iter().enumerate() {
        let details = serde_json::json!({
            "name": model.name,
            "fields": model.fields,
        })
        .to_string();
        sqlx::query(
            "INSERT INTO connection_models (provider_id, position, model_id, details)
             VALUES (?, ?, ?, ?)",
        )
        .bind(provider_id)
        .bind(i64::try_from(position).unwrap_or(i64::MAX))
        .bind(&model.id)
        .bind(details)
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
                models: Vec::new(),
                truncated: truncated != 0,
            },
        );
    }
    let models = sqlx::query(
        "SELECT provider_id, model_id, details
         FROM connection_models ORDER BY provider_id, position",
    )
    .fetch_all(pool)
    .await?;
    for row in models {
        let id: String = row.try_get("provider_id")?;
        if let Some(list) = lists.get_mut(&id) {
            let model_id: String = row.try_get("model_id")?;
            let details: String = row.try_get("details")?;
            let model = stored_model(&model_id, &details);
            list.ids.push(model.id.clone());
            list.models.push(model);
        }
    }
    Ok(lists)
}

/// A missing or unreadable details blob still yields the stored ID.
fn stored_model(id: &str, details: &str) -> StoredModel {
    #[derive(Deserialize)]
    struct Details {
        name: Option<String>,
        fields: Option<Vec<ModelField>>,
    }
    let parsed = serde_json::from_str::<Details>(details).unwrap_or(Details {
        name: None,
        fields: None,
    });
    let name = parsed
        .name
        .filter(|name| !name.trim().is_empty())
        .unwrap_or_else(|| id.to_owned());
    StoredModel {
        id: id.to_owned(),
        name,
        fields: parsed.fields.unwrap_or_default(),
    }
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
            &[
                StoredModel {
                    id: "second".into(),
                    name: "Second".into(),
                    fields: vec![ModelField {
                        label: "Owned by".into(),
                        value: "lab".into(),
                    }],
                },
                StoredModel {
                    id: "first".into(),
                    name: "First".into(),
                    fields: Vec::new(),
                },
            ],
            true,
        )
        .await
        .unwrap();
        replace(
            &pool,
            "connection",
            &[StoredModel {
                id: "only".into(),
                name: "Only model".into(),
                fields: vec![ModelField {
                    label: "Context length".into(),
                    value: "128000".into(),
                }],
            }],
            false,
        )
        .await
        .unwrap();
        let lists = for_connections(&pool, &["connection".into()])
            .await
            .unwrap();
        let list = lists.get("connection").unwrap();
        assert_eq!(list.ids, vec!["only".to_owned()]);
        assert_eq!(list.models[0].name, "Only model");
        assert_eq!(list.models[0].fields[0].value, "128000");
        assert!(!list.truncated);
        providers::delete(&pool, "connection").await.unwrap();
        let remaining: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM connection_models")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(remaining, 0);
    }
}
