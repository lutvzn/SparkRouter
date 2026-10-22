//! Per-key token and USD budgets end to end: refusal with
//! `insufficient_quota` once reached, counting with logging Off, streamed
//! usage, persistence across restarts, and the dashboard form.

mod common;
use common::{json_body, spawn_upstream};

use axum::{
    Json, Router,
    body::Body,
    http::{Request, StatusCode, header},
    response::IntoResponse,
    routing::post,
};
use base64::{Engine, engine::general_purpose::STANDARD};
use secrecy::{ExposeSecret, SecretString};
use serde_json::{Value, json};
use tower::ServiceExt;

use sparkrouter::{
    app::{self, AppState},
    config::{Config, DashboardLogin},
    crypto::Crypto,
    db::{
        api_keys::{self, BudgetPeriod, Expiry, KeyAccess, KeyBudget, KeyLimits},
        combos, providers as provider_db,
        settings::{self, LoggingMode},
    },
    providers::Protocol,
};

async fn test_state(database_url: &str) -> AppState {
    AppState::from_config(Config {
        database_url: database_url.into(),
        bind: "127.0.0.1:0".parse().unwrap(),
        crypto: Crypto::from_base64(&SecretString::from(STANDARD.encode([42u8; 32]))).unwrap(),
        api_key: None,
        dashboard_login: DashboardLogin::Disabled,
        dashboard_secure_cookie: false,
        metrics_token: None,
    })
    .await
    .unwrap()
}

/// Reports 8 prompt + 4 completion tokens and $0.25, streamed or not.
async fn seed(state: &AppState) -> common::MockUpstream {
    let upstream = spawn_upstream(Router::new().route(
        "/chat/completions",
        post(|Json(body): Json<Value>| async move {
            let usage = json!({"prompt_tokens":8, "completion_tokens":4, "cost":0.25});
            if body["stream"] == true {
                let wire = format!(
                    "data: {{\"choices\":[{{\"delta\":{{\"content\":\"hi\"}}}}]}}\n\ndata: {}\n\ndata: [DONE]\n\n",
                    json!({"choices":[], "usage":usage})
                );
                return ([("content-type", "text/event-stream")], wire).into_response();
            }
            Json(json!({"choices":[{"message":{"content":"hi"}}], "usage":usage})).into_response()
        }),
    ))
    .await;
    provider_db::save(
        &state.db,
        &provider_db::Provider {
            id: "provider".into(),
            name: "Mock".into(),
            base_url: upstream.url(),
            protocol: Protocol::OpenAi,
            encrypted_api_key: vec![],
            enabled: true,
        },
    )
    .await
    .unwrap();
    let combo = combos::create(&state.db, "chat").await.unwrap();
    combos::add_step(&state.db, &combo.id, "provider", "inner")
        .await
        .unwrap();
    upstream
}

async fn issue(state: &AppState, budget: KeyBudget) -> api_keys::IssuedKey {
    api_keys::create_with_budget(
        &state.db,
        &state.crypto,
        "Budgeted",
        KeyAccess::All,
        Expiry::Never,
        KeyLimits::default(),
        Some(budget),
    )
    .await
    .unwrap()
}

async fn chat(state: &AppState, key: &str, stream: bool) -> axum::response::Response {
    app::router(state.clone())
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/chat/completions")
                .header("authorization", format!("Bearer {key}"))
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({"model":"combo/chat", "stream":stream,
                        "messages":[{"role":"user","content":"hi"}]})
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap()
}

async fn form(state: &AppState, path: &str, body: String) -> StatusCode {
    app::router(state.clone())
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(path)
                .header("content-type", "application/x-www-form-urlencoded")
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap()
        .status()
}

