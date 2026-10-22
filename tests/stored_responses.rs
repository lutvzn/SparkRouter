//! Stored Responses API calls end to end: retrieve, input items, cancel and
//! delete reach only the connection that created the response, only for the
//! caller that created it; `store: false` is not remembered; streamed
//! responses are remembered from `response.created`; and
//! `previous_response_id` starts at the connection holding that response.

mod common;
use common::{json_body, spawn_upstream};

use axum::{
    Json, Router,
    body::Body,
    extract::{Path, RawQuery},
    http::{Request, StatusCode},
    response::IntoResponse,
    routing::{get, post},
};
use base64::{Engine, engine::general_purpose::STANDARD};
use secrecy::{ExposeSecret, SecretString};
use serde_json::{Value, json};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use tower::ServiceExt;

use sparkrouter::{
    app::{self, AppState},
    config::{Config, DashboardLogin},
    crypto::Crypto,
    db::{
        api_keys::{self, Expiry, KeyAccess},
        combos::{self, Category, Strategy},
        providers as provider_db,
    },
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

/// A Responses upstream; `name` prefixes its response IDs.
async fn upstream(name: &'static str, failing: Arc<AtomicBool>) -> common::MockUpstream {
    spawn_upstream(
        Router::new()
            .route(
                "/responses",
                post(move |Json(body): Json<Value>| {
                    let failing = failing.clone();
                    async move {
                        if failing.load(Ordering::SeqCst) {
                            return StatusCode::SERVICE_UNAVAILABLE.into_response();
                        }
                        let store = body["store"] != false;
                        if body["stream"] == true {
                            let wire = format!(
                                "event: response.created\ndata: {}\n\nevent: response.completed\ndata: {}\n\n",
                                json!({"type":"response.created", "response":{"id":format!("resp_{name}_stream"), "output":[], "store":store}}),
                                json!({"type":"response.completed", "response":{"id":format!("resp_{name}_stream"), "output":[]}}),
                            );
                            return ([("content-type", "text/event-stream")], wire).into_response();
                        }
                        Json(json!({"id":format!("resp_{name}"), "object":"response", "output":[],
                            "store":store, "previous":body["previous_response_id"]}))
                        .into_response()
                    }
                }),
            )
            .route(
                "/responses/{id}",
                get(move |Path(id): Path<String>| async move {
                    Json(json!({"id":id, "served_by":name, "output":[]}))
                })
                .delete(|Path(id): Path<String>| async move {
                    Json(json!({"id":id, "deleted":true}))
                }),
            )
            .route(
                "/responses/{id}/cancel",
                post(|Path(id): Path<String>| async move {
                    Json(json!({"id":id, "status":"cancelled"}))
                }),
            )
            .route(
                "/responses/{id}/input_items",
                get(|RawQuery(query): RawQuery| async move {
                    Json(json!({"data":[], "query":query}))
                }),
            ),
    )
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

async fn send(state: &AppState, request: Request<Body>) -> axum::response::Response {
    app::router(state.clone()).oneshot(request).await.unwrap()
}

fn create(key: &str, body: Value) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri("/v1/responses")
        .header("authorization", format!("Bearer {key}"))
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap()
}

fn call(method: &str, key: &str, path: &str) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(path)
        .header("authorization", format!("Bearer {key}"))
        .body(Body::empty())
        .unwrap()
}

