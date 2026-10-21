//! Optional per-combo placement affinity, using only local mock upstreams.
mod common;

use axum::{
    Json, Router,
    body::Body,
    http::{HeaderMap, Request, StatusCode},
    response::{IntoResponse, Response},
    routing::post,
};
use base64::{Engine, engine::general_purpose::STANDARD};
use bytes::Bytes;
use common::{MockUpstream, body_bytes, json_body, spawn_upstream};
use http_body_util::BodyExt;
use secrecy::{ExposeSecret, SecretString};
use serde_json::{Value, json};
use sparkrouter::{
    app::{self, AppState},
    config::Config,
    crypto::Crypto,
    db::{
        self,
        combos::{self, AffinityMode, AffinityOptions, Strategy},
        providers::Provider,
    },
    providers::Protocol,
};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicU16, Ordering},
};
use tower::ServiceExt;

async fn state() -> AppState {
    state_at("sqlite::memory:").await
}

async fn state_at(database_url: &str) -> AppState {
    AppState::from_config(Config {
        database_url: database_url.into(),
        bind: "127.0.0.1:0".parse().unwrap(),
        crypto: Crypto::from_base64(&SecretString::from(STANDARD.encode([41; 32]))).unwrap(),
        api_key: None,
        dashboard_login: sparkrouter::config::DashboardLogin::Disabled,
        dashboard_secure_cookie: false,
    })
    .await
    .unwrap()
}

struct Mock {
    server: MockUpstream,
    status: Arc<AtomicU16>,
    received: Arc<Mutex<Vec<Value>>>,
    release: Arc<tokio::sync::Notify>,
}

async fn mock() -> Mock {
    let status = Arc::new(AtomicU16::new(200));
    let received = Arc::new(Mutex::new(Vec::new()));
    let release = Arc::new(tokio::sync::Notify::new());
    let (s, r, gate) = (status.clone(), received.clone(), release.clone());
    let server = spawn_upstream(Router::new().route("/chat/completions", post(move |headers: HeaderMap, Json(body): Json<Value>| {
        let (s, r, gate) = (s.clone(), r.clone(), gate.clone());
        async move {
            assert!(!headers.contains_key("session-id"));
            assert!(!headers.contains_key("x-sparkrouter-session-id"));
            r.lock().unwrap().push(body.clone());
            let status = StatusCode::from_u16(s.load(Ordering::SeqCst)).unwrap();
            if !status.is_success() { return status.into_response(); }
            if body["stream"] == true {
                let mode = body["mock_stream"].as_str().unwrap_or("complete").to_owned();
                let stream = async_stream::stream! {
                    yield Ok::<_, std::io::Error>(Bytes::from_static(b"data: {\"choices\":[{\"delta\":{\"content\":\"hello\"}}]}\n\n"));
                    if mode != "complete" { gate.notified().await; }
                    if mode == "broken" { yield Err(std::io::Error::other("local simulated interruption")); }
                    else { yield Ok(Bytes::from_static(b"data: [DONE]\n\n")); }
                };
                return ([("content-type", "text/event-stream")], Body::from_stream(stream)).into_response();
            }
            Json(json!({"model":body["model"],"choices":[{"message":{"role":"assistant","content":"ok"}}]})).into_response()
        }
    }))).await;
    Mock {
        server,
        status,
        received,
        release,
    }
}

async fn setup(mode: AffinityMode) -> (AppState, combos::Combo, Mock, Mock) {
    let state = state().await;
    let a = mock().await;
    let b = mock().await;
    for (id, mock) in [("a", &a), ("b", &b)] {
        db::providers::save(
            &state.db,
            &Provider {
                id: id.into(),
                name: id.into(),
                base_url: mock.server.url(),
                protocol: Protocol::OpenAi,
                encrypted_api_key: vec![],
                enabled: true,
            },
        )
        .await
        .unwrap();
    }
    let combo = combos::create_with_options(
        &state.db,
        "test",
        Strategy::RoundRobin,
        AffinityOptions {
            mode,
            sticky_minutes: 7,
        },
    )
    .await
    .unwrap();
    combos::add_step(&state.db, &combo.id, "a", "model-a")
        .await
        .unwrap();
    combos::add_step(&state.db, &combo.id, "b", "model-b")
        .await
        .unwrap();
    (state, combo, a, b)
}

