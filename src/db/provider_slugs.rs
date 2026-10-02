//! Stable public provider namespaces. Credentials and upstream model IDs stay separate.

use std::collections::HashMap;

use sha2::{Digest, Sha256};
use sqlx::{Row, SqliteConnection, SqlitePool, sqlite::SqliteRow};

use crate::{error::AppError, provider_catalog};

pub const MAX_SLUG_CHARS: usize = 64;
pub const MAX_PUBLIC_MODEL_CHARS: usize = MAX_SLUG_CHARS + 1 + 256;

pub fn validate(slug: &str) -> Result<(), AppError> {
    if slug.is_empty()
        || slug.len() > MAX_SLUG_CHARS
        || slug.starts_with('-')
        || slug.ends_with('-')
        || !slug
            .bytes()
            .all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == b'-')
        || reserved(slug)
    {
        return Err(AppError::InvalidRequest(
            "provider slug must be 1–64 lowercase letters, digits, or hyphens; leading/trailing hyphens and reserved provider slugs are not allowed".into(),
        ));
    }
    Ok(())
}

fn reserved(slug: &str) -> bool {
    slug == "combo"
        || slug.starts_with("connection-")
        || provider_catalog::PRESETS
            .iter()
            .any(|preset| preset.slug == slug || preset.routing_slug() == slug)
}

fn suggested(name: &str) -> String {
    let mut slug = String::new();
    for ch in name.bytes() {
        if ch.is_ascii_alphanumeric() {
            slug.push(char::from(ch.to_ascii_lowercase()));
        } else if !slug.is_empty() && !slug.ends_with('-') {
            slug.push('-');
        }
        if slug.len() >= MAX_SLUG_CHARS {
            break;
        }
    }
    let slug = slug.trim_end_matches('-');
    if slug.is_empty() {
        "provider".into()
    } else {
        slug.into()
    }
}

/// The caller holds an immediate write transaction. Imports may suffix a desired
/// slug; an explicitly entered dashboard slug instead fails on any conflict.
pub(crate) async fn assign(
    connection: &mut SqliteConnection,
    group_id: &str,
    name: &str,
    requested: Option<&str>,
    suffix_conflicts: bool,
) -> Result<String, AppError> {
    let requested = requested.map(str::trim).filter(|slug| !slug.is_empty());
    if let Some(slug) = requested {
        validate(slug)?;
    }
    let mut base = requested
        .map(str::to_owned)
        .unwrap_or_else(|| suggested(name));
    if base.starts_with("connection-") {
        base = suggested(&format!("provider-{base}"));
    }
    let mut candidate = base.clone();
    let mut suffix = 2_u64;
    loop {
        let exists: bool =
            sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM provider_slugs WHERE slug = ?)")
                .bind(&candidate)
                .fetch_one(&mut *connection)
                .await?;
        if !reserved(&candidate) && !exists {
            break;
        }
        if requested.is_some() && !suffix_conflicts {
            return Err(AppError::InvalidRequest(
                "provider slug is already in use".into(),
            ));
        }
        let tail = format!("-{suffix}");
        let head = &base[..base.len().min(MAX_SLUG_CHARS - tail.len())];
        candidate = format!("{}{tail}", head.trim_end_matches('-'));
        suffix += 1;
    }
    sqlx::query("INSERT INTO provider_slugs (group_id, slug) VALUES (?, ?)")
        .bind(group_id)
        .bind(&candidate)
        .execute(connection)
        .await?;
    Ok(candidate)
}

/// Assign legacy groups once, preserving their IDs, credentials, and combo steps.
pub(crate) async fn initialize(pool: &SqlitePool) -> Result<(), AppError> {
    let mut transaction = pool.begin_with("BEGIN IMMEDIATE").await?;
    let rows = sqlx::query("SELECT g.id, g.name FROM custom_provider_groups g LEFT JOIN provider_slugs s ON s.group_id = g.id WHERE s.group_id IS NULL ORDER BY g.rowid")
        .fetch_all(&mut *transaction).await?;
    for row in rows {
        assign(
            &mut transaction,
            &row.try_get::<String, _>("id")?,
            &row.try_get::<String, _>("name")?,
            None,
            true,
        )
        .await?;
    }
    transaction.commit().await?;
    Ok(())
}

pub async fn for_group(pool: &SqlitePool, group_id: &str) -> Result<String, AppError> {
    sqlx::query_scalar("SELECT slug FROM provider_slugs WHERE group_id = ?")
        .bind(group_id)
        .fetch_optional(pool)
        .await?
        .ok_or_else(|| AppError::NotFound("provider slug was not found".into()))
}

