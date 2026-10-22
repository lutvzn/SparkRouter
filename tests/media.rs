mod common;
use axum::{
    Json, Router,
    body::Body,
    extract::Multipart,
    http::{HeaderMap, Request, StatusCode, header},
    response::{IntoResponse, Response},
    routing::{get, post},
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
    db::{self, combos, connection_models, providers::Provider},
    providers::Protocol,
};
use tower::ServiceExt;

async fn state() -> AppState {
    AppState::from_config(Config {
        database_url: "sqlite::memory:".into(),
        bind: "127.0.0.1:0".parse().unwrap(),
        crypto: Crypto::from_base64(&SecretString::from(STANDARD.encode([25; 32]))).unwrap(),
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
        &Provider {
            id: id.into(),
            name: id.into(),
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
async fn combo(state: &AppState, name: &str, steps: &[(&str, &str)]) {
    let category = match name {
        "video" => combos::Category::Video,
        "music" => combos::Category::Music,
        "tts" => combos::Category::Tts,
        "stt" | "large" => combos::Category::Stt,
        _ => combos::Category::Image,
    };
    let combo = combos::create_typed(
        &state.db,
        name,
        category,
        combos::Strategy::Priority,
        combos::AffinityOptions::default(),
    )
    .await
    .unwrap();
    for (provider, model) in steps {
        combos::add_step(&state.db, &combo.id, provider, model)
            .await
            .unwrap();
    }
}
async fn call(state: &AppState, path: &str, body: Value) -> Response {
    app::router(state.clone())
        .oneshot(
            Request::post(path)
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap()
}
fn upload(model: &str, extra: &str) -> String {
    format!(
        "--test-boundary\r\nContent-Disposition: form-data; name=\"model\"\r\n\r\n{model}\r\n--test-boundary\r\nContent-Disposition: form-data; name=\"file\"; filename=\"audio.wav\"\r\nContent-Type: audio/wav\r\n\r\nRIFF-audio-bytes\r\n{extra}--test-boundary--\r\n"
    )
}
async fn upload_call(state: &AppState, path: &str, body: String) -> Response {
    app::router(state.clone())
        .oneshot(
            Request::post(path)
                .header(
                    header::CONTENT_TYPE,
                    "multipart/form-data; boundary=test-boundary",
                )
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap()
}

#[tokio::test]
async fn compatible_media_preserves_extensions_replaces_only_model_and_shares_fallback() {
    let bad =
        spawn_upstream(Router::new().fallback(|| async { StatusCode::TOO_MANY_REQUESTS })).await;
    let good = spawn_upstream(Router::new().route(
        "/images/generations",
        post(|headers: HeaderMap, Json(body): Json<Value>| async move {
            assert_eq!(headers[header::AUTHORIZATION], "Bearer fake-upstream-key");
            assert_eq!(body["model"], "vendor/nested/image");
            assert_eq!(body["extension"]["keep"], true);
            Json(json!({"created": 1, "data":[{"b64_json":"aW1hZ2U="}], "extension":"preserved"}))
        }),
    ))
    .await;
    let state = state().await;
    provider(&state, "a", &bad.url(), Protocol::OpenAi).await;
    provider(&state, "b", &good.url(), Protocol::OpenAi).await;
    provider(&state, "chat-only", &bad.url(), Protocol::Anthropic).await;
    combo(
        &state,
        "image",
        &[("a", "retry"), ("b", "vendor/nested/image")],
    )
    .await;
    let response = call(
        &state,
        "/v1/images/generations",
        json!({"model":"combo/image", "prompt":"test", "extension":{"keep":true}}),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(json_body(response).await["extension"], "preserved");
    assert_eq!(bad.hits.count(), 1);
    assert_eq!(good.hits.count(), 1);
}

#[tokio::test]
async fn permanent_media_errors_stop_fallback_and_invalid_success_retries() {
    for (status, body, expected_backup) in [
        (StatusCode::BAD_REQUEST, "upstream-secret", 0),
        (StatusCode::FORBIDDEN, "secret", 0),
        (StatusCode::UNPROCESSABLE_ENTITY, "secret", 0),
        (StatusCode::OK, "{\"data\":[]}", 1),
        (StatusCode::OK, "{\"error\":\"secret\"}", 1),
    ] {
        let bad =
            spawn_upstream(Router::new().fallback(move || async move { (status, body) })).await;
        let backup = spawn_upstream(Router::new().fallback(|| async {
            Json(json!({"data":[{"url":"https://example.com/image.png"}]}))
        }))
        .await;
        let state = state().await;
        provider(&state, "a", &bad.url(), Protocol::OpenAi).await;
        provider(&state, "b", &backup.url(), Protocol::OpenAi).await;
        combo(&state, "media", &[("a", "a"), ("b", "b")]).await;
        let response = call(
            &state,
            "/v1/images/generations",
            json!({"model":"combo/media","prompt":"test"}),
        )
        .await;
        assert_eq!(backup.hits.count(), expected_backup);
        let bytes = body_bytes(response).await;
        assert!(!String::from_utf8_lossy(&bytes).contains("secret"));
    }
}

#[tokio::test]
async fn fal_image_video_and_music_share_one_connection_and_keep_native_options() {
    let mock = spawn_upstream(Router::new().fallback(|headers: HeaderMap, Json(body): Json<Value>| async move {
        assert_eq!(headers[header::AUTHORIZATION],"Key fake-upstream-key");
        assert!(body.get("model").is_none());
        assert_eq!(body["seed"],42);
        if body.get("tags").is_some() { assert_eq!(body["tags"],"test"); Json(json!({"audio":{"url":"https://example.com/audio.wav"}})) }
        else { Json(json!({"images":[{"url":"https://example.com/image.png","width":512}],"video":{"url":"https://example.com/video.mp4"}})) }
    })).await;
    let state = state().await;
    provider(&state, "fal", &mock.url(), Protocol::Fal).await;
    for (name, path, model) in [
        ("image", "/v1/images/generations", "fal-ai/flux/dev"),
        (
            "video",
            "/v1/videos/generations",
            "fal-ai/wan/v2.2-a14b/text-to-video",
        ),
        ("music", "/v1/music/generations", "fal-ai/ace-step"),
    ] {
        combo(&state, name, &[("fal", model)]).await;
        let response = call(
            &state,
            path,
            json!({"model":format!("combo/{name}"),"prompt":"test","seed":42}),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        assert!(
            json_body(response).await["data"][0]["url"]
                .as_str()
                .unwrap()
                .starts_with("https://")
        );
    }
    assert_eq!(mock.hits.count(), 3);
    let response = call(
        &state,
        "/v1/chat/completions",
        json!({"model":"combo/image","messages":[]}),
    )
    .await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(mock.hits.count(), 3);
}

#[tokio::test]
async fn fal_path_injection_is_rejected_without_upstream_calls() {
    let mock = spawn_upstream(Router::new().fallback(|| async { StatusCode::OK })).await;
    let state = state().await;
    provider(&state, "fal", &mock.url(), Protocol::Fal).await;
    for (i, model) in [
        "../models",
        "fal-ai/../../secret",
        "//host/path",
        "foo?secret=true",
        "https://example.com",
    ]
    .iter()
    .enumerate()
    {
        let name = format!("unsafe{i}");
        combo(&state, &name, &[("fal", model)]).await;
        assert_eq!(
            call(
                &state,
                "/v1/images/generations",
                json!({"model":format!("combo/{name}"),"prompt":"test"})
            )
            .await
            .status(),
            StatusCode::BAD_REQUEST
        );
    }
    assert_eq!(mock.hits.count(), 0);
}

#[tokio::test]
async fn multipart_transcription_replays_file_and_repeated_extensions_on_fallback() {
    let bad =
        spawn_upstream(Router::new().fallback(|| async { StatusCode::SERVICE_UNAVAILABLE })).await;
    let good = spawn_upstream(Router::new().route(
        "/audio/transcriptions",
        post(|mut form: Multipart| async move {
            let mut models = vec![];
            let mut repeated = vec![];
            while let Some(field) = form.next_field().await.unwrap() {
                match field.name().unwrap() {
                    "model" => models.push(field.text().await.unwrap()),
                    "file" => {
                        assert_eq!(field.file_name(), Some("audio.wav"));
                        assert_eq!(field.bytes().await.unwrap(), "RIFF-audio-bytes");
                    }
                    "timestamp_granularities[]" => repeated.push(field.text().await.unwrap()),
                    _ => panic!("unexpected field"),
                }
            }
            assert_eq!(models, ["nested/whisper"]);
            assert_eq!(repeated, ["word", "segment"]);
            Json(json!({"text":"transcribed","extension":true}))
        }),
    ))
    .await;
    let state = state().await;
    provider(&state, "a", &bad.url(), Protocol::OpenAi).await;
    provider(&state, "b", &good.url(), Protocol::OpenAi).await;
    combo(&state, "stt", &[("a", "first"), ("b", "nested/whisper")]).await;
    let extra = "--test-boundary\r\nContent-Disposition: form-data; name=\"timestamp_granularities[]\"\r\n\r\nword\r\n--test-boundary\r\nContent-Disposition: form-data; name=\"timestamp_granularities[]\"\r\n\r\nsegment\r\n";
    let response = upload_call(
        &state,
        "/v1/audio/transcriptions",
        upload("combo/stt", extra),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(json_body(response).await["extension"], true);
    assert_eq!(bad.hits.count(), 1);
    assert_eq!(good.hits.count(), 1);
}

#[tokio::test]
async fn deepgram_native_speech_and_transcription_auth_and_shapes() {
    let mock = spawn_upstream(
        Router::new()
            .route(
                "/listen",
                post(|headers: HeaderMap, bytes: Bytes| async move {
                    assert_eq!(headers[header::AUTHORIZATION], "Token fake-upstream-key");
                    assert_eq!(bytes, "RIFF-audio-bytes");
                    Json(
                        json!({"results":{"channels":[{"alternatives":[{"transcript":"hello"}]}]}}),
                    )
                }),
            )
            .route(
                "/speak",
                post(|headers: HeaderMap, Json(body): Json<Value>| async move {
                    assert_eq!(headers[header::AUTHORIZATION], "Token fake-upstream-key");
                    assert_eq!(body["text"], "Hello");
                    ([("content-type", "audio/mpeg")], "mp3-bytes")
                }),
            ),
    )
    .await;
    let state = state().await;
    provider(&state, "dg", &mock.url(), Protocol::Deepgram).await;
    combo(&state, "stt", &[("dg", "nova-3")]).await;
    combo(&state, "tts", &[("dg", "aura-2-thalia-en")]).await;
    let response = upload_call(&state, "/v1/audio/transcriptions", upload("combo/stt", "")).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(json_body(response).await["text"], "hello");
    let response = call(
        &state,
        "/v1/audio/speech",
        json!({"model":"combo/tts","input":"Hello"}),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(body_bytes(response).await, "mp3-bytes");
}

#[tokio::test]
async fn elevenlabs_tts_stt_and_music_share_native_key_without_model_override() {
    let mock = spawn_upstream(
        Router::new()
            .route(
                "/text-to-speech/real-voice-id",
                post(|headers: HeaderMap, Json(body): Json<Value>| async move {
                    assert_eq!(headers["xi-api-key"], "fake-upstream-key");
                    assert!(headers.get("authorization").is_none());
                    assert_eq!(body["model_id"], "eleven_multilingual_v2");
                    assert_eq!(body["text"], "Hello");
                    ([("content-type", "audio/mpeg")], "voice")
                }),
            )
            .route(
                "/speech-to-text",
                post(|mut form: Multipart| async move {
                    let mut models = vec![];
                    while let Some(field) = form.next_field().await.unwrap() {
                        if field.name() == Some("model_id") {
                            models.push(field.text().await.unwrap());
                        }
                    }
                    assert_eq!(models, ["scribe_v1"]);
                    Json(json!({"text":"hello","words":[]}))
                }),
            )
            .route(
                "/music",
                post(|headers: HeaderMap, Json(body): Json<Value>| async move {
                    assert_eq!(headers["xi-api-key"], "fake-upstream-key");
                    assert_eq!(body["model_id"], "music_v1");
                    assert_eq!(body["music_length_ms"], 10000);
                    ([("content-type", "audio/mpeg")], "music")
                }),
            ),
    )
    .await;
    let state = state().await;
    provider(&state, "el", &mock.url(), Protocol::ElevenLabs).await;
    combo(&state, "tts", &[("el", "eleven_multilingual_v2")]).await;
    combo(&state, "stt", &[("el", "scribe_v1")]).await;
    combo(&state, "music", &[("el", "music_v1")]).await;
    let response = call(
        &state,
        "/v1/audio/speech",
        json!({"model":"combo/tts","input":"Hello","voice":"real-voice-id"}),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(body_bytes(response).await, "voice");
    let response=upload_call(&state,"/v1/audio/transcriptions",upload("combo/stt","--test-boundary\r\nContent-Disposition: form-data; name=\"model_id\"\r\n\r\nattacker\r\n")).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(json_body(response).await["text"], "hello");
    let response = call(
        &state,
        "/v1/music/generations",
        json!({"model":"combo/music","prompt":"ambient","music_length_ms":10000}),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(body_bytes(response).await, "music");
}

#[tokio::test]
async fn minimax_sync_native_outputs_and_hex_music() {
    let mock = spawn_upstream(
        Router::new()
            .route(
                "/music_generation",
                post(|Json(body): Json<Value>| async move {
                    assert_eq!(body["model"], "music-3.0");
                    assert_eq!(body["output_format"], "hex");
                    assert_eq!(body["lyrics"], "hello");
                    Json(
                        json!({"base_resp":{"status_code":0},"data":{"status":2,"audio":"414243"}}),
                    )
                }),
            )
            .route(
                "/image_generation",
                post(|Json(body): Json<Value>| async move {
                    assert_eq!(body["response_format"], "base64");
                    Json(
                        json!({"base_resp":{"status_code":0},"data":{"image_base64":["aW1hZ2U="]}}),
                    )
                }),
            )
            .route(
                "/t2a_v2",
                post(|Json(body): Json<Value>| async move {
                    assert_eq!(body["text"], "Hello");
                    assert_eq!(body["voice_setting"]["voice_id"], "native-voice");
                    Json(
                        json!({"base_resp":{"status_code":0},"data":{"status":2,"audio":"414243"}}),
                    )
                }),
            )
            .route(
                "/speech_to_text",
                post(|headers: HeaderMap, mut form: Multipart| async move {
                    assert_eq!(headers["language"], "en");
                    while let Some(field) = form.next_field().await.unwrap() {
                        assert_ne!(field.name(), Some("language"));
                    }
                    Json(json!({"text":"hello"}))
                }),
            ),
    )
    .await;
    let state = state().await;
    provider(&state, "mm", &mock.url(), Protocol::MiniMax).await;
    for (name, model) in [
        ("music", "music-3.0"),
        ("image", "image-01"),
        ("tts", "speech-2.8-hd"),
        ("stt", "asr-1.0"),
    ] {
        combo(&state, name, &[("mm", model)]).await;
    }
    let response = call(
        &state,
        "/v1/music/generations",
        json!({"model":"combo/music","lyrics":"hello","output_format":"hex"}),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(json_body(response).await["data"][0]["b64_json"], "QUJD");
    let response = call(
        &state,
        "/v1/images/generations",
        json!({"model":"combo/image","prompt":"test","response_format":"b64_json"}),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(json_body(response).await["data"][0]["b64_json"], "aW1hZ2U=");
    let response = call(
        &state,
        "/v1/audio/speech",
        json!({"model":"combo/tts","input":"Hello","voice":"native-voice"}),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(body_bytes(response).await, "ABC");
    let response = upload_call(
        &state,
        "/v1/audio/transcriptions",
        upload(
            "combo/stt",
            "--test-boundary\r\nContent-Disposition: form-data; name=\"language\"\r\n\r\nen\r\n",
        ),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(json_body(response).await["text"], "hello");
}

#[tokio::test]
async fn video_jobs_poll_only_selected_provider_and_never_fail_over_after_acceptance() {
    for protocol in [Protocol::Xai, Protocol::MiniMax] {
        for success in [true, false] {
            let mock=spawn_upstream(Router::new()
                .route("/videos/generations",post(||async{Json(json!({"request_id":"task-1"}))}))
                .route("/videos/task-1",get(move||async move{if success{Json(json!({"status":"done","video":{"url":"https://example.com/video.mp4"}})).into_response()}else{StatusCode::SERVICE_UNAVAILABLE.into_response()}}))
                .route("/video_generation",post(||async{Json(json!({"base_resp":{"status_code":0},"task_id":"task-1"}))}))
                .route("/query/video_generation",get(move||async move{if success{Json(json!({"base_resp":{"status_code":0},"status":"Success","file_id":"file-1"})).into_response()}else{StatusCode::SERVICE_UNAVAILABLE.into_response()}}))
                .route("/files/retrieve",get(||async{Json(json!({"base_resp":{"status_code":0},"file":{"download_url":"https://example.com/video.mp4"}}))}))).await;
            let backup = spawn_upstream(Router::new().fallback(|| async {
                Json(json!({"data":[{"url":"https://example.com/backup.mp4"}]}))
            }))
            .await;
            let state = state().await;
            provider(&state, "native", &mock.url(), protocol).await;
            provider(&state, "backup", &backup.url(), Protocol::OpenAi).await;
            combo(
                &state,
                "video",
                &[("native", "video-model"), ("backup", "backup")],
            )
            .await;
            let response = call(
                &state,
                "/v1/videos/generations",
                json!({"model":"combo/video","prompt":"test","duration":6}),
            )
            .await;
            assert_eq!(
                response.status(),
                if success {
                    StatusCode::OK
                } else {
                    StatusCode::BAD_GATEWAY
                }
            );
            if success {
                assert_eq!(
                    json_body(response).await["data"][0]["url"],
                    "https://example.com/video.mp4"
                );
            }
            assert_eq!(backup.hits.count(), 0);
        }
    }
}

#[tokio::test]
async fn media_scopes_limits_validation_and_direct_namespace_are_shared() {
    let mock =
        spawn_upstream(Router::new().fallback(|| async {
            Json(json!({"data":[{"url":"https://example.com/image.png"}]}))
        }))
        .await;
    let state = state().await;
    provider(&state, "a", &mock.url(), Protocol::OpenAi).await;
    connection_models::replace(
        &state.db,
        "a",
        &[connection_models::StoredModel {
            id: "vendor/nested/image".into(),
            name: "Image".into(),
            fields: vec![],
            free: false,
        }],
        false,
    )
    .await
    .unwrap();
    let model = connection_models::routable_model_ids(&state.db)
        .await
        .unwrap()
        .remove(0);
    let minted = db::api_keys::create_with_limits(
        &state.db,
        &state.crypto,
        "media",
        db::api_keys::KeyAccess::Selected(vec![model.clone()]),
        db::api_keys::Expiry::Never,
        db::api_keys::KeyLimits {
            requests_per_minute: Some(1),
            max_concurrent: Some(1),
        },
    )
    .await
    .unwrap();
    let router = app::router(state.clone());
    for (target, expected) in [
        ("combo/forbidden".to_owned(), StatusCode::FORBIDDEN),
        (model.clone(), StatusCode::OK),
        (model.clone(), StatusCode::TOO_MANY_REQUESTS),
    ] {
        let response = router
            .clone()
            .oneshot(
                Request::post("/v1/images/generations")
                    .header(
                        "authorization",
                        format!("Bearer {}", minted.secret.expose_secret()),
                    )
                    .header("content-type", "application/json")
                    .body(Body::from(
                        json!({"model":target,"prompt":"test"}).to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), expected);
        let _ = body_bytes(response).await;
    }
    assert_eq!(mock.hits.count(), 1);
    assert_eq!(
        call(
            &state,
            "/v1/images/generations",
            json!({"model":model,"prompt":"test"})
        )
        .await
        .status(),
        StatusCode::UNAUTHORIZED
    );
}

#[tokio::test]
async fn binary_speech_permit_lasts_until_body_drop_and_late_failure_never_falls_back() {
    let (send, receive) = tokio::sync::oneshot::channel::<()>();
    let receive = std::sync::Arc::new(tokio::sync::Mutex::new(Some(receive)));
    let mock=spawn_upstream(Router::new().fallback(move||{let receive=receive.clone();async move{
        let stream=async_stream::stream!{yield Ok::<_,std::io::Error>(Bytes::from_static(b"first-audio"));if let Some(receive)=receive.lock().await.take(){let _=receive.await;}yield Err(std::io::Error::other("private-upstream-url-and-key"));};
        let mut response=Response::new(Body::from_stream(stream));response.headers_mut().insert(header::CONTENT_TYPE,"audio/mpeg".parse().unwrap());response
    }})).await;
    let backup = spawn_upstream(
        Router::new().fallback(|| async { ([("content-type", "audio/mpeg")], "backup") }),
    )
    .await;
    let state = state().await;
    provider(&state, "a", &mock.url(), Protocol::OpenAi).await;
    provider(&state, "b", &backup.url(), Protocol::OpenAi).await;
    combo(&state, "tts", &[("a", "a"), ("b", "b")]).await;
    let minted = db::api_keys::create_with_limits(
        &state.db,
        &state.crypto,
        "speech",
        db::api_keys::KeyAccess::All,
        db::api_keys::Expiry::Never,
        db::api_keys::KeyLimits {
            requests_per_minute: None,
            max_concurrent: Some(1),
        },
    )
    .await
    .unwrap();
    let make = || {
        Request::post("/v1/audio/speech")
            .header(
                "authorization",
                format!("Bearer {}", minted.secret.expose_secret()),
            )
            .header("content-type", "application/json")
            .body(Body::from(
                json!({"model":"combo/tts","input":"Hello"}).to_string(),
            ))
            .unwrap()
    };
    let router = app::router(state.clone());
    let response = router.clone().oneshot(make()).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let mut body = response.into_body().into_data_stream();
    assert_eq!(body.next().await.unwrap().unwrap(), "first-audio");
    assert_eq!(
        router.clone().oneshot(make()).await.unwrap().status(),
        StatusCode::TOO_MANY_REQUESTS
    );
    send.send(()).unwrap();
    assert!(body.next().await.unwrap().is_err());
    drop(body);
    assert_eq!(backup.hits.count(), 0);
    let resumed = router.oneshot(make()).await.unwrap();
    assert_ne!(resumed.status(), StatusCode::TOO_MANY_REQUESTS);
    drop(resumed);
}

#[tokio::test]
async fn binary_wrong_content_type_and_empty_response_retry_before_commit() {
    for (mime, content) in [
        ("text/html", "not-audio"),
        ("application/json", "not-audio"),
        ("audio/mpeg", ""),
    ] {
        let bad = spawn_upstream(
            Router::new().fallback(move || async move { ([("content-type", mime)], content) }),
        )
        .await;
        let good = spawn_upstream(
            Router::new().fallback(|| async { ([("content-type", "audio/mpeg")], "audio") }),
        )
        .await;
        let state = state().await;
        provider(&state, "a", &bad.url(), Protocol::OpenAi).await;
        provider(&state, "b", &good.url(), Protocol::OpenAi).await;
        combo(&state, "tts", &[("a", "a"), ("b", "b")]).await;
        let response = call(
            &state,
            "/v1/audio/speech",
            json!({"model":"combo/tts","input":"Hello"}),
        )
        .await;
        assert_eq!(body_bytes(response).await, "audio");
        assert_eq!(good.hits.count(), 1);
    }
}

#[tokio::test]
async fn media_redirects_never_leak_credentials() {
    let destination = spawn_upstream(Router::new().fallback(|| async { StatusCode::OK })).await;
    let url = destination.url();
    let redirect = spawn_upstream(Router::new().fallback(move || {
        let url = url.clone();
        async move {
            (
                StatusCode::TEMPORARY_REDIRECT,
                [("location", url)],
                "private-error",
            )
        }
    }))
    .await;
    let state = state().await;
    provider(&state, "a", &redirect.url(), Protocol::OpenAi).await;
    combo(&state, "image", &[("a", "a")]).await;
    assert_eq!(
        call(
            &state,
            "/v1/images/generations",
            json!({"model":"combo/image","prompt":"test"})
        )
        .await
        .status(),
        StatusCode::BAD_GATEWAY
    );
    assert_eq!(destination.hits.count(), 0);
}

#[tokio::test]
async fn media_is_metadata_only_even_in_extended_logging_and_binary_finishes_on_eof() {
    let mock = spawn_upstream(
        Router::new().fallback(|| async { ([("content-type", "audio/mpeg")], "private-audio") }),
    )
    .await;
    let state = state().await;
    provider(&state, "a", &mock.url(), Protocol::OpenAi).await;
    combo(&state, "tts", &[("a", "a")]).await;
    db::settings::set_logging_mode(&state.db, db::settings::LoggingMode::Extended)
        .await
        .unwrap();
    let response = call(
        &state,
        "/v1/audio/speech",
        json!({"model":"combo/tts","input":"private-prompt"}),
    )
    .await;
    assert_eq!(body_bytes(response).await, "private-audio");
    // Finalization runs in the background so the body end never waits on SQLite.
    let mut outcome = String::new();
    for _ in 0..2_000 {
        outcome = sqlx::query_scalar("SELECT outcome FROM request_history LIMIT 1")
            .fetch_optional(&state.db)
            .await
            .unwrap()
            .unwrap_or_default();
        if outcome == "success" {
            break;
        }
        tokio::task::yield_now().await;
    }
    assert_eq!(outcome, "success");
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM request_payloads")
        .fetch_one(&state.db)
        .await
        .unwrap();
    assert_eq!(count, 0);
}

#[tokio::test]
async fn fresh_schema_supports_all_media_protocols_and_grouped_catalog_defaults() {
    let state = state().await;
    for protocol in [
        Protocol::Fal,
        Protocol::MiniMax,
        Protocol::Xai,
        Protocol::Deepgram,
        Protocol::ElevenLabs,
    ] {
        let group = db::provider_groups::create(
            &state.db,
            protocol.as_str(),
            "http://localhost/v1",
            &protocol,
        )
        .await
        .unwrap();
        assert_eq!(group.protocol, protocol);
    }
    let response = app::router(state.clone())
        .oneshot(Request::get("/providers").body(Body::empty()).unwrap())
        .await
        .unwrap();
    let html = String::from_utf8(body_bytes(response).await.to_vec()).unwrap();
    assert_eq!(
        html.matches("href=\"/providers/catalog/openai\"").count(),
        2 // Favorites shortcut plus the single All providers tile.
    );
    assert_eq!(
        html.matches("href=\"/providers/catalog/minimax\"").count(),
        1
    );
    assert!(html.contains("data-provider-tags=\"text image tts stt\""));
    assert!(html.contains("data-provider-tags=\"music tts stt\""));
    for label in ["fal.ai", "Deepgram", "ElevenLabs", "MiniMax", "xAI"] {
        assert!(!html.contains(&format!("<span class=\"badge\">{label}</span>")));
    }
    assert!(!html.contains("OpenAI-video"));
    // A newly added preset must not silently take an existing custom namespace.
    // Use current-schema fixtures, not a historical migration/upgrade test.
    let owner: String =
        sqlx::query_scalar("SELECT id FROM custom_provider_groups WHERE name = 'fal'")
            .fetch_one(&state.db)
            .await
            .unwrap();
    sqlx::query("UPDATE provider_slugs SET slug = 'fal' WHERE group_id = ?")
        .bind(&owner)
        .execute(&state.db)
        .await
        .unwrap();
    let error = db::providers::save(
        &state.db,
        &Provider {
            id: "builtin-fal".into(),
            name: "Builtin".into(),
            base_url: "https://queue.fal.run".into(),
            protocol: Protocol::Fal,
            encrypted_api_key: vec![],
            enabled: true,
        },
    )
    .await;
    assert!(error.is_err());
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM providers WHERE id = 'builtin-fal'")
        .fetch_one(&state.db)
        .await
        .unwrap();
    assert_eq!(count, 0);
}

#[tokio::test]
async fn fal_queue_polls_same_origin_and_refuses_credential_exfiltration() {
    for hostile in [false, true] {
        let destination = spawn_upstream(Router::new().fallback(|| async { StatusCode::OK })).await;
        let destination_url = destination.url();
        let queue = spawn_upstream(Router::new().fallback(move |uri: axum::http::Uri, headers: HeaderMap| {
            let destination_url = destination_url.clone();
            async move {
                assert_eq!(headers[header::AUTHORIZATION], "Key fake-upstream-key");
                if uri.path().ends_with("/status") { return Json(json!({"status":"COMPLETED"})); }
                if uri.path().contains("/requests/") { return Json(json!({"images":[{"url":"https://example.com/image.png"}]})); }
                let origin = if hostile { destination_url } else { format!("http://{}", headers[header::HOST].to_str().unwrap()) };
                Json(json!({"request_id":"task-1", "status_url":format!("{origin}/fal-ai/test/requests/task-1/status"), "response_url":format!("{origin}/fal-ai/test/requests/task-1")}))
            }
        })).await;
        let backup = spawn_upstream(Router::new().fallback(|| async {
            Json(json!({"data":[{"url":"https://example.com/backup.png"}]}))
        }))
        .await;
        let state = state().await;
        provider(&state, "fal", &queue.url(), Protocol::Fal).await;
        provider(&state, "backup", &backup.url(), Protocol::OpenAi).await;
        combo(
            &state,
            "image",
            &[("fal", "fal-ai/test"), ("backup", "backup")],
        )
        .await;
        let response = call(
            &state,
            "/v1/images/generations",
            json!({"model":"combo/image","prompt":"test"}),
        )
        .await;
        assert_eq!(
            response.status(),
            if hostile {
                StatusCode::BAD_GATEWAY
            } else {
                StatusCode::OK
            }
        );
        if !hostile {
            assert_eq!(
                json_body(response).await["data"][0]["url"],
                "https://example.com/image.png"
            );
        }
        assert_eq!(queue.hits.count(), if hostile { 1 } else { 3 });
        assert_eq!(destination.hits.count(), 0);
        assert_eq!(backup.hits.count(), 0);
    }
}

#[tokio::test]
async fn media_boundary_rejections_never_call_providers() {
    let mock = spawn_upstream(Router::new().fallback(|| async { StatusCode::OK })).await;
    let state = state().await;
    provider(&state, "a", &mock.url(), Protocol::OpenAi).await;
    combo(&state, "media", &[("a", "a")]).await;
    for body in [
        json!([]),
        json!({"model":"combo/media"}),
        json!({"model":"combo/media", "prompt":"x", "stream":true}),
        json!({"model":"combo/media", "prompt":"x", "stream":"false"}),
    ] {
        assert_eq!(
            call(&state, "/v1/images/generations", body).await.status(),
            StatusCode::BAD_REQUEST
        );
    }
    let duplicate =
        "--test-boundary\r\nContent-Disposition: form-data; name=\"model\"\r\n\r\nother\r\n";
    assert_eq!(
        upload_call(
            &state,
            "/v1/audio/transcriptions",
            upload("combo/media", duplicate)
        )
        .await
        .status(),
        StatusCode::BAD_REQUEST
    );
    let extra = format!(
        "--test-boundary\r\nContent-Disposition: form-data; name=\"prompt\"\r\n\r\n{}\r\n",
        "x".repeat(65537)
    );
    assert_eq!(
        upload_call(
            &state,
            "/v1/audio/transcriptions",
            upload("combo/media", &extra)
        )
        .await
        .status(),
        StatusCode::PAYLOAD_TOO_LARGE
    );
    let huge = upload("combo/media", "").replace("RIFF-audio-bytes", &"x".repeat(33 * 1024 * 1024));
    assert_eq!(
        upload_call(&state, "/v1/audio/transcriptions", huge)
            .await
            .status(),
        StatusCode::PAYLOAD_TOO_LARGE
    );
    assert_eq!(mock.hits.count(), 0);
    let large = spawn_upstream(
        Router::new()
            .route(
                "/audio/transcriptions",
                post(|bytes: Bytes| async move {
                    assert!(bytes.len() > 4 * 1024 * 1024);
                    Json(json!({"text":"large upload"}))
                }),
            )
            .layer(axum::extract::DefaultBodyLimit::max(32 * 1024 * 1024)),
    )
    .await;
    provider(&state, "large", &large.url(), Protocol::OpenAi).await;
    combo(&state, "large", &[("large", "whisper")]).await;
    let valid = upload("combo/large", "").replace("RIFF-audio-bytes", &"x".repeat(5 * 1024 * 1024));
    let response = upload_call(&state, "/v1/audio/transcriptions", valid).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(json_body(response).await["text"], "large upload");
}

#[tokio::test]
async fn accepted_malformed_video_success_never_creates_backup_job() {
    for protocol in [Protocol::MiniMax, Protocol::Xai, Protocol::OpenAi] {
        let bad = spawn_upstream(Router::new().fallback(move || async move {
            (
                if protocol == Protocol::OpenAi {
                    StatusCode::ACCEPTED
                } else {
                    StatusCode::OK
                },
                Json(json!({"base_resp":{"status_code":0}})),
            )
        }))
        .await;
        let backup = spawn_upstream(Router::new().fallback(|| async {
            Json(json!({"data":[{"url":"https://example.com/backup.mp4"}]}))
        }))
        .await;
        let state = state().await;
        provider(&state, "a", &bad.url(), protocol).await;
        provider(&state, "b", &backup.url(), Protocol::OpenAi).await;
        combo(&state, "video", &[("a", "video"), ("b", "backup")]).await;
        assert_eq!(
            call(
                &state,
                "/v1/videos/generations",
                json!({"model":"combo/video","prompt":"test"})
            )
            .await
            .status(),
            StatusCode::BAD_GATEWAY
        );
        assert_eq!(backup.hits.count(), 0);
    }
}

#[tokio::test]
async fn native_model_refresh_and_credential_free_protocol_transfer() {
    for (protocol, body, expected) in [
        (
            Protocol::ElevenLabs,
            json!([{"model_id":"eleven_v3","name":"Eleven v3"}]),
            "eleven_v3",
        ),
        (
            Protocol::Deepgram,
            json!({"stt":[{"canonical_name":"nova-3"}],"tts":[{"canonical_name":"aura-2-thalia-en"}]}),
            "nova-3",
        ),
        (
            Protocol::MiniMax,
            json!({"data":[{"id":"MiniMax-M3"}]}),
            "MiniMax-M3",
        ),
    ] {
        let mock = spawn_upstream(Router::new().route(
            "/models",
            get(move || {
                let body = body.clone();
                async move { Json(body) }
            }),
        ))
        .await;
        let state = state().await;
        provider(&state, "a", &mock.url(), protocol).await;
        let saved = db::providers::get(&state.db, "a").await.unwrap();
        let listed = sparkrouter::providers::list_models(&state.client, &state.crypto, &saved)
            .await
            .unwrap();
        assert!(listed.models.iter().any(|model| model.id == expected));
        if protocol == Protocol::MiniMax {
            assert!(listed.models.iter().any(|model| model.id == "music-3.0"));
        }
        if protocol == Protocol::ElevenLabs {
            assert!(listed.models.iter().any(|model| model.id == "music_v1"));
        }
    }
    let state = state().await;
    for protocol in [
        Protocol::Fal,
        Protocol::MiniMax,
        Protocol::Xai,
        Protocol::Deepgram,
        Protocol::ElevenLabs,
    ] {
        let group = db::provider_groups::create(
            &state.db,
            protocol.as_str(),
            "http://localhost/v1",
            &protocol,
        )
        .await
        .unwrap();
        let id = protocol.as_str();
        db::provider_groups::create_connection(
            &state.db,
            &group.id,
            &Provider {
                id: id.into(),
                name: "Key".into(),
                base_url: group.base_url.clone(),
                protocol,
                encrypted_api_key: state
                    .crypto
                    .encrypt_api_key(id, &SecretString::from("private-media-key"))
                    .unwrap(),
                enabled: true,
            },
        )
        .await
        .unwrap();
    }
    let exported = db::config_transfer::export(&state.db).await.unwrap();
    let json = serde_json::to_string(&exported).unwrap();
    assert!(!json.contains("private-media-key"));
    let target = self::state().await;
    db::config_transfer::apply(&target.db, &json, &|_, _| false)
        .await
        .unwrap();
    for group in db::provider_groups::list(&target.db).await.unwrap() {
        let members = db::provider_groups::members(&target.db, &group.group.id)
            .await
            .unwrap();
        assert_eq!(members.len(), 1);
        assert!(!members[0].enabled);
        assert!(members[0].encrypted_api_key.is_empty());
        assert_eq!(members[0].protocol, group.group.protocol);
    }
}
