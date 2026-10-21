//! Usage aggregation and its dashboard home summary.

use axum::{
    Router,
    body::Body,
    http::{Request, StatusCode},
};
use base64::{Engine, engine::general_purpose::STANDARD};
use http_body_util::BodyExt;
use secrecy::SecretString;
use sparkrouter::{
    app::{self, AppState},
    config::Config,
    crypto::Crypto,
    db,
    db::{request_history as history, usage},
};
use tower::ServiceExt;

async fn pool() -> sqlx::SqlitePool {
    db::connect("sqlite::memory:").await.expect("connect")
}

async fn state() -> AppState {
    AppState::from_config(Config {
        database_url: "sqlite::memory:".into(),
        bind: "127.0.0.1:0".parse().unwrap(),
        crypto: Crypto::from_base64(&SecretString::from(STANDARD.encode([23; 32]))).unwrap(),
        api_key: None,
        dashboard_login: sparkrouter::config::DashboardLogin::Disabled,
        dashboard_secure_cookie: false,
    })
    .await
    .unwrap()
}

fn record(id: &str) -> history::Record {
    history::Record {
        id: id.into(),
        source: "api".into(),
        model: "model".into(),
        streaming: true,
        outcome: history::OUTCOME_STREAMING.into(),
        status: 0,
        latency_ms: 0,
        input_tokens: None,
        output_tokens: None,
        attempts: Vec::new(),
    }
}

fn attempt(id: &str, name: &str, outcome: &str, position: i64) -> history::Attempt {
    history::Attempt {
        provider_id: id.into(),
        provider_name: name.into(),
        model: "model".into(),
        position,
        outcome: outcome.into(),
        latency_ms: 10,
    }
}

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

#[tokio::test]
async fn empty_overview_is_unknown_and_home_renders() {
    let pool = pool().await;
    let overview = usage::overview(&pool).await.unwrap();
    assert_eq!(overview.requests, 0);
    assert_eq!(overview.successes, 0);
    assert_eq!(
        (
            overview.input_tokens,
            overview.output_tokens,
            overview.cached_tokens,
            overview.cost_usd
        ),
        (None, None, None, None)
    );
    assert!(overview.providers.is_empty());

    let state = state().await;
    let html = get_page(&app::router(state), "/").await;
    assert!(html.contains("Usage"));
}

#[tokio::test]
async fn overview_shows_connectivity_memory_and_finished_outcomes() {
    let state = state().await;
    for outcome in ["success", "error", "interrupted", "cancelled", "streaming"] {
        let mut row = record(outcome);
        row.outcome = outcome.into();
        history::save(&state.db, &row).await.unwrap();
    }
    let html = get_page(&app::router(state.clone()), "/").await;
    assert!(html.contains("<span>Database status</span><strong>Connected</strong>"));
    assert!(html.contains("<span>Successful requests</span><strong>1</strong>"));
    assert!(html.contains("<span>Errors / failed requests</span><strong>3</strong>"));
    // Total requests would repeat the success and failure cards.
    assert!(!html.contains("<span>Requests</span>"));
    assert!(html.contains("<span>Memory usage</span><strong>"));
    assert!(!html.contains("Usage unavailable"));
    history::clear(&state.db).await.unwrap();
    let html = get_page(&app::router(state), "/").await;
    assert!(html.contains("<span>Successful requests</span><strong>0</strong>"));
    assert!(html.contains("<span>Errors / failed requests</span><strong>0</strong>"));
}

#[tokio::test]
async fn overview_still_renders_when_database_is_unavailable() {
    let state = state().await;
    state.db.close().await;
    let html = get_page(&app::router(state), "/").await;
    assert!(html.contains("<span>Database status</span><strong>Unavailable</strong>"));
    assert!(html.contains("<span>Successful requests</span><strong>—</strong>"));
    assert!(html.contains("<span>Errors / failed requests</span><strong>—</strong>"));
    assert!(html.contains("Usage unavailable"));
    assert!(!html.contains("No requests yet"));
    assert!(!html.contains("pool closed"));
}

