//! SQLite storage: connection pool setup, pragmas, migrations, and shared
//! helpers used by the [`providers`] and [`combos`] persistence modules.

pub mod api_keys;
pub mod combos;
pub mod config_transfer;
pub mod connection_models;
pub mod provider_groups;
pub mod provider_slugs;
pub mod providers;
pub mod request_history;

use std::{str::FromStr, time::Duration};

use sqlx::{
    SqlitePool,
    sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions},
};

use crate::error::AppError;

/// Busy timeout applied to every pooled connection so concurrent writers
/// wait instead of failing immediately with `SQLITE_BUSY`.
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

/// Maximum pooled connections for file-backed databases.
const MAX_FILE_CONNECTIONS: u32 = 5;

/// Open the SQLite database at `url`.
///
/// Every connection has foreign keys, WAL journaling, and a busy timeout
/// enabled. The database file is created if missing, pending embedded
/// migrations are applied, and queries are fully runtime (no `DATABASE_URL`
/// is required at compile time).
///
/// In-memory databases (`sqlite::memory:`) are private to a single
/// connection, so the pool is capped at one connection to keep the schema
/// and data visible across queries.
pub async fn connect(url: &str) -> Result<SqlitePool, AppError> {
    let in_memory = url.contains(":memory:");
    let options = SqliteConnectOptions::from_str(url)
        .map_err(|error| AppError::Configuration(format!("invalid database URL: {error}")))?
        .create_if_missing(true)
        .foreign_keys(true)
        .journal_mode(SqliteJournalMode::Wal)
        .busy_timeout(BUSY_TIMEOUT);

    let pool = SqlitePoolOptions::new()
        .max_connections(if in_memory { 1 } else { MAX_FILE_CONNECTIONS })
        .connect_with(options)
        .await?;

    sqlx::migrate!().run(&pool).await?;
    provider_slugs::initialize(&pool).await?;

    Ok(pool)
}

/// True when a SQLite error is a constraint violation (UNIQUE, NOT NULL,
/// CHECK, or FOREIGN KEY). Those map to user-facing
/// [`AppError::InvalidRequest`] errors instead of internal server errors.
pub(crate) fn is_constraint_violation(error: &sqlx::Error) -> bool {
    matches!(
        error,
        sqlx::Error::Database(database)
            if database.code().and_then(|code| code.parse::<i32>().ok())
                .is_some_and(|code| code & 0xff == 19)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn connect_applies_migrations_to_a_fresh_in_memory_database() {
        let pool = connect("sqlite::memory:").await.expect("connect");
        let tables: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM sqlite_master
             WHERE type = 'table'
               AND name IN ('providers', 'combos', 'combo_steps',
                            'custom_provider_groups', 'provider_group_connections',
                            'api_keys', 'connection_model_lists', 'connection_models')",
        )
        .fetch_one(&pool)
        .await
        .expect("schema tables");
        assert_eq!(tables, 8);
    }

    #[tokio::test]
    async fn connect_enables_foreign_keys_on_every_connection() {
        let pool = connect("sqlite::memory:").await.expect("connect");
        let enabled: i64 = sqlx::query_scalar("PRAGMA foreign_keys")
            .fetch_one(&pool)
            .await
            .expect("pragma");
        assert_eq!(enabled, 1);
    }

    #[tokio::test]
    async fn connect_creates_a_file_backed_wal_database_that_persists() {
        let dir = tempfile::TempDir::new().expect("temp dir");
        let path = dir.path().join("router.db");
        let url = format!("sqlite://{}", path.to_string_lossy());

        let pool = connect(&url).await.expect("connect");
        assert!(path.exists(), "database file should be created on disk");

        let mode: String = sqlx::query_scalar("PRAGMA journal_mode")
            .fetch_one(&pool)
            .await
            .expect("journal mode");
        assert_eq!(mode.to_ascii_lowercase(), "wal");

        sqlx::query("INSERT INTO combos (id, name) VALUES ('c1', 'persisted')")
            .execute(&pool)
            .await
            .expect("insert");
        pool.close().await;

        // Reconnecting picks up both the migrated schema and the data.
        let pool = connect(&url).await.expect("reconnect");
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM combos")
            .fetch_one(&pool)
            .await
            .expect("count");
        assert_eq!(count, 1);
        pool.close().await;
    }
}
