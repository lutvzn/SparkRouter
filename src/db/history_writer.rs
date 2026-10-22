//! Group-commit writer for request history.
//!
//! History is written once or twice per gateway request. Committing each write
//! on its own cost about 0.75 ms on the single history connection, mostly
//! per-statement round trips, which capped history far below response
//! throughput. Request handlers instead queue writes without waiting, and one
//! task owns the history connection: it commits everything queued since its
//! previous commit in a single transaction with multi-row statements and one
//! retention prune. There is no timer. An idle writer commits a lone write at
//! once; batches grow only while a commit is already in progress.
//!
//! The queue is FIFO, so a stream's terminal outcome and an Extended response
//! capture are always written after their insert, in the same or a later
//! transaction. Queuing never blocks a response: a full queue, or a full
//! budget for Extended bodies, drops the write with a warning. A started
//! transaction is never cancelled; it waits for the write lock within the
//! busy timeout like any other writer.

use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

use sqlx::{Connection, SqliteConnection, SqlitePool};
use tokio::sync::{mpsc, oneshot};

use super::{
    is_constraint_violation,
    request_history::{self, NewRequest, Terminal},
    request_payloads,
};

/// Writes waiting for the writer at the default buffer size, bounding memory
/// and how stale history can get behind a stalled disk. History writes beyond
/// it are dropped; see [`Op::capped`]. Settings can raise it, with [`PAYLOAD_BUDGET`], by a whole multiple.
const QUEUE_CAPACITY: usize = 8192;

/// Most writes committed by one transaction.
const MAX_BATCH: usize = 1024;

/// Bytes of Extended request/response bodies allowed in the queue at once at
/// the default buffer size; captures beyond it are dropped while their
/// metadata is still recorded.
const PAYLOAD_BUDGET: usize = 32 * 1024 * 1024;

/// Handle for queuing history writes; clones share one writer task.
#[derive(Clone)]
pub(crate) struct HistoryWriter {
    // Unbounded so the limit can change at runtime; `queued` enforces it.
    queue: mpsc::UnboundedSender<Op>,
    queued: Arc<AtomicUsize>,
    payload_bytes: Arc<AtomicUsize>,
    /// Multiple of [`QUEUE_CAPACITY`] and [`PAYLOAD_BUDGET`] in effect.
    scale: Arc<AtomicUsize>,
    batches: Arc<AtomicUsize>,
}

// No Debug: captures can hold private conversation data.
enum Op {
    Insert(Box<NewRequest>, Option<Capture>),
    Finish(Terminal),
    FinishPayload(String, Capture),
    Spend(Spend),
    RememberResponse(RememberedResponse),
    ForgetResponse(String),
    Flush(oneshot::Sender<()>),
}

impl Op {
    /// History and captures are best-effort and count against the queue
    /// limit. Budget usage and stored-response links are small, one per
    /// request, and must survive a stalled disk, so like flushes they are
    /// always queued, and a failed commit holds them for the next batch.
    // shortcut: uncapped writes grow memory (~100 bytes per request) for as
    // long as the disk stalls or commits fail; merge spends per key and
    // period if outages that long become real.
    fn capped(&self) -> bool {
        matches!(self, Op::Insert(..) | Op::Finish(_) | Op::FinishPayload(..))
    }
}

/// The connection that created a stored Responses API response.
struct RememberedResponse {
    id: String,
    provider_id: String,
    scope: String,
    model: String,
}

/// Usage counted against a key's budget period.
struct Spend {
    key: String,
    period: String,
    tokens: i64,
    cost_usd: f64,
}

/// A bounded body capture whose bytes count against [`PAYLOAD_BUDGET`] until
/// it is written or dropped.
struct Capture {
    body: String,
    truncated: bool,
    complete: bool,
    used: Arc<AtomicUsize>,
}

impl Drop for Capture {
    fn drop(&mut self) {
        self.used.fetch_sub(self.body.len(), Ordering::Relaxed);
    }
}

impl HistoryWriter {
    /// Spawn the writer task for `pool`. It ends once every handle is dropped
    /// and the queue is empty.
    pub(crate) fn start(pool: SqlitePool) -> Self {
        let (queue, receiver) = mpsc::unbounded_channel();
        let writer = Self {
            queue,
            queued: Arc::default(),
            payload_bytes: Arc::default(),
            scale: Arc::new(AtomicUsize::new(1)),
            batches: Arc::default(),
        };
        drop(tokio::spawn(run(
            pool,
            receiver,
            writer.queued.clone(),
            writer.batches.clone(),
        )));
        writer
    }

