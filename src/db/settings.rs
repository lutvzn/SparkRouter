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

pub async fn logging_mode<'e>(
    executor: impl sqlx::SqliteExecutor<'e>,
) -> Result<LoggingMode, AppError> {
    let value: String =
        sqlx::query_scalar("SELECT logging_mode FROM dashboard_settings WHERE id = 1")
            .fetch_one(executor)
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

/// Multiples of the history writer's default queue and Extended body budget
/// that Settings offers.
pub const HISTORY_BUFFER_SCALES: [u32; 4] = [1, 2, 4, 8];

pub async fn history_buffer(pool: &SqlitePool) -> Result<u32, AppError> {
    Ok(
        sqlx::query_scalar("SELECT history_buffer FROM dashboard_settings WHERE id = 1")
            .fetch_one(pool)
            .await?,
    )
}

pub async fn set_history_buffer(pool: &SqlitePool, scale: u32) -> Result<(), AppError> {
    if !HISTORY_BUFFER_SCALES.contains(&scale) {
        return Err(AppError::InvalidRequest(
            "history write buffer must be 1, 2, 4, or 8 times the default".into(),
        ));
    }
    sqlx::query("UPDATE dashboard_settings SET history_buffer = ? WHERE id = 1")
        .bind(scale)
        .execute(pool)
        .await?;
    Ok(())
}

/// Whether failing targets cool down (see `routing::cooldown`).
pub async fn target_cooldown(pool: &SqlitePool) -> Result<bool, AppError> {
    Ok(
        sqlx::query_scalar("SELECT target_cooldown FROM dashboard_settings WHERE id = 1")
            .fetch_one(pool)
            .await?,
    )
}

pub async fn set_target_cooldown(pool: &SqlitePool, enabled: bool) -> Result<(), AppError> {
    sqlx::query("UPDATE dashboard_settings SET target_cooldown = ? WHERE id = 1")
        .bind(enabled)
        .execute(pool)
        .await?;
    Ok(())
}

/// Longest direct-route affinity Settings accepts, matching combo timeouts.
pub const MAX_DIRECT_AFFINITY_MINUTES: u32 = 1440;

/// Minutes a direct model ID keeps its last successful connection; 0 is off.
pub async fn direct_affinity_minutes(pool: &SqlitePool) -> Result<u32, AppError> {
    Ok(
        sqlx::query_scalar("SELECT direct_affinity_minutes FROM dashboard_settings WHERE id = 1")
            .fetch_one(pool)
            .await?,
    )
}

pub async fn set_direct_affinity_minutes(pool: &SqlitePool, minutes: u32) -> Result<(), AppError> {
    if minutes > MAX_DIRECT_AFFINITY_MINUTES {
        return Err(AppError::InvalidRequest(
            "direct-route affinity must be between 0 and 1,440 minutes".into(),
        ));
    }
    sqlx::query("UPDATE dashboard_settings SET direct_affinity_minutes = ? WHERE id = 1")
        .bind(minutes)
        .execute(pool)
        .await?;
    Ok(())
}

/// The SHA-256 (hex) of the dashboard-generated metrics token, if any.
pub async fn metrics_token_hash(pool: &SqlitePool) -> Result<Option<String>, AppError> {
    Ok(
        sqlx::query_scalar("SELECT metrics_token_hash FROM dashboard_settings WHERE id = 1")
            .fetch_one(pool)
            .await?,
    )
}

/// Replace (or with `None`, remove) the metrics token's hash.
pub async fn set_metrics_token_hash(pool: &SqlitePool, hash: Option<&str>) -> Result<(), AppError> {
    sqlx::query("UPDATE dashboard_settings SET metrics_token_hash = ? WHERE id = 1")
        .bind(hash)
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
