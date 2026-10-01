//! Persisted custom providers and explicit membership of named connections.

use std::collections::HashMap;

use sqlx::{Row, SqliteConnection, SqlitePool, sqlite::SqliteRow};

use crate::{error::AppError, providers::Protocol};

use super::providers::{Provider, provider_from_row};

pub struct CustomGroup {
    pub id: String,
    pub name: String,
    pub base_url: String,
    pub protocol: Protocol,
}

pub struct GroupSummary {
    pub group: CustomGroup,
    pub connection_count: usize,
    pub enabled_count: usize,
}

fn from_row(row: &SqliteRow) -> Result<CustomGroup, AppError> {
    let protocol: String = row.try_get("protocol")?;
    Ok(CustomGroup {
        id: row.try_get("id")?,
        name: row.try_get("name")?,
        base_url: row.try_get("base_url")?,
        protocol: protocol.parse()?,
    })
}

pub async fn list(pool: &SqlitePool) -> Result<Vec<GroupSummary>, AppError> {
    let rows = sqlx::query(
        "SELECT g.id, g.name, g.base_url, g.protocol,
                COUNT(p.id) AS connection_count,
                COALESCE(SUM(p.enabled), 0) AS enabled_count
         FROM custom_provider_groups g
         LEFT JOIN provider_group_connections m ON m.group_id = g.id
         LEFT JOIN providers p ON p.id = m.provider_id
         GROUP BY g.id ORDER BY g.name, g.id",
    )
    .fetch_all(pool)
    .await?;
    rows.iter()
        .map(|row| {
            Ok(GroupSummary {
                group: from_row(row)?,
                connection_count: row.try_get::<i64, _>("connection_count")? as usize,
                enabled_count: row.try_get::<i64, _>("enabled_count")? as usize,
            })
        })
        .collect()
}

pub async fn get(pool: &SqlitePool, id: &str) -> Result<CustomGroup, AppError> {
    let row =
        sqlx::query("SELECT id, name, base_url, protocol FROM custom_provider_groups WHERE id = ?")
            .bind(id)
            .fetch_optional(pool)
            .await?;
    row.as_ref()
        .map(from_row)
        .transpose()?
        .ok_or_else(|| AppError::NotFound("custom provider was not found".into()))
}

pub async fn by_connection(pool: &SqlitePool, id: &str) -> Result<Option<CustomGroup>, AppError> {
    let row = sqlx::query(
        "SELECT g.id, g.name, g.base_url, g.protocol FROM custom_provider_groups g
         JOIN provider_group_connections m ON m.group_id = g.id WHERE m.provider_id = ?",
    )
    .bind(id)
    .fetch_optional(pool)
    .await?;
    row.as_ref().map(from_row).transpose()
}

pub async fn members(pool: &SqlitePool, id: &str) -> Result<Vec<Provider>, AppError> {
    let rows = sqlx::query(
        "SELECT p.id, p.name, p.base_url, p.protocol, p.encrypted_api_key, p.enabled
         FROM providers p JOIN provider_group_connections m ON m.provider_id = p.id
         WHERE m.group_id = ? ORDER BY p.name, p.id",
    )
    .bind(id)
    .fetch_all(pool)
    .await?;
    rows.iter().map(provider_from_row).collect()
}

/// Custom-provider membership for a connection, when it has one.
pub struct ConnectionGroup {
    pub id: String,
    pub name: String,
}

/// Custom provider id and name for each grouped connection.
pub async fn connection_groups(
    pool: &SqlitePool,
) -> Result<HashMap<String, ConnectionGroup>, AppError> {
    let rows = sqlx::query(
        "SELECT m.provider_id, g.id, g.name FROM provider_group_connections m
         JOIN custom_provider_groups g ON g.id = m.group_id",
    )
    .fetch_all(pool)
    .await?;
    rows.iter()
        .map(|row| {
            Ok((
                row.try_get("provider_id")?,
                ConnectionGroup {
                    id: row.try_get("id")?,
                    name: row.try_get("name")?,
                },
            ))
        })
        .collect()
}

