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
            .field("event_len", &self.event.as_ref().map(String::len))
            .field("data_len", &self.data.len())
            .finish()
    }
}

/// Incremental SSE decoder. Feed complete or partial chunks to [`push`]; the
/// events it returns are whole and ordered.
///
/// [`push`]: SseDecoder::push
/// Maximum retained size of one wire line and one unfinished event.
pub const SSE_LINE_LIMIT: usize = 256 * 1024;
pub const SSE_EVENT_LIMIT: usize = 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SseLimitExceeded;

impl std::fmt::Display for SseLimitExceeded {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SSE payload exceeds decoder limit")
    }
}

impl std::error::Error for SseLimitExceeded {}

#[derive(Default)]
pub struct SseDecoder {
    /// Bytes not yet known to end at a line terminator. May hold a partial line,
    /// a partial UTF-8 character, or a lone trailing `\r`.
    pending: Vec<u8>,
    /// How many leading bytes of `pending` have already been searched for a line
    /// terminator. The scan resumes here, so a chunk with no terminator never
    /// rescans the bytes before it (a one-byte-at-a-time line is linear, not
    /// quadratic).
    scanned: usize,
    /// The `event:` field of the event under construction.
    event: Option<String>,
    /// The `data:` field values of the event under construction, in order.
    data: Vec<String>,
    event_bytes: usize,
    /// Test-only: bytes examined by the line scan, to prove the scan stays linear.
    #[cfg(test)]
    scanned_bytes: usize,
}

impl SseDecoder {
    pub fn new() -> Self {
        Self::default()
    }

    /// Decode everything `bytes` completes. A returned event is complete; an
    /// event still missing its blank line stays buffered until the next `push`
    /// or [`finish`](SseDecoder::finish).
    pub fn push(&mut self, bytes: &[u8]) -> Vec<SseEvent> {
        self.try_push(bytes)
            .expect("SSE input exceeds decoder limit")
    }

    /// Decode with bounded retained memory, including a single oversized chunk.
    /// On overflow the caller must terminate this stream, not retry its payload.
    pub fn try_push(&mut self, bytes: &[u8]) -> Result<Vec<SseEvent>, SseLimitExceeded> {
        if bytes.len() > SSE_EVENT_LIMIT {
            return Err(SseLimitExceeded);
        }
        let mut events = Vec::new();
        for piece in bytes.split_inclusive(|byte| *byte == b'\n' || *byte == b'\r') {
            if self.pending.len().saturating_add(piece.len()) > SSE_LINE_LIMIT + 1 {
                return Err(SseLimitExceeded);
            }
            self.pending.extend_from_slice(piece);
            self.consume_lines(&mut events)?;
        }
        Ok(events)
    }

    fn consume_lines(&mut self, events: &mut Vec<SseEvent>) -> Result<(), SseLimitExceeded> {
        loop {
            let start = self.scanned;
            let found = self.pending[start..]
                .iter()
                .position(|byte| matches!(byte, b'\n' | b'\r'));
            // The scan examines only `start..`: up to and including the terminator,
            // or the whole remaining buffer when there is none.
            #[cfg(test)]
            {
                self.scanned_bytes += found.map_or(self.pending.len() - start, |offset| offset + 1);
            }
            let Some(offset) = found else {
                self.scanned = self.pending.len();
                return Ok(());
            };
            let index = start + offset;
            let skip = if self.pending[index] == b'\r' {
                if index + 1 == self.pending.len() {
                    // A trailing `\r` may be the first half of a `\r\n`; wait for
                    // the next chunk rather than emit a spurious blank line, and
                    // resume the scan at that byte rather than rescanning it.
                    self.scanned = index;
                    return Ok(());
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
            // The drained bytes were the only ones searched; the tail after the
            // terminator was never examined, so the scan restarts at its front.
            self.scanned = 0;
            if self.event_bytes.saturating_add(line.len()) > SSE_EVENT_LIMIT {
                return Err(SseLimitExceeded);
            }
            self.event_bytes += line.len();
            if let Some(event) = self.push_line(&String::from_utf8_lossy(&line)) {
                events.push(event);
            }
        }
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
        self.event_bytes = 0;
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
            .field("event_len", &self.event.as_ref().map(String::len))
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
    fn oversized_lines_and_unfinished_events_fail_without_retaining_a_chunk() {
        let mut decoder = SseDecoder::new();
        assert_eq!(
            decoder.try_push(&vec![b'x'; SSE_LINE_LIMIT + 2]),
            Err(SseLimitExceeded)
        );
        let mut decoder = SseDecoder::new();
        for _ in 0..8 {
            assert!(
                decoder
                    .try_push(&vec![b'x'; SSE_LINE_LIMIT / 2 - 1])
                    .is_ok()
            );
            assert!(decoder.try_push(b"\n").is_ok());
        }
        assert_eq!(decoder.try_push(b"data: more\n"), Err(SseLimitExceeded));
    }

    #[test]
    fn event_field_is_redacted_in_debug() {
        let mut decoder = SseDecoder::new();
        decoder.push(b"event: SECRET-SENTINEL\ndata: x\n");
        assert!(!format!("{decoder:?}").contains("SECRET-SENTINEL"));
        let event = decoder.finish().unwrap();
        assert!(!format!("{event:?}").contains("SECRET-SENTINEL"));
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

    /// The scan resumes where it stopped, so a peer that sends an unterminated line
    /// one byte at a time costs one examined byte per byte instead of rescanning the
    /// whole buffer each time. The exact linear property is the assertion: N
    /// one-byte pushes examine each byte once. The old full-buffer scan would
    /// examine N(N+1)/2.
    #[test]
    fn a_fragmented_unterminated_line_is_scanned_once_per_byte() {
        const N: usize = 4096;
        let mut decoder = SseDecoder::new();
        for _ in 0..N {
            assert!(decoder.try_push(b"x").unwrap().is_empty());
        }
        assert_eq!(decoder.scanned_bytes, N);
    }
}