fn prompt(text: &str) -> Value {
    json!({"model":"combo/test","messages":[{"role":"system","content":"rules"},{"role":"developer","content":"more rules"},{"role":"user","content":text}],"tools":[{"type":"function","function":{"name":"lookup","parameters":{"type":"object"}}}],"unknown":{"keep":[1,2]}})
}

async fn request(
    state: &AppState,
    body: Value,
    session: Option<&str>,
    key: Option<&str>,
) -> Response {
    let mut request =
        Request::post("/v1/chat/completions").header("content-type", "application/json");
    if let Some(session) = session {
        request = request.header("session-id", session);
    }
    if let Some(key) = key {
        request = request.header("authorization", format!("Bearer {key}"));
    }
    app::router(state.clone())
        .oneshot(request.body(Body::from(body.to_string())).unwrap())
        .await
        .unwrap()
}

async fn selected(
    state: &AppState,
    body: Value,
    session: Option<&str>,
    key: Option<&str>,
) -> String {
    let response = request(state, body, session, key).await;
    assert_eq!(response.status(), StatusCode::OK);
    json_body(response).await["model"]
        .as_str()
        .unwrap()
        .to_owned()
}

#[tokio::test]
async fn off_and_basic_without_ids_keep_round_robin_basic_matches_explicit_ids() {
    for mode in [AffinityMode::Off, AffinityMode::Basic] {
        let (state, _, a, b) = setup(mode).await;
        let session = if mode == AffinityMode::Off {
            Some("ignored")
        } else {
            None
        };
        assert_eq!(
            selected(&state, prompt("same"), session, None).await,
            "model-a"
        );
        assert_eq!(
            selected(&state, prompt("same"), session, None).await,
            "model-b"
        );
        assert_eq!((a.server.hits.count(), b.server.hits.count()), (1, 1));
    }
    let (state, _, a, b) = setup(AffinityMode::Basic).await;
    assert_eq!(
        selected(&state, prompt("first"), Some("s1"), None).await,
        "model-a"
    );
    assert_eq!(
        selected(&state, prompt("different content"), Some("s1"), None).await,
        "model-a"
    );
    // Hits still reserve one normal round-robin turn; third reservation is A.
    assert_eq!(
        selected(&state, prompt("new"), Some("s2"), None).await,
        "model-a"
    );
    let mut body = prompt("body-session");
    body["metadata"] = json!({"session_id":"s2"});
    assert_eq!(selected(&state, body, None, None).await, "model-a");
    assert_eq!((a.server.hits.count(), b.server.hits.count()), (4, 0));
    assert_eq!(
        a.received.lock().unwrap()[0]["unknown"],
        json!({"keep":[1,2]})
    );
}

#[tokio::test]
async fn smart_tracks_growing_history_but_distinguishes_initial_messages_and_tools() {
    let (state, _, a, b) = setup(AffinityMode::Smart).await;
    let body = prompt("first question");
    assert_eq!(selected(&state, body.clone(), None, None).await, "model-a");
    let mut next = body.clone();
    next["messages"].as_array_mut().unwrap().extend([
        json!({"role":"assistant","content":"answer"}),
        json!({"role":"user","content":"follow up"}),
    ]);
    assert_eq!(selected(&state, next.clone(), None, None).await, "model-a");
    next["tools"][0]["function"]["name"] = json!("changed");
    assert_eq!(selected(&state, next.clone(), None, None).await, "model-a");
    next["messages"][0]["content"] = json!("different rules");
    assert_eq!(selected(&state, next, None, None).await, "model-b");
    assert_eq!(
        selected(&state, prompt("different first question"), None, None).await,
        "model-a"
    );
    assert_eq!((a.server.hits.count(), b.server.hits.count()), (4, 1));
}

#[tokio::test]
async fn sticky_ignores_sessions_and_content_and_is_per_combo() {
    let (state, _, a, b) = setup(AffinityMode::Sticky).await;
    assert_eq!(
        selected(&state, prompt("first"), Some("s1"), None).await,
        "model-a"
    );
    assert_eq!(
        selected(&state, prompt("totally different"), Some("s2"), None).await,
        "model-a"
    );
    let other = combos::create_with_options(
        &state.db,
        "other",
        Strategy::Priority,
        AffinityOptions {
            mode: AffinityMode::Sticky,
            sticky_minutes: 1,
        },
    )
    .await
    .unwrap();
    combos::add_step(&state.db, &other.id, "b", "other-model")
        .await
        .unwrap();
    let mut body = prompt("another combo");
    body["model"] = json!("combo/other");
    assert_eq!(
        selected(&state, body, Some("s1"), None).await,
        "other-model"
    );
    assert_eq!((a.server.hits.count(), b.server.hits.count()), (2, 1));
}

