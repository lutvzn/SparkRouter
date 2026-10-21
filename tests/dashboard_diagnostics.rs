//! Local-only regression coverage for category review, refresh health, and routing snapshots.
mod common;

use axum::{
    Json, Router,
    body::Body,
    http::{Request, StatusCode},
    routing::{get, post},
};
use base64::{Engine, engine::general_purpose::STANDARD};
use secrecy::SecretString;
use serde_json::json;
use sparkrouter::{
    app::{self, AppState},
    config::Config,
    crypto::Crypto,
    db::{
        self, combos,
        connection_models::{self, StoredModel},
        providers::Provider,
    },
    providers::Protocol,
};
use tower::ServiceExt;

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
async fn provider(state: &AppState, id: &str, url: &str) {
    db::provider_groups::save_legacy_connection(
        &state.db,
        &Provider {
            id: id.into(),
            name: format!("Connection {id}"),
            base_url: url.into(),
            protocol: Protocol::OpenAi,
            encrypted_api_key: vec![],
            enabled: true,
        },
        id,
    )
    .await
    .unwrap();
}
async fn form(app: &Router, path: &str, body: String) -> axum::response::Response {
    app.clone()
        .oneshot(
            Request::post(path)
                .header("content-type", "application/x-www-form-urlencoded")
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap()
}
async fn page(app: &Router, path: &str) -> String {
    let response = app
        .clone()
        .oneshot(Request::get(path).body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    String::from_utf8(common::body_bytes(response).await.to_vec()).unwrap()
}

#[tokio::test]
async fn refresh_health_survives_reload_preserves_cached_list_and_clears_on_success() {
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };
    let fail = Arc::new(AtomicBool::new(false));
    let flag = fail.clone();
    let upstream = common::spawn_upstream(Router::new().route(
        "/models",
        get(move || {
            let flag = flag.clone();
            async move {
                if flag.load(Ordering::SeqCst) {
                    (
                        StatusCode::UNAUTHORIZED,
                        Json(json!({"error":"secret-key https://sensitive.example/?key=secret"})),
                    )
                } else {
                    (
                        StatusCode::OK,
                        Json(json!({"data":[{"id":"new-model","supported_endpoints":["chat"]}]})),
                    )
                }
            }
        }),
    ))
    .await;
    let state = state().await;
    provider(&state, "refresh", &upstream.url()).await;
    let app = app::router(state.clone());
    let body = format!("_csrf={}", state.csrf_token);
    let response = form(&app, "/providers/refresh/models", body.clone()).await;
    assert_eq!(response.status(), StatusCode::OK);
    let html = String::from_utf8(common::body_bytes(response).await.to_vec()).unwrap();
    assert!(html.contains("Last successful refresh:"));
    assert!(html.contains("1 model"));
    assert!(html.contains("Upstream metadata"));
    assert!(html.contains("bundled snapshot") || html.contains("live download"));
    fail.store(true, Ordering::SeqCst);
    let response = form(&app, "/providers/refresh/models", body.clone()).await;
    let html = String::from_utf8(common::body_bytes(response).await.to_vec()).unwrap();
    assert!(html.contains("upstream HTTP 401"));
    assert!(html.contains("Using the last successful cached list"));
    assert!(!html.contains("secret-key"));
    assert!(!html.contains("sensitive.example"));
    sqlx::query("UPDATE connection_model_lists SET refreshed_at = '2000-01-01T00:00:00Z'")
        .execute(&state.db)
        .await
        .unwrap();
    let namespace = db::provider_slugs::for_connections(&state.db)
        .await
        .unwrap();
    // Legacy connection IDs resolve the custom group page.
    let html = page(&app, "/providers/groups/refresh").await;
    assert!(html.contains("Stale (over 24 hours)"));
    assert!(html.contains("upstream HTTP 401"));
    assert!(html.contains(&format!("{}/new-model", namespace["refresh"])));
    fail.store(false, Ordering::SeqCst);
    let response = form(&app, "/providers/refresh/models", body).await;
    let html = String::from_utf8(common::body_bytes(response).await.to_vec()).unwrap();
    assert!(!html.contains("upstream HTTP 401"));
    assert!(!html.contains("Stale (over 24 hours)"));
    assert_eq!(upstream.hits.count(), 3);
}

#[tokio::test]
async fn manual_review_enforces_csrf_catalog_metadata_and_referenced_declarations() {
    let state = state().await;
    provider(&state, "manual", "http://127.0.0.1:9").await;
    let app = app::router(state.clone());
    let post = |model: &str, category: &str| {
        format!(
            "_csrf={}&model={model}&category={category}",
            state.csrf_token
        )
    };
    let response = form(
        &app,
        "/providers/manual/classification",
        "_csrf=bad&model=custom&category=image".into(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        form(
            &app,
            "/providers/manual/classification",
            post("custom", "image")
        )
        .await
        .status(),
        StatusCode::OK
    );
    connection_models::replace(
        &state.db,
        "manual",
        &[StoredModel {
            id: "custom".into(),
            name: "Custom".into(),
            fields: vec![],
            free: false,
        }],
        false,
    )
    .await
    .unwrap();
    let html = page(&app, "/providers/groups/manual").await;
    assert!(html.contains("Image · Manual declaration"));
    let combo = combos::create_typed(
        &state.db,
        "images",
        combos::Category::Image,
        combos::Strategy::Priority,
        combos::AffinityOptions::default(),
    )
    .await
    .unwrap();
    combos::add_step(&state.db, &combo.id, "manual", "custom")
        .await
        .unwrap();
    assert_eq!(
        form(
            &app,
            "/providers/manual/classification",
            post("custom", "text")
        )
        .await
        .status(),
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        form(
            &app,
            "/providers/manual/classification",
            post("custom", "invalid")
        )
        .await
        .status(),
        StatusCode::BAD_REQUEST
    );
    provider(&state, "official", "https://api.openai.com/v1").await;
    assert_eq!(
        form(
            &app,
            "/providers/official/classification",
            post("gpt-image-1", "text")
        )
        .await
        .status(),
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        form(
            &app,
            "/providers/manual/classification",
            post("%00", "text")
        )
        .await
        .status(),
        StatusCode::BAD_REQUEST
    );
    connection_models::replace(
        &state.db,
        "manual",
        &[StoredModel {
            id: "custom".into(),
            name: "Custom".into(),
            free: false,
            fields: vec![connection_models::ModelField {
                label: "supported_endpoints".into(),
                value: "chat".into(),
            }],
        }],
        false,
    )
    .await
    .unwrap();
    let html = page(&app, "/providers/groups/manual").await;
    assert!(html.contains("Why review is needed:"));
    assert!(html.contains("Upstream metadata identifies this model as Text"));
    assert!(html.contains("saved declaration is Image"));
    assert!(html.contains("icons.svg#triangle-alert"));
    let popup = html.find("class=\"model-info-body\"").unwrap();
    assert!(!html[..popup].contains("Why review is needed:"));
    combos::delete(&state.db, &combo.id).await.unwrap();
    assert_eq!(
        form(
            &app,
            "/providers/manual/classification",
            post("custom", "text")
        )
        .await
        .status(),
        StatusCode::OK
    );
}

#[tokio::test]
async fn reset_restores_defaults_and_keeps_referenced_models_fail_closed() {
    let upstream = common::spawn_upstream(Router::new().route("/chat/completions", post(|| async {
        Json(json!({"id":"ok","object":"chat.completion","choices":[{"index":0,"message":{"role":"assistant","content":"OK"},"finish_reason":"stop"}]}))
    }))).await;
    let state = state().await;
    provider(&state, "reset", &upstream.url()).await;
    let app = app::router(state.clone());
    let body = |model: &str, category: &str| {
        format!(
            "_csrf={}&model={model}&category={category}",
            state.csrf_token
        )
    };
    connection_models::replace(
        &state.db,
        "reset",
        &[StoredModel {
            id: "known".into(),
            name: "Known".into(),
            free: false,
            fields: vec![connection_models::ModelField {
                label: "supported_endpoints".into(),
                value: "chat".into(),
            }],
        }],
        false,
    )
    .await
    .unwrap();
    let combo = combos::create(&state.db, "diagnostic").await.unwrap();
    combos::add_step(&state.db, &combo.id, "reset", "known")
        .await
        .unwrap();
    let response = form(
        &app,
        "/providers/reset/classification",
        body("known", "default"),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let html = String::from_utf8(common::body_bytes(response).await.to_vec()).unwrap();
    assert!(!html.contains("Review model classifications"));
    assert!(!html.contains("Reset to default"));
    assert!(html.contains(">Default</option>"));
    assert!(html.contains("value=\"default\" selected"));
    assert!(html.contains("Text · Upstream metadata"));
    let count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM model_categories WHERE provider_id='reset'")
            .fetch_one(&state.db)
            .await
            .unwrap();
    assert_eq!(count, 0);
    chat(&app).await;
    combos::add_step(&state.db, &combo.id, "reset", "unknown")
        .await
        .unwrap();
    assert_eq!(
        form(
            &app,
            "/providers/reset/classification",
            body("unknown", "default")
        )
        .await
        .status(),
        StatusCode::BAD_REQUEST
    );
    // Changed metadata makes a formerly valid automatic default incompatible.
    connection_models::replace(
        &state.db,
        "reset",
        &[StoredModel {
            id: "known".into(),
            name: "Known".into(),
            free: false,
            fields: vec![connection_models::ModelField {
                label: "supported_endpoints".into(),
                value: "images".into(),
            }],
        }],
        false,
    )
    .await
    .unwrap();
    assert_eq!(
        form(
            &app,
            "/providers/reset/classification",
            body("known", "default")
        )
        .await
        .status(),
        StatusCode::BAD_REQUEST
    );
    combos::delete(&state.db, &combo.id).await.unwrap();
    assert_eq!(
        form(
            &app,
            "/providers/reset/classification",
            body("unknown", "default")
        )
        .await
        .status(),
        StatusCode::OK
    );
    assert_eq!(upstream.hits.count(), 1);
}

async fn wait_for_history(state: &AppState, count: i64) {
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let stored: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM request_routing")
                .fetch_one(&state.db)
                .await
                .unwrap();
            if stored == count {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
}
async fn chat(app: &Router) {
    let response = app.clone().oneshot(Request::post("/v1/chat/completions")
        .header("content-type", "application/json").header("x-sparkrouter-session-id", "private-session")
        .body(Body::from(json!({"model":"combo/diagnostic","messages":[{"role":"user","content":"private-prompt"}]}).to_string())).unwrap()).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let result = common::json_body(response).await;
    assert_eq!(result["id"], "ok");
}

#[tokio::test]
async fn history_explains_strategy_affinity_fallback_and_cascading_clear() {
    let bad = common::spawn_upstream(Router::new().route(
        "/chat/completions",
        post(|| async { (StatusCode::TOO_MANY_REQUESTS, "sensitive-upstream-body") }),
    ))
    .await;
    let good = common::spawn_upstream(Router::new().route("/chat/completions", post(|| async {
        Json(json!({"id":"ok","object":"chat.completion","choices":[{"index":0,"message":{"role":"assistant","content":"OK"},"finish_reason":"stop"}]}))
    }))).await;
    let state = state().await;
    provider(&state, "bad", &bad.url()).await;
    provider(&state, "good", &good.url()).await;
    let combo = combos::create_with_options(
        &state.db,
        "diagnostic",
        combos::Strategy::RoundRobin,
        combos::AffinityOptions {
            mode: combos::AffinityMode::Basic,
            sticky_minutes: 5,
        },
    )
    .await
    .unwrap();
    combos::add_step(&state.db, &combo.id, "bad", "model")
        .await
        .unwrap();
    combos::add_step(&state.db, &combo.id, "good", "model")
        .await
        .unwrap();
    let app = app::router(state.clone());
    chat(&app).await;
    wait_for_history(&state, 1).await;
    chat(&app).await;
    wait_for_history(&state, 2).await;
    chat(&app).await;
    wait_for_history(&state, 3).await;
    // Selection metadata is a snapshot, not reconstructed from edited settings.
    combos::update_with_options(
        &state.db,
        &combo.id,
        "diagnostic",
        Some(combos::Strategy::Priority),
        Some(combos::AffinityOptions::default()),
    )
    .await
    .unwrap();
    let html = page(&app, "/history").await;
    assert!(html.contains("Round robin"));
    assert!(html.contains("Hit: preferred target selected"));
    assert!(html.contains("strategy start: step 1 · actual start: step 2"));
    assert!(html.contains("Retryable upstream HTTP 429: Too Many Requests"));
    assert!(html.contains("Upstream selected (HTTP 200)"));
    assert!(!html.contains("private-prompt"));
    assert!(!html.contains("private-session"));
    assert!(!html.contains("sensitive-upstream-body"));
    assert_eq!(bad.hits.count(), 1);
    assert_eq!(good.hits.count(), 3);
    db::settings::set_history_retention(&state.db, 1)
        .await
        .unwrap();
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM request_routing")
        .fetch_one(&state.db)
        .await
        .unwrap();
    assert_eq!(count, 1, "retention also prunes diagnostics");
    let response = form(
        &app,
        "/history/clear",
        format!("_csrf={}", state.csrf_token),
    )
    .await;
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM request_routing")
        .fetch_one(&state.db)
        .await
        .unwrap();
    assert_eq!(count, 0);
    db::settings::set_logging_mode(&state.db, db::settings::LoggingMode::Off)
        .await
        .unwrap();
    chat(&app).await;
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM request_routing")
        .fetch_one(&state.db)
        .await
        .unwrap();
    assert_eq!(count, 0, "logging Off does not store routing diagnostics");
}
