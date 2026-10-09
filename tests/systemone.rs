//! Native SystemOne boundary, protocol isolation and local upstream fallback.

#[tokio::test]
async fn dashboard_systemone_testing_preserves_native_results_and_actual_trace() {
    let state = state().await;
    let first = spawn_upstream(Router::new().route(
        "/systemone",
        post(|| async { StatusCode::TOO_MANY_REQUESTS }),
    ))
    .await;
    let backup = spawn_upstream(Router::new().route(
        "/systemone",
        post(|headers: HeaderMap, Json(body): Json<Value>| async move {
            assert_eq!(headers["authorization"], "Bearer upstream-key");
            let mut expected = payload();
            expected["model"] = json!("jev-latest");
            assert_eq!(body, expected); // CSRF and chat fields must not enter evaluation JSON.
            ([("content-type", "application/json")], ANSWER)
        }),
    ))
    .await;
    for (id, mock) in [("first", &first), ("backup", &backup)] {
        save(&state, id, &mock.url(), Protocol::SystemOne).await;
    }
    let combo = combos::create_with_strategy(&state.db, "eval", combos::Strategy::RoundRobin)
        .await
        .unwrap();
    for id in ["first", "backup"] {
        combos::add_step(&state.db, &combo.id, id, "jev-latest")
            .await
            .unwrap();
    }
    let mut body = payload();
    body["_csrf"] = json!(state.csrf_token.as_str());
    let response = call(&state, "/test/systemone", body.clone(), Some("gateway-key")).await;
    assert_eq!(response.status(), StatusCode::OK);
    let value = json_body(response).await;
    assert_eq!(
        value["completion"],
        serde_json::from_str::<Value>(ANSWER).unwrap()
    );
    assert_eq!(value["attempts"][0]["outcome"], "retryable_failure");
    assert_eq!(value["attempts"][1]["provider_name"], "backup");
    assert_eq!(value["attempts"][1]["outcome"], "selected");
    assert_eq!(first.hits.count(), 1);
    assert_eq!(backup.hits.count(), 1);
    // API requests share the same round-robin reservation; next starts at backup.
    assert_eq!(
        call(&state, "/v1/systemone", payload(), Some("gateway-key"))
            .await
            .status(),
        StatusCode::OK
    );
    assert_eq!(first.hits.count(), 1);
    assert_eq!(backup.hits.count(), 2);
    let page = app::router(state.clone())
        .oneshot(Request::builder().uri("/test").body(Body::empty()).unwrap())
        .await
        .unwrap();
    let html = String::from_utf8(body_bytes(page).await.to_vec()).unwrap();
    for marker in [
        "id=\"test-mode\"",
        "value=\"systemone\"",
        "id=\"test-state\"",
        "id=\"test-questions\"",
        "id=\"test-attempt-list\"",
    ] {
        assert!(html.contains(marker));
    }
}