#[tokio::test]
async fn generated_keys_have_separate_affinity_and_denied_calls_cannot_change_it() {
    let (state, _, a, b) = setup(AffinityMode::Basic).await;
    let k1 = db::api_keys::create(&state.db, &state.crypto, "one")
        .await
        .unwrap();
    let k2 = db::api_keys::create(&state.db, &state.crypto, "two")
        .await
        .unwrap();
    let denied = db::api_keys::create_with_policy(
        &state.db,
        &state.crypto,
        "denied",
        db::api_keys::KeyAccess::Selected(vec![]),
        db::api_keys::Expiry::Never,
    )
    .await
    .unwrap();
    assert_eq!(
        selected(
            &state,
            prompt("q"),
            Some("shared"),
            Some(k1.secret.expose_secret())
        )
        .await,
        "model-a"
    );
    assert_eq!(
        request(
            &state,
            prompt("q"),
            Some("shared"),
            Some(denied.secret.expose_secret())
        )
        .await
        .status(),
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        selected(
            &state,
            prompt("q"),
            Some("shared"),
            Some(k2.secret.expose_secret())
        )
        .await,
        "model-b"
    );
    assert_eq!(
        selected(
            &state,
            prompt("q"),
            Some("shared"),
            Some(k1.secret.expose_secret())
        )
        .await,
        "model-a"
    );
    assert_eq!(
        selected(
            &state,
            prompt("q"),
            Some("shared"),
            Some(k2.secret.expose_secret())
        )
        .await,
        "model-b"
    );
    assert_eq!((a.server.hits.count(), b.server.hits.count()), (2, 2));
}

#[tokio::test]
async fn retryable_affinity_failure_rebinds_to_backup_permanent_errors_stop() {
    let (state, _, a, b) = setup(AffinityMode::Basic).await;
    assert_eq!(
        selected(&state, prompt("q"), Some("s"), None).await,
        "model-a"
    );
    a.status.store(503, Ordering::SeqCst);
    assert_eq!(
        selected(&state, prompt("q"), Some("s"), None).await,
        "model-b"
    );
    a.status.store(200, Ordering::SeqCst);
    assert_eq!(
        selected(&state, prompt("q"), Some("s"), None).await,
        "model-b"
    );
    b.status.store(400, Ordering::SeqCst);
    assert_eq!(
        request(&state, prompt("q"), Some("s"), None).await.status(),
        StatusCode::BAD_REQUEST
    );
    assert_eq!((a.server.hits.count(), b.server.hits.count()), (2, 3));
}

#[tokio::test]
async fn disabled_and_edited_targets_do_not_reuse_stale_affinity() {
    let (state, combo, a, b) = setup(AffinityMode::Basic).await;
    assert_eq!(
        selected(&state, prompt("q"), Some("s"), None).await,
        "model-a"
    );
    let mut provider = db::providers::get(&state.db, "a").await.unwrap();
    provider.enabled = false;
    db::providers::save(&state.db, &provider).await.unwrap();
    assert_eq!(
        selected(&state, prompt("q"), Some("s"), None).await,
        "model-b"
    );
    provider.enabled = true;
    db::providers::save(&state.db, &provider).await.unwrap();
    let steps = combos::steps(&state.db, &combo.id).await.unwrap();
    combos::update_step(&state.db, &combo.id, &steps[0].id, "a", "new-model")
        .await
        .unwrap();
    // A changed routing revision invalidates B's hint; current cursor starts A.
    assert_eq!(
        selected(&state, prompt("q"), Some("s"), None).await,
        "new-model"
    );
    assert_eq!((a.server.hits.count(), b.server.hits.count()), (2, 1));
}