    /// Hold up to `scale` times the default queue and Extended body budget.
    /// Shrinking keeps writes already queued; new ones wait for room.
    pub(crate) fn set_buffer(&self, scale: u32) {
        self.scale.store(scale.max(1) as usize, Ordering::Relaxed);
    }

    /// Batches taken from the queue so far, including flush-only ones.
    #[cfg(test)]
    fn batches(&self) -> usize {
        self.batches.load(Ordering::Relaxed)
    }

    /// Queue a new record with its optional Extended request capture. False
    /// when the record was dropped; the capture alone may be dropped too.
    pub(crate) fn insert(&self, request: NewRequest, request_body: Option<(String, bool)>) -> bool {
        let capture = request_body.and_then(|(body, truncated)| {
            let capture = self.capture(body, truncated, false);
            if capture.is_none() {
                tracing::warn!("request payload write unavailable");
            }
            capture
        });
        let queued = self.enqueue(Op::Insert(Box::new(request), capture));
        if !queued {
            tracing::warn!("request history write unavailable");
        }
        queued
    }

    /// Queue the terminal outcome of a record queued earlier.
    pub(crate) fn finish(&self, terminal: Terminal) {
        if request_history::validate_usage(terminal.usage).is_err()
            || !self.enqueue(Op::Finish(terminal))
        {
            tracing::warn!("request history finalization unavailable");
        }
    }

    /// Queue the Extended response capture of a record queued earlier.
    pub(crate) fn finish_payload(&self, id: String, body: String, truncated: bool, complete: bool) {
        let queued = self
            .capture(body, truncated, complete)
            .is_some_and(|capture| self.enqueue(Op::FinishPayload(id, capture)));
        if !queued {
            tracing::warn!("request payload write unavailable");
        }
    }

    /// Queue usage counted against a key's budget. Written on this
    /// connection, so it never invalidates the configuration cache.
    pub(crate) fn spend(&self, key: String, period: String, tokens: i64, cost_usd: f64) {
        self.submit(Op::Spend(Spend {
            key,
            period,
            tokens,
            cost_usd,
        }));
    }

    /// Queue which connection created a stored response, for later
    /// retrieval and `previous_response_id` continuation.
    pub(crate) fn remember_response(
        &self,
        id: String,
        provider_id: String,
        scope: String,
        model: String,
    ) {
        self.submit(Op::RememberResponse(RememberedResponse {
            id,
            provider_id,
            scope,
            model,
        }));
    }

    /// Queue removal of a stored response the upstream deleted.
    pub(crate) fn forget_response(&self, id: String) {
        self.submit(Op::ForgetResponse(id));
    }

    /// Wait until every write queued before this call is committed, dropped,
    /// or held for retry after a failed commit. Flushes do not count against
    /// the queue limit.
    pub(crate) async fn flush(&self) {
        let (done, wait) = oneshot::channel();
        if self.queue.send(Op::Flush(done)).is_ok() {
            let _ = wait.await;
        }
    }

    /// Queue `op`, or warn that it was dropped.
    fn submit(&self, op: Op) {
        if let Err(op) = self.try_enqueue(op) {
            warn_dropped(&op);
        }
    }

    /// Queue `op` unless the queue already holds its limit of writes.
    fn enqueue(&self, op: Op) -> bool {
        self.try_enqueue(op).is_ok()
    }

    /// [`Self::enqueue`], handing a dropped `op` back.
    fn try_enqueue(&self, op: Op) -> Result<(), Op> {
        let capped = op.capped();
        let limit = QUEUE_CAPACITY * self.scale.load(Ordering::Relaxed);
        if capped && self.queued.fetch_add(1, Ordering::Relaxed) >= limit {
            self.queued.fetch_sub(1, Ordering::Relaxed);
            return Err(op);
        }
        self.queue.send(op).map_err(|error| {
            if capped {
                self.queued.fetch_sub(1, Ordering::Relaxed);
            }
            error.0
        })
    }

