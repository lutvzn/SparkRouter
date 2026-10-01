//! Dashboard-generated gateway API keys.
//!
//! The full secret is encrypted only until the one-time reveal page reads it.
//! Authentication uses an HMAC fingerprint and never needs the secret again.

use aes_gcm::aead::{OsRng, rand_core::RngCore};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use secrecy::{ExposeSecret, SecretString};
use sqlx::{Row, SqlitePool};
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
}

/// Newly created key. The secret is not `Debug` and is not written to the list.
pub struct IssuedKey {
    pub id: String,
    pub secret: SecretString,
}

pub async fn list(pool: &SqlitePool) -> Result<Vec<ApiKey>, AppError> {
    let rows = sqlx::query(
        "SELECT id, name, key_prefix, created_at FROM api_keys ORDER BY created_at, id",
    )
    .fetch_all(pool)
    .await?;
    rows.iter()
        .map(|row| {
            Ok(ApiKey {
                id: row.try_get("id")?,
                name: row.try_get("name")?,
                prefix: row.try_get("key_prefix")?,
                created_at: row.try_get("created_at")?,
            })
        })
        .collect()
}

pub async fn create(pool: &SqlitePool, crypto: &Crypto, name: &str) -> Result<IssuedKey, AppError> {
    let name = validate_name(name)?;
    let id = uuid::Uuid::new_v4().to_string();
    let secret = generate_secret();
    let prefix: String = secret.expose_secret().chars().take(PREFIX_CHARS).collect();
    let key_hash = crypto.gateway_key_fingerprint(secret.expose_secret())?;
    let reveal_envelope = crypto.encrypt_api_key(&id, &secret)?;
    sqlx::query(
        "INSERT INTO api_keys (id, name, key_prefix, key_hash, reveal_envelope)
         VALUES (?, ?, ?, ?, ?)",
    )
    .bind(&id)
    .bind(name)
    .bind(prefix)
    .bind(key_hash)
    .bind(reveal_envelope)
    .execute(pool)
    .await?;
    Ok(IssuedKey { id, secret })
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
    if secret.is_empty() || secret.len() > 256 {
        return Ok(false);
    }
    let fingerprint = crypto.gateway_key_fingerprint(secret)?;
    let found: Option<i64> = sqlx::query_scalar("SELECT 1 FROM api_keys WHERE key_hash = ?")
        .bind(fingerprint)
        .fetch_optional(pool)
        .await?;
    Ok(found.is_some())
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
