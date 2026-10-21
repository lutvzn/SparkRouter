//! Master-key verification at startup and offline rotation, end to end through
//! `AppState`: credentials re-encrypted, the old key refused, and generated
//! gateway keys still accepted by `/v1` after rotation.

use axum::{body::Body, http::Request};
use base64::{Engine, engine::general_purpose::STANDARD};
use secrecy::{ExposeSecret, SecretString};
use sparkrouter::{
    app::{self, AppState},
    config::{Config, DashboardLogin},
    crypto::Crypto,
    db,
    error::AppError,
};
use tower::ServiceExt;

fn crypto(byte: u8) -> Crypto {
    Crypto::from_base64(&SecretString::from(STANDARD.encode([byte; 32]))).unwrap()
}

async fn start(url: &str, byte: u8) -> Result<AppState, AppError> {
    AppState::from_config(Config {
        database_url: url.into(),
        bind: "127.0.0.1:0".parse().unwrap(),
        crypto: crypto(byte),
        api_key: None,
        dashboard_login: DashboardLogin::Disabled,
        dashboard_secure_cookie: false,
    })
    .await
}

#[tokio::test]
async fn rotation_moves_the_database_to_the_new_key_and_keeps_api_keys_working() {
    let directory = tempfile::tempdir().unwrap();
    let url = format!("sqlite://{}", directory.path().join("keys.db").display());
    let state = start(&url, 1).await.unwrap();
    db::providers::save(
        &state.db,
        &db::providers::Provider {
            id: "upstream".into(),
            name: "Upstream".into(),
            base_url: "https://api.example.test/v1".into(),
            protocol: sparkrouter::providers::Protocol::OpenAi,
            encrypted_api_key: state
                .crypto
                .encrypt_api_key("upstream", &SecretString::from("sk-upstream"))
                .unwrap(),
            enabled: true,
        },
    )
    .await
    .unwrap();
    let issued = db::api_keys::create(&state.db, &state.crypto, "client")
        .await
        .unwrap();
    state.db.close().await;

    // A different key never starts against this database.
    assert!(matches!(
        start(&url, 2).await.err(),
        Some(AppError::Configuration(message)) if message.contains("does not match")
    ));

    let pool = db::connect(&url).await.unwrap();
    let rotation = db::master_key::rotate(&pool, &crypto(1), &crypto(2))
        .await
        .unwrap();
    assert_eq!(rotation.provider_credentials, 1);
    assert_eq!(rotation.unrevealed_api_keys, 1);
    pool.close().await;

    assert!(start(&url, 1).await.is_err());
    let state = start(&url, 2).await.unwrap();
    let provider = db::providers::get(&state.db, "upstream").await.unwrap();
    assert_eq!(
        state
            .crypto
            .decrypt_api_key("upstream", &provider.encrypted_api_key)
            .unwrap()
            .expose_secret(),
        "sk-upstream"
    );
    let router = app::router(state.clone());
    let models = |authorization: String| {
        router.clone().oneshot(
            Request::get("/v1/models")
                .header("authorization", authorization)
                .body(Body::empty())
                .unwrap(),
        )
    };
    let accepted = models(format!("Bearer {}", issued.secret.expose_secret()))
        .await
        .unwrap();
    assert_eq!(accepted.status(), 200);
    let rejected = models("Bearer sk-not-a-key".into()).await.unwrap();
    assert_eq!(rejected.status(), 401);
}
