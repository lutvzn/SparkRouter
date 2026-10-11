//! Credential-free configuration transfer between SparkRouter databases.
//!
//! [`export`] snapshots custom provider groups, named connections, and combos
//! with their ordered steps into a versioned JSON document. Credentials,
//! gateway API keys, cached model lists, and timestamps are never exported,
//! so a transferred document can be shared without leaking secrets.
//!
//! Imports revalidate the whole document and write it in one immediate
//! transaction with fresh ids: nothing existing is overwritten, merged, or
//! deduplicated, and any conflicting combo name aborts the entire import.
//! Imported connections always start disabled with an empty credential blob,
//! so each one must be reviewed, given a key, and enabled before routing.
//!
//! The dashboard index lists only catalog presets and custom provider
//! groups, so an imported connection with no group and no preset endpoint
//! would be invisible and unmanageable. The caller therefore injects a
//! preset-endpoint predicate; every ungrouped connection it does not
//! recognize is attached to its own fresh custom group, named after the
//! connection. Explicit groups in the document are never merged, with each
//! other or with synthesized ones.

use std::collections::{HashMap, HashSet};

use serde::{Deserialize, Serialize};
use sqlx::{Row, SqlitePool};

use crate::db::combos::{AffinityMode, AffinityOptions, Category, Strategy};
use crate::error::AppError;
use crate::providers::{Protocol, normalize_base_url};

/// Document version produced by [`export`] and required by every import.
pub const VERSION: u8 = 1;

/// Upper bound on an accepted document. The request body limit is larger;
/// documents anywhere near this size are rejected before parsing.
const MAX_JSON_BYTES: usize = 2 * 1024 * 1024;

/// Bounded document sizes so an import stays reviewable in the dashboard.
const MAX_GROUPS: usize = 100;
const MAX_CONNECTIONS: usize = 500;
const MAX_COMBOS: usize = 200;
const MAX_STEPS_PER_COMBO: usize = 50;
const MAX_TOTAL_STEPS: usize = 2000;

/// Length limits mirroring the live forms and table constraints.
const NAME_MAX_CHARS: usize = 100;
const ID_MAX_CHARS: usize = 128;

/// The transfer document. Strict deserialization (`deny_unknown_fields`)
/// rejects unknown keys, so a document carrying credentials or future fields
/// fails loudly instead of silently dropping data.
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConfigTransfer {
    pub version: u8,
    pub groups: Vec<TransferGroup>,
    pub connections: Vec<TransferConnection>,
    pub combos: Vec<TransferCombo>,
}

/// A custom provider group. `id` is only a cross-reference inside the
/// document; imports allocate a fresh group id.
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TransferGroup {
    pub id: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub slug: Option<String>,
    pub base_url: String,
    pub protocol: String,
}

/// A named connection. `group_id` references [`TransferGroup::id`] when the
/// connection belongs to a custom provider group; `None` connections keep
/// their preset tile on import, while non-preset ones are attached to a
/// fresh group of their own so they stay visible. No credential field
/// exists.
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TransferConnection {
    pub id: String,
    pub name: String,
    pub base_url: String,
    pub protocol: String,
    pub group_id: Option<String>,
    pub enabled: bool,
}

/// A combo with its fallback steps in execution order. Imports allocate a
/// fresh combo id; only the (unique) name is carried over.
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TransferCombo {
    pub name: String,
    #[serde(default = "default_category")]
    pub category: String,
    pub strategy: String,
    #[serde(default = "default_affinity_mode")]
    pub affinity_mode: String,
    #[serde(default = "default_sticky_minutes")]
    pub sticky_minutes: u32,
    pub steps: Vec<TransferStep>,
}

fn default_category() -> String {
    "text".into()
}

fn default_affinity_mode() -> String {
    "off".into()
}
fn default_sticky_minutes() -> u32 {
    AffinityOptions::default().sticky_minutes
}

/// One ordered step. `provider_id` references [`TransferConnection::id`].
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TransferStep {
    pub provider_id: String,
    pub model: String,
}

/// Counted contents of a validated document. `groups` counts every group
/// the import would create, explicit ones from the document included.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ImportStats {
    pub groups: usize,
    pub connections: usize,
    pub combos: usize,
    pub steps: usize,
    /// Connections the document marks enabled; imports still disable them.
    pub enabled_connections: usize,
    /// Fresh groups synthesized for ungrouped non-preset connections so no
    /// imported entry can end up hidden from the dashboard.
    pub synthesized_groups: usize,
}

/// Advisory result of [`check_import`]: the same validation an import would
/// run, plus the database-derived conflicts that would abort it.
pub struct ImportCheck {
    pub configuration: ConfigTransfer,
    pub stats: ImportStats,
    /// Combo names that already exist here. Any conflict aborts the import.
    pub conflicts: Vec<String>,
    pub warnings: Vec<String>,
}

