//! Integration tests for the credential-free configuration transfer pages
//! (`GET /configuration`, `POST /configuration/export`, `/preview`, and
//! `/import`).
//!
//! These drive `app::router(state).oneshot(request)` so the dashboard CSRF
//! and origin middleware layers run exactly as they will in production. The
//! round-trip test exports from a gateway under one master key and imports
//! into a gateway under a different one, proving the document carries no
//! ciphertext bound to the source key.

use axum::{
    Router,
    body::Body,
    http::{Request, StatusCode, header},
    response::Response,
};
use base64::{Engine, engine::general_purpose::STANDARD};
use http_body_util::BodyExt;
use secrecy::{ExposeSecret, SecretString};
use serde_json::{Value, json};
use tower::ServiceExt;

use sparkrouter::{
    app::{self, AppState},
    config::Config,
    crypto::Crypto,
    db,
    providers::Protocol,
};

const SOURCE_KEY: &[u8; 32] = &[1; 32];
const TARGET_KEY: &[u8; 32] = &[2; 32];

#[tokio::test]
async fn insert_failure_rolls_back_all_imported_rows_and_preserves_existing_credentials() {
    let source = state_with(SOURCE_KEY).await;
    seed_source(&source).await;
    let document = export_document(&source, &app::router(source.clone())).await;
    let target = state_with(TARGET_KEY).await;
    let protected = db::providers::Provider {
        id: "protected".into(),
        name: "Existing connection".into(),
        base_url: "https://api.openai.com/v1".into(),
        protocol: Protocol::OpenAi,
        encrypted_api_key: target
            .crypto
            .encrypt_api_key("protected", &SecretString::from("existing-secret-key"))
            .unwrap(),
        enabled: true,
    };
    db::providers::save(&target.db, &protected).await.unwrap();
    db::combos::create(&target.db, "existing").await.unwrap();
    let before =
        serde_json::to_string(&db::config_transfer::export(&target.db).await.unwrap()).unwrap();
    sqlx::query("CREATE TRIGGER reject_import_step BEFORE INSERT ON combo_steps WHEN NEW.position = 1 BEGIN SELECT RAISE(ABORT, 'test import failure'); END")
        .execute(&target.db).await.unwrap();
    let response = submit(
        &app::router(target.clone()),
        "/configuration/import",
        &target.csrf_token,
        &[("configuration", &document)],
    )
    .await;
    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    let after =
        serde_json::to_string(&db::config_transfer::export(&target.db).await.unwrap()).unwrap();
    assert_eq!(before, after);
    assert_eq!(
        db::providers::get(&target.db, "protected")
            .await
            .unwrap()
            .encrypted_api_key,
        protected.encrypted_api_key
    );
}

#[tokio::test]
async fn invalid_configuration_values_are_not_echoed_in_errors() {
    let state = state_with(TARGET_KEY).await;
    let router = app::router(state.clone());
    let marker = "secret-value-never-echo-this";
    let documents = [
        json!({"version":marker,"groups":[],"connections":[],"combos":[]}),
        json!({"version":1,"groups":[{"id":"g","name":"Group","base_url":"https://example.invalid/v1","protocol":marker}],"connections":[],"combos":[]}),
        json!({"version":1,"groups":[{"id":"g","name":"Group","base_url":format!("https://user:{marker}@example.invalid/v1"),"protocol":"openai"}],"connections":[],"combos":[]}),
    ];
    for document in documents {
        let text = document.to_string();
        for path in ["/configuration/preview", "/configuration/import"] {
            let response = submit(
                &router,
                path,
                &state.csrf_token,
                &[("configuration", &text)],
            )
            .await;
            assert_eq!(response.status(), StatusCode::BAD_REQUEST);
            assert!(!body_string(response).await.contains(marker));
        }
    }
}

fn config(master_key: &[u8; 32]) -> Config {
    Config {
        database_url: "sqlite::memory:".into(),
        bind: "127.0.0.1:0".parse().unwrap(),
        crypto: Crypto::from_base64(&SecretString::from(STANDARD.encode(master_key))).unwrap(),
        api_key: None,
        dashboard_password: None,
        dashboard_secure_cookie: false,
    }
}

