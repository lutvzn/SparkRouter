//! Model authorization happens before routing for JSON, streaming and test chat.
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
        api_keys::{self, Expiry, KeyAccess},
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
        crypto: Crypto::from_base64(&SecretString::from(STANDARD.encode([32; 32]))).unwrap(),
        api_key: None,
        dashboard_login: sparkrouter::config::DashboardLogin::Disabled,
        dashboard_secure_cookie: false,
    })
    .await
    .unwrap()
}

struct Mock {
    url: String,
    hits: Arc<AtomicUsize>,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for Mock {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn mock() -> Mock {
    mock_with_status(StatusCode::OK).await
}

async fn mock_with_status(status: StatusCode) -> Mock {
    let hits = Arc::new(AtomicUsize::new(0));
    let counter = hits.clone();
    let router = Router::new().route(
        "/chat/completions",
        post(move |Json(body): Json<Value>| {
            let counter = counter.clone();
            async move {
                counter.fetch_add(1, Ordering::SeqCst);
                if !status.is_success() {
                    return (status, Json(json!({"error":"local mock failure"}))).into_response();
                }
                if body["stream"] == true {
                    (
                        [("content-type", "text/event-stream")],
                        "data: {\"choices\":[]}\n\ndata: [DONE]\n\n",
                    )
                        .into_response()
                } else {
                    Json(json!({"choices":[],"target":body["model"],"extra":body["extra"]}))
                        .into_response()
                }
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let task = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    Mock { url, hits, task }
}

#[tokio::test]
async fn allowing_a_combo_covers_ordered_fallback_but_not_its_direct_targets() {
    let state = state().await;
    let failing = mock_with_status(StatusCode::SERVICE_UNAVAILABLE).await;
    let healthy = mock().await;
    seed(&state, &healthy).await;
    db::providers::save(
        &state.db,
        &db::providers::Provider {
            id: "retry".into(),
            name: "Retry".into(),
            base_url: failing.url.clone(),
            protocol: Protocol::OpenAi,
            encrypted_api_key: vec![],
            enabled: true,
        },
    )
    .await
    .unwrap();
    let combo = db::combos::create(&state.db, "fallback").await.unwrap();
    db::combos::add_step(&state.db, &combo.id, "retry", "first-target")
        .await
        .unwrap();
    db::combos::add_step(&state.db, &combo.id, "provider", "direct-model")
        .await
        .unwrap();
    let issued = api_keys::create_with_policy(
        &state.db,
        &state.crypto,
        "App",
        KeyAccess::Selected(vec!["combo/fallback".into()]),
        Expiry::Never,
    )
    .await
    .unwrap();
    assert_eq!(
        request(
            &state,
            "/v1/chat/completions",
            issued.secret.expose_secret(),
            Some(json!({"model":"connection-provider/direct-model"}))
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    let (status, value) = request(
        &state,
        "/v1/chat/completions",
        issued.secret.expose_secret(),
        Some(json!({"model":"combo/fallback"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(value["target"], "direct-model");
    assert_eq!(failing.hits.load(Ordering::SeqCst), 1);
    assert_eq!(healthy.hits.load(Ordering::SeqCst), 1);
}

async fn seed(state: &AppState, upstream: &Mock) {
    let provider = db::providers::Provider {
        id: "provider".into(),
        name: "Provider".into(),
        base_url: upstream.url.clone(),
        protocol: Protocol::OpenAi,
        encrypted_api_key: vec![],
        enabled: true,
    };
    db::providers::save(&state.db, &provider).await.unwrap();
    for name in ["allowed", "denied", "allowed-copy"] {
        let combo = db::combos::create(&state.db, name).await.unwrap();
        db::combos::add_step(&state.db, &combo.id, "provider", "inner-model")
            .await
            .unwrap();
    }
    let models: Vec<_> = ["direct-model", "other-model"]
        .into_iter()
        .map(|id| db::connection_models::StoredModel {
            id: id.into(),
            name: id.into(),
            fields: vec![],
            free: false,
        })
        .collect();
    db::connection_models::replace(&state.db, "provider", &models, false)
        .await
        .unwrap();
}

async fn request(
    state: &AppState,
    path: &str,
    token: &str,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let mut builder = Request::builder()
        .uri(path)
        .header("authorization", format!("Bearer {token}"));
    let body = match body {
        Some(body) => {
            builder = builder
                .method("POST")
                .header("content-type", "application/json");
            Body::from(body.to_string())
        }
        None => Body::empty(),
    };
    let response = app::router(state.clone())
        .oneshot(builder.body(body).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let value = serde_json::from_slice(&bytes)
        .unwrap_or_else(|_| json!({"raw":String::from_utf8_lossy(&bytes)}));
    (status, value)
}

async fn form(
    state: &AppState,
    path: &str,
    fields: &[(&str, &str)],
    csrf: &str,
    origin: bool,
) -> axum::response::Response {
    let mut encoded = reqwest::Url::parse("http://localhost").unwrap();
    encoded.query_pairs_mut().append_pair("_csrf", csrf);
    for (name, value) in fields {
        encoded.query_pairs_mut().append_pair(name, value);
    }
    let mut builder = Request::builder()
        .method("POST")
        .uri(path)
        .header("content-type", "application/x-www-form-urlencoded");
    if origin {
        builder = builder
            .header("origin", "https://foreign.invalid")
            .header("host", "localhost");
    }
    app::router(state.clone())
        .oneshot(
            builder
                .body(Body::from(encoded.query().unwrap().to_owned()))
                .unwrap(),
        )
        .await
        .unwrap()
}

#[tokio::test]
async fn exact_scopes_filter_models_and_stop_unauthorized_json_streams_and_test_chat() {
    let state = state().await;
    let upstream = mock().await;
    seed(&state, &upstream).await;
    let issued = api_keys::create_with_policy(
        &state.db,
        &state.crypto,
        "App",
        KeyAccess::Selected(vec![
            "combo/allowed".into(),
            "connection-provider/direct-model".into(),
        ]),
        Expiry::Never,
    )
    .await
    .unwrap();
    let token = issued.secret.expose_secret();
    let (status, list) = request(&state, "/v1/models", token, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        list["data"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v["id"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["combo/allowed", "connection-provider/direct-model"]
    );
    for model in [
        "combo/denied",
        "combo/allowed-copy",
        "combo/Allowed",
        "connection-provider/other-model",
        "direct-model",
        "retry/first-target",
        "inner-model",
        "does-not-exist",
    ] {
        for streaming in [false, true] {
            let (status, error) = request(
                &state,
                "/v1/chat/completions",
                token,
                Some(json!({"model":model,"stream":streaming})),
            )
            .await;
            assert_eq!(status, StatusCode::FORBIDDEN);
            assert_eq!(error["error"]["type"], "permission_error");
            assert!(!error.to_string().contains(token));
        }
    }
    let (status, _) = request(&state,"/test/chat",token,Some(json!({"_csrf":state.csrf_token.as_str(),"model":"combo/denied","messages":[{"role":"user","content":"hello"}]}))).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(upstream.hits.load(Ordering::SeqCst), 0);
    assert_eq!(db::request_history::count(&state.db).await.unwrap(), 0);
    let (status, value) = request(
        &state,
        "/v1/chat/completions",
        token,
        Some(json!({"model":" combo/allowed ","extra":{"preserved":true}})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(value["target"], "inner-model");
    assert_eq!(value["extra"]["preserved"], true);
    let (status, _) = request(
        &state,
        "/v1/chat/completions",
        token,
        Some(json!({"model":"connection-provider/direct-model","stream":true})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (status, value) = request(&state,"/test/chat",token,Some(json!({"_csrf":state.csrf_token.as_str(),"model":"combo/allowed","messages":[{"role":"user","content":"hello"}]}))).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(value["attempts"].as_array().unwrap().len(), 1);
    assert_eq!(upstream.hits.load(Ordering::SeqCst), 3);
}

#[tokio::test]
async fn direct_model_scopes_are_isolated_by_provider_slug() {
    let state = state().await;
    let upstream = mock().await;
    seed(&state, &upstream).await;
    let other = db::providers::Provider {
        id: "other".into(),
        name: "Other".into(),
        base_url: upstream.url.clone(),
        protocol: Protocol::OpenAi,
        encrypted_api_key: vec![],
        enabled: true,
    };
    db::providers::save(&state.db, &other).await.unwrap();
    let group = db::provider_groups::create_with_slug(
        &state.db,
        "Other group",
        &upstream.url,
        &Protocol::OpenAi,
        Some("provider2"),
    )
    .await
    .unwrap();
    sqlx::query("INSERT INTO provider_group_connections (provider_id, group_id) VALUES (?, ?)")
        .bind("other")
        .bind(group.id)
        .execute(&state.db)
        .await
        .unwrap();
    for provider in ["provider", "other"] {
        db::connection_models::replace(
            &state.db,
            provider,
            &[db::connection_models::StoredModel {
                id: "openai/gpt-6-luna".into(),
                name: "model".into(),
                fields: vec![],
                free: false,
            }],
            false,
        )
        .await
        .unwrap();
    }
    let issued = api_keys::create_with_policy(
        &state.db,
        &state.crypto,
        "Only one provider",
        KeyAccess::Selected(vec!["connection-provider/openai/gpt-6-luna".into()]),
        Expiry::Never,
    )
    .await
    .unwrap();
    let (status, list) = request(&state, "/v1/models", issued.secret.expose_secret(), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        list["data"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m["id"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["connection-provider/openai/gpt-6-luna"]
    );
    assert_eq!(
        request(
            &state,
            "/v1/chat/completions",
            issued.secret.expose_secret(),
            Some(json!({"model":"provider2/openai/gpt-6-luna"}))
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        request(
            &state,
            "/v1/chat/completions",
            issued.secret.expose_secret(),
            Some(json!({"model":"openai/gpt-6-luna"}))
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(upstream.hits.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn expired_keys_fail_closed_while_environment_and_legacy_keys_are_unrestricted() {
    let mut state = state().await;
    let upstream = mock().await;
    seed(&state, &upstream).await;
    let issued = api_keys::create_with_policy(
        &state.db,
        &state.crypto,
        "Expired",
        KeyAccess::All,
        Expiry::AfterDays(1),
    )
    .await
    .unwrap();
    let token = issued.secret.expose_secret();
    let remaining: i64 =
        sqlx::query_scalar("SELECT expires_at - unixepoch() FROM api_keys WHERE id = ?")
            .bind(&issued.id)
            .fetch_one(&state.db)
            .await
            .unwrap();
    assert!((86398..=86400).contains(&remaining));
    sqlx::query("UPDATE api_keys SET expires_at = unixepoch() WHERE id = ?")
        .bind(&issued.id)
        .execute(&state.db)
        .await
        .unwrap();
    for supplied in [token, "", "wrong-key"] {
        assert_eq!(
            request(&state, "/v1/models", supplied, None).await.0,
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            request(
                &state,
                "/v1/chat/completions",
                supplied,
                Some(json!({"model":"combo/allowed","stream":true}))
            )
            .await
            .0,
            StatusCode::UNAUTHORIZED
        );
    }
    assert!(api_keys::any(&state.db).await.unwrap());
    assert!(
        !api_keys::matches(&state.db, &state.crypto, token)
            .await
            .unwrap()
    );
    assert!(api_keys::list(&state.db).await.unwrap()[0].expired);
    assert_eq!(upstream.hits.load(Ordering::SeqCst), 0);
    state.api_key = Some(Arc::new(SecretString::from("environment-unrestricted")));
    assert_eq!(
        request(&state, "/v1/models", "environment-unrestricted", None)
            .await
            .1["data"]
            .as_array()
            .unwrap()
            .len(),
        5
    );
    let legacy = api_keys::create(&state.db, &state.crypto, "Legacy")
        .await
        .unwrap();
    assert_eq!(
        request(&state, "/v1/models", legacy.secret.expose_secret(), None)
            .await
            .1["data"]
            .as_array()
            .unwrap()
            .len(),
        5
    );
    api_keys::delete(&state.db, &issued.id).await.unwrap();
    api_keys::delete(&state.db, &legacy.id).await.unwrap();
    state.api_key = None;
    assert_eq!(
        request(&state, "/v1/models", "placeholder", None).await.0,
        StatusCode::OK
    );
}

#[tokio::test]
async fn policy_edits_are_atomic_keep_secrets_and_empty_selected_never_means_all() {
    let state = state().await;
    let upstream = mock().await;
    seed(&state, &upstream).await;
    let issued = api_keys::create_with_policy(
        &state.db,
        &state.crypto,
        "App",
        KeyAccess::All,
        Expiry::AfterDays(7),
    )
    .await
    .unwrap();
    let before: (String, Vec<u8>, i64) =
        sqlx::query_as("SELECT key_hash, reveal_envelope, expires_at FROM api_keys WHERE id = ?")
            .bind(&issued.id)
            .fetch_one(&state.db)
            .await
            .unwrap();
    let path = format!("/api-keys/{}/policy", issued.id);
    let fields = [
        ("access", "selected"),
        (
            "models",
            "combo/allowed\nconnection-provider/direct-model\ncombo/allowed",
        ),
        ("expiry", "keep"),
    ];
    assert_eq!(
        form(&state, &path, &fields, "wrong", false).await.status(),
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        form(&state, &path, &fields, &state.csrf_token, true)
            .await
            .status(),
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        form(&state, &path, &fields, &state.csrf_token, false)
            .await
            .status(),
        StatusCode::SEE_OTHER
    );
    let after: (String, Vec<u8>, i64) =
        sqlx::query_as("SELECT key_hash, reveal_envelope, expires_at FROM api_keys WHERE id = ?")
            .bind(&issued.id)
            .fetch_one(&state.db)
            .await
            .unwrap();
    assert_eq!(before, after);
    // Missing access must not upgrade a selected key to unrestricted.
    assert_eq!(
        form(
            &state,
            &path,
            &[("expiry", "keep")],
            &state.csrf_token,
            false
        )
        .await
        .status(),
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        request(&state, "/v1/models", issued.secret.expose_secret(), None)
            .await
            .1["data"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    // Force a failure partway through replacing the allowlist; old rows/policy survive.
    sqlx::query("CREATE TRIGGER reject_scope BEFORE INSERT ON api_key_models WHEN NEW.model = 'bad-model' BEGIN SELECT RAISE(ABORT, 'test scope failure'); END").execute(&state.db).await.unwrap();
    assert!(
        api_keys::update_policy(
            &state.db,
            &issued.id,
            KeyAccess::Selected(vec!["bad-model".into()]),
            Expiry::Never
        )
        .await
        .is_err()
    );
    assert_eq!(
        request(&state, "/v1/models", issued.secret.expose_secret(), None)
            .await
            .1["data"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    let after_failure: (String, Vec<u8>, i64) =
        sqlx::query_as("SELECT key_hash, reveal_envelope, expires_at FROM api_keys WHERE id = ?")
            .bind(&issued.id)
            .fetch_one(&state.db)
            .await
            .unwrap();
    assert_eq!(before, after_failure);
    api_keys::update_policy(
        &state.db,
        &issued.id,
        KeyAccess::Selected(vec![]),
        Expiry::Never,
    )
    .await
    .unwrap();
    assert_eq!(
        request(&state, "/v1/models", issued.secret.expose_secret(), None)
            .await
            .1["data"],
        json!([])
    );
    assert_eq!(
        request(
            &state,
            "/v1/chat/completions",
            issued.secret.expose_secret(),
            Some(json!({"model":"combo/allowed"}))
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    api_keys::update_policy(&state.db, &issued.id, KeyAccess::All, Expiry::Never)
        .await
        .unwrap();
    assert!(
        api_keys::list(&state.db).await.unwrap()[0]
            .expires_at
            .is_none()
    );
    api_keys::delete(&state.db, &issued.id).await.unwrap();
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM api_key_models")
        .fetch_one(&state.db)
        .await
        .unwrap();
    assert_eq!(count, 0);
}

#[tokio::test]
async fn dashboard_creates_scoped_key_reveals_once_and_validates_policies() {
    let state = state().await;
    let fields = [
        ("name", "<App>"),
        ("access", "selected"),
        ("models", " combo/new \nmodel<&>"),
        ("expiry", "days"),
        ("expires_days", "30"),
    ];
    let response = form(&state, "/api-keys", &fields, &state.csrf_token, false).await;
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    let location = response.headers()["location"].to_str().unwrap().to_owned();
    let keys = api_keys::list(&state.db).await.unwrap();
    assert!(keys[0].access.allows("combo/new"));
    assert!(keys[0].expires_at.is_some());
    let reveal = app::router(state.clone())
        .oneshot(
            Request::builder()
                .uri(&location)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let html = String::from_utf8(
        reveal
            .into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes()
            .to_vec(),
    )
    .unwrap();
    assert!(html.contains("&#60;App&#62;"));
    assert!(html.contains("sk-"));
    let repeated = app::router(state.clone())
        .oneshot(
            Request::builder()
                .uri(&location)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let repeated = String::from_utf8(
        repeated
            .into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes()
            .to_vec(),
    )
    .unwrap();
    assert!(!repeated.contains("class=\"key-secret\""));
    let list = app::router(state.clone())
        .oneshot(
            Request::builder()
                .uri("/api-keys")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let list = String::from_utf8(
        list.into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes()
            .to_vec(),
    )
    .unwrap();
    assert!(list.contains("model&#60;&#38;&#62;"));
    assert!(list.contains("Selected models"));
    for fields in [
        vec![("name", "bad"), ("access", "all"), ("models", "combo/new")],
        vec![
            ("name", "bad"),
            ("access", "selected"),
            ("models", "combo/"),
        ],
        vec![("name", "bad"), ("expiry", "days"), ("expires_days", "0")],
        vec![
            ("name", "bad"),
            ("expiry", "days"),
            ("expires_days", "3651"),
        ],
        vec![("name", "bad"), ("access", "unknown")],
    ] {
        assert_eq!(
            form(&state, "/api-keys", &fields, &state.csrf_token, false)
                .await
                .status(),
            StatusCode::BAD_REQUEST
        );
    }
    assert_eq!(api_keys::list(&state.db).await.unwrap().len(), 1);
}

#[tokio::test]
async fn combo_rename_does_not_expand_name_based_permissions_and_restart_keeps_policy() {
    let directory = tempfile::tempdir().unwrap();
    let database_url = format!(
        "sqlite://{}",
        directory
            .path()
            .join("policy.db")
            .to_string_lossy()
            .replace('\\', "/")
    );
    let config = || Config {
        database_url: database_url.clone(),
        bind: "127.0.0.1:0".parse().unwrap(),
        crypto: Crypto::from_base64(&SecretString::from(STANDARD.encode([32; 32]))).unwrap(),
        api_key: None,
        dashboard_login: sparkrouter::config::DashboardLogin::Disabled,
        dashboard_secure_cookie: false,
    };
    let state = AppState::from_config(config()).await.unwrap();
    let combo = db::combos::create(&state.db, "old").await.unwrap();
    let issued = api_keys::create_with_policy(
        &state.db,
        &state.crypto,
        "App",
        KeyAccess::Selected(vec!["combo/old".into()]),
        Expiry::AfterDays(30),
    )
    .await
    .unwrap();
    let token = issued.secret.expose_secret().to_owned();
    db::combos::rename(&state.db, &combo.id, "new")
        .await
        .unwrap();
    assert_eq!(
        request(&state, "/v1/models", &token, None).await.1["data"],
        json!([])
    );
    assert_eq!(
        request(
            &state,
            "/v1/chat/completions",
            &token,
            Some(json!({"model":"combo/new"}))
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        request(
            &state,
            "/v1/chat/completions",
            &token,
            Some(json!({"model":"combo/old"}))
        )
        .await
        .0,
        StatusCode::NOT_FOUND
    );
    state.db.close().await;
    drop(state);
    let state = AppState::from_config(config()).await.unwrap();
    assert!(
        api_keys::matches(&state.db, &state.crypto, &token)
            .await
            .unwrap()
    );
    let keys = api_keys::list(&state.db).await.unwrap();
    assert!(keys[0].access.allows("combo/old"));
    assert!(!keys[0].access.allows("combo/new"));
    assert!(keys[0].expires_at.is_some());
    state.db.close().await;
}

#[tokio::test]
async fn dashboard_regenerates_a_key_secret_once_and_keeps_its_policy() {
    let state = state().await;
    let issued = api_keys::create_with_policy(
        &state.db,
        &state.crypto,
        "<Client>",
        KeyAccess::Selected(vec!["combo/kept".into()]),
        Expiry::Never,
    )
    .await
    .unwrap();
    let old = issued.secret.expose_secret().to_owned();
    assert_eq!(
        request(&state, "/v1/models", &old, None).await.0,
        StatusCode::OK
    );

    let path = format!("/api-keys/{}/regenerate", issued.id);
    assert_eq!(
        form(&state, &path, &[], "wrong", false).await.status(),
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        form(&state, &path, &[], &state.csrf_token, true)
            .await
            .status(),
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        request(&state, "/v1/models", &old, None).await.0,
        StatusCode::OK,
        "rejected regenerations leave the secret unchanged"
    );
    assert_eq!(
        form(
            &state,
            "/api-keys/missing/regenerate",
            &[],
            &state.csrf_token,
            false
        )
        .await
        .status(),
        StatusCode::NOT_FOUND
    );

    let response = form(&state, &path, &[], &state.csrf_token, false).await;
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    let location = response.headers()["location"].to_str().unwrap().to_owned();
    assert_eq!(location, format!("/api-keys/{}?regenerated=1", issued.id));

    let page = |state: AppState, uri: String| async move {
        let response = app::router(state)
            .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
            .await
            .unwrap();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        String::from_utf8(bytes.to_vec()).unwrap()
    };
    let html = page(state.clone(), location.clone()).await;
    assert!(html.contains("<h1>API key regenerated</h1>"));
    assert!(html.contains("&#60;Client&#62;"));
    let start = html.find("class=\"key-secret\">").expect("secret shown") + 19;
    let new = html[start..start + html[start..].find('<').unwrap()].to_owned();
    assert!(new.starts_with("sk-") && new != old);
    assert!(
        !page(state.clone(), location)
            .await
            .contains("class=\"key-secret\"")
    );

    assert_eq!(
        request(&state, "/v1/models", &old, None).await.0,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        request(&state, "/v1/models", &new, None).await.0,
        StatusCode::OK
    );
    let keys = api_keys::list(&state.db).await.unwrap();
    assert_eq!(keys.len(), 1);
    assert_eq!(keys[0].id, issued.id);
    assert!(keys[0].access.allows("combo/kept"));
    assert!(
        page(state.clone(), "/api-keys".into())
            .await
            .contains(&path)
    );
}
