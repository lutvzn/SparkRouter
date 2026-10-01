//! Metadata-only request history: what each routed request did, never what
//! it contained.
//!
//! A [`Record`] stores the outcome label, HTTP status, latency, and token
//! counts of one gateway request plus [`Attempt`] snapshots of the
//! provider/model identifiers tried, in order. Attempts are snapshots: there
//! are no foreign keys into live configuration, so deleting a provider later
//! keeps its history. No prompts, completions, credentials, endpoint URLs,
//! or upstream error bodies are ever persisted.
//!
//! Retention is bounded inline by [`save`]: one transaction inserts the
//! record with its attempts and then deletes everything older than the
//! newest [`MAX_RETAINED`] requests (their attempts cascade). There is no
//! background worker, and [`finish`] on a pruned record is a no-op.

use std::collections::HashMap;

use sqlx::{Row, SqlitePool};

use crate::error::AppError;

/// Newest requests retained; older ones are deleted as new ones arrive.
pub const MAX_RETAINED: i64 = 10_000;

/// Outcome recorded while a request is still pending (in flight); the
/// recording path uses this label for committed streaming requests.
/// Excluded from stats until the request finishes.
pub const OUTCOME_STREAMING: &str = "streaming";

/// Outcome of a request that completed successfully.
pub const OUTCOME_SUCCESS: &str = "success";

// Length limits mirrored by the schema CHECK constraints so oversize values
// surface as InvalidRequest errors instead of internal server errors.
const MAX_SOURCE_CHARS: usize = 32;
const MAX_MODEL_CHARS: usize = 256;
const MAX_OUTCOME_CHARS: usize = 64;
const MAX_PROVIDER_ID_CHARS: usize = 64;
const MAX_PROVIDER_NAME_CHARS: usize = 128;

/// One recorded gateway request with its ordered attempt snapshots.
pub struct Record {
    pub id: String,
    pub source: String,
    pub model: String,
    pub streaming: bool,
    pub outcome: String,
    pub status: i64,
    pub latency_ms: i64,
    pub input_tokens: Option<i64>,
    pub output_tokens: Option<i64>,
    pub attempts: Vec<Attempt>,
}

/// One provider/model try within a request, as it looked when attempted.
pub struct Attempt {
    pub provider_id: String,
    pub provider_name: String,
    pub model: String,
    pub position: i64,
    pub outcome: String,
    pub latency_ms: i64,
}

/// A stored [`Record`] plus the `created_at` timestamp the database assigned
/// on save; timestamps are never supplied by the caller.
pub struct StoredRecord {
    pub created_at: String,
    pub record: Record,
}

/// Aggregate figures over the retained finished records.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct HistoryStats {
    pub finished: i64,
    pub successes: i64,
    pub failures: i64,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub average_latency_ms: i64,
}

