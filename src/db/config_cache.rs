//! In-memory copies of the configuration every gateway request reads: the
//! logging mode, API keys, combo routing snapshots, connections, and direct
//! model listings. Explicitly requested to take SQLite off the request path.
//!
//! Every write on a main-pool connection invalidates all of it. A commit hook
//! marks the commit as it starts; the mark is settled once the connection goes
//! back to the pool or closes, after the commit is visible to other
//! connections. Entries read while any commit is unsettled, or across one, are
//! not kept, so a lookup never returns configuration older than the last
//! commit that finished. Writes by other processes (for example the `sqlite3`
//! shell) are not noticed until this process commits something itself.
//!
//! Only lookups that found something are kept, so attacker-chosen names and
//! secrets cannot grow the cache beyond the stored configuration.

use std::{
    collections::HashMap,
    future::Future,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, AtomicUsize, Ordering},
    },
};

use sqlx::{SqliteConnection, SqlitePool, sqlite::SqlitePoolOptions};

use super::{
    api_keys::AuthenticatedKey,
    combos::{self, Category, Combo, RoutingStep},
    providers::Provider,
    settings::LoggingMode,
};
use crate::error::AppError;

/// Changes whenever a main-pool commit starts or settles. Process-wide, so a
/// second pool opened on the same file also invalidates; extra invalidations
/// only cost a reload.
static GENERATION: AtomicU64 = AtomicU64::new(0);
/// Commits started but not yet known to be visible to other connections.
static UNSETTLED: AtomicUsize = AtomicUsize::new(0);

/// Watch commits on every connection of a pool.
pub(super) fn track_commits(options: SqlitePoolOptions) -> SqlitePoolOptions {
    options
        .after_connect(|connection, _| Box::pin(watch(connection)))
        .after_release(|connection, _| {
            Box::pin(async move {
                watch(connection).await?;
                Ok(true)
            })
        })
}

/// Install a fresh commit watch. Replacing the hook drops the previous watch,
/// which settles any commit it saw; that happens after the commit finished.
async fn watch(connection: &mut SqliteConnection) -> Result<(), sqlx::Error> {
    let mut commits = CommitWatch { committed: false };
    connection.lock_handle().await?.set_commit_hook(move || {
        commits.started();
        true
    });
    Ok(())
}

struct CommitWatch {
    committed: bool,
}

impl CommitWatch {
    fn started(&mut self) {
        if !std::mem::replace(&mut self.committed, true) {
            UNSETTLED.fetch_add(1, Ordering::SeqCst);
        }
        GENERATION.fetch_add(1, Ordering::SeqCst);
    }
}

impl Drop for CommitWatch {
    fn drop(&mut self) {
        if self.committed {
            GENERATION.fetch_add(1, Ordering::SeqCst);
            UNSETTLED.fetch_sub(1, Ordering::SeqCst);
        }
    }
}

/// A key found by its fingerprint, with the expiry it was loaded with.
#[derive(Clone)]
pub(crate) struct CachedKey {
    pub key: AuthenticatedKey,
    pub expires_at: Option<i64>,
}

/// A connection with its routing namespace and display group name.
pub(crate) type Connection = (Provider, String, String);

/// A combo with its validated steps, as [`combos::routing_snapshot`] loads it.
pub(crate) type RoutingSnapshot = (Combo, Vec<RoutingStep>);

#[derive(Default)]
struct Entries {
    generation: u64,
    logging_mode: Option<LoggingMode>,
    cooldown: Option<bool>,
    direct_affinity: Option<u32>,
    keys_exist: Option<bool>,
    keys: HashMap<String, CachedKey>,
    combos: HashMap<(String, Category), Arc<RoutingSnapshot>>,
    connections: HashMap<String, Arc<Connection>>,
    direct: HashMap<(String, String), Arc<[String]>>,
}

/// The generation an entry was read at; it is stored only if still current.
#[derive(Clone, Copy)]
struct Token(u64);

