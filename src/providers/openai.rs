use std::ops::ControlFlow;

use axum::response::Response;
use bytes::Bytes;
use futures_util::{Stream, StreamExt};
use serde_json::Value;

use super::sse::{SseDecoder, SseEvent};
use super::transport_error;
use super::{
    AttemptError, ResponseStream, check_sse, check_status, json_response_with_usage, model_body,
    read_bounded, request, streaming_response,
};
use crate::{crypto::Crypto, db::providers::Provider};

/// POST the request with its model swapped to `endpoint` under the base URL.
async fn post(
    client: &reqwest::Client,
    crypto: &Crypto,
    provider: &Provider,
    endpoint: &str,
    model: &str,
    body: &Value,
) -> Result<reqwest::Response, AttemptError> {
    let response = request(client, crypto, provider, reqwest::Method::POST, endpoint)?
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .body(model_body(body, model)?)
        .send()
        .await
        .map_err(transport_error)?;
    check_status(&response)?;
    Ok(response)
}

fn upstream_events(response: reqwest::Response) -> impl Stream<Item = Result<Bytes, AttemptError>> {
    response
        .bytes_stream()
        .map(|chunk| chunk.map_err(transport_error))
}

pub async fn call(
    client: &reqwest::Client,
    crypto: &Crypto,
    provider: &Provider,
    model: &str,
    body: &Value,
) -> Result<Response, AttemptError> {
    let streaming = body.get("stream").and_then(Value::as_bool).unwrap_or(false);
    let response = post(client, crypto, provider, "chat/completions", model, body).await?;
    if streaming {
        check_sse(&response)?;
        streaming_response(terminated(upstream_events(response), is_done)).await
    } else {
        let status = response.status();
        let bytes = read_bounded(response).await?;
        // Validate the completion envelope, but preserve original bytes and
        // unknown fields rather than enforcing a full provider response schema.
        if bytes.trim_ascii_start().first() != Some(&b'{') {
            return Err(AttemptError::Retryable);
        }
        let completion: Completion =
            serde_json::from_slice(&bytes).map_err(|_| AttemptError::Retryable)?;
        if completion.error.is_some() {
            return Err(AttemptError::Retryable);
        }
        let usage = crate::history::Usage::from_usage(&completion.usage);
        Ok(json_response_with_usage(status, bytes, usage))
    }
}

/// The envelope fields a completion is checked for. Choices are skipped
/// unparsed; a null `error` is the same as none.
#[derive(serde::Deserialize)]
struct Completion {
    #[allow(dead_code)]
    choices: Vec<serde::de::IgnoredAny>,
    #[serde(default)]
    error: Option<serde::de::IgnoredAny>,
    #[serde(default)]
    usage: Value,
}

/// `POST /responses`, passed through like chat. Stateful fields such as
/// `previous_response_id` refer to the provider that stored the response, so
/// fallback to another target usually fails for them; that is upstream's answer.
pub async fn responses(
    client: &reqwest::Client,
    crypto: &Crypto,
    provider: &Provider,
    model: &str,
    body: &Value,
) -> Result<Response, AttemptError> {
    let streaming = body.get("stream").and_then(Value::as_bool).unwrap_or(false);
    let response = post(client, crypto, provider, "responses", model, body).await?;
    if streaming {
        check_sse(&response)?;
        return streaming_response(terminated(upstream_events(response), is_response_end)).await;
    }
    let status = response.status();
    let bytes = read_bounded(response).await?;
    if bytes.trim_ascii_start().first() != Some(&b'{') {
        return Err(AttemptError::Retryable);
    }
    // A queued background response has an empty output and no error yet; it
    // is an accepted job and must not be retried elsewhere.
    let envelope: ResponseObject =
        serde_json::from_slice(&bytes).map_err(|_| AttemptError::Retryable)?;
    if envelope.error.is_some() {
        return Err(AttemptError::Retryable);
    }
    let usage = crate::history::Usage::from_usage(&envelope.usage);
    Ok(json_response_with_usage(status, bytes, usage))
}

#[derive(serde::Deserialize)]
struct ResponseObject {
    #[allow(dead_code)]
    output: Vec<serde::de::IgnoredAny>,
    #[serde(default)]
    error: Option<serde::de::IgnoredAny>,
    #[serde(default)]
    usage: Value,
}

/// `POST /embeddings`; the vectors are forwarded unchanged.
pub async fn embeddings(
    client: &reqwest::Client,
    crypto: &Crypto,
    provider: &Provider,
    model: &str,
    body: &Value,
) -> Result<Response, AttemptError> {
    let response = post(client, crypto, provider, "embeddings", model, body).await?;
    let status = response.status();
    let bytes = read_bounded(response).await?;
    if bytes.trim_ascii_start().first() != Some(&b'{') {
        return Err(AttemptError::Retryable);
    }
    let envelope: Embeddings =
        serde_json::from_slice(&bytes).map_err(|_| AttemptError::Retryable)?;
    if envelope.error.is_some() {
        return Err(AttemptError::Retryable);
    }
    let usage = crate::history::Usage::from_usage(&envelope.usage);
    Ok(json_response_with_usage(status, bytes, usage))
}