#[tokio::test]
async fn dashboard_systemone_rejects_invalid_csrf_auth_scope_origin_and_payload() {
    use secrecy::ExposeSecret;
    let state = state().await;
    let mock =
        spawn_upstream(Router::new().route("/systemone", post(|| async { StatusCode::OK }))).await;
    save(&state, "eval", &mock.url(), Protocol::SystemOne).await;
    let combo = combos::create(&state.db, "eval").await.unwrap();
    combos::add_step(&state.db, &combo.id, "eval", "jev-latest")
        .await
        .unwrap();
    assert_eq!(
        call(&state, "/test/systemone", payload(), Some("gateway-key"))
            .await
            .status(),
        StatusCode::BAD_REQUEST
    );
    let mut valid = payload();
    valid["_csrf"] = json!(state.csrf_token.as_str());
    assert_eq!(
        call(&state, "/test/systemone", valid.clone(), None)
            .await
            .status(),
        StatusCode::UNAUTHORIZED
    );
    let key = db::api_keys::create_with_policy(
        &state.db,
        &state.crypto,
        "denied",
        db::api_keys::KeyAccess::Selected(vec![]),
        db::api_keys::Expiry::Never,
    )
    .await
    .unwrap();
    assert_eq!(
        call(
            &state,
            "/test/systemone",
            valid.clone(),
            Some(key.secret.expose_secret())
        )
        .await
        .status(),
        StatusCode::FORBIDDEN
    );
    for (field, value) in [
        ("model", json!("typesafe/jev-latest")),
        ("state", json!(null)),
        ("questions", json!([])),
        ("stream", json!(true)),
    ] {
        let mut bad = valid.clone();
        bad[field] = value;
        assert_eq!(
            call(&state, "/test/systemone", bad, Some("gateway-key"))
                .await
                .status(),
            StatusCode::BAD_REQUEST
        );
    }
    let response = app::router(state.clone())
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/test/systemone")
                .header("host", "localhost")
                .header("origin", "https://evil.example")
                .header("content-type", "application/json")
                .header("authorization", "Bearer gateway-key")
                .body(Body::from(valid.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(mock.hits.count(), 0);
}

#[tokio::test]
async fn custom_systemone_groups_transfer_without_credentials() {
    let state = state().await;
    let group = db::provider_groups::create_with_slug(
        &state.db,
        "Evaluators",
        "http://localhost/v1",
        &Protocol::SystemOne,
        Some("evaluators"),
    )
    .await
    .unwrap();
    let provider = Provider {
        id: "custom-eval".into(),
        name: "Key".into(),
        base_url: group.base_url.clone(),
        protocol: Protocol::SystemOne,
        encrypted_api_key: vec![1, 2, 3],
        enabled: true,
    };
    db::provider_groups::create_connection(&state.db, &group.id, &provider)
        .await
        .unwrap();
    let exported = db::config_transfer::export(&state.db).await.unwrap();
    assert_eq!(exported.groups[0].protocol, "systemone");
    let target = self::state().await;
    db::config_transfer::apply(
        &target.db,
        &serde_json::to_string(&exported).unwrap(),
        &|_, _| false,
    )
    .await
    .unwrap();
    let groups = db::provider_groups::list(&target.db).await.unwrap();
    assert_eq!(groups[0].group.protocol, Protocol::SystemOne);
    let members = db::provider_groups::members(&target.db, &groups[0].group.id)
        .await
        .unwrap();
    assert!(!members[0].enabled);
    assert!(members[0].encrypted_api_key.is_empty());
}
mod common;

use axum::{
    Json, Router,
    body::Body,
    http::{HeaderMap, Request, StatusCode},
    response::IntoResponse,
    routing::{get, post},
};
use base64::{Engine, engine::general_purpose::STANDARD};
use common::{body_bytes, json_body, spawn_upstream};
use secrecy::SecretString;
use serde_json::{Value, json};
use sparkrouter::{
    app::{self, AppState},
    config::Config,
    crypto::Crypto,
    db::{self, combos, connection_models, providers::Provider},
    providers::{self, Protocol},
};
use tower::ServiceExt;

async fn state() -> AppState {
    AppState::from_config(Config {
        database_url: "sqlite::memory:".into(),
        bind: "127.0.0.1:0".parse().unwrap(),
        crypto: Crypto::from_base64(&SecretString::from(STANDARD.encode([47; 32]))).unwrap(),
        api_key: Some(SecretString::from("gateway-key")),
        dashboard_password: None,
        dashboard_secure_cookie: false,
    })
    .await
    .unwrap()
}

async fn save(state: &AppState, id: &str, url: &str, protocol: Protocol) -> Provider {
    let provider = Provider {
        id: id.into(),
        name: id.into(),
        base_url: url.into(),
        protocol,
        encrypted_api_key: state
            .crypto
            .encrypt_api_key(id, &SecretString::from("upstream-key"))
            .unwrap(),
        enabled: true,
    };
    db::providers::save(&state.db, &provider).await.unwrap();
    provider
}

fn payload() -> Value {
    json!({"model":"combo/eval", "state":{"text":"Help!"}, "questions":{
        "urgent":{"type":"noul","instructions":"Urgent?"},
        "team":{"type":"choice","instructions":{"ask":"Which team?"},"criteria":{"billing":null,"technical":["bugs"]}},
        "rating":{"type":"score","instructions":["Rate urgency"],"criteria":["low","high"]}
    },"extension":{"keep":[1,2]}})
}

async fn call(
    state: &AppState,
    path: &str,
    body: Value,
    key: Option<&str>,
) -> axum::response::Response {
    let mut request = Request::builder()
        .method("POST")
        .uri(path)
        .header("content-type", "application/json");
    if let Some(key) = key {
        request = request.header("authorization", format!("Bearer {key}"));
    }
    app::router(state.clone())
        .oneshot(request.body(Body::from(body.to_string())).unwrap())
        .await
        .unwrap()
}

#[tokio::test]
async fn systemone_shares_generated_key_admission_with_chat() {
    use db::api_keys::{Expiry, KeyAccess, KeyLimits};
    use secrecy::ExposeSecret;
    let state = state().await;
    let mock = spawn_upstream(Router::new().route(
        "/systemone",
        post(|| async { ([("content-type", "application/json")], ANSWER) }),
    ))
    .await;
    save(&state, "eval", &mock.url(), Protocol::SystemOne).await;
    let combo = combos::create(&state.db, "eval").await.unwrap();
    combos::add_step(&state.db, &combo.id, "eval", "jev-latest")
        .await
        .unwrap();
    let key = db::api_keys::create_with_limits(
        &state.db,
        &state.crypto,
        "limited",
        KeyAccess::All,
        Expiry::Never,
        KeyLimits {
            requests_per_minute: Some(1),
            max_concurrent: Some(1),
        },
    )
    .await
    .unwrap();
    let secret = key.secret.expose_secret();
    assert_eq!(
        call(
            &state,
            "/v1/systemone",
            json!({"model":"combo/eval"}),
            Some(secret)
        )
        .await
        .status(),
        StatusCode::BAD_REQUEST
    );
    let response = call(&state, "/v1/systemone", payload(), Some(secret)).await;
    assert_eq!(response.status(), StatusCode::OK);
    body_bytes(response).await;
    for path in ["/v1/systemone", "/v1/chat/completions", "/test/systemone"] {
        let mut body = payload();
        body["_csrf"] = json!(state.csrf_token.as_str());
        let response = call(&state, path, body, Some(secret)).await;
        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
        assert!(response.headers().contains_key("retry-after"));
    }
    assert_eq!(mock.hits.count(), 1);
}

const ANSWER: &str = "{\"model\":\"jev-1.13.0\",\"answers\":{\"urgent\":{\"type\":\"noul\",\"noul\":0.95}},\"usage\":{\"input_tokens\":296,\"output_tokens\":20},\"extension\":true}";

#[tokio::test]
async fn native_passthrough_direct_discovery_and_protocol_isolation() {
    let state = state().await;
    let mock = spawn_upstream(Router::new()
        .route("/systemone",post(|headers: HeaderMap, Json(body): Json<Value>| async move {
            assert_eq!(headers["authorization"],"Bearer upstream-key");
            assert!(!headers.contains_key("x-sparkrouter-session-id"));
            let mut expected = payload(); expected["model"] = json!("jev-latest");
            assert_eq!(body,expected);
            ([("content-type","application/json")],ANSWER)
        }))
        .route("/models",get(|| async { Json(json!({"models":[{"name":"jev-latest","description":"Flagship","release_date":"2026-01-01"},{"name":"jev-preview"}]})) }))).await;
    let provider = save(&state, "eval", &mock.url(), Protocol::SystemOne).await;
    let chat = spawn_upstream(Router::new().route(
        "/chat/completions",
        post(|| async { Json(json!({"choices":[]})) }),
    ))
    .await;
    save(&state, "chat", &chat.url(), Protocol::OpenAi).await;
    let combo = combos::create(&state.db, "eval").await.unwrap();
    combos::add_step(&state.db, &combo.id, "chat", "chat-model")
        .await
        .unwrap();
    combos::add_step(&state.db, &combo.id, "eval", "jev-latest")
        .await
        .unwrap();
    let response = call(&state, "/v1/systemone", payload(), Some("gateway-key")).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(body_bytes(response).await.as_ref(), ANSWER.as_bytes());
    assert_eq!(chat.hits.count(), 0);
    let listed = providers::list_models(&state.client, &state.crypto, &provider)
        .await
        .unwrap();
    assert_eq!(listed.models[0].id, "jev-latest");
    assert_eq!(listed.models.len(), 2);
    connection_models::replace(&state.db, "eval", &listed.models, false)
        .await
        .unwrap();
    let ids = connection_models::routable_model_ids(&state.db)
        .await
        .unwrap();
    let id = ids.iter().find(|id| id.ends_with("/jev-latest")).unwrap();
    let mut body = payload();
    body["model"] = json!(id);
    assert_eq!(
        call(&state, "/v1/systemone", body.clone(), Some("gateway-key"))
            .await
            .status(),
        StatusCode::OK
    );
    let before = mock.hits.count();
    assert_eq!(
        call(&state, "/v1/chat/completions", body, Some("gateway-key"))
            .await
            .status(),
        StatusCode::BAD_GATEWAY
    );
    assert_eq!(mock.hits.count(), before);
    assert_eq!(
        call(
            &state,
            "/v1/chat/completions",
            json!({"model":"combo/eval","messages":[]}),
            Some("gateway-key")
        )
        .await
        .status(),
        StatusCode::OK
    );
    assert_eq!(chat.hits.count(), 1);
}

#[tokio::test]
async fn retryable_and_invalid_success_fall_back_but_422_stops() {
    for status in [401, 429, 529, 422, 200] {
        let state = state().await;
        let first = spawn_upstream(Router::new().route(
            "/systemone",
            post(move || async move {
                (
                    StatusCode::from_u16(status).unwrap(),
                    Json(json!({"error":"secret upstream details"})),
                )
                    .into_response()
            }),
        ))
        .await;
        let backup = spawn_upstream(Router::new().route(
            "/systemone",
            post(|| async { ([("content-type", "application/json")], ANSWER) }),
        ))
        .await;
        save(&state, "first", &first.url(), Protocol::SystemOne).await;
        save(&state, "backup", &backup.url(), Protocol::SystemOne).await;
        let combo = combos::create(&state.db, "eval").await.unwrap();
        for id in ["first", "backup"] {
            combos::add_step(&state.db, &combo.id, id, "jev-latest")
                .await
                .unwrap();
        }
        let response = call(&state, "/v1/systemone", payload(), Some("gateway-key")).await;
        assert_eq!(first.hits.count(), 1);
        if status == 422 {
            assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
            assert_eq!(backup.hits.count(), 0);
            assert!(
                !json_body(response)
                    .await
                    .to_string()
                    .contains("secret upstream")
            );
        } else {
            assert_eq!(response.status(), StatusCode::OK);
            assert_eq!(backup.hits.count(), 1);
        }
    }
}

#[tokio::test]
async fn authentication_validation_and_scope_rejections_do_not_call_upstream() {
    let state = state().await;
    assert_eq!(
        call(&state, "/v1/systemone", payload(), None)
            .await
            .status(),
        StatusCode::UNAUTHORIZED
    );
    for bad in [
        json!({"model":"combo/eval","questions":{}}),
        json!({"model":"combo/eval","state":false,"questions":{}}),
        json!({"model":"combo/eval","state":"text","questions":[]}),
        json!({"model":"combo/eval","state":[],"questions":{},"stream":true}),
    ] {
        assert_eq!(
            call(&state, "/v1/systemone", bad, Some("gateway-key"))
                .await
                .status(),
            StatusCode::BAD_REQUEST
        );
    }
    let key = db::api_keys::create_with_policy(
        &state.db,
        &state.crypto,
        "scoped",
        db::api_keys::KeyAccess::Selected(vec![]),
        db::api_keys::Expiry::Never,
    )
    .await
    .unwrap();
    use secrecy::ExposeSecret;
    assert_eq!(
        call(
            &state,
            "/v1/systemone",
            payload(),
            Some(key.secret.expose_secret())
        )
        .await
        .status(),
        StatusCode::FORBIDDEN
    );
}
