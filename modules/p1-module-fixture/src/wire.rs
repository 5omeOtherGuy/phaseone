//! The JSON text the fixture reads and writes, by hand.
//!
//! The module workspace depends on wit-bindgen alone — no serde, no serde_json — so the wire
//! shapes of `docs/design/modules/protocol.md` are written as text here: a tool outcome, a
//! call description and a result description. The two values the fixture reads out of text it
//! receives are a wire tool call's `input.raw` (the tool input) and a `tool_result` item's
//! `content` (for `describe-result`).

use std::fmt::Write as _;

/// The string value of the first `key` in JSON text `json`, with its escapes decoded, or
/// `None` when the text has no such key or is not readable as JSON around it.
///
/// A JSON key is a string literal that a colon follows, so scanning string literals for that
/// colon finds a key at any depth — `raw` inside `input` — without parsing the rest of the
/// document. Only string values are read: the fixture's two keys are strings.
pub fn string_field(json: &str, key: &str) -> Option<String> {
    let bytes = json.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        // 0x22 starts a string literal in UTF-8 and never continues one.
        if bytes[i] != b'"' {
            i += 1;
            continue;
        }
        let literal = Literal::at(bytes, i)?;
        let after = literal.end + 1;
        let mut colon = after;
        while bytes.get(colon).is_some_and(u8::is_ascii_whitespace) {
            colon += 1;
        }
        if bytes.get(colon) != Some(&b':') {
            // A string in a value position; keep scanning after it, once it has proved
            // readable: an undecodable literal anywhere makes the text unreadable.
            literal.decode(json)?;
            i = after;
            continue;
        }
        if literal.decode(json)? == key {
            let mut start = colon + 1;
            while bytes.get(start).is_some_and(u8::is_ascii_whitespace) {
                start += 1;
            }
            if bytes.get(start) != Some(&b'"') {
                return None;
            }
            return Some(Literal::at(bytes, start)?.decode(json)?.into_owned());
        }
        i = colon + 1;
    }
    None
}

/// The extent of one JSON string literal, found by a byte scan before anything is decoded,
/// so that its text is copied once, into an allocation of the right size.
struct Literal {
    /// The byte just after the opening quote.
    start: usize,
    /// The closing quote.
    end: usize,
    /// Whether any escape occurs; a literal without one is its own text.
    escaped: bool,
}

impl Literal {
    /// The literal whose opening quote is at byte `open`, or `None` when it is unterminated
    /// or holds a raw control character, which JSON text never does.
    fn at(bytes: &[u8], open: usize) -> Option<Self> {
        let start = open + 1;
        let mut i = start;
        let mut escaped = false;
        loop {
            i = special(bytes, i)?;
            match bytes[i] {
                b'"' => {
                    return Some(Self {
                        start,
                        end: i,
                        escaped,
                    });
                }
                // The escaped byte is skipped whatever it is: `\"` does not end the literal.
                // Every escape is ASCII, so the skip never lands inside a character.
                b'\\' => {
                    escaped = true;
                    i += 2;
                }
                _ => return None,
            }
        }
    }

    /// The literal's text with its escapes decoded; `None` when an escape is not JSON.
    fn decode<'a>(&self, json: &'a str) -> Option<std::borrow::Cow<'a, str>> {
        let body = &json[self.start..self.end];
        if !self.escaped {
            return Some(std::borrow::Cow::Borrowed(body));
        }
        // Escapes only shrink the text, so the body's length is enough room.
        let mut text = String::with_capacity(body.len());
        let bytes = body.as_bytes();
        let mut run = 0;
        let mut i = 0;
        while i < bytes.len() {
            if bytes[i] != b'\\' {
                i += 1;
                continue;
            }
            // `run..i` holds no escape and ends before an ASCII byte: whole characters.
            text.push_str(&body[run..i]);
            i += 1;
            match *bytes.get(i)? {
                b'"' => text.push('"'),
                b'\\' => text.push('\\'),
                b'/' => text.push('/'),
                b'b' => text.push('\u{8}'),
                b'f' => text.push('\u{c}'),
                b'n' => text.push('\n'),
                b'r' => text.push('\r'),
                b't' => text.push('\t'),
                b'u' => {
                    let mut unit = hex4(bytes, i + 1)?;
                    i += 4;
                    if (0xd800..0xdc00).contains(&unit) {
                        // A character outside the BMP arrives as two escapes, and only a
                        // following low surrogate makes them one char.
                        let low = match bytes.get(i + 1..i + 3) {
                            Some(b"\\u") => hex4(bytes, i + 3),
                            _ => None,
                        };
                        match low {
                            Some(low) if (0xdc00..0xe000).contains(&low) => {
                                i += 6;
                                unit = 0x10000 + ((unit - 0xd800) << 10) + (low - 0xdc00);
                            }
                            _ => unit = 0xfffd,
                        }
                    }
                    text.push(char::from_u32(unit).unwrap_or('\u{fffd}'));
                }
                _ => return None,
            }
            i += 1;
            run = i;
        }
        text.push_str(&body[run..]);
        Some(std::borrow::Cow::Owned(text))
    }
}