/// Custom membership takes precedence even when its endpoint matches a preset.
/// Ungrouped, non-preset connections use a reserved, connection-specific namespace.
pub async fn for_connections(pool: &SqlitePool) -> Result<HashMap<String, String>, AppError> {
    let rows = sqlx::query("SELECT p.id, p.base_url, p.protocol, m.group_id, s.slug FROM providers p LEFT JOIN provider_group_connections m ON m.provider_id = p.id LEFT JOIN provider_slugs s ON s.group_id = m.group_id")
        .fetch_all(pool).await?;
    rows.into_iter()
        .map(|row| {
            let id: String = row.try_get("id")?;
            let slug = namespace(&row, &id)?;
            Ok((id, slug))
        })
        .collect()
}

/// Read configuration and its namespace in a single SQLite snapshot. The
/// returned provider is the exact configuration whose namespace was checked.
pub(crate) async fn connection(
    pool: &SqlitePool,
    id: &str,
) -> Result<Option<(super::providers::Provider, String)>, AppError> {
    let row = sqlx::query("SELECT p.id, p.name, p.base_url, p.protocol, p.encrypted_api_key, p.enabled, m.group_id, s.slug FROM providers p LEFT JOIN provider_group_connections m ON m.provider_id = p.id LEFT JOIN provider_slugs s ON s.group_id = m.group_id WHERE p.id = ?")
        .bind(id).fetch_optional(pool).await?;
    let Some(row) = row else {
        return Ok(None);
    };
    Ok(Some((
        super::providers::provider_from_row(&row)?,
        namespace(&row, id)?,
    )))
}

fn namespace(row: &SqliteRow, id: &str) -> Result<String, AppError> {
    if let Some(slug) = row.try_get::<Option<String>, _>("slug")? {
        return Ok(slug);
    }
    if row.try_get::<Option<String>, _>("group_id")?.is_some() {
        return Err(AppError::Configuration("provider slug is missing".into()));
    }
    let protocol = row.try_get::<String, _>("protocol")?.parse()?;
    Ok(
        match provider_catalog::matching(&row.try_get::<String, _>("base_url")?, &protocol) {
            Some(preset) => preset.routing_slug().to_owned(),
            None => connection_slug(id),
        },
    )
}

