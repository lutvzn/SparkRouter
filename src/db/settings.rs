//! Persisted dashboard preferences; no deployment secrets.
use crate::error::AppError;
use sqlx::SqlitePool;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum LoggingMode {
    Off,
    Basic,
    Extended,
}

impl LoggingMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Basic => "basic",
            Self::Extended => "extended",
        }
    }
    pub fn parse(value: &str) -> Result<Self, AppError> {
        match value {
            "off" => Ok(Self::Off),
            "basic" => Ok(Self::Basic),
            "extended" => Ok(Self::Extended),
            _ => Err(AppError::InvalidRequest(
                "invalid request logging mode".into(),
            )),
        }
    }
}

pub async fn logging_mode(pool: &SqlitePool) -> Result<LoggingMode, AppError> {
    let value: String =
        sqlx::query_scalar("SELECT logging_mode FROM dashboard_settings WHERE id = 1")
            .fetch_one(pool)
            .await?;
    LoggingMode::parse(&value)
}

pub async fn set_logging_mode(pool: &SqlitePool, mode: LoggingMode) -> Result<(), AppError> {
    sqlx::query("UPDATE dashboard_settings SET logging_mode = ? WHERE id = 1")
        .bind(mode.as_str())
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn history_retention(pool: &SqlitePool) -> Result<i64, AppError> {
    Ok(
        sqlx::query_scalar("SELECT history_retention FROM dashboard_settings WHERE id = 1")
            .fetch_one(pool)
            .await?,
    )
}

/// Changing the cap and pruning existing history serialize with recording.
pub async fn set_history_retention(pool: &SqlitePool, limit: i64) -> Result<(), AppError> {
    if !(1..=super::request_history::MAX_RETAINED).contains(&limit) {
        return Err(AppError::InvalidRequest(
            "history retention must be between 1 and 10,000 requests".into(),
        ));
    }
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    sqlx::query("UPDATE dashboard_settings SET history_retention = ? WHERE id = 1")
        .bind(limit)
        .execute(&mut *tx)
        .await?;
    sqlx::query("DELETE FROM request_history WHERE id IN (SELECT id FROM request_history ORDER BY created_at DESC, rowid DESC LIMIT -1 OFFSET ?)")
        .bind(limit).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(())
}
