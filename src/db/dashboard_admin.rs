//! The stored dashboard password hash (Argon2id PHC string), one row at most.
use crate::error::AppError;
use sqlx::SqlitePool;

pub async fn password_hash(pool: &SqlitePool) -> Result<Option<String>, AppError> {
    Ok(
        sqlx::query_scalar("SELECT password_hash FROM dashboard_admin WHERE id = 1")
            .fetch_optional(pool)
            .await?,
    )
}

/// First-run setup. False when a password was stored first, e.g. by a
/// concurrent setup request.
pub async fn create(pool: &SqlitePool, hash: &str) -> Result<bool, AppError> {
    match sqlx::query("INSERT INTO dashboard_admin (id, password_hash) VALUES (1, ?)")
        .bind(hash)
        .execute(pool)
        .await
    {
        Ok(_) => Ok(true),
        Err(error) if super::is_constraint_violation(&error) => Ok(false),
        Err(error) => Err(error.into()),
    }
}

pub async fn replace(pool: &SqlitePool, hash: &str) -> Result<(), AppError> {
    let result = sqlx::query(
        "UPDATE dashboard_admin SET password_hash = ?, \
         updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now') WHERE id = 1",
    )
    .bind(hash)
    .execute(pool)
    .await?;
    if result.rows_affected() == 0 {
        return Err(AppError::InvalidRequest(
            "no stored dashboard password to change".into(),
        ));
    }
    Ok(())
}

/// Remove the stored password so the next start offers first-run setup again.
/// True when a password was removed.
pub async fn reset(pool: &SqlitePool) -> Result<bool, AppError> {
    Ok(sqlx::query("DELETE FROM dashboard_admin")
        .execute(pool)
        .await?
        .rows_affected()
        > 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn one_password_row_created_once_replaced_and_reset() {
        let pool = crate::db::connect("sqlite::memory:").await.unwrap();
        assert_eq!(password_hash(&pool).await.unwrap(), None);
        assert!(replace(&pool, "$argon2id$v=19$first").await.is_err());
        assert!(create(&pool, "$argon2id$v=19$first").await.unwrap());
        assert!(!create(&pool, "$argon2id$v=19$second").await.unwrap());
        replace(&pool, "$argon2id$v=19$third").await.unwrap();
        assert_eq!(
            password_hash(&pool).await.unwrap().as_deref(),
            Some("$argon2id$v=19$third")
        );
        // Only Argon2id PHC strings are accepted, never a plaintext password.
        assert!(replace(&pool, "plaintext-password").await.is_err());
        assert!(reset(&pool).await.unwrap());
        assert!(!reset(&pool).await.unwrap());
        assert_eq!(password_hash(&pool).await.unwrap(), None);
    }
}
