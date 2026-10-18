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
//! Retention is bounded inline by every insert transaction: after inserting
//! records with their attempts it deletes everything older than the newest
//! configured number of requests (at most [`MAX_RETAINED`], attempts cascade).
//! The gateway's inserts and terminal outcomes arrive in batches from
//! [`super::history_writer`]; [`save`] and [`finish`] write one directly.
//! Finishing a pruned record is a no-op.

use std::collections::HashMap;

use sqlx::{QueryBuilder, Row, Sqlite, SqliteConnection, SqlitePool};

use crate::error::AppError;

/// Default and maximum retained requests; Settings may select a lower cap.
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
const MAX_PUBLIC_MODEL_CHARS: usize = 321;
const MAX_OUTCOME_CHARS: usize = 64;
const MAX_PROVIDER_ID_CHARS: usize = 64;
const MAX_PROVIDER_NAME_CHARS: usize = 128;

/// Rows per multi-row statement, keeping bound parameters (at most 11 per row)
/// far below SQLite's limit. A power of two; see [`statement_chunks`].
const ROWS_PER_STATEMENT: usize = 256;

/// Split rows into chunks whose sizes are powers of two up to
/// [`ROWS_PER_STATEMENT`], so a multi-row statement has one of nine texts and
/// stays prepared in the connection's statement cache. Parsing a new text for
/// every batch cost more than executing it.
fn statement_chunks<T>(mut rows: &[T]) -> impl Iterator<Item = &[T]> {
    std::iter::from_fn(move || {
        if rows.is_empty() {
            return None;
        }
        let size = ROWS_PER_STATEMENT.min(1 << rows.len().ilog2());
        let (chunk, rest) = rows.split_at(size);
        rows = rest;
        Some(chunk)
    })
}

/// One recorded gateway request with its ordered attempt snapshots.
#[derive(Clone)]
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
#[derive(Clone)]
pub struct Attempt {
    pub provider_id: String,
    pub provider_name: String,
    pub model: String,
    pub position: i64,
    pub outcome: String,
    pub latency_ms: i64,
}

#[derive(Clone, Copy, Default)]
pub struct UsageMetrics {
    pub cached_tokens: Option<i64>,
    pub cost_usd: Option<f64>,
}