/// Snapshot the configuration in one read transaction.
///
/// Only structure is selected: the queries never touch credentials, gateway
/// API keys, cached model lists, or timestamps.
pub async fn export(pool: &SqlitePool) -> Result<ConfigTransfer, AppError> {
    // A single transaction keeps the document a consistent snapshot: groups,
    // connections, combos, and steps cannot interleave a concurrent edit.
    let mut transaction = pool.begin().await?;

    let group_rows = sqlx::query(
        "SELECT g.id, g.name, g.base_url, g.protocol, s.slug FROM custom_provider_groups g LEFT JOIN provider_slugs s ON s.group_id = g.id ORDER BY g.rowid",
    )
    .fetch_all(&mut *transaction)
    .await?;
    let connection_rows = sqlx::query(
        "SELECT p.id, p.name, p.base_url, p.protocol, p.enabled, m.group_id
         FROM providers p
         LEFT JOIN provider_group_connections m ON m.provider_id = p.id
         ORDER BY p.rowid",
    )
    .fetch_all(&mut *transaction)
    .await?;
    let combo_rows = sqlx::query(
        "SELECT id, name, category, strategy, affinity_mode, sticky_minutes FROM combos ORDER BY name",
    )
    .fetch_all(&mut *transaction)
    .await?;
    let step_rows = sqlx::query(
        "SELECT combo_id, provider_id, model FROM combo_steps ORDER BY combo_id, position",
    )
    .fetch_all(&mut *transaction)
    .await?;
    transaction.commit().await?;

    let groups = group_rows
        .iter()
        .map(|row| {
            Ok(TransferGroup {
                id: row.try_get("id")?,
                name: row.try_get("name")?,
                slug: row.try_get("slug")?,
                base_url: row.try_get("base_url")?,
                protocol: row.try_get("protocol")?,
            })
        })
        .collect::<Result<Vec<_>, AppError>>()?;

    let connections = connection_rows
        .iter()
        .map(|row| {
            Ok(TransferConnection {
                id: row.try_get("id")?,
                name: row.try_get("name")?,
                base_url: row.try_get("base_url")?,
                protocol: row.try_get("protocol")?,
                group_id: row.try_get::<Option<String>, _>("group_id")?,
                enabled: row.try_get::<i64, _>("enabled")? != 0,
            })
        })
        .collect::<Result<Vec<_>, AppError>>()?;

    // Ordered by (combo_id, position): rows of one combo arrive in step order.
    let mut steps_by_combo: HashMap<String, Vec<TransferStep>> = HashMap::new();
    for row in &step_rows {
        let combo_id: String = row.try_get("combo_id")?;
        steps_by_combo
            .entry(combo_id)
            .or_default()
            .push(TransferStep {
                provider_id: row.try_get("provider_id")?,
                model: row.try_get("model")?,
            });
    }

    let combos = combo_rows
        .iter()
        .map(|row| {
            let id: String = row.try_get("id")?;
            Ok(TransferCombo {
                name: row.try_get("name")?,
                category: row.try_get("category")?,
                strategy: row.try_get("strategy")?,
                affinity_mode: row.try_get("affinity_mode")?,
                sticky_minutes: row.try_get("sticky_minutes")?,
                steps: steps_by_combo.remove(&id).unwrap_or_default(),
            })
        })
        .collect::<Result<Vec<_>, AppError>>()?;

    Ok(ConfigTransfer {
        version: VERSION,
        groups,
        connections,
        combos,
    })
}