pub async fn connection_group_names(
    pool: &SqlitePool,
) -> Result<HashMap<String, String>, AppError> {
    let rows = sqlx::query(
        "SELECT m.provider_id, g.name FROM provider_group_connections m
         JOIN custom_provider_groups g ON g.id = m.group_id",
    )
    .fetch_all(pool)
    .await?;
    rows.iter()
        .map(|row| Ok((row.try_get("provider_id")?, row.try_get("name")?)))
        .collect()
}

pub async fn create(
    pool: &SqlitePool,
    name: &str,
    base_url: &str,
    protocol: &Protocol,
) -> Result<CustomGroup, AppError> {
    let name = name.trim();
    if name.is_empty() || name.chars().count() > 100 {
        return Err(AppError::InvalidRequest(
            "provider name must be 1–100 characters".into(),
        ));
    }
    let group = CustomGroup {
        id: format!("custom-{}", uuid::Uuid::new_v4()),
        name: name.to_string(),
        base_url: crate::providers::normalize_base_url(base_url.trim())?,
        protocol: *protocol,
    };
    let mut connection = pool.acquire().await?;
    insert_group(&mut connection, &group).await?;
    Ok(group)
}

async fn insert_group(
    connection: &mut SqliteConnection,
    group: &CustomGroup,
) -> Result<(), AppError> {
    sqlx::query(
        "INSERT INTO custom_provider_groups (id, name, base_url, protocol) VALUES (?, ?, ?, ?)",
    )
    .bind(&group.id)
    .bind(&group.name)
    .bind(&group.base_url)
    .bind(group.protocol.as_str())
    .execute(connection)
    .await
    .map_err(storage_error)?;
    Ok(())
}

fn storage_error(error: sqlx::Error) -> AppError {
    if super::is_constraint_violation(&error) {
        AppError::InvalidRequest(
            "custom provider or connection violates a storage constraint".into(),
        )
    } else {
        error.into()
    }
}

async fn insert_connection(
    connection: &mut SqliteConnection,
    group: &CustomGroup,
    provider: &Provider,
    update_unassigned: bool,
) -> Result<(), AppError> {
    if provider.base_url != group.base_url || provider.protocol != group.protocol {
        return Err(AppError::InvalidRequest(
            "connection endpoint must match its provider".into(),
        ));
    }
    // Group-add forms create new rows only. The legacy edit path may update an
    // existing unassigned row, but must never move a connection between groups.
    if update_unassigned {
        let assigned: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM provider_group_connections WHERE provider_id = ?)",
        )
        .bind(&provider.id)
        .fetch_one(&mut *connection)
        .await?;
        if assigned {
            return Err(AppError::InvalidRequest(
                "connection already belongs to a provider".into(),
            ));
        }
    }
    let sql = if update_unassigned {
        "INSERT INTO providers (id, name, base_url, protocol, encrypted_api_key, enabled)
         VALUES (?, ?, ?, ?, ?, ?)
         ON CONFLICT(id) DO UPDATE SET name = excluded.name,
           base_url = excluded.base_url, protocol = excluded.protocol,
           encrypted_api_key = excluded.encrypted_api_key, enabled = excluded.enabled,
           updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')"
    } else {
        "INSERT INTO providers (id, name, base_url, protocol, encrypted_api_key, enabled)
         VALUES (?, ?, ?, ?, ?, ?)"
    };
    sqlx::query(sql)
        .bind(&provider.id)
        .bind(&provider.name)
        .bind(&provider.base_url)
        .bind(provider.protocol.as_str())
        .bind(&provider.encrypted_api_key)
        .bind(provider.enabled)
        .execute(&mut *connection)
        .await
        .map_err(storage_error)?;
    sqlx::query("INSERT INTO provider_group_connections (provider_id, group_id) VALUES (?, ?)")
        .bind(&provider.id)
        .bind(&group.id)
        .execute(connection)
        .await
        .map_err(storage_error)?;
    Ok(())
}

