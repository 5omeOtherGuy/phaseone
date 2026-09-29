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
    frame_bytes: usize,
}

impl SseDecoder {
    pub fn new() -> Self {
        Self::default()
    }

    /// Reject an oversized unfinished frame before allocating it, and a batch
    /// that passes the adapters' 16 MiB response bound before retaining it —
    /// the parsers can only consult that bound once they receive the events.
    /// Use at transport boundaries; `push` remains available for pure decoder
    /// fixtures.
    pub fn try_push(&mut self, bytes: &[u8]) -> Result<Vec<SseEvent>, &'static str> {
        let mut events = Vec::new();
        let mut batch_bytes = 0;
        let mut cursor = 0;
        while cursor < bytes.len() {
            if self.pending.last() == Some(&b'\r') {
                // A pending bare `\r` may already be the blank line that
                // dispatches the event: resolve it against the byte that
                // follows before the next frame is charged, so a frame's byte
                // limit does not move with the transport's chunk boundaries.
                self.push_bounded(&bytes[cursor..=cursor], &mut events, &mut batch_bytes)?;
                cursor += 1;
                continue;
            }
            let Some(index) = bytes[cursor..]
                .iter()
                .position(|byte| matches!(*byte, b'\n' | b'\r'))
                .map(|offset| cursor + offset)
            else {
                break;
            };
            self.push_bounded(&bytes[cursor..=index], &mut events, &mut batch_bytes)?;
            cursor = index + 1;
        }
        self.push_bounded(&bytes[cursor..], &mut events, &mut batch_bytes)?;
        Ok(events)
    }

    /// Charge one feed to the unfinished frame's byte limit, then retain what
    /// it decoded while the batch still fits the adapters' 16 MiB response
    /// bound (`16_777_216`, the parsers' own cumulative limit): a batch past
    /// that bound fails the stream anyway, and refusing it here stops one
    /// transport chunk from being copied into strings before the bound is
    /// consulted.
    fn push_bounded(
        &mut self,
        bytes: &[u8],
        events: &mut Vec<SseEvent>,
        batch_bytes: &mut usize,
    ) -> Result<(), &'static str> {
        const MAX_EVENT: usize = 1_048_576;
        const MAX_BATCH: usize = 16_777_216;
        if self.frame_bytes.saturating_add(bytes.len()) > MAX_EVENT {
            return Err("provider SSE event exceeds byte limit");
        }
        self.frame_bytes += bytes.len();
        let produced = self.push(bytes);
        *batch_bytes += produced.iter().map(|event| event.data.len()).sum::<usize>();
        if *batch_bytes > MAX_BATCH {
            return Err("provider SSE batch exceeds byte limit");
        }
        events.extend(produced);
        Ok(())
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
            "data" => {
                self.data.push(value.to_string());
            }
            "event" => self.event = Some(value.to_string()),
            _ => {}
        }
        None
    }

    /// Close the event under construction. An event with no data (only comments,
    /// or only an `event:` field) is not dispatched.
    fn take_event(&mut self) -> Option<SseEvent> {
        self.frame_bytes = 0;
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
    fn keepalive_chunks_cannot_grow_an_unterminated_line() {
        let mut decoder = SseDecoder::new();
        let chunk = vec![b'x'; 128 * 1024];
        for _ in 0..8 {
            assert!(decoder.try_push(&chunk).unwrap().is_empty());
        }
        assert!(decoder.try_push(b"x").is_err());
    }
    #[test]
    fn batched_small_frames_do_not_hit_the_frame_limit() {
        let mut decoder = SseDecoder::new();
        let frame = format!("data: {}\n\n", "a".repeat(600_000));
        let events = decoder
            .try_push(format!("{frame}{frame}").as_bytes())
            .unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].data.len(), 600_000);
        let mut decoder = SseDecoder::new();
        assert!(
            decoder
                .try_push(format!("data: {}\n", "b".repeat(600_000)).as_bytes())
                .unwrap()
                .is_empty()
        );
        assert!(
            decoder
                .try_push(format!("data: {}\n\n", "c".repeat(600_000)).as_bytes())
                .is_err()
        );
    }

    /// Codex: one transport chunk carrying many individually valid frames must
    /// not be copied into strings past the adapters' 16 MiB response bound —
    /// the parser only consults that bound event by event, after retention.
    #[test]
    fn a_batch_past_the_response_bound_is_refused_before_retention() {
        let frame = format!("data: {}\n\n", "a".repeat(1_000_000));
        // 16 × 1,000,000 = 16,000,000 bytes of event data fits the bound.
        let mut decoder = SseDecoder::new();
        let under = frame.repeat(16);
        assert_eq!(decoder.try_push(under.as_bytes()).unwrap().len(), 16);
        // 17 × 1,000,000 = 17,000,000 > 16,777,216: the batch is refused
        // before all of it is retained, instead of reaching the parser whole.
        let mut decoder = SseDecoder::new();
        let over = frame.repeat(17);
        assert!(decoder.try_push(over.as_bytes()).is_err());
    }

    /// Codex: with bare `\r` line endings the separator's final `\r` stays
    /// pending, so the next frame's bytes must not be charged against the
    /// finished frame's byte limit — the chunk must not decide the verdict.
    #[test]
    fn bare_cr_frames_are_not_charged_across_a_pending_separator() {
        let chunk = format!(
            "data: {}\r\rdata: {}\r\r",
            "a".repeat(600_000),
            "b".repeat(600_000)
        );
        let mut decoder = SseDecoder::new();
        let mut events = decoder.try_push(chunk.as_bytes()).unwrap();
        if let Some(event) = decoder.finish() {
            events.push(event);
        }
        assert_eq!(
            events
                .iter()
                .map(|event| event.data.len())
                .collect::<Vec<_>>(),
            vec![600_000usize, 600_000]
        );
        assert!(events[0].data.bytes().all(|byte| byte == b'a'));
        assert!(events[1].data.bytes().all(|byte| byte == b'b'));

        // The same bytes split between the separator's two bare `\r` decode
        // to the same events: the verdict no longer depends on the chunk.
        let at = "data: ".len() + 600_000 + 1;
        let mut split = SseDecoder::new();
        let mut split_events = split.try_push(&chunk[..at]).unwrap();
        split_events.extend(split.try_push(&chunk[at..]).unwrap());
        if let Some(event) = split.finish() {
            split_events.push(event);
        }
        assert_eq!(split_events, events);
    }

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
