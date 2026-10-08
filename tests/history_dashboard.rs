//! Request-history persistence and its dashboard page: transactional saves,
//! retention pruning with cascading attempts, stats, escaping, pagination,
//! and the CSRF/origin guards around clearing.

use axum::{
    Router,
    body::Body,
    http::{Request, StatusCode},
    response::Response,
};
use base64::{Engine, engine::general_purpose::STANDARD};
use http_body_util::BodyExt;
use secrecy::SecretString;
use sparkrouter::{
    app::{self, AppState},
    config::Config,
    crypto::Crypto,
    db,
    db::request_history as history,
    error::AppError,
};
use tower::ServiceExt;

async fn state() -> AppState {
    AppState::from_config(Config {
        database_url: "sqlite::memory:".into(),
        bind: "127.0.0.1:0".parse().unwrap(),
        crypto: Crypto::from_base64(&SecretString::from(STANDARD.encode([23; 32]))).unwrap(),
        api_key: None,
        dashboard_password: None,
        dashboard_secure_cookie: false,
    })
    .await
    .unwrap()
}

async fn pool() -> sqlx::SqlitePool {
    db::connect("sqlite::memory:").await.expect("connect")
}

fn record(id: &str, model: &str) -> history::Record {
    history::Record {
        id: id.to_owned(),
        source: "api".into(),
        model: model.to_owned(),
        streaming: false,
        outcome: history::OUTCOME_STREAMING.into(),
        status: 0,
        latency_ms: 0,
        input_tokens: None,
        output_tokens: None,
        attempts: Vec::new(),
    }
}

fn attempt(
    provider_id: &str,
    provider_name: &str,
    position: i64,
    outcome: &str,
) -> history::Attempt {
    history::Attempt {
        provider_id: provider_id.to_owned(),
        provider_name: provider_name.to_owned(),
        model: "snapshot-model".into(),
        position,
        outcome: outcome.to_owned(),
        latency_ms: 40 + position,
    }
}

/// GETs a dashboard page, asserting it renders successfully, and returns the HTML.
async fn get_page(router: &Router, path: &str) -> String {
    let response = router
        .clone()
        .oneshot(Request::get(path).body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK, "GET {path} must render");
    String::from_utf8(
        response
            .into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes()
            .to_vec(),
    )
    .unwrap()
}

/// POSTs a CSRF form body, optionally with cross-origin annotations.
async fn post_form(
    router: &Router,
    path: &str,
    csrf: &str,
    origin: Option<&str>,
    fetch_site: Option<&str>,
) -> Response {
    let mut request = Request::post(path)
        .header("content-type", "application/x-www-form-urlencoded")
        .header("host", "localhost");
    if let Some(origin) = origin {
        request = request.header("origin", origin);
    }
    if let Some(fetch_site) = fetch_site {
        request = request.header("sec-fetch-site", fetch_site);
    }
    router
        .clone()
        .oneshot(request.body(Body::from(format!("_csrf={csrf}"))).unwrap())
        .await
        .unwrap()
}

#[tokio::test]
async fn save_stores_attempts_in_order_and_finish_updates_the_record() {
    let pool = pool().await;
    let mut saved = record("req-1", "gpt-4o-mini");
    saved.streaming = true;
    saved.status = 200;
    saved.attempts = vec![
        attempt("provider-a", "Alpha", 0, "error"),
        attempt("provider-b", "Beta", 1, "success"),
    ];
    history::save(&pool, &saved).await.expect("save");

    let stored = history::list_page(&pool, 10, 0).await.expect("list");
    assert_eq!(stored.len(), 1);
    assert_eq!(stored[0].record.id, "req-1");
    assert_eq!(stored[0].record.source, "api");
    assert_eq!(stored[0].record.model, "gpt-4o-mini");
    assert!(stored[0].record.streaming);
    assert_eq!(stored[0].record.outcome, history::OUTCOME_STREAMING);
    assert!(
        !stored[0].created_at.is_empty(),
        "created_at is a database timestamp"
    );
    let attempts = &stored[0].record.attempts;
    assert_eq!(attempts.len(), 2);
    assert_eq!(
        (
            attempts[0].position,
            attempts[0].provider_name.as_str(),
            attempts[0].outcome.as_str(),
            attempts[0].latency_ms
        ),
        (0, "Alpha", "error", 40)
    );
    assert_eq!(
        (
            attempts[1].position,
            attempts[1].provider_name.as_str(),
            attempts[1].outcome.as_str()
        ),
        (1, "Beta", "success")
    );

    history::finish(&pool, "req-1", "success", 460, Some(12), Some(34))
        .await
        .expect("finish");
    let stored = history::list_page(&pool, 10, 0).await.expect("list");
    assert_eq!(stored[0].record.outcome, "success");
    assert_eq!(stored[0].record.latency_ms, 460);
    assert_eq!(stored[0].record.input_tokens, Some(12));
    assert_eq!(stored[0].record.output_tokens, Some(34));
    assert_eq!(
        stored[0].record.attempts.len(),
        2,
        "attempts survive finish"
    );

    // Finishing an unknown or pruned id is a no-op, never an error.
    history::finish(&pool, "missing", "success", 1, None, None)
        .await
        .expect("finish of an unknown id is a no-op");
}

