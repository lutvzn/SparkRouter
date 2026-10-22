//! `/metrics` end to end: absent until a token exists, bearer-protected,
//! token generation and regeneration from Settings, the environment token,
//! and counters that only label configuration a request reached.

mod common;
use common::{body_bytes, json_body, spawn_upstream};

use axum::{
    Json, Router,
    body::Body,
    http::{Request, StatusCode, header},
    routing::post,
};
use base64::{Engine, engine::general_purpose::STANDARD};
use secrecy::SecretString;
use serde_json::json;
use tower::ServiceExt;

use sparkrouter::{
    app::{self, AppState},
    config::{Config, DashboardLogin},
    crypto::Crypto,
    db::{combos, providers as provider_db},
    providers::Protocol,
};

async fn test_state(metrics_token: Option<&str>) -> AppState {
    AppState::from_config(Config {
        database_url: "sqlite::memory:".into(),
        bind: "127.0.0.1:0".parse().unwrap(),
        crypto: Crypto::from_base64(&SecretString::from(STANDARD.encode([42u8; 32]))).unwrap(),
        api_key: None,
        dashboard_login: DashboardLogin::Disabled,
        dashboard_secure_cookie: false,
        metrics_token: metrics_token.map(SecretString::from),
    })
    .await
    .unwrap()
}

async fn scrape(state: &AppState, token: Option<&str>) -> (StatusCode, String) {
    let mut request = Request::get("/metrics");
    if let Some(token) = token {
        request = request.header("authorization", format!("Bearer {token}"));
    }
    let response = app::router(state.clone())
        .oneshot(request.body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = response.status();
    if status == StatusCode::OK {
        assert_eq!(
            response.headers()[header::CONTENT_TYPE],
            "text/plain; version=0.0.4; charset=utf-8"
        );
    }
    if status == StatusCode::UNAUTHORIZED {
        assert_eq!(response.headers()[header::WWW_AUTHENTICATE], "Bearer");
    }
    (
        status,
        String::from_utf8(body_bytes(response).await.to_vec()).unwrap(),
    )
}

async fn token_action(state: &AppState, action: &str) -> (StatusCode, String) {
    let response = app::router(state.clone())
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/settings/metrics-token")
                .header("content-type", "application/x-www-form-urlencoded")
                .body(Body::from(format!(
                    "_csrf={}&action={action}",
                    state.csrf_token
                )))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    (
        status,
        String::from_utf8(body_bytes(response).await.to_vec()).unwrap(),
    )
}

/// The token shown once on the page that generated it.
fn shown_token(page: &str) -> String {
    let start = page.find("data-copy-model=\"srm-").expect("token shown") + 17;
    page[start..start + page[start..].find('"').unwrap()].to_owned()
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

#[tokio::test]
async fn settings_token_protects_metrics_and_regenerates() {
    let state = test_state(None).await;
    assert_eq!(scrape(&state, None).await.0, StatusCode::NOT_FOUND);
    assert_eq!(
        token_action(&state, "explode").await.0,
        StatusCode::BAD_REQUEST
    );

    let (status, page) = token_action(&state, "generate").await;
    assert_eq!(status, StatusCode::OK);
    assert!(page.contains("Metrics token generated"));
    let token = shown_token(&page);
    assert!(token.len() > 40);
    assert_eq!(scrape(&state, None).await.0, StatusCode::UNAUTHORIZED);
    assert_eq!(
        scrape(&state, Some("srm-wrong")).await.0,
        StatusCode::UNAUTHORIZED
    );
    let (status, text) = scrape(&state, Some(&token)).await;
    assert_eq!(status, StatusCode::OK);
    assert!(text.contains("# TYPE sparkrouter_requests_total counter"));
    // The token is never stored or shown again.
    let settings = app::router(state.clone())
        .oneshot(Request::get("/settings").body(Body::empty()).unwrap())
        .await
        .unwrap();
    let settings = String::from_utf8(body_bytes(settings).await.to_vec()).unwrap();
    assert!(!settings.contains(&token));
    assert!(settings.contains("Regenerate token"));
    let stored: String =
        sqlx::query_scalar("SELECT metrics_token_hash FROM dashboard_settings WHERE id = 1")
            .fetch_one(&state.db)
            .await
            .unwrap();
    assert!(!stored.contains(&token) && stored.len() == 64);

    let (_, page) = token_action(&state, "generate").await;
    let regenerated = shown_token(&page);
    assert_ne!(regenerated, token);
    assert_eq!(
        scrape(&state, Some(&token)).await.0,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(scrape(&state, Some(&regenerated)).await.0, StatusCode::OK);

    assert_eq!(
        token_action(&state, "remove").await.0,
        StatusCode::SEE_OTHER
    );
    assert_eq!(
        scrape(&state, Some(&regenerated)).await.0,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn counters_label_only_configuration_a_request_reached() {
    let state = test_state(Some("environment-metrics-token")).await;
    let upstream = spawn_upstream(Router::new().route(
        "/chat/completions",
        post(|| async {
            Json(json!({"choices":[], "usage":{"prompt_tokens":5, "completion_tokens":2, "cost":0.5}}))
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
    let combo = combos::create(&state.db, "coding").await.unwrap();
    combos::add_step(&state.db, &combo.id, "provider", "inner")
        .await
        .unwrap();
    assert_eq!(chat(&state, "combo/coding").await, StatusCode::OK);
    assert_eq!(chat(&state, "combo/coding").await, StatusCode::OK);
    assert_eq!(
        chat(&state, "combo/attacker-chosen").await,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        chat(&state, "nobody/attacker-model").await,
        StatusCode::NOT_FOUND
    );

    assert_eq!(upstream.hits.count(), 2);
    // Model listing is not a routed request.
    let listed = app::router(state.clone())
        .oneshot(Request::get("/v1/models").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(json_body(listed).await["data"][0]["id"], "combo/coding");

    let (status, text) = scrape(&state, Some("environment-metrics-token")).await;
    assert_eq!(status, StatusCode::OK);
    assert!(text.contains(
        "sparkrouter_requests_total{endpoint=\"chat\",combo=\"coding\",outcome=\"success\"} 2\n"
    ));
    assert!(text.contains(
        "sparkrouter_requests_total{endpoint=\"chat\",combo=\"\",outcome=\"failure\"} 2\n"
    ));
    assert!(!text.contains("attacker"));
    let provider = text
        .lines()
        .find(|line| line.starts_with("sparkrouter_upstream_attempts_total{"))
        .expect("attempt counter");
    assert!(provider.contains("outcome=\"selected\"} 2"), "{provider}");
    assert!(text.contains("type=\"input\"} 10\n"));
    assert!(text.contains("type=\"output\"} 4\n"));
    assert!(text.contains("sparkrouter_request_duration_seconds_count{endpoint=\"chat\"} 4\n"));
    assert!(text.contains(&format!(
        "sparkrouter_build_info{{version=\"{}\"}} 1\n",
        env!("CARGO_PKG_VERSION")
    )));
    // Every sample follows its own family's TYPE line.
    let mut family = String::new();
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("# TYPE ") {
            family = rest.split(' ').next().unwrap().to_owned();
        } else if !line.starts_with('#') {
            assert!(line.starts_with(&family), "{line} outside {family}");
        }
    }
}
