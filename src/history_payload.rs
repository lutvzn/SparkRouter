//! Opt-in bounded gateway-body capture. Never emits body content to tracing.
use crate::{
    app::AppState,
    db::{
        request_payloads::{self, BODY_LIMIT},
        settings::{self, LoggingMode},
    },
};
use axum::{body::Body, response::Response};
use futures_util::StreamExt;
use serde_json::Value;
use std::time::Duration;

// No Debug: this can hold private conversation data.
#[derive(Clone)]
pub(crate) struct Capture {
    pub mode: LoggingMode,
    pub request: String,
    pub truncated: bool,
}

impl Capture {
    pub async fn start(state: &AppState, body: &Value) -> Self {
        // Failure disables recording rather than accidentally persisting bodies.
        let mode = match tokio::time::timeout(
            Duration::from_millis(200),
            settings::logging_mode(&state.db),
        )
        .await
        {
            Ok(Ok(mode)) => mode,
            _ => {
                tracing::warn!("request logging preference unavailable");
                LoggingMode::Off
            }
        };
        let mut request = if mode == LoggingMode::Extended {
            body.to_string()
        } else {
            String::new()
        };
        let truncated = request.len() > BODY_LIMIT;
        if truncated {
            let mut end = BODY_LIMIT;
            while !request.is_char_boundary(end) {
                end -= 1;
            }
            request.truncate(end);
        }
        Self {
            mode,
            request,
            truncated,
        }
    }
}

pub(crate) fn wrap(
    response: Response,
    pool: sqlx::SqlitePool,
    id: String,
    gate: crate::history::InsertGate,
) -> Response {
    let guard = PayloadGuard {
        pool,
        id,
        gate,
        bytes: Vec::new(),
        truncated: false,
        finished: false,
        complete: None,
    };
    let (parts, body) = response.into_parts();
    let stream = async_stream::stream! {
        let mut guard = guard;
        let mut source = body.into_data_stream();
        while let Some(chunk) = source.next().await {
            if let Ok(bytes) = &chunk {
                let remaining = BODY_LIMIT - guard.bytes.len();
                guard.truncated |= bytes.len() > remaining;
                guard.bytes.extend_from_slice(&bytes[..bytes.len().min(remaining)]);
            }
            let failed = chunk.is_err();
            if failed { guard.finish(false).await; }
            yield chunk;
            if failed { return; }
        }
        guard.finish(true).await;
    };
    Response::from_parts(parts, Body::from_stream(stream))
}

struct PayloadGuard {
    pool: sqlx::SqlitePool,
    id: String,
    gate: crate::history::InsertGate,
    bytes: Vec<u8>,
    truncated: bool,
    finished: bool,
    complete: Option<bool>,
}

impl PayloadGuard {
    async fn finish(&mut self, complete: bool) {
        if self.finished {
            return;
        }
        self.complete = Some(complete);
        self.finished = write(
            &self.pool,
            &self.id,
            &self.gate,
            &self.bytes,
            self.truncated,
            complete,
        )
        .await;
    }
}

async fn write(
    pool: &sqlx::SqlitePool,
    id: &str,
    gate: &crate::history::InsertGate,
    bytes: &[u8],
    truncated: bool,
    complete: bool,
) -> bool {
    matches!(
        tokio::time::timeout(Duration::from_secs(2), async {
            if gate.wait().await {
                let mut text = String::from_utf8_lossy(bytes).into_owned();
                let truncated = truncated || text.len() > BODY_LIMIT;
                if text.len() > BODY_LIMIT {
                    let mut end = BODY_LIMIT;
                    while !text.is_char_boundary(end) {
                        end -= 1;
                    }
                    text.truncate(end);
                }
                request_payloads::finish(pool, id, &text, truncated, complete).await?;
            }
            Ok::<_, crate::error::AppError>(())
        })
        .await,
        Ok(Ok(()))
    )
}

impl Drop for PayloadGuard {
    fn drop(&mut self) {
        if self.finished {
            return;
        }
        let pool = self.pool.clone();
        let id = self.id.clone();
        let gate = self.gate.clone();
        let bytes = std::mem::take(&mut self.bytes);
        let truncated = self.truncated;
        let complete = self.complete.unwrap_or(false);
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(async move {
                if !write(&pool, &id, &gate, &bytes, truncated, complete).await {
                    tracing::warn!("request payload write unavailable");
                }
            });
        }
    }
}
