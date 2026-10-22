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
    providers::Protocol,
};
use tower::ServiceExt;

async fn state(url: &str) -> AppState {
    AppState::from_config(Config {
        database_url: url.into(),
        bind: "127.0.0.1:0".parse().unwrap(),
        crypto: Crypto::from_base64(&SecretString::from(STANDARD.encode([15; 32]))).unwrap(),
        api_key: None,
        dashboard_login: sparkrouter::config::DashboardLogin::Disabled,
        dashboard_secure_cookie: false,
        metrics_token: None,
    })
    .await
    .unwrap()
}

async fn get(state: &AppState, path: &str) -> String {
    let response = app::router(state.clone())
        .oneshot(Request::get(path).body(Body::empty()).unwrap())
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

async fn save(
    state: &AppState,
    path: &str,
    fields: &str,
    csrf: &str,
    origin: Option<&str>,
) -> StatusCode {
    let mut request = Request::post(path)
        .header("content-type", "application/x-www-form-urlencoded")
        .header("host", "localhost");
    if let Some(origin) = origin {
        request = request.header("origin", origin);
    }
    app::router(state.clone())
        .oneshot(
            request
                .body(Body::from(format!("_csrf={csrf}{fields}")))
                .unwrap(),
        )
        .await
        .unwrap()
        .status()
}

#[tokio::test]
async fn tags_defaults_overrides_validation_and_persistence() {
    let dir = tempfile::tempdir().unwrap();
    let url = format!("sqlite://{}", dir.path().join("tags.db").display());
    let state = state(&url).await;
    let html = get(&state, "/providers").await;
    assert!(html.contains("data-provider-tags=\"decision\""));
    assert!(html.contains("data-provider-tags=\"text\""));
    assert!(!html.contains("<span class=\"badge\">SystemOne</span>"));
    assert!(html.contains("class=\"badge provider-capability-tag\">Decision</span>"));
    assert_eq!(html.matches("data-provider-tag-filter").count(), 7);
    let path = "/providers/catalog/openai/tags";
    assert_eq!(
        save(
            &state,
            path,
            "&text=1&image=1&tts=1",
            &state.csrf_token,
            None
        )
        .await,
        StatusCode::SEE_OTHER
    );
    assert_eq!(
        db::provider_tags::list(&state.db).await.unwrap()["/providers/catalog/openai"],
        vec!["image", "text", "tts"]
    );
    let page = get(&state, "/providers/catalog/openai").await;
    assert!(page.contains("name=\"image\" value=\"1\" checked"));
    assert_eq!(
        save(&state, path, "&video=1", "wrong", None).await,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        save(
            &state,
            path,
            "&video=1",
            &state.csrf_token,
            Some("https://other.example")
        )
        .await,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        save(&state, path, "&bogus=1", &state.csrf_token, None).await,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        save(
            &state,
            "/providers/catalog/missing/tags",
            "",
            &state.csrf_token,
            None
        )
        .await,
        StatusCode::NOT_FOUND
    );
    state.db.close().await;
    let reopened = self::state(&url).await;
    assert!(
        get(&reopened, "/providers")
            .await
            .contains("data-provider-tags=\"image text tts\"")
    );
    assert_eq!(
        save(&reopened, path, "", &reopened.csrf_token, None).await,
        StatusCode::SEE_OTHER
    );
    assert!(
        db::provider_tags::list(&reopened.db).await.unwrap()["/providers/catalog/openai"]
            .is_empty()
    );
    reopened.db.close().await;
}

#[tokio::test]
async fn custom_tags_are_independent_and_cascade_on_delete() {
    let state = state("sqlite::memory:").await;
    let one = db::provider_groups::create_with_slug(
        &state.db,
        "One <test>",
        "https://example.com/v1",
        &Protocol::OpenAi,
        None,
    )
    .await
    .unwrap();
    let two = db::provider_groups::create_with_slug(
        &state.db,
        "Two",
        "https://example.com/v1",
        &Protocol::SystemOne,
        None,
    )
    .await
    .unwrap();
    let identity = format!("/providers/groups/{}", one.id);
    assert_eq!(
        save(
            &state,
            &format!("{identity}/tags"),
            "&music=1&stt=1&video=1",
            &state.csrf_token,
            None
        )
        .await,
        StatusCode::SEE_OTHER
    );
    let page = get(&state, &identity).await;
    assert!(page.contains("name=\"music\" value=\"1\" checked"));
    assert!(page.contains("One &#60;test&#62;"));
    assert!(
        get(&state, &format!("/providers/groups/{}", two.id))
            .await
            .contains("name=\"decision\" value=\"1\" checked")
    );
    assert_eq!(
        save(
            &state,
            "/providers/groups/missing/tags",
            "&text=1",
            &state.csrf_token,
            None
        )
        .await,
        StatusCode::NOT_FOUND
    );
    db::provider_groups::delete(&state.db, &one.id)
        .await
        .unwrap();
    assert!(
        !db::provider_tags::list(&state.db)
            .await
            .unwrap()
            .contains_key(&identity)
    );
}
