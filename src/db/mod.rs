//! SQLite storage: connection pool setup, pragmas, migrations, and shared
//! helpers used by the [`providers`] and [`combos`] persistence modules.

pub mod api_keys;
pub mod combos;
pub(crate) mod config_cache;
pub mod config_transfer;
pub mod connection_models;
pub mod dashboard_admin;
pub(crate) mod history_writer;
pub mod master_key;
pub mod model_categories;
pub mod provider_favorites;
pub mod provider_groups;
pub mod provider_slugs;
pub mod provider_tags;
pub mod providers;
pub mod request_history;
pub mod request_payloads;
pub mod settings;
pub mod usage;

use std::{str::FromStr, time::Duration};

use sqlx::{
    SqlitePool,
    sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous},
};

use crate::error::AppError;

/// Busy timeout applied to every pooled connection so concurrent writers
/// wait instead of failing immediately with `SQLITE_BUSY`.
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

/// How long a history batch waits for the writer connection before it is
/// abandoned. The batch writer is that pool's only user, so this matters only
/// for in-memory databases sharing the main connection. A transaction that has
/// started runs to completion (still subject to [`BUSY_TIMEOUT`] for the lock).
pub(crate) const HISTORY_ACQUIRE_TIMEOUT: Duration = Duration::from_secs(2);

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

    let max_connections = if in_memory { 1 } else { MAX_FILE_CONNECTIONS };
    // Commits on this pool invalidate the request-path configuration cache.
    let mut pool_options =
        config_cache::track_commits(SqlitePoolOptions::new().max_connections(max_connections));
    if in_memory {
        // Closing the only connection would silently replace the database
        // with an empty one, so never retire it for idleness or age.
        pool_options = pool_options
            .min_connections(1)
            .idle_timeout(None)
            .max_lifetime(None);
    }
    let pool = pool_options.connect_with(options).await?;

    sqlx::migrate!().run(&pool).await?;
    provider_slugs::initialize(&pool).await?;

    Ok(pool)
}