/// Validate a document without touching the database, normalizing names,
/// URLs, and models in place. `is_preset` decides whether an ungrouped
/// connection's endpoint is a catalog preset (kept ungrouped); the catalog
/// itself lives in the dashboard layer and is injected by the caller.
/// Returns the counted contents.
fn parse_and_validate(
    json: &str,
    is_preset: &(dyn Fn(&str, &str) -> bool + Sync),
) -> Result<(ConfigTransfer, ImportStats), AppError> {
    if json.len() > MAX_JSON_BYTES {
        return Err(AppError::InvalidRequest(format!(
            "configuration document exceeds {MAX_JSON_BYTES} bytes"
        )));
    }
    let mut transfer: ConfigTransfer = serde_json::from_str(json).map_err(|_| {
        // A fixed category only: serde messages can echo submitted values
        // (for example an invalid type carrying a pasted secret).
        AppError::InvalidRequest(
            "configuration document is not valid: expected the versioned credential-free format"
                .into(),
        )
    })?;
    if transfer.version != VERSION {
        return Err(AppError::InvalidRequest(format!(
            "unsupported configuration version {}; this gateway exports and imports version {VERSION}",
            transfer.version
        )));
    }
    if transfer.groups.len() > MAX_GROUPS {
        return Err(AppError::InvalidRequest(format!(
            "at most {MAX_GROUPS} groups may be imported at once"
        )));
    }
    if transfer.connections.len() > MAX_CONNECTIONS {
        return Err(AppError::InvalidRequest(format!(
            "at most {MAX_CONNECTIONS} connections may be imported at once"
        )));
    }
    if transfer.combos.len() > MAX_COMBOS {
        return Err(AppError::InvalidRequest(format!(
            "at most {MAX_COMBOS} combos may be imported at once"
        )));
    }

    // Groups first: their normalized endpoints validate grouped connections.
    let mut group_ids: HashSet<String> = HashSet::new();
    let mut group_endpoints: HashMap<String, (String, Protocol)> = HashMap::new();
    for group in &mut transfer.groups {
        group.id = validate_id(&group.id, "group id")?.to_owned();
        group.name = validate_text(&group.name, NAME_MAX_CHARS, "group name")?;
        if let Some(slug) = &group.slug {
            crate::db::provider_slugs::validate(slug)?;
        }
        group.base_url = validate_url(&group.base_url)?;
        let protocol = parse_protocol(&group.protocol)?;
        group.protocol = protocol.as_str().to_owned();
        if !group_ids.insert(group.id.clone()) {
            return Err(AppError::InvalidRequest(
                "a group id appears more than once".into(),
            ));
        }
        group_endpoints.insert(group.id.clone(), (group.base_url.clone(), protocol));
    }

    let mut connection_ids: HashSet<String> = HashSet::new();
    let mut enabled_connections = 0;
    let mut synthesized_groups = 0;
    for connection in &mut transfer.connections {
        connection.id = validate_id(&connection.id, "connection id")?.to_owned();
        connection.name = validate_text(&connection.name, NAME_MAX_CHARS, "connection name")?;
        connection.base_url = validate_url(&connection.base_url)?;
        let protocol = parse_protocol(&connection.protocol)?;
        connection.protocol = protocol.as_str().to_owned();
        if let Some(group_id) = connection.group_id.as_deref() {
            let group_id = validate_id(group_id, "connection group id")?.to_owned();
            // Fixed categories only: these fields are raw pasted input and
            // must never be reflected back through an error message.
            let endpoint = group_endpoints.get(&group_id).ok_or_else(|| {
                AppError::InvalidRequest(
                    "a connection references a group id that is not in the document".into(),
                )
            })?;
            // Groups own their endpoint: a grouped connection must match it,
            // mirroring the invariant every dashboard write already follows.
            if endpoint.0 != connection.base_url || endpoint.1 != protocol {
                return Err(AppError::InvalidRequest(
                    "a grouped connection must use its group's base URL and protocol".into(),
                ));
            }
            connection.group_id = Some(group_id);
        }
        if !connection_ids.insert(connection.id.clone()) {
            return Err(AppError::InvalidRequest(
                "a connection id appears more than once".into(),
            ));
        }
        if connection.enabled {
            enabled_connections += 1;
        }
        // Ungrouped non-preset endpoints cannot appear on the dashboard
        // index without a group, so the import will give each its own.
        if connection.group_id.is_none() && !is_preset(&connection.base_url, &connection.protocol) {
            synthesized_groups += 1;
        }
    }

    let mut combo_names: HashSet<String> = HashSet::new();
    let mut steps = 0;
    let mut classified = HashMap::new();
    for combo in &mut transfer.combos {
        combo.name = validate_combo_name(&combo.name)?;
        if !combo_names.insert(combo.name.clone()) {
            return Err(AppError::InvalidRequest(
                "a combo name appears more than once".into(),
            ));
        }
        let category = Category::parse(&combo.category)?;
        Strategy::parse(&combo.strategy)?;
        AffinityOptions {
            mode: AffinityMode::parse(&combo.affinity_mode)?,
            sticky_minutes: combo.sticky_minutes,
        }
        .validate()?;
        if combo.steps.len() > MAX_STEPS_PER_COMBO {
            return Err(AppError::InvalidRequest(format!(
                "a combo has more than {MAX_STEPS_PER_COMBO} steps"
            )));
        }
        steps += combo.steps.len();
        for step in &mut combo.steps {
            step.provider_id = validate_id(&step.provider_id, "step connection id")?.to_owned();
            if !connection_ids.contains(&step.provider_id) {
                return Err(AppError::InvalidRequest(
                    "a combo step references a connection id that is not in the document".into(),
                ));
            }
            step.model = validate_model(&step.model)?;
            let connection = transfer
                .connections
                .iter()
                .find(|connection| connection.id == step.provider_id)
                .expect("validated connection reference");
            let provider = crate::db::providers::Provider {
                id: connection.id.clone(),
                name: connection.name.clone(),
                base_url: connection.base_url.clone(),
                protocol: connection.protocol.parse()?,
                encrypted_api_key: Vec::new(),
                enabled: false,
            };
            if !category.accepts(&provider) || !category.matches_model(&provider, &step.model, &[])
            {
                return Err(AppError::InvalidRequest(
                    "Imported step protocol or known model category is incompatible with its combo category".into(),
                ));
            }
            if classified
                .insert((step.provider_id.clone(), step.model.clone()), category)
                .is_some_and(|existing| existing != category)
            {
                return Err(AppError::InvalidRequest(
                    "Imported model has conflicting categories".into(),
                ));
            }
        }
    }
    if steps > MAX_TOTAL_STEPS {
        return Err(AppError::InvalidRequest(format!(
            "at most {MAX_TOTAL_STEPS} combo steps may be imported at once"
        )));
    }

    let stats = ImportStats {
        groups: transfer.groups.len() + synthesized_groups,
        connections: transfer.connections.len(),
        combos: transfer.combos.len(),
        steps,
        enabled_connections,
        synthesized_groups,
    };
    Ok((transfer, stats))
}

/// Validate a document against the current database without writing.
///
/// This powers the preview page; it is advisory only, because [`apply`]
/// repeats every check transactionally before writing anything.
pub async fn check_import(
    pool: &SqlitePool,
    json: &str,
    is_preset: &(dyn Fn(&str, &str) -> bool + Sync),
) -> Result<ImportCheck, AppError> {
    let (transfer, stats) = parse_and_validate(json, is_preset)?;
    let existing: Vec<String> = sqlx::query_scalar("SELECT name FROM combos")
        .fetch_all(pool)
        .await?;
    let conflicts = conflicting_names(&transfer, &existing.into_iter().collect());

    let mut warnings = vec![
        "Every imported connection starts disabled with no API key: review it, add its key, and enable it before routing uses it."
            .to_owned(),
        "Custom provider slugs may receive a suffix if they are already in use on this gateway."
            .to_owned(),
    ];
    for combo in &transfer.combos {
        if combo.steps.is_empty() {
            warnings.push(format!("combo '{}' has no steps.", combo.name));
        }
    }
    if stats.synthesized_groups > 0 {
        warnings.push(format!(
            "{} ungrouped custom connection(s) will be placed in their own new provider group so they stay visible on the Providers page.",
            stats.synthesized_groups
        ));
    }
    Ok(ImportCheck {
        configuration: transfer,
        stats,
        conflicts,
        warnings,
    })
}

