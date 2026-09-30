//! Incremental `text/event-stream` framing shared by the provider adapters.
//!
//! Private to the `providers` module: the OpenAI raw passthrough only watches
//! for the terminal `data: [DONE]` event, while the Anthropic adapter fully
//! decodes events. Both must frame upstream SSE without assuming HTTP chunk
//! boundaries align with events.

use super::AttemptError;

/// Upper bound for a single framed SSE event, including field lines,
/// comments, and unrecognized fields. The same bound applies to the OpenAI
/// raw passthrough, whose terminal-event tracker otherwise forwards bytes
/// verbatim and unbounded in total.
pub(super) const MAX_EVENT_BYTES: usize = 1024 * 1024;

/// A fully framed SSE event: `name` is the `event:` field (empty when the
/// event had none) and `data` is the `data:` field lines joined with `\n`.
#[derive(Debug)]
pub(super) struct SseEvent {
    pub(super) name: String,
    pub(super) data: String,
}

/// Byte-at-a-time SSE framing decoder.
///
/// Lines are decoded only after their terminator, so neither UTF-8 nor CRLF
/// boundaries need to coincide with HTTP chunks. The size bound covers the
/// entire event (including comments and unrecognized fields), not just the
/// data lines.
#[derive(Default)]
pub(super) struct SseDecoder {
    line: Vec<u8>,
    name: String,
    data: String,
    size: usize,
    has_data: bool,
}

impl SseDecoder {
    pub(super) fn push(&mut self, byte: u8) -> Result<Option<SseEvent>, AttemptError> {
        self.size += 1;
        if self.size > MAX_EVENT_BYTES {
            return Err(AttemptError::Retryable);
        }
        if byte != b'\n' {
            self.line.push(byte);
            return Ok(None);
        }
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