#[derive(Clone)]
pub struct ProviderSnapshot {
    pub slug: String,
    pub name: String,
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
/// configured retention limit is deleted; attempt rows follow through
/// `ON DELETE CASCADE`.
pub async fn save(pool: &SqlitePool, record: &Record) -> Result<(), AppError> {
    save_with_usage(pool, record, UsageMetrics::default(), &[]).await
}

pub async fn save_with_usage(
    pool: &SqlitePool,
    record: &Record,
    usage: UsageMetrics,
    snapshots: &[Option<ProviderSnapshot>],
) -> Result<(), AppError> {
    save_with_diagnostics(pool, record, usage, snapshots, None).await
}

#[derive(Default, serde::Serialize, serde::Deserialize)]
pub(crate) struct RoutingDiagnostics {
    pub selection: Option<crate::routing::combo::RoutingSelection>,
    pub reasons: Vec<String>,
}

pub(crate) async fn diagnostics(
    pool: &SqlitePool,
    id: &str,
) -> Result<RoutingDiagnostics, AppError> {
    let details: Option<String> =
        sqlx::query_scalar("SELECT details FROM request_routing WHERE request_id = ?")
            .bind(id)
            .fetch_optional(pool)
            .await?;
    Ok(details
        .and_then(|value| serde_json::from_str(&value).ok())
        .unwrap_or_default())
}

pub(crate) async fn save_with_diagnostics(
    pool: &SqlitePool,
    record: &Record,
    usage: UsageMetrics,
    snapshots: &[Option<ProviderSnapshot>],
    diagnostics: Option<&RoutingDiagnostics>,
) -> Result<(), AppError> {
    let request = NewRequest::new(record.clone(), usage, snapshots.to_vec(), diagnostics)?;
    let mut transaction = pool.begin_with("BEGIN IMMEDIATE").await?;
    insert_all(&mut transaction, &[&request]).await?;
    prune(&mut transaction).await?;
    transaction.commit().await?;
    Ok(())
}

/// A validated record ready to insert, with its usage, per-attempt provider
/// snapshots (empty or one per attempt), and serialized routing diagnostics.
pub(crate) struct NewRequest {
    pub record: Record,
    usage: UsageMetrics,
    snapshots: Vec<Option<ProviderSnapshot>>,
    diagnostics: Option<String>,
}

impl NewRequest {
    /// Reject values the schema CHECK constraints would refuse, so a batch
    /// only fails for reasons outside the record itself.
    pub(crate) fn new(
        record: Record,
        usage: UsageMetrics,
        snapshots: Vec<Option<ProviderSnapshot>>,
        diagnostics: Option<&RoutingDiagnostics>,
    ) -> Result<Self, AppError> {
        validate_usage(usage)?;
        if !snapshots.is_empty() && snapshots.len() != record.attempts.len() {
            return Err(AppError::InvalidRequest(
                "provider snapshots must match attempts".into(),
            ));
        }
        for snapshot in snapshots.iter().flatten() {
            if snapshot.slug.is_empty()
                || snapshot.slug.len() > 64
                || !snapshot
                    .slug
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
            {
                return Err(AppError::InvalidRequest("invalid provider snapshot".into()));
            }
            validate(
                "provider group name",
                &snapshot.name,
                MAX_PROVIDER_NAME_CHARS,
            )?;
        }
        validate("source", &record.source, MAX_SOURCE_CHARS)?;
        validate("model", &record.model, MAX_PUBLIC_MODEL_CHARS)?;
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
        let diagnostics = diagnostics
            .map(serde_json::to_string)
            .transpose()
            .map_err(|_| AppError::Routing)?;
        Ok(Self {
            record,
            usage,
            snapshots,
            diagnostics,
        })
    }
}

/// Insert records with their attempts and diagnostics using multi-row
/// statements. Run inside a write transaction, followed by [`prune`].
///
/// `created_at` is assigned by the database; rows of one statement share it
/// and keep their order through `rowid`. Attempts keep their input order:
/// each row gets its `ordinal` from the enumeration of `record.attempts`,
/// while its `position` stays the configured step-position snapshot, so a
/// round-robin or random start records and displays the order actually tried.
pub(crate) async fn insert_all(
    connection: &mut SqliteConnection,
    requests: &[&NewRequest],
) -> Result<(), sqlx::Error> {
    for chunk in statement_chunks(requests) {
        let mut query = QueryBuilder::<Sqlite>::new(
            "INSERT INTO request_history
                 (id, source, model, streaming, outcome, status, latency_ms,
                  input_tokens, output_tokens, cached_tokens, cost_usd) ",
        );
        query.push_values(chunk, |mut row, request| {
            let record = &request.record;
            row.push_bind(&record.id)
                .push_bind(&record.source)
                .push_bind(&record.model)
                .push_bind(record.streaming)
                .push_bind(&record.outcome)
                .push_bind(record.status)
                .push_bind(record.latency_ms)
                .push_bind(record.input_tokens)
                .push_bind(record.output_tokens)
                .push_bind(request.usage.cached_tokens)
                .push_bind(request.usage.cost_usd);
        });
        query.build().execute(&mut *connection).await?;
    }
    let attempts: Vec<_> = requests
        .iter()
        .flat_map(|request| {
            request
                .record
                .attempts
                .iter()
                .enumerate()
                .map(move |(ordinal, attempt)| {
                    let snapshot = request.snapshots.get(ordinal).and_then(Option::as_ref);
                    (&request.record.id, ordinal as i64, attempt, snapshot)
                })
        })
        .collect();
    for chunk in statement_chunks(&attempts) {
        let mut query = QueryBuilder::<Sqlite>::new(
            "INSERT INTO request_attempts
                 (request_id, ordinal, position, provider_id, provider_name,
                  model, outcome, latency_ms, provider_slug, provider_group_name) ",
        );
        query.push_values(chunk, |mut row, (id, ordinal, attempt, snapshot)| {
            row.push_bind(*id)
                .push_bind(*ordinal)
                .push_bind(attempt.position)
                .push_bind(&attempt.provider_id)
                .push_bind(&attempt.provider_name)
                .push_bind(&attempt.model)
                .push_bind(&attempt.outcome)
                .push_bind(attempt.latency_ms)
                .push_bind(snapshot.map(|s| &s.slug))
                .push_bind(snapshot.map(|s| &s.name));
        });
        query.build().execute(&mut *connection).await?;
    }
    let routing: Vec<_> = requests
        .iter()
        .filter_map(|request| Some((&request.record.id, request.diagnostics.as_ref()?)))
        .collect();
    for chunk in statement_chunks(&routing) {
        let mut query =
            QueryBuilder::<Sqlite>::new("INSERT INTO request_routing (request_id, details) ");
        query.push_values(chunk, |mut row, (id, details)| {
            row.push_bind(*id).push_bind(*details);
        });
        query.build().execute(&mut *connection).await?;
    }
    Ok(())
}

/// Delete every request beyond the configured retention, oldest first.
pub(crate) async fn prune(connection: &mut SqliteConnection) -> Result<(), sqlx::Error> {
    // Keep the newest `history_retention` rows by (created_at, rowid), read
    // under the same write lock. Deleting the oldest `count - cap` rows is the
    // same set as skipping the newest `cap`, but walks only the excess instead
    // of every retained row on each insert; COUNT(*) reads index page counts.
    sqlx::query(
        "DELETE FROM request_history
         WHERE id IN (
             SELECT id FROM request_history
             ORDER BY created_at ASC, rowid ASC
             LIMIT max(0, (SELECT COUNT(*) FROM request_history)
                 - (SELECT history_retention FROM dashboard_settings WHERE id = 1))
         )",
    )
    .execute(connection)
    .await?;
    Ok(())
}

/// The final outcome of a committed streaming request.
pub(crate) struct Terminal {
    pub id: String,
    pub outcome: &'static str,
    pub latency_ms: i64,
    pub input_tokens: Option<i64>,
    pub output_tokens: Option<i64>,
    pub usage: UsageMetrics,
}

/// Write terminal outcomes, one statement per chunk; pruned or missing
/// records are skipped. Callers pass validated outcome labels and usage.
pub(crate) async fn finish_all(
    connection: &mut SqliteConnection,
    terminals: &[&Terminal],
) -> Result<(), sqlx::Error> {
    for chunk in statement_chunks(terminals) {
        let mut query = QueryBuilder::<Sqlite>::new(
            "WITH terminal (id, outcome, latency_ms, input_tokens, output_tokens,
                            cached_tokens, cost_usd) AS (",
        );
        query.push_values(chunk, |mut row, terminal| {
            row.push_bind(&terminal.id)
                .push_bind(terminal.outcome)
                .push_bind(terminal.latency_ms)
                .push_bind(terminal.input_tokens)
                .push_bind(terminal.output_tokens)
                .push_bind(terminal.usage.cached_tokens)
                .push_bind(terminal.usage.cost_usd);
        });
        query.push(
            ")
             UPDATE request_history
             SET outcome = terminal.outcome, latency_ms = terminal.latency_ms,
                 input_tokens = terminal.input_tokens, output_tokens = terminal.output_tokens,
                 cached_tokens = terminal.cached_tokens, cost_usd = terminal.cost_usd
             FROM terminal
             WHERE request_history.id = terminal.id",
        );
        query.build().execute(&mut *connection).await?;
    }
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
    finish_with_usage(
        pool,
        id,
        outcome,
        latency_ms,
        input_tokens,
        output_tokens,
        UsageMetrics::default(),
    )
    .await
}

pub async fn finish_with_usage(
    pool: &SqlitePool,
    id: &str,
    outcome: &str,
    latency_ms: i64,
    input_tokens: Option<i64>,
    output_tokens: Option<i64>,
    usage: UsageMetrics,
) -> Result<(), AppError> {
    validate("outcome", outcome, MAX_OUTCOME_CHARS)?;
    validate_usage(usage)?;
    sqlx::query(
        "UPDATE request_history
         SET outcome = ?, latency_ms = ?, input_tokens = ?, output_tokens = ?, cached_tokens = ?, cost_usd = ?
         WHERE id = ?",
    )
    .bind(outcome)
    .bind(latency_ms)
    .bind(input_tokens)
    .bind(output_tokens)
    .bind(usage.cached_tokens)
    .bind(usage.cost_usd)
    .bind(id)
    .execute(pool)
    .await?;
    Ok(())
}

pub(crate) fn validate_usage(usage: UsageMetrics) -> Result<(), AppError> {
    if usage
        .cached_tokens
        .is_some_and(|v| !(0..=1_000_000_000).contains(&v))
        || usage
            .cost_usd
            .is_some_and(|v| !v.is_finite() || !(0.0..=1_000_000.0).contains(&v))
    {
        return Err(AppError::InvalidRequest("invalid usage metrics".into()));
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn record(model: String, attempt_model: String) -> Record {
        Record {
            id: uuid::Uuid::new_v4().to_string(),
            source: "api".into(),
            model,
            streaming: false,
            outcome: "success".into(),
            status: 200,
            latency_ms: 0,
            input_tokens: None,
            output_tokens: None,
            attempts: vec![Attempt {
                provider_id: "provider".into(),
                provider_name: "Provider".into(),
                model: attempt_model,
                position: 0,
                outcome: "success".into(),
                latency_ms: 0,
            }],
        }
    }

    #[test]
    fn statement_chunks_use_power_of_two_sizes_in_order() {
        let sizes = |count: usize| {
            let rows: Vec<usize> = (0..count).collect();
            let chunks: Vec<&[usize]> = statement_chunks(&rows).collect();
            assert_eq!(chunks.concat(), rows);
            chunks.iter().map(|chunk| chunk.len()).collect::<Vec<_>>()
        };
        assert!(sizes(0).is_empty());
        assert_eq!(sizes(1), [1]);
        assert_eq!(sizes(7), [4, 2, 1]);
        assert_eq!(sizes(256), [256]);
        assert_eq!(sizes(600), [256, 256, 64, 16, 8]);
    }

    #[tokio::test]
    async fn insert_keeps_exactly_the_newest_rows_by_time_then_rowid() {
        let pool = crate::db::connect("sqlite::memory:").await.unwrap();
        sqlx::query("UPDATE dashboard_settings SET history_retention = 3 WHERE id = 1")
            .execute(&pool)
            .await
            .unwrap();
        let mut ids = Vec::new();
        for _ in 0..5 {
            let row = record("m".into(), "m".into());
            ids.push(row.id.clone());
            save(&pool, &row).await.unwrap();
            // Identical timestamps fall back to insertion (rowid) order.
            sqlx::query("UPDATE request_history SET created_at = '2026-01-01T00:00:00.000Z'")
                .execute(&pool)
                .await
                .unwrap();
        }
        let kept: Vec<String> = sqlx::query_scalar("SELECT id FROM request_history ORDER BY rowid")
            .fetch_all(&pool)
            .await
            .unwrap();
        assert_eq!(kept, ids[2..]);
        let attempts: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM request_attempts")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(attempts, 3, "pruned rows cascade to their attempts");

        // A raised cap stops pruning; a lowered one is enforced on the next insert.
        sqlx::query("UPDATE dashboard_settings SET history_retention = 10 WHERE id = 1")
            .execute(&pool)
            .await
            .unwrap();
        save(&pool, &record("m".into(), "m".into())).await.unwrap();
        assert_eq!(count(&pool).await.unwrap(), 4);
        sqlx::query("UPDATE dashboard_settings SET history_retention = 1 WHERE id = 1")
            .execute(&pool)
            .await
            .unwrap();
        let newest = record("m".into(), "m".into());
        save(&pool, &newest).await.unwrap();
        let kept: Vec<String> = sqlx::query_scalar("SELECT id FROM request_history")
            .fetch_all(&pool)
            .await
            .unwrap();
        assert_eq!(kept, [newest.id]);
    }

    #[tokio::test]
    async fn public_model_accepts_321_chars_but_attempt_model_stays_limited_to_256() {
        let pool = crate::db::connect("sqlite::memory:").await.unwrap();
        let public_model = "p".repeat(321);
        save(&pool, &record(public_model.clone(), "m".repeat(256)))
            .await
            .unwrap();
        let stored: String = sqlx::query_scalar("SELECT model FROM request_history")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(stored, public_model);
        assert!(
            save(&pool, &record("valid".into(), "m".repeat(257)))
                .await
                .is_err()
        );
    }
}
