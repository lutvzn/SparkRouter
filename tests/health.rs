use axum::{
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
};
use tower::ServiceExt;

async fn state() -> AppState {
    AppState::from_config(Config {
        database_url: "sqlite::memory:".into(),
        bind: "127.0.0.1:0".parse().unwrap(),
        crypto: Crypto::from_base64(&SecretString::from(STANDARD.encode([8; 32]))).unwrap(),
        api_key: Some(SecretString::from("probe-secret-key")),
        dashboard_login: sparkrouter::config::DashboardLogin::Environment(SecretString::from(
            "probe-dashboard-password",
        )),
        dashboard_secure_cookie: false,
    })
    .await
    .unwrap()
}

async fn probe(state: &AppState, path: &str, method: &str, status: StatusCode, body: &str) {
    let response = app::router(state.clone())
        .oneshot(
            Request::builder()
                .method(method)
                .uri(path)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), status);
    assert_eq!(response.headers()["cache-control"], "no-store");
    assert!(response.headers().get("set-cookie").is_none());
    assert!(response.headers().get("location").is_none());
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(bytes.as_ref(), body.as_bytes());
}

#[tokio::test]
async fn probes_are_public_and_head_is_supported() {
    let state = state().await;
    probe(
        &state,
        "/healthz",
        "GET",
        StatusCode::OK,
        "{\"status\":\"ok\"}",
    )
    .await;
    probe(
        &state,
        "/readyz",
        "GET",
        StatusCode::OK,
        "{\"status\":\"ready\"}",
    )
    .await;
    probe(&state, "/healthz", "HEAD", StatusCode::OK, "").await;
    probe(&state, "/readyz", "HEAD", StatusCode::OK, "").await;
}

#[tokio::test]
async fn database_failure_is_sanitized_and_does_not_change_liveness() {
    let state = state().await;
    state.db.close().await;
    probe(
        &state,
        "/readyz",
        "GET",
        StatusCode::SERVICE_UNAVAILABLE,
        "{\"status\":\"unavailable\"}",
    )
    .await;
    probe(
        &state,
        "/healthz",
        "GET",
        StatusCode::OK,
        "{\"status\":\"ok\"}",
    )
    .await;
}

#[tokio::test]
async fn readiness_times_out_if_the_pool_is_exhausted() {
    let state = state().await;
    let connection = state.db.acquire().await.unwrap();
    let started = std::time::Instant::now();
    probe(
        &state,
        "/readyz",
        "GET",
        StatusCode::SERVICE_UNAVAILABLE,
        "{\"status\":\"unavailable\"}",
    )
    .await;
    assert!(started.elapsed() >= std::time::Duration::from_secs(2));
    assert!(started.elapsed() < std::time::Duration::from_secs(5));
    probe(
        &state,
        "/healthz",
        "GET",
        StatusCode::OK,
        "{\"status\":\"ok\"}",
    )
    .await;
    drop(connection);
    probe(
        &state,
        "/readyz",
        "GET",
        StatusCode::OK,
        "{\"status\":\"ready\"}",
    )
    .await;
}
