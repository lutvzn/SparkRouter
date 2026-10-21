//! Master-key verification and rotation. Rotation re-encrypts every stored
//! credential in one transaction; it runs offline (`sparkrouter
//! rotate-master-key`) because a running server keeps using the key it was
//! started with.
use sqlx::{Row, SqlitePool};
use zeroize::Zeroizing;

use crate::{crypto::Crypto, error::AppError};

const CHECK_CONTEXT: &[u8] = b"sparkrouter.master-key-check.v1";
const CHECK_VALUE: &[u8] = b"sparkrouter master key";
const FINGERPRINT_CONTEXT: &[u8] = b"sparkrouter.fingerprint-key.v1";

const WRONG_KEY: &str = "ROUTER_MASTER_KEY does not match this database; \
    after a rotation, start with the new key";

async fn material(pool: &SqlitePool, purpose: &str) -> Result<Option<Vec<u8>>, AppError> {
    Ok(
        sqlx::query_scalar("SELECT envelope FROM master_key_material WHERE purpose = ?")
            .bind(purpose)
            .fetch_optional(pool)
            .await?,
    )
}

/// Envelopes sealed with the master key: (context, row id, ciphertext).
async fn envelopes(
    connection: &mut sqlx::SqliteConnection,
) -> Result<Vec<(&'static str, String, Vec<u8>)>, AppError> {
    let mut found = Vec::new();
    for row in sqlx::query(
        "SELECT id, encrypted_api_key FROM providers WHERE length(encrypted_api_key) > 0",
    )
    .fetch_all(&mut *connection)
    .await?
    {
        found.push((
            "provider",
            row.try_get("id")?,
            row.try_get("encrypted_api_key")?,
        ));
    }
    for row in
        sqlx::query("SELECT id, reveal_envelope FROM api_keys WHERE reveal_envelope IS NOT NULL")
            .fetch_all(&mut *connection)
            .await?
    {
        found.push((
            "api_key",
            row.try_get("id")?,
            row.try_get("reveal_envelope")?,
        ));
    }
    Ok(found)
}

/// Fail unless `crypto` holds the master key this database was sealed with.
///
/// The first start after this check was introduced records it, unless stored
/// credentials exist and none of them decrypt, which means the key is wrong.
async fn verify(pool: &SqlitePool, crypto: &Crypto) -> Result<(), AppError> {
    if let Some(check) = material(pool, "check").await? {
        return if crypto
            .open(CHECK_CONTEXT, &check)
            .is_ok_and(|value| value.as_slice() == CHECK_VALUE)
        {
            Ok(())
        } else {
            Err(AppError::Configuration(WRONG_KEY.into()))
        };
    }
    let mut connection = pool.acquire().await?;
    let stored = envelopes(&mut connection).await?;
    if !stored.is_empty()
        && !stored
            .iter()
            .any(|(_, id, envelope)| crypto.open(id.as_bytes(), envelope).is_ok())
    {
        return Err(AppError::Configuration(WRONG_KEY.into()));
    }
    sqlx::query(
        "INSERT OR IGNORE INTO master_key_material (purpose, envelope) VALUES ('check', ?)",
    )
    .bind(crypto.seal(CHECK_CONTEXT, CHECK_VALUE)?)
    .execute(&mut *connection)
    .await?;
    Ok(())
}

/// The fingerprint key: sealed in the database after a rotation, otherwise
/// the master key itself.
async fn fingerprint_key(
    pool: &SqlitePool,
    crypto: &Crypto,
) -> Result<Zeroizing<Vec<u8>>, AppError> {
    match material(pool, "fingerprint").await? {
        Some(envelope) => crypto
            .open(FINGERPRINT_CONTEXT, &envelope)
            .map_err(|_| AppError::Configuration(WRONG_KEY.into())),
        None => Ok(Zeroizing::new(crypto.master_key().to_vec())),
    }
}

