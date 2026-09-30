use axum::{
    Router,
    http::{StatusCode, header},
};
use base64::{Engine, engine::general_purpose::STANDARD};
use secrecy::SecretString;
use serde_json::json;
use sparkrouter::{
    app::AppState,
    config::Config,
    crypto::Crypto,
    db::providers::Provider,
    providers::{self, Protocol},
};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

struct Server(tokio::task::JoinHandle<()>);
impl Drop for Server {
    fn drop(&mut self) {
        self.0.abort();
    }
}

async fn serve(router: Router) -> (String, Server) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let task = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    (url, Server(task))
}

#[tokio::test]
async fn redirects_never_forward_either_protocols_credentials() {
    let hits = Arc::new(AtomicUsize::new(0));
    let counter = hits.clone();
    let (destination, _destination_server) = serve(Router::new().fallback(move || {
        let counter = counter.clone();
        async move {
            counter.fetch_add(1, Ordering::SeqCst);
            StatusCode::OK
        }
    }))
    .await;
    let (source, _source_server) = serve(Router::new().fallback(move || {
        let destination = destination.clone();
        async move {
            (
                StatusCode::TEMPORARY_REDIRECT,
                [(header::LOCATION, destination)],
            )
        }
    }))
    .await;
    let state = AppState::from_config(Config {
        database_url: "sqlite::memory:".into(),
        bind: "127.0.0.1:0".parse().unwrap(),
        crypto: Crypto::from_base64(&SecretString::from(STANDARD.encode([23; 32]))).unwrap(),
        api_key: None,
    })
    .await
    .unwrap();
    for protocol in [Protocol::OpenAi, Protocol::Anthropic] {
        let provider = Provider {
            id: "provider".into(),
            name: "redirecting upstream".into(),
            base_url: source.clone(),
            protocol,
            enabled: true,
            encrypted_api_key: state
                .crypto
                .encrypt_api_key("provider", &SecretString::from("private-upstream-key"))
                .unwrap(),
        };
        assert!(
            providers::test_connection(&state.client, &state.crypto, &provider)
                .await
                .is_err()
        );
        assert!(
            providers::call(
                &state.client,
                &state.crypto,
                &provider,
                "model",
                &json!({
                    "model":"combo/test", "messages":[{"role":"user","content":"hello"}]
                })
            )
            .await
            .is_err()
        );
    }
    assert_eq!(hits.load(Ordering::SeqCst), 0);
}
