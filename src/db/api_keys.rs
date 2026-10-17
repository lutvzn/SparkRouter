//! Dashboard-generated gateway API keys.
//!
//! The full secret is encrypted only until the one-time reveal page reads it.
//! Authentication uses an HMAC fingerprint and never needs the secret again.

use aes_gcm::aead::{OsRng, rand_core::RngCore};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use secrecy::{ExposeSecret, SecretString};
use sqlx::{Row, SqlitePool};
use std::collections::{BTreeSet, HashMap};
use zeroize::Zeroizing;

use crate::crypto::Crypto;
use crate::error::AppError;

const MAX_NAME_CHARS: usize = 64;
const PREFIX_CHARS: usize = 11;

/// A stored key safe to render. Never includes the secret or its fingerprint.
pub struct ApiKey {
    pub id: String,
    pub name: String,
    pub prefix: String,
    pub created_at: String,
    pub access: KeyAccess,
    pub expires_at: Option<String>,
    pub expired: bool,
    pub limits: KeyLimits,
}

#[derive(Clone, Copy, Default)]
pub struct KeyLimits {
    pub requests_per_minute: Option<u32>,
    pub max_concurrent: Option<u32>,
}
impl KeyLimits {
    pub fn validate(self) -> Result<Self, AppError> {
        if self
            .requests_per_minute
            .is_some_and(|n| !(1..=1_000_000).contains(&n))
            || self
                .max_concurrent
                .is_some_and(|n| !(1..=10_000).contains(&n))
        {
            return Err(AppError::InvalidRequest("Requests per minute must be 1–1000000 and concurrency 1–10000; blank means unlimited".into()));
        }
        Ok(self)
    }
}

#[derive(Clone)]
pub struct AuthenticatedKey {
    pub id: String,
    pub access: KeyAccess,
    pub limits: KeyLimits,
}

/// Exact public model IDs, not upstream targets inside an allowed combo.
/// No wildcard or prefix matching. Selected([]) deliberately denies all.
#[derive(Clone)]
pub enum KeyAccess {
    All,
    Selected(Vec<String>),
}

impl KeyAccess {
    pub fn allows(&self, model: &str) -> bool {
        match self {
            Self::All => true,
            Self::Selected(models) => models.iter().any(|allowed| allowed == model),
        }
    }
}

pub enum Expiry {
    Keep,
    Never,
    AfterDays(i64),
}

/// Newly created key. The secret is not `Debug` and is not written to the list.
pub struct IssuedKey {
    pub id: String,
    pub secret: SecretString,
}

pub async fn list(pool: &SqlitePool) -> Result<Vec<ApiKey>, AppError> {
    let mut transaction = pool.begin().await?;
    let rows = sqlx::query(
        "SELECT id, name, key_prefix, created_at, restricted, requests_per_minute, max_concurrent,
         strftime('%Y-%m-%dT%H:%M:%SZ', expires_at, 'unixepoch') AS expires_at,
         expires_at IS NOT NULL AND expires_at <= unixepoch() AS expired
         FROM api_keys ORDER BY created_at, id",
    )
    .fetch_all(&mut *transaction)
    .await?;
    let scopes = sqlx::query("SELECT key_id, model FROM api_key_models ORDER BY model")
        .fetch_all(&mut *transaction)
        .await?;
    let mut models = HashMap::<String, Vec<String>>::new();
    for row in scopes {
        models
            .entry(row.try_get("key_id")?)
            .or_default()
            .push(row.try_get("model")?);
    }
    let result = rows
        .iter()
        .map(|row| {
            let id: String = row.try_get("id")?;
            Ok(ApiKey {
                access: if row.try_get::<bool, _>("restricted")? {
                    KeyAccess::Selected(models.remove(&id).unwrap_or_default())
                } else {
                    KeyAccess::All
                },
                id,
                name: row.try_get("name")?,
                prefix: row.try_get("key_prefix")?,
                created_at: row.try_get("created_at")?,
                expires_at: row.try_get("expires_at")?,
                expired: row.try_get("expired")?,
                limits: KeyLimits {
                    requests_per_minute: row.try_get("requests_per_minute")?,
                    max_concurrent: row.try_get("max_concurrent")?,
                },
            })
        })
        .collect();
    transaction.commit().await?;
    result
}