/// Reads the four hex digits of a `\uXXXX` escape that start at byte `at`.
fn hex4(bytes: &[u8], at: usize) -> Option<u32> {
    let mut value = 0;
    for &byte in bytes.get(at..at + 4)? {
        value = value * 16 + char::from(byte).to_digit(16)?;
    }
    Some(value)
}

/// The index of the first byte at or after `from` that a JSON string literal cannot hold
/// as it is — a quote, a backslash or a control character — or `None` when there is none.
///
/// It tests eight bytes per step, because the guest runs metered: every wasm instruction
/// costs fuel and time, and a byte at a time is most of a large echo's cost.
fn special(bytes: &[u8], from: usize) -> Option<usize> {
    const ONES: u64 = 0x0101_0101_0101_0101;
    const HIGHS: u64 = 0x8080_8080_8080_8080;
    // A high bit where a byte of `word` is zero; exact for the first such byte, which is
    // the one the scan below stops at.
    let zero = |word: u64| word.wrapping_sub(ONES) & !word & HIGHS;
    let tail = bytes.get(from..)?;
    let mut chunks = tail.chunks_exact(8);
    let mut offset = from;
    for chunk in &mut chunks {
        let mut word = [0; 8];
        word.copy_from_slice(chunk);
        let word = u64::from_le_bytes(word);
        let hit = zero(word ^ (ONES * u64::from(b'"')))
            | zero(word ^ (ONES * u64::from(b'\\')))
            // A byte below 0x20 has its three top bits clear.
            | zero(word & (ONES * 0xe0));
        if hit != 0 {
            break;
        }
        offset += 8;
    }
    bytes[offset..]
        .iter()
        .position(|&byte| byte == b'"' || byte == b'\\' || byte < 0x20)
        .map(|found| offset + found)
}

/// The tool outcome of a call that ended successfully.
pub fn ok_outcome(content: &str) -> String {
    outcome("ok", content)
}

/// The tool outcome of a call the host cancelled (empty content, as the native tools do).
pub fn cancelled_outcome() -> String {
    outcome("cancelled", "")
}

/// The tool outcome of a call that failed; `content` is what the model is told.
pub fn error_outcome(content: &str) -> String {
    outcome("error", content)
}

/// The `tool_outcome` shape: `status` is the closed set of `ToolStatus` names. Written into
/// one allocation of its final size, because `content` may be a whole echoed history.
fn outcome(status: &str, content: &str) -> String {
    const OPEN: &str = "{\"status\":";
    const MIDDLE: &str = ",\"content\":";
    let size = OPEN.len() + json_len(status) + MIDDLE.len() + json_len(content) + 1;
    let mut out = String::with_capacity(size);
    out.push_str(OPEN);
    push_json_text(&mut out, status);
    out.push_str(MIDDLE);
    push_json_text(&mut out, content);
    out.push('}');
    out
}

/// The `call_description` shape: `destructive` is always stated, `target` only when known.
pub fn call_description(verb: &str, target: Option<&str>, destructive: bool) -> String {
    let mut text = format!("{{\"verb\":{}", json_text(verb));
    if let Some(target) = target {
        let _ = write!(text, ",\"target\":{}", json_text(target));
    }
    let _ = write!(text, ",\"destructive\":{destructive}}}");
    text
}

/// The `result_description` shape: a one-line summary and no structured detail.
pub fn result_description(summary: &str) -> String {
    format!("{{\"summary\":{}}}", json_text(summary))
}

/// JSON text for `text` as a string literal, escape by escape, so a module's output can never
/// be read as something other than the value it was given. Every output of this module goes
/// through it; the tests build their input with it too.
pub(crate) fn json_text(text: &str) -> String {
    let mut out = String::with_capacity(json_len(text));
    push_json_text(&mut out, text);
    out
}

