//! Last refreshed model list for a provider connection.
//!
//! Combo steps still accept any model string; a missing list never blocks a
//! combo. Direct requests use this snapshot as the routing table: an ID is
//! reachable only when an enabled connection's last refresh listed it.

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

    /// Only context size is shown on a card; other fields stay in the popup.
    /// Context counts are abbreviated here, including previously saved lists.
    pub fn display_fields(&self) -> Vec<ModelField> {
        self.fields
            .iter()
            .filter(|field| useful_card_label(&field.label))
            .filter(|field| !field.value.contains("->"))
            .take(1)
            .cloned()
            .map(|mut field| {
                field.value = format_context_tag(&field.value);
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

    /// Exact cached token limits for discovery, not the rounded card tags.
    pub fn token_limits(&self) -> (Option<u64>, Option<u64>) {
        let limit = |label: &str, suffix: &str| {
            self.fields
                .iter()
                .filter(|field| field.label.eq_ignore_ascii_case(label))
                .find_map(|field| parse_token_count(&field.value, suffix))
        };
        (limit("Context", " ctx"), limit("Output", " output"))
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

fn parse_token_count(value: &str, suffix: &str) -> Option<u64> {
    let text = value.trim();
    let number = text.strip_suffix(suffix).unwrap_or(text);
    let parts: Vec<_> = number.split(['.', ',']).collect();
    if !parts
        .iter()
        .all(|part| !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_digit()))
        || (parts.len() > 1
            && (parts[0].len() > 3 || parts[1..].iter().any(|part| part.len() != 3)))
    {
        return None;
    }
    parts
        .concat()
        .parse::<u64>()
        .ok()
        .filter(|count| *count > 0)
}

fn format_context_tag(value: &str) -> String {
    let text = value.trim();
    let number = text
        .strip_suffix(" ctx")
        .or_else(|| text.strip_suffix(" context"))
        .unwrap_or(text);
    let parts: Vec<_> = number.split(['.', ',']).collect();
    let valid = parts
        .iter()
        .all(|part| !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_digit()))
        && (parts.len() == 1
            || (parts[0].len() <= 3 && parts[1..].iter().all(|part| part.len() == 3)));
    if valid && let Ok(count) = parts.concat().parse::<u64>() {
        let (divisor, suffix) = if count >= 1_000_000 {
            (1_000_000, "M")
        } else if count >= 1_000 {
            (1_000, "k")
        } else {
            return format!("{count} ctx");
        };
        // Round to hundredths using integers, avoiding floating-point drift.
        let hundredth = divisor / 100;
        let rounded = count / hundredth + u64::from(count % hundredth >= hundredth / 2);
        let whole = rounded / 100;
        let remainder = rounded % 100;
        if remainder == 0 {
            return format!("{whole}{suffix} ctx");
        }
        let fraction = format!("{remainder:02}");
        return format!("{whole}.{}{suffix} ctx", fraction.trim_end_matches('0'));
    }
    if text.to_ascii_lowercase().contains("ctx") {
        text.to_owned()
    } else {
        format!("{number} ctx")
    }
}

fn format_info_value(label: &str, value: &str) -> String {
    let label = label.to_ascii_lowercase();
    if label.contains("web search") || label.contains("web_search") {
        return format_currency(value).unwrap_or_else(|| value.to_owned());
    }
    if (label.contains("pricing") || label.contains("price"))
        && let Some(formatted) = format_per_million(&label, value)
    {
        return formatted;
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
    label.contains("context")
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
    pub refreshed_at: String,
    pub stale: bool,
}

pub struct RefreshStatus {
    pub attempted_at: String,
    pub error: String,
}

pub async fn refresh_failed(
    pool: &SqlitePool,
    provider_id: &str,
    error: &str,
) -> Result<(), AppError> {
    // Callers supply only fixed, sanitized messages, never upstream bodies/URLs.
    sqlx::query("INSERT INTO connection_refresh_status(provider_id, attempted_at, error)
        VALUES (?, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'), ?)
        ON CONFLICT(provider_id) DO UPDATE SET attempted_at = excluded.attempted_at, error = excluded.error")
        .bind(provider_id).bind(error).execute(pool).await?;
    Ok(())
}

pub async fn refresh_statuses(
    pool: &SqlitePool,
) -> Result<HashMap<String, RefreshStatus>, AppError> {
    let rows =
        sqlx::query("SELECT provider_id, attempted_at, error FROM connection_refresh_status")
            .fetch_all(pool)
            .await?;
    rows.into_iter()
        .map(|row| {
            Ok((
                row.try_get("provider_id")?,
                RefreshStatus {
                    attempted_at: row.try_get("attempted_at")?,
                    error: row.try_get("error")?,
                },
            ))
        })
        .collect()
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
    sqlx::query(
        "INSERT INTO connection_refresh_status(provider_id, attempted_at, error)
        VALUES (?, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'), '')
        ON CONFLICT(provider_id) DO UPDATE SET attempted_at = excluded.attempted_at, error = ''",
    )
    .bind(provider_id)
    .execute(&mut *transaction)
    .await?;
    transaction.commit().await?;
    Ok(())
}

/// Connections whose last refresh listed `model_id`, oldest connection first.
/// Disabled connections are included so the caller can tell "not listed" from
/// "listed, but unavailable".
pub async fn providers_for_model(
    pool: &SqlitePool,
    model_id: &str,
) -> Result<Vec<String>, AppError> {
    let rows = sqlx::query(
        "SELECT p.id
         FROM connection_models m
         JOIN providers p ON p.id = m.provider_id
         WHERE m.model_id = ?
         ORDER BY p.rowid",
    )
    .bind(model_id)
    .fetch_all(pool)
    .await?;
    rows.iter().map(|row| Ok(row.try_get("id")?)).collect()
}

/// Connections listing this public provider/model ID, oldest first. The prefix
/// is removed exactly once; slashes inside the upstream model stay unchanged.
pub async fn providers_for_public_model(
    pool: &SqlitePool,
    slug: &str,
    upstream_model: &str,
) -> Result<Vec<String>, AppError> {
    let namespaces = super::provider_slugs::for_connections(pool).await?;
    Ok(providers_for_model(pool, upstream_model)
        .await?
        .into_iter()
        .filter(|id| {
            namespaces
                .get(id)
                .is_some_and(|namespace| namespace == slug)
        })
        .collect())
}

/// Distinct provider-prefixed public IDs on enabled connections, in ID order.
pub async fn routable_model_ids(pool: &SqlitePool) -> Result<Vec<String>, AppError> {
    let namespaces = super::provider_slugs::for_connections(pool).await?;
    let rows = sqlx::query(
        "SELECT m.provider_id, m.model_id
         FROM connection_models m
         JOIN providers p ON p.id = m.provider_id
         WHERE p.enabled != 0
         ORDER BY m.model_id",
    )
    .fetch_all(pool)
    .await?;
    let mut ids = std::collections::BTreeSet::new();
    for row in rows {
        let provider_id: String = row.try_get("provider_id")?;
        let id: String = row.try_get("model_id")?;
        if let Some(slug) = namespaces.get(&provider_id) {
            ids.insert(format!("{slug}/{id}"));
        }
    }
    Ok(ids.into_iter().collect())
}

/// Cached details for public direct IDs, using the oldest enabled connection
/// listing each ID, exactly as direct routing chooses its first attempt.
pub async fn public_model_details(
    pool: &SqlitePool,
) -> Result<std::collections::BTreeMap<String, StoredModel>, AppError> {
    let namespaces = super::provider_slugs::for_connections(pool).await?;
    let rows = sqlx::query(
        "SELECT m.provider_id, m.model_id, m.details FROM connection_models m
         JOIN providers p ON p.id = m.provider_id WHERE p.enabled != 0 ORDER BY p.rowid, m.position"
    ).fetch_all(pool).await?;
    let mut models = std::collections::BTreeMap::new();
    for row in rows {
        let provider_id: String = row.try_get("provider_id")?;
        let id: String = row.try_get("model_id")?;
        let details: String = row.try_get("details")?;
        if let Some(slug) = namespaces.get(&provider_id) {
            models
                .entry(format!("{slug}/{id}"))
                .or_insert_with(|| stored_model(&id, &details));
        }
    }
    Ok(models)
}

/// First configured step only, including disabled connections. Missing cached
/// details never borrow another step's limits or affect combo routability.
pub async fn first_combo_model_details(
    pool: &SqlitePool,
) -> Result<HashMap<String, StoredModel>, AppError> {
    let rows = sqlx::query(
        "SELECT s.combo_id, m.model_id, m.details FROM combo_steps s
         JOIN connection_models m ON m.provider_id = s.provider_id AND m.model_id = s.model
         WHERE NOT EXISTS (SELECT 1 FROM combo_steps earlier
             WHERE earlier.combo_id = s.combo_id AND earlier.position < s.position)",
    )
    .fetch_all(pool)
    .await?;
    let mut models = HashMap::new();
    for row in rows {
        let id: String = row.try_get("model_id")?;
        let details: String = row.try_get("details")?;
        models.insert(row.try_get("combo_id")?, stored_model(&id, &details));
    }
    Ok(models)
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
    let rows = sqlx::query(
        "SELECT provider_id, truncated, refreshed_at,
        (julianday('now') - julianday(refreshed_at) >= 1) AS stale FROM connection_model_lists",
    )
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
                refreshed_at: row.try_get("refreshed_at")?,
                stale: row.try_get::<i64, _>("stale")? != 0,
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

    #[test]
    fn context_tags_abbreviate_without_changing_info_counts() {
        for (value, expected) in [
            ("100000", "100k ctx"),
            ("100.000 context", "100k ctx"),
            ("128.000 ctx", "128k ctx"),
            ("1.000.000", "1M ctx"),
            ("1,000,000 ctx", "1M ctx"),
            ("1.050.000 ctx", "1.05M ctx"),
            ("1500", "1.5k ctx"),
            ("1048576", "1.05M ctx"),
            ("1051235", "1.05M ctx"),
            ("1055000", "1.06M ctx"),
            ("1999999", "2M ctx"),
            ("123456", "123.46k ctx"),
            ("123450", "123.45k ctx"),
            ("1004999", "1M ctx"),
            ("1100000", "1.1M ctx"),
            ("512", "512 ctx"),
            ("unknown", "unknown ctx"),
            ("1.5M ctx", "1.5M ctx"),
        ] {
            let model = StoredModel {
                id: "test".into(),
                name: "Test".into(),
                fields: vec![ModelField {
                    label: "Context".into(),
                    value: value.into(),
                }],
                free: false,
            };
            assert_eq!(model.display_fields()[0].value, expected, "{value}");
            assert_eq!(model.info_fields()[0].value, group_plain_number(value));
            assert_eq!(model.fields[0].value, value);
        }
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
        assert_eq!(legacy.display_fields()[0].value, "1M ctx");
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

    #[tokio::test]
    async fn direct_lookup_follows_connection_age_and_enabled_state() {
        let pool = pool().await;
        provider(&pool, "older").await;
        provider(&pool, "newer").await;
        provider(&pool, "off").await;
        sqlx::query("UPDATE providers SET enabled = 0 WHERE id = 'off'")
            .execute(&pool)
            .await
            .unwrap();
        let listed = StoredModel {
            id: "openai/gpt-6-luna".into(),
            name: "GPT-6 Luna".into(),
            fields: Vec::new(),
            free: false,
        };
        for id in ["older", "newer", "off"] {
            replace(&pool, id, std::slice::from_ref(&listed), false)
                .await
                .unwrap();
        }
        replace(
            &pool,
            "newer",
            &[
                listed.clone(),
                StoredModel {
                    id: "combo/not-a-combo".into(),
                    name: "Reserved".into(),
                    fields: Vec::new(),
                    free: false,
                },
            ],
            false,
        )
        .await
        .unwrap();

        assert_eq!(
            providers_for_model(&pool, "openai/gpt-6-luna")
                .await
                .unwrap(),
            vec!["older".to_owned(), "newer".to_owned(), "off".to_owned()]
        );
        assert!(
            providers_for_model(&pool, "missing")
                .await
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            routable_model_ids(&pool).await.unwrap(),
            vec![
                "connection-newer/combo/not-a-combo".to_owned(),
                "connection-newer/openai/gpt-6-luna".to_owned(),
                "connection-older/openai/gpt-6-luna".to_owned(),
            ]
        );
        assert_eq!(
            providers_for_public_model(&pool, "connection-older", "openai/gpt-6-luna")
                .await
                .unwrap(),
            vec!["older"]
        );
        assert_eq!(
            providers_for_public_model(&pool, "connection-off", "openai/gpt-6-luna")
                .await
                .unwrap(),
            vec!["off"]
        );
        assert!(
            providers_for_public_model(&pool, "unknown", "openai/gpt-6-luna")
                .await
                .unwrap()
                .is_empty()
        );
    }
}
