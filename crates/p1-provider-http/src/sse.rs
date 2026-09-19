//! SSE decoding: bytes in, whole events out.
//!
//! The decoder is transport-agnostic and synchronous. It obeys the SSE rules in
//! `docs/design/providers.md`: events are separated by a blank line; `\n`, `\r\n`
//! and `\r` all end a line; several `data:` lines join with `\n`; one optional
//! space after the colon is stripped; `:` comment lines are ignored; and a chunk
//! may end anywhere — mid-line, mid-UTF-8-character, or between `\r` and `\n`.
//! `finish` flushes a final event that lacks the closing blank line.

/// One decoded SSE event. `event` is the `event:` field when present.
///
/// `Debug` prints the data length, never the data: an SSE payload is response
/// body text and must not reach a log.
#[derive(Clone, PartialEq, Eq)]
pub struct SseEvent {
    pub event: Option<String>,
    pub data: String,
}

impl std::fmt::Debug for SseEvent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SseEvent")
            .field("event", &self.event)
            .field("data_len", &self.data.len())
            .finish()
    }
}

/// Incremental SSE decoder. Feed complete or partial chunks to [`push`]; the
/// events it returns are whole and ordered.
///
/// [`push`]: SseDecoder::push
#[derive(Default)]
pub struct SseDecoder {
    /// Bytes not yet known to end at a line terminator. May hold a partial line,
    /// a partial UTF-8 character, or a lone trailing `\r`.
    pending: Vec<u8>,
    /// The `event:` field of the event under construction.
    event: Option<String>,
    /// The `data:` field values of the event under construction, in order.
    data: Vec<String>,
}

impl SseDecoder {
    pub fn new() -> Self {
        Self::default()
    }

    /// Decode everything `bytes` completes. A returned event is complete; an
    /// event still missing its blank line stays buffered until the next `push`
    /// or [`finish`](SseDecoder::finish).
    pub fn push(&mut self, bytes: &[u8]) -> Vec<SseEvent> {
        self.pending.extend_from_slice(bytes);
        let mut events = Vec::new();
        while let Some(index) = self
            .pending
            .iter()
            .position(|byte| matches!(byte, b'\n' | b'\r'))
        {
            let skip = if self.pending[index] == b'\r' {
                if index + 1 == self.pending.len() {
                    // A trailing `\r` may be the first half of a `\r\n`; wait for
                    // the next chunk rather than emit a spurious blank line.
                    break;
                }
                if self.pending[index + 1] == b'\n' {
                    2
                } else {
                    1
                }
            } else {
                1
            };
            let line: Vec<u8> = self.pending[..index].to_vec();
            self.pending.drain(..index + skip);
            if let Some(event) = self.push_line(&String::from_utf8_lossy(&line)) {
                events.push(event);
            }
        }
        events
    }

    /// Flush the final event, which may lack a trailing line terminator or blank
    /// line. Returns it only if it carries data.
    pub fn finish(mut self) -> Option<SseEvent> {
        if !self.pending.is_empty() {
            let bytes = std::mem::take(&mut self.pending);
            let line = String::from_utf8_lossy(&bytes).into_owned();
            // A lone trailing `\r` is a line terminator, not part of the field
            // value, so trim it exactly as `push` would have.
            if let Some(event) = self.push_line(line.trim_end_matches('\r')) {
                return Some(event);
            }
        }
        self.take_event()
    }

    /// Feed one complete line; returns an event when the line is blank.
    fn push_line(&mut self, line: &str) -> Option<SseEvent> {
        if line.is_empty() {
            return self.take_event();
        }
        if line.starts_with(':') {
            return None;
        }
        let (field, value) = match line.split_once(':') {
            Some((field, value)) => (field, value.strip_prefix(' ').unwrap_or(value)),
            None => (line, ""),
        };
        match field {
            "data" => self.data.push(value.to_string()),
            "event" => self.event = Some(value.to_string()),
            _ => {}
        }
        None
    }

    /// Close the event under construction. An event with no data (only comments,
    /// or only an `event:` field) is not dispatched.
    fn take_event(&mut self) -> Option<SseEvent> {
        let data = std::mem::take(&mut self.data).join("\n");
        let event = self.event.take();
        if data.is_empty() {
            return None;
        }
        Some(SseEvent { event, data })
    }
}

