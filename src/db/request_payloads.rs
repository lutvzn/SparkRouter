//! Opt-in sensitive debugging bodies, retained only alongside their metadata.
use super::request_history::statement_chunks;
use crate::error::AppError;
use sqlx::{FromRow, QueryBuilder, Sqlite, SqliteConnection, SqlitePool};

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
    // A response not captured yet reads as empty and incomplete.
    Ok(sqlx::query_as(
        "SELECT request.request_body, request.request_truncated,
                COALESCE(response.response_body, '') AS response_body,
                COALESCE(response.response_truncated, 0) AS response_truncated,
                COALESCE(response.response_complete, 0) AS response_complete
         FROM request_payloads AS request
         LEFT JOIN request_payload_responses AS response USING (request_id)
         WHERE request.request_id = ?",
    )
    .bind(id)
    .fetch_optional(pool)
    .await?)
}

/// A request capture: record ID, body, and whether the body was truncated.
pub(crate) type RequestCapture<'a> = (&'a str, &'a str, bool);

/// A response capture: record ID, body, truncated, and whether the response
/// finished normally.
pub(crate) type ResponseCapture<'a> = (&'a str, &'a str, bool, bool);

/// Store request captures, one statement per chunk. Captures whose record is
/// gone are skipped: a concurrent clear/prune must not recreate deleted history.
pub(crate) async fn insert_all(
    connection: &mut SqliteConnection,
    captures: &[RequestCapture<'_>],
) -> Result<(), sqlx::Error> {
    for chunk in statement_chunks(captures) {
        let mut query = QueryBuilder::<Sqlite>::new("WITH capture (id, body, truncated) AS (");
        query.push_values(chunk, |mut row, &(id, body, truncated)| {
            row.push_bind(id).push_bind(body).push_bind(truncated);
        });
        query.push(
            ")
             INSERT INTO request_payloads (request_id, request_body, request_truncated)
             SELECT request_history.id, capture.body, capture.truncated
             FROM capture JOIN request_history ON request_history.id = capture.id",
        );
        query.build().execute(&mut *connection).await?;
    }
    Ok(())
}

/// Store response captures in their own rows, so the request body is never
/// rewritten, one statement per chunk. Captures without a stored request
/// capture are skipped; a repeated capture replaces the earlier one.
pub(crate) async fn finish_all(
    connection: &mut SqliteConnection,
    captures: &[ResponseCapture<'_>],
) -> Result<(), sqlx::Error> {
    for chunk in statement_chunks(captures) {
        let mut query =
            QueryBuilder::<Sqlite>::new("WITH capture (id, body, truncated, complete) AS (");
        query.push_values(chunk, |mut row, &(id, body, truncated, complete)| {
            row.push_bind(id)
                .push_bind(body)
                .push_bind(truncated)
                .push_bind(complete);
        });
        query.push(
            ")
             INSERT OR REPLACE INTO request_payload_responses
                 (request_id, response_body, response_truncated, response_complete)
             SELECT request_payloads.request_id, capture.body, capture.truncated,
                    capture.complete
             FROM capture JOIN request_payloads ON request_payloads.request_id = capture.id",
        );
        query.build().execute(&mut *connection).await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::request_history::{self, Record};
    use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
    use std::str::FromStr;

    #[tokio::test]
    async fn splitting_responses_keeps_existing_captures() {
        let options = SqliteConnectOptions::from_str("sqlite::memory:")
            .unwrap()
            .foreign_keys(true);
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(options)
            .await
            .unwrap();
        let mut before = sqlx::migrate!();
        before.migrations = before
            .migrations
            .iter()
            .filter(|migration| migration.version < 22)
            .cloned()
            .collect::<Vec<_>>()
            .into();
        before.run(&pool).await.unwrap();
        for id in ["answered", "pending"] {
            let record = Record {
                id: id.into(),
                source: "api".into(),
                model: "combo/a".into(),
                streaming: false,
                outcome: "success".into(),
                status: 200,
                latency_ms: 1,
                input_tokens: None,
                output_tokens: None,
                attempts: vec![],
            };
            request_history::save(&pool, &record).await.unwrap();
        }
        sqlx::query(
            "INSERT INTO request_payloads (request_id, request_body, request_truncated,
                 response_body, response_truncated, response_complete)
             VALUES ('answered', 'asked', 1, 'answer', 1, 1), ('pending', 'waiting', 0, '', 0, 0)",
        )
        .execute(&pool)
        .await
        .unwrap();

        sqlx::migrate!().run(&pool).await.unwrap();
        let answered = get(&pool, "answered").await.unwrap().unwrap();
        assert_eq!(
            (
                answered.request_body.as_str(),
                answered.request_truncated,
                answered.response_body.as_str(),
                answered.response_truncated,
                answered.response_complete,
            ),
            ("asked", true, "answer", true, true)
        );
        let pending = get(&pool, "pending").await.unwrap().unwrap();
        assert_eq!(
            (
                pending.request_body.as_str(),
                pending.response_body.as_str(),
                pending.response_complete,
            ),
            ("waiting", "", false)
        );
        let responses: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM request_payload_responses")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(responses, 1, "an empty pending response is not copied");

        // Responses still go when their history does.
        sqlx::query("DELETE FROM request_history")
            .execute(&pool)
            .await
            .unwrap();
        let responses: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM request_payload_responses")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(responses, 0);
    }
}
