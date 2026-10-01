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
    /// True when the provider marked this model free, by id or by a zero price.
    pub free: bool,
}

impl StoredModel {
    /// True for a stored free flag, a `:free` id, or an older "Is free: yes" field.
    pub fn is_free(&self) -> bool {
        self.free
            || self
                .id
                .rsplit([':', '/'])
                .next()
                .is_some_and(|part| part.eq_ignore_ascii_case("free"))
            || self.fields.iter().any(|field| {
                let label = field.label.to_ascii_lowercase();
                (label == "is free" || label == "free") && truthy(&field.value)
            })
    }

    /// Fields worth showing on a card. Older snapshots may still contain dates,
    /// owners, and pricing; those stay stored but are not displayed. Plain
    /// context numbers are grouped here so an old refresh still reads 1.000.000.
    pub fn display_fields(&self) -> Vec<ModelField> {
        self.fields
            .iter()
            .filter(|field| useful_card_label(&field.label))
            .take(2)
            .cloned()
            .filter(|field| !field.value.contains("->"))
            .map(|mut field| {
                field.value = group_plain_number(&field.value);
                if field.label.to_ascii_lowercase().contains("context") {
                    if let Some(number) = field.value.strip_suffix(" context") {
                        field.value = format!("{number} ctx");
                    } else if !field.value.to_ascii_lowercase().contains("ctx") {
                        field.value.push_str(" ctx");
                    }
                }
                field
            })
            .collect()
    }

    /// Every stored field for the details popup. Token rates are shown per
    /// million tokens. Web search stays a per-request price.
    pub fn info_fields(&self) -> Vec<ModelField> {
        self.fields
            .iter()
            .cloned()
            .map(|mut field| {
                field.value = format_info_value(&field.label, &field.value);
                field
            })
            .collect()
    }

    /// Text the dashboard search box matches: name, id, and the visible chips.
    pub fn search_text(&self) -> String {
        let mut text = format!("{} {}", self.name, self.id);
        for field in self.display_fields() {
            text.push(' ');
            text.push_str(&field.value);
        }
        if self.is_free() {
            text.push_str(" free");
        }
        text
    }
}

fn format_info_value(label: &str, value: &str) -> String {
    let label = label.to_ascii_lowercase();
    if label.contains("web search") || label.contains("web_search") {
        return format_currency(value).unwrap_or_else(|| value.to_owned());
    }
    if label.contains("pricing") || label.contains("price") {
        if let Some(formatted) = format_per_million(&label, value) {
            return formatted;
        }
    }
    group_plain_number(value)
}

fn format_per_million(label: &str, value: &str) -> Option<String> {
    if value.contains("/ 1M") {
        return Some(value.to_owned());
    }
    let amount = parse_amount(value)?;
    let per_million = if label.contains("per million") || label.contains("per_million") {
        amount
    } else {
        amount * 1_000_000.0
    };
    Some(format!("${} / 1M tokens", format_money(per_million)))
}

fn format_currency(value: &str) -> Option<String> {
    if value.starts_with('$') && !value.contains("/ 1M") {
        return Some(value.to_owned());
    }
    Some(format!("${}", format_money(parse_amount(value)?)))
}

fn parse_amount(value: &str) -> Option<f64> {
    let text = value.trim().trim_start_matches('$').trim();
    let text = text.split('/').next()?.trim().replace(',', "");
    if text.is_empty() {
        return None;
    }
    text.parse().ok()
}

fn format_money(amount: f64) -> String {
    let raw = format!("{amount:.8}");
    let trimmed = raw.trim_end_matches('0').trim_end_matches('.');
    match trimmed.split_once('.') {
        Some((whole, fraction)) if fraction.len() < 2 => {
            format!("{whole}.{fraction}{}", "0".repeat(2 - fraction.len()))
        }
        Some(_) => trimmed.to_owned(),
        None => format!("{trimmed}.00"),
    }
}

fn useful_card_label(label: &str) -> bool {
    let label = label.to_ascii_lowercase();
    label != "modality" && (label.contains("context") || label.contains("output"))
}

fn truthy(value: &str) -> bool {
    matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "yes" | "true" | "1" | "free"
    )
}

/// Groups a leading plain integer with dots: `1050000` becomes `1.050.000`.
/// Values that are already grouped, or are not a plain integer, are unchanged.
pub fn group_plain_number(value: &str) -> String {
    let (head, rest) = value.split_once(' ').unwrap_or((value, ""));
    if head.len() < 4 || !head.chars().all(|ch| ch.is_ascii_digit()) {
        return value.to_owned();
    }
    let mut grouped = String::new();
    for (index, ch) in head.chars().rev().enumerate() {
        if index > 0 && index % 3 == 0 {
            grouped.push('.');
        }
        grouped.push(ch);
    }
    let grouped: String = grouped.chars().rev().collect();
    if rest.is_empty() {
        grouped
    } else {
        format!("{grouped} {rest}")
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
            "free": model.free,
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
        free: Option<bool>,
    }
    let parsed = serde_json::from_str::<Details>(details).unwrap_or(Details {
        name: None,
        fields: None,
        free: None,
    });
    let name = parsed
        .name
        .filter(|name| !name.trim().is_empty())
        .unwrap_or_else(|| id.to_owned());
    StoredModel {
        id: id.to_owned(),
        name,
        fields: parsed.fields.unwrap_or_default(),
        free: parsed.free.unwrap_or(false),
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
                    free: false,
                },
                StoredModel {
                    id: "first".into(),
                    name: "First".into(),
                    fields: Vec::new(),
                    free: true,
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
                free: false,
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
        let legacy = StoredModel {
            id: "legacy".into(),
            name: "Legacy".into(),
            fields: vec![
                ModelField {
                    label: "Context length".into(),
                    value: "1000000".into(),
                },
                ModelField {
                    label: "Is free".into(),
                    value: "yes".into(),
                },
            ],
            free: false,
        };
        assert!(legacy.is_free());
        assert_eq!(legacy.display_fields()[0].value, "1.000.000 ctx");
        let priced = StoredModel {
            id: "priced".into(),
            name: "Priced".into(),
            fields: vec![
                ModelField {
                    label: "Pricing · Prompt".into(),
                    value: "0.000002".into(),
                },
                ModelField {
                    label: "Pricing · Web search".into(),
                    value: "0.01".into(),
                },
            ],
            free: false,
        };
        let info = priced.info_fields();
        assert_eq!(info[0].value, "$2.00 / 1M tokens");
        assert_eq!(info[1].value, "$0.01");
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