impl std::fmt::Debug for SseDecoder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SseDecoder")
            .field("pending_len", &self.pending.len())
            .field("event", &self.event)
            .field("data_lines", &self.data.len())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_events_on_a_blank_line_and_joins_data_lines() {
        let mut decoder = SseDecoder::new();
        let events = decoder.push(b"data: {\"a\":1}\n\ndata: line1\ndata: line2\n\n");
        assert_eq!(
            events,
            vec![
                SseEvent {
                    event: None,
                    data: "{\"a\":1}".to_string()
                },
                SseEvent {
                    event: None,
                    data: "line1\nline2".to_string()
                },
            ]
        );
    }

    #[test]
    fn accepts_crlf_and_cr_line_endings() {
        let mut decoder = SseDecoder::new();
        let mut events = decoder.push(b"data: a\r\n\r\ndata: b\r\r");
        // The final `\r` may still be half of a `\r\n`, so the second event is
        // flushed by `finish`.
        if let Some(event) = decoder.finish() {
            events.push(event);
        }
        assert_eq!(
            events,
            vec![
                SseEvent {
                    event: None,
                    data: "a".to_string()
                },
                SseEvent {
                    event: None,
                    data: "b".to_string()
                },
            ]
        );
    }

    #[test]
    fn strips_one_optional_space_and_ignores_comments() {
        let mut decoder = SseDecoder::new();
        assert_eq!(
            decoder.push(b": a comment\ndata:  two spaces\nevent: ping\ndata:x\n\n"),
            vec![SseEvent {
                event: Some("ping".to_string()),
                data: " two spaces\nx".to_string(),
            }]
        );
    }

    #[test]
    fn finish_flushes_an_event_without_a_trailing_blank_line() {
        let mut decoder = SseDecoder::new();
        assert!(decoder.push(b"data: tail").is_empty());
        assert_eq!(
            decoder.finish(),
            Some(SseEvent {
                event: None,
                data: "tail".to_string()
            })
        );
    }

    #[test]
    fn finish_returns_none_for_a_data_free_event() {
        let mut decoder = SseDecoder::new();
        assert!(decoder.push(b": comment only").is_empty());
        assert_eq!(decoder.finish(), None);
    }

    /// Ported from the donor's
    /// `async_sse_decoder_handles_split_chunks_and_multiline_events`: a chunk
    /// boundary in the middle of a line and of a multi-line `data:` payload must
    /// still produce one event with the data lines joined by `\n`.
    #[test]
    fn async_sse_decoder_handles_split_chunks_and_multiline_events() {
        let mut decoder = SseDecoder::new();
        let mut events = Vec::new();
        events.extend(decoder.push(b"event: response.output_text.delta\ndata: {\"type\":"));
        events
            .extend(decoder.push(b"\"response.output_text.delta\",\ndata: \"delta\":\"hi\"}\n\n"));
        if let Some(event) = decoder.finish() {
            events.push(event);
        }
        assert_eq!(
            events,
            vec![SseEvent {
                event: Some("response.output_text.delta".to_string()),
                data: "{\"type\":\"response.output_text.delta\",\n\"delta\":\"hi\"}".to_string(),
            }]
        );
    }

    /// Splitting the input at every byte offset must produce the same events as a
    /// single push. The fixture exercises CRLF, a comment line, multi-line data
    /// and a 4-byte UTF-8 character (U+1F600), so a boundary may fall inside the
    /// character's encoding.
    #[test]
    fn splitting_at_every_byte_offset_is_equivalent_to_one_push() {
        let fixture = ": comment\r\nevent: greet\r\ndata: line one\r\ndata: li\u{1F600}ne two\r\n\r\ndata: tail\n\n";
        let expected = {
            let mut decoder = SseDecoder::new();
            let mut events = decoder.push(fixture.as_bytes());
            if let Some(event) = decoder.finish() {
                events.push(event);
            }
            events
        };
        assert_eq!(
            expected,
            vec![
                SseEvent {
                    event: Some("greet".to_string()),
                    data: "line one\nli\u{1F600}ne two".to_string(),
                },
                SseEvent {
                    event: None,
                    data: "tail".to_string()
                },
            ]
        );

        let bytes = fixture.as_bytes();
        for split in 0..=bytes.len() {
            let mut decoder = SseDecoder::new();
            let mut events = decoder.push(&bytes[..split]);
            events.extend(decoder.push(&bytes[split..]));
            if let Some(event) = decoder.finish() {
                events.push(event);
            }
            assert_eq!(events, expected, "split at byte offset {split}");
        }
    }

    #[test]
    fn debug_output_does_not_print_event_data() {
        let event = SseEvent {
            event: Some("message".to_string()),
            data: "SENTINEL-BODY-456".to_string(),
        };
        assert!(!format!("{event:?}").contains("SENTINEL-BODY-456"));
    }

    #[test]
    fn data_free_events_are_skipped() {
        let mut decoder = SseDecoder::new();
        assert!(
            decoder
                .push(b"event: ping\n\ndata:\n\ndata: real\n\n")
                .into_iter()
                .map(|event| event.data)
                .collect::<Vec<_>>()
                == vec!["real".to_string()]
        );
    }
}