#[tokio::test]
async fn overview_history_failure_does_not_invent_zero_counts() {
    let state = state().await;
    sqlx::query("DROP TABLE request_history")
        .execute(&state.db)
        .await
        .unwrap();
    let html = get_page(&app::router(state), "/").await;
    assert!(html.contains("<span>Database status</span><strong>Connected</strong>"));
    assert!(html.contains("Usage unavailable"));
    assert!(html.contains("<span>Successful requests</span><strong>—</strong>"));
    assert!(!html.contains("no such table"));
}

#[tokio::test]
async fn usage_counts_finished_requests_once_and_uses_selected_provider_snapshots() {
    let pool = pool().await;
    // Two attempts at the same provider are still one request for that provider;
    // only the selected attempt receives usage.
    let mut first = record("one");
    first.attempts = vec![
        attempt("conn-a", "Old Alpha", "error", 0),
        attempt("conn-b", "Beta", "selected", 1),
    ];
    history::save_with_usage(
        &pool,
        &first,
        history::UsageMetrics::default(),
        &[
            Some(history::ProviderSnapshot {
                slug: "alpha".into(),
                name: "Alpha <group>".into(),
            }),
            Some(history::ProviderSnapshot {
                slug: "beta".into(),
                name: "Beta".into(),
            }),
        ],
    )
    .await
    .unwrap();
    history::finish_with_usage(
        &pool,
        "one",
        "success",
        25,
        Some(8),
        Some(3),
        history::UsageMetrics {
            cached_tokens: Some(2),
            cost_usd: Some(0.5),
        },
    )
    .await
    .unwrap();

    let mut second = record("two");
    second.attempts = vec![attempt("conn-c", "Gamma", "error", 0)];
    history::save_with_usage(
        &pool,
        &second,
        history::UsageMetrics::default(),
        &[Some(history::ProviderSnapshot {
            slug: "gamma".into(),
            name: "Gamma".into(),
        })],
    )
    .await
    .unwrap();
    history::finish_with_usage(
        &pool,
        "two",
        "error",
        30,
        None,
        None,
        history::UsageMetrics::default(),
    )
    .await
    .unwrap();

    let mut pending = record("pending");
    pending.attempts = vec![attempt("conn-a", "Alpha", "selected", 0)];
    history::save_with_usage(
        &pool,
        &pending,
        history::UsageMetrics {
            cached_tokens: Some(100),
            cost_usd: Some(10.0),
        },
        &[Some(history::ProviderSnapshot {
            slug: "alpha".into(),
            name: "Alpha".into(),
        })],
    )
    .await
    .unwrap();

    let summary = usage::overview(&pool).await.unwrap();
    assert_eq!((summary.requests, summary.successes), (2, 1));
    assert_eq!(
        (
            summary.input_tokens,
            summary.output_tokens,
            summary.cached_tokens
        ),
        (Some(8), Some(3), Some(2))
    );
    assert_eq!(summary.cost_usd, Some(0.5));
    assert_eq!(
        (
            summary.input_known,
            summary.output_known,
            summary.cache_known,
            summary.cost_known
        ),
        (1, 1, 1, 1)
    );
    let beta = summary.providers.iter().find(|p| p.slug == "beta").unwrap();
    assert_eq!(
        (
            beta.requests,
            beta.input_tokens,
            beta.cached_tokens,
            beta.cost_usd
        ),
        (1, Some(8), Some(2), Some(0.5))
    );
    assert_eq!((beta.usage_requests, beta.cost_known), (1, 1));
    let alpha = summary
        .providers
        .iter()
        .find(|p| p.slug == "alpha")
        .unwrap();
    assert_eq!(
        (alpha.requests, alpha.input_tokens, alpha.cost_usd),
        (1, None, None)
    );
    assert!(
        summary
            .providers
            .iter()
            .any(|p| p.slug == "gamma" && p.requests == 1)
    );

    let state = state().await;
    // Exercise actual server-rendered dashboard escaping and usage fields using this app's DB.
    history::save_with_usage(
        &state.db,
        &first,
        history::UsageMetrics::default(),
        &[
            Some(history::ProviderSnapshot {
                slug: "alpha".into(),
                name: "Alpha <group>".into(),
            }),
            Some(history::ProviderSnapshot {
                slug: "beta".into(),
                name: "Beta".into(),
            }),
        ],
    )
    .await
    .unwrap();
    history::finish_with_usage(
        &state.db,
        "one",
        "success",
        25,
        Some(8),
        Some(3),
        history::UsageMetrics {
            cached_tokens: Some(2),
            cost_usd: Some(0.5),
        },
    )
    .await
    .unwrap();
    let html = get_page(&app::router(state), "/").await;
    assert!(html.contains("Alpha &#60;group&#62;"));
    assert!(!html.contains("Alpha <group>"));
}