pub async fn create_connection(
    pool: &SqlitePool,
    id: &str,
    provider: &Provider,
) -> Result<(), AppError> {
    let mut transaction = pool.begin_with("BEGIN IMMEDIATE").await?;
    let row =
        sqlx::query("SELECT id, name, base_url, protocol FROM custom_provider_groups WHERE id = ?")
            .bind(id)
            .fetch_optional(&mut *transaction)
            .await?
            .ok_or_else(|| AppError::NotFound("custom provider was not found".into()))?;
    insert_connection(&mut transaction, &from_row(&row)?, provider, false).await?;
    transaction.commit().await?;
    Ok(())
}

/// Deletes a custom provider and all its credentials atomically. A combo's
/// restricted foreign key blocks the entire operation if any member is in use.
pub async fn delete(pool: &SqlitePool, id: &str) -> Result<(), AppError> {
    let mut transaction = pool.begin_with("BEGIN IMMEDIATE").await?;
    let exists: bool =
        sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM custom_provider_groups WHERE id = ?)")
            .bind(id)
            .fetch_one(&mut *transaction)
            .await?;
    if !exists {
        return Err(AppError::NotFound("custom provider was not found".into()));
    }
    sqlx::query(
        "DELETE FROM providers WHERE id IN (
            SELECT provider_id FROM provider_group_connections WHERE group_id = ?
         )",
    )
    .bind(id)
    .execute(&mut *transaction)
    .await
    .map_err(|error| {
        if super::is_constraint_violation(&error) {
            AppError::InvalidRequest(
                "custom provider cannot be deleted while a combo uses one of its connections"
                    .into(),
            )
        } else {
            error.into()
        }
    })?;
    // Memberships cascade from the connection deletion; group deletion cannot
    // leave orphaned credentials or hide connections from the dashboard.
    sqlx::query("DELETE FROM custom_provider_groups WHERE id = ?")
        .bind(id)
        .execute(&mut *transaction)
        .await?;
    transaction.commit().await?;
    Ok(())
}

