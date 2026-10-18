//! Incremental `text/event-stream` framing shared by the provider adapters.
//!
//! Internal to the crate: the OpenAI raw passthrough only watches
//! for the terminal `data: [DONE]` event, while the Anthropic adapter fully
//! decodes events. Both must frame upstream SSE without assuming HTTP chunk
//! boundaries align with events. The history observer also uses this bounded
//! decoder without changing forwarded bytes.

use std::ops::ControlFlow;

use super::AttemptError;

/// Upper bound for a single framed SSE event, including field lines,
/// comments, and unrecognized fields. The same bound applies to the OpenAI
/// raw passthrough, whose terminal-event tracker otherwise forwards bytes
/// verbatim and unbounded in total.
pub(crate) const MAX_EVENT_BYTES: usize = 1024 * 1024;

/// A fully framed SSE event: `name` is the `event:` field (empty when the
/// event had none) and `data` is the `data:` field lines joined with `\n`.
#[derive(Debug)]
pub(crate) struct SseEvent {
    pub(crate) name: String,
    pub(crate) data: String,
}

/// Byte-at-a-time SSE framing decoder.
///
/// Lines are decoded only after their terminator, so neither UTF-8 nor CRLF
/// boundaries need to coincide with HTTP chunks. The size bound covers the
/// entire event (including comments and unrecognized fields), not just the
/// data lines.
#[derive(Default)]
pub(crate) struct SseDecoder {
    line: Vec<u8>,
    name: String,
    data: String,
    size: usize,
    has_data: bool,
}

impl SseDecoder {
    pub(crate) fn push(&mut self, byte: u8) -> Result<Option<SseEvent>, AttemptError> {
        self.size += 1;
        if self.size > MAX_EVENT_BYTES {
            return Err(AttemptError::Retryable);
        }
        if byte != b'\n' {
            self.line.push(byte);
            return Ok(None);
        }
        self.end_line()
    }

    /// Decode a whole chunk exactly like pushing its bytes one at a time,
    /// copying each line at once. `on_event` returning `Break` skips the rest
    /// of the chunk, as breaking out of a byte loop would.
    pub(crate) fn push_chunk(
        &mut self,
        mut bytes: &[u8],
        mut on_event: impl FnMut(SseEvent) -> ControlFlow<()>,
    ) -> Result<(), AttemptError> {
        while !bytes.is_empty() {
            let newline = bytes.iter().position(|&byte| byte == b'\n');
            let consumed = newline.map_or(bytes.len(), |index| index + 1);
            // The bound is checked per line rather than per byte; a line ends
            // the event only at its newline, so the outcome is the same.
            self.size += consumed;
            if self.size > MAX_EVENT_BYTES {
                return Err(AttemptError::Retryable);
            }
            let (segment, rest) = bytes.split_at(consumed);
            bytes = rest;
            let Some(index) = newline else {
                self.line.extend_from_slice(segment);
                break;
            };
            self.line.extend_from_slice(&segment[..index]);
            if let Some(event) = self.end_line()?
                && on_event(event).is_break()
            {
                break;
            }
        }
        Ok(())
    }

    fn end_line(&mut self) -> Result<Option<SseEvent>, AttemptError> {
        if self.line.last() == Some(&b'\r') {
            self.line.pop();
        }
        if self.line.is_empty() {
            self.size = 0;
            let name = std::mem::take(&mut self.name);
            let mut data = std::mem::take(&mut self.data);
            if !std::mem::take(&mut self.has_data) {
                return Ok(None);
            }
            data.pop(); // Last newline added by a data field.
            return Ok(Some(SseEvent { name, data }));
        }
        let line = std::str::from_utf8(&self.line).map_err(|_| AttemptError::Retryable)?;
        let (field, value) = line.split_once(':').unwrap_or((line, ""));
        let value = value.strip_prefix(' ').unwrap_or(value);
        match field {
            "event" => self.name = value.to_owned(),
            "data" => {
                self.has_data = true;
                self.data.push_str(value);
                self.data.push('\n');
            }
            _ => {}
        }
        self.line.clear();
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    type Decoded = (Vec<(String, String)>, bool);

    fn bytewise(chunks: &[&[u8]], stop: &str) -> Decoded {
        let mut decoder = SseDecoder::default();
        let mut events = Vec::new();
        for chunk in chunks {
            for byte in *chunk {
                match decoder.push(*byte) {
                    Ok(Some(event)) => {
                        let done = event.data == stop;
                        events.push((event.name, event.data));
                        if done {
                            break;
                        }
                    }
                    Ok(None) => {}
                    Err(_) => return (events, true),
                }
            }
        }
        (events, false)
    }

    fn chunked(chunks: &[&[u8]], stop: &str) -> Decoded {
        let mut decoder = SseDecoder::default();
        let mut events = Vec::new();
        for chunk in chunks {
            let result = decoder.push_chunk(chunk, |event| {
                let done = event.data == stop;
                events.push((event.name, event.data));
                if done {
                    ControlFlow::Break(())
                } else {
                    ControlFlow::Continue(())
                }
            });
            if result.is_err() {
                return (events, true);
            }
        }
        (events, false)
    }

    #[test]
    fn chunks_decode_exactly_like_single_bytes_at_every_split() {
        let oversized = format!("data: {}\n\n", "x".repeat(MAX_EVENT_BYTES));
        let inputs: [&[u8]; 6] = [
            b"event: a\ndata: one\ndata: two\n\n: comment\nid: 7\ndata: three\r\n\r\n",
            b"data: first\n\ndata: stop\n\ndata: after stop\n\ndata: tail\n\n",
            b"data:no-space\n\nretry: 5\n\n\n\ndata\n\ndata: \xc3\xa9\n\n",
            b"data: ok\n\ndata: \xff\n\ndata: never\n\n",
            b"data: unterminated",
            oversized.as_bytes(),
        ];
        for input in inputs {
            let expected = bytewise(&[input], "stop");
            assert_eq!(chunked(&[input], "stop"), expected);
            for split in 0..=input.len().min(80) {
                let (head, tail) = input.split_at(split);
                assert_eq!(
                    chunked(&[head, tail], "stop"),
                    bytewise(&[head, tail], "stop")
                );
            }
            let singles: Vec<&[u8]> = input.chunks(1).collect();
            assert_eq!(chunked(&singles, "stop"), bytewise(&singles, "stop"));
        }
    }

    #[test]
    fn the_event_bound_includes_the_newline_that_ends_it() {
        let fits = format!("data: {}\n\n", "x".repeat(MAX_EVENT_BYTES - 8));
        assert_eq!(fits.len(), MAX_EVENT_BYTES);
        assert!(!chunked(&[fits.as_bytes()], "").1);
        let over = format!("data: {}\n\n", "x".repeat(MAX_EVENT_BYTES - 7));
        assert!(chunked(&[over.as_bytes()], "").1);
        assert!(bytewise(&[over.as_bytes()], "").1);
    }
}