#[tokio::test]
async fn streams_only_remember_completed_bodies_and_never_fail_over_after_commit() {
    for mode in ["complete", "broken", "pending"] {
        let (state, _, a, b) = setup(AffinityMode::Sticky).await;
        let mut body = prompt("q");
        body["stream"] = json!(true);
        body["mock_stream"] = json!(mode);
        let response = request(&state, body, None, None).await;
        assert_eq!(response.status(), StatusCode::OK);
        match mode {
            "complete" => {
                let bytes = body_bytes(response).await;
                assert!(bytes.ends_with(b"data: [DONE]\n\n"));
            }
            "broken" => {
                let mut body = response.into_body();
                let first = body.frame().await.unwrap().unwrap().into_data().unwrap();
                assert!(!first.is_empty());
                a.release.notify_one();
                assert!(body.collect().await.is_err());
            }
            _ => drop(response),
        }
        assert_eq!(b.server.hits.count(), 0, "no post-commit fallback");
        assert_eq!(
            selected(&state, prompt("next"), None, None).await,
            if mode == "complete" {
                "model-a"
            } else {
                "model-b"
            }
        );
        assert_eq!(
            a.server.hits.count(),
            if mode == "complete" { 2 } else { 1 }
        );
    }
}

#[tokio::test]
async fn affinity_works_without_body_logging_validates_ids_and_is_shared_with_test_chat() {
    let (state, _, a, b) = setup(AffinityMode::Basic).await;
    db::settings::set_logging_mode(&state.db, db::settings::LoggingMode::Off)
        .await
        .unwrap();
    assert_eq!(
        selected(&state, prompt("q"), Some("page-chat"), None).await,
        "model-a"
    );
    let body = json!({"_csrf":state.csrf_token.as_str(),"model":"combo/test","messages":[{"role":"user","content":"follow up"}]});
    let response = app::router(state.clone())
        .oneshot(
            Request::post("/test/chat")
                .header("content-type", "application/json")
                .header("x-sparkrouter-session-id", "page-chat")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(json_body(response).await["completion"]["model"], "model-a");
    let private_id = "sensitive-id-".repeat(30);
    let response = request(&state, prompt("q"), Some(&private_id), None).await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let bytes = body_bytes(response).await;
    assert!(!String::from_utf8_lossy(&bytes).contains("sensitive-id"));
    let mut body = prompt("q");
    body["session_id"] = json!(123);
    assert_eq!(
        request(&state, body, None, None).await.status(),
        StatusCode::BAD_REQUEST
    );
    assert_eq!((a.server.hits.count(), b.server.hits.count()), (2, 0));
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM request_history")
        .fetch_one(&state.db)
        .await
        .unwrap();
    assert_eq!(count, 0);
}

#[tokio::test]
async fn configuration_persists_on_restart_but_placement_hints_do_not() {
    let directory = tempfile::tempdir().unwrap();
    let database_url = format!(
        "sqlite://{}",
        directory.path().join("affinity.db").display()
    );
    let state = state_at(&database_url).await;
    db::settings::set_logging_mode(&state.db, db::settings::LoggingMode::Off)
        .await
        .unwrap();
    let a = mock().await;
    let b = mock().await;
    for (id, mock) in [("a", &a), ("b", &b)] {
        db::providers::save(
            &state.db,
            &Provider {
                id: id.into(),
                name: id.into(),
                base_url: mock.server.url(),
                protocol: Protocol::OpenAi,
                encrypted_api_key: vec![],
                enabled: true,
            },
        )
        .await
        .unwrap();
    }
    let options = AffinityOptions {
        mode: AffinityMode::Sticky,
        sticky_minutes: 12,
    };
    let combo = combos::create_with_options(&state.db, "test", Strategy::RoundRobin, options)
        .await
        .unwrap();
    combos::add_step(&state.db, &combo.id, "a", "model-a")
        .await
        .unwrap();
    combos::add_step(&state.db, &combo.id, "b", "model-b")
        .await
        .unwrap();
    a.status.store(503, Ordering::SeqCst);
    assert_eq!(selected(&state, prompt("q"), None, None).await, "model-b");
    a.status.store(200, Ordering::SeqCst);
    assert_eq!(selected(&state, prompt("q"), None, None).await, "model-b");
    state.db.close().await;
    drop(state);
    let restarted = state_at(&database_url).await;
    assert_eq!(
        combos::get(&restarted.db, &combo.id)
            .await
            .unwrap()
            .affinity,
        options
    );
    assert_eq!(
        selected(&restarted, prompt("q"), None, None).await,
        "model-a"
    );
    restarted.db.close().await;
}

async fn form(state: &AppState, path: &str, fields: &[(&str, &str)], csrf: &str) -> Response {
    let mut url = reqwest::Url::parse("http://localhost").unwrap();
    url.query_pairs_mut()
        .append_pair("_csrf", csrf)
        .extend_pairs(fields.iter().copied());
    app::router(state.clone())
        .oneshot(
            Request::post(path)
                .header("content-type", "application/x-www-form-urlencoded")
                .body(Body::from(url.query().unwrap().to_owned()))
                .unwrap(),
        )
        .await
        .unwrap()
}

#[tokio::test]
async fn per_combo_dropdown_validation_atomic_edits_duplication_and_transfer() {
    let state = state().await;
    let page = app::router(state.clone())
        .oneshot(Request::get("/combos/new").body(Body::empty()).unwrap())
        .await
        .unwrap();
    let html = String::from_utf8(body_bytes(page).await.to_vec()).unwrap();
    assert!(html.contains("<select id=\"affinity_mode\" name=\"affinity_mode\""));
    for (mode, label) in [
        ("off", "Off"),
        ("basic", "Basic"),
        ("smart", "Smart"),
        ("sticky", "Sticky"),
    ] {
        assert!(html.contains(&format!("value=\"{mode}\"")));
        assert!(html.contains(&format!(">{label}</option>")));
        let response = form(
            &state,
            "/combos",
            &[
                ("name", mode),
                ("affinity_mode", mode),
                ("sticky_minutes", "12"),
            ],
            &state.csrf_token,
        )
        .await;
        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        let combo = combos::by_name(&state.db, mode).await.unwrap();
        assert_eq!(combo.affinity.mode.as_str(), mode);
        assert_eq!(combo.affinity.sticky_minutes, 12);
        let copy = combos::duplicate(&state.db, &combo.id).await.unwrap();
        assert_eq!(copy.affinity, combo.affinity);
    }
    let combo = combos::by_name(&state.db, "smart").await.unwrap();
    let path = format!("/combos/{}", combo.id);
    for (mode, minutes) in [
        ("bad", "5"),
        ("sticky", "0"),
        ("sticky", "1441"),
        ("sticky", "-1"),
        ("sticky", "oops"),
    ] {
        assert_eq!(
            form(
                &state,
                &path,
                &[
                    ("name", "renamed"),
                    ("affinity_mode", mode),
                    ("sticky_minutes", minutes)
                ],
                &state.csrf_token
            )
            .await
            .status(),
            StatusCode::BAD_REQUEST
        );
        let unchanged = combos::get(&state.db, &combo.id).await.unwrap();
        assert_eq!(unchanged.name, "smart");
        assert_eq!(unchanged.affinity, combo.affinity);
    }
    assert_eq!(
        form(
            &state,
            &path,
            &[("name", "renamed"), ("affinity_mode", "off")],
            "wrong-csrf"
        )
        .await
        .status(),
        StatusCode::BAD_REQUEST
    );
    // Legacy name-only edits preserve both affinity fields.
    assert_eq!(
        form(&state, &path, &[("name", "renamed")], &state.csrf_token)
            .await
            .status(),
        StatusCode::SEE_OTHER
    );
    assert_eq!(
        combos::get(&state.db, &combo.id).await.unwrap().affinity,
        combo.affinity
    );
    let exported = db::config_transfer::export(&state.db).await.unwrap();
    let json = serde_json::to_string(&exported).unwrap();
    let destination = self::state().await;
    db::config_transfer::apply(&destination.db, &json, &|_, _| false)
        .await
        .unwrap();
    assert_eq!(
        combos::by_name(&destination.db, "renamed")
            .await
            .unwrap()
            .affinity,
        combo.affinity
    );
    // Existing v1 transfer files remain accepted with Off + 5 minutes.
    let legacy=json!({"version":1,"groups":[],"connections":[],"combos":[{"name":"legacy","strategy":"priority","steps":[]}]}).to_string();
    db::config_transfer::apply(&destination.db, &legacy, &|_, _| false)
        .await
        .unwrap();
    assert_eq!(
        combos::by_name(&destination.db, "legacy")
            .await
            .unwrap()
            .affinity,
        AffinityOptions::default()
    );
}
