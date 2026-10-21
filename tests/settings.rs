//! Settings persist only bounded retention; deployment credentials stay private.
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
    db,
};
use tower::ServiceExt;

async fn state(password: bool) -> AppState {
    AppState::from_config(Config {
        database_url: "sqlite::memory:".into(),
        bind: "127.0.0.1:0".parse().unwrap(),
        crypto: Crypto::from_base64(&SecretString::from(STANDARD.encode([27; 32]))).unwrap(),
        api_key: Some(SecretString::from("private-api-value")),
        dashboard_password: password.then(|| SecretString::from("private-dashboard-password")),
        dashboard_secure_cookie: false,
    })
    .await
    .unwrap()
}

async fn html(state: AppState) -> String {
    let response = app::router(state)
        .oneshot(Request::get("/settings").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
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
async fn settings_render_navigation_sections_and_sanitized_status() {
    let html = html(state(false).await).await;
    assert!(html.contains("href=\"/settings\" aria-current=\"page\""));
    for text in [
        "Appearance",
        "Request history",
        "Backup &amp; restore",
        "Security",
        "System",
        "Connected",
        "Enabled",
        "dashboard is open",
        "name=\"_csrf\"",
        "value=\"10000\"",
    ] {
        assert!(html.contains(text), "missing {text}");
    }
    assert_eq!(html.matches("aria-current=\"page\"").count(), 1);
    assert!(!html.contains("href=\"/configuration\""));
    assert!(html.contains("action=\"/configuration/export\""));
    assert!(html.contains("action=\"/configuration/preview\""));
    assert!(html.contains("name=\"configuration\""));
    assert!(!html.contains("<footer"));
    assert!(!html.contains("self-hosted LLM gateway with ordered fallback combos"));
    assert!(!html.contains("private-api-value"));
    assert!(!html.contains("private-dashboard-password"));
    assert!(!html.contains("sqlite::memory:"));
}

#[tokio::test]
async fn standalone_configuration_page_is_removed() {
    let response = app::router(state(false).await)
        .oneshot(Request::get("/configuration").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn settings_require_dashboard_login_and_tolerate_database_failure() {
    let authenticated = state(true).await;
    for path in ["/settings", "/history/example/payload"] {
        let response = app::router(authenticated.clone())
            .oneshot(Request::get(path).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        assert!(
            response.headers()["location"]
                .to_str()
                .unwrap()
                .starts_with("/login")
        );
    }
    let state = state(false).await;
    state.db.close().await;
    let html = html(state).await;
    assert!(html.contains("History settings unavailable"));
    assert!(html.contains("<dd>Unavailable</dd>"));
    assert!(!html.contains("pool closed"));
}

async fn post(state: &AppState, path: &str, body: String, cross_origin: bool) -> StatusCode {
    let mut request =
        Request::post(path).header("content-type", "application/x-www-form-urlencoded");
    if cross_origin {
        request = request
            .header("host", "localhost")
            .header("origin", "http://elsewhere.invalid");
    }
    app::router(state.clone())
        .oneshot(request.body(Body::from(body)).unwrap())
        .await
        .unwrap()
        .status()
}

#[tokio::test]
async fn retention_forms_validate_csrf_origin_range_and_clear_confirmation() {
    let state = state(false).await;
    for input in ["0", "10001", "-1", "abc", "1.5"] {
        assert_eq!(
            post(
                &state,
                "/settings",
                format!("_csrf={}&history_retention={input}", state.csrf_token),
                false
            )
            .await,
            StatusCode::BAD_REQUEST
        );
    }
    assert_eq!(
        post(
            &state,
            "/settings",
            "_csrf=wrong&history_retention=1".into(),
            false
        )
        .await,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        post(
            &state,
            "/settings",
            format!("_csrf={}&history_retention=1", state.csrf_token),
            true
        )
        .await,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        db::settings::history_retention(&state.db).await.unwrap(),
        10000
    );
    assert_eq!(
        post(
            &state,
            "/settings",
            format!("_csrf={}&history_retention=2", state.csrf_token),
            false
        )
        .await,
        StatusCode::SEE_OTHER
    );
    assert_eq!(db::settings::history_retention(&state.db).await.unwrap(), 2);
    for body in [
        format!("_csrf={}", state.csrf_token),
        "_csrf=wrong&confirm=yes".into(),
    ] {
        assert_eq!(
            post(&state, "/settings/history/clear", body, false).await,
            StatusCode::BAD_REQUEST
        );
    }
    assert_eq!(
        post(
            &state,
            "/settings/history/clear",
            format!("_csrf={}&confirm=yes", state.csrf_token),
            true
        )
        .await,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        post(
            &state,
            "/settings/history/clear",
            format!("_csrf={}&confirm=yes", state.csrf_token),
            false
        )
        .await,
        StatusCode::SEE_OTHER
    );
}

#[tokio::test]
async fn logging_mode_requires_valid_choice_csrf_origin_and_extended_confirmation() {
    use db::settings::{LoggingMode, logging_mode};
    let state = state(false).await;
    for body in [
        format!("_csrf={}&logging_mode=extended", state.csrf_token),
        format!("_csrf={}&logging_mode=invalid", state.csrf_token),
        "_csrf=wrong&logging_mode=off".into(),
    ] {
        assert_eq!(
            post(&state, "/settings/logging", body, false).await,
            StatusCode::BAD_REQUEST
        );
    }
    assert!(logging_mode(&state.db).await.unwrap() == LoggingMode::Basic);
    assert_eq!(
        post(
            &state,
            "/settings/logging",
            format!(
                "_csrf={}&logging_mode=extended&confirm_extended=yes",
                state.csrf_token
            ),
            true
        )
        .await,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        post(
            &state,
            "/settings/logging",
            format!(
                "_csrf={}&logging_mode=extended&confirm_extended=yes",
                state.csrf_token
            ),
            false
        )
        .await,
        StatusCode::SEE_OTHER
    );
    assert!(logging_mode(&state.db).await.unwrap() == LoggingMode::Extended);
    let page = html(state.clone()).await;
    assert!(page.contains("value=\"extended\" selected"));
    assert!(page.contains("plaintext"));
    for mode in ["off", "basic"] {
        assert_eq!(
            post(
                &state,
                "/settings/logging",
                format!("_csrf={}&logging_mode={mode}", state.csrf_token),
                false
            )
            .await,
            StatusCode::SEE_OTHER
        );
        assert_eq!(logging_mode(&state.db).await.unwrap().as_str(), mode);
    }
}

async fn record(pool: &sqlx::SqlitePool, id: &str) {
    db::request_history::save(
        pool,
        &db::request_history::Record {
            id: id.into(),
            source: "api".into(),
            model: "combo/example".into(),
            streaming: false,
            outcome: "success".into(),
            status: 200,
            latency_ms: 1,
            input_tokens: None,
            output_tokens: None,
            attempts: vec![db::request_history::Attempt {
                provider_id: "p".into(),
                provider_name: "p".into(),
                model: "m".into(),
                position: 0,
                outcome: "selected".into(),
                latency_ms: 1,
            }],
        },
    )
    .await
    .unwrap();
}

#[tokio::test]
async fn retention_prunes_immediately_cascades_and_survives_reopening() {
    let dir = tempfile::tempdir().unwrap();
    let url = format!("sqlite://{}", dir.path().join("settings.db").display());
    let pool = db::connect(&url).await.unwrap();
    for id in ["one", "two", "three"] {
        record(&pool, id).await;
    }
    db::settings::set_history_retention(&pool, 2).await.unwrap();
    assert_eq!(db::request_history::count(&pool).await.unwrap(), 2);
    let attempts: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM request_attempts")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(attempts, 2);
    record(&pool, "four").await;
    let ids: Vec<String> = sqlx::query_scalar("SELECT id FROM request_history ORDER BY rowid")
        .fetch_all(&pool)
        .await
        .unwrap();
    assert_eq!(ids, ["three", "four"]);
    db::settings::set_logging_mode(&pool, db::settings::LoggingMode::Extended)
        .await
        .unwrap();
    pool.close().await;
    let pool = db::connect(&url).await.unwrap();
    assert!(
        db::settings::logging_mode(&pool).await.unwrap() == db::settings::LoggingMode::Extended
    );
    assert_eq!(db::settings::history_retention(&pool).await.unwrap(), 2);
    db::settings::set_history_retention(&pool, 10000)
        .await
        .unwrap();
    assert_eq!(db::request_history::count(&pool).await.unwrap(), 2);
    db::request_history::clear(&pool).await.unwrap();
    let attempts: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM request_attempts")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(attempts, 0);
    assert_eq!(db::usage::overview(&pool).await.unwrap().requests, 0);
    pool.close().await;
}

#[tokio::test]
async fn history_buffer_accepts_only_offered_sizes_with_csrf_and_origin() {
    use db::settings::history_buffer;
    let state = state(false).await;
    assert_eq!(history_buffer(&state.db).await.unwrap(), 1);
    let page = html(state.clone()).await;
    assert!(page.contains("value=\"1\" selected"));
    assert!(page.contains("request history write unavailable"));
    for input in ["0", "3", "16", "-1", "abc"] {
        assert_eq!(
            post(
                &state,
                "/settings/history-buffer",
                format!("_csrf={}&history_buffer={input}", state.csrf_token),
                false
            )
            .await,
            StatusCode::BAD_REQUEST
        );
    }
    for (body, cross_origin) in [
        ("_csrf=wrong&history_buffer=4".to_owned(), false),
        (format!("_csrf={}&history_buffer=4", state.csrf_token), true),
    ] {
        assert_eq!(
            post(&state, "/settings/history-buffer", body, cross_origin).await,
            StatusCode::BAD_REQUEST
        );
    }
    assert_eq!(history_buffer(&state.db).await.unwrap(), 1);
    assert_eq!(
        post(
            &state,
            "/settings/history-buffer",
            format!("_csrf={}&history_buffer=4", state.csrf_token),
            false
        )
        .await,
        StatusCode::SEE_OTHER
    );
    assert_eq!(history_buffer(&state.db).await.unwrap(), 4);
    assert!(html(state).await.contains("value=\"4\" selected"));
}
