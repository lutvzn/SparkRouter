mod common;
use axum::{
    Json, Router,
    body::Body,
    http::{Request, StatusCode, header},
    routing::post,
};
use base64::{Engine, engine::general_purpose::STANDARD};
use common::{body_bytes, json_body, spawn_upstream};
use secrecy::{ExposeSecret, SecretString};
use serde_json::{Value, json};
use sparkrouter::{
    app::{self, AppState},
    config::Config,
    crypto::Crypto,
    db::{
        self,
        combos::{self, AffinityOptions, Category, Strategy},
        connection_models::{ModelField, StoredModel},
    },
    providers::Protocol,
};
use tower::ServiceExt;

async fn state() -> AppState {
    AppState::from_config(Config {
        database_url: "sqlite::memory:".into(),
        bind: "127.0.0.1:0".parse().unwrap(),
        crypto: Crypto::from_base64(&SecretString::from(STANDARD.encode([71; 32]))).unwrap(),
        api_key: None,
        dashboard_login: sparkrouter::config::DashboardLogin::Disabled,
        dashboard_secure_cookie: false,
        metrics_token: None,
    })
    .await
    .unwrap()
}
async fn provider(state: &AppState, id: &str, url: &str, protocol: Protocol) {
    db::providers::save(
        &state.db,
        &db::providers::Provider {
            id: id.into(),
            name: id.into(),
            base_url: url.into(),
            protocol,
            encrypted_api_key: state
                .crypto
                .encrypt_api_key(id, &SecretString::from("fake-provider-key"))
                .unwrap(),
            enabled: true,
        },
    )
    .await
    .unwrap();
}
async fn typed(state: &AppState, name: &str, category: Category) -> combos::Combo {
    combos::create_typed(
        &state.db,
        name,
        category,
        Strategy::Priority,
        AffinityOptions::default(),
    )
    .await
    .unwrap()
}
async fn call(
    state: &AppState,
    path: &str,
    body: Value,
    key: Option<&str>,
) -> axum::response::Response {
    let mut request = Request::post(path).header(header::CONTENT_TYPE, "application/json");
    if let Some(key) = key {
        request = request.header(header::AUTHORIZATION, format!("Bearer {key}"));
    }
    app::router(state.clone())
        .oneshot(request.body(Body::from(body.to_string())).unwrap())
        .await
        .unwrap()
}
async fn form(state: &AppState, path: &str, fields: &str) -> axum::response::Response {
    app::router(state.clone())
        .oneshot(
            Request::post(path)
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .body(Body::from(format!("_csrf={}&{fields}", state.csrf_token)))
                .unwrap(),
        )
        .await
        .unwrap()
}
async fn get(state: &AppState, path: &str) -> String {
    let response = app::router(state.clone())
        .oneshot(Request::get(path).body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    String::from_utf8(body_bytes(response).await.to_vec()).unwrap()
}

#[tokio::test]
async fn documented_catalog_is_exact_scoped_and_used_by_mutations_and_import() {
    let state = state().await;
    for (id, url, protocol, ids) in [
        (
            "o",
            "https://api.openai.com/v1",
            Protocol::OpenAi,
            vec![
                "gpt-image-1",
                "tts-1",
                "whisper-1",
                "gpt-4o",
                "gpt-image-future",
                "sora-2",
                "nested/gpt-image-1",
            ],
        ),
        (
            "e",
            "http://localhost:1",
            Protocol::ElevenLabs,
            vec!["eleven_multilingual_v2", "scribe_v2", "music_v2_5"],
        ),
        (
            "m",
            "http://localhost:1",
            Protocol::MiniMax,
            vec![
                "image-01",
                "MiniMax-Hailuo-2.3",
                "music-3.0",
                "speech-2.8-hd",
                "asr-1.0",
            ],
        ),
        (
            "f",
            "http://localhost:1",
            Protocol::Fal,
            vec![
                "fal-ai/flux/dev",
                "fal-ai/wan/v2.2-a14b/text-to-video",
                "fal-ai/ace-step",
            ],
        ),
        (
            "custom",
            "http://localhost:1",
            Protocol::OpenAi,
            vec!["gpt-image-1"],
        ),
    ] {
        provider(&state, id, url, protocol).await;
        let models: Vec<_> = ids
            .into_iter()
            .map(|id| StoredModel {
                id: id.into(),
                name: id.into(),
                fields: vec![],
                free: false,
            })
            .collect();
        db::connection_models::replace(&state.db, id, &models, false)
            .await
            .unwrap();
    }
    let text = typed(&state, "catalog-text", Category::Text).await;
    let images = typed(&state, "catalog-image", Category::Image).await;
    let music = typed(&state, "catalog-music", Category::Music).await;
    let speech = typed(&state, "catalog-speech", Category::Tts).await;
    let stt = typed(&state, "catalog-stt", Category::Stt).await;
    let video = typed(&state, "catalog-video", Category::Video).await;
    let image_html = get(&state, &format!("/combos/{}", images.id)).await;
    for id in ["gpt-image-1", "image-01", "fal-ai/flux/dev"] {
        assert!(image_html.contains(&format!("<option value=\"{id}\"></option>")));
    }
    for id in ["tts-1", "gpt-image-future", "sora-2", "nested/gpt-image-1"] {
        assert!(!image_html.contains(&format!("<option value=\"{id}\"></option>")));
    }
    // Only the real OpenAI catalog, not the custom same-name model, is suggested.
    assert_eq!(
        image_html
            .matches("<option value=\"gpt-image-1\"></option>")
            .count(),
        1
    );
    for (combo, expected) in [
        (&text, "gpt-4o"),
        (&music, "music_v2_5"),
        (&speech, "eleven_multilingual_v2"),
        (&stt, "whisper-1"),
        (&video, "MiniMax-Hailuo-2.3"),
    ] {
        let html = get(&state, &format!("/combos/{}", combo.id)).await;
        assert!(html.contains(&format!("<option value=\"{expected}\"></option>")));
    }
    assert!(
        combos::add_step(&state.db, &text.id, "o", "gpt-image-1")
            .await
            .is_err()
    );
    assert!(
        combos::add_step(&state.db, &music.id, "e", "scribe_v2")
            .await
            .is_err()
    );
    combos::add_step(&state.db, &images.id, "o", "gpt-image-1")
        .await
        .unwrap();
    // Reusing a documented vendor ID on a custom service is not a global alias.
    combos::add_step(&state.db, &text.id, "custom", "gpt-image-1")
        .await
        .unwrap();
    let invalid = json!({"version":1,"groups":[],"connections":[{"id":"c","name":"c","base_url":"https://api.openai.com/v1","protocol":"openai","group_id":null,"enabled":false}],"combos":[{"name":"wrong","category":"text","strategy":"priority","steps":[{"provider_id":"c","model":"gpt-image-1"}]}]});
    assert!(
        db::config_transfer::apply(&state.db, &invalid.to_string(), &|_, _| false)
            .await
            .is_err()
    );
    assert!(combos::by_name(&state.db, "wrong").await.is_err());
}

#[tokio::test]
async fn explicit_endpoints_classify_all_modes_and_reject_unsupported_or_changed_capabilities() {
    let state = state().await;
    provider(&state, "p", "http://localhost:1", Protocol::OpenAi).await;
    provider(&state, "d", "http://localhost:1", Protocol::SystemOne).await;
    for (category, endpoint, label) in [
        (Category::Text, "/v1/chat/completions", "SupportedEndpoints"),
        (Category::Decision, "systemone", "Supported endpoints"),
        (Category::Image, "images-generations", "Supported endpoints"),
        (Category::Video, "videos", "Supported_endpoints"),
        (
            Category::Music,
            "/v1/music/generations",
            "Supported endpoints",
        ),
        (Category::Tts, "audio-speech", "Supported endpoints"),
        (Category::Stt, "audio-transcriptions", "Supported endpoints"),
    ] {
        let combo = typed(
            &state,
            &format!("endpoints-{}", category.as_str()),
            category,
        )
        .await;
        let id = if category == Category::Decision {
            "d"
        } else {
            "p"
        };
        let models = [
            StoredModel {
                id: "capable".into(),
                name: "capable".into(),
                fields: vec![ModelField {
                    label: label.into(),
                    value: endpoint.into(),
                }],
                free: false,
            },
            StoredModel {
                id: "unsupported".into(),
                name: "unsupported".into(),
                fields: vec![ModelField {
                    label: label.into(),
                    value: "embeddings".into(),
                }],
                free: false,
            },
        ];
        db::connection_models::replace(&state.db, id, &models, false)
            .await
            .unwrap();
        let html = get(&state, &format!("/combos/{}", combo.id)).await;
        assert!(html.contains("<option value=\"capable\"></option>"));
        assert!(!html.contains("<option value=\"unsupported\"></option>"));
        assert!(
            combos::add_step(&state.db, &combo.id, id, "unsupported")
                .await
                .is_err()
        );
        // Separate IDs avoid deliberately cross-classifying the same connection/model.
        let mut unique = models[0].clone();
        unique.id = category.as_str().into();
        db::connection_models::replace(&state.db, id, &[unique.clone()], false)
            .await
            .unwrap();
        combos::add_step(&state.db, &combo.id, id, &unique.id)
            .await
            .unwrap();
        unique.fields[0].value = "embeddings".into();
        db::connection_models::replace(&state.db, id, &[unique.clone()], false)
            .await
            .unwrap();
        assert!(
            combos::add_step(&state.db, &combo.id, id, &unique.id)
                .await
                .is_err()
        );
        if category == Category::Text {
            let model = format!("combo/{}", combo.name);
            let response = call(
                &state,
                "/v1/chat/completions",
                json!({"model": model, "messages": []}),
                None,
            )
            .await;
            // An attempt on the unreachable upstream would be 502 instead.
            assert_eq!(
                response.status(),
                StatusCode::BAD_REQUEST,
                "changed endpoint metadata must fail before an upstream attempt"
            );
        }
    }
}

#[tokio::test]
async fn picker_only_suggests_models_with_positive_category_evidence() {
    let state = state().await;
    for (id, protocol) in [
        ("p", Protocol::OpenAi),
        ("d", Protocol::SystemOne),
        ("asr", Protocol::Deepgram),
        ("chat", Protocol::Anthropic),
    ] {
        provider(&state, id, "http://localhost:1", protocol).await;
    }
    let model = |id: &str, input: &str, output: &str| StoredModel {
        id: id.into(),
        name: id.into(),
        free: false,
        fields: [("Input modalities", input), ("Output modalities", output)]
            .into_iter()
            .filter(|(_, value)| !value.is_empty())
            .map(|(label, value)| ModelField {
                label: label.into(),
                value: value.into(),
            })
            .collect(),
    };
    let mut catalog = vec![
        model("opaque", "", ""),
        model("vision", "text,image", "text"),
        model("image", "text,image", "image"),
        model("video", "text,image", "video"),
        model("speech", "text", "audio"),
        model("transcription", "audio", "text"),
        model("ambiguous-audio", "", "audio"),
        model("declared-text", "", ""),
        model("declared-tts", "", ""),
        model("declared-music", "", ""),
    ];
    db::connection_models::replace(&state.db, "p", &catalog, false)
        .await
        .unwrap();
    for (id, name) in [
        ("d", "decision-only"),
        ("asr", "asr-only"),
        ("chat", "chat-only"),
    ] {
        db::connection_models::replace(&state.db, id, &[model(name, "", "")], false)
            .await
            .unwrap();
    }
    for (category, name) in [
        (Category::Text, "declared-text"),
        (Category::Tts, "declared-tts"),
        (Category::Music, "declared-music"),
    ] {
        let combo = typed(&state, name, category).await;
        combos::add_step(&state.db, &combo.id, "p", name)
            .await
            .unwrap();
    }
    let all = [
        "opaque",
        "vision",
        "image",
        "video",
        "speech",
        "transcription",
        "ambiguous-audio",
        "declared-text",
        "declared-tts",
        "declared-music",
        "decision-only",
        "asr-only",
        "chat-only",
    ];
    for category in Category::ALL {
        let combo = typed(&state, &format!("picker-{}", category.as_str()), category).await;
        let expected: &[&str] = match category {
            Category::Text => &["vision", "declared-text", "chat-only"],
            Category::Decision => &["decision-only"],
            Category::Image => &["image"],
            Category::Video => &["video"],
            Category::Music => &["declared-music"],
            Category::Tts => &["speech", "declared-tts"],
            Category::Stt => &["transcription"],
            // Text output alone never suggests an embedding model.
            Category::Embedding => &[],
        };
        let html = get(&state, &format!("/combos/{}", combo.id)).await;
        for name in all {
            assert_eq!(
                html.contains(&format!("<option value=\"{name}\"></option>")),
                expected.contains(&name),
                "{} picker: {name}",
                category.as_str()
            );
        }
    }
    // A matching declaration cannot override contradictory refreshed metadata.
    let entry = catalog
        .iter_mut()
        .find(|m| m.id == "declared-text")
        .unwrap();
    *entry = model("declared-text", "text", "image");
    db::connection_models::replace(&state.db, "p", &catalog, false)
        .await
        .unwrap();
    let combo = combos::by_name(&state.db, "declared-text").await.unwrap();
    let step = combos::steps(&state.db, &combo.id).await.unwrap().remove(0);
    let response = app::router(state.clone())
        .oneshot(
            Request::post(format!("/combos/{}/steps/{}/up", combo.id, step.id))
                .header("HX-Request", "true")
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .body(Body::from(format!("_csrf={}", state.csrf_token)))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let html = String::from_utf8(body_bytes(response).await.to_vec()).unwrap();
    assert!(!html.contains("<option value=\"declared-text\"></option>"));
    assert!(!html.contains("<option value=\"opaque\"></option>"));
    assert!(html.contains("<option value=\"vision\"></option>"));
}

#[tokio::test]
async fn categories_persist_render_filter_and_survive_duplication_and_transfer() {
    let state = state().await;
    provider(&state, "p", "http://localhost:1", Protocol::OpenAi).await;
    for category in Category::ALL {
        let name = format!("{}-combo", category.as_str());
        let response = form(
            &state,
            "/combos",
            &format!("name={name}&category={}", category.as_str()),
        )
        .await;
        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        let combo = combos::by_name(&state.db, &name).await.unwrap();
        assert_eq!(combo.category, category);
        assert_eq!(
            combos::duplicate(&state.db, &combo.id)
                .await
                .unwrap()
                .category,
            category
        );
    }
    let images = combos::by_name(&state.db, "image-combo").await.unwrap();
    combos::add_step(&state.db, &images.id, "p", "arbitrary-id")
        .await
        .unwrap();
    assert!(
        get(&state, "/combos?category=image")
            .await
            .contains("image-combo")
    );
    assert!(
        !get(&state, "/combos?category=image")
            .await
            .contains("text-combo")
    );
    let edit = get(&state, &format!("/combos/{}", images.id)).await;
    assert!(edit.contains("value=\"image\" selected"));
    let test = get(&state, "/test").await;
    assert!(test.contains("value=\"image-combo\" data-category=\"image\""));
    let response = app::router(state.clone())
        .oneshot(Request::get("/v1/models").body(Body::empty()).unwrap())
        .await
        .unwrap();
    let discovery = json_body(response).await;
    assert_eq!(
        discovery["data"]
            .as_array()
            .unwrap()
            .iter()
            .find(|item| item["id"] == "combo/image-combo")
            .unwrap()["category"],
        "image"
    );
    let exported = db::config_transfer::export(&state.db).await.unwrap();
    let encoded = serde_json::to_string(&exported).unwrap();
    assert!(!encoded.contains("fake-provider-key"));
    let target = state_fixture().await;
    db::config_transfer::apply(&target.db, &encoded, &|_, _| false)
        .await
        .unwrap();
    assert_eq!(
        combos::by_name(&target.db, "image-combo")
            .await
            .unwrap()
            .category,
        Category::Image
    );
    let assigned: String = sqlx::query_scalar("SELECT category FROM model_categories")
        .fetch_one(&target.db)
        .await
        .unwrap();
    assert_eq!(assigned, "image");
}
async fn state_fixture() -> AppState {
    state().await
}

#[tokio::test]
async fn wrong_api_modes_fail_before_calls_quotas_and_rotation() {
    let mock =
        spawn_upstream(Router::new().route(
            "/chat/completions",
            post(|Json(body): Json<Value>| async move {
                Json(json!({"choices":[],"model":body["model"]}))
            }),
        ))
        .await;
    let state = state().await;
    provider(&state, "p", &mock.url(), Protocol::OpenAi).await;
    let combo = combos::create_typed(
        &state.db,
        "text",
        Category::Text,
        Strategy::RoundRobin,
        AffinityOptions::default(),
    )
    .await
    .unwrap();
    for model in ["first", "second"] {
        combos::add_step(&state.db, &combo.id, "p", model)
            .await
            .unwrap();
    }
    let issued = db::api_keys::create_with_limits(
        &state.db,
        &state.crypto,
        "limited",
        db::api_keys::KeyAccess::All,
        db::api_keys::Expiry::Never,
        db::api_keys::KeyLimits {
            requests_per_minute: Some(1),
            max_concurrent: Some(1),
        },
    )
    .await
    .unwrap();
    let key = issued.secret.expose_secret();
    let body = json!({"model":"combo/text","prompt":"x","input":"x","voice":"alloy","state":{},"questions":{},"messages":[]});
    for path in [
        "/v1/systemone",
        "/v1/images/generations",
        "/v1/videos/generations",
        "/v1/music/generations",
        "/v1/audio/speech",
    ] {
        assert_eq!(
            call(&state, path, body.clone(), Some(key)).await.status(),
            StatusCode::BAD_REQUEST
        );
    }
    assert_eq!(mock.hits.count(), 0);
    let response = call(&state, "/v1/chat/completions", body.clone(), Some(key)).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(json_body(response).await["model"], "first");
    assert_eq!(
        call(&state, "/v1/chat/completions", body, Some(key))
            .await
            .status(),
        StatusCode::TOO_MANY_REQUESTS
    );
    for category in [
        Category::Decision,
        Category::Image,
        Category::Video,
        Category::Music,
        Category::Tts,
        Category::Stt,
    ] {
        let combo = typed(&state, category.as_str(), category).await;
        let key = db::api_keys::create(&state.db, &state.crypto, category.as_str())
            .await
            .unwrap();
        assert_eq!(
            call(
                &state,
                "/v1/chat/completions",
                json!({"model":format!("combo/{}",combo.name),"messages":[]}),
                Some(key.secret.expose_secret())
            )
            .await
            .status(),
            StatusCode::BAD_REQUEST
        );
    }
    assert_eq!(mock.hits.count(), 1);
}

#[tokio::test]
async fn known_output_metadata_and_existing_declarations_prevent_mixed_models() {
    let mock = spawn_upstream(Router::new().fallback(|| async { StatusCode::OK })).await;
    let state = state().await;
    provider(&state, "p", &mock.url(), Protocol::OpenAi).await;
    provider(&state, "decision", &mock.url(), Protocol::SystemOne).await;
    let text = typed(&state, "text", Category::Text).await;
    let images = typed(&state, "images", Category::Image).await;
    let stt = typed(&state, "stt", Category::Stt).await;
    db::connection_models::replace(
        &state.db,
        "p",
        &[StoredModel {
            id: "vision".into(),
            name: "Vision".into(),
            free: false,
            fields: vec![
                ModelField {
                    label: "Architecture · Input modalities".into(),
                    value: "text, image".into(),
                },
                ModelField {
                    label: "Architecture · Output modalities".into(),
                    value: "text".into(),
                },
            ],
        }],
        false,
    )
    .await
    .unwrap();
    assert!(
        combos::add_step(&state.db, &images.id, "p", "vision")
            .await
            .is_err()
    );
    assert!(
        combos::add_step(&state.db, &stt.id, "p", "vision")
            .await
            .is_err()
    );
    combos::add_step(&state.db, &text.id, "p", "vision")
        .await
        .unwrap();
    assert!(
        combos::add_step(&state.db, &images.id, "decision", "any")
            .await
            .is_err()
    );
    combos::add_step(&state.db, &images.id, "p", "custom-id")
        .await
        .unwrap();
    assert!(
        combos::add_step(&state.db, &text.id, "p", "custom-id")
            .await
            .is_err()
    );
    let step = combos::steps(&state.db, &text.id).await.unwrap().remove(0);
    assert!(
        combos::update_step(&state.db, &text.id, &step.id, "p", "custom-id")
            .await
            .is_err()
    );
    assert_eq!(
        combos::steps(&state.db, &text.id).await.unwrap()[0].model,
        "vision"
    );
    let editor = get(&state, &format!("/combos/{}", images.id)).await;
    assert!(!editor.contains("<option value=\"vision\""));
    // A refresh may change metadata, but routing cannot silently use the now
    // incompatible model or fall through to any later target.
    db::connection_models::replace(
        &state.db,
        "p",
        &[StoredModel {
            id: "vision".into(),
            name: "Vision".into(),
            free: false,
            fields: vec![ModelField {
                label: "Output modalities".into(),
                value: "image".into(),
            }],
        }],
        false,
    )
    .await
    .unwrap();
    assert_eq!(
        call(
            &state,
            "/v1/chat/completions",
            json!({"model":"combo/text","messages":[]}),
            None
        )
        .await
        .status(),
        StatusCode::BAD_REQUEST
    );
    assert_eq!(mock.hits.count(), 0);
}

#[tokio::test]
async fn models_dev_categories_feed_picker_and_step_validation_without_upstream_metadata() {
    let state = state().await;
    provider(
        &state,
        "vendor",
        "https://api.groq.com/openai/v1",
        Protocol::OpenAi,
    )
    .await;
    provider(
        &state,
        "custom",
        "https://custom.example/v1",
        Protocol::OpenAi,
    )
    .await;
    for id in ["vendor", "custom"] {
        db::connection_models::replace(
            &state.db,
            id,
            &[StoredModel {
                id: "whisper-large-v3".into(),
                name: "Whisper".into(),
                free: false,
                fields: vec![],
            }],
            false,
        )
        .await
        .unwrap();
    }
    let speech = typed(&state, "speech", Category::Stt).await;
    let text = typed(&state, "text", Category::Text).await;
    let editor = get(&state, &format!("/combos/{}", speech.id)).await;
    assert!(editor.contains("<option value=\"whisper-large-v3\""));
    let editor = get(&state, &format!("/combos/{}", text.id)).await;
    assert!(!editor.contains("<option value=\"whisper-large-v3\""));
    assert!(
        combos::add_step(&state.db, &text.id, "vendor", "whisper-large-v3")
            .await
            .is_err()
    );
    combos::add_step(&state.db, &speech.id, "vendor", "whisper-large-v3")
        .await
        .unwrap();
    // The same ID on an unknown host retains manual classification.
    combos::add_step(&state.db, &text.id, "custom", "whisper-large-v3")
        .await
        .unwrap();
}

#[tokio::test]
async fn category_change_is_atomic_requires_empty_chain_and_unused_models_can_be_reclassified() {
    let state = state().await;
    provider(&state, "p", "http://localhost:1", Protocol::OpenAi).await;
    let text = typed(&state, "text", Category::Text).await;
    combos::add_step(&state.db, &text.id, "p", "custom")
        .await
        .unwrap();
    let revision = combos::get(&state.db, &text.id)
        .await
        .unwrap()
        .routing_revision;
    assert!(
        combos::update_typed(
            &state.db,
            &text.id,
            "renamed",
            Some(Strategy::Random),
            None,
            Some(Category::Image)
        )
        .await
        .is_err()
    );
    let current = combos::get(&state.db, &text.id).await.unwrap();
    assert_eq!(current.name, "text");
    assert_eq!(current.strategy, Strategy::Priority);
    assert_eq!(current.category, Category::Text);
    assert_eq!(current.routing_revision, revision);
    let copy = combos::duplicate(&state.db, &text.id).await.unwrap();
    assert_eq!(copy.category, Category::Text);
    combos::delete(&state.db, &copy.id).await.unwrap();
    let step = combos::steps(&state.db, &text.id).await.unwrap().remove(0);
    combos::remove_step(&state.db, &text.id, &step.id)
        .await
        .unwrap();
    combos::update_typed(
        &state.db,
        &text.id,
        "image",
        None,
        None,
        Some(Category::Image),
    )
    .await
    .unwrap();
    combos::add_step(&state.db, &text.id, "p", "custom")
        .await
        .unwrap();
    let category: String =
        sqlx::query_scalar("SELECT category FROM model_categories WHERE model='custom'")
            .fetch_one(&state.db)
            .await
            .unwrap();
    assert_eq!(category, "image");
    assert_eq!(
        form(&state, "/combos", "name=invalid&category=other")
            .await
            .status(),
        StatusCode::BAD_REQUEST
    );
    assert!(combos::by_name(&state.db, "invalid").await.is_err());
}

#[tokio::test]
async fn concurrent_classifications_and_imports_cannot_create_cross_category_models() {
    let state = state().await;
    provider(&state, "p", "http://localhost:1", Protocol::OpenAi).await;
    let text = typed(&state, "text", Category::Text).await;
    let images = typed(&state, "images", Category::Image).await;
    let (a, b) = tokio::join!(
        combos::add_step(&state.db, &text.id, "p", "same"),
        combos::add_step(&state.db, &images.id, "p", "same")
    );
    assert_ne!(a.is_ok(), b.is_ok());
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM combo_steps")
        .fetch_one(&state.db)
        .await
        .unwrap();
    assert_eq!(count, 1);
    let document = json!({"version":1,"groups":[],"connections":[{"id":"c","name":"c","base_url":"http://localhost:1","protocol":"openai","group_id":null,"enabled":false}],"combos":[{"name":"t","category":"text","strategy":"priority","steps":[{"provider_id":"c","model":"same"}]},{"name":"i","category":"image","strategy":"priority","steps":[{"provider_id":"c","model":"same"}]}]});
    let before = db::providers::list(&state.db).await.unwrap().len();
    assert!(
        db::config_transfer::apply(&state.db, &document.to_string(), &|_, _| false)
            .await
            .is_err()
    );
    assert_eq!(db::providers::list(&state.db).await.unwrap().len(), before);
}
