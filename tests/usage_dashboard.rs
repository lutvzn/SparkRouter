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
        dashboard_password: None,
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

#[tokio::test]
async fn usage_migration_preserves_legacy_rows_without_inventing_metrics() {
    use sqlx::{
        Row,
        sqlite::{SqliteConnectOptions, SqlitePoolOptions},
    };
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(
            SqliteConnectOptions::new()
                .filename(":memory:")
                .foreign_keys(true),
        )
        .await
        .unwrap();
    for migration in [
        include_str!("../migrations/0001_initial.sql"),
        include_str!("../migrations/0002_custom_provider_groups.sql"),
        include_str!("../migrations/0003_combo_strategies.sql"),
        include_str!("../migrations/0004_api_keys.sql"),
        include_str!("../migrations/0005_connection_models.sql"),
        include_str!("../migrations/0006_connection_model_details.sql"),
        include_str!("../migrations/0007_request_history.sql"),
        include_str!("../migrations/0008_scoped_api_keys.sql"),
        include_str!("../migrations/0009_provider_slugs.sql"),
        include_str!("../migrations/0010_public_model_id_lengths.sql"),
    ] {
        sqlx::raw_sql(migration).execute(&pool).await.unwrap();
    }
    sqlx::query("INSERT INTO request_history (id,source,model,outcome,status,input_tokens,output_tokens) VALUES ('old','api','combo/old','success',200,42,7)").execute(&pool).await.unwrap();
    sqlx::query("INSERT INTO request_attempts (request_id,ordinal,position,provider_id,provider_name,model,outcome) VALUES ('old',0,1,'gone','Deleted connection','upstream','selected')").execute(&pool).await.unwrap();
    sqlx::raw_sql(include_str!("../migrations/0011_usage_metrics.sql"))
        .execute(&pool)
        .await
        .unwrap();
    let old = sqlx::query("SELECT cached_tokens,cost_usd FROM request_history WHERE id='old'")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(old.get::<Option<i64>, _>("cached_tokens"), None);
    assert_eq!(old.get::<Option<f64>, _>("cost_usd"), None);
    let summary = usage::overview(&pool).await.unwrap();
    assert_eq!(summary.requests, 1);
    assert_eq!(summary.input_tokens, Some(42));
    assert_eq!(summary.output_tokens, Some(7));
    assert_eq!(summary.cost_usd, None);
    assert_eq!(summary.providers[0].name, "Deleted connection");
    assert_eq!(summary.providers[0].slug, "connection-gone");
    assert_eq!(summary.providers[0].input_tokens, Some(42));
    assert_eq!(summary.providers[0].cost_usd, None);
    let violations = sqlx::query("PRAGMA foreign_key_check")
        .fetch_all(&pool)
        .await
        .unwrap();
    assert!(violations.is_empty());
}