    fn capture(&self, body: String, truncated: bool, complete: bool) -> Option<Capture> {
        let bytes = body.len();
        // Reserve optimistically; a concurrent reservation may briefly see
        // the excess and drop its capture too, which only errs toward less.
        let budget = PAYLOAD_BUDGET * self.scale.load(Ordering::Relaxed);
        if self.payload_bytes.fetch_add(bytes, Ordering::Relaxed) + bytes > budget {
            self.payload_bytes.fetch_sub(bytes, Ordering::Relaxed);
            return None;
        }
        Some(Capture {
            body,
            truncated,
            complete,
            used: self.payload_bytes.clone(),
        })
    }
}

async fn run(
    pool: SqlitePool,
    mut receiver: mpsc::UnboundedReceiver<Op>,
    queued: Arc<AtomicUsize>,
    batches: Arc<AtomicUsize>,
) {
    let mut batch = Vec::with_capacity(MAX_BATCH);
    // Uncapped writes from a failed commit, retried ahead of the next batch.
    let mut held = Vec::new();
    while receiver.recv_many(&mut batch, MAX_BATCH).await > 0 {
        // Like a bounded channel, taking writes frees their room in the queue.
        let capped = batch.iter().filter(|op| op.capped()).count();
        queued.fetch_sub(capped, Ordering::Relaxed);
        let (flushes, writes): (Vec<_>, Vec<_>) = held
            .drain(..)
            .chain(batch.drain(..))
            .partition(|op| matches!(op, Op::Flush(_)));
        let refs: Vec<_> = writes.iter().collect();
        let mut hold = vec![false; refs.len()];
        // History is dropped. Budget usage and stored responses are dropped
        // only when the database rejects the row itself.
        let mut failed = |index: usize, op: &Op, error: &sqlx::Error| {
            if op.capped() || is_constraint_violation(error) {
                warn_dropped(op);
            } else {
                hold[index] = true;
            }
        };
        match commit(&pool, &refs).await {
            Ok(()) => {}
            // A row the database rejects must not discard the rest of the
            // batch. Other failures (I/O, lock timeout) would only repeat.
            Err(error) if is_constraint_violation(&error) && refs.len() > 1 => {
                for (index, op) in refs.iter().enumerate() {
                    if let Err(error) = commit(&pool, &[op]).await {
                        failed(index, op, &error);
                    }
                }
            }
            Err(error) => {
                for (index, op) in refs.iter().enumerate() {
                    failed(index, op, &error);
                }
            }
        }
        held = writes
            .into_iter()
            .zip(hold)
            .filter_map(|(op, hold)| hold.then_some(op))
            .collect();
        if !held.is_empty() {
            tracing::warn!(
                writes = held.len(),
                "budget usage or stored response writes held for retry"
            );
        }
        batches.fetch_add(1, Ordering::Relaxed);
        for op in flushes {
            if let Op::Flush(done) = op {
                let _ = done.send(());
            }
        }
    }
}

/// Write `ops` in one transaction. Every terminal outcome or response capture
/// belongs to an insert queued before it, so writing all inserts first keeps
/// that order.
async fn commit(pool: &SqlitePool, ops: &[&Op]) -> Result<(), sqlx::Error> {
    if ops.is_empty() {
        return Ok(());
    }
    let inserts: Vec<_> = ops
        .iter()
        .filter_map(|op| match op {
            Op::Insert(request, _) => Some(&**request),
            _ => None,
        })
        .collect();
    let terminals: Vec<_> = ops
        .iter()
        .filter_map(|op| match op {
            Op::Finish(terminal) => Some(terminal),
            _ => None,
        })
        .collect();
    let mut transaction = pool.begin_with("BEGIN IMMEDIATE").await?;
    request_history::insert_all(&mut transaction, &inserts).await?;
    request_history::finish_all(&mut transaction, &terminals).await?;
    // One statement pair per key and period, however many requests it had.
    let mut spends = std::collections::BTreeMap::<(&str, &str), (i64, f64)>::new();
    for op in ops {
        if let Op::Spend(spend) = op {
            let total = spends.entry((&spend.key, &spend.period)).or_default();
            total.0 = total.0.saturating_add(spend.tokens);
            total.1 += spend.cost_usd;
        }
    }
    for ((key, period), (tokens, cost_usd)) in spends {
        super::api_keys::add_spend(&mut transaction, key, period, tokens, cost_usd).await?;
    }
    let mut remembered = false;
    for op in ops {
        match op {
            Op::RememberResponse(response) => {
                remembered = true;
                super::stored_responses::insert(
                    &mut transaction,
                    &response.id,
                    &response.provider_id,
                    &response.scope,
                    &response.model,
                )
                .await?;
            }
            Op::ForgetResponse(id) => super::stored_responses::delete(&mut transaction, id).await?,
            _ => {}
        }
    }
    if remembered {
        super::stored_responses::prune(&mut transaction).await?;
    }
    let captures = ops
        .iter()
        .filter(|op| matches!(op, Op::Insert(_, Some(_)) | Op::FinishPayload(..)))
        .count();
    if captures > 0 {
        // Bodies are best-effort: a failed capture rolls back to this
        // savepoint and never discards the metadata around it.
        let mut savepoint = transaction.begin().await?;
        if write_captures(&mut savepoint, ops).await.is_ok() {
            savepoint.commit().await?;
        } else {
            savepoint.rollback().await?;
            for _ in 0..captures {
                tracing::warn!("request payload write unavailable");
            }
        }
    }
    if !inserts.is_empty() {
        request_history::prune(&mut transaction).await?;
    }
    transaction.commit().await
}