/// Verify the master key against this database and load its fingerprint key.
pub async fn load(pool: &SqlitePool, crypto: &mut Crypto) -> Result<(), AppError> {
    verify(pool, crypto).await?;
    let key = fingerprint_key(pool, crypto).await?;
    crypto.set_fingerprint_key(&key)
}

pub struct Rotation {
    pub provider_credentials: usize,
    pub unrevealed_api_keys: usize,
}

/// Re-encrypt every credential from `current` to `new` in one transaction.
/// Any envelope that `current` cannot decrypt aborts the whole rotation, so a
/// partially rotated database is never committed.
pub async fn rotate(
    pool: &SqlitePool,
    current: &Crypto,
    new: &Crypto,
) -> Result<Rotation, AppError> {
    if current.same_master_key(new) {
        return Err(AppError::Configuration(
            "ROUTER_NEW_MASTER_KEY must differ from ROUTER_MASTER_KEY".into(),
        ));
    }
    verify(pool, current).await?;
    let fingerprint_key = fingerprint_key(pool, current).await?;
    let mut transaction = pool.begin_with("BEGIN IMMEDIATE").await?;
    let mut rotation = Rotation {
        provider_credentials: 0,
        unrevealed_api_keys: 0,
    };
    for (kind, id, envelope) in envelopes(&mut transaction).await? {
        let plain = current.open(id.as_bytes(), &envelope).map_err(|_| {
            AppError::Configuration(format!(
                "stored {} {id} does not decrypt with ROUTER_MASTER_KEY; nothing was changed",
                kind.replace('_', " ")
            ))
        })?;
        let sealed = new.seal(id.as_bytes(), &plain)?;
        if kind == "provider" {
            sqlx::query("UPDATE providers SET encrypted_api_key = ? WHERE id = ?")
                .bind(sealed)
                .bind(&id)
                .execute(&mut *transaction)
                .await?;
            rotation.provider_credentials += 1;
        } else {
            sqlx::query("UPDATE api_keys SET reveal_envelope = ? WHERE id = ?")
                .bind(sealed)
                .bind(&id)
                .execute(&mut *transaction)
                .await?;
            rotation.unrevealed_api_keys += 1;
        }
    }
    for (purpose, sealed) in [
        ("check", new.seal(CHECK_CONTEXT, CHECK_VALUE)?),
        (
            "fingerprint",
            new.seal(FINGERPRINT_CONTEXT, &fingerprint_key)?,
        ),
    ] {
        sqlx::query(
            "INSERT INTO master_key_material (purpose, envelope) VALUES (?, ?) \
             ON CONFLICT(purpose) DO UPDATE SET envelope = excluded.envelope",
        )
        .bind(purpose)
        .bind(sealed)
        .execute(&mut *transaction)
        .await?;
    }
    transaction.commit().await?;
    Ok(rotation)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{api_keys, providers};
    use base64::{Engine, engine::general_purpose::STANDARD};
    use secrecy::{ExposeSecret, SecretString};

    fn crypto(byte: u8) -> Crypto {
        Crypto::from_base64(&SecretString::from(STANDARD.encode([byte; 32]))).unwrap()
    }

    async fn provider(pool: &SqlitePool, crypto: &Crypto, id: &str, key: Option<&str>) {
        providers::save(
            pool,
            &providers::Provider {
                id: id.into(),
                name: id.into(),
                base_url: "https://api.example.test/v1".into(),
                protocol: crate::providers::Protocol::OpenAi,
                encrypted_api_key: key.map_or_else(Vec::new, |key| {
                    crypto
                        .encrypt_api_key(id, &SecretString::from(key))
                        .unwrap()
                }),
                enabled: true,
            },
        )
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn rotation_reencrypts_credentials_and_keeps_gateway_keys_valid() {
        let pool = crate::db::connect("sqlite::memory:").await.unwrap();
        let mut old = crypto(1);
        load(&pool, &mut old).await.unwrap();
        provider(&pool, &old, "secret", Some("sk-provider")).await;
        provider(&pool, &old, "local", None).await;
        let issued = api_keys::create(&pool, &old, "client").await.unwrap();
        let unrevealed = api_keys::create(&pool, &old, "pending").await.unwrap();
        api_keys::take_reveal(&pool, &old, &issued.id)
            .await
            .unwrap();

        let rotation = rotate(&pool, &old, &crypto(2)).await.unwrap();
        assert_eq!(rotation.provider_credentials, 1);
        assert_eq!(rotation.unrevealed_api_keys, 1);

        // The old key no longer starts the server; the new one does.
        assert!(matches!(
            load(&pool, &mut crypto(1)).await,
            Err(AppError::Configuration(_))
        ));
        let mut new = crypto(2);
        load(&pool, &mut new).await.unwrap();
        let stored = providers::get(&pool, "secret").await.unwrap();
        assert_eq!(
            new.decrypt_api_key("secret", &stored.encrypted_api_key)
                .unwrap()
                .expose_secret(),
            "sk-provider"
        );
        assert!(
            old.decrypt_api_key("secret", &stored.encrypted_api_key)
                .is_err()
        );
        // An unauthenticated local provider keeps its empty credential.
        assert!(
            providers::get(&pool, "local")
                .await
                .unwrap()
                .encrypted_api_key
                .is_empty()
        );
        // Fingerprints made with the old key still authenticate both keys.
        for key in [&issued, &unrevealed] {
            assert!(
                api_keys::matches(&pool, &new, key.secret.expose_secret())
                    .await
                    .unwrap(),
            );
        }
        assert_eq!(
            api_keys::take_reveal(&pool, &new, &unrevealed.id)
                .await
                .unwrap()
                .map(|secret| secret.expose_secret().to_owned()),
            Some(unrevealed.secret.expose_secret().to_owned())
        );

        // A second rotation carries the original fingerprint key forward.
        rotate(&pool, &new, &crypto(3)).await.unwrap();
        let mut third = crypto(3);
        load(&pool, &mut third).await.unwrap();
        assert!(
            api_keys::matches(&pool, &third, issued.secret.expose_secret())
                .await
                .unwrap()
        );
    }

    #[tokio::test]
    async fn rotation_refuses_wrong_or_unchanged_keys_and_undecryptable_rows() {
        let pool = crate::db::connect("sqlite::memory:").await.unwrap();
        let old = crypto(1);
        provider(&pool, &old, "first", Some("sk-first")).await;
        // An unrecorded database is checked against its credentials.
        assert!(matches!(
            load(&pool, &mut crypto(9)).await,
            Err(AppError::Configuration(_))
        ));
        assert!(rotate(&pool, &crypto(9), &crypto(2)).await.is_err());
        assert!(rotate(&pool, &old, &crypto(1)).await.is_err());
        // One credential sealed with another key aborts the whole rotation.
        provider(&pool, &crypto(5), "foreign", Some("sk-foreign")).await;
        let error = rotate(&pool, &old, &crypto(2)).await.err().unwrap();
        assert!(error.to_string().contains("provider foreign"));
        assert!(!error.to_string().contains("sk-"));
        let first = providers::get(&pool, "first").await.unwrap();
        assert!(
            old.decrypt_api_key("first", &first.encrypted_api_key)
                .is_ok()
        );
        let mut still_old = crypto(1);
        load(&pool, &mut still_old).await.unwrap();
    }

    #[tokio::test]
    async fn an_empty_database_records_the_first_key_it_starts_with() {
        let pool = crate::db::connect("sqlite::memory:").await.unwrap();
        load(&pool, &mut crypto(1)).await.unwrap();
        load(&pool, &mut crypto(1)).await.unwrap();
        assert!(load(&pool, &mut crypto(2)).await.is_err());
    }
}
