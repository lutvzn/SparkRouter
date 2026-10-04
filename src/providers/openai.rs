use axum::response::Response;
use bytes::Bytes;
use futures_util::{Stream, StreamExt};
use serde_json::Value;

use super::sse::SseDecoder;
use super::transport_error;
use super::{
    AttemptError, ResponseStream, check_sse, check_status, json_response, read_bounded,
    replace_model, request, streaming_response,
};
use crate::{crypto::Crypto, db::providers::Provider};

pub async fn call(
    client: &reqwest::Client,
    crypto: &Crypto,
    provider: &Provider,
    model: &str,
    body: &Value,
) -> Result<Response, AttemptError> {
    let body = replace_model(body, model)?;
    let streaming = body.get("stream").and_then(Value::as_bool).unwrap_or(false);
    let response = request(
        client,
        crypto,
        provider,
        reqwest::Method::POST,
        "chat/completions",
    )?
    .json(&body)
    .send()
    .await
    .map_err(transport_error)?;
    check_status(&response)?;
    if streaming {
        check_sse(&response)?;
        let source = response
            .bytes_stream()
            .map(|chunk| chunk.map_err(transport_error));
        streaming_response(done_terminated(source)).await
    } else {
        let status = response.status();
        let bytes = read_bounded(response).await?;
        // Validate the completion envelope, but preserve original bytes and
        // unknown fields rather than enforcing a full provider response schema.
        let completion: Value =
            serde_json::from_slice(&bytes).map_err(|_| AttemptError::Retryable)?;
        if completion
            .get("choices")
            .and_then(Value::as_array)
            .is_none()
            || completion
                .get("error")
                .is_some_and(|error| !error.is_null())
        {
            return Err(AttemptError::Retryable);
        }
        Ok(json_response(status, bytes))
    }
}

/// Raw OpenAI SSE passthrough with terminal-event tracking.
///
/// Bytes are forwarded verbatim: framing, unknown fields, and payloads are
/// never parsed or re-encoded. The shared framing decoder only watches for
/// the terminal `data: [DONE]` event (no JSON parsing); the chunk that
/// completes it — including any trailing bytes after it in that chunk — is
/// forwarded unchanged, then the upstream body is dropped so a finished
/// generation does not wait for the connection to close or time out. Framing
/// obeys the shared 1 MiB per-event bound.
///
/// A clean HTTP EOF before the terminal event is a truncated stream: while
/// still precommit (no bytes forwarded yet, e.g. an empty upstream body) it
/// stays a retryable attempt; after commitment `streaming_response` turns it
/// into a terminal body error with no fallback.
fn done_terminated<S>(source: S) -> ResponseStream
where
    S: Stream<Item = Result<Bytes, AttemptError>> + Send + 'static,
{
    Box::pin(async_stream::try_stream! {
        futures_util::pin_mut!(source);
        let mut decoder = SseDecoder::default();
        let mut done = false;
        while !done {
            let bytes = match source.next().await { Some(result) => result?, None => break };
            for byte in &bytes {
                if decoder.push(*byte)?.is_some_and(|event| event.data.trim() == "[DONE]") {
                    done = true;
                    break;
                }
            }
            if !bytes.is_empty() { yield bytes; }
        }
        // A clean EOF without the terminal event means the generation was cut
        // short, not that it completed.
        if !done { Err(AttemptError::Retryable)?; }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::stream;
    use std::time::Duration;

    // Comments, CRLF framing, multi-event chunks, a padded terminal event.
    const WIRE: &[u8] = b": stream opened\r\n\r\ndata: {\"id\":\"x\",\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\r\n\r\nevent: ping\r\ndata: {\"unknown\":{\"nested\":[1,2]}}\r\n\r\ndata: [DONE]  \r\n\r\n";

    async fn collect(stream: ResponseStream) -> Result<Vec<u8>, AttemptError> {
        let mut forwarded = Vec::new();
        for chunk in stream.collect::<Vec<_>>().await {
            forwarded.extend_from_slice(&chunk?);
        }
        Ok(forwarded)
    }

    #[tokio::test]
    async fn raw_bytes_are_preserved_whole_and_fragmented() {
        let whole = done_terminated(stream::iter(vec![Ok(Bytes::from_static(WIRE))]));
        assert_eq!(collect(whole).await.unwrap(), WIRE);
        let fragmented = WIRE
            .iter()
            .map(|byte| Ok(Bytes::from(vec![*byte])))
            .collect::<Vec<_>>();
        let split = done_terminated(stream::iter(fragmented));
        assert_eq!(collect(split).await.unwrap(), WIRE);
    }

    #[tokio::test]
    async fn eof_without_terminal_event_is_truncation() {
        let mut stream = done_terminated(stream::iter(vec![Ok(Bytes::from_static(
            b"data: {\"a\":1}\n\n",
        ))]));
        let forwarded = stream.next().await.unwrap().unwrap();
        assert_eq!(forwarded.as_ref(), b"data: {\"a\":1}\n\n");
        assert!(matches!(
            stream.next().await,
            Some(Err(AttemptError::Retryable))
        ));
        // An empty upstream body stays a precommit retry: nothing forwarded.
        let mut empty = done_terminated(stream::empty());
        assert!(matches!(
            empty.next().await,
            Some(Err(AttemptError::Retryable))
        ));
    }

    #[tokio::test]
    async fn terminal_event_stops_reading_upstream() {
        let chunk =
            Bytes::from_static(b"data: {\"a\":1}\n\ndata: [DONE]\n\n: trailing keepalive\n\n");
        let mut exhausted = false;
        // Stays pending forever after the first chunk; completing without
        // polling it proves [DONE] alone terminates the passthrough.
        let source = futures_util::stream::poll_fn(move |_context| {
            if exhausted {
                std::task::Poll::Pending
            } else {
                exhausted = true;
                std::task::Poll::Ready(Some(Ok(chunk.clone())))
            }
        });
        let received = tokio::time::timeout(
            Duration::from_secs(1),
            done_terminated(source).collect::<Vec<_>>(),
        )
        .await
        .expect("terminal event must complete without waiting for upstream EOF");
        assert_eq!(received.len(), 1);
        assert!(received[0].is_ok());
    }
}
