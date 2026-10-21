use axum::{
    Json, Router,
    body::Body,
    http::{Request, StatusCode},
    routing::post,
};
use base64::{Engine, engine::general_purpose::STANDARD};
use http_body_util::BodyExt;
use secrecy::SecretString;
use serde_json::{Value, json};
use sparkrouter::{
    app::{self, AppState},
    config::Config,
    crypto::Crypto,
    db::{self, combos},
    providers::Protocol,
};
use tower::ServiceExt;

async fn state() -> AppState {
    AppState::from_config(Config {
        database_url: "sqlite::memory:".into(),
        bind: "127.0.0.1:0".parse().unwrap(),
        crypto: Crypto::from_base64(&SecretString::from(STANDARD.encode([38; 32]))).unwrap(),
        api_key: None,
        dashboard_login: sparkrouter::config::DashboardLogin::Disabled,
        dashboard_secure_cookie: false,
    })
    .await
    .unwrap()
}

async fn provider(state: &AppState, id: &str, url: &str, enabled: bool) {
    db::providers::save(
        &state.db,
        &db::providers::Provider {
            id: id.into(),
            name: format!("Connection <{id}>"),
            base_url: url.into(),
            protocol: Protocol::OpenAi,
            encrypted_api_key: vec![],
            enabled,
        },
    )
    .await
    .unwrap();
}

async fn submit(
    state: &AppState,
    path: &str,
    connection: &str,
    model: &str,
    csrf: &str,
    htmx: bool,
    cross_origin: bool,
) -> axum::response::Response {
    let mut encoded = reqwest::Url::parse("http://localhost").unwrap();
    encoded
        .query_pairs_mut()
        .append_pair("_csrf", csrf)
        .append_pair("provider_id", connection)
        .append_pair("model", model);
    let mut request = Request::builder()
        .method("POST")
        .uri(path)
        .header("content-type", "application/x-www-form-urlencoded");
    if htmx {
        request = request.header("hx-request", "true");
    }
    if cross_origin {
        request = request
            .header("origin", "https://foreign.invalid")
            .header("host", "localhost");
    }
    app::router(state.clone())
        .oneshot(
            request
                .body(Body::from(encoded.query().unwrap().to_owned()))
                .unwrap(),
        )
        .await
        .unwrap()
}