async fn state_with(master_key: &[u8; 32]) -> AppState {
    AppState::from_config(config(master_key)).await.unwrap()
}

/// Submits a urlencoded form with the process CSRF token.
async fn submit(router: &Router, path: &str, csrf: &str, fields: &[(&str, &str)]) -> Response {
    submit_token(router, path, fields, csrf).await
}

/// Like [`submit`] but with an explicit CSRF token, for rejection coverage.
async fn submit_token(
    router: &Router,
    path: &str,
    fields: &[(&str, &str)],
    csrf: &str,
) -> Response {
    let mut url = reqwest::Url::parse("http://localhost/").unwrap();
    url.query_pairs_mut()
        .append_pair("_csrf", csrf)
        .extend_pairs(fields.iter().copied());
    router
        .clone()
        .oneshot(
            Request::post(path)
                .header("content-type", "application/x-www-form-urlencoded")
                .body(Body::from(url.query().unwrap().to_owned()))
                .unwrap(),
        )
        .await
        .unwrap()
}

async fn body_string(response: Response) -> String {
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

async fn get_page(router: &Router, path: &str) -> String {
    let response = router
        .clone()
        .oneshot(Request::get(path).body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK, "GET {path} must render");
    body_string(response).await
}

/// Seed a source gateway: one custom group with an enabled keyed connection
/// and a disabled one, an empty custom group, an ungrouped preset connection,
/// and a round-robin combo with ordered steps.
async fn seed_source(state: &AppState) {
    let work = db::provider_groups::create(
        &state.db,
        "Work <LLM>",
        "https://work.example/v1/",
        &Protocol::OpenAi,
    )
    .await
    .unwrap();
    db::provider_groups::create(
        &state.db,
        "Empty group",
        "https://empty.example/v1",
        &Protocol::Anthropic,
    )
    .await
    .unwrap();
    for (id, name, enabled) in [("src-1", "work key", true), ("src-2", "backup key", false)] {
        let provider = db::providers::Provider {
            id: id.into(),
            name: name.into(),
            base_url: work.base_url.clone(),
            protocol: work.protocol,
            encrypted_api_key: state
                .crypto
                .encrypt_api_key(id, &SecretString::from("sk-live-source-secret"))
                .unwrap(),
            enabled,
        };
        db::provider_groups::create_connection(&state.db, &work.id, &provider)
            .await
            .unwrap();
    }
    db::providers::save(
        &state.db,
        &db::providers::Provider {
            id: "src-3".into(),
            name: "personal preset".into(),
            base_url: "https://api.openai.com/v1".into(),
            protocol: Protocol::OpenAi,
            encrypted_api_key: state
                .crypto
                .encrypt_api_key("src-3", &SecretString::from("sk-live-preset-secret"))
                .unwrap(),
            enabled: true,
        },
    )
    .await
    .unwrap();
    // A direct-database ungrouped custom connection (the "old entries"
    // case): no membership, non-preset endpoint, no credential.
    db::providers::save(
        &state.db,
        &db::providers::Provider {
            id: "src-4".into(),
            name: "local noauth".into(),
            base_url: "http://127.0.0.1:9919/v1".into(),
            protocol: Protocol::OpenAi,
            encrypted_api_key: Vec::new(),
            enabled: true,
        },
    )
    .await
    .unwrap();
    let combo =
        db::combos::create_with_strategy(&state.db, "chain", db::combos::Strategy::RoundRobin)
            .await
            .unwrap();
    for (provider_id, model) in [("src-1", "model-one"), ("src-3", "model-two")] {
        db::combos::add_step(&state.db, &combo.id, provider_id, model)
            .await
            .unwrap();
    }
}

/// Export a seeded source gateway through the dashboard route.
async fn export_document(state: &AppState, router: &Router) -> String {
    let response = submit(router, "/configuration/export", &state.csrf_token, &[]).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok()),
        Some("application/json")
    );
    let disposition = response
        .headers()
        .get(header::CONTENT_DISPOSITION)
        .and_then(|value| value.to_str().ok())
        .expect("content-disposition header")
        .to_owned();
    assert!(
        disposition.starts_with("attachment; filename=\"sparkrouter-configuration"),
        "export is an attachment with a sanitized filename: {disposition}"
    );
    body_string(response).await
}