#[derive(Default)]
pub(crate) struct ConfigCache {
    entries: Mutex<Entries>,
}

impl ConfigCache {
    fn read<T>(&self, get: impl FnOnce(&Entries) -> Option<T>) -> (Token, Option<T>) {
        let mut entries = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        let generation = GENERATION.load(Ordering::SeqCst);
        if entries.generation != generation {
            *entries = Entries {
                generation,
                ..Entries::default()
            };
        }
        (Token(generation), get(&entries))
    }

    fn store(&self, token: Token, put: impl FnOnce(&mut Entries)) {
        let mut entries = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        // Settled is checked first: settling bumps the generation before
        // decrementing, so a read that overlapped a commit always fails one.
        if UNSETTLED.load(Ordering::SeqCst) != 0 || GENERATION.load(Ordering::SeqCst) != token.0 {
            return;
        }
        if entries.generation != token.0 {
            *entries = Entries {
                generation: token.0,
                ..Entries::default()
            };
        }
        put(&mut entries);
    }

    /// The logging mode, loaded by `load` when not cached; `None` if it failed.
    pub(crate) async fn logging_mode(
        &self,
        load: impl Future<Output = Option<LoggingMode>>,
    ) -> Option<LoggingMode> {
        let (token, cached) = self.read(|entries| entries.logging_mode);
        if cached.is_some() {
            return cached;
        }
        let mode = load.await?;
        self.store(token, |entries| entries.logging_mode = Some(mode));
        Some(mode)
    }

    /// Whether failing targets cool down.
    pub(crate) async fn cooldown(&self, pool: &SqlitePool) -> Result<bool, AppError> {
        let (token, cached) = self.read(|entries| entries.cooldown);
        if let Some(enabled) = cached {
            return Ok(enabled);
        }
        let enabled = super::settings::target_cooldown(pool).await?;
        self.store(token, |entries| entries.cooldown = Some(enabled));
        Ok(enabled)
    }

    /// Minutes direct model IDs keep their last successful connection; 0 is off.
    pub(crate) async fn direct_affinity(&self, pool: &SqlitePool) -> Result<u32, AppError> {
        let (token, cached) = self.read(|entries| entries.direct_affinity);
        if let Some(minutes) = cached {
            return Ok(minutes);
        }
        let minutes = super::settings::direct_affinity_minutes(pool).await?;
        self.store(token, |entries| entries.direct_affinity = Some(minutes));
        Ok(minutes)
    }

    /// Whether any generated API key exists, expired ones included.
    pub(crate) async fn keys_exist(&self, pool: &SqlitePool) -> Result<bool, AppError> {
        let (token, cached) = self.read(|entries| entries.keys_exist);
        if let Some(exist) = cached {
            return Ok(exist);
        }
        let exist = super::api_keys::any(pool).await?;
        self.store(token, |entries| entries.keys_exist = Some(exist));
        Ok(exist)
    }

    /// The unexpired key with this fingerprint. Expiry is checked on every
    /// lookup, so a cached key stops authenticating when it expires.
    pub(crate) async fn key(
        &self,
        pool: &SqlitePool,
        fingerprint: String,
        now: i64,
    ) -> Result<Option<AuthenticatedKey>, AppError> {
        let (token, cached) = self.read(|entries| entries.keys.get(&fingerprint).cloned());
        let found = match cached {
            Some(found) => found,
            None => {
                let Some(found) = super::api_keys::by_fingerprint(pool, &fingerprint).await? else {
                    return Ok(None);
                };
                let stored = found.clone();
                self.store(token, |entries| {
                    entries.keys.insert(fingerprint, stored);
                });
                found
            }
        };
        Ok(found
            .expires_at
            .is_none_or(|expires_at| expires_at > now)
            .then_some(found.key))
    }

