//! Dashboard media uses the real API validators, adapters, routing and stream guards.
mod common;
use axum::{
    Json, Router,
    body::Body,
    extract::Multipart,
    http::{HeaderMap, Request, StatusCode, header},
    response::{IntoResponse, Response},
    routing::post,
};
use base64::{Engine, engine::general_purpose::STANDARD};
use bytes::Bytes;
use common::{body_bytes, json_body, spawn_upstream};
use futures_util::StreamExt;
use secrecy::{ExposeSecret, SecretString};
use serde_json::{Value, json};
use sparkrouter::{
    app::{self, AppState},
    config::Config,
    crypto::Crypto,
    db::{
        self,
        api_keys::{self, Expiry, KeyAccess, KeyLimits},
        combos::Strategy,
        providers::Provider,
    },
    providers::Protocol,
};
use std::sync::{Arc, Mutex};
use tower::ServiceExt;

async fn state() -> AppState {
    AppState::from_config(Config {
        database_url: "sqlite::memory:".into(),
        bind: "127.0.0.1:0".parse().unwrap(),
        crypto: Crypto::from_base64(&SecretString::from(STANDARD.encode([69; 32]))).unwrap(),
        api_key: None,
        dashboard_password: None,
        dashboard_secure_cookie: false,
    })
    .await
    .unwrap()
}
async fn provider(state: &AppState, id: &str, url: &str, protocol: Protocol) {
    db::providers::save(
        &state.db,
        &Provider {
            id: id.into(),
            name: "Connection ✦".into(),
            base_url: url.into(),
            protocol,
            encrypted_api_key: state
                .crypto
                .encrypt_api_key(id, &SecretString::from("fake-upstream-key"))
                .unwrap(),
            enabled: true,
        },
    )
    .await
    .unwrap();
}
async fn combo(state: &AppState, name: &str, steps: &[(&str, &str)], strategy: Strategy) {
    let category = match name {
        "video" => db::combos::Category::Video,
        "music" => db::combos::Category::Music,
        "speech" => db::combos::Category::Tts,
        "audio" => db::combos::Category::Stt,
        _ => db::combos::Category::Image,
    };
    let saved = db::combos::create_typed(
        &state.db,
        name,
        category,
        strategy,
        db::combos::AffinityOptions::default(),
    )
    .await
    .unwrap();
    for (provider, model) in steps {
        db::combos::add_step(&state.db, &saved.id, provider, model)
            .await
            .unwrap();
    }
}
fn trace(response: &Response) -> Value {
    let encoded = response.headers()["x-sparkrouter-attempts"].as_bytes();
    assert!(encoded.len() <= 5504);
    serde_json::from_slice(&STANDARD.decode(encoded).unwrap()).unwrap()
}
async fn call(state: &AppState, path: &str, body: Value, key: Option<&str>) -> Response {
    let mut request = Request::post(path)
        .header(header::CONTENT_TYPE, "application/json")
        .header("x-sparkrouter-csrf", state.csrf_token.as_str());
    if let Some(key) = key {
        request = request.header(header::AUTHORIZATION, format!("Bearer {key}"));
    }
    app::router(state.clone())
        .oneshot(request.body(Body::from(body.to_string())).unwrap())
        .await
        .unwrap()
}
fn multipart(fields: &[(&str, &str, Option<&str>)]) -> String {
    let mut body = String::new();
    for (name, value, filename) in fields {
        body.push_str(&format!(
            "--fixture\r\nContent-Disposition: form-data; name=\"{name}\""
        ));
        if let Some(filename) = filename {
            body.push_str(&format!("; filename=\"{filename}\""));
        }
        body.push_str(&format!("\r\n\r\n{value}\r\n"));
    }
    body.push_str("--fixture--\r\n");
    body
}
async fn upload(state: &AppState, mode: &str, body: String, key: Option<&str>) -> Response {
    let mut request = Request::post(format!("/test/media-upload/{mode}"))
        .header(
            header::CONTENT_TYPE,
            "multipart/form-data; boundary=fixture",
        )
        .header("x-sparkrouter-csrf", state.csrf_token.as_str());
    if let Some(key) = key {
        request = request.header(header::AUTHORIZATION, format!("Bearer {key}"));
    }
    app::router(state.clone())
        .oneshot(request.body(Body::from(body)).unwrap())
        .await
        .unwrap()
}
async fn history_count(state: &AppState, expected: i64) {
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM request_history")
                .fetch_one(&state.db)
                .await
                .unwrap();
            if count == expected {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn page_exposes_all_media_modes_and_local_blob_only_csp() {
    let state = state().await;
    combo(&state, "media", &[], Strategy::Priority).await;
    let response = app::router(state.clone())
        .oneshot(Request::get("/test").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let csp = response.headers()[header::CONTENT_SECURITY_POLICY]
        .to_str()
        .unwrap();
    assert!(csp.contains("img-src 'self' data: blob:"));
    assert!(csp.contains("media-src 'self' blob:"));
    assert!(csp.contains("connect-src 'self';"));
    assert!(!csp.contains("https:"));
    let html = String::from_utf8(body_bytes(response).await.to_vec()).unwrap();
    for mode in [
        "chat",
        "systemone",
        "image",
        "image-edit",
        "video",
        "music",
        "speech",
        "transcription",
        "translation",
    ] {
        assert!(html.contains(&format!("value=\"{mode}\"")));
    }
    assert!(html.contains("id=\"test-file\""));
    assert!(html.contains("id=\"test-mask\""));
    assert!(html.contains("id=\"test-media-options\""));
    assert!(html.contains("never automatically fetched"));
}

#[tokio::test]
async fn json_modes_keep_bytes_extensions_fallback_traces_and_metadata_only_history() {
    for mode in ["image", "video", "music"] {
        let bad = spawn_upstream(
            Router::new()
                .fallback(|| async { (StatusCode::TOO_MANY_REQUESTS, "private-upstream-detail") }),
        )
        .await;
        let good = spawn_upstream(Router::new().fallback(|headers: HeaderMap, Json(body): Json<Value>| async move {
            assert_eq!(headers[header::AUTHORIZATION], "Bearer fake-upstream-key");
            assert!(!headers.contains_key("x-sparkrouter-csrf"));
            assert!(!headers.contains_key("x-sparkrouter-session-id"));
            assert_eq!(body["model"], "vendor/nested/model");
            assert_eq!(body["extension"]["keep"], true);
            assert!(body.get("_csrf").is_none());
            Json(json!({"data":[{"url":"https://example.com/private-signed-output"}], "extension":"preserved"}))
        })).await;
        let state = state().await;
        db::settings::set_logging_mode(&state.db, db::settings::LoggingMode::Extended)
            .await
            .unwrap();
        provider(&state, "text", &bad.url(), Protocol::Anthropic).await;
        provider(&state, "bad", &bad.url(), Protocol::OpenAi).await;
        provider(&state, "good", &good.url(), Protocol::OpenAi).await;
        combo(
            &state,
            mode,
            &[("bad", "bad"), ("good", "vendor/nested/model")],
            Strategy::Priority,
        )
        .await;
        let response = call(&state, &format!("/test/media/{mode}"), json!({"model":format!("combo/{mode}"), "prompt":"private-prompt", "_csrf":"must-not-leak", "extension":{"keep":true}}), None).await;
        assert_eq!(response.status(), StatusCode::OK);
        let trace = trace(&response);
        assert_eq!(trace["attempts"].as_array().unwrap().len(), 2);
        assert_eq!(trace["attempts"][0]["outcome"], "retryable_failure");
        assert_eq!(trace["attempts"][1]["outcome"], "selected");
        assert_eq!(trace["attempts"][1]["position"], 2);
        assert_eq!(trace["attempts"][1]["provider_name"], "Connection ✦");
        assert!(!trace.to_string().contains("private-upstream-detail"));
        assert_eq!(json_body(response).await["extension"], "preserved");
        assert_eq!(bad.hits.count(), 1);
        assert_eq!(good.hits.count(), 1);
        history_count(&state, 1).await;
        let source: String = sqlx::query_scalar("SELECT source FROM request_history")
            .fetch_one(&state.db)
            .await
            .unwrap();
        assert_eq!(source, "test");
        let captured: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM request_payloads")
            .fetch_one(&state.db)
            .await
            .unwrap();
        assert_eq!(captured, 0);
    }
}

#[tokio::test]
async fn media_auth_csrf_origin_scopes_and_quotas_are_shared_with_api() {
    let mock = spawn_upstream(
        Router::new().fallback(|| async { Json(json!({"data":[{"b64_json":"aW1hZ2U="}]})) }),
    )
    .await;
    let state = state().await;
    provider(&state, "a", &mock.url(), Protocol::OpenAi).await;
    combo(&state, "allowed", &[("a", "image")], Strategy::Priority).await;
    let issued = api_keys::create_with_limits(
        &state.db,
        &state.crypto,
        "limited",
        KeyAccess::Selected(vec!["combo/allowed".into()]),
        Expiry::Never,
        KeyLimits {
            requests_per_minute: Some(1),
            max_concurrent: Some(1),
        },
    )
    .await
    .unwrap();
    let key = issued.secret.expose_secret();
    let body = json!({"model":"combo/allowed", "prompt":"image"});
    assert_eq!(
        call(&state, "/test/media/image", body.clone(), None)
            .await
            .status(),
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        call(&state, "/test/media/image", body.clone(), Some("wrong-key"))
            .await
            .status(),
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        call(
            &state,
            "/test/media/image",
            json!({"model":"combo/denied", "prompt":"image"}),
            Some(key)
        )
        .await
        .status(),
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        call(
            &state,
            "/test/media/image",
            json!({"model":"openai/image", "prompt":"image"}),
            Some(key)
        )
        .await
        .status(),
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        call(&state, "/test/media/unknown", body.clone(), Some(key))
            .await
            .status(),
        StatusCode::BAD_REQUEST
    );
    for (token, origin) in [
        ("wrong", "http://localhost"),
        (state.csrf_token.as_str(), "https://evil.example"),
    ] {
        let response = app::router(state.clone())
            .oneshot(
                Request::post("/test/media/image")
                    .header(header::CONTENT_TYPE, "application/json")
                    .header(header::AUTHORIZATION, format!("Bearer {key}"))
                    .header("x-sparkrouter-csrf", token)
                    .header(header::HOST, "localhost")
                    .header(header::ORIGIN, origin)
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }
    assert_eq!(
        call(
            &state,
            "/test/media/image",
            json!({"model":"combo/allowed"}),
            Some(key)
        )
        .await
        .status(),
        StatusCode::BAD_REQUEST
    );
    assert_eq!(mock.hits.count(), 0);
    assert_eq!(
        call(&state, "/test/media/image", body.clone(), Some(key))
            .await
            .status(),
        StatusCode::OK
    );
    let limited = call(&state, "/v1/images/generations", body, Some(key)).await;
    assert_eq!(limited.status(), StatusCode::TOO_MANY_REQUESTS);
    assert!(limited.headers().contains_key(header::RETRY_AFTER));
    assert!(!limited.headers().contains_key("x-sparkrouter-attempts"));
    assert_eq!(mock.hits.count(), 1);
    sqlx::query("UPDATE api_keys SET expires_at = unixepoch() - 1 WHERE id = ?")
        .bind(&issued.id)
        .execute(&state.db)
        .await
        .unwrap();
    assert_eq!(
        call(
            &state,
            "/test/media/image",
            json!({"model":"combo/allowed", "prompt":"image"}),
            Some(key)
        )
        .await
        .status(),
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        call(
            &state,
            "/test/media/image",
            json!({"model":"combo/allowed", "prompt":"image"}),
            None
        )
        .await
        .status(),
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(mock.hits.count(), 1);
}

#[tokio::test]
async fn multipart_modes_replay_files_strip_csrf_and_keep_large_upload_and_text_outputs() {
    for mode in ["transcription", "translation", "image-edit"] {
        let editing = mode == "image-edit";
        let path = if editing {
            "/images/edits".to_owned()
        } else {
            format!("/audio/{mode}s")
        };
        let mock = spawn_upstream(
            Router::new()
                .route(
                    &path,
                    post(
                        move |headers: HeaderMap, mut multipart: Multipart| async move {
                            assert!(!headers.contains_key("x-sparkrouter-csrf"));
                            let mut fields = std::collections::HashMap::new();
                            while let Some(field) = multipart.next_field().await.unwrap() {
                                let name = field.name().unwrap().to_owned();
                                fields.insert(name, field.bytes().await.unwrap());
                            }
                            assert!(!fields.contains_key("_csrf"));
                            assert_eq!(fields["model"], "upstream/model");
                            if editing {
                                assert_eq!(fields["image"], "image-bytes");
                                assert_eq!(fields["mask"], "mask-bytes");
                                assert_eq!(fields["prompt"], "edit");
                            } else {
                                assert!(fields["file"].len() > 4 * 1024 * 1024);
                                assert_eq!(fields["language"], "en");
                            }
                            if editing {
                                Json(json!({"data":[{"b64_json":"aW1hZ2U="}]})).into_response()
                            } else {
                                (
                                    [("content-type", "text/plain")],
                                    "<script>plain transcript</script>",
                                )
                                    .into_response()
                            }
                        },
                    ),
                )
                .layer(axum::extract::DefaultBodyLimit::max(32 * 1024 * 1024)),
        )
        .await;
        let bad =
            spawn_upstream(Router::new().fallback(|| async { StatusCode::SERVICE_UNAVAILABLE }))
                .await;
        let state = state().await;
        provider(&state, "bad", &bad.url(), Protocol::OpenAi).await;
        provider(&state, "good", &mock.url(), Protocol::OpenAi).await;
        combo(
            &state,
            if editing { "media" } else { "audio" },
            &[("bad", "bad"), ("good", "upstream/model")],
            Strategy::Priority,
        )
        .await;
        let large = "x".repeat(5 * 1024 * 1024);
        let mut fields = vec![
            (
                "model",
                if editing {
                    "combo/media"
                } else {
                    "combo/audio"
                },
                None,
            ),
            ("_csrf", "must-not-leak", None),
        ];
        if editing {
            fields.extend([
                ("image", "image-bytes", Some("image.png")),
                ("mask", "mask-bytes", Some("mask.png")),
                ("prompt", "edit", None),
            ]);
        } else {
            fields.extend([
                ("file", large.as_str(), Some("audio.wav")),
                ("language", "en", None),
            ]);
        }
        let response = upload(&state, mode, multipart(&fields), None).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(trace(&response)["attempts"].as_array().unwrap().len(), 2);
        if editing {
            assert!(json_body(response).await["data"].is_array());
        } else {
            assert_eq!(response.headers()[header::CONTENT_TYPE], "text/plain");
            assert_eq!(
                body_bytes(response).await,
                "<script>plain transcript</script>"
            );
        }
        assert_eq!(bad.hits.count(), 1);
        assert_eq!(mock.hits.count(), 1);
    }
}

#[tokio::test]
async fn binary_speech_retains_stream_permits_and_records_metadata_only_completion() {
    let (send, receive) = tokio::sync::oneshot::channel::<()>();
    let receive = Arc::new(Mutex::new(Some(receive)));
    let mock = spawn_upstream(Router::new().route("/audio/speech", post(move |Json(body): Json<Value>| { let receive = receive.lock().unwrap().take().unwrap(); async move {
        assert_eq!(body["input"], "private speech");
        assert_eq!(body["voice"], "alloy");
        let stream = async_stream::stream! { yield Ok::<_,std::io::Error>(Bytes::from_static(b"first-audio")); receive.await.unwrap(); yield Ok(Bytes::from_static(b"last-audio")); };
        ([("content-type", "audio/mpeg")], Body::from_stream(stream))
    }}))).await;
    let state = state().await;
    db::settings::set_logging_mode(&state.db, db::settings::LoggingMode::Extended)
        .await
        .unwrap();
    provider(&state, "a", &mock.url(), Protocol::OpenAi).await;
    combo(&state, "speech", &[("a", "tts")], Strategy::Priority).await;
    let issued = api_keys::create_with_limits(
        &state.db,
        &state.crypto,
        "one-at-a-time",
        KeyAccess::All,
        Expiry::Never,
        KeyLimits {
            requests_per_minute: None,
            max_concurrent: Some(1),
        },
    )
    .await
    .unwrap();
    let key = issued.secret.expose_secret();
    let body = json!({"model":"combo/speech", "input":"private speech", "voice":"alloy"});
    let response = call(&state, "/test/media/speech", body.clone(), Some(key)).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()[header::CONTENT_TYPE], "audio/mpeg");
    assert_eq!(trace(&response)["attempts"][0]["outcome"], "selected");
    let mut stream = response.into_body().into_data_stream();
    assert_eq!(stream.next().await.unwrap().unwrap(), "first-audio");
    assert_eq!(
        call(&state, "/test/media/speech", body.clone(), Some(key))
            .await
            .status(),
        StatusCode::TOO_MANY_REQUESTS
    );
    assert_eq!(
        call(&state, "/v1/audio/speech", body, Some(key))
            .await
            .status(),
        StatusCode::TOO_MANY_REQUESTS
    );
    send.send(()).unwrap();
    assert_eq!(stream.next().await.unwrap().unwrap(), "last-audio");
    assert!(stream.next().await.is_none());
    history_count(&state, 1).await;
    // The row appears with its insert; the terminal outcome queued at the end
    // of the body may commit in a later batch.
    state.flush_history().await;
    let (source, outcome, streaming): (String, String, bool) =
        sqlx::query_as("SELECT source, outcome, streaming FROM request_history")
            .fetch_one(&state.db)
            .await
            .unwrap();
    assert_eq!(
        (source.as_str(), outcome.as_str(), streaming),
        ("test", "success", true)
    );
    let captured: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM request_payloads")
        .fetch_one(&state.db)
        .await
        .unwrap();
    assert_eq!(captured, 0);
    assert_eq!(mock.hits.count(), 1);
}

#[tokio::test]
async fn diagnostic_header_is_bounded_full_attempts_remain_in_history_and_public_api_has_none() {
    let mock = spawn_upstream(
        Router::new()
            .fallback(|| async { (StatusCode::SERVICE_UNAVAILABLE, "private error body") }),
    )
    .await;
    let state = state().await;
    provider(&state, "a", &mock.url(), Protocol::OpenAi).await;
    let models: Vec<_> = (0..30)
        .map(|n| format!("{}-{n}", "x".repeat(240)))
        .collect();
    let steps: Vec<_> = models.iter().map(|model| ("a", model.as_str())).collect();
    combo(&state, "many", &steps, Strategy::Priority).await;
    let body = json!({"model":"combo/many", "prompt":"image"});
    let response = call(&state, "/test/media/image", body.clone(), None).await;
    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    let trace = trace(&response);
    assert_eq!(trace["truncated"], true);
    assert!(trace["attempts"].as_array().unwrap().len() < 30);
    assert!(
        !json_body(response)
            .await
            .to_string()
            .contains("private error body")
    );
    history_count(&state, 1).await;
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM request_attempts")
        .fetch_one(&state.db)
        .await
        .unwrap();
    assert_eq!(count, 30);
    let response = call(&state, "/v1/images/generations", body, None).await;
    assert!(!response.headers().contains_key("x-sparkrouter-attempts"));
    assert_eq!(mock.hits.count(), 60);
}

#[tokio::test]
async fn media_test_and_api_share_round_robin_state() {
    let mock = spawn_upstream(
        Router::new().fallback(|Json(body): Json<Value>| async move {
            Json(json!({"data":[{"url":"https://example.com/image"}], "model":body["model"]}))
        }),
    )
    .await;
    let state = state().await;
    provider(&state, "a", &mock.url(), Protocol::OpenAi).await;
    provider(&state, "b", &mock.url(), Protocol::OpenAi).await;
    combo(
        &state,
        "rotate",
        &[("a", "first"), ("b", "second")],
        Strategy::RoundRobin,
    )
    .await;
    for (path, expected) in [
        ("/test/media/image", "first"),
        ("/v1/images/generations", "second"),
        ("/test/media/image", "first"),
    ] {
        let response = call(
            &state,
            path,
            json!({"model":"combo/rotate", "prompt":"image"}),
            None,
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(json_body(response).await["model"], expected);
    }
}

#[tokio::test]
async fn uploads_require_header_csrf_auth_scopes_and_valid_bounded_parts() {
    let mock =
        spawn_upstream(Router::new().fallback(|| async { Json(json!({"text":"ok"})) })).await;
    let state = state().await;
    provider(&state, "a", &mock.url(), Protocol::OpenAi).await;
    combo(&state, "audio", &[("a", "whisper")], Strategy::Priority).await;
    let denied = api_keys::create_with_policy(
        &state.db,
        &state.crypto,
        "denied",
        KeyAccess::Selected(vec![]),
        Expiry::Never,
    )
    .await
    .unwrap();
    let allowed = api_keys::create(&state.db, &state.crypto, "allowed")
        .await
        .unwrap();
    let fields = [
        ("model", "combo/audio", None),
        ("file", "audio", Some("audio.wav")),
        ("_csrf", state.csrf_token.as_str(), None),
    ];
    let body = multipart(&fields);
    assert_eq!(
        upload(&state, "transcription", body.clone(), None)
            .await
            .status(),
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        upload(
            &state,
            "transcription",
            body.clone(),
            Some(denied.secret.expose_secret())
        )
        .await
        .status(),
        StatusCode::FORBIDDEN
    );
    let response = app::router(state.clone())
        .oneshot(
            Request::post("/test/media-upload/transcription")
                .header(
                    header::CONTENT_TYPE,
                    "multipart/form-data; boundary=fixture",
                )
                .header(
                    header::AUTHORIZATION,
                    format!("Bearer {}", allowed.secret.expose_secret()),
                )
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST); // Body CSRF alone cannot authorize.
    assert!(!response.headers().contains_key("x-sparkrouter-attempts"));
    for fields in [
        vec![("model", "combo/audio", None)],
        vec![
            ("model", "combo/audio", None),
            ("file", "", Some("empty.wav")),
        ],
        vec![
            ("model", "connection-a/whisper", None),
            ("file", "audio", Some("audio.wav")),
        ],
        vec![
            ("model", "combo/audio", None),
            ("model", "combo/other", None),
            ("file", "audio", Some("audio.wav")),
        ],
    ] {
        assert_eq!(
            upload(
                &state,
                "transcription",
                multipart(&fields),
                Some(allowed.secret.expose_secret())
            )
            .await
            .status(),
            StatusCode::BAD_REQUEST
        );
    }
    let large = "x".repeat(65537);
    assert_eq!(
        upload(
            &state,
            "transcription",
            multipart(&[
                ("model", "combo/audio", None),
                ("file", "audio", Some("audio.wav")),
                ("prompt", &large, None)
            ]),
            Some(allowed.secret.expose_secret())
        )
        .await
        .status(),
        StatusCode::PAYLOAD_TOO_LARGE
    );
    assert_eq!(mock.hits.count(), 0);
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM request_history")
        .fetch_one(&state.db)
        .await
        .unwrap();
    assert_eq!(count, 0);
}