#[tokio::test]
async fn export_excludes_secrets_and_round_trips_under_a_new_master_key() {
    let source = state_with(SOURCE_KEY).await;
    let source_router = app::router(source.clone());
    seed_source(&source).await;
    let document = export_document(&source, &source_router).await;

    // The document is structural only: no credentials, no timestamps, no
    // gateway keys, no cached model data, no exact key material.
    for secret in ["sk-live-source-secret", "sk-live-preset-secret"] {
        assert!(
            !document.contains(secret),
            "export must not contain {secret}"
        );
    }
    for forbidden in [
        "encrypted",
        "api_key",
        "api-key",
        "created_at",
        "updated_at",
        "refreshed_at",
        "models",
    ] {
        assert!(
            !document.contains(forbidden),
            "export must not mention '{forbidden}'"
        );
    }

    // Strict shape: exactly the versioned DTO fields, nothing else.
    let value: Value = serde_json::from_str(&document).unwrap();
    let top: Vec<&str> = value
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(top, ["combos", "connections", "groups", "version"]);
    assert_eq!(value["version"], 1);
    assert_eq!(value["groups"].as_array().unwrap().len(), 2);
    let connections = value["connections"].as_array().unwrap();
    assert_eq!(connections.len(), 4);
    for connection in connections {
        let mut keys: Vec<&str> = connection
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            ["base_url", "enabled", "group_id", "id", "name", "protocol"]
        );
    }
    let combos = value["combos"].as_array().unwrap();
    assert_eq!(combos.len(), 1);
    assert_eq!(combos[0]["name"], "chain");
    assert_eq!(combos[0]["strategy"], "round_robin");
    assert_eq!(
        combos[0]["steps"]
            .as_array()
            .unwrap()
            .iter()
            .map(|step| step["model"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["model-one", "model-two"]
    );

    // Import into a gateway with a different master key.
    let target = state_with(TARGET_KEY).await;
    let target_router = app::router(target.clone());
    let response = submit(
        &target_router,
        "/configuration/import",
        &target.csrf_token,
        &[("configuration", document.as_str())],
    )
    .await;
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    assert_eq!(
        response
            .headers()
            .get(header::LOCATION)
            .and_then(|value| value.to_str().ok()),
        Some("/providers")
    );

    // Two distinct groups survive, including the empty one; the ungrouped
    // custom endpoint gains its own fresh group so it stays visible.
    let groups = db::provider_groups::list(&target.db).await.unwrap();
    assert_eq!(groups.len(), 3);
    let empty = groups
        .iter()
        .find(|summary| summary.group.name == "Empty group")
        .expect("empty group preserved");
    assert_eq!(empty.connection_count, 0);
    let work = groups
        .iter()
        .find(|summary| summary.group.name == "Work <LLM>")
        .expect("work group");
    assert_eq!(work.group.base_url, "https://work.example/v1");
    assert_ne!(work.group.id, empty.group.id);
    let solo = groups
        .iter()
        .find(|summary| summary.group.name == "local noauth")
        .expect("ungrouped custom endpoint gets its own group");
    assert_eq!(solo.connection_count, 1);
    assert_eq!(solo.group.base_url, "http://127.0.0.1:9919/v1");

    // Every imported connection is fresh, disabled, and credential-free,
    // regardless of the enabled flags in the document.
    let imported = db::providers::list(&target.db).await.unwrap();
    assert_eq!(imported.len(), 4);
    for connection in &imported {
        assert!(!connection.id.starts_with("src-"), "ids are fresh");
        assert!(!connection.enabled, "imports start disabled");
        assert!(
            connection.encrypted_api_key.is_empty(),
            "imports carry no credential"
        );
        let group = db::provider_groups::by_connection(&target.db, &connection.id)
            .await
            .unwrap();
        match connection.name.as_str() {
            "personal preset" => {
                assert!(
                    group.is_none(),
                    "ungrouped preset connections stay ungrouped"
                )
            }
            "local noauth" => assert_eq!(
                group.expect("grouped").id,
                solo.group.id,
                "ungrouped custom connections join their synthesized group"
            ),
            _ => assert_eq!(
                group.expect("grouped").id,
                work.group.id,
                "grouped connections rejoin their imported group"
            ),
        }
    }

    // Nothing is hidden: the synthesized group is listed on the Providers
    // page, linking to its group page so the connection can be reviewed,
    // keyed, and enabled.
    let index = get_page(&target_router, "/providers").await;
    assert!(
        index.contains(&format!("href=\"/providers/groups/{}\"", solo.group.id)),
        "the synthesized group for an imported custom endpoint must be listed"
    );
    assert!(index.contains("local noauth"));
    let group_page = get_page(
        &target_router,
        &format!("/providers/groups/{}", solo.group.id),
    )
    .await;
    assert!(group_page.contains("local noauth"));
    assert!(group_page.contains("http://127.0.0.1:9919/v1"));

    // The combo keeps its name, strategy, and ordered steps, now pointing at
    // the fresh connection ids.
    let combo = db::combos::by_name(&target.db, "chain").await.unwrap();
    assert_eq!(combo.strategy, db::combos::Strategy::RoundRobin);
    let steps = db::combos::steps(&target.db, &combo.id).await.unwrap();
    assert_eq!(
        steps
            .iter()
            .map(|step| step.model.as_str())
            .collect::<Vec<_>>(),
        ["model-one", "model-two"]
    );
    let imported_ids: std::collections::HashSet<&str> = imported
        .iter()
        .map(|connection| connection.id.as_str())
        .collect();
    assert!(
        steps
            .iter()
            .all(|step| imported_ids.contains(step.provider_id.as_str()))
    );
}

#[tokio::test]
async fn preview_shows_counts_warnings_and_a_confirmation_form() {
    let source = state_with(SOURCE_KEY).await;
    let source_router = app::router(source.clone());
    seed_source(&source).await;
    let document = export_document(&source, &source_router).await;

    let target = state_with(TARGET_KEY).await;
    let target_router = app::router(target.clone());

    // The plain page carries both forms and no confirmation form.
    let page = get_page(&target_router, "/configuration").await;
    assert!(page.contains("action=\"/configuration/export\""));
    assert!(page.contains("action=\"/configuration/preview\""));
    assert!(page.contains("name=\"_csrf\""));
    assert!(!page.contains("action=\"/configuration/import\""));

    // The preview shows counts, the disabled warning, and a confirmation
    // form echoing the original document and CSRF token.
    let response = submit(
        &target_router,
        "/configuration/preview",
        &target.csrf_token,
        &[("configuration", document.as_str())],
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let page = body_string(response).await;
    assert!(page.contains("Import preview"));
    assert!(page.contains("3 groups"));
    assert!(page.contains("4 connections"));
    assert!(page.contains("1 combos with 2 steps"));
    assert!(page.contains("starts disabled"));
    assert!(
        page.contains("stay visible on the Providers page"),
        "the preview warns about synthesized groups for custom endpoints"
    );
    assert!(page.contains("action=\"/configuration/import\""));
    assert!(page.contains("type=\"hidden\" name=\"configuration\""));
    // Names from the document are escaped; JSON quotes are entity-encoded.
    assert!(page.contains("Work &#60;LLM&#62;"));
    assert!(page.contains("&#34;version&#34;"));
    assert!(page.contains("Import configuration"));

    // Nothing was written by a preview.
    assert!(db::providers::list(&target.db).await.unwrap().is_empty());
    assert!(db::combos::list(&target.db).await.unwrap().is_empty());

    // A conflicting combo name turns the confirmation into a conflict list;
    // the import itself is rejected and writes nothing.
    db::combos::create(&target.db, "chain").await.unwrap();
    let response = submit(
        &target_router,
        "/configuration/preview",
        &target.csrf_token,
        &[("configuration", document.as_str())],
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let page = body_string(response).await;
    assert!(!page.contains("action=\"/configuration/import\""));
    assert!(page.contains("<code>chain</code>"));
    assert!(page.contains("already exists"));

    let response = submit(
        &target_router,
        "/configuration/import",
        &target.csrf_token,
        &[("configuration", document.as_str())],
    )
    .await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert!(
        db::providers::list(&target.db).await.unwrap().is_empty(),
        "conflicts abort the whole import"
    );
    assert_eq!(db::combos::list(&target.db).await.unwrap().len(), 1);

    // With the conflict removed, the same document imports cleanly.
    let chain = db::combos::by_name(&target.db, "chain").await.unwrap();
    db::combos::delete(&target.db, &chain.id).await.unwrap();
    let response = submit(
        &target_router,
        "/configuration/import",
        &target.csrf_token,
        &[("configuration", document.as_str())],
    )
    .await;
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    assert_eq!(db::providers::list(&target.db).await.unwrap().len(), 4);
}

#[tokio::test]
async fn invalid_documents_and_references_are_rejected_without_writing() {
    let state = state_with(TARGET_KEY).await;
    let router = app::router(state.clone());
    let valid = json!({
        "version": 1,
        "groups": [{"id": "g", "name": "Group", "base_url": "https://g.example/v1", "protocol": "openai"}],
        "connections": [{"id": "c", "name": "Conn", "base_url": "https://g.example/v1", "protocol": "openai", "group_id": "g", "enabled": true}],
        "combos": [{"name": "chain", "strategy": "priority", "steps": [{"provider_id": "c", "model": "m"}]}]
    });

    let broken = |value: Value| serde_json::to_string(&value).unwrap();
    let documents = [
        // Unknown and credential-carrying fields are rejected, not ignored.
        broken(
            json!({"version": 1, "groups": [], "connections": [], "combos": [], "api_keys": []}),
        ),
        broken({
            let mut document = valid.clone();
            document["connections"][0]["api_key"] = "sk-embedded-secret".into();
            document
        }),
        broken({
            let mut document = valid.clone();
            document["groups"][0]["encrypted_api_key"] = "AAECAw==".into();
            document
        }),
        // Wrong or missing version.
        broken(json!({"version": 2, "groups": [], "connections": [], "combos": []})),
        broken(json!({"groups": [], "connections": [], "combos": []})),
        // Dangling references.
        broken({
            let mut document = valid.clone();
            document["connections"][0]["group_id"] = "ghost".into();
            document
        }),
        broken({
            let mut document = valid.clone();
            document["combos"][0]["steps"][0]["provider_id"] = "ghost".into();
            document
        }),
        // A grouped connection must share its group's endpoint.
        broken({
            let mut document = valid.clone();
            document["connections"][0]["base_url"] = "https://other.example/v1".into();
            document
        }),
        // Duplicates and invalid values.
        broken({
            let mut document = valid.clone();
            let duplicate = document["connections"][0].clone();
            document["connections"]
                .as_array_mut()
                .unwrap()
                .push(duplicate);
            document
        }),
        broken({
            let mut document = valid.clone();
            let duplicate = document["combos"][0].clone();
            document["combos"].as_array_mut().unwrap().push(duplicate);
            document
        }),
        broken({
            let mut document = valid.clone();
            document["combos"][0]["strategy"] = "weighted".into();
            document
        }),
        broken({
            let mut document = valid.clone();
            document["connections"][0]["protocol"] = "carrier-pigeon".into();
            document
        }),
        broken({
            let mut document = valid.clone();
            document["groups"][0]["base_url"] = "https://user:pw@g.example/v1".into();
            document
        }),
        broken({
            let mut document = valid.clone();
            document["groups"][0]["name"] = "bad\u{7}name".into();
            document
        }),
        broken({
            let mut document = valid.clone();
            document["combos"][0]["steps"][0]["model"] = "   ".into();
            document
        }),
        // Malformed JSON entirely.
        "not json".to_owned(),
    ];

    for document in &documents {
        for path in ["/configuration/preview", "/configuration/import"] {
            let response = submit(
                &router,
                path,
                &state.csrf_token,
                &[("configuration", document.as_str())],
            )
            .await;
            assert_eq!(
                response.status(),
                StatusCode::BAD_REQUEST,
                "{path} must reject {document}"
            );
        }
    }
    assert!(db::providers::list(&state.db).await.unwrap().is_empty());
    assert!(
        db::provider_groups::list(&state.db)
            .await
            .unwrap()
            .is_empty()
    );
    assert!(db::combos::list(&state.db).await.unwrap().is_empty());

    // The minimal empty document is a valid no-op import.
    let empty = json!({"version": 1, "groups": [], "connections": [], "combos": []}).to_string();
    let response = submit(
        &router,
        "/configuration/import",
        &state.csrf_token,
        &[("configuration", empty.as_str())],
    )
    .await;
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
}

#[tokio::test]
async fn csrf_and_origin_checks_guard_every_transfer_action() {
    let state = state_with(TARGET_KEY).await;
    let router = app::router(state.clone());
    let document = json!({"version": 1, "groups": [], "connections": [], "combos": []}).to_string();

    // An invalid CSRF token is a 400 for export, preview, and import alike.
    for path in [
        "/configuration/export",
        "/configuration/preview",
        "/configuration/import",
    ] {
        let response = submit_token(
            &router,
            path,
            &[("configuration", document.as_str())],
            "definitely-not-the-token",
        )
        .await;
        assert_eq!(
            response.status(),
            StatusCode::BAD_REQUEST,
            "POST {path} must reject an invalid CSRF token"
        );
    }
    assert!(db::providers::list(&state.db).await.unwrap().is_empty());
    assert!(db::combos::list(&state.db).await.unwrap().is_empty());

    // A valid token still cannot come from a cross-origin form submission:
    // the dashboard origin middleware covers the transfer routes too.
    for (origin, fetch_site) in [
        (Some("http://evil.example"), None),
        (None, Some("cross-site")),
    ] {
        let mut url = reqwest::Url::parse("http://localhost/").unwrap();
        url.query_pairs_mut()
            .append_pair("_csrf", &state.csrf_token)
            .append_pair("configuration", &document);
        let mut request = Request::post("/configuration/import")
            .header("content-type", "application/x-www-form-urlencoded")
            .header("host", "localhost:9999")
            .body(Body::from(url.query().unwrap().to_owned()))
            .unwrap();
        if let Some(origin) = origin {
            request.headers_mut().insert(
                header::ORIGIN,
                header::HeaderValue::from_str(origin).unwrap(),
            );
        }
        if let Some(site) = fetch_site {
            request.headers_mut().insert(
                "sec-fetch-site",
                header::HeaderValue::from_str(site).unwrap(),
            );
        }
        let response = router.clone().oneshot(request).await.unwrap();
        assert_eq!(
            response.status(),
            StatusCode::BAD_REQUEST,
            "cross-origin submission ({origin:?}/{fetch_site:?}) must be rejected"
        );
    }
    assert!(db::combos::list(&state.db).await.unwrap().is_empty());

    // A same-origin submission with the right token still succeeds.
    let response = submit(
        &router,
        "/configuration/import",
        &state.csrf_token,
        &[("configuration", document.as_str())],
    )
    .await;
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
}

#[tokio::test]
async fn exports_are_empty_but_well_formed_on_a_fresh_gateway() {
    let state = state_with(SOURCE_KEY).await;
    let router = app::router(state.clone());
    let document = export_document(&state, &router).await;
    let value: Value = serde_json::from_str(&document).unwrap();
    assert_eq!(value["version"], 1);
    assert_eq!(value["groups"].as_array().unwrap().len(), 0);
    assert_eq!(value["connections"].as_array().unwrap().len(), 0);
    assert_eq!(value["combos"].as_array().unwrap().len(), 0);
}

// Guard the crypto helper assumptions used above: exported ciphertext under
// one master key must never be decryptable under another, which is why the
// transfer document omits ciphertext entirely.
#[test]
fn master_keys_do_not_share_ciphertext() {
    let source = Crypto::from_base64(&SecretString::from(STANDARD.encode(SOURCE_KEY))).unwrap();
    let target = Crypto::from_base64(&SecretString::from(STANDARD.encode(TARGET_KEY))).unwrap();
    let sealed = source
        .encrypt_api_key("src-1", &SecretString::from("sk-live-source-secret"))
        .unwrap();
    assert!(target.decrypt_api_key("src-1", &sealed).is_err());
    assert_eq!(
        source
            .decrypt_api_key("src-1", &sealed)
            .unwrap()
            .expose_secret(),
        "sk-live-source-secret"
    );
}