async fn text(response: axum::response::Response) -> String {
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
async fn edit_step_preserves_identity_order_strategy_and_routing_uses_new_target() {
    let state = state().await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let upstream = Router::new().route(
        "/chat/completions",
        post(|Json(body): Json<Value>| async move {
            Json(json!({"choices":[],"target":body["model"],"extra":body["extra"]}))
        }),
    );
    struct Task(tokio::task::JoinHandle<()>);
    impl Drop for Task {
        fn drop(&mut self) {
            self.0.abort();
        }
    }
    let _task = Task(tokio::spawn(async move {
        axum::serve(listener, upstream).await.unwrap();
    }));
    provider(&state, "old", "http://127.0.0.1:1", true).await;
    provider(&state, "new", &url, true).await;
    let combo = combos::create_with_strategy(&state.db, "editing", combos::Strategy::RoundRobin)
        .await
        .unwrap();
    for model in ["original", "backup", "last"] {
        combos::add_step(&state.db, &combo.id, "old", model)
            .await
            .unwrap();
    }
    let before = combos::steps(&state.db, &combo.id).await.unwrap();
    let created: String = sqlx::query_scalar("SELECT created_at FROM combos WHERE id = ?")
        .bind(&combo.id)
        .fetch_one(&state.db)
        .await
        .unwrap();
    sqlx::query("UPDATE combos SET updated_at = '2000-01-01' WHERE id = ?")
        .bind(&combo.id)
        .execute(&state.db)
        .await
        .unwrap();
    let path = format!("/combos/{}/steps/{}/edit", combo.id, before[0].id);
    let response = submit(
        &state,
        &path,
        "new",
        " new/model ",
        &state.csrf_token,
        false,
        false,
    )
    .await;
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    assert_eq!(
        response.headers()["location"],
        format!("/combos/{}", combo.id)
    );
    let after = combos::steps(&state.db, &combo.id).await.unwrap();
    assert_eq!(
        before
            .iter()
            .map(|s| (&s.id, s.position))
            .collect::<Vec<_>>(),
        after
            .iter()
            .map(|s| (&s.id, s.position))
            .collect::<Vec<_>>()
    );
    assert_eq!(after[0].provider_id, "new");
    assert_eq!(after[0].model, "new/model");
    assert_eq!(after[1].model, "backup");
    assert_eq!(after[2].model, "last");
    assert_eq!(
        combos::get(&state.db, &combo.id).await.unwrap().strategy,
        combos::Strategy::RoundRobin
    );
    let (created_after, updated): (String, String) =
        sqlx::query_as("SELECT created_at, updated_at FROM combos WHERE id = ?")
            .bind(&combo.id)
            .fetch_one(&state.db)
            .await
            .unwrap();
    assert_eq!(created, created_after);
    assert_ne!(updated, "2000-01-01");
    let response = app::router(state.clone())
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/chat/completions")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({"model":"combo/editing","extra":{"kept":true}}).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let result: Value = serde_json::from_str(&text(response).await).unwrap();
    assert_eq!(result["target"], "new/model");
    assert_eq!(result["extra"]["kept"], true);
}

#[tokio::test]
async fn edit_rejects_invalid_targets_wrong_combo_csrf_and_origin_without_changes() {
    let state = state().await;
    provider(&state, "a", "http://127.0.0.1:1", true).await;
    let combo = combos::create(&state.db, "original").await.unwrap();
    let other = combos::create(&state.db, "other").await.unwrap();
    combos::add_step(&state.db, &combo.id, "a", "kept")
        .await
        .unwrap();
    let step = combos::steps(&state.db, &combo.id).await.unwrap().remove(0);
    let path = format!("/combos/{}/steps/{}/edit", combo.id, step.id);
    for (connection, model) in [("a", " "), ("ghost", "changed"), ("a", &"x".repeat(257))] {
        assert_eq!(
            submit(
                &state,
                &path,
                connection,
                model,
                &state.csrf_token,
                false,
                false
            )
            .await
            .status(),
            StatusCode::BAD_REQUEST
        );
    }
    assert_eq!(
        submit(&state, &path, "a", "changed", "wrong", true, false)
            .await
            .status(),
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        submit(
            &state,
            &path,
            "a",
            "changed",
            &state.csrf_token,
            false,
            true
        )
        .await
        .status(),
        StatusCode::BAD_REQUEST
    );
    for owner in [&other.id, &"ghost".to_string()] {
        let path = format!("/combos/{owner}/steps/{}/edit", step.id);
        assert_eq!(
            submit(
                &state,
                &path,
                "a",
                "changed",
                &state.csrf_token,
                true,
                false
            )
            .await
            .status(),
            StatusCode::NOT_FOUND
        );
    }
    let path = format!("/combos/{}/steps/ghost/edit", combo.id);
    assert_eq!(
        submit(
            &state,
            &path,
            "a",
            "changed",
            &state.csrf_token,
            false,
            false
        )
        .await
        .status(),
        StatusCode::NOT_FOUND
    );
    let kept = combos::steps(&state.db, &combo.id).await.unwrap().remove(0);
    assert_eq!(kept.id, step.id);
    assert_eq!(kept.provider_id, "a");
    assert_eq!(kept.model, "kept");
    assert_eq!(kept.position, 0);
}

#[tokio::test]
async fn htmx_edit_returns_escaped_independent_forms_and_refreshed_model_suggestions() {
    let state = state().await;
    provider(&state, "a", "https://api.openai.com/v1", true).await;
    provider(&state, "b", "https://api.openai.com/v1", false).await;
    let combo = combos::create(&state.db, "ui").await.unwrap();
    for model in ["one", "two"] {
        combos::add_step(&state.db, &combo.id, "a", model)
            .await
            .unwrap();
    }
    let steps = combos::steps(&state.db, &combo.id).await.unwrap();
    db::connection_models::replace(
        &state.db,
        "b",
        &[db::connection_models::StoredModel {
            id: "suggested<&>".into(),
            name: "Suggested".into(),
            fields: vec![db::connection_models::ModelField {
                label: "Output modalities".into(),
                value: "text".into(),
            }],
            free: false,
        }],
        false,
    )
    .await
    .unwrap();
    let response = submit(
        &state,
        &format!("/combos/{}/steps/{}/edit", combo.id, steps[1].id),
        "b",
        "typed<&>\"",
        &state.csrf_token,
        true,
        false,
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let html = text(response).await;
    assert!(!html.contains("<html"));
    assert!(html.contains("typed&#60;&#38;&#62;&#34;"));
    assert!(html.contains("suggested&#60;&#38;&#62;"));
    assert!(html.contains("Connection &#60;b&#62; (disabled)"));
    assert_eq!(html.matches("data-step-picker").count(), 2);
    assert_eq!(html.matches("name=\"_csrf\"").count(), 8);
    for step in &steps {
        let toggle = html
            .find(&format!(
                "data-step-edit-toggle aria-controls=\"edit-step-{}\"",
                step.id
            ))
            .unwrap();
        let remove = html
            .find(&format!(
                "action=\"/combos/{}/steps/{}/remove\"",
                combo.id, step.id
            ))
            .unwrap();
        assert!(
            toggle < remove,
            "Edit must precede Remove in the action cell"
        );
        assert!(html[..toggle].ends_with("<button class=\"btn small\" type=\"button\" "));
        assert!(html.contains(&format!(
            "id=\"edit-step-{}\" class=\"step-edit-row\"",
            step.id
        )));
        assert_eq!(
            html.matches(&format!("id=\"model-{}\"", step.id)).count(),
            1
        );
        assert!(html.contains(&format!("id=\"models-{}-b\"", step.id)));
    }
    assert!(html.contains("value=\"b\" data-provider=\"preset:openai\" selected"));
    let page = app::router(state.clone())
        .oneshot(
            Request::builder()
                .uri(format!("/combos/{}", combo.id))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let page = text(page).await;
    assert_eq!(page.matches("data-step-picker").count(), 3);
    for control in ["provider_group", "provider_id", "model"] {
        assert_eq!(page.matches(&format!("id=\"{control}\"")).count(), 1);
        assert!(page.contains(&format!("for=\"{control}\"")));
    }
    for step in &steps {
        for prefix in ["group", "connection", "model"] {
            let id = format!("{prefix}-{}", step.id);
            assert_eq!(page.matches(&format!("id=\"{id}\"")).count(), 1);
            assert!(page.contains(&format!("for=\"{id}\"")));
        }
    }
    assert!(page.contains("id=\"models-b\""));
    let kept = combos::steps(&state.db, &combo.id).await.unwrap();
    assert_eq!(kept[0].model, "one");
    assert_eq!(kept[1].provider_id, "b");
    assert_eq!(kept[1].position, 1);
    let js = include_str!("../static/app.js");
    assert!(js.contains("htmx:afterSwap\", bindComboStepPickers"));
    assert!(js.contains("form.querySelector(\"[data-step-connection]\")"));
    assert!(js.contains("htmx:afterSwap\", bindComboStepEditors"));
    assert!(js.contains("button.setAttribute(\"aria-expanded\", String(!row.hidden))"));
}

#[tokio::test]
async fn failed_step_update_rolls_back_combo_touch_and_target() {
    let state = state().await;
    provider(&state, "a", "http://127.0.0.1:1", true).await;
    let combo = combos::create(&state.db, "rollback").await.unwrap();
    combos::add_step(&state.db, &combo.id, "a", "kept")
        .await
        .unwrap();
    let step = combos::steps(&state.db, &combo.id).await.unwrap().remove(0);
    sqlx::query("CREATE TRIGGER reject_touch BEFORE UPDATE ON combos BEGIN SELECT RAISE(ABORT, 'test touch failure'); END").execute(&state.db).await.unwrap();
    assert!(
        combos::update_step(&state.db, &combo.id, &step.id, "a", "changed")
            .await
            .is_err()
    );
    assert_eq!(
        combos::steps(&state.db, &combo.id).await.unwrap()[0].model,
        "kept"
    );
}
