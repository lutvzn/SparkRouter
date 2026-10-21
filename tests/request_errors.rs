use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use base64::{Engine, engine::general_purpose::STANDARD};
use http_body_util::BodyExt;
use secrecy::SecretString;
use serde_json::Value;
use sparkrouter::{
    app::{self, AppState},
    config::Config,
    crypto::Crypto,
};
use tower::ServiceExt;

#[tokio::test]
async fn malformed_and_oversized_json_have_sanitized_openai_errors() {
    let state = AppState::from_config(Config {
        database_url: "sqlite::memory:".into(),
        bind: "127.0.0.1:0".parse().unwrap(),
        crypto: Crypto::from_base64(&SecretString::from(STANDARD.encode([31; 32]))).unwrap(),
        api_key: None,
        dashboard_login: sparkrouter::config::DashboardLogin::Disabled,
        dashboard_secure_cookie: false,
    })
    .await
    .unwrap();
    let router = app::router(state);
    let requests = [
        (
            "application/json",
            "{\"private-field\": unclosed".to_owned(),
            StatusCode::BAD_REQUEST,
        ),
        (
            "text/plain",
            "{}".to_owned(),
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
        ),
        (
            "application/json",
            " ".repeat(4 * 1024 * 1024 + 1),
            StatusCode::PAYLOAD_TOO_LARGE,
        ),
        (
            "text/json",
            "{}".to_owned(),
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
        ),
        ("", "{}".to_owned(), StatusCode::UNSUPPORTED_MEDIA_TYPE),
        (
            "application/json",
            "{\"private-field\": 1} trailing".to_owned(),
            StatusCode::BAD_REQUEST,
        ),
    ];
    for (content_type, body, expected) in requests {
        let response = router
            .clone()
            .oneshot(
                Request::post("/v1/chat/completions")
                    .header("content-type", content_type)
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), expected);
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let error: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(error["error"]["type"], "invalid_request_error");
        assert!(!String::from_utf8_lossy(&bytes).contains("private-field"));
    }
}

#[tokio::test]
async fn json_media_type_parameters_and_suffixes_are_accepted() {
    let state = AppState::from_config(Config {
        database_url: "sqlite::memory:".into(),
        bind: "127.0.0.1:0".parse().unwrap(),
        crypto: Crypto::from_base64(&SecretString::from(STANDARD.encode([31; 32]))).unwrap(),
        api_key: None,
        dashboard_login: sparkrouter::config::DashboardLogin::Disabled,
        dashboard_secure_cookie: false,
    })
    .await
    .unwrap();
    let router = app::router(state);
    for content_type in [
        "application/json; charset=utf-8",
        "Application/JSON;charset=utf-8",
        "application/cloudevents+json",
    ] {
        let response = router
            .clone()
            .oneshot(
                Request::post("/v1/chat/completions")
                    .header("content-type", content_type)
                    .body(Body::from("{}"))
                    .unwrap(),
            )
            .await
            .unwrap();
        // Parsed as JSON, then rejected for the missing model.
        assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{content_type}");
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let error: Value = serde_json::from_slice(&bytes).unwrap();
        assert!(
            error["error"]["message"]
                .as_str()
                .unwrap()
                .starts_with("model must be"),
            "{content_type}: {error}"
        );
    }
}