/// Compatibility for the old connection-create form: find or create an endpoint
/// group and save both the unassigned connection and membership atomically.
pub async fn save_legacy_connection(
    pool: &SqlitePool,
    provider: &Provider,
    group_name: &str,
) -> Result<(), AppError> {
    let mut transaction = pool.begin_with("BEGIN IMMEDIATE").await?;
    let row = sqlx::query(
        "SELECT id, name, base_url, protocol FROM custom_provider_groups
         WHERE base_url = ? AND protocol = ? ORDER BY rowid LIMIT 1",
    )
    .bind(&provider.base_url)
    .bind(provider.protocol.as_str())
    .fetch_optional(&mut *transaction)
    .await?;
    let group = if let Some(row) = row {
        from_row(&row)?
    } else {
        let group = CustomGroup {
            id: format!("custom-{}", uuid::Uuid::new_v4()),
            name: group_name.chars().take(100).collect(),
            base_url: provider.base_url.clone(),
            protocol: provider.protocol,
        };
        insert_group(&mut transaction, &group).await?;
        group
    };
    insert_connection(&mut transaction, &group, provider, true).await?;
    transaction.commit().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::{Engine, engine::general_purpose::STANDARD};
    use secrecy::{ExposeSecret, SecretString};
    use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
    use std::str::FromStr;

    use crate::{crypto::Crypto, db::providers};

    const ENDPOINT: &str = "https://api.example.invalid/v1";

    async fn pool() -> SqlitePool {
        crate::db::connect("sqlite::memory:")
            .await
            .expect("connect")
    }

    fn connection(
        id: &str,
        name: &str,
        base_url: &str,
        protocol: Protocol,
        cipher: Vec<u8>,
        enabled: bool,
    ) -> Provider {
        Provider {
            id: id.into(),
            name: name.into(),
            base_url: base_url.into(),
            protocol,
            encrypted_api_key: cipher,
            enabled,
        }
    }

    #[tokio::test]
    async fn stale_unassigned_edit_cannot_override_a_new_group_membership() {
        let pool = pool().await;
        let stale = connection(
            "raced-id",
            "Old key",
            "https://api.openai.com/v1",
            Protocol::OpenAi,
            vec![7, 8, 9],
            true,
        );
        providers::save(&pool, &stale).await.unwrap();
        // Both requests read the row while unassigned. The custom-target edit
        // wins first, then the stale preset-target edit resumes its save.
        let custom_edit = connection(
            "raced-id",
            "Custom key",
            ENDPOINT,
            Protocol::Anthropic,
            vec![7, 8, 9],
            true,
        );
        save_legacy_connection(&pool, &custom_edit, "Custom service")
            .await
            .unwrap();
        providers::save(&pool, &stale).await.unwrap();
        let group = by_connection(&pool, "raced-id").await.unwrap().unwrap();
        let stored = providers::get(&pool, "raced-id").await.unwrap();
        assert_eq!(stored.base_url, group.base_url);
        assert_eq!(stored.protocol, group.protocol);
        assert_eq!(stored.base_url, ENDPOINT);
        assert_eq!(stored.protocol, Protocol::Anthropic);
        assert_eq!(stored.encrypted_api_key, vec![7, 8, 9]);
    }

    #[tokio::test]
    async fn legacy_endpoint_edit_assigns_group_without_changing_id_or_ciphertext() {
        let pool = pool().await;
        let mut conn = connection(
            "old-id",
            "Old key",
            "https://api.openai.com/v1",
            Protocol::OpenAi,
            vec![7, 8, 9],
            true,
        );
        providers::save(&pool, &conn).await.unwrap();
        conn.name = "Renamed key".to_string();
        conn.base_url = ENDPOINT.to_string();
        save_legacy_connection(&pool, &conn, "Custom service")
            .await
            .unwrap();
        let stored = providers::get(&pool, "old-id").await.unwrap();
        assert_eq!(stored.name, "Renamed key");
        assert_eq!(stored.base_url, ENDPOINT);
        assert_eq!(stored.encrypted_api_key, vec![7, 8, 9]);
        let group = by_connection(&pool, "old-id").await.unwrap().unwrap();
        assert_eq!(group.name, "Custom service");
        assert_eq!(members(&pool, &group.id).await.unwrap().len(), 1);
        conn.base_url = "https://other.invalid/v1".to_string();
        assert!(save_legacy_connection(&pool, &conn, "Other").await.is_err());
        assert_eq!(
            providers::get(&pool, "old-id").await.unwrap().base_url,
            ENDPOINT
        );
        assert_eq!(list(&pool).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn empty_named_group_survives_deleting_its_last_connection() {
        let pool = pool().await;
        let group = create(&pool, "solo", ENDPOINT, &Protocol::OpenAi)
            .await
            .expect("create group");

        let summary = &list(&pool).await.expect("list")[0];
        assert_eq!(summary.group.id, group.id);
        assert_eq!(summary.connection_count, 0);
        assert_eq!(summary.enabled_count, 0);
        assert!(members(&pool, &group.id).await.expect("members").is_empty());

        let conn = connection(
            "p1",
            "only",
            ENDPOINT,
            Protocol::OpenAi,
            vec![7, 7, 7],
            true,
        );
        create_connection(&pool, &group.id, &conn)
            .await
            .expect("attach connection");
        assert_eq!(list(&pool).await.expect("list")[0].connection_count, 1);

        // Deleting the connection cascades the membership away, not the group.
        providers::delete(&pool, "p1")
            .await
            .expect("delete provider");
        let summary = &list(&pool).await.expect("list")[0];
        assert_eq!(summary.group.id, group.id);
        assert_eq!(summary.connection_count, 0);
        assert_eq!(summary.enabled_count, 0);
        assert!(members(&pool, &group.id).await.expect("members").is_empty());
        assert!(
            by_connection(&pool, "p1")
                .await
                .expect("by connection")
                .is_none()
        );
    }

    #[tokio::test]
    async fn create_connection_validates_endpoints_and_never_overwrites_existing_rows() {
        let pool = pool().await;
        let group = create(&pool, "primary", ENDPOINT, &Protocol::OpenAi)
            .await
            .expect("create group");

        // A mismatched endpoint is rejected and the transaction leaves no
        // provider row or membership behind.
        let wrong_url = connection(
            "p1",
            "imposter",
            "https://elsewhere.invalid/v1",
            Protocol::OpenAi,
            vec![9],
            true,
        );
        assert!(matches!(
            create_connection(&pool, &group.id, &wrong_url).await,
            Err(AppError::InvalidRequest(_))
        ));
        let wrong_protocol = connection(
            "p1",
            "imposter",
            ENDPOINT,
            Protocol::Anthropic,
            vec![9],
            true,
        );
        assert!(matches!(
            create_connection(&pool, &group.id, &wrong_protocol).await,
            Err(AppError::InvalidRequest(_))
        ));
        assert!(matches!(
            providers::get(&pool, "p1").await,
            Err(AppError::NotFound(_))
        ));
        assert!(members(&pool, &group.id).await.expect("members").is_empty());

        create_connection(
            &pool,
            &group.id,
            &connection(
                "p1",
                "primary",
                ENDPOINT,
                Protocol::OpenAi,
                vec![1, 2, 3],
                true,
            ),
        )
        .await
        .expect("insert");
        create_connection(
            &pool,
            &group.id,
            &connection(
                "p2",
                "secondary",
                ENDPOINT,
                Protocol::OpenAi,
                vec![4],
                false,
            ),
        )
        .await
        .expect("insert disabled");

        let member = &members(&pool, &group.id).await.expect("members")[0];
        assert_eq!(member.id, "p1");
        assert_eq!(member.name, "primary");
        assert_eq!(member.encrypted_api_key, vec![1, 2, 3]);
        assert!(member.enabled);
        let summary = &list(&pool).await.expect("list")[0];
        assert_eq!(summary.connection_count, 2);
        assert_eq!(summary.enabled_count, 1);

        // Re-using an existing id is a constraint violation: the transaction
        // rolls back and the stored credential and membership stay untouched.
        let clash = connection(
            "p1",
            "attacker",
            ENDPOINT,
            Protocol::OpenAi,
            vec![9, 9, 9],
            false,
        );
        assert!(matches!(
            create_connection(&pool, &group.id, &clash).await,
            Err(AppError::InvalidRequest(_))
        ));
        let stored = providers::get(&pool, "p1").await.expect("stored provider");
        assert_eq!(stored.name, "primary");
        assert_eq!(stored.encrypted_api_key, vec![1, 2, 3]);
        assert!(stored.enabled);

        // The duplicate is not silently re-homed into another group either.
        let other = create(&pool, "other", ENDPOINT, &Protocol::OpenAi)
            .await
            .expect("second group");
        assert!(matches!(
            create_connection(&pool, &other.id, &clash).await,
            Err(AppError::InvalidRequest(_))
        ));
        assert!(members(&pool, &other.id).await.expect("members").is_empty());
        assert_eq!(
            by_connection(&pool, "p1")
                .await
                .expect("by connection")
                .map(|group| group.id),
            Some(group.id.clone())
        );
        let names = connection_group_names(&pool).await.expect("names");
        assert_eq!(names.get("p1"), Some(&"primary".to_string()));
    }

    #[tokio::test]
    async fn identically_endpointed_named_groups_stay_separate() {
        let pool = pool().await;
        let first = create(&pool, "Alpha", ENDPOINT, &Protocol::OpenAi)
            .await
            .expect("first group");
        let second = create(&pool, "Beta", ENDPOINT, &Protocol::OpenAi)
            .await
            .expect("second group");
        assert_ne!(first.id, second.id);

        // Both groups exist independently; neither swallows the other.
        let summaries = list(&pool).await.expect("list");
        assert_eq!(summaries.len(), 2);
        assert_eq!(summaries[0].connection_count, 0);
        assert_eq!(summaries[1].connection_count, 0);

        // The legacy form reuses the oldest matching group instead of merging
        // the endpoint into every group or creating a third one.
        save_legacy_connection(
            &pool,
            &connection("p1", "legacy", ENDPOINT, Protocol::OpenAi, vec![5], true),
            "unused",
        )
        .await
        .expect("legacy save");
        let summaries = list(&pool).await.expect("list");
        assert_eq!(summaries.len(), 2);
        assert_eq!(summaries[0].group.id, first.id);
        assert_eq!(summaries[0].connection_count, 1);
        assert_eq!(summaries[1].group.id, second.id);
        assert_eq!(summaries[1].connection_count, 0);
        assert!(
            members(&pool, &second.id)
                .await
                .expect("members")
                .is_empty()
        );
        assert_eq!(
            by_connection(&pool, "p1")
                .await
                .expect("by connection")
                .map(|group| group.id),
            Some(first.id.clone())
        );
    }

    #[tokio::test]
    async fn groups_migration_imports_non_preset_connections_verbatim() {
        let crypto =
            Crypto::from_base64(&SecretString::from(STANDARD.encode([11; 32]))).expect("crypto");
        let secret = SecretString::from("sk-legacy-live-key");
        let alpha_cipher = crypto
            .encrypt_api_key("alpha", &secret)
            .expect("seal alpha");

        // A database as it looked before the groups migration: schema from
        // 0001 only, foreign keys enforced, no migration bookkeeping rows.
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(
                SqliteConnectOptions::from_str("sqlite::memory:")
                    .expect("options")
                    .foreign_keys(true),
            )
            .await
            .expect("legacy pool");
        let foreign_keys: i64 = sqlx::query_scalar("PRAGMA foreign_keys")
            .fetch_one(&pool)
            .await
            .expect("pragma");
        assert_eq!(foreign_keys, 1);
        sqlx::raw_sql(include_str!("../../migrations/0001_initial.sql"))
            .execute(&pool)
            .await
            .expect("apply 0001");

        let seeds = [
            (
                "alpha",
                "Alpha conn",
                "https://custom.example/v1/",
                "openai",
                alpha_cipher.clone(),
            ),
            (
                "beta",
                "Beta conn",
                "https://custom.example/v1",
                "openai",
                vec![3, 1, 4],
            ),
            (
                "gamma",
                "Preset conn",
                "https://api.openai.com/v1",
                "openai",
                vec![9],
            ),
            (
                "delta",
                "Preset clash",
                "https://api.openai.com/v1",
                "anthropic",
                vec![8],
            ),
        ];
        for (id, name, base_url, protocol, cipher) in seeds {
            sqlx::query(
                "INSERT INTO providers (id, name, base_url, protocol, encrypted_api_key)
                 VALUES (?, ?, ?, ?, ?)",
            )
            .bind(id)
            .bind(name)
            .bind(base_url)
            .bind(protocol)
            .bind(cipher)
            .execute(&pool)
            .await
            .expect("seed provider");
        }
        sqlx::query("INSERT INTO combos (id, name) VALUES ('combo-1', 'chain')")
            .execute(&pool)
            .await
            .expect("seed combo");
        sqlx::query(
            "INSERT INTO combo_steps (id, combo_id, provider_id, position, model)
             VALUES ('step-1', 'combo-1', 'alpha', 1, 'gpt-test')",
        )
        .execute(&pool)
        .await
        .expect("seed step");

        sqlx::raw_sql(include_str!(
            "../../migrations/0002_custom_provider_groups.sql"
        ))
        .execute(&pool)
        .await
        .expect("apply 0002");

        // One imported group per endpoint: trailing slashes normalize onto the
        // same group, matching presets stay unassigned, and a preset URL with
        // the wrong protocol still counts as custom.
        let summaries = list(&pool).await.expect("list");
        let ids: Vec<&str> = summaries
            .iter()
            .map(|summary| summary.group.id.as_str())
            .collect();
        assert_eq!(ids, ["custom-alpha", "custom-delta"]);

        let custom = &summaries[0];
        assert_eq!(custom.group.name, "Alpha conn");
        assert_eq!(custom.group.base_url, "https://custom.example/v1");
        assert_eq!(custom.group.protocol, Protocol::OpenAi);
        assert_eq!(custom.connection_count, 2);
        assert_eq!(custom.enabled_count, 2);
        let imported = members(&pool, "custom-alpha").await.expect("members");
        assert_eq!(imported.len(), 2);

        let protocol_clash = &summaries[1];
        assert_eq!(protocol_clash.group.base_url, "https://api.openai.com/v1");
        assert_eq!(protocol_clash.group.protocol, Protocol::Anthropic);
        assert_eq!(protocol_clash.connection_count, 1);
        assert!(
            by_connection(&pool, "gamma")
                .await
                .expect("preset lookup")
                .is_none()
        );
        assert_eq!(
            by_connection(&pool, "delta")
                .await
                .expect("clash lookup")
                .map(|group| group.id),
            Some("custom-delta".to_string())
        );

        // Credentials are untouched: exact ciphertext bytes survive and the
        // original master key still decrypts them.
        let alpha = providers::get(&pool, "alpha").await.expect("alpha");
        assert_eq!(alpha.encrypted_api_key, alpha_cipher);
        assert_eq!(
            crypto
                .decrypt_api_key("alpha", &alpha.encrypted_api_key)
                .expect("decrypt")
                .expose_secret(),
            "sk-legacy-live-key"
        );
        let beta = providers::get(&pool, "beta").await.expect("beta");
        assert_eq!(beta.encrypted_api_key, vec![3, 1, 4]);

        // Combo references ride along unchanged.
        let (provider_id, model): (String, String) =
            sqlx::query_as("SELECT provider_id, model FROM combo_steps WHERE combo_id = 'combo-1'")
                .fetch_one(&pool)
                .await
                .expect("step after upgrade");
        assert_eq!(provider_id, "alpha");
        assert_eq!(model, "gpt-test");

        pool.close().await;
    }

    #[tokio::test]
    async fn delete_clears_members_and_credentials_but_not_unrelated_rows() {
        let pool = pool().await;
        let empty = create(
            &pool,
            "empty",
            "https://empty.invalid/v1",
            &Protocol::OpenAi,
        )
        .await
        .expect("empty group");
        let doomed = create(&pool, "doomed", ENDPOINT, &Protocol::OpenAi)
            .await
            .expect("doomed group");
        create_connection(
            &pool,
            &doomed.id,
            &connection(
                "gone-1",
                "first key",
                ENDPOINT,
                Protocol::OpenAi,
                vec![1, 2, 3],
                true,
            ),
        )
        .await
        .expect("first key");
        create_connection(
            &pool,
            &doomed.id,
            &connection(
                "gone-2",
                "second key",
                ENDPOINT,
                Protocol::OpenAi,
                vec![4, 5],
                false,
            ),
        )
        .await
        .expect("second key");
        let keeper = create(
            &pool,
            "keeper",
            "https://keep.invalid/v1",
            &Protocol::Anthropic,
        )
        .await
        .expect("keeper group");
        create_connection(
            &pool,
            &keeper.id,
            &connection(
                "keep-1",
                "keeper key",
                "https://keep.invalid/v1",
                Protocol::Anthropic,
                vec![9, 9, 9],
                true,
            ),
        )
        .await
        .expect("keeper key");
        // An unassigned preset-matching row: no group deletion may touch it.
        providers::save(
            &pool,
            &connection(
                "preset-1",
                "preset key",
                "https://api.openai.com/v1",
                Protocol::OpenAi,
                vec![7, 7],
                true,
            ),
        )
        .await
        .expect("preset row");

        // An empty group deletes cleanly and touches nothing else.
        delete(&pool, &empty.id).await.expect("empty group delete");
        assert!(matches!(
            get(&pool, &empty.id).await,
            Err(AppError::NotFound(_))
        ));
        assert_eq!(list(&pool).await.expect("list").len(), 2);

        delete(&pool, &doomed.id).await.expect("group delete");

        // The group, both member rows, and their ciphertext blobs are gone,
        // and no orphaned membership can hide surviving connections anywhere.
        assert!(matches!(
            get(&pool, &doomed.id).await,
            Err(AppError::NotFound(_))
        ));
        assert!(matches!(
            providers::get(&pool, "gone-1").await,
            Err(AppError::NotFound(_))
        ));
        assert!(matches!(
            providers::get(&pool, "gone-2").await,
            Err(AppError::NotFound(_))
        ));
        assert!(
            by_connection(&pool, "gone-1")
                .await
                .expect("mapping gone")
                .is_none()
        );
        assert!(
            by_connection(&pool, "gone-2")
                .await
                .expect("mapping gone")
                .is_none()
        );

        // The unrelated group and the unassigned preset row survive intact.
        let summaries = list(&pool).await.expect("list");
        assert_eq!(summaries.len(), 1);
        assert_eq!(summaries[0].group.id, keeper.id);
        assert_eq!(summaries[0].connection_count, 1);
        assert_eq!(summaries[0].enabled_count, 1);
        let kept = &members(&pool, &keeper.id).await.expect("keeper members")[0];
        assert_eq!(kept.id, "keep-1");
        assert_eq!(kept.encrypted_api_key, vec![9, 9, 9]);
        let preset = providers::get(&pool, "preset-1")
            .await
            .expect("preset row survives");
        assert_eq!(preset.base_url, "https://api.openai.com/v1");
        assert_eq!(preset.encrypted_api_key, vec![7, 7]);
        assert!(
            by_connection(&pool, "preset-1")
                .await
                .expect("preset stays unassigned")
                .is_none()
        );
    }

    #[tokio::test]
    async fn combo_references_block_group_delete_until_the_combo_is_removed() {
        let pool = pool().await;
        let group = create(&pool, "locked", ENDPOINT, &Protocol::OpenAi)
            .await
            .expect("group");
        // The unreferenced connection is the older row: a blocked bulk delete
        // must roll that row back too, not just the referenced one.
        create_connection(
            &pool,
            &group.id,
            &connection(
                "free-1",
                "alpha key",
                ENDPOINT,
                Protocol::OpenAi,
                vec![1, 2, 3],
                true,
            ),
        )
        .await
        .expect("alpha key");
        create_connection(
            &pool,
            &group.id,
            &connection(
                "held-1",
                "beta key",
                ENDPOINT,
                Protocol::OpenAi,
                vec![4, 5, 6],
                false,
            ),
        )
        .await
        .expect("beta key");
        let combo = crate::db::combos::create(&pool, "chain")
            .await
            .expect("combo");
        crate::db::combos::add_step(&pool, &combo.id, "held-1", "gpt-test")
            .await
            .expect("combo step");

        assert!(matches!(
            delete(&pool, &group.id).await,
            Err(AppError::InvalidRequest(_))
        ));

        // The rolled-back transaction preserves both connections with exact
        // ciphertext, the group, both membership mappings, and the combo step.
        let survivor = get(&pool, &group.id).await.expect("group survives");
        assert_eq!(survivor.name, "locked");
        let member_rows = members(&pool, &group.id).await.expect("members");
        let ids: Vec<&str> = member_rows
            .iter()
            .map(|member| member.id.as_str())
            .collect();
        assert_eq!(ids, ["free-1", "held-1"]);
        assert_eq!(member_rows[0].encrypted_api_key, vec![1, 2, 3]);
        assert_eq!(member_rows[1].encrypted_api_key, vec![4, 5, 6]);
        for id in ["free-1", "held-1"] {
            assert_eq!(
                by_connection(&pool, id)
                    .await
                    .expect("mapping survives")
                    .map(|mapped| mapped.id),
                Some(group.id.clone())
            );
        }
        let steps = crate::db::combos::steps(&pool, &combo.id)
            .await
            .expect("steps");
        assert_eq!(steps.len(), 1);
        assert_eq!(steps[0].provider_id, "held-1");
        assert_eq!(steps[0].model, "gpt-test");

        // Once the combo is gone the same delete succeeds and removes the
        // group with every member row.
        crate::db::combos::delete(&pool, &combo.id)
            .await
            .expect("combo delete");
        delete(&pool, &group.id).await.expect("group delete");
        assert!(matches!(
            get(&pool, &group.id).await,
            Err(AppError::NotFound(_))
        ));
        assert!(matches!(
            providers::get(&pool, "free-1").await,
            Err(AppError::NotFound(_))
        ));
        assert!(matches!(
            providers::get(&pool, "held-1").await,
            Err(AppError::NotFound(_))
        ));
        assert!(list(&pool).await.expect("list").is_empty());

        // Unknown ids (preset slugs included) are NotFound, never silent
        // no-op deletes.
        assert!(matches!(
            delete(&pool, "openai").await,
            Err(AppError::NotFound(_))
        ));
        assert!(matches!(
            delete(&pool, "custom-00000000-0000-0000-0000-000000000000").await,
            Err(AppError::NotFound(_))
        ));
    }
}
