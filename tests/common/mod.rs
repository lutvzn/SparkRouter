//! Shared local upstream harness; dropping a mock aborts its server task.

use axum::{Router, extract::Request, middleware::Next, response::Response};
use bytes::Bytes;
use http_body_util::BodyExt;
use serde_json::Value;
use std::{
    net::SocketAddr,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};

#[derive(Clone, Default)]
pub struct Hits(Arc<AtomicUsize>);

impl Hits {
    pub fn count(&self) -> usize {
        self.0.load(Ordering::SeqCst)
    }
}

pub struct MockUpstream {
    addr: SocketAddr,
    pub hits: Hits,
    task: tokio::task::JoinHandle<()>,
}

impl MockUpstream {
    pub fn url(&self) -> String {
        format!("http://{}", self.addr)
    }
}

impl Drop for MockUpstream {
    fn drop(&mut self) {
        self.task.abort();
    }
}

pub async fn spawn_upstream(router: Router) -> MockUpstream {
    let hits = Hits::default();
    let counted = hits.clone();
    let app = router.layer(axum::middleware::from_fn(
        move |request: Request, next: Next| {
            let counted = counted.clone();
            async move {
                counted.0.fetch_add(1, Ordering::SeqCst);
                next.run(request).await
            }
        },
    ));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind mock upstream");
    let addr = listener.local_addr().expect("mock upstream local address");
    let task = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    MockUpstream { addr, hits, task }
}

pub async fn body_bytes(response: Response) -> Bytes {
    response
        .into_body()
        .collect()
        .await
        .expect("read gateway response body")
        .to_bytes()
}

pub async fn json_body(response: Response) -> Value {
    let bytes = body_bytes(response).await;
    serde_json::from_slice(&bytes).unwrap_or_else(|error| {
        panic!(
            "gateway response is not JSON ({error}): {}",
            String::from_utf8_lossy(&bytes)
        )
    })
}