#[tokio::test]
async fn attempts_keep_recorded_order_independent_of_configured_positions() {
    let pool = pool().await;
    let mut saved = record("req-rotate", "gpt-4o-mini");
    // A round-robin or random start records steps out of configured order:
    // the step at configured position 3 is tried first, then 1, then 2.
    saved.attempts = vec![
        attempt("provider-c", "Gamma", 3, "error"),
        attempt("provider-a", "Alpha", 1, "error"),
        attempt("provider-b", "Beta", 2, "success"),
    ];
    history::save(&pool, &saved).await.expect("save");

    let stored = history::list_page(&pool, 10, 0).await.expect("list");
    let attempts = &stored[0].record.attempts;
    let positions: Vec<i64> = attempts.iter().map(|attempt| attempt.position).collect();
    assert_eq!(
        positions,
        [3, 1, 2],
        "the recorded sequence wins over the configured positions"
    );
    assert_eq!(attempts[0].provider_name, "Gamma");
    assert_eq!(attempts[2].provider_name, "Beta");
    assert_eq!(attempts[2].outcome, "success");
}

#[tokio::test]
async fn save_is_transactional_and_rejects_oversize_values() {
    let pool = pool().await;
    // Failing the second attempt insert rolls back the whole save: no record
    // row may survive without its attempts.
    sqlx::query(
        "CREATE TRIGGER reject_second_attempt BEFORE INSERT ON request_attempts
         WHEN NEW.ordinal = 1 BEGIN SELECT RAISE(ABORT, 'test insert failure'); END",
    )
    .execute(&pool)
    .await
    .expect("failure trigger");
    let mut failing = record("rollback", "m");
    failing.attempts = vec![
        attempt("p1", "One", 0, "error"),
        attempt("p2", "Two", 1, "error"),
    ];
    assert!(history::save(&pool, &failing).await.is_err());
    assert_eq!(
        history::count(&pool).await.unwrap(),
        0,
        "the record row rolls back with its attempts"
    );
    let attempts: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM request_attempts")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(attempts, 0, "a failed save stores no attempts");

    // Oversize or empty metadata fails validation before any write.
    let mut long_model = record("long-model", &"m".repeat(322));
    long_model.attempts = vec![attempt("p", "One", 0, "success")];
    assert!(matches!(
        history::save(&pool, &long_model).await,
        Err(AppError::InvalidRequest(_))
    ));
    let mut long_source = record("long-source", "m");
    long_source.source = "s".repeat(33);
    assert!(matches!(
        history::save(&pool, &long_source).await,
        Err(AppError::InvalidRequest(_))
    ));
    let mut long_outcome = record("long-outcome", "m");
    long_outcome.outcome = "o".repeat(65);
    assert!(matches!(
        history::save(&pool, &long_outcome).await,
        Err(AppError::InvalidRequest(_))
    ));
    let mut long_attempt_name = record("long-attempt-name", "m");
    long_attempt_name.attempts = vec![attempt("p", &"n".repeat(129), 0, "success")];
    assert!(matches!(
        history::save(&pool, &long_attempt_name).await,
        Err(AppError::InvalidRequest(_))
    ));
    let mut blank_model = record("blank-model", "");
    blank_model.source = "api".into();
    assert!(matches!(
        history::save(&pool, &blank_model).await,
        Err(AppError::InvalidRequest(_))
    ));
    assert!(
        matches!(
            history::finish(&pool, "x", &"o".repeat(65), 1, None, None).await,
            Err(AppError::InvalidRequest(_))
        ),
        "finish validates its outcome too"
    );
    assert_eq!(
        history::count(&pool).await.unwrap(),
        0,
        "rejected saves write nothing"
    );
}

