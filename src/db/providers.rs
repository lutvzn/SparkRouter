//! Persistence for upstream provider configurations.

use std::str::FromStr;

use sqlx::{Row, SqlitePool, sqlite::SqliteRow};

use crate::{error::AppError, providers::Protocol};

/// Columns selected whenever providers are loaded.
const COLUMNS: &str = "id, name, base_url, protocol, encrypted_api_key, enabled";

/// A stored provider configuration.
///
/// `encrypted_api_key` holds opaque ciphertext produced by the crypto layer;
/// the database never sees plaintext credentials.
pub struct Provider {
    pub id: String,
    pub name: String,
    pub base_url: String,
    pub protocol: Protocol,
    pub encrypted_api_key: Vec<u8>,
    pub enabled: bool,
}

/// Parse a provider from a raw row, decoding `protocol` through
/// [`Protocol::from_str`].
pub(super) fn provider_from_row(row: &SqliteRow) -> Result<Provider, AppError> {
    let protocol: String = row.try_get("protocol")?;
    Ok(Provider {
        id: row.try_get("id")?,
        name: row.try_get("name")?,
        base_url: row.try_get("base_url")?,
        protocol: Protocol::from_str(&protocol)?,
        encrypted_api_key: row.try_get::<Vec<u8>, _>("encrypted_api_key")?,
        enabled: row.try_get::<i64, _>("enabled")? != 0,
    })
}

/// All providers, ordered by name.
pub async fn list(pool: &SqlitePool) -> Result<Vec<Provider>, AppError> {
    let sql = format!("SELECT {COLUMNS} FROM providers ORDER BY name");
    let rows = sqlx::query(&sql).fetch_all(pool).await?;
    rows.iter().map(provider_from_row).collect()
}

/// Oldest connection first, so a group's representative does not change when
/// a new connection is added or an existing connection is renamed.
pub async fn list_in_creation_order(pool: &SqlitePool) -> Result<Vec<Provider>, AppError> {
    let sql = format!("SELECT {COLUMNS} FROM providers ORDER BY rowid");
    let rows = sqlx::query(&sql).fetch_all(pool).await?;
    rows.iter().map(provider_from_row).collect()
}

/// Look up a provider by id.
pub async fn get(pool: &SqlitePool, id: &str) -> Result<Provider, AppError> {
    let sql = format!("SELECT {COLUMNS} FROM providers WHERE id = ?");
    let row = sqlx::query(&sql).bind(id).fetch_optional(pool).await?;
    row.as_ref()
        .map(provider_from_row)
        .transpose()?
        .ok_or_else(|| AppError::NotFound(format!("provider '{id}' was not found")))
}

/// Insert a provider, or update the existing row with the same id.
///
/// The upsert preserves creation time and refreshes the last-modified time.
/// An existing group membership owns the endpoint. Resolve it in the same SQL
/// statement as the write so a stale concurrent edit cannot change that target.
pub async fn save(pool: &SqlitePool, provider: &Provider) -> Result<(), AppError> {
    // New presets must not adopt a public namespace already owned by a custom
    // group from an earlier catalog. That would silently broaden exact scopes.
    // Explicit custom membership still owns its saved namespace and endpoint.
    if let Some(preset) = crate::provider_catalog::matching(&provider.base_url, &provider.protocol)
    {
        let conflict: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM provider_slugs WHERE slug = ?) AND NOT EXISTS(SELECT 1 FROM provider_group_connections WHERE provider_id = ?)")
            .bind(preset.routing_slug()).bind(&provider.id).fetch_one(pool).await?;
        if conflict {
            return Err(AppError::InvalidRequest("This preset routing slug belongs to an existing custom provider; use a custom media provider with a distinct slug instead".into()));
        }
    }
    let result = sqlx::query(
        "INSERT INTO providers (id, name, base_url, protocol, encrypted_api_key, enabled)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)
         ON CONFLICT (id) DO UPDATE SET
             name = excluded.name,
              base_url = COALESCE((
                  SELECT g.base_url FROM custom_provider_groups g
                  JOIN provider_group_connections m ON m.group_id = g.id
                  WHERE m.provider_id = excluded.id
              ), excluded.base_url),
              protocol = COALESCE((
                  SELECT g.protocol FROM custom_provider_groups g
                  JOIN provider_group_connections m ON m.group_id = g.id
                  WHERE m.provider_id = excluded.id
              ), excluded.protocol),
             encrypted_api_key = excluded.encrypted_api_key,
              enabled = excluded.enabled,
              updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')",
    )
    .bind(&provider.id)
    .bind(&provider.name)
    .bind(&provider.base_url)
    .bind(provider.protocol.as_str())
    .bind(&provider.encrypted_api_key)
    .bind(provider.enabled)
    .execute(pool)
    .await;

    match result {
        Ok(_) => Ok(()),
        Err(error) if super::is_constraint_violation(&error) => Err(AppError::InvalidRequest(
            "Provider configuration violates a storage constraint".into(),
        )),
        Err(error) => Err(error.into()),
    }
}