    /// The validated routing snapshot of a combo for one category.
    pub(crate) async fn combo(
        &self,
        pool: &SqlitePool,
        name: &str,
        category: Category,
    ) -> Result<Arc<RoutingSnapshot>, AppError> {
        let key = (name.to_owned(), category);
        let (token, cached) = self.read(|entries| entries.combos.get(&key).cloned());
        if let Some(snapshot) = cached {
            return Ok(snapshot);
        }
        let snapshot = Arc::new(combos::routing_snapshot(pool, name, category).await?);
        self.store(token, |entries| {
            entries.combos.insert(key, snapshot.clone());
        });
        Ok(snapshot)
    }

    /// True when a combo with this name and category is cached; a miss says
    /// nothing either way.
    pub(crate) fn has_combo(&self, name: &str, category: Category) -> bool {
        self.read(|entries| Some(entries.combos.contains_key(&(name.to_owned(), category))))
            .1
            .unwrap_or(false)
    }

    /// A connection with its namespace and group name, as
    /// [`super::provider_slugs::connection_with_name`] returns it.
    pub(crate) async fn connection(
        &self,
        pool: &SqlitePool,
        id: &str,
    ) -> Result<Option<Arc<Connection>>, AppError> {
        let (token, cached) = self.read(|entries| entries.connections.get(id).cloned());
        if cached.is_some() {
            return Ok(cached);
        }
        let Some(connection) = super::provider_slugs::connection_with_name(pool, id).await? else {
            return Ok(None);
        };
        let connection = Arc::new(connection);
        self.store(token, |entries| {
            entries
                .connections
                .insert(id.to_owned(), connection.clone());
        });
        Ok(Some(connection))
    }

    /// Connections listing a public provider/model ID, oldest first.
    pub(crate) async fn direct_targets(
        &self,
        pool: &SqlitePool,
        slug: &str,
        model: &str,
    ) -> Result<Arc<[String]>, AppError> {
        let key = (slug.to_owned(), model.to_owned());
        let (token, cached) = self.read(|entries| entries.direct.get(&key).cloned());
        if let Some(targets) = cached {
            return Ok(targets);
        }
        let targets: Arc<[String]> =
            super::connection_models::providers_for_public_model(pool, slug, model)
                .await?
                .into();
        if !targets.is_empty() {
            self.store(token, |entries| {
                entries.direct.insert(key, targets.clone());
            });
        }
        Ok(targets)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        crypto::Crypto,
        db::{api_keys, providers, settings},
        providers::Protocol,
    };
    use base64::{Engine, engine::general_purpose::STANDARD};
    use secrecy::{ExposeSecret, SecretString};

    // Other tests commit concurrently and share the generation, so these
    // check only what must hold whether or not a lookup hit the cache.

    async fn mode(cache: &ConfigCache, pool: &SqlitePool) -> Option<LoggingMode> {
        cache
            .logging_mode(async { settings::logging_mode(pool).await.ok() })
            .await
    }