async fn write_captures(connection: &mut SqliteConnection, ops: &[&Op]) -> Result<(), sqlx::Error> {
    let requests: Vec<_> = ops
        .iter()
        .filter_map(|op| match op {
            Op::Insert(request, Some(capture)) => Some((
                request.record.id.as_str(),
                capture.body.as_str(),
                capture.truncated,
            )),
            _ => None,
        })
        .collect();
    let responses: Vec<_> = ops
        .iter()
        .filter_map(|op| match op {
            Op::FinishPayload(id, capture) => Some((
                id.as_str(),
                capture.body.as_str(),
                capture.truncated,
                capture.complete,
            )),
            _ => None,
        })
        .collect();
    request_payloads::insert_all(connection, &requests).await?;
    request_payloads::finish_all(connection, &responses).await
}

fn warn_dropped(op: &Op) {
    match op {
        Op::Insert(..) => tracing::warn!("request history write unavailable"),
        Op::Finish(_) => tracing::warn!("request history finalization unavailable"),
        Op::FinishPayload(..) => tracing::warn!("request payload write unavailable"),
        Op::Spend(_) => tracing::warn!("budget usage write unavailable"),
        Op::RememberResponse(_) | Op::ForgetResponse(_) => {
            tracing::warn!("stored response write unavailable");
        }
        Op::Flush(_) => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::request_history::{Attempt, Record, UsageMetrics};

    fn request(id: &str, streaming: bool) -> NewRequest {
        let attempt = |position| Attempt {
            provider_id: "provider".into(),
            provider_name: "Provider".into(),
            model: "m".into(),
            position,
            outcome: "failed".into(),
            latency_ms: 1,
        };
        let record = Record {
            id: id.into(),
            source: "api".into(),
            model: "combo/coding".into(),
            streaming,
            outcome: if streaming { "streaming" } else { "success" }.into(),
            status: 200,
            latency_ms: 1,
            input_tokens: None,
            output_tokens: None,
            attempts: vec![attempt(1), attempt(0)],
        };
        NewRequest::new(record, UsageMetrics::default(), vec![], None).unwrap()
    }

    fn terminal(id: &str) -> Terminal {
        Terminal {
            id: id.into(),
            outcome: "success",
            latency_ms: 7,
            input_tokens: Some(3),
            output_tokens: Some(4),
            usage: UsageMetrics {
                cached_tokens: Some(1),
                cost_usd: Some(0.5),
            },
        }
    }

    async fn count(pool: &SqlitePool, table: &str) -> i64 {
        sqlx::query_scalar(&format!("SELECT COUNT(*) FROM {table}"))
            .fetch_one(pool)
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn queued_writes_commit_together_and_keep_their_order() {
        let pool = crate::db::connect("sqlite::memory:").await.unwrap();
        let history = HistoryWriter::start(pool.clone());
        // More rows than one multi-row statement holds, with outcomes for
        // streams inserted in the same batch and for a pruned/unknown record.
        let ids: Vec<String> = (0..300).map(|n| format!("req-{n:03}")).collect();
        for (n, id) in ids.iter().enumerate() {
            assert!(history.insert(request(id, n % 2 == 0), None));
        }
        for id in ids.iter().step_by(2) {
            history.finish(terminal(id));
        }
        history.finish(terminal("missing"));
        history.flush().await;
        assert_eq!(history.batches(), 1, "one transaction for the whole queue");

        let stored: Vec<String> =
            sqlx::query_scalar("SELECT id FROM request_history ORDER BY rowid")
                .fetch_all(&pool)
                .await
                .unwrap();
        assert_eq!(stored, ids);
        let ordinals: Vec<(i64, i64)> = sqlx::query_as(
            "SELECT ordinal, position FROM request_attempts WHERE request_id = 'req-299' ORDER BY ordinal",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(ordinals, [(0, 1), (1, 0)]);
        assert_eq!(count(&pool, "request_attempts").await, 600);
        let pending: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM request_history WHERE outcome = 'streaming'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(pending, 0, "every stream outcome landed after its insert");
        let finished: (String, i64, Option<i64>, Option<i64>, Option<f64>) = sqlx::query_as(
            "SELECT outcome, latency_ms, output_tokens, cached_tokens, cost_usd
             FROM request_history WHERE id = 'req-000'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(finished, ("success".into(), 7, Some(4), Some(1), Some(0.5)));
    }

    #[tokio::test]
    async fn retention_prunes_once_per_batch_to_the_newest_rows() {
        let pool = crate::db::connect("sqlite::memory:").await.unwrap();
        sqlx::query("UPDATE dashboard_settings SET history_retention = 5 WHERE id = 1")
            .execute(&pool)
            .await
            .unwrap();
        let history = HistoryWriter::start(pool.clone());
        for n in 0..12 {
            assert!(history.insert(request(&format!("req-{n:02}"), false), None));
        }
        history.flush().await;
        let kept: Vec<String> = sqlx::query_scalar("SELECT id FROM request_history ORDER BY rowid")
            .fetch_all(&pool)
            .await
            .unwrap();
        assert_eq!(kept, ["req-07", "req-08", "req-09", "req-10", "req-11"]);
        assert_eq!(count(&pool, "request_attempts").await, 10);
    }

    #[tokio::test]
    async fn a_rejected_row_does_not_discard_the_rest_of_its_batch() {
        let pool = crate::db::connect("sqlite::memory:").await.unwrap();
        let history = HistoryWriter::start(pool.clone());
        assert!(history.insert(request("duplicate", false), None));
        history.flush().await;
        assert!(history.insert(request("before", false), None));
        assert!(history.insert(request("duplicate", false), None));
        assert!(history.insert(request("after", true), None));
        history.finish(terminal("after"));
        history.flush().await;
        let mut stored: Vec<String> = sqlx::query_scalar("SELECT id FROM request_history")
            .fetch_all(&pool)
            .await
            .unwrap();
        stored.sort();
        assert_eq!(stored, ["after", "before", "duplicate"]);
        let outcome: String =
            sqlx::query_scalar("SELECT outcome FROM request_history WHERE id = 'after'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(outcome, "success");
    }

    #[tokio::test]
    async fn a_full_queue_drops_writes_instead_of_waiting() {
        let pool = crate::db::connect("sqlite::memory:").await.unwrap();
        let history = HistoryWriter::start(pool.clone());
        // The writer cannot start while the only connection is held.
        let connection = pool.acquire().await.unwrap();
        let mut queued = 0;
        while history.insert(request(&format!("req-{queued}"), false), None) {
            queued += 1;
            assert!(queued <= QUEUE_CAPACITY + MAX_BATCH, "queue is unbounded");
        }
        assert!(queued >= QUEUE_CAPACITY);
        drop(connection);
        history.flush().await;
        assert_eq!(count(&pool, "request_history").await, queued as i64);
        assert!(
            history.insert(request("later", false), None),
            "capacity returns"
        );
    }

    async fn key_id(pool: &SqlitePool) -> String {
        use base64::{Engine, engine::general_purpose::STANDARD};
        let crypto = crate::crypto::Crypto::from_base64(&secrecy::SecretString::from(
            STANDARD.encode([7; 32]),
        ))
        .unwrap();
        crate::db::api_keys::create(pool, &crypto, "client")
            .await
            .unwrap()
            .id
    }

    #[tokio::test]
    async fn budget_usage_and_stored_responses_survive_a_full_queue() {
        let pool = crate::db::connect("sqlite::memory:").await.unwrap();
        let key = key_id(&pool).await;
        crate::db::providers::save(
            &pool,
            &crate::db::providers::Provider {
                id: "provider".into(),
                name: "Provider".into(),
                base_url: "http://127.0.0.1:9".into(),
                protocol: crate::providers::Protocol::OpenAi,
                encrypted_api_key: vec![],
                enabled: true,
            },
        )
        .await
        .unwrap();
        let history = HistoryWriter::start(pool.clone());
        let connection = pool.acquire().await.unwrap();
        let mut queued = 0;
        while history.insert(request(&format!("req-{queued}"), false), None) {
            queued += 1;
            assert!(queued <= QUEUE_CAPACITY + MAX_BATCH, "queue is unbounded");
        }
        history.spend(key.clone(), "2026-10-21".into(), 40, 0.25);
        history.remember_response(
            "resp_1".into(),
            "provider".into(),
            "scope".into(),
            "combo/coding".into(),
        );
        drop(connection);
        history.flush().await;
        assert_eq!(
            crate::db::api_keys::spent(&pool, &key, "2026-10-21")
                .await
                .unwrap(),
            (40, 0.25)
        );
        assert!(
            crate::db::stored_responses::get(&pool, "resp_1", "scope")
                .await
                .unwrap()
                .is_some()
        );
        assert_eq!(history.queued.load(Ordering::Relaxed), 0);
    }

    #[tokio::test]
    async fn budget_usage_from_a_failed_commit_is_retried_with_the_next_batch() {
        let pool = crate::db::connect("sqlite::memory:").await.unwrap();
        let key = key_id(&pool).await;
        let history = HistoryWriter::start(pool.clone());
        // A missing table fails the commit without a constraint violation.
        sqlx::query("ALTER TABLE api_key_spend RENAME TO api_key_spend_away")
            .execute(&pool)
            .await
            .unwrap();
        history.spend(key.clone(), "2026-10-21".into(), 40, 0.25);
        assert!(history.insert(request("dropped", false), None));
        history.flush().await;
        sqlx::query("ALTER TABLE api_key_spend_away RENAME TO api_key_spend")
            .execute(&pool)
            .await
            .unwrap();
        history.spend(key.clone(), "2026-10-21".into(), 2, 0.5);
        history.flush().await;
        assert_eq!(
            crate::db::api_keys::spent(&pool, &key, "2026-10-21")
                .await
                .unwrap(),
            (42, 0.75)
        );
        // Best-effort history from the failed commit is not held.
        assert_eq!(count(&pool, "request_history").await, 0);
    }

    #[tokio::test]
    async fn a_larger_buffer_holds_more_and_shrinking_keeps_what_is_queued() {
        let pool = crate::db::connect("sqlite::memory:").await.unwrap();
        let history = HistoryWriter::start(pool.clone());
        history.set_buffer(2);
        let connection = pool.acquire().await.unwrap();
        let mut queued = 0;
        while history.insert(request(&format!("req-{queued}"), false), None) {
            queued += 1;
            assert!(
                queued <= 2 * QUEUE_CAPACITY + MAX_BATCH,
                "queue is unbounded"
            );
        }
        assert!(queued >= 2 * QUEUE_CAPACITY);
        history.set_buffer(1);
        assert!(!history.insert(request("over", false), None));
        drop(connection);
        history.flush().await;
        // Everything queued was written; retention then kept the newest.
        assert_eq!(
            count(&pool, "request_history").await,
            request_history::MAX_RETAINED.min(queued as i64)
        );
        assert_eq!(history.queued.load(Ordering::Relaxed), 0);

        // The Extended body budget scales with it.
        let body = "x".repeat(PAYLOAD_BUDGET + 1);
        assert!(history.capture(body.clone(), false, false).is_none());
        history.set_buffer(2);
        assert!(history.capture(body, false, false).is_some());
        assert_eq!(history.payload_bytes.load(Ordering::Relaxed), 0);
    }

    #[tokio::test]
    async fn extended_bodies_beyond_the_budget_keep_their_metadata() {
        let pool = crate::db::connect("sqlite::memory:").await.unwrap();
        let history = HistoryWriter::start(pool.clone());
        let body = "x".repeat(crate::db::request_payloads::BODY_LIMIT);
        let fit = PAYLOAD_BUDGET / body.len();
        let connection = pool.acquire().await.unwrap();
        for n in 0..fit + 3 {
            let id = format!("req-{n}");
            assert!(history.insert(request(&id, false), Some((body.clone(), false))));
        }
        history.finish_payload("req-0".into(), "late".into(), false, true);
        drop(connection);
        history.flush().await;
        assert_eq!(count(&pool, "request_history").await, (fit + 3) as i64);
        assert_eq!(count(&pool, "request_payloads").await, fit as i64);
        assert_eq!(history.payload_bytes.load(Ordering::Relaxed), 0);
        // Once written, the budget is free for the response capture again.
        history.finish_payload("req-0".into(), "done".into(), false, true);
        history.flush().await;
        let payload = request_payloads::get(&pool, "req-0")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            (payload.response_body.as_str(), payload.response_complete),
            ("done", true)
        );
    }

    #[tokio::test]
    async fn captures_in_one_batch_pair_requests_with_their_responses() {
        let pool = crate::db::connect("sqlite::memory:").await.unwrap();
        let history = HistoryWriter::start(pool.clone());
        // More captures than one multi-row statement holds, with responses
        // queued in the same batch as their requests and for unknown records.
        let connection = pool.acquire().await.unwrap();
        for n in 0..300 {
            let id = format!("req-{n:03}");
            let body = format!("request {n}");
            assert!(history.insert(request(&id, false), Some((body, n == 7))));
            if n % 3 == 0 {
                let body = format!("response {n}");
                history.finish_payload(id, body, n == 9, n != 12);
            }
        }
        history.finish_payload("missing".into(), "orphan".into(), false, true);
        // A repeated response capture replaces the earlier one.
        history.finish_payload("req-000".into(), "again".into(), false, true);
        drop(connection);
        history.flush().await;
        assert_eq!(history.batches(), 1);
        assert_eq!(count(&pool, "request_payloads").await, 300);

        let row = |id: &'static str| {
            let pool = pool.clone();
            async move {
                let payload = request_payloads::get(&pool, id).await.unwrap().unwrap();
                (
                    payload.request_body,
                    payload.request_truncated,
                    payload.response_body,
                    payload.response_truncated,
                    payload.response_complete,
                )
            }
        };
        assert_eq!(
            row("req-007").await,
            ("request 7".into(), true, String::new(), false, false)
        );
        assert_eq!(
            row("req-009").await,
            ("request 9".into(), false, "response 9".into(), true, true)
        );
        assert_eq!(
            row("req-012").await,
            (
                "request 12".into(),
                false,
                "response 12".into(),
                false,
                false
            )
        );
        assert_eq!(
            row("req-299").await,
            ("request 299".into(), false, String::new(), false, false)
        );
        assert_eq!(
            row("req-000").await,
            ("request 0".into(), false, "again".into(), false, true)
        );
        assert_eq!(count(&pool, "request_payload_responses").await, 100);
    }

    #[tokio::test]
    async fn a_failed_capture_keeps_the_metadata_of_its_batch() {
        let pool = crate::db::connect("sqlite::memory:").await.unwrap();
        sqlx::query(
            "CREATE TRIGGER reject_payload BEFORE INSERT ON request_payloads
             BEGIN SELECT RAISE(ABORT, 'payload unavailable'); END",
        )
        .execute(&pool)
        .await
        .unwrap();
        let history = HistoryWriter::start(pool.clone());
        assert!(history.insert(request("plain", true), None));
        assert!(history.insert(request("captured", false), Some(("{}".into(), false))));
        history.finish(terminal("plain"));
        history.flush().await;
        assert_eq!(history.batches(), 1);
        assert_eq!(count(&pool, "request_history").await, 2);
        assert_eq!(count(&pool, "request_payloads").await, 0);
        let outcome: String =
            sqlx::query_scalar("SELECT outcome FROM request_history WHERE id = 'plain'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(outcome, "success");
    }
}