#[tokio::test]
async fn stored_responses_are_reached_through_the_connection_that_made_them() {
    let state = test_state().await;
    let first_failing = Arc::new(AtomicBool::new(true));
    let first = upstream("first", first_failing.clone()).await;
    let second = upstream("second", Arc::default()).await;
    save_provider(&state, "first", &first.url()).await;
    save_provider(&state, "second", &second.url()).await;
    let combo = combos::create_typed(
        &state.db,
        "coding",
        Category::Text,
        Strategy::Priority,
        Default::default(),
    )
    .await
    .unwrap();
    for provider in ["first", "second"] {
        combos::add_step(&state.db, &combo.id, provider, "m")
            .await
            .unwrap();
    }
    let owner = api_keys::create(&state.db, &state.crypto, "owner")
        .await
        .unwrap();
    let other = api_keys::create(&state.db, &state.crypto, "other")
        .await
        .unwrap();
    let (owner_key, other_key) = (owner.secret.expose_secret(), other.secret.expose_secret());

    // The first connection is down, so the second stores the response.
    let created = send(
        &state,
        create(owner_key, json!({"model":"combo/coding", "input":"hi"})),
    )
    .await;
    assert_eq!(created.status(), StatusCode::OK);
    assert_eq!(json_body(created).await["id"], "resp_second");
    state.flush_history().await;
    let (first_hits, second_hits) = (first.hits.count(), second.hits.count());

    let retrieved = send(&state, call("GET", owner_key, "/v1/responses/resp_second")).await;
    assert_eq!(retrieved.status(), StatusCode::OK);
    assert_eq!(json_body(retrieved).await["served_by"], "second");
    let items = send(
        &state,
        call(
            "GET",
            owner_key,
            "/v1/responses/resp_second/input_items?limit=2&order=asc",
        ),
    )
    .await;
    assert_eq!(json_body(items).await["query"], "limit=2&order=asc");
    let cancelled = send(
        &state,
        call("POST", owner_key, "/v1/responses/resp_second/cancel"),
    )
    .await;
    assert_eq!(json_body(cancelled).await["status"], "cancelled");
    assert_eq!(first.hits.count(), first_hits);
    assert_eq!(second.hits.count(), second_hits + 3);

    // Another caller cannot see it, and invalid IDs never reach an upstream.
    for (key, path) in [
        (other_key, "/v1/responses/resp_second"),
        (owner_key, "/v1/responses/resp_unknown"),
        (owner_key, "/v1/responses/resp%20second"),
    ] {
        assert_eq!(
            send(&state, call("GET", key, path)).await.status(),
            StatusCode::NOT_FOUND,
            "{path}"
        );
    }
    assert_eq!(
        send(
            &state,
            call("GET", owner_key, "/v1/responses/resp_second?x=a;b")
        )
        .await
        .status(),
        StatusCode::BAD_REQUEST
    );
    assert_eq!(second.hits.count(), second_hits + 3);

    // `previous_response_id` starts at the connection holding the response,
    // even though the healthy first step would otherwise go first.
    first_failing.store(false, Ordering::SeqCst);
    let continued = send(
        &state,
        create(
            owner_key,
            json!({"model":"combo/coding", "input":"more", "previous_response_id":"resp_second"}),
        ),
    )
    .await;
    assert_eq!(json_body(continued).await["previous"], "resp_second");
    assert_eq!(first.hits.count(), first_hits);
    // For a caller who did not create it, the normal order applies.
    let unrelated = send(
        &state,
        create(
            other_key,
            json!({"model":"combo/coding", "input":"x", "previous_response_id":"resp_second"}),
        ),
    )
    .await;
    assert_eq!(json_body(unrelated).await["id"], "resp_first");

    // `store: false` responses are not remembered; streamed ones are.
    let unstored = send(
        &state,
        create(
            owner_key,
            json!({"model":"combo/coding", "input":"x", "store":false}),
        ),
    )
    .await;
    assert_eq!(json_body(unstored).await["id"], "resp_first");
    let streamed = send(
        &state,
        create(
            owner_key,
            json!({"model":"combo/coding", "input":"x", "stream":true}),
        ),
    )
    .await;
    assert_eq!(streamed.status(), StatusCode::OK);
    common::body_bytes(streamed).await;
    state.flush_history().await;
    assert_eq!(
        send(&state, call("GET", owner_key, "/v1/responses/resp_first"))
            .await
            .status(),
        StatusCode::NOT_FOUND
    );
    let retrieved = send(
        &state,
        call("GET", owner_key, "/v1/responses/resp_first_stream"),
    )
    .await;
    assert_eq!(json_body(retrieved).await["served_by"], "first");

    // A key narrowed afterwards loses access to responses of other models.
    api_keys::update_policy(
        &state.db,
        &owner.id,
        KeyAccess::Selected(vec!["combo/other".into()]),
        Expiry::Keep,
    )
    .await
    .unwrap();
    assert_eq!(
        send(&state, call("GET", owner_key, "/v1/responses/resp_second"))
            .await
            .status(),
        StatusCode::FORBIDDEN
    );
    api_keys::update_policy(&state.db, &owner.id, KeyAccess::All, Expiry::Keep)
        .await
        .unwrap();

    // Deleting forwards once and forgets the response.
    let deleted = send(
        &state,
        call("DELETE", owner_key, "/v1/responses/resp_second"),
    )
    .await;
    assert_eq!(json_body(deleted).await["deleted"], true);
    state.flush_history().await;
    let hits = second.hits.count();
    assert_eq!(
        send(&state, call("GET", owner_key, "/v1/responses/resp_second"))
            .await
            .status(),
        StatusCode::NOT_FOUND
    );
    assert_eq!(second.hits.count(), hits);
}