/// Delete a provider by id.
///
/// Deleting a provider that is still referenced by a combo step fails with
/// [`AppError::InvalidRequest`]: the schema restricts such deletes so combos
/// never end up with dangling steps.
pub async fn delete(pool: &SqlitePool, id: &str) -> Result<(), AppError> {
    let result = sqlx::query("DELETE FROM providers WHERE id = ?")
        .bind(id)
        .execute(pool)
        .await;

    match result {
        Ok(result) if result.rows_affected() == 0 => {
            Err(AppError::NotFound(format!("provider '{id}' was not found")))
        }
        Ok(_) => Ok(()),
        Err(error) if super::is_constraint_violation(&error) => Err(AppError::InvalidRequest(
            "provider cannot be deleted while a combo step still uses it".into(),
        )),
        Err(error) => Err(error.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::combos;

    async fn pool() -> SqlitePool {
        crate::db::connect("sqlite::memory:")
            .await
            .expect("connect")
    }

    fn provider(id: &str, name: &str, protocol: Protocol) -> Provider {
        Provider {
            id: id.into(),
            name: name.into(),
            base_url: "https://api.example.invalid/v1".into(),
            protocol,
            encrypted_api_key: vec![7, 7, 7],
            enabled: true,
        }
    }

    #[tokio::test]
    async fn creation_order_survives_additions_and_renames() {
        let pool = pool().await;
        let mut first = provider("first", "Zulu", Protocol::OpenAi);
        save(&pool, &first).await.unwrap();
        save(&pool, &provider("second", "Alpha", Protocol::OpenAi))
            .await
            .unwrap();
        assert_eq!(list(&pool).await.unwrap()[0].id, "second");
        assert_eq!(list_in_creation_order(&pool).await.unwrap()[0].id, "first");
        first.name = "Updated".to_string();
        save(&pool, &first).await.unwrap();
        assert_eq!(list_in_creation_order(&pool).await.unwrap()[0].id, "first");
        delete(&pool, "first").await.unwrap();
        assert_eq!(list_in_creation_order(&pool).await.unwrap()[0].id, "second");
    }

    #[tokio::test]
    async fn save_inserts_and_updates_through_one_upsert_path() {
        let pool = pool().await;
        save(&pool, &provider("p1", "primary", Protocol::Anthropic))
            .await
            .expect("insert");

        let stored = get(&pool, "p1").await.expect("get");
        assert_eq!(stored.name, "primary");
        assert_eq!(stored.base_url, "https://api.example.invalid/v1");
        assert_eq!(stored.encrypted_api_key, vec![7, 7, 7]);
        assert!(stored.enabled);
        assert_eq!(stored.protocol.as_str(), Protocol::Anthropic.as_str());

        let mut updated = provider("p1", "renamed", Protocol::OpenAi);
        updated.enabled = false;
        updated.encrypted_api_key = vec![1];
        save(&pool, &updated).await.expect("update");

        let stored = get(&pool, "p1").await.expect("get");
        assert_eq!(stored.name, "renamed");
        assert!(!stored.enabled);
        assert_eq!(stored.encrypted_api_key, vec![1]);
        assert_eq!(stored.protocol.as_str(), Protocol::OpenAi.as_str());
        assert_eq!(list(&pool).await.expect("list").len(), 1);
    }

    #[tokio::test]
    async fn save_preserves_creation_time_and_refreshes_update_time() {
        let pool = pool().await;
        save(&pool, &provider("p1", "primary", Protocol::OpenAi))
            .await
            .expect("insert");

        sqlx::query("UPDATE providers SET created_at = '2020-01-01', updated_at = '2020-01-01' WHERE id = 'p1'")
            .execute(&pool).await.unwrap();
        let (created_before, updated_before): (String, String) =
            sqlx::query_as("SELECT created_at, updated_at FROM providers WHERE id = 'p1'")
                .fetch_one(&pool)
                .await
                .expect("timestamps");

        save(&pool, &provider("p1", "edited", Protocol::OpenAi))
            .await
            .expect("update");

        let (created_after, updated_after): (String, String) =
            sqlx::query_as("SELECT created_at, updated_at FROM providers WHERE id = 'p1'")
                .fetch_one(&pool)
                .await
                .expect("timestamps");
        assert_eq!(created_before, created_after);
        assert_ne!(updated_before, updated_after);
    }

    #[tokio::test]
    async fn list_orders_by_name() {
        let pool = pool().await;
        for (id, name) in [("p3", "gamma"), ("p1", "alpha"), ("p2", "beta")] {
            save(&pool, &provider(id, name, Protocol::OpenAi))
                .await
                .expect("insert");
        }
        let names: Vec<_> = list(&pool)
            .await
            .expect("list")
            .into_iter()
            .map(|provider| provider.name)
            .collect();
        assert_eq!(names, ["alpha", "beta", "gamma"]);
    }

    #[tokio::test]
    async fn missing_providers_report_not_found() {
        let pool = pool().await;
        assert!(matches!(
            get(&pool, "ghost").await,
            Err(AppError::NotFound(_))
        ));
        assert!(matches!(
            delete(&pool, "ghost").await,
            Err(AppError::NotFound(_))
        ));
    }

    #[tokio::test]
    async fn delete_removes_unreferenced_providers() {
        let pool = pool().await;
        save(&pool, &provider("p1", "primary", Protocol::OpenAi))
            .await
            .expect("insert");
        delete(&pool, "p1").await.expect("delete");
        assert!(matches!(get(&pool, "p1").await, Err(AppError::NotFound(_))));
    }

    #[tokio::test]
    async fn deleting_a_referenced_provider_is_rejected() {
        let pool = pool().await;
        save(&pool, &provider("p1", "primary", Protocol::OpenAi))
            .await
            .expect("insert");
        let combo = combos::create(&pool, "chain").await.expect("combo");
        combos::add_step(&pool, &combo.id, "p1", "claude-3")
            .await
            .expect("step");

        assert!(matches!(
            delete(&pool, "p1").await,
            Err(AppError::InvalidRequest(_))
        ));

        // Removing the step frees the provider for deletion.
        let step = &combos::steps(&pool, &combo.id).await.expect("steps")[0];
        combos::remove_step(&pool, &combo.id, &step.id)
            .await
            .expect("remove step");
        delete(&pool, "p1").await.expect("delete after unlink");
    }
}