#[derive(serde::Deserialize)]
struct Embeddings {
    #[allow(dead_code)]
    data: Vec<serde::de::IgnoredAny>,
    #[serde(default)]
    error: Option<serde::de::IgnoredAny>,
    #[serde(default)]
    usage: Value,
}

/// Chat Completions streams end with `data: [DONE]`.
fn is_done(event: &SseEvent) -> bool {
    event.data.trim() == "[DONE]"
}

/// Responses streams end with one of these events instead of `[DONE]`. An
/// `error` event is not terminal: upstream may still send `response.failed`.
const RESPONSE_END: [&str; 3] = [
    "response.completed",
    "response.incomplete",
    "response.failed",
];

/// The event name, or the data's `type` when a server omits `event:` lines.
fn is_response_end(event: &SseEvent) -> bool {
    if !event.name.is_empty() {
        return RESPONSE_END.contains(&event.name.as_str());
    }
    #[derive(serde::Deserialize)]
    struct Typed<'a> {
        #[serde(borrow)]
        r#type: std::borrow::Cow<'a, str>,
    }
    event.data.contains("\"response.")
        && serde_json::from_str::<Typed>(&event.data)
            .is_ok_and(|typed| RESPONSE_END.contains(&typed.r#type.as_ref()))
}

/// True for the last event of a Chat Completions or Responses stream.
pub(crate) fn is_terminal(event: &SseEvent) -> bool {
    is_done(event) || is_response_end(event)
}

/// Raw OpenAI SSE passthrough with terminal-event tracking.
///
/// Bytes are forwarded verbatim: framing, unknown fields, and payloads are
/// never parsed or re-encoded. The shared framing decoder only watches for
/// the terminal event (`data: [DONE]` for chat, `response.completed` and its
/// siblings for Responses); the chunk that completes it — including any
/// trailing bytes after it in that chunk — is forwarded unchanged, then the
/// upstream body is dropped so a finished generation does not wait for the
/// connection to close or time out. Framing obeys the shared 1 MiB per-event
/// bound.
///
/// A clean HTTP EOF before the terminal event is a truncated stream: while
/// still precommit (no bytes forwarded yet, e.g. an empty upstream body) it
/// stays a retryable attempt; after commitment `streaming_response` turns it
/// into a terminal body error with no fallback.
fn terminated<S>(source: S, is_end: fn(&SseEvent) -> bool) -> ResponseStream
where
    S: Stream<Item = Result<Bytes, AttemptError>> + Send + 'static,
{
    Box::pin(async_stream::try_stream! {
        futures_util::pin_mut!(source);
        let mut decoder = SseDecoder::default();
        let mut done = false;
        while !done {
            let bytes = match source.next().await { Some(result) => result?, None => break };
            decoder.push_chunk(&bytes, |event| {
                if !is_end(&event) {
                    return ControlFlow::Continue(());
                }
                done = true;
                ControlFlow::Break(())
            })?;
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
        let whole = terminated(stream::iter(vec![Ok(Bytes::from_static(WIRE))]), is_done);
        assert_eq!(collect(whole).await.unwrap(), WIRE);
        let fragmented = WIRE
            .iter()
            .map(|byte| Ok(Bytes::from(vec![*byte])))
            .collect::<Vec<_>>();
        let split = terminated(stream::iter(fragmented), is_done);
        assert_eq!(collect(split).await.unwrap(), WIRE);
    }

    #[tokio::test]
    async fn eof_without_terminal_event_is_truncation() {
        let mut stream = terminated(
            stream::iter(vec![Ok(Bytes::from_static(b"data: {\"a\":1}\n\n"))]),
            is_done,
        );
        let forwarded = stream.next().await.unwrap().unwrap();
        assert_eq!(forwarded.as_ref(), b"data: {\"a\":1}\n\n");
        assert!(matches!(
            stream.next().await,
            Some(Err(AttemptError::Retryable))
        ));
        // An empty upstream body stays a precommit retry: nothing forwarded.
        let mut empty = terminated(stream::empty(), is_done);
        assert!(matches!(
            empty.next().await,
            Some(Err(AttemptError::Retryable))
        ));
    }

    #[test]
    fn responses_end_on_named_or_typed_terminal_events_only() {
        let event = |name: &str, data: &str| SseEvent {
            name: name.into(),
            data: data.into(),
        };
        for name in RESPONSE_END {
            assert!(is_response_end(&event(name, "{}")));
            assert!(is_response_end(&event(
                "",
                &format!("{{\"type\":\"{name}\"}}")
            )));
        }
        for (name, data) in [
            ("error", r#"{"type":"error"}"#),
            (
                "response.output_text.delta",
                r#"{"type":"response.completed"}"#,
            ),
            (
                "",
                r#"{"type":"response.output_text.delta","delta":"response.completed"}"#,
            ),
            ("", "[DONE]"),
            ("", "not json \"response.completed\""),
        ] {
            assert!(!is_response_end(&event(name, data)), "{name} {data}");
        }
        assert!(is_terminal(&event("", " [DONE] ")));
        assert!(is_terminal(&event("response.failed", "{}")));
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
            terminated(source, is_done).collect::<Vec<_>>(),
        )
        .await
        .expect("terminal event must complete without waiting for upstream EOF");
        assert_eq!(received.len(), 1);
        assert!(received[0].is_ok());
    }
}