/// Open the dedicated request-history writer for an already-migrated database.
///
/// History is written once or twice per gateway request. On the shared pool
/// those writers held connections while sleeping in SQLite's busy handler for
/// the single write lock, starving authentication and routing reads, and each
/// commit waited for an fsync. This pool has exactly one connection, owned by
/// [`history_writer`], which queues writes in memory instead of contending
/// inside SQLite and commits them in batches. It commits
/// with `synchronous = NORMAL`: in WAL mode that cannot corrupt the database,
/// but the newest history rows can be lost on power failure until the next
/// fully synchronous commit or checkpoint. Configuration, keys, and settings
/// keep the main pool's default full synchronization.
///
/// An in-memory database is private to its connection, so it reuses `main`.
pub async fn connect_history_writer(url: &str, main: &SqlitePool) -> Result<SqlitePool, AppError> {
    if url.contains(":memory:") {
        return Ok(main.clone());
    }
    let options = SqliteConnectOptions::from_str(url)
        .map_err(|error| AppError::Configuration(format!("invalid database URL: {error}")))?
        .foreign_keys(true)
        .journal_mode(SqliteJournalMode::Wal)
        .synchronous(SqliteSynchronous::Normal)
        .busy_timeout(BUSY_TIMEOUT);
    Ok(SqlitePoolOptions::new()
        .max_connections(1)
        .acquire_timeout(HISTORY_ACQUIRE_TIMEOUT)
        .connect_with(options)
        .await?)
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
    async fn the_affinity_timeout_migration_keeps_basic_and_smart_at_thirty_minutes() {
        let pool = SqlitePool::connect("sqlite::memory:").await.unwrap();
        let mut before = sqlx::migrate!();
        before.migrations = before
            .migrations
            .iter()
            .filter(|migration| migration.version < 27)
            .cloned()
            .collect::<Vec<_>>()
            .into();
        before.run(&pool).await.unwrap();
        sqlx::query(
            "INSERT INTO combos (id, name, strategy, affinity_mode, sticky_minutes) VALUES \
             ('o', 'off', 'priority', 'off', 5), ('b', 'basic', 'priority', 'basic', 5), \
             ('m', 'smart', 'priority', 'smart', 12), ('s', 'sticky', 'priority', 'sticky', 7)",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::migrate!().run(&pool).await.unwrap();
        let minutes: Vec<(String, i64)> =
            sqlx::query_as("SELECT id, sticky_minutes FROM combos ORDER BY id")
                .fetch_all(&pool)
                .await
                .unwrap();
        assert_eq!(
            minutes,
            [
                ("b".into(), 30),
                ("m".into(), 30),
                ("o".into(), 5),
                ("s".into(), 7)
            ]
        );
    }

    #[tokio::test]
    async fn the_embedding_category_migration_keeps_combos_steps_and_declarations() {
        let pool = SqlitePool::connect("sqlite::memory:").await.unwrap();
        sqlx::query("PRAGMA foreign_keys = ON")
            .execute(&pool)
            .await
            .unwrap();
        let mut before = sqlx::migrate!();
        before.migrations = before
            .migrations
            .iter()
            .filter(|migration| migration.version < 26)
            .cloned()
            .collect::<Vec<_>>()
            .into();
        before.run(&pool).await.unwrap();
        for statement in [
            "INSERT INTO providers (id, name, base_url, protocol, encrypted_api_key, enabled) VALUES ('p', 'P', 'https://example.test/v1', 'openai', x'', 1)",
            "INSERT INTO combos (id, name, strategy, affinity_mode, sticky_minutes, routing_revision, category) VALUES ('c', 'kept', 'round_robin', 'sticky', 30, 4, 'stt')",
            "INSERT INTO combo_steps (id, combo_id, provider_id, position, model) VALUES ('s0', 'c', 'p', 0, 'first'), ('s1', 'c', 'p', 1, 'second')",
            "INSERT INTO model_categories (provider_id, model, category) VALUES ('p', 'first', 'stt')",
        ] {
            sqlx::query(statement).execute(&pool).await.unwrap();
        }
        sqlx::migrate!().run(&pool).await.unwrap();
        let combo: (String, String, String, i64, i64, String) = sqlx::query_as(
            "SELECT name, strategy, affinity_mode, sticky_minutes, routing_revision, category FROM combos WHERE id = 'c'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(
            combo,
            (
                "kept".into(),
                "round_robin".into(),
                "sticky".into(),
                30,
                4,
                "stt".into()
            )
        );
        let steps: Vec<(String, i64, String)> =
            sqlx::query_as("SELECT id, position, model FROM combo_steps ORDER BY position")
                .fetch_all(&pool)
                .await
                .unwrap();
        assert_eq!(
            steps,
            [
                ("s0".into(), 0, "first".into()),
                ("s1".into(), 1, "second".into())
            ]
        );
        let declared: String = sqlx::query_scalar("SELECT category FROM model_categories")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(declared, "stt");
        // The rebuilt tables keep their constraints and accept the new category.
        sqlx::query("UPDATE combos SET category = 'embedding' WHERE id = 'c'")
            .execute(&pool)
            .await
            .unwrap();
        assert!(
            sqlx::query("UPDATE combos SET category = 'vectors'")
                .execute(&pool)
                .await
                .is_err()
        );
        assert!(
            sqlx::query("INSERT INTO combo_steps (id, combo_id, provider_id, position, model) VALUES ('dup', 'c', 'p', 0, 'x')")
                .execute(&pool)
                .await
                .is_err()
        );
        assert!(
            sqlx::query("DELETE FROM providers")
                .execute(&pool)
                .await
                .is_err()
        );
        sqlx::query("DELETE FROM combos")
            .execute(&pool)
            .await
            .unwrap();
        let remaining: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM combo_steps")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(remaining, 0);
    }

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
    async fn history_writer_is_a_single_relaxed_connection_sharing_the_file() {
        let directory = tempfile::tempdir().unwrap();
        let url = format!("sqlite://{}", directory.path().join("history.db").display());
        let main = connect(&url).await.unwrap();
        let writer = connect_history_writer(&url, &main).await.unwrap();
        assert_eq!(writer.options().get_max_connections(), 1);
        assert_eq!(
            writer.options().get_acquire_timeout(),
            HISTORY_ACQUIRE_TIMEOUT
        );
        // 2 = FULL keeps configuration durable; 1 = NORMAL only for history.
        let main_sync: i64 = sqlx::query_scalar("PRAGMA synchronous")
            .fetch_one(&main)
            .await
            .unwrap();
        let writer_sync: i64 = sqlx::query_scalar("PRAGMA synchronous")
            .fetch_one(&writer)
            .await
            .unwrap();
        assert_eq!((main_sync, writer_sync), (2, 1));
        let foreign_keys: i64 = sqlx::query_scalar("PRAGMA foreign_keys")
            .fetch_one(&writer)
            .await
            .unwrap();
        assert_eq!(foreign_keys, 1, "retention pruning relies on cascades");

        let record = request_history::Record {
            id: "req-1".into(),
            source: "api".into(),
            model: "combo/a".into(),
            streaming: false,
            outcome: "success".into(),
            status: 200,
            latency_ms: 1,
            input_tokens: None,
            output_tokens: None,
            attempts: vec![request_history::Attempt {
                provider_id: "p".into(),
                provider_name: "P".into(),
                model: "m".into(),
                position: 1,
                outcome: "selected".into(),
                latency_ms: 1,
            }],
        };
        request_history::save(&writer, &record).await.unwrap();
        assert_eq!(request_history::count(&main).await.unwrap(), 1);
        sqlx::query("DELETE FROM request_history")
            .execute(&writer)
            .await
            .unwrap();
        let attempts: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM request_attempts")
            .fetch_one(&main)
            .await
            .unwrap();
        assert_eq!(attempts, 0);

        let memory = connect("sqlite::memory:").await.unwrap();
        let shared = connect_history_writer("sqlite::memory:", &memory)
            .await
            .unwrap();
        request_history::save(&shared, &record).await.unwrap();
        assert_eq!(request_history::count(&memory).await.unwrap(), 1);
    }

    #[tokio::test]
    async fn in_memory_pool_never_retires_its_only_connection() {
        let pool = connect("sqlite::memory:").await.expect("connect");
        assert_eq!(pool.options().get_idle_timeout(), None);
        assert_eq!(pool.options().get_max_lifetime(), None);
        assert_eq!(pool.options().get_min_connections(), 1);
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