#[tokio::test]
async fn invalid_usage_and_snapshot_mismatch_are_rejected_without_writes() {
    let pool = pool().await;
    let mut r = record("bad");
    r.attempts.push(attempt("c", "Provider", "selected", 0));
    assert!(
        history::save_with_usage(&pool, &r, history::UsageMetrics::default(), &[None, None])
            .await
            .is_err()
    );
    assert!(
        history::save_with_usage(
            &pool,
            &r,
            history::UsageMetrics {
                cached_tokens: Some(-1),
                cost_usd: None
            },
            &[None]
        )
        .await
        .is_err()
    );
    assert!(
        history::save_with_usage(
            &pool,
            &r,
            history::UsageMetrics {
                cached_tokens: None,
                cost_usd: Some(f64::NAN)
            },
            &[None]
        )
        .await
        .is_err()
    );
    assert_eq!(history::count(&pool).await.unwrap(), 0);
}

#[tokio::test]
async fn same_provider_retries_are_deduplicated_and_explicit_zero_is_known() {
    let pool = pool().await;
    let mut r = record("same-provider");
    r.outcome = "success".into();
    r.input_tokens = Some(100);
    r.output_tokens = Some(20);
    r.attempts = vec![
        attempt("key-one", "First key", "retryable_failure", 1),
        attempt("key-two", "Second key", "selected", 2),
    ];
    let snapshot = history::ProviderSnapshot {
        slug: "shared".into(),
        name: "Shared provider".into(),
    };
    history::save_with_usage(
        &pool,
        &r,
        history::UsageMetrics {
            cached_tokens: Some(80),
            cost_usd: Some(0.012),
        },
        &[Some(snapshot.clone()), Some(snapshot.clone())],
    )
    .await
    .unwrap();
    let mut zero = record("reported-zero");
    zero.outcome = "success".into();
    zero.input_tokens = Some(0);
    zero.output_tokens = Some(0);
    zero.attempts = vec![attempt("key-two", "Second key", "selected", 1)];
    history::save_with_usage(
        &pool,
        &zero,
        history::UsageMetrics {
            cached_tokens: Some(0),
            cost_usd: Some(0.0),
        },
        &[Some(snapshot)],
    )
    .await
    .unwrap();
    let mut unknown = record("unknown");
    unknown.outcome = "interrupted".into();
    unknown.attempts = vec![attempt("old-key", "Legacy provider", "selected", 1)];
    history::save(&pool, &unknown).await.unwrap();
    let summary = usage::overview(&pool).await.unwrap();
    assert_eq!(summary.requests, 3);
    assert_eq!(
        (
            summary.input_known,
            summary.output_known,
            summary.cache_known,
            summary.cost_known
        ),
        (2, 2, 2, 2)
    );
    assert_eq!(
        (
            summary.input_tokens,
            summary.output_tokens,
            summary.cached_tokens,
            summary.cost_usd
        ),
        (Some(100), Some(20), Some(80), Some(0.012))
    );
    assert_eq!(summary.providers[0].slug, "shared");
    assert_eq!(
        summary.providers[0].requests, 2,
        "a retry must not inflate request counts"
    );
    assert_eq!(
        summary.providers[0].input_tokens,
        Some(100),
        "a retry must not multiply usage"
    );
    assert_eq!(summary.providers[0].cost_usd, Some(0.012));
    assert_eq!(summary.providers[1].slug, "connection-old-key");
    assert_eq!(summary.providers[1].input_tokens, None);
    assert_eq!(summary.providers[1].cost_usd, None);
    history::clear(&pool).await.unwrap();
    let empty = usage::overview(&pool).await.unwrap();
    assert_eq!(empty.requests, 0);
    assert_eq!(empty.cached_tokens, None);
    assert_eq!(empty.cost_usd, None);
    assert!(empty.providers.is_empty());
}