#[tokio::test]
async fn save_prunes_to_the_newest_10000_and_cascades_attempts() {
    let pool = pool().await;
    // Seed exactly the cap in one bulk insert (ids seed-1..seed-10000, oldest
    // first), with an attempt on the oldest row to observe the cascade.
    sqlx::raw_sql(
        "INSERT INTO request_history (id, source, model, streaming, outcome, status, latency_ms)
         WITH RECURSIVE seq(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM seq WHERE i < 10000)
         SELECT 'seed-' || i, 'api', 'seed-model', 0, 'success', 200, 5 FROM seq;",
    )
    .execute(&pool)
    .await
    .expect("seed history");
    sqlx::query(
        "INSERT INTO request_attempts
             (request_id, ordinal, position, provider_id, provider_name, model,
              outcome, latency_ms)
         VALUES ('seed-1', 0, 0, 'provider-a', 'Alpha', 'seed-model', 'success', 9)",
    )
    .execute(&pool)
    .await
    .expect("seed attempt");
    assert_eq!(history::count(&pool).await.unwrap(), 10_000);

    history::save(&pool, &record("fresh-1", "fresh-model"))
        .await
        .expect("save");

    assert_eq!(
        history::count(&pool).await.unwrap(),
        10_000,
        "retention stays at the cap"
    );
    let oldest_left: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM request_history WHERE id = 'seed-1'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(oldest_left, 0, "the oldest request is the one pruned");
    let attempts: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM request_attempts")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(attempts, 0, "pruned requests delete their attempts");
    let newest = history::list_page(&pool, 2, 0).await.unwrap();
    assert_eq!(newest[0].record.id, "fresh-1");
    assert_eq!(newest[1].record.id, "seed-10000");
    // Finishing a pruned request is a no-op, not an error.
    history::finish(&pool, "seed-1", "success", 1, None, None)
        .await
        .expect("finish after prune");
}

#[tokio::test]
async fn stats_cover_retained_finished_requests_only() {
    let pool = pool().await;
    // Still pending with the streaming outcome: excluded from every figure,
    // including the average latency.
    history::save(&pool, &record("pending-1", "m"))
        .await
        .unwrap();
    for (id, outcome, latency, input, output) in [
        ("ok-1", "success", 100, Some(10), Some(20)),
        ("ok-2", "success", 300, Some(5), Some(5)),
        ("bad-1", "error", 200, None, None),
    ] {
        history::save(&pool, &record(id, "m")).await.unwrap();
        history::finish(&pool, id, outcome, latency, input, output)
            .await
            .unwrap();
    }

    let stats = history::stats(&pool).await.unwrap();
    assert_eq!(stats.finished, 3);
    assert_eq!(stats.successes, 2);
    assert_eq!(stats.failures, 1);
    assert_eq!(stats.input_tokens, 15);
    assert_eq!(stats.output_tokens, 25);
    assert_eq!(stats.average_latency_ms, 200);
    // The pending request is retained, just not counted as finished.
    assert_eq!(history::count(&pool).await.unwrap(), 4);

    // An empty history yields all-zero stats rather than NULLs.
    history::clear(&pool).await.unwrap();
    assert_eq!(
        history::stats(&pool).await.unwrap(),
        history::HistoryStats::default()
    );
}

#[tokio::test]
async fn clear_removes_records_and_their_attempts() {
    let pool = pool().await;
    let mut saved = record("gone-1", "m");
    saved.attempts = vec![attempt("provider-a", "Alpha", 0, "success")];
    history::save(&pool, &saved).await.unwrap();
    history::clear(&pool).await.unwrap();
    assert_eq!(history::count(&pool).await.unwrap(), 0);
    let attempts: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM request_attempts")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(attempts, 0, "attempts cascade when history is cleared");
}

