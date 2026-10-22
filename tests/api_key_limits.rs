use axum::{
    Json, Router,
    body::Body,
    http::{Request, StatusCode},
    response::IntoResponse,
    routing::post,
};
use base64::{Engine, engine::general_purpose::STANDARD};
use http_body_util::BodyExt;
use secrecy::{ExposeSecret, SecretString};
use serde_json::{Value, json};
use sparkrouter::{
    app::{self, AppState},
    config::Config,
    crypto::Crypto,
    db::{
        self,
        api_keys::{self, Expiry, KeyAccess, KeyLimits},
    },
    providers::Protocol,
};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use tower::ServiceExt;

async fn state() -> AppState {
    AppState::from_config(Config {
        database_url: "sqlite::memory:".into(),
        bind: "127.0.0.1:0".parse().unwrap(),
        crypto: Crypto::from_base64(&SecretString::from(STANDARD.encode([42; 32]))).unwrap(),
        api_key: Some(SecretString::from("environment-key")),
        dashboard_login: sparkrouter::config::DashboardLogin::Disabled,
        dashboard_secure_cookie: false,
        metrics_token: None,
    })
    .await
    .unwrap()
}
struct Mock {
    hits: Arc<AtomicUsize>,
    entered: Arc<tokio::sync::Notify>,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for Mock {
    fn drop(&mut self) {
        self.task.abort();
    }
}
async fn seed(state: &AppState) -> Mock {
    let hits = Arc::new(AtomicUsize::new(0));
    let counter = hits.clone();
    let entered = Arc::new(tokio::sync::Notify::new());
    let notification = entered.clone();
    let router = Router::new().route(
        "/chat/completions",
        post(move |Json(body): Json<Value>| {
            let counter = counter.clone();
            let notification = notification.clone();
            async move {
                counter.fetch_add(1, Ordering::SeqCst);
                notification.notify_one();
                if body["wait"] == true {
                    std::future::pending::<()>().await;
                }
                if body["fail"] == true {
                    return (StatusCode::BAD_REQUEST, Json(json!({"error":"mock"})))
                        .into_response();
                }
                if body["stream"] == true {
                    let hang = body["hang"] == true;
                    let truncate = body["truncate"] == true;
                    let stream = async_stream::stream! {
                        yield Ok::<_, std::io::Error>("data: {\"choices\":[]}\n\n");
                        if hang { std::future::pending::<()>().await; }
                        if !truncate { yield Ok("data: [DONE]\n\n"); }
                    };
                    return (
                        [("content-type", "text/event-stream")],
                        Body::from_stream(stream),
                    )
                        .into_response();
                }
                Json(json!({"choices":[]})).into_response()
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let task = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    db::providers::save(
        &state.db,
        &db::providers::Provider {
            id: "provider".into(),
            name: "Mock".into(),
            base_url: url,
            protocol: Protocol::OpenAi,
            encrypted_api_key: vec![],
            enabled: true,
        },
    )
    .await
    .unwrap();
    let combo = db::combos::create(&state.db, "chat").await.unwrap();
    db::combos::add_step(&state.db, &combo.id, "provider", "inner")
        .await
        .unwrap();
    Mock {
        hits,
        entered,
        task,
    }
}
async fn issue(state: &AppState, rpm: Option<u32>, concurrent: Option<u32>) -> api_keys::IssuedKey {
    api_keys::create_with_limits(
        &state.db,
        &state.crypto,
        "App",
        KeyAccess::All,
        Expiry::Never,
        KeyLimits {
            requests_per_minute: rpm,
            max_concurrent: concurrent,
        },
    )
    .await
    .unwrap()
}
async fn chat(state: &AppState, key: &str, body: Value) -> axum::response::Response {
    app::router(state.clone())
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/chat/completions")
                .header("authorization", format!("Bearer {key}"))
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap()
}
async fn value(response: axum::response::Response) -> Value {
    serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes()).unwrap()
}
#[tokio::test]
async fn quota_is_per_key_models_exempt_and_rejections_do_not_route() {
    let state = state().await;
    let mock = seed(&state).await;
    let key = issue(&state, Some(1), None).await;
    let other = issue(&state, Some(1), None).await;
    let body = json!({"model":"combo/chat"});
    assert_eq!(
        chat(&state, key.secret.expose_secret(), body.clone())
            .await
            .status(),
        StatusCode::OK
    );
    let rejected = chat(&state, key.secret.expose_secret(), body.clone()).await;
    assert_eq!(rejected.status(), StatusCode::TOO_MANY_REQUESTS);
    let retry: u64 = rejected.headers()["retry-after"]
        .to_str()
        .unwrap()
        .parse()
        .unwrap();
    assert!((1..=60).contains(&retry));
    assert_eq!(value(rejected).await["error"]["type"], "rate_limit_error");
    assert_eq!(
        chat(&state, other.secret.expose_secret(), body.clone())
            .await
            .status(),
        StatusCode::OK
    );
    assert_eq!(
        chat(&state, "environment-key", body).await.status(),
        StatusCode::OK
    );
    let models = app::router(state.clone())
        .oneshot(
            Request::builder()
                .uri("/v1/models")
                .header(
                    "authorization",
                    format!("Bearer {}", key.secret.expose_secret()),
                )
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(models.status(), StatusCode::OK);
    assert_eq!(mock.hits.load(Ordering::SeqCst), 3);
    let page = app::router(state.clone())
        .oneshot(
            Request::builder()
                .uri("/api-keys")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let html = String::from_utf8(
        page.into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes()
            .to_vec(),
    )
    .unwrap();
    assert!(html.contains("Rejections in current 60-second window: 1"));
    assert!(!html.contains(key.secret.expose_secret()));
}
#[tokio::test]
async fn streaming_slots_survive_commit_and_release_on_drop_completion_and_error() {
    let state = state().await;
    let mock = seed(&state).await;
    let key = issue(&state, None, Some(1)).await;
    let secret = key.secret.expose_secret();
    let response = chat(
        &state,
        secret,
        json!({"model":"combo/chat", "stream":true, "hang":true}),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    // The body has not been polled by this client; the slot must already be held.
    assert_eq!(
        chat(&state, secret, json!({"model":"combo/chat"}))
            .await
            .status(),
        StatusCode::TOO_MANY_REQUESTS
    );
    assert_eq!(mock.hits.load(Ordering::SeqCst), 1);
    drop(response);
    let response = chat(&state, secret, json!({"model":"combo/chat", "stream":true})).await;
    assert_eq!(response.status(), StatusCode::OK);
    response.into_body().collect().await.unwrap();
    let response = chat(
        &state,
        secret,
        json!({"model":"combo/chat", "stream":true, "truncate":true}),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert!(response.into_body().collect().await.is_err());
    let response = chat(&state, secret, json!({"model":"combo/chat", "fail":true})).await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        chat(&state, secret, json!({"model":"combo/chat"}))
            .await
            .status(),
        StatusCode::OK
    );
    assert_eq!(mock.hits.load(Ordering::SeqCst), 5);
}
#[tokio::test]
async fn invalid_and_forbidden_requests_do_not_consume_quota_and_test_chat_shares_it() {
    let state = state().await;
    let mock = seed(&state).await;
    let key = issue(&state, Some(1), Some(1)).await;
    api_keys::update_policy(
        &state.db,
        &key.id,
        KeyAccess::Selected(vec!["combo/chat".into()]),
        Expiry::Keep,
    )
    .await
    .unwrap();
    let secret = key.secret.expose_secret();
    assert_eq!(
        chat(&state, secret, json!({"model":"combo/denied"}))
            .await
            .status(),
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        chat(&state, secret, json!({"model":"combo/chat","stream":"yes"}))
            .await
            .status(),
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        chat(&state, secret, json!({"model":"combo/chat"}))
            .await
            .status(),
        StatusCode::OK
    );
    let response = app::router(state.clone()).oneshot(Request::builder().method("POST").uri("/test/chat").header("authorization", format!("Bearer {secret}")).header("content-type", "application/json").body(Body::from(json!({"_csrf":state.csrf_token.as_str(),"model":"combo/chat","messages":[{"role":"user","content":"hello"}]}).to_string())).unwrap()).await.unwrap();
    assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    assert!(response.headers().contains_key("retry-after"));
    assert_eq!(mock.hits.load(Ordering::SeqCst), 1);
}
#[tokio::test]
async fn dashboard_limits_are_validated_and_edits_preserve_the_secret_and_active_slots() {
    let state = state().await;
    let _mock = seed(&state).await;
    let key = issue(&state, None, None).await;
    let secret = key.secret.expose_secret();
    let active = chat(
        &state,
        secret,
        json!({"model":"combo/chat","stream":true,"hang":true}),
    )
    .await;
    let form = |rpm: &str, concurrent: &str| {
        format!(
            "_csrf={}&access=all&expiry=keep&requests_per_minute={rpm}&max_concurrent={concurrent}",
            state.csrf_token
        )
    };
    for (rpm, concurrent) in [("0", "1"), ("1", "10001"), ("-1", "1"), ("abc", "1")] {
        let response = app::router(state.clone())
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(format!("/api-keys/{}/policy", key.id))
                    .header("content-type", "application/x-www-form-urlencoded")
                    .body(Body::from(form(rpm, concurrent)))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }
    let response = app::router(state.clone())
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/api-keys/{}/policy", key.id))
                .header("content-type", "application/x-www-form-urlencoded")
                .body(Body::from(form("", "1")))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    assert!(
        api_keys::matches(&state.db, &state.crypto, secret)
            .await
            .unwrap()
    );
    assert_eq!(
        api_keys::list(&state.db).await.unwrap()[0]
            .limits
            .max_concurrent,
        Some(1)
    );
    assert_eq!(
        chat(&state, secret, json!({"model":"combo/chat"}))
            .await
            .status(),
        StatusCode::TOO_MANY_REQUESTS
    );
    drop(active);
    assert_eq!(
        chat(&state, secret, json!({"model":"combo/chat"}))
            .await
            .status(),
        StatusCode::OK
    );
}

#[tokio::test]
async fn cancelling_precommit_request_releases_the_reservation() {
    let state = state().await;
    let mock = seed(&state).await;
    let key = issue(&state, None, Some(1)).await;
    let s = state.clone();
    let secret = key.secret.expose_secret().to_owned();
    let task =
        tokio::spawn(
            async move { chat(&s, &secret, json!({"model":"combo/chat", "wait":true})).await },
        );
    tokio::time::timeout(std::time::Duration::from_secs(5), mock.entered.notified())
        .await
        .unwrap();
    assert_eq!(
        chat(
            &state,
            key.secret.expose_secret(),
            json!({"model":"combo/chat"})
        )
        .await
        .status(),
        StatusCode::TOO_MANY_REQUESTS
    );
    assert_eq!(mock.hits.load(Ordering::SeqCst), 1);
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    assert_eq!(
        chat(
            &state,
            key.secret.expose_secret(),
            json!({"model":"combo/chat"})
        )
        .await
        .status(),
        StatusCode::OK
    );
}

#[tokio::test]
async fn create_form_saves_limits_and_requires_csrf() {
    let state = state().await;
    let form = |csrf: &str| {
        format!(
            "_csrf={csrf}&name=Limited&access=all&expiry=never&requests_per_minute=12&max_concurrent=2"
        )
    };
    for (csrf, expected) in [
        ("wrong", StatusCode::BAD_REQUEST),
        (state.csrf_token.as_str(), StatusCode::SEE_OTHER),
    ] {
        let response = app::router(state.clone())
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api-keys")
                    .header("content-type", "application/x-www-form-urlencoded")
                    .body(Body::from(form(csrf)))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), expected);
    }
    let keys = api_keys::list(&state.db).await.unwrap();
    assert_eq!(keys.len(), 1);
    assert_eq!(keys[0].limits.requests_per_minute, Some(12));
    assert_eq!(keys[0].limits.max_concurrent, Some(2));
}