/// Insert a record with its attempts and prune retention, in one transaction.
///
/// `created_at` is assigned by the database. Attempts keep their input order:
/// each row gets its `ordinal` from the enumeration of `record.attempts`,
/// while its `position` stays the configured step-position snapshot, so a
/// round-robin or random start records and displays the order actually
/// tried. After the insert, every request beyond the newest
/// [`MAX_RETAINED`] rows is deleted; attempt rows follow through
/// `ON DELETE CASCADE`.
pub async fn save(pool: &SqlitePool, record: &Record) -> Result<(), AppError> {
    validate("source", &record.source, MAX_SOURCE_CHARS)?;
    validate("model", &record.model, MAX_MODEL_CHARS)?;
    validate("outcome", &record.outcome, MAX_OUTCOME_CHARS)?;
    for attempt in &record.attempts {
        validate(
            "attempt provider id",
            &attempt.provider_id,
            MAX_PROVIDER_ID_CHARS,
        )?;
        validate(
            "attempt provider name",
            &attempt.provider_name,
            MAX_PROVIDER_NAME_CHARS,
        )?;
        validate("attempt model", &attempt.model, MAX_MODEL_CHARS)?;
        validate("attempt outcome", &attempt.outcome, MAX_OUTCOME_CHARS)?;
    }

    let mut transaction = pool.begin_with("BEGIN IMMEDIATE").await?;
    sqlx::query(
        "INSERT INTO request_history
             (id, source, model, streaming, outcome, status, latency_ms,
              input_tokens, output_tokens)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(&record.id)
    .bind(&record.source)
    .bind(&record.model)
    .bind(record.streaming)
    .bind(&record.outcome)
    .bind(record.status)
    .bind(record.latency_ms)
    .bind(record.input_tokens)
    .bind(record.output_tokens)
    .execute(&mut *transaction)
    .await?;
    for (ordinal, attempt) in record.attempts.iter().enumerate() {
        sqlx::query(
            "INSERT INTO request_attempts
                 (request_id, ordinal, position, provider_id, provider_name,
                  model, outcome, latency_ms)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&record.id)
        .bind(ordinal as i64)
        .bind(attempt.position)
        .bind(&attempt.provider_id)
        .bind(&attempt.provider_name)
        .bind(&attempt.model)
        .bind(&attempt.outcome)
        .bind(attempt.latency_ms)
        .execute(&mut *transaction)
        .await?;
    }
    // Retention: delete every request older than the newest MAX_RETAINED.
    sqlx::query(
        "DELETE FROM request_history
         WHERE id IN (
             SELECT id FROM request_history
             ORDER BY created_at DESC, rowid DESC
             LIMIT -1 OFFSET ?
         )",
    )
    .bind(MAX_RETAINED)
    .execute(&mut *transaction)
    .await?;
    transaction.commit().await?;
    Ok(())
}

/// Record the final outcome of a request.
///
/// Finishing a request whose row was pruned (or never existed) is a no-op:
/// a request that fell out of retention must not fail the response path.
pub async fn finish(
    pool: &SqlitePool,
    id: &str,
    outcome: &str,
    latency_ms: i64,
    input_tokens: Option<i64>,
    output_tokens: Option<i64>,
) -> Result<(), AppError> {
    validate("outcome", outcome, MAX_OUTCOME_CHARS)?;
    sqlx::query(
        "UPDATE request_history
         SET outcome = ?, latency_ms = ?, input_tokens = ?, output_tokens = ?
         WHERE id = ?",
    )
    .bind(outcome)
    .bind(latency_ms)
    .bind(input_tokens)
    .bind(output_tokens)
    .bind(id)
    .execute(pool)
    .await?;
    Ok(())
}

/// The `limit` newest records starting at `offset`, newest first, with their
/// attempts attached in recorded order (their `ordinal` sequence).
pub async fn list_page(
    pool: &SqlitePool,
    limit: i64,
    offset: i64,
) -> Result<Vec<StoredRecord>, AppError> {
    // Keep the page and its attempts in one snapshot while other requests
    // insert/prune or the dashboard clears history.
    let mut transaction = pool.begin().await?;
    let rows = sqlx::query(
        "SELECT id, source, model, streaming, outcome, status, latency_ms,
                input_tokens, output_tokens, created_at
         FROM request_history
         ORDER BY created_at DESC, rowid DESC
         LIMIT ? OFFSET ?",
    )
    .bind(limit)
    .bind(offset)
    .fetch_all(&mut *transaction)
    .await?;
    let mut records: Vec<StoredRecord> = Vec::with_capacity(rows.len());
    for row in &rows {
        records.push(StoredRecord {
            created_at: row.try_get("created_at")?,
            record: Record {
                id: row.try_get("id")?,
                source: row.try_get("source")?,
                model: row.try_get("model")?,
                streaming: row.try_get::<bool, _>("streaming")?,
                outcome: row.try_get("outcome")?,
                status: row.try_get("status")?,
                latency_ms: row.try_get("latency_ms")?,
                input_tokens: row.try_get("input_tokens")?,
                output_tokens: row.try_get("output_tokens")?,
                attempts: Vec::new(),
            },
        });
    }

    // The attempts of exactly the listed records, in one query; each group
    // keeps its recorded order (ascending ordinal) within its request.
    let attempt_rows = sqlx::query(
        "SELECT request_id, provider_id, provider_name, model, position,
                outcome, latency_ms
         FROM request_attempts
         WHERE request_id IN (
             SELECT id FROM request_history
             ORDER BY created_at DESC, rowid DESC
             LIMIT ? OFFSET ?
         )
         ORDER BY ordinal",
    )
    .bind(limit)
    .bind(offset)
    .fetch_all(&mut *transaction)
    .await?;
    let mut grouped: HashMap<String, Vec<Attempt>> = HashMap::new();
    for row in &attempt_rows {
        let request_id: String = row.try_get("request_id")?;
        grouped.entry(request_id).or_default().push(Attempt {
            provider_id: row.try_get("provider_id")?,
            provider_name: row.try_get("provider_name")?,
            model: row.try_get("model")?,
            position: row.try_get("position")?,
            outcome: row.try_get("outcome")?,
            latency_ms: row.try_get("latency_ms")?,
        });
    }
    for stored in &mut records {
        stored.record.attempts = grouped.remove(&stored.record.id).unwrap_or_default();
    }
    transaction.commit().await?;
    Ok(records)
}

/// Number of retained requests, finished or not.
pub async fn count(pool: &SqlitePool) -> Result<i64, AppError> {
    Ok(sqlx::query_scalar("SELECT COUNT(*) FROM request_history")
        .fetch_one(pool)
        .await?)
}

/// Totals over the retained finished records; requests still pending with
/// the [`OUTCOME_STREAMING`] outcome are excluded from every figure.
/// Failures are finished requests without a success outcome. The average
/// latency truncates to whole milliseconds.
pub async fn stats(pool: &SqlitePool) -> Result<HistoryStats, AppError> {
    // AVG is decoded through an integer SUM so the aggregate columns are
    // always INTEGER regardless of how SQLite types its AVG result.
    let row = sqlx::query(
        "SELECT COUNT(*) AS finished,
                COALESCE(SUM(outcome = ?), 0) AS successes,
                COALESCE(SUM(input_tokens), 0) AS input_tokens,
                COALESCE(SUM(output_tokens), 0) AS output_tokens,
                COALESCE(SUM(latency_ms), 0) AS total_latency_ms
         FROM request_history
         WHERE outcome <> ?",
    )
    .bind(OUTCOME_SUCCESS)
    .bind(OUTCOME_STREAMING)
    .fetch_one(pool)
    .await?;
    let finished: i64 = row.try_get("finished")?;
    let successes: i64 = row.try_get("successes")?;
    let total_latency_ms: i64 = row.try_get("total_latency_ms")?;
    Ok(HistoryStats {
        finished,
        successes,
        failures: finished - successes,
        input_tokens: row.try_get("input_tokens")?,
        output_tokens: row.try_get("output_tokens")?,
        average_latency_ms: if finished > 0 {
            total_latency_ms / finished
        } else {
            0
        },
    })
}

/// Delete all retained history; attempts cascade.
pub async fn clear(pool: &SqlitePool) -> Result<(), AppError> {
    sqlx::query("DELETE FROM request_history")
        .execute(pool)
        .await?;
    Ok(())
}

/// Reject empty or oversize text before it can hit a CHECK constraint.
fn validate(kind: &str, value: &str, max_chars: usize) -> Result<(), AppError> {
    if value.is_empty() {
        return Err(AppError::InvalidRequest(format!(
            "request history {kind} must not be empty"
        )));
    }
    if value.chars().count() > max_chars {
        return Err(AppError::InvalidRequest(format!(
            "request history {kind} must be at most {max_chars} characters"
        )));
    }
    Ok(())
}
