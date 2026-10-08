//! Opt-in sensitive debugging bodies, retained only alongside their metadata.
use crate::error::AppError;
use sqlx::{FromRow, SqlitePool};

pub const BODY_LIMIT: usize = 64 * 1024;

#[derive(FromRow)]
pub struct Payload {
    pub request_body: String,
    pub response_body: String,
    pub request_truncated: bool,
    pub response_truncated: bool,
    pub response_complete: bool,
}

pub async fn get(pool: &SqlitePool, id: &str) -> Result<Option<Payload>, AppError> {
    Ok(sqlx::query_as("SELECT request_body, response_body, request_truncated, response_truncated, response_complete FROM request_payloads WHERE request_id = ?").bind(id).fetch_optional(pool).await?)
}

pub(crate) async fn insert(
    pool: &SqlitePool,
    id: &str,
    body: &str,
    truncated: bool,
) -> Result<(), AppError> {
    // A concurrent clear/prune must not recreate deleted history.
    sqlx::query("INSERT INTO request_payloads (request_id, request_body, request_truncated) SELECT id, ?, ? FROM request_history WHERE id = ?")
        .bind(body).bind(truncated).bind(id).execute(pool).await?;
    Ok(())
}

pub(crate) async fn finish(
    pool: &SqlitePool,
    id: &str,
    body: &str,
    truncated: bool,
    complete: bool,
) -> Result<(), AppError> {
    sqlx::query("UPDATE request_payloads SET response_body = ?, response_truncated = ?, response_complete = ? WHERE request_id = ?")
        .bind(body).bind(truncated).bind(complete).bind(id).execute(pool).await?;
    Ok(())
}