/// The escape of one byte a literal cannot hold as it is ([`special`]): a short escape where
/// JSON has one, else `\u00XX`.
fn escape(byte: u8) -> Escape {
    match byte {
        b'"' => Escape::Short(b'"'),
        b'\\' => Escape::Short(b'\\'),
        b'\n' => Escape::Short(b'n'),
        b'\r' => Escape::Short(b'r'),
        b'\t' => Escape::Short(b't'),
        0x08 => Escape::Short(b'b'),
        0x0c => Escape::Short(b'f'),
        control => Escape::Unicode(control),
    }
}

/// How [`escape`] writes one byte: `\` and a letter, or `\u00XX` for the other controls.
enum Escape {
    Short(u8),
    Unicode(u8),
}

/// The length of [`json_text`]`(text)`, found by the same scan, so the output is allocated
/// once at its final size.
fn json_len(text: &str) -> usize {
    let bytes = text.as_bytes();
    let mut size = text.len() + 2;
    let mut i = 0;
    while let Some(found) = special(bytes, i) {
        size += match escape(bytes[found]) {
            Escape::Short(_) => 1,
            Escape::Unicode(_) => 5,
        };
        i = found + 1;
    }
    size
}

/// Appends `text` as a JSON string literal: runs without a special byte are copied whole,
/// and every special byte is ASCII, so each run is whole characters.
fn push_json_text(out: &mut String, text: &str) {
    let bytes = text.as_bytes();
    out.push('"');
    let mut run = 0;
    while let Some(found) = special(bytes, run) {
        out.push_str(&text[run..found]);
        match escape(bytes[found]) {
            Escape::Short(letter) => {
                out.push('\\');
                out.push(char::from(letter));
            }
            Escape::Unicode(control) => {
                let _ = write!(out, "\\u{control:04x}");
            }
        }
        run = found + 1;
    }
    out.push_str(&text[run..]);
    out.push('"');
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A wire tool call, spelled as the host serializes one.
    fn call(raw: &str) -> String {
        format!(
            "{{\"call_id\":\"c1\",\"name\":\"fixture\",\"input\":{{\"kind\":\"text\",\"raw\":{}}}}}",
            json_text(raw)
        )
    }

    #[test]
    fn reads_the_raw_field_of_a_wire_tool_call() {
        assert_eq!(
            string_field(&call("echo:hi"), "raw").as_deref(),
            Some("echo:hi")
        );
        // The key is found at depth, after other string values and keys.
        assert_eq!(
            string_field(
                "{\"call_id\":\"c1\",\"name\":\"fixture\",\"input\":{\"kind\":\"text\",\"raw\":\"stream:true\"}}",
                "raw"
            )
            .as_deref(),
            Some("stream:true")
        );
    }

    #[test]
    fn reads_the_content_field_of_a_tool_result_item() {
        let item = "{\"item\":\"tool_result\",\"call_id\":\"c1\",\"name\":\"fixture\",\"status\":\"ok\",\"content\":\"first line\\nsecond\"}";
        assert_eq!(
            string_field(item, "content").as_deref(),
            Some("first line\nsecond")
        );
        assert_eq!(string_field(item, "digest"), None);
    }

    #[test]
    fn decodes_escapes_including_a_surrogate_pair() {
        let json = "{\"raw\":\"a\\\"b\\\\c\\/d\\ne\\tf\\u0041\\u00e9\\ud83d\\ude00\"}";
        assert_eq!(
            string_field(json, "raw").as_deref(),
            Some("a\"b\\c/d\ne\tfA\u{e9}\u{1f600}")
        );
    }

    #[test]
    fn refuses_text_it_cannot_read() {
        assert_eq!(string_field("not json", "raw"), None);
        assert_eq!(string_field("{\"raw\":\"unterminated}", "raw"), None);
        // A key whose value is not a string is not the field to read.
        assert_eq!(string_field("{\"raw\":12}", "raw"), None);
        // A key that is not there, even when another one holds the text.
        assert_eq!(string_field("{\"kind\":\"text\"}", "raw"), None);
        // A raw control character is not valid JSON text.
        assert_eq!(string_field("{\"raw\":\"a\nb\"}", "raw"), None);
    }

    #[test]
    fn writes_outcomes_for_every_status_the_fixture_uses() {
        assert_eq!(
            ok_outcome("hi"),
            "{\"status\":\"ok\",\"content\":\"hi\"}".to_owned()
        );
        assert_eq!(
            error_outcome("bad input"),
            "{\"status\":\"error\",\"content\":\"bad input\"}".to_owned()
        );
        assert_eq!(
            cancelled_outcome(),
            "{\"status\":\"cancelled\",\"content\":\"\"}".to_owned()
        );
    }

    #[test]
    fn escapes_text_that_would_otherwise_end_the_literal() {
        assert_eq!(
            ok_outcome("a\"b\\c\nd\u{1}\u{7f}"),
            "{\"status\":\"ok\",\"content\":\"a\\\"b\\\\c\\nd\\u0001\u{7f}\"}".to_owned()
        );
    }

    #[test]
    fn writes_descriptions() {
        assert_eq!(
            call_description("run", Some("echo hi"), true),
            "{\"verb\":\"run\",\"target\":\"echo hi\",\"destructive\":true}".to_owned()
        );
        assert_eq!(
            call_description("call", None, false),
            "{\"verb\":\"call\",\"destructive\":false}".to_owned()
        );
        assert_eq!(
            result_description("first line"),
            "{\"summary\":\"first line\"}".to_owned()
        );
    }

    /// The fixture's own escaping must be readable by its own reader, round trip included.
    #[test]
    fn what_it_writes_it_reads_back() {
        let text = "quote\" backslash\\ newline\n tab\t bell\u{7} non-bmp \u{1f600}";
        let json = format!("{{\"content\":{}}}", json_text(text));
        assert_eq!(string_field(&json, "content").as_deref(), Some(text));
    }

    /// Every ASCII byte and a few multi-byte characters, each at every offset of an eight-byte
    /// step, so the word-at-a-time scan meets each special byte in every lane and at the tail.
    fn every_byte_in_every_lane() -> Vec<String> {
        let mut alphabet: Vec<char> = (0u8..0x80).map(char::from).collect();
        alphabet.extend(['\u{e9}', '\u{2028}', '\u{1f600}']);
        let mut texts = Vec::new();
        for pad in 0..9 {
            for &ch in &alphabet {
                texts.push(format!("{}{ch}{}", "x".repeat(pad), "y".repeat(8 - pad)));
            }
        }
        texts.push(alphabet.iter().cycle().take(1000).collect());
        texts
    }

    /// The escaping of the character-by-character writer this module had before its
    /// byte scans, kept as the reference its output must equal.
    fn reference_json_text(text: &str) -> String {
        let mut out = String::from("\"");
        for ch in text.chars() {
            match ch {
                '"' => out.push_str("\\\""),
                '\\' => out.push_str("\\\\"),
                '\n' => out.push_str("\\n"),
                '\r' => out.push_str("\\r"),
                '\t' => out.push_str("\\t"),
                '\u{8}' => out.push_str("\\b"),
                '\u{c}' => out.push_str("\\f"),
                ch if (ch as u32) < 0x20 => {
                    let _ = write!(out, "\\u{:04x}", ch as u32);
                }
                ch => out.push(ch),
            }
        }
        out.push('"');
        out
    }

    #[test]
    fn the_byte_scans_write_and_read_what_the_character_loop_did() {
        for text in every_byte_in_every_lane() {
            let written = json_text(&text);
            assert_eq!(written, reference_json_text(&text), "{text:?}");
            assert_eq!(json_len(&text), written.len(), "{text:?}");
            assert_eq!(
                ok_outcome(&text),
                format!("{{\"status\":\"ok\",\"content\":{written}}}")
            );
            let json = format!("{{\"raw\":{written}}}");
            assert_eq!(string_field(&json, "raw").as_deref(), Some(text.as_str()));
        }
    }

    #[test]
    fn an_escape_is_read_in_every_lane_and_a_lone_surrogate_is_replaced() {
        for pad in 0..9 {
            let x = "x".repeat(pad);
            let json = format!("{{\"raw\":\"{x}\\u0041\\/\\ud83d{x}\\ud83d\\ude00\"}}");
            assert_eq!(
                string_field(&json, "raw"),
                Some(format!("{x}A/\u{fffd}{x}\u{1f600}"))
            );
            // An escape the literal ends inside, or one JSON does not have, is unreadable.
            for bad in ["\\u12\"", "\\x", "\\ud83d\\u12"] {
                let json = format!("{{\"raw\":\"{x}{bad}\"}}");
                assert_eq!(string_field(&json, "raw"), None, "{json}");
            }
        }
        // An unreadable literal in a value position makes the whole text unreadable, as
        // before the byte scans.
        assert_eq!(string_field("{\"a\":\"\\q\",\"raw\":\"x\"}", "raw"), None);
    }
}
