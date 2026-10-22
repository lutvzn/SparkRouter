//! Direct-route affinity end to end: on for 5 minutes by default, the last
//! successful connection tried first, a retryable failure clearing it, 0 in
//! Settings restoring oldest-first, and the History diagnostics.

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
    db::{connection_models, provider_groups, providers as provider_db},
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

/// Two connections under slug `pool`, both listing model `m`, oldest first.
async fn pool(state: &AppState, urls: [&str; 2]) {
    let group = provider_groups::create_with_slug(
        &state.db,
        "Pool",
        urls[0],
        &Protocol::OpenAi,
        Some("pool"),
    )
    .await
    .unwrap();
    for (id, url) in ["first", "second"].into_iter().zip(urls) {
        provider_db::save(
            &state.db,
            &provider_db::Provider {
                id: id.into(),
                name: id.into(),
                base_url: url.into(),
                protocol: Protocol::OpenAi,
                encrypted_api_key: vec![],
                enabled: true,
            },
        )
        .await
        .unwrap();
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
}

async fn chat(state: &AppState) -> StatusCode {
    app::router(state.clone())
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/chat/completions")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({"model":"pool/m", "messages":[{"role":"user","content":"hi"}]})
                        .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap()
        .status()
}

async fn set_minutes(state: &AppState, value: &str) -> StatusCode {
    app::router(state.clone())
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/settings/direct-affinity")
                .header("content-type", "application/x-www-form-urlencoded")
                .body(Body::from(format!(
                    "_csrf={}&direct_affinity_minutes={value}",
                    state.csrf_token
                )))
                .unwrap(),
        )
        .await
        .unwrap()
        .status()
}

async fn page(state: &AppState, uri: &str) -> String {
    let response = app::router(state.clone())
        .oneshot(Request::get(uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    String::from_utf8(common::body_bytes(response).await.to_vec()).unwrap()
}

#[tokio::test]
async fn direct_models_stick_to_the_last_successful_connection() {
    let state = test_state().await;
    let first_failing = Arc::new(AtomicBool::new(true));
    let second_failing = Arc::new(AtomicBool::new(false));
    let first = upstream(first_failing.clone()).await;
    let second = upstream(second_failing.clone()).await;
    pool(&state, [&first.url(), &second.url()]).await;
    assert!(page(&state, "/settings").await.contains(r#"name="direct_affinity_minutes" type="number" min="0" max="1440" step="1" required value="5""#));

    // On by default: after the first connection fails once, the second is
    // remembered and tried first.
    assert_eq!(chat(&state).await, StatusCode::OK);
    assert_eq!((first.hits.count(), second.hits.count()), (1, 1));
    first_failing.store(false, Ordering::SeqCst);
    assert_eq!(chat(&state).await, StatusCode::OK);
    assert_eq!((first.hits.count(), second.hits.count()), (1, 2));

    // A retryable failure clears the hint; the other connection is still
    // tried, and becomes the remembered one.
    second_failing.store(true, Ordering::SeqCst);
    assert_eq!(chat(&state).await, StatusCode::OK);
    assert_eq!((first.hits.count(), second.hits.count()), (2, 3));
    second_failing.store(false, Ordering::SeqCst);
    assert_eq!(chat(&state).await, StatusCode::OK);
    assert_eq!((first.hits.count(), second.hits.count()), (3, 3));

    state.flush_history().await;
    let history = page(&state, "/history").await;
    assert!(history.contains("Strategy: Oldest connection first"));
    assert!(history.contains("affinity: 5 min (Hit: last successful connection selected)"));
    assert!(history.contains("actual start: step 2"));

    for invalid in ["1441", "-1", "soon", ""] {
        assert_eq!(set_minutes(&state, invalid).await, StatusCode::BAD_REQUEST);
    }
    // 0 is off: every request starts at the oldest connection again.
    assert_eq!(set_minutes(&state, "0").await, StatusCode::SEE_OTHER);
    first_failing.store(true, Ordering::SeqCst);
    for expected in [(4, 4), (5, 5)] {
        assert_eq!(chat(&state).await, StatusCode::OK);
        assert_eq!((first.hits.count(), second.hits.count()), expected);
    }
    assert!(page(&state, "/settings").await.contains(r#"value="0""#));

    // A new timeout takes effect for new hints at once.
    assert_eq!(set_minutes(&state, "30").await, StatusCode::SEE_OTHER);
    assert_eq!(chat(&state).await, StatusCode::OK);
    assert_eq!(chat(&state).await, StatusCode::OK);
    assert_eq!((first.hits.count(), second.hits.count()), (6, 7));

    // With every connection failing, each is still tried exactly once.
    second_failing.store(true, Ordering::SeqCst);
    let exhausted = app::router(state.clone())
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/chat/completions")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({"model":"pool/m", "messages":[{"role":"user","content":"hi"}]})
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
    assert_eq!((first.hits.count(), second.hits.count()), (7, 8));
}