#[tokio::test]
async fn history_page_escapes_metadata_and_inlines_attempts() {
    let state = state().await;
    let router = app::router(state.clone());
    let mut saved = record("req-1", "<script>alert(1)</script>");
    saved.attempts = vec![
        attempt("provider-a", "Evil <&> Co", 0, "error"),
        attempt("provider-b", "Beta \"quotes\"", 1, "success"),
    ];
    history::save(&state.db, &saved).await.unwrap();
    history::finish(&state.db, "req-1", "success", 90, Some(3), Some(4))
        .await
        .unwrap();

    let html = get_page(&router, "/history").await;
    // User-controlled strings are HTML-escaped by the template.
    assert!(html.contains("&#60;script&#62;alert(1)&#60;/script&#62;"));
    assert!(html.contains("Evil &#60;&#38;&#62; Co"));
    assert!(html.contains("Beta &#34;quotes&#34;"));
    assert!(!html.contains("<script>alert(1)"));
    assert!(!html.contains("Evil <&>"));
    // Attempts render inline on the same page via a details disclosure...
    assert!(html.contains("<details>"));
    assert!(html.contains("2 attempts"));
    // A per-request inspection page exposes opt-in Extended bodies.
    assert!(html.contains("/history/req-1/payload"));
    // The page states Basic/Extended semantics and bounded retention up front.
    assert!(html.contains("Basic stores metadata only"));
    assert!(html.contains("10000"));
    assert!(html.contains("Retained requests"));
    // Stats cover the finished request.
    assert!(html.contains("Reported tokens in / out"));
}

#[tokio::test]
async fn history_page_paginates_and_caps_out_of_range_pages() {
    let state = state().await;
    let router = app::router(state.clone());
    for number in 1..=120 {
        history::save(
            &state.db,
            &record(&format!("req-{number:03}"), &format!("model-{number:03}")),
        )
        .await
        .unwrap();
    }

    let first = get_page(&router, "/history").await;
    assert_eq!(
        first.matches("<tr").count(),
        51,
        "page one shows 50 rows plus the header row"
    );
    assert!(first.contains("Page 1 of 3"));
    assert!(
        first.find("model-120").unwrap() < first.find("model-071").unwrap(),
        "rows are newest first"
    );
    assert!(!first.contains("model-070"), "later rows stay on page two");

    let third = get_page(&router, "/history?page=3").await;
    assert_eq!(
        third.matches("<tr").count(),
        21,
        "the last page shows its 20 remaining rows"
    );
    assert!(third.contains("Page 3 of 3"));
    assert!(third.contains("model-001"));
    assert!(!third.contains("model-021"));

    for path in [
        "/history?page=0",
        "/history?page=-7",
        "/history?page=abc",
        "/history?page=",
    ] {
        let html = get_page(&router, path).await;
        assert!(html.contains("Page 1 of 3"), "{path} falls back to page 1");
        assert!(!html.contains("model-070"), "{path} must not show page two");
    }
    let capped = get_page(&router, "/history?page=99").await;
    assert!(
        capped.contains("Page 3 of 3"),
        "pages past the end are capped to the last page"
    );
}

#[tokio::test]
async fn clearing_history_requires_csrf_and_a_same_origin_submission() {
    let state = state().await;
    let router = app::router(state.clone());
    history::save(&state.db, &record("keep-1", "m"))
        .await
        .unwrap();
    history::finish(&state.db, "keep-1", "success", 5, Some(1), Some(1))
        .await
        .unwrap();

    // An invalid CSRF token is rejected and keeps the history.
    assert_eq!(
        post_form(
            &router,
            "/history/clear",
            "definitely-not-the-token",
            None,
            None
        )
        .await
        .status(),
        StatusCode::BAD_REQUEST
    );
    assert_eq!(history::count(&state.db).await.unwrap(), 1);

    // Cross-origin submissions are rejected even with a valid CSRF token.
    for (origin, fetch_site) in [
        (Some("https://attacker.example"), None),
        (None, Some("cross-site")),
    ] {
        let response = post_form(
            &router,
            "/history/clear",
            state.csrf_token.as_str(),
            origin,
            fetch_site,
        )
        .await;
        assert_eq!(
            response.status(),
            StatusCode::BAD_REQUEST,
            "cross-origin clear must be rejected"
        );
    }
    assert_eq!(
        history::count(&state.db).await.unwrap(),
        1,
        "rejected clears delete nothing"
    );

    // A same-origin submission with the real token clears and redirects.
    let response = post_form(
        &router,
        "/history/clear",
        state.csrf_token.as_str(),
        None,
        None,
    )
    .await;
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    assert_eq!(
        response
            .headers()
            .get("location")
            .and_then(|value| value.to_str().ok()),
        Some("/history")
    );
    assert_eq!(history::count(&state.db).await.unwrap(), 0);
    let html = get_page(&router, "/history").await;
    assert!(html.contains("No requests recorded yet."));
    assert!(
        html.contains("Basic stores metadata only"),
        "the empty page keeps its note"
    );
}
