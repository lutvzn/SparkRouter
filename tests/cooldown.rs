//! Failing-target cooldown end to end: off by default, enabled from Settings,
//! cooling targets tried last (never skipped), a success ending the cooldown,
//! direct routing using the same order, and the History diagnostics.

mod common;
use common::{json_body, spawn_upstream};

use axum::{
    Json, Router,
    body::Body,
    http::{Request, StatusCode},
    response::IntoResponse,
    routing::post,
};
use base64::{Engine, engine::general_purpose::STANDARD};
use secrecy::SecretString;
use serde_json::json;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use tower::ServiceExt;

use sparkrouter::{
    app::{self, AppState},
    config::{Config, DashboardLogin},
    crypto::Crypto,
    db::{combos, connection_models, provider_groups, providers as provider_db},
    providers::Protocol,
};

async fn test_state() -> AppState {
    AppState::from_config(Config {
        database_url: "sqlite::memory:".into(),
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

/// A chat upstream that answers 503 while `failing` is set.
async fn upstream(failing: Arc<AtomicBool>) -> common::MockUpstream {
    spawn_upstream(Router::new().route(
        "/chat/completions",
        post(move || {
            let failing = failing.clone();
            async move {
                if failing.load(Ordering::SeqCst) {
                    return StatusCode::SERVICE_UNAVAILABLE.into_response();
                }
                Json(json!({"choices":[{"message":{"content":"ok"}}]})).into_response()
            }
        }),
    ))
    .await
}

async fn save_provider(state: &AppState, id: &str, base_url: &str) {
    provider_db::save(
        &state.db,
        &provider_db::Provider {
            id: id.into(),
            name: id.into(),
            base_url: base_url.into(),
            protocol: Protocol::OpenAi,
            encrypted_api_key: vec![],
            enabled: true,
        },
    )
    .await
    .unwrap();
}

async fn chat(state: &AppState, model: &str) -> StatusCode {
    app::router(state.clone())
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/chat/completions")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({"model":model, "messages":[{"role":"user","content":"hi"}]}).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap()
        .status()
}

async fn set_cooldown(state: &AppState, value: &str) -> StatusCode {
    app::router(state.clone())
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/settings/cooldown")
                .header("content-type", "application/x-www-form-urlencoded")
                .body(Body::from(format!(
                    "_csrf={}&target_cooldown={value}",
                    state.csrf_token
                )))
                .unwrap(),
        )
        .await
        .unwrap()
        .status()
}

#[tokio::test]
async fn cooling_targets_are_tried_last_until_they_succeed() {
    let state = test_state().await;
    let primary_failing = Arc::new(AtomicBool::new(true));
    let backup_failing = Arc::new(AtomicBool::new(false));
    let primary = upstream(primary_failing.clone()).await;
    let backup = upstream(backup_failing.clone()).await;
    save_provider(&state, "primary", &primary.url()).await;
    save_provider(&state, "backup", &backup.url()).await;
    let combo = combos::create(&state.db, "coding").await.unwrap();
    combos::add_step(&state.db, &combo.id, "primary", "m")
        .await
        .unwrap();
    combos::add_step(&state.db, &combo.id, "backup", "m")
        .await
        .unwrap();

    // Off by default: every request starts at the failing primary.
    for expected in 1..=2 {
        assert_eq!(chat(&state, "combo/coding").await, StatusCode::OK);
        assert_eq!(primary.hits.count(), expected);
    }
    assert_eq!(set_cooldown(&state, "maybe").await, StatusCode::BAD_REQUEST);
    assert_eq!(set_cooldown(&state, "on").await, StatusCode::SEE_OTHER);

    assert_eq!(chat(&state, "combo/coding").await, StatusCode::OK);
    assert_eq!((primary.hits.count(), backup.hits.count()), (3, 3));
    // The primary is cooling: the backup goes first.
    assert_eq!(chat(&state, "combo/coding").await, StatusCode::OK);
    assert_eq!((primary.hits.count(), backup.hits.count()), (3, 4));

    // Cooling targets are deferred, not skipped: with the backup failing too,
    // the recovered primary is still tried, last.
    backup_failing.store(true, Ordering::SeqCst);
    primary_failing.store(false, Ordering::SeqCst);
    assert_eq!(chat(&state, "combo/coding").await, StatusCode::OK);
    assert_eq!((primary.hits.count(), backup.hits.count()), (4, 5));
    // Its success ended its cooldown; the failed backup now waits instead.
    assert_eq!(chat(&state, "combo/coding").await, StatusCode::OK);
    assert_eq!((primary.hits.count(), backup.hits.count()), (5, 5));

    state.flush_history().await;
    let history = app::router(state.clone())
        .oneshot(Request::get("/history").body(Body::empty()).unwrap())
        .await
        .unwrap();
    let page = String::from_utf8(common::body_bytes(history).await.to_vec()).unwrap();
    assert!(page.contains("cooldown: 1 cooling target(s) moved last"));

    assert_eq!(set_cooldown(&state, "off").await, StatusCode::SEE_OTHER);
    primary_failing.store(true, Ordering::SeqCst);
    backup_failing.store(false, Ordering::SeqCst);
    for _ in 0..2 {
        assert_eq!(chat(&state, "combo/coding").await, StatusCode::OK);
    }
    assert_eq!(primary.hits.count(), 7);

    // With cooldown on and every target cooling, each is still tried once.
    assert_eq!(set_cooldown(&state, "on").await, StatusCode::SEE_OTHER);
    backup_failing.store(true, Ordering::SeqCst);
    assert_eq!(chat(&state, "combo/coding").await, StatusCode::BAD_GATEWAY);
    let exhausted = app::router(state.clone())
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/chat/completions")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({"model":"combo/coding", "messages":[{"role":"user","content":"hi"}]})
                        .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(exhausted.status(), StatusCode::BAD_GATEWAY);
    assert_eq!(
        json_body(exhausted).await["error"]["type"],
        "upstream_error"
    );
    assert_eq!((primary.hits.count(), backup.hits.count()), (9, 9));
}

#[tokio::test]
async fn direct_models_order_their_connections_the_same_way() {
    let state = test_state().await;
    let first_failing = Arc::new(AtomicBool::new(true));
    let first = upstream(first_failing.clone()).await;
    let second = upstream(Arc::default()).await;
    save_provider(&state, "first", &first.url()).await;
    save_provider(&state, "second", &second.url()).await;
    let group = provider_groups::create_with_slug(
        &state.db,
        "Pool",
        &first.url(),
        &Protocol::OpenAi,
        Some("pool"),
    )
    .await
    .unwrap();
    for id in ["first", "second"] {
        sqlx::query("INSERT INTO provider_group_connections (provider_id, group_id) VALUES (?, ?)")
            .bind(id)
            .bind(&group.id)
            .execute(&state.db)
            .await
            .unwrap();
        connection_models::replace(
            &state.db,
            id,
            &[connection_models::StoredModel {
                id: "m".into(),
                name: "m".into(),
                fields: vec![],
                free: false,
            }],
            false,
        )
        .await
        .unwrap();
    }
    assert_eq!(set_cooldown(&state, "on").await, StatusCode::SEE_OTHER);
    assert_eq!(chat(&state, "pool/m").await, StatusCode::OK);
    assert_eq!((first.hits.count(), second.hits.count()), (1, 1));
    assert_eq!(chat(&state, "pool/m").await, StatusCode::OK);
    assert_eq!((first.hits.count(), second.hits.count()), (1, 2));
}