#[tokio::test]
async fn token_budgets_refuse_once_reached_even_with_logging_off() {
    let directory = tempfile::tempdir().unwrap();
    let url = format!("sqlite://{}", directory.path().join("budget.db").display());
    let state = test_state(&url).await;
    let upstream = seed(&state).await;
    settings::set_logging_mode(&state.db, LoggingMode::Off)
        .await
        .unwrap();
    let key = issue(
        &state,
        KeyBudget {
            period: BudgetPeriod::Day,
            tokens: Some(20),
            cost_usd: None,
        },
    )
    .await;
    let secret = key.secret.expose_secret();
    for _ in 0..2 {
        assert_eq!(chat(&state, secret, false).await.status(), StatusCode::OK);
    }
    let refused = chat(&state, secret, false).await;
    assert_eq!(refused.status(), StatusCode::TOO_MANY_REQUESTS);
    let retry_after: u64 = refused.headers()[header::RETRY_AFTER]
        .to_str()
        .unwrap()
        .parse()
        .unwrap();
    assert!((1..=86_400).contains(&retry_after));
    let body = json_body(refused).await;
    assert_eq!(body["error"]["type"], "insufficient_quota");
    assert_eq!(upstream.hits.count(), 2);
    let recorded: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM request_history")
        .fetch_one(&state.db)
        .await
        .unwrap();
    assert_eq!(recorded, 0, "logging Off records no history");

    // The count survives a restart on the same database.
    state.flush_history().await;
    let (tokens, _) = api_keys::spent(&state.db, &key.id, &BudgetPeriod::Day.label(unix_now()))
        .await
        .unwrap();
    assert_eq!(tokens, 24);
    drop(state);
    let restarted = test_state(&url).await;
    assert_eq!(
        chat(&restarted, secret, false).await.status(),
        StatusCode::TOO_MANY_REQUESTS
    );
    assert_eq!(upstream.hits.count(), 2);

    // Removing the budget from the dashboard reopens the key.
    let page = app::router(restarted.clone())
        .oneshot(Request::get("/api-keys").body(Body::empty()).unwrap())
        .await
        .unwrap();
    let page = String::from_utf8(common::body_bytes(page).await.to_vec()).unwrap();
    assert!(page.contains("20 tokens per UTC day · used this period: 24 tokens"));
    assert_eq!(
        form(
            &restarted,
            &format!("/api-keys/{}/policy", key.id),
            format!(
                "_csrf={}&access=all&expiry=keep&budget_period=none",
                restarted.csrf_token
            ),
        )
        .await,
        StatusCode::SEE_OTHER
    );
    assert_eq!(
        chat(&restarted, secret, false).await.status(),
        StatusCode::OK
    );
}

fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}

#[tokio::test]
async fn usd_budgets_count_streamed_usage() {
    let state = test_state("sqlite::memory:").await;
    let upstream = seed(&state).await;
    let key = issue(
        &state,
        KeyBudget {
            period: BudgetPeriod::Month,
            tokens: None,
            cost_usd: Some(0.4),
        },
    )
    .await;
    let secret = key.secret.expose_secret();
    for _ in 0..2 {
        let response = chat(&state, secret, true).await;
        assert_eq!(response.status(), StatusCode::OK);
        // The charge lands when the stream body ends.
        common::body_bytes(response).await;
    }
    let refused = chat(&state, secret, true).await;
    assert_eq!(refused.status(), StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(upstream.hits.count(), 2);
    // Other keys are unaffected.
    let other = issue(
        &state,
        KeyBudget {
            period: BudgetPeriod::Month,
            tokens: None,
            cost_usd: Some(0.4),
        },
    )
    .await;
    assert_eq!(
        chat(&state, other.secret.expose_secret(), false)
            .await
            .status(),
        StatusCode::OK
    );
}

#[tokio::test]
async fn dashboard_budget_forms_validate_and_save() {
    let state = test_state("sqlite::memory:").await;
    let csrf = state.csrf_token.clone();
    for invalid in [
        "budget_period=day",
        "budget_period=none&token_budget=5",
        "budget_period=day&token_budget=abc",
        "budget_period=day&token_budget=0",
        "budget_period=week&token_budget=5",
        "budget_period=day&cost_budget_usd=-1",
    ] {
        assert_eq!(
            form(
                &state,
                "/api-keys",
                format!("_csrf={csrf}&name=App&{invalid}")
            )
            .await,
            StatusCode::BAD_REQUEST,
            "{invalid}"
        );
    }
    assert!(api_keys::list(&state.db).await.unwrap().is_empty());
    assert_eq!(
        form(
            &state,
            "/api-keys",
            format!(
                "_csrf={csrf}&name=App&budget_period=month&token_budget=1000&cost_budget_usd=2.5"
            ),
        )
        .await,
        StatusCode::SEE_OTHER
    );
    let key = api_keys::list(&state.db).await.unwrap().remove(0);
    assert_eq!(
        key.budget,
        Some(KeyBudget {
            period: BudgetPeriod::Month,
            tokens: Some(1000),
            cost_usd: Some(2.5),
        })
    );
    // A policy edit without budget fields keeps the budget.
    assert_eq!(
        form(
            &state,
            &format!("/api-keys/{}/policy", key.id),
            format!("_csrf={csrf}&access=all&expiry=keep"),
        )
        .await,
        StatusCode::SEE_OTHER
    );
    assert!(api_keys::list(&state.db).await.unwrap()[0].budget.is_some());
    assert_eq!(
        form(
            &state,
            &format!("/api-keys/{}/policy", key.id),
            format!("_csrf={csrf}&access=all&expiry=keep&budget_period=day&token_budget=7"),
        )
        .await,
        StatusCode::SEE_OTHER
    );
    assert_eq!(
        api_keys::list(&state.db).await.unwrap()[0].budget,
        Some(KeyBudget {
            period: BudgetPeriod::Day,
            tokens: Some(7),
            cost_usd: None,
        })
    );
}