/// Import a document atomically, allocating fresh ids for every row.
///
/// Pure document validation is repeated before opening the write transaction;
/// database conflict checks and every insert run in `BEGIN IMMEDIATE`, so a
/// preview cannot bypass checks or race a concurrently created combo.
pub async fn apply(
    pool: &SqlitePool,
    json: &str,
    is_preset: &(dyn Fn(&str, &str) -> bool + Sync),
) -> Result<ImportStats, AppError> {
    let (transfer, stats) = parse_and_validate(json, is_preset)?;
    let mut transaction = pool.begin_with("BEGIN IMMEDIATE").await?;

    let existing: Vec<String> = sqlx::query_scalar("SELECT name FROM combos")
        .fetch_all(&mut *transaction)
        .await?;
    let conflicts = conflicting_names(&transfer, &existing.into_iter().collect());
    if !conflicts.is_empty() {
        return Err(AppError::InvalidRequest(format!(
            "a combo named '{}' already exists; conflicts abort the whole import",
            conflicts.join("', '")
        )));
    }

    let mut group_ids: HashMap<String, String> = HashMap::new();
    for group in &transfer.groups {
        let id = format!("custom-{}", uuid::Uuid::new_v4());
        sqlx::query(
            "INSERT INTO custom_provider_groups (id, name, base_url, protocol) VALUES (?, ?, ?, ?)",
        )
        .bind(&id)
        .bind(&group.name)
        .bind(&group.base_url)
        .bind(&group.protocol)
        .execute(&mut *transaction)
        .await?;
        crate::db::provider_slugs::assign(
            &mut transaction,
            &id,
            &group.name,
            group.slug.as_deref(),
            true,
        )
        .await?;
        group_ids.insert(group.id.clone(), id);
    }

    let mut connection_ids: HashMap<String, String> = HashMap::new();
    for connection in &transfer.connections {
        let id = uuid::Uuid::new_v4().to_string();
        // No credential is imported: the empty blob is the deliberate
        // unauthenticated sentinel, and enabled is forced off so nothing
        // routes before an operator has reviewed the connection.
        sqlx::query(
            "INSERT INTO providers (id, name, base_url, protocol, encrypted_api_key, enabled)
             VALUES (?, ?, ?, ?, ?, 0)",
        )
        .bind(&id)
        .bind(&connection.name)
        .bind(&connection.base_url)
        .bind(&connection.protocol)
        .bind(Vec::<u8>::new())
        .execute(&mut *transaction)
        .await?;
        if let Some(group_id) = &connection.group_id {
            sqlx::query(
                "INSERT INTO provider_group_connections (provider_id, group_id) VALUES (?, ?)",
            )
            .bind(&id)
            .bind(&group_ids[group_id])
            .execute(&mut *transaction)
            .await?;
        } else if !is_preset(&connection.base_url, &connection.protocol) {
            // An ungrouped non-preset endpoint has no tile on the dashboard
            // index, so the connection would be hidden and unmanageable.
            // Attach it to its own fresh group, named after the connection;
            // document groups are never merged, with each other or here.
            let group_id = format!("custom-{}", uuid::Uuid::new_v4());
            sqlx::query(
                "INSERT INTO custom_provider_groups (id, name, base_url, protocol) VALUES (?, ?, ?, ?)",
            )
            .bind(&group_id)
            .bind(&connection.name)
            .bind(&connection.base_url)
            .bind(&connection.protocol)
            .execute(&mut *transaction)
            .await?;
            crate::db::provider_slugs::assign(
                &mut transaction,
                &group_id,
                &connection.name,
                None,
                true,
            )
            .await?;
            sqlx::query(
                "INSERT INTO provider_group_connections (provider_id, group_id) VALUES (?, ?)",
            )
            .bind(&id)
            .bind(&group_id)
            .execute(&mut *transaction)
            .await?;
        }
        connection_ids.insert(connection.id.clone(), id);
    }

    for combo in &transfer.combos {
        let id = uuid::Uuid::new_v4().to_string();
        sqlx::query("INSERT INTO combos (id, name, category, strategy, affinity_mode, sticky_minutes) VALUES (?, ?, ?, ?, ?, ?)")
            .bind(&id)
            .bind(&combo.name)
            .bind(&combo.category)
            .bind(&combo.strategy)
            .bind(&combo.affinity_mode)
            .bind(combo.sticky_minutes)
            .execute(&mut *transaction)
            .await?;
        for (position, step) in combo.steps.iter().enumerate() {
            super::model_categories::validate(
                &mut transaction,
                &connection_ids[&step.provider_id],
                &step.model,
                Category::parse(&combo.category)?,
                true,
            )
            .await?;
            sqlx::query(
                "INSERT INTO combo_steps (id, combo_id, position, provider_id, model)
                 VALUES (?, ?, ?, ?, ?)",
            )
            .bind(uuid::Uuid::new_v4().to_string())
            .bind(&id)
            .bind(position as i64)
            .bind(&connection_ids[&step.provider_id])
            .bind(&step.model)
            .execute(&mut *transaction)
            .await?;
        }
    }

    transaction.commit().await?;
    Ok(stats)
}

/// Combo names from the document that already exist in `existing`.
fn conflicting_names(transfer: &ConfigTransfer, existing: &HashSet<String>) -> Vec<String> {
    transfer
        .combos
        .iter()
        .filter(|combo| existing.contains(&combo.name))
        .map(|combo| combo.name.clone())
        .collect()
}

/// Nonempty, length-bounded, control-character-free text field.
fn validate_text(value: &str, max_chars: usize, what: &str) -> Result<String, AppError> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Err(AppError::InvalidRequest(format!(
            "{what} must not be empty"
        )));
    }
    if trimmed.chars().count() > max_chars {
        return Err(AppError::InvalidRequest(format!(
            "{what} must be at most {max_chars} characters"
        )));
    }
    if trimmed.chars().any(char::is_control) {
        return Err(AppError::InvalidRequest(format!(
            "{what} must not contain control characters"
        )));
    }
    Ok(trimmed.to_owned())
}

