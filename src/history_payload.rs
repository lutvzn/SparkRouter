//! Opt-in bounded gateway-body capture. Never emits body content to tracing.
use crate::{
    app::AppState,
    db::{
        history_writer::HistoryWriter,
        request_payloads::BODY_LIMIT,
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
        // Only the wait for a connection is bounded: cancelling the query after
        // it started would make the pool discard and reopen that connection.
        let mode = match tokio::time::timeout(Duration::from_millis(200), state.db.acquire()).await
        {
            Ok(Ok(mut connection)) => settings::logging_mode(&mut *connection).await.ok(),
            _ => None,
        };
        let mode = mode.unwrap_or_else(|| {
            tracing::warn!("request logging preference unavailable");
            LoggingMode::Off
        });
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

pub(crate) fn wrap(response: Response, history: HistoryWriter, id: String) -> Response {
    let guard = PayloadGuard {
        history,
        id,
        bytes: Vec::new(),
        truncated: false,
        finished: false,
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
            if failed { guard.finish(false); }
            yield chunk;
            if failed { return; }
        }
        guard.finish(true);
    };
    Response::from_parts(parts, Body::from_stream(stream))
}

struct PayloadGuard {
    history: HistoryWriter,
    id: String,
    bytes: Vec<u8>,
    truncated: bool,
    finished: bool,
}

impl PayloadGuard {
    /// Queue the capture once; it is written after the record's insert, so
    /// the end of the body never waits on SQLite.
    fn finish(&mut self, complete: bool) {
        if std::mem::replace(&mut self.finished, true) {
            return;
        }
        let mut text = String::from_utf8_lossy(&std::mem::take(&mut self.bytes)).into_owned();
        let truncated = self.truncated || text.len() > BODY_LIMIT;
        if text.len() > BODY_LIMIT {
            let mut end = BODY_LIMIT;
            while !text.is_char_boundary(end) {
                end -= 1;
            }
            text.truncate(end);
        }
        self.history
            .finish_payload(self.id.clone(), text, truncated, complete);
    }
}

impl Drop for PayloadGuard {
    fn drop(&mut self) {
        self.finish(false);
    }
}