fn connection_slug(id: &str) -> String {
    if !id.is_empty()
        && id.len() <= 53
        && id
            .bytes()
            .all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == b'-')
        && !id.ends_with('-')
    {
        format!("connection-{id}")
    } else {
        let hash = Sha256::digest(id.as_bytes());
        let hex: String = hash[..26]
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        format!("connection-{hex}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{db::provider_groups, providers::Protocol};

    #[test]
    fn slug_validation_reserves_provider_and_combo_namespaces() {
        for slug in ["provider1", "my-provider", "a--b", &"a".repeat(64)] {
            validate(slug).unwrap();
        }
        for slug in [
            "",
            "OpenAI",
            "provider/model",
            "-start",
            "end-",
            "combo",
            "openrouter",
            "ds",
            "deepseek",
            "connection-first",
            &"a".repeat(65),
        ] {
            assert!(validate(slug).is_err(), "{slug}");
        }
        assert_eq!(connection_slug("first"), "connection-first");
        let unusual = connection_slug("bad/id");
        assert!(unusual.len() <= MAX_SLUG_CHARS);
        assert!(!unusual.contains('/'));
    }

    #[tokio::test]
    async fn readable_slugs_are_unique_and_explicit_conflicts_roll_back() {
        let pool = crate::db::connect("sqlite::memory:").await.unwrap();
        for (name, expected) in [
            ("Provider One", "provider-one"),
            ("Provider One", "provider-one-2"),
            ("OpenRouter", "openrouter-2"),
            ("Combo", "combo-2"),
            ("日本語", "provider"),
        ] {
            let group =
                provider_groups::create(&pool, name, "http://localhost/v1", &Protocol::OpenAi)
                    .await
                    .unwrap();
            assert_eq!(for_group(&pool, &group.id).await.unwrap(), expected);
        }
        let count = provider_groups::list(&pool).await.unwrap().len();
        for slug in ["provider-one", "openai", "ds", "combo", "bad/id"] {
            assert!(
                provider_groups::create_with_slug(
                    &pool,
                    "New",
                    "http://localhost/v1",
                    &Protocol::OpenAi,
                    Some(slug)
                )
                .await
                .is_err()
            );
            assert_eq!(provider_groups::list(&pool).await.unwrap().len(), count);
        }
        let group = provider_groups::create_with_slug(
            &pool,
            "Custom",
            "http://localhost/v1",
            &Protocol::OpenAi,
            Some("provider1"),
        )
        .await
        .unwrap();
        assert_eq!(for_group(&pool, &group.id).await.unwrap(), "provider1");
        provider_groups::delete(&pool, &group.id).await.unwrap();
        assert!(for_group(&pool, &group.id).await.is_err());
    }

    #[tokio::test]
    async fn startup_assigns_legacy_groups_once_without_changing_configuration() {
        let file = tempfile::NamedTempFile::new().unwrap();
        let url = format!("sqlite://{}", file.path().display());
        let pool = crate::db::connect(&url).await.unwrap();
        sqlx::query("INSERT INTO custom_provider_groups (id, name, base_url, protocol) VALUES ('legacy', 'My Provider', 'http://localhost/v1', 'openai')").execute(&pool).await.unwrap();
        sqlx::query("INSERT INTO providers (id, name, base_url, protocol, encrypted_api_key) VALUES ('p1', 'Key', 'http://localhost/v1', 'openai', X'070809')").execute(&pool).await.unwrap();
        sqlx::query("INSERT INTO provider_group_connections (provider_id, group_id) VALUES ('p1', 'legacy')").execute(&pool).await.unwrap();
        let combo = crate::db::combos::create(&pool, "chain").await.unwrap();
        crate::db::combos::add_step(&pool, &combo.id, "p1", "vendor/model")
            .await
            .unwrap();
        pool.close().await;
        let pool = crate::db::connect(&url).await.unwrap();
        assert_eq!(for_group(&pool, "legacy").await.unwrap(), "my-provider");
        assert_eq!(for_connections(&pool).await.unwrap()["p1"], "my-provider");
        sqlx::query("UPDATE custom_provider_groups SET name = 'Renamed' WHERE id = 'legacy'")
            .execute(&pool)
            .await
            .unwrap();
        initialize(&pool).await.unwrap();
        assert_eq!(for_group(&pool, "legacy").await.unwrap(), "my-provider");
        assert_eq!(
            crate::db::providers::get(&pool, "p1")
                .await
                .unwrap()
                .encrypted_api_key,
            vec![7, 8, 9]
        );
        assert_eq!(
            crate::db::combos::steps(&pool, &combo.id).await.unwrap()[0].model,
            "vendor/model"
        );
        pool.close().await;
    }

    #[tokio::test]
    async fn namespace_and_attempt_configuration_share_one_snapshot_during_endpoint_edits() {
        let file = tempfile::NamedTempFile::new().unwrap();
        let pool = crate::db::connect(&format!("sqlite://{}", file.path().display()))
            .await
            .unwrap();
        let mut provider = crate::db::providers::Provider {
            id: "changing".into(),
            name: "Key".into(),
            base_url: "https://api.openai.com/v1".into(),
            protocol: Protocol::OpenAi,
            encrypted_api_key: Vec::new(),
            enabled: true,
        };
        crate::db::providers::save(&pool, &provider).await.unwrap();
        let (read_started, start_reading) = tokio::sync::oneshot::channel();
        let (edit_started, start_editing) = tokio::sync::oneshot::channel();
        let writer = async {
            start_reading.await.unwrap();
            provider.base_url = "https://api.groq.com/openai/v1".into();
            crate::db::providers::save(&pool, &provider).await.unwrap();
            edit_started.send(()).unwrap();
            for index in 0..80 {
                provider.base_url = if index % 2 == 0 {
                    "https://api.openai.com/v1"
                } else {
                    "https://api.groq.com/openai/v1"
                }
                .into();
                crate::db::providers::save(&pool, &provider).await.unwrap();
            }
        };
        let reader = async {
            let before = connection(&pool, "changing").await.unwrap().unwrap();
            assert_eq!(before.1, "openai");
            read_started.send(()).unwrap();
            start_editing.await.unwrap();
            // Keep the initial attempt snapshot stable despite later edits.
            assert_eq!(before.0.base_url, "https://api.openai.com/v1");
            for _ in 0..80 {
                let (config, slug) = connection(&pool, "changing").await.unwrap().unwrap();
                let expected = provider_catalog::matching(&config.base_url, &config.protocol)
                    .unwrap()
                    .routing_slug();
                assert_eq!(slug, expected);
            }
        };
        tokio::join!(writer, reader);
        pool.close().await;
    }

    #[tokio::test]
    async fn concurrent_creation_cannot_claim_the_same_slug() {
        let file = tempfile::NamedTempFile::new().unwrap();
        let pool = crate::db::connect(&format!("sqlite://{}", file.path().display()))
            .await
            .unwrap();
        let (first, second) = tokio::join!(
            provider_groups::create(&pool, "Shared", "http://localhost/v1", &Protocol::OpenAi),
            provider_groups::create(&pool, "Shared", "http://localhost/v1", &Protocol::OpenAi),
        );
        let first = for_group(&pool, &first.unwrap().id).await.unwrap();
        let second = for_group(&pool, &second.unwrap().id).await.unwrap();
        assert_ne!(first, second);
        assert!(["shared", "shared-2"].contains(&first.as_str()));
        assert!(["shared", "shared-2"].contains(&second.as_str()));
        pool.close().await;
    }
}