/// A document-local cross-reference id.
fn validate_id<'a>(value: &'a str, what: &str) -> Result<&'a str, AppError> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Err(AppError::InvalidRequest(format!(
            "{what} must not be empty"
        )));
    }
    if trimmed.chars().count() > ID_MAX_CHARS {
        return Err(AppError::InvalidRequest(format!(
            "{what} must be at most {ID_MAX_CHARS} characters"
        )));
    }
    if trimmed.chars().any(char::is_control) {
        return Err(AppError::InvalidRequest(format!(
            "{what} must not contain control characters"
        )));
    }
    Ok(trimmed)
}

/// A base URL normalized by the shared provider rules.
fn validate_url(value: &str) -> Result<String, AppError> {
    let trimmed = value.trim();
    if trimmed.chars().any(char::is_control) {
        return Err(AppError::InvalidRequest(
            "base URL must not contain control characters".into(),
        ));
    }
    normalize_base_url(trimmed)
}

/// A protocol parsed strictly through the adapter boundary. The error is a
/// fixed category: the submitted value is raw input and is not reflected.
fn parse_protocol(value: &str) -> Result<Protocol, AppError> {
    value
        .parse::<Protocol>()
        .map_err(|_| AppError::InvalidRequest("unknown protocol".into()))
}

/// Combo names mirror the stored constraint: 1–64 characters of ASCII
/// letters, digits, `-`, `_`, and `.`.
fn validate_combo_name(value: &str) -> Result<String, AppError> {
    let name = value.trim();
    super::combos::validate_name(name).map_err(|_| {
        AppError::InvalidRequest(format!(
            "combo name must be 1-{} characters of letters, digits, '_', '.', '-'",
            super::combos::MAX_NAME_LENGTH,
        ))
    })?;
    Ok(name.to_owned())
}