pub async fn create(pool: &SqlitePool, crypto: &Crypto, name: &str) -> Result<IssuedKey, AppError> {
    create_with_policy(pool, crypto, name, KeyAccess::All, Expiry::Never).await
}

pub async fn create_with_policy(
    pool: &SqlitePool,
    crypto: &Crypto,
    name: &str,
    access: KeyAccess,
    expiry: Expiry,
) -> Result<IssuedKey, AppError> {
    create_with_limits(pool, crypto, name, access, expiry, KeyLimits::default()).await
}

pub async fn create_with_limits(
    pool: &SqlitePool,
    crypto: &Crypto,
    name: &str,
    access: KeyAccess,
    expiry: Expiry,
    limits: KeyLimits,
) -> Result<IssuedKey, AppError> {
    let limits = limits.validate()?;
    let name = validate_name(name)?;
    let access = validate_access(access)?;
    let mut transaction = pool.begin_with("BEGIN IMMEDIATE").await?;
    let expires_at = expiration(&mut transaction, expiry, None).await?;
    let id = uuid::Uuid::new_v4().to_string();
    let secret = generate_secret();
    let prefix: String = secret.expose_secret().chars().take(PREFIX_CHARS).collect();
    let key_hash = crypto.gateway_key_fingerprint(secret.expose_secret())?;
    let reveal_envelope = crypto.encrypt_api_key(&id, &secret)?;
    sqlx::query(
        "INSERT INTO api_keys (id, name, key_prefix, key_hash, reveal_envelope, restricted, expires_at, requests_per_minute, max_concurrent)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(&id)
    .bind(name)
    .bind(prefix)
    .bind(key_hash)
    .bind(reveal_envelope)
    .bind(matches!(access, KeyAccess::Selected(_)))
    .bind(expires_at)
    .bind(limits.requests_per_minute)
    .bind(limits.max_concurrent)
    .execute(&mut *transaction)
    .await?;
    write_models(&mut transaction, &id, &access).await?;
    transaction.commit().await?;
    Ok(IssuedKey { id, secret })
}

/// Change authorization without revealing or rotating the existing secret.
pub async fn update_policy(
    pool: &SqlitePool,
    id: &str,
    access: KeyAccess,
    expiry: Expiry,
) -> Result<(), AppError> {
    update_policy_with_limits(pool, id, access, expiry, None).await
}

/// `None` preserves limits for callers editing only scopes/expiry.
pub async fn update_policy_with_limits(
    pool: &SqlitePool,
    id: &str,
    access: KeyAccess,
    expiry: Expiry,
    limits: Option<KeyLimits>,
) -> Result<(), AppError> {
    let limits = limits.map(KeyLimits::validate).transpose()?;
    let access = validate_access(access)?;
    let mut transaction = pool.begin_with("BEGIN IMMEDIATE").await?;
    let existing: Option<i64> = sqlx::query_scalar("SELECT expires_at FROM api_keys WHERE id = ?")
        .bind(id)
        .fetch_optional(&mut *transaction)
        .await?
        .ok_or_else(|| AppError::NotFound("API key was not found".into()))?;
    let expires_at = expiration(&mut transaction, expiry, existing).await?;
    sqlx::query("UPDATE api_keys SET restricted = ?, expires_at = ? WHERE id = ?")
        .bind(matches!(access, KeyAccess::Selected(_)))
        .bind(expires_at)
        .bind(id)
        .execute(&mut *transaction)
        .await?;
    if let Some(limits) = limits {
        sqlx::query("UPDATE api_keys SET requests_per_minute = ?, max_concurrent = ? WHERE id = ?")
            .bind(limits.requests_per_minute)
            .bind(limits.max_concurrent)
            .bind(id)
            .execute(&mut *transaction)
            .await?;
    }
    sqlx::query("DELETE FROM api_key_models WHERE key_id = ?")
        .bind(id)
        .execute(&mut *transaction)
        .await?;
    write_models(&mut transaction, id, &access).await?;
    transaction.commit().await?;
    Ok(())
}

async fn expiration(
    connection: &mut sqlx::SqliteConnection,
    expiry: Expiry,
    existing: Option<i64>,
) -> Result<Option<i64>, AppError> {
    match expiry {
        Expiry::Keep => Ok(existing),
        Expiry::Never => Ok(None),
        Expiry::AfterDays(days) if (1..=3650).contains(&days) => {
            let now: i64 = sqlx::query_scalar("SELECT unixepoch()")
                .fetch_one(connection)
                .await?;
            Ok(Some(now + days * 86400))
        }
        _ => Err(AppError::InvalidRequest(
            "Expiry must be 1–3650 days from now".into(),
        )),
    }
}

fn validate_access(access: KeyAccess) -> Result<KeyAccess, AppError> {
    let KeyAccess::Selected(models) = access else {
        return Ok(KeyAccess::All);
    };
    if models.len() > 200 {
        return Err(AppError::InvalidRequest(
            "At most 200 allowed model IDs per key".into(),
        ));
    }
    let mut normalized = BTreeSet::new();
    for model in models {
        let model = model.trim();
        if model.is_empty() || model.chars().count() > 321 || model.chars().any(char::is_control) {
            return Err(AppError::InvalidRequest(
                "Allowed model IDs must be 1–321 characters without control characters".into(),
            ));
        }
        if let Some(name) = model.strip_prefix("combo/")
            && (name.is_empty()
                || name.len() > 64
                || !name
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.')))
        {
            return Err(AppError::InvalidRequest(
                "Use combo/<name> with a valid combo name".into(),
            ));
        }
        normalized.insert(model.to_owned());
    }
    Ok(KeyAccess::Selected(normalized.into_iter().collect()))
}

async fn write_models(
    connection: &mut sqlx::SqliteConnection,
    id: &str,
    access: &KeyAccess,
) -> Result<(), AppError> {
    if let KeyAccess::Selected(models) = access {
        for model in models {
            sqlx::query("INSERT INTO api_key_models (key_id, model) VALUES (?, ?)")
                .bind(id)
                .bind(model)
                .execute(&mut *connection)
                .await?;
        }
    }
    Ok(())
}

/// Replace a key's secret while keeping its ID, name, scopes, expiry and
/// limits. The old secret stops authenticating when this commits; the new one
/// gets a fresh one-time reveal envelope.
pub async fn regenerate(
    pool: &SqlitePool,
    crypto: &Crypto,
    id: &str,
) -> Result<IssuedKey, AppError> {
    let secret = generate_secret();
    let prefix: String = secret.expose_secret().chars().take(PREFIX_CHARS).collect();
    let key_hash = crypto.gateway_key_fingerprint(secret.expose_secret())?;
    let reveal_envelope = crypto.encrypt_api_key(id, &secret)?;
    let result = sqlx::query(
        "UPDATE api_keys SET key_prefix = ?, key_hash = ?, reveal_envelope = ? WHERE id = ?",
    )
    .bind(prefix)
    .bind(key_hash)
    .bind(reveal_envelope)
    .bind(id)
    .execute(pool)
    .await?;
    if result.rows_affected() == 0 {
        return Err(AppError::NotFound(format!("API key '{id}' was not found")));
    }
    Ok(IssuedKey {
        id: id.to_owned(),
        secret,
    })
}

/// Returns the secret exactly once, clearing the encrypted copy in the same
/// transaction. `Ok(None)` means the key exists but was already revealed.
pub async fn take_reveal(
    pool: &SqlitePool,
    crypto: &Crypto,
    id: &str,
) -> Result<Option<SecretString>, AppError> {
    let mut transaction = pool.begin_with("BEGIN IMMEDIATE").await?;
    let row = sqlx::query("SELECT reveal_envelope FROM api_keys WHERE id = ?")
        .bind(id)
        .fetch_optional(&mut *transaction)
        .await?
        .ok_or_else(|| AppError::NotFound(format!("API key '{id}' was not found")))?;
    let Some(envelope): Option<Vec<u8>> = row.try_get("reveal_envelope")? else {
        transaction.commit().await?;
        return Ok(None);
    };
    let secret = crypto.decrypt_api_key(id, &envelope)?;
    sqlx::query("UPDATE api_keys SET reveal_envelope = NULL WHERE id = ?")
        .bind(id)
        .execute(&mut *transaction)
        .await?;
    transaction.commit().await?;
    Ok(Some(secret))
}

pub async fn delete(pool: &SqlitePool, id: &str) -> Result<(), AppError> {
    let result = sqlx::query("DELETE FROM api_keys WHERE id = ?")
        .bind(id)
        .execute(pool)
        .await?;
    if result.rows_affected() == 0 {
        return Err(AppError::NotFound(format!("API key '{id}' was not found")));
    }
    Ok(())
}

pub async fn any(pool: &SqlitePool) -> Result<bool, AppError> {
    let exists: i64 = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM api_keys)")
        .fetch_one(pool)
        .await?;
    Ok(exists == 1)
}

/// True when `secret` matches a stored fingerprint. Empty and oversized values
/// are rejected without a database lookup.
pub async fn matches(pool: &SqlitePool, crypto: &Crypto, secret: &str) -> Result<bool, AppError> {
    Ok(authenticate(pool, crypto, secret).await?.is_some())
}

/// Snapshot the policy once per request, including expiry and revocation.
pub async fn authenticate(
    pool: &SqlitePool,
    crypto: &Crypto,
    secret: &str,
) -> Result<Option<KeyAccess>, AppError> {
    Ok(authenticate_key(pool, crypto, secret)
        .await?
        .map(|key| key.access))
}

pub async fn authenticate_key(
    pool: &SqlitePool,
    crypto: &Crypto,
    secret: &str,
) -> Result<Option<AuthenticatedKey>, AppError> {
    if secret.is_empty() || secret.len() > 256 {
        return Ok(None);
    }
    let fingerprint = crypto.gateway_key_fingerprint(secret)?;
    let mut transaction = pool.begin().await?;
    let found = sqlx::query("SELECT id, restricted, requests_per_minute, max_concurrent FROM api_keys WHERE key_hash = ? AND (expires_at IS NULL OR expires_at > unixepoch())")
        .bind(fingerprint)
        .fetch_optional(&mut *transaction)
        .await?;
    let access = match found {
        None => None,
        Some(row) => {
            let id: String = row.try_get("id")?;
            let access = if !row.try_get::<bool, _>("restricted")? {
                KeyAccess::All
            } else {
                let models = sqlx::query_scalar(
                    "SELECT model FROM api_key_models WHERE key_id = ? ORDER BY model",
                )
                .bind(&id)
                .fetch_all(&mut *transaction)
                .await?;
                KeyAccess::Selected(models)
            };
            Some(AuthenticatedKey {
                id,
                access,
                limits: KeyLimits {
                    requests_per_minute: row.try_get("requests_per_minute")?,
                    max_concurrent: row.try_get("max_concurrent")?,
                },
            })
        }
    };
    transaction.commit().await?;
    Ok(access)
}

fn validate_name(name: &str) -> Result<String, AppError> {
    let name = name.trim();
    if name.is_empty() {
        return Err(AppError::InvalidRequest(
            "API key name must not be empty".into(),
        ));
    }
    if name.chars().count() > MAX_NAME_CHARS {
        return Err(AppError::InvalidRequest(format!(
            "API key name must be at most {MAX_NAME_CHARS} characters"
        )));
    }
    if name.chars().any(char::is_control) {
        return Err(AppError::InvalidRequest(
            "API key name must not contain control characters".into(),
        ));
    }
    Ok(name.to_owned())
}

fn generate_secret() -> SecretString {
    let mut bytes = Zeroizing::new([0u8; 32]);
    OsRng.fill_bytes(bytes.as_mut());
    SecretString::from(format!("sk-{}", URL_SAFE_NO_PAD.encode(bytes.as_slice())))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crypto::Crypto;
    use base64::engine::general_purpose::STANDARD;
    use secrecy::ExposeSecret;

    async fn pool() -> SqlitePool {
        crate::db::connect("sqlite::memory:")
            .await
            .expect("connect")
    }

    fn crypto() -> Crypto {
        Crypto::from_base64(&SecretString::from(STANDARD.encode([9u8; 32]))).unwrap()
    }

    #[tokio::test]
    async fn invalid_policy_never_creates_a_key_and_scope_deduplication_is_exact() {
        let pool = pool().await;
        for models in [
            vec![String::new()],
            vec!["x".repeat(322)],
            vec!["has\0control".into()],
            vec!["combo/".into()],
            vec!["x".into(); 201],
        ] {
            assert!(
                create_with_policy(
                    &pool,
                    &crypto(),
                    "Bad",
                    KeyAccess::Selected(models),
                    Expiry::Never
                )
                .await
                .is_err()
            );
        }
        assert!(
            create_with_policy(
                &pool,
                &crypto(),
                "Bad",
                KeyAccess::All,
                Expiry::AfterDays(-1)
            )
            .await
            .is_err()
        );
        assert!(!any(&pool).await.unwrap());
        let issued = create_with_policy(
            &pool,
            &crypto(),
            "Good",
            KeyAccess::Selected(vec![
                " foo ".into(),
                "foo".into(),
                "Foo".into(),
                "x".repeat(321),
            ]),
            Expiry::Never,
        )
        .await
        .unwrap();
        let Some(KeyAccess::Selected(models)) =
            authenticate(&pool, &crypto(), issued.secret.expose_secret())
                .await
                .unwrap()
        else {
            panic!("selected policy expected");
        };
        assert_eq!(models.len(), 3);
        assert!(models.contains(&"Foo".to_owned()));
        assert!(models.contains(&"foo".to_owned()));
        assert!(models.contains(&"x".repeat(321)));
        assert!(KeyAccess::Selected(vec!["x".repeat(321)]).allows(&"x".repeat(321)));
        assert!(
            !KeyAccess::Selected(vec!["x".repeat(321)]).allows(&format!("{}y", "x".repeat(321)))
        );
    }

    #[tokio::test]
    async fn create_stores_fingerprint_not_secret_and_reveals_once() {
        let pool = pool().await;
        let crypto = crypto();
        let issued = create(&pool, &crypto, " laptop ").await.expect("create");
        let secret = issued.secret.expose_secret().to_owned();
        assert!(secret.starts_with("sk-"));
        assert!(any(&pool).await.unwrap());
        assert!(matches(&pool, &crypto, &secret).await.unwrap());
        assert!(!matches(&pool, &crypto, "sk-wrong").await.unwrap());

        let stored: String = sqlx::query_scalar("SELECT key_hash FROM api_keys WHERE id = ?")
            .bind(&issued.id)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert!(!stored.contains(&secret));
        let listed = list(&pool).await.unwrap();
        assert!(listed[0].limits.requests_per_minute.is_none());
        assert!(listed[0].limits.max_concurrent.is_none());
        assert_eq!(listed[0].name, "laptop");
        assert_eq!(
            listed[0].prefix,
            secret.chars().take(11).collect::<String>()
        );
        assert_ne!(listed[0].prefix, secret);

        let revealed = take_reveal(&pool, &crypto, &issued.id)
            .await
            .unwrap()
            .expect("first reveal");
        assert_eq!(revealed.expose_secret(), secret);
        assert!(
            take_reveal(&pool, &crypto, &issued.id)
                .await
                .unwrap()
                .is_none()
        );
        let envelope: Option<Vec<u8>> =
            sqlx::query_scalar("SELECT reveal_envelope FROM api_keys WHERE id = ?")
                .bind(&issued.id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert!(envelope.is_none());
        assert!(matches(&pool, &crypto, &secret).await.unwrap());
    }

    #[tokio::test]
    async fn limits_are_persisted_and_invalid_edits_roll_back() {
        let directory = tempfile::tempdir().unwrap();
        let url = format!("sqlite://{}", directory.path().join("limits.db").display());
        let pool = crate::db::connect(&url).await.unwrap();
        let issued = create_with_limits(
            &pool,
            &crypto(),
            "App",
            KeyAccess::All,
            Expiry::Never,
            KeyLimits {
                requests_per_minute: Some(20),
                max_concurrent: Some(2),
            },
        )
        .await
        .unwrap();
        assert!(
            update_policy_with_limits(
                &pool,
                &issued.id,
                KeyAccess::Selected(vec!["combo/other".into()]),
                Expiry::AfterDays(1),
                Some(KeyLimits {
                    requests_per_minute: Some(0),
                    max_concurrent: None
                })
            )
            .await
            .is_err()
        );
        pool.close().await;
        let pool = crate::db::connect(&url).await.unwrap();
        let key = authenticate_key(&pool, &crypto(), issued.secret.expose_secret())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(key.limits.requests_per_minute, Some(20));
        assert_eq!(key.limits.max_concurrent, Some(2));
        assert!(matches!(key.access, KeyAccess::All));
        update_policy(&pool, &issued.id, KeyAccess::All, Expiry::Keep)
            .await
            .unwrap();
        assert_eq!(
            list(&pool).await.unwrap()[0].limits.requests_per_minute,
            Some(20)
        );
        update_policy_with_limits(
            &pool,
            &issued.id,
            KeyAccess::All,
            Expiry::Keep,
            Some(KeyLimits::default()),
        )
        .await
        .unwrap();
        assert!(
            list(&pool).await.unwrap()[0]
                .limits
                .requests_per_minute
                .is_none()
        );
        assert!(
            list(&pool).await.unwrap()[0]
                .limits
                .max_concurrent
                .is_none()
        );
        pool.close().await;
    }

    #[tokio::test]
    async fn regenerate_replaces_the_secret_and_keeps_the_policy() {
        let pool = pool().await;
        let crypto = crypto();
        let issued = create_with_limits(
            &pool,
            &crypto,
            "client",
            KeyAccess::Selected(vec!["combo/a".into()]),
            Expiry::AfterDays(30),
            KeyLimits {
                requests_per_minute: Some(5),
                max_concurrent: Some(1),
            },
        )
        .await
        .unwrap();
        take_reveal(&pool, &crypto, &issued.id).await.unwrap();
        let before = list(&pool).await.unwrap().remove(0);

        let regenerated = regenerate(&pool, &crypto, &issued.id).await.unwrap();
        assert_eq!(regenerated.id, issued.id);
        assert_ne!(
            regenerated.secret.expose_secret(),
            issued.secret.expose_secret()
        );
        assert!(
            !matches(&pool, &crypto, issued.secret.expose_secret())
                .await
                .unwrap()
        );
        let key = authenticate_key(&pool, &crypto, regenerated.secret.expose_secret())
            .await
            .unwrap()
            .expect("new secret authenticates");
        assert_eq!(key.id, issued.id);
        assert!(key.access.allows("combo/a") && !key.access.allows("combo/b"));
        assert_eq!(key.limits.requests_per_minute, Some(5));
        assert_eq!(key.limits.max_concurrent, Some(1));

        let after = list(&pool).await.unwrap().remove(0);
        assert_eq!(after.name, before.name);
        assert_eq!(after.created_at, before.created_at);
        assert_eq!(after.expires_at, before.expires_at);
        assert_ne!(after.prefix, before.prefix);
        assert_eq!(
            after.prefix,
            regenerated
                .secret
                .expose_secret()
                .chars()
                .take(11)
                .collect::<String>()
        );

        // The new secret is revealable exactly once, like a fresh key.
        let revealed = take_reveal(&pool, &crypto, &issued.id)
            .await
            .unwrap()
            .expect("regenerated secret is revealable");
        assert_eq!(revealed.expose_secret(), regenerated.secret.expose_secret());
        assert!(
            take_reveal(&pool, &crypto, &issued.id)
                .await
                .unwrap()
                .is_none()
        );
        assert!(matches!(
            regenerate(&pool, &crypto, "missing").await,
            Err(AppError::NotFound(_))
        ));
    }

    #[tokio::test]
    async fn delete_removes_the_key_and_rejects_blank_names() {
        let pool = pool().await;
        let crypto = crypto();
        assert!(create(&pool, &crypto, " ").await.is_err());
        let issued = create(&pool, &crypto, "client").await.unwrap();
        delete(&pool, &issued.id).await.unwrap();
        assert!(!any(&pool).await.unwrap());
        assert!(
            !matches(&pool, &crypto, issued.secret.expose_secret())
                .await
                .unwrap()
        );
        assert!(matches!(
            delete(&pool, &issued.id).await,
            Err(AppError::NotFound(_))
        ));
    }
}