    #[tokio::test]
    async fn every_commit_is_seen_by_the_next_lookup() {
        let pool = crate::db::connect("sqlite::memory:").await.unwrap();
        let cache = ConfigCache::default();
        let crypto = Crypto::from_base64(&SecretString::from(STANDARD.encode([5u8; 32]))).unwrap();
        for expected in [LoggingMode::Extended, LoggingMode::Off, LoggingMode::Basic] {
            mode(&cache, &pool).await;
            settings::set_logging_mode(&pool, expected).await.unwrap();
            assert!(mode(&cache, &pool).await == Some(expected));
        }

        assert!(!cache.keys_exist(&pool).await.unwrap());
        let issued = api_keys::create(&pool, &crypto, "key").await.unwrap();
        assert!(cache.keys_exist(&pool).await.unwrap());
        let fingerprint = crypto
            .gateway_key_fingerprint(issued.secret.expose_secret())
            .unwrap();
        let found = cache.key(&pool, fingerprint.clone(), 0).await.unwrap();
        assert_eq!(found.map(|key| key.id), Some(issued.id.clone()));
        api_keys::delete(&pool, &issued.id).await.unwrap();
        assert!(cache.key(&pool, fingerprint, 0).await.unwrap().is_none());
        assert!(!cache.keys_exist(&pool).await.unwrap());

        for id in ["provider-a", "provider-b"] {
            providers::save(
                &pool,
                &Provider {
                    id: id.into(),
                    name: id.into(),
                    base_url: "https://api.example.invalid".into(),
                    protocol: Protocol::OpenAi,
                    encrypted_api_key: vec![1, 2, 3],
                    enabled: true,
                },
            )
            .await
            .unwrap();
        }
        let combo = combos::create(&pool, "cached").await.unwrap();
        combos::add_step(&pool, &combo.id, "provider-a", "first")
            .await
            .unwrap();
        let steps = |snapshot: Arc<RoutingSnapshot>| {
            snapshot
                .1
                .iter()
                .map(|entry| (entry.step.model.clone(), entry.provider.enabled))
                .collect::<Vec<_>>()
        };
        let first = cache.combo(&pool, "cached", Category::Text).await.unwrap();
        assert_eq!(steps(first), [("first".to_owned(), true)]);
        combos::add_step(&pool, &combo.id, "provider-b", "second")
            .await
            .unwrap();
        sqlx::query("UPDATE providers SET enabled = 0 WHERE id = 'provider-a'")
            .execute(&pool)
            .await
            .unwrap();
        let both = cache.combo(&pool, "cached", Category::Text).await.unwrap();
        assert_eq!(
            steps(both),
            [("first".to_owned(), false), ("second".to_owned(), true)]
        );
        let connection = cache.connection(&pool, "provider-a").await.unwrap();
        assert!(!connection.unwrap().0.enabled);
        sqlx::query("UPDATE providers SET enabled = 1 WHERE id = 'provider-a'")
            .execute(&pool)
            .await
            .unwrap();
        let connection = cache.connection(&pool, "provider-a").await.unwrap();
        assert!(connection.unwrap().0.enabled);
        combos::delete(&pool, &combo.id).await.unwrap();
        assert!(!cache.has_combo("cached", Category::Text));
        assert!(cache.combo(&pool, "cached", Category::Text).await.is_err());
    }

    #[tokio::test]
    async fn a_cached_key_stops_authenticating_when_it_expires() {
        let pool = crate::db::connect("sqlite::memory:").await.unwrap();
        let cache = ConfigCache::default();
        let crypto = Crypto::from_base64(&SecretString::from(STANDARD.encode([6u8; 32]))).unwrap();
        let issued = api_keys::create_with_policy(
            &pool,
            &crypto,
            "expiring",
            api_keys::KeyAccess::All,
            api_keys::Expiry::AfterDays(1),
        )
        .await
        .unwrap();
        let fingerprint = crypto
            .gateway_key_fingerprint(issued.secret.expose_secret())
            .unwrap();
        let expires_at: i64 = sqlx::query_scalar("SELECT expires_at FROM api_keys")
            .fetch_one(&pool)
            .await
            .unwrap();
        let before = cache.key(&pool, fingerprint.clone(), expires_at - 1).await;
        assert!(before.unwrap().is_some());
        let after = cache.key(&pool, fingerprint, expires_at).await;
        assert!(after.unwrap().is_none());
    }

    #[tokio::test]
    async fn a_lookup_overlapping_an_unsettled_commit_is_not_kept() {
        let pool = crate::db::connect("sqlite::memory:").await.unwrap();
        let cache = ConfigCache::default();
        // The commit stays unsettled while its connection is checked out.
        let mut connection = pool.acquire().await.unwrap();
        sqlx::query("UPDATE dashboard_settings SET logging_mode = 'extended' WHERE id = 1")
            .execute(&mut *connection)
            .await
            .unwrap();
        let loaded = cache.logging_mode(async { Some(LoggingMode::Off) }).await;
        assert!(loaded == Some(LoggingMode::Off));
        let reloaded = cache.logging_mode(async { Some(LoggingMode::Basic) }).await;
        assert!(
            reloaded == Some(LoggingMode::Basic),
            "the first load was kept"
        );
        drop(connection);
    }
}