/// Step models mirror the stored rule: trimmed, nonempty, bounded, and free
/// of control characters.
fn validate_model(value: &str) -> Result<String, AppError> {
    let model = super::combos::validate_model(value)?;
    // Import deliberately rejects controls; existing stored-step validation
    // permits arbitrary upstream strings. Keep that compatibility distinction.
    if model.chars().any(char::is_control) {
        return Err(AppError::InvalidRequest(
            "step model must not contain control characters".into(),
        ));
    }
    Ok(model)
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::{Engine, engine::general_purpose::STANDARD};
    use secrecy::SecretString;
    use sqlx::SqlitePool;

    use crate::db::{api_keys, combos, provider_groups, providers};
    use crate::providers::Protocol;

    const ENDPOINT: &str = "https://work.example/v1";

    async fn pool() -> SqlitePool {
        crate::db::connect("sqlite::memory:")
            .await
            .expect("connect")
    }

    fn crypto() -> crate::crypto::Crypto {
        crate::crypto::Crypto::from_base64(&SecretString::from(STANDARD.encode([7u8; 32])))
            .expect("crypto")
    }

    /// A document with two grouped connections, one empty group, one
    /// ungrouped preset connection, one ungrouped custom endpoint, and one
    /// round-robin combo with ordered steps.
    fn sample_document() -> String {
        serde_json::json!({
            "version": 1,
            "groups": [
                {"id": "g-work", "name": "Work <LLM>", "base_url": "https://work.example/v1/", "protocol": "openai"},
                {"id": "g-empty", "name": "Empty", "base_url": "https://empty.example/v1", "protocol": "anthropic"}
            ],
            "connections": [
                {"id": "c1", "name": "first", "base_url": "https://work.example/v1", "protocol": "openai", "group_id": "g-work", "enabled": true},
                {"id": "c2", "name": "second", "base_url": "https://work.example/v1", "protocol": "openai", "group_id": "g-work", "enabled": false},
                {"id": "c3", "name": "preset", "base_url": "https://api.openai.com/v1", "protocol": "openai", "group_id": null, "enabled": true},
                {"id": "c4", "name": "solo endpoint", "base_url": "https://solo.example/v1", "protocol": "openai", "group_id": null, "enabled": true}
            ],
            "combos": [
                {"name": "chain", "strategy": "round_robin", "steps": [
                    {"provider_id": "c1", "model": "model-one"},
                    {"provider_id": "c3", "model": "model-two"}
                ]}
            ]
        })
        .to_string()
    }

    /// Preset endpoints used by the fixtures. The real catalog lives in the
    /// dashboard layer and is injected from there in production.
    fn is_preset(base_url: &str, protocol: &str) -> bool {
        matches!(
            (base_url, protocol),
            ("https://api.openai.com/v1", "openai") | ("https://api.anthropic.com/v1", "anthropic")
        )
    }

    async fn seed_source(pool: &SqlitePool) {
        let crypto = crypto();
        let work = provider_groups::create(pool, "Work <LLM>", ENDPOINT, &Protocol::OpenAi)
            .await
            .expect("work group");
        provider_groups::create(
            pool,
            "Empty",
            "https://empty.example/v1",
            &Protocol::Anthropic,
        )
        .await
        .expect("empty group");
        for (id, name, enabled) in [("src-1", "first", true), ("src-2", "second", false)] {
            let secret = SecretString::from(format!("sk-{id}-live-secret"));
            provider_groups::create_connection(
                pool,
                &work.id,
                &providers::Provider {
                    id: id.into(),
                    name: name.into(),
                    base_url: work.base_url.clone(),
                    protocol: work.protocol,
                    encrypted_api_key: crypto.encrypt_api_key(id, &secret).unwrap(),
                    enabled,
                },
            )
            .await
            .expect("connection");
        }
        let preset_id = "src-3";
        providers::save(
            pool,
            &providers::Provider {
                id: preset_id.into(),
                name: "preset".into(),
                base_url: "https://api.openai.com/v1".into(),
                protocol: Protocol::OpenAi,
                encrypted_api_key: crypto
                    .encrypt_api_key(preset_id, &SecretString::from("sk-preset-live-secret"))
                    .unwrap(),
                enabled: true,
            },
        )
        .await
        .expect("preset connection");
        // A direct-database ungrouped custom connection (the "old entries"
        // case): no membership, non-preset endpoint, no credential.
        providers::save(
            pool,
            &providers::Provider {
                id: "src-4".into(),
                name: "solo endpoint".into(),
                base_url: "https://solo.example/v1".into(),
                protocol: Protocol::OpenAi,
                encrypted_api_key: Vec::new(),
                enabled: true,
            },
        )
        .await
        .expect("ungrouped custom connection");
        let combo = combos::create_with_strategy(pool, "chain", combos::Strategy::RoundRobin)
            .await
            .expect("combo");
        combos::add_step(pool, &combo.id, "src-1", "model-one")
            .await
            .expect("step one");
        combos::add_step(pool, &combo.id, "src-3", "model-two")
            .await
            .expect("step two");
        api_keys::create(pool, &crypto, "laptop")
            .await
            .expect("gateway api key");
    }

    #[tokio::test]
    async fn export_contains_structure_but_never_secrets_or_timestamps() {
        let pool = pool().await;
        seed_source(&pool).await;

        let transfer = export(&pool).await.expect("export");
        assert_eq!(transfer.version, VERSION);
        assert_eq!(transfer.groups.len(), 2);
        assert_eq!(transfer.connections.len(), 4);
        assert_eq!(transfer.combos.len(), 1);

        let json = serde_json::to_string(&transfer).unwrap();
        for secret in [
            "sk-src-1-live-secret",
            "sk-src-2-live-secret",
            "sk-preset-live-secret",
        ] {
            assert!(!json.contains(secret), "export must not contain {secret}");
        }
        for forbidden in [
            "encrypted",
            "api_key",
            "created_at",
            "updated_at",
            "refreshed_at",
        ] {
            assert!(
                !json.contains(forbidden),
                "export must not mention '{forbidden}'"
            );
        }
        // The gateway key (name and prefix) and cached model lists stay out.
        assert!(!json.contains("laptop"));
        assert!(!json.contains("models"));

        // Connections carry their membership; preset rows stay ungrouped.
        let first = transfer
            .connections
            .iter()
            .find(|connection| connection.id == "src-1")
            .expect("first connection");
        assert!(
            first
                .group_id
                .as_deref()
                .is_some_and(|id| id.starts_with("custom-"))
        );
        assert_eq!(first.base_url, ENDPOINT);
        let preset = transfer
            .connections
            .iter()
            .find(|connection| connection.id == "src-3")
            .expect("preset connection");
        assert_eq!(preset.group_id, None);
        let solo = transfer
            .connections
            .iter()
            .find(|connection| connection.id == "src-4")
            .expect("ungrouped custom connection");
        assert_eq!(solo.group_id, None);
        assert_eq!(solo.base_url, "https://solo.example/v1");

        // Group ids are the stored custom- ids; combos keep name+strategy.
        assert!(
            transfer
                .groups
                .iter()
                .all(|group| group.id.starts_with("custom-"))
        );
        assert_eq!(transfer.combos[0].strategy, "round_robin");
        let models: Vec<_> = transfer.combos[0]
            .steps
            .iter()
            .map(|step| step.model.as_str())
            .collect();
        assert_eq!(models, ["model-one", "model-two"]);
    }

    #[tokio::test]
    async fn apply_imports_disabled_with_fresh_ids_and_preserved_structure() {
        let source = pool().await;
        seed_source(&source).await;
        let document = serde_json::to_string(&export(&source).await.unwrap()).unwrap();

        let target = pool().await;
        let stats = apply(&target, &document, &is_preset).await.expect("import");
        assert_eq!(
            stats,
            ImportStats {
                groups: 3,
                connections: 4,
                combos: 1,
                steps: 2,
                enabled_connections: 3,
                synthesized_groups: 1,
            }
        );

        // Every imported connection is disabled with an empty credential
        // blob, regardless of the exported enabled flags.
        let imported = providers::list(&target).await.expect("connections");
        assert_eq!(imported.len(), 4);
        for connection in &imported {
            assert!(!connection.enabled, "imports start disabled");
            assert!(
                connection.encrypted_api_key.is_empty(),
                "imports carry no credential"
            );
            assert!(!connection.id.starts_with("src-"), "ids are fresh");
        }

        // Distinct groups survive, including the empty one; memberships map
        // to the new group ids, preset rows stay ungrouped, and the ungrouped
        // custom endpoint gets a fresh group of its own so it stays visible.
        let groups = provider_groups::list(&target).await.expect("groups");
        assert_eq!(groups.len(), 3);
        let empty = groups
            .iter()
            .find(|summary| summary.group.name == "Empty")
            .expect("empty group preserved");
        assert_eq!(empty.connection_count, 0);
        assert!(
            empty.group.id
                != groups
                    .iter()
                    .find(|summary| summary.group.name == "Work <LLM>")
                    .unwrap()
                    .group
                    .id
        );
        let work = groups
            .iter()
            .find(|summary| summary.group.name == "Work <LLM>")
            .expect("work group");
        assert_eq!(work.connection_count, 2);
        let solo = groups
            .iter()
            .find(|summary| summary.group.name == "solo endpoint")
            .expect("synthesized group for the ungrouped custom endpoint");
        assert_eq!(solo.connection_count, 1);
        assert_eq!(solo.group.base_url, "https://solo.example/v1");
        assert_eq!(solo.group.protocol, Protocol::OpenAi);
        for connection in &imported {
            let group = provider_groups::by_connection(&target, &connection.id)
                .await
                .unwrap();
            match connection.name.as_str() {
                "preset" => assert!(group.is_none(), "preset rows stay ungrouped"),
                "solo endpoint" => assert_eq!(
                    group.expect("grouped").id,
                    solo.group.id,
                    "ungrouped custom endpoints join their synthesized group"
                ),
                _ => assert_eq!(
                    group.expect("grouped").id,
                    work.group.id,
                    "grouped connections rejoin their imported group"
                ),
            }
        }
        assert_eq!(work.group.base_url, ENDPOINT);

        // The combo keeps its name, strategy, and ordered steps, now
        // referencing the fresh connection ids.
        let combo = combos::by_name(&target, "chain").await.expect("combo");
        assert_eq!(combo.strategy, combos::Strategy::RoundRobin);
        let steps = combos::steps(&target, &combo.id).await.expect("steps");
        assert_eq!(
            steps
                .iter()
                .map(|step| step.model.as_str())
                .collect::<Vec<_>>(),
            ["model-one", "model-two"]
        );
        let imported_ids: HashSet<_> = imported.iter().map(|row| row.id.clone()).collect();
        assert!(
            steps
                .iter()
                .all(|step| imported_ids.contains(&step.provider_id))
        );

        // Re-importing the same document conflicts on the combo name and
        // writes nothing at all.
        let before = providers::list(&target).await.unwrap().len();
        assert!(matches!(
            apply(&target, &document, &is_preset).await,
            Err(AppError::InvalidRequest(_))
        ));
        assert_eq!(providers::list(&target).await.unwrap().len(), before);
        assert_eq!(
            provider_groups::list(&target).await.unwrap().len(),
            3,
            "conflicts abort the whole import"
        );
    }

    #[tokio::test]
    async fn strict_documents_reject_unknown_and_secret_fields() {
        let pool = pool().await;
        let secret = "sk-should-never-be-accepted";
        let documents = [
            // Unknown top-level key.
            serde_json::json!({"version": 1, "groups": [], "connections": [], "combos": [], "api_keys": []}).to_string(),
            // Credential-looking fields at every level.
            serde_json::json!({"version": 1, "groups": [], "connections": [{"id": "c", "name": "n", "base_url": "https://a.example/v1", "protocol": "openai", "group_id": null, "enabled": false, "api_key": secret}], "combos": []}).to_string(),
            serde_json::json!({"version": 1, "groups": [{"id": "g", "name": "n", "base_url": "https://a.example/v1", "protocol": "openai", "encrypted_api_key": "AAECAw=="}], "connections": [], "combos": []}).to_string(),
            serde_json::json!({"version": 1, "groups": [], "connections": [], "combos": [{"name": "c", "strategy": "priority", "steps": [{"provider_id": "c", "model": "m", "token": secret}]}]}).to_string(),
            // Wrong and missing versions.
            serde_json::json!({"version": 2, "groups": [], "connections": [], "combos": []}).to_string(),
            serde_json::json!({"groups": [], "connections": [], "combos": []}).to_string(),
            // Not an object.
            "[]".to_string(),
            "not json".to_string(),
        ];
        for document in documents {
            assert!(
                matches!(
                    check_import(&pool, &document, &is_preset).await,
                    Err(AppError::InvalidRequest(_))
                ),
                "document must be rejected: {document}"
            );
            assert!(
                matches!(
                    apply(&pool, &document, &is_preset).await,
                    Err(AppError::InvalidRequest(_))
                ),
                "document must be rejected by apply: {document}"
            );
        }
        assert!(providers::list(&pool).await.unwrap().is_empty());
        assert!(provider_groups::list(&pool).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn invalid_references_values_and_bounds_are_rejected() {
        let base = serde_json::json!({
            "version": 1,
            "groups": [{"id": "g", "name": "Group", "base_url": "https://g.example/v1", "protocol": "openai"}],
            "connections": [{"id": "c", "name": "Conn", "base_url": "https://g.example/v1", "protocol": "openai", "group_id": "g", "enabled": false}],
            "combos": [{"name": "chain", "strategy": "priority", "steps": [{"provider_id": "c", "model": "m"}]}]
        });

        let bad = |mutate: serde_json::Value| mutate.to_string();
        let documents = [
            // Unknown group and step references.
            bad(
                serde_json::json!({"version": 1, "groups": [], "connections": [{"id": "c", "name": "n", "base_url": "https://a.example/v1", "protocol": "openai", "group_id": "ghost", "enabled": false}], "combos": []}),
            ),
            bad({
                let mut document = base.clone();
                document["combos"][0]["steps"][0]["provider_id"] = "ghost".into();
                document
            }),
            // A grouped connection must share its group's endpoint.
            bad({
                let mut document = base.clone();
                document["connections"][0]["base_url"] = "https://other.example/v1".into();
                document
            }),
            bad({
                let mut document = base.clone();
                document["connections"][0]["protocol"] = "anthropic".into();
                document
            }),
            // Duplicate ids and combo names.
            bad({
                let mut document = base.clone();
                let duplicate = document["groups"][0].clone();
                document["groups"].as_array_mut().unwrap().push(duplicate);
                document
            }),
            bad({
                let mut document = base.clone();
                let duplicate = document["connections"][0].clone();
                document["connections"]
                    .as_array_mut()
                    .unwrap()
                    .push(duplicate);
                document
            }),
            bad({
                let mut document = base.clone();
                let duplicate = document["combos"][0].clone();
                document["combos"].as_array_mut().unwrap().push(duplicate);
                document
            }),
            // Invalid URLs, protocols, strategies, names, models.
            bad({
                let mut document = base.clone();
                document["groups"][0]["base_url"] = "https://user:pw@g.example/v1".into();
                document
            }),
            bad({
                let mut document = base.clone();
                document["connections"][0]["protocol"] = "carrier-pigeon".into();
                document
            }),
            bad({
                let mut document = base.clone();
                document["combos"][0]["strategy"] = "weighted".into();
                document
            }),
            bad({
                let mut document = base.clone();
                document["combos"][0]["name"] = "has space".into();
                document
            }),
            bad({
                let mut document = base.clone();
                document["groups"][0]["name"] = "\u{7}control".into();
                document
            }),
            bad({
                let mut document = base.clone();
                document["combos"][0]["steps"][0]["model"] = "\u{7}model".into();
                document
            }),
            bad({
                let mut document = base.clone();
                document["combos"][0]["steps"][0]["model"] = "   ".into();
                document
            }),
            bad({
                let mut document = base.clone();
                document["connections"][0]["name"] = "x".repeat(101).into();
                document
            }),
            bad({
                let mut document = base.clone();
                document["combos"][0]["name"] = "y".repeat(65).into();
                document
            }),
        ];
        for document in documents {
            assert!(
                matches!(
                    parse_and_validate(&document, &is_preset),
                    Err(AppError::InvalidRequest(_))
                ),
                "document must be rejected: {document}"
            );
        }
        assert!(parse_and_validate(&sample_document(), &is_preset).is_ok());
    }

    #[tokio::test]
    async fn bounded_counts_and_document_size_are_enforced() {
        let pool = pool().await;
        let group = serde_json::json!({"id": "g", "name": "G", "base_url": "https://g.example/v1", "protocol": "openai"});
        let connection = serde_json::json!({"id": "c", "name": "C", "base_url": "https://g.example/v1", "protocol": "openai", "group_id": null, "enabled": false});

        let too_many_groups = serde_json::json!({
            "version": 1,
            "groups": vec![group.clone(); MAX_GROUPS + 1],
            "connections": [],
            "combos": []
        })
        .to_string();
        assert!(parse_and_validate(&too_many_groups, &is_preset).is_err());

        let too_many_connections = serde_json::json!({
            "version": 1,
            "groups": [],
            "connections": vec![connection.clone(); MAX_CONNECTIONS + 1],
            "combos": []
        })
        .to_string();
        assert!(parse_and_validate(&too_many_connections, &is_preset).is_err());

        let too_many_steps = serde_json::json!({
            "version": 1,
            "groups": [],
            "connections": [],
            "combos": (0..MAX_COMBOS + 1).map(|index|
                serde_json::json!({"name": format!("combo-{index}"), "strategy": "priority", "steps": []})
            ).collect::<Vec<_>>()
        })
        .to_string();
        assert!(parse_and_validate(&too_many_steps, &is_preset).is_err());

        let oversized = format!(
            "{{\"version\":1,\"groups\":[],\"connections\":[],\"combos\":[],\"pad\":\"{}\"}}",
            "x".repeat(MAX_JSON_BYTES)
        );
        assert!(parse_and_validate(&oversized, &is_preset).is_err());

        // A maximal-but-legal document passes validation.
        let legal = serde_json::json!({
            "version": 1,
            "groups": vec![group],
            "connections": vec![connection],
            "combos": (0..MAX_COMBOS).map(|index|
                serde_json::json!({"name": format!("combo-{index}"), "strategy": "priority", "steps": []})
            ).collect::<Vec<_>>()
        })
        .to_string();
        let stats = apply(&pool, &legal, &is_preset)
            .await
            .expect("maximal import");
        assert_eq!(stats.combos, MAX_COMBOS);
    }

    #[tokio::test]
    async fn check_import_reports_conflicts_and_warnings_without_writing() {
        let pool = pool().await;
        combos::create(&pool, "chain")
            .await
            .expect("existing combo");

        let check = check_import(&pool, &sample_document(), &is_preset)
            .await
            .expect("check");
        assert_eq!(check.stats.connections, 4);
        assert_eq!(check.stats.groups, 3);
        assert_eq!(check.stats.synthesized_groups, 1);
        assert_eq!(check.conflicts, ["chain".to_owned()]);
        assert!(
            check
                .warnings
                .iter()
                .any(|warning| warning.contains("starts disabled"))
        );
        assert!(
            check
                .warnings
                .iter()
                .any(|warning| warning.contains("stay visible on the Providers page"))
        );

        // Empty combos surface a warning of their own.
        let empty_combo = serde_json::json!({
            "version": 1, "groups": [], "connections": [],
            "combos": [{"name": "empty", "strategy": "priority", "steps": []}]
        })
        .to_string();
        let check = check_import(&pool, &empty_combo, &is_preset)
            .await
            .expect("check");
        assert!(check.conflicts.is_empty());
        assert!(
            check
                .warnings
                .iter()
                .any(|warning| warning.contains("'empty' has no steps"))
        );

        // Previewing wrote nothing, and the conflict still aborts an import.
        assert!(providers::list(&pool).await.unwrap().is_empty());
        assert!(matches!(
            apply(&pool, &sample_document(), &is_preset).await,
            Err(AppError::InvalidRequest(_))
        ));
        assert!(providers::list(&pool).await.unwrap().is_empty());

        // The minimal empty document is a valid no-op import.
        let empty =
            serde_json::json!({"version": 1, "groups": [], "connections": [], "combos": []})
                .to_string();
        let stats = apply(&pool, &empty, &is_preset)
            .await
            .expect("empty import");
        assert_eq!(stats.steps, 0);
        assert!(provider_groups::list(&pool).await.unwrap().is_empty());
    }
}
