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
        let (text, after) = string_literal(json, i)?;
        let mut colon = after;
        while bytes.get(colon).is_some_and(u8::is_ascii_whitespace) {
            colon += 1;
        }
        if bytes.get(colon) != Some(&b':') {
            // A string in a value position; keep scanning after it.
            i = after;
            continue;
        }
        if text == key {
            let mut start = colon + 1;
            while bytes.get(start).is_some_and(u8::is_ascii_whitespace) {
                start += 1;
            }
            if bytes.get(start) != Some(&b'"') {
                return None;
            }
            let (value, _) = string_literal(json, start)?;
            return Some(value);
        }
        i = colon + 1;
    }
    None
}

/// Decodes the JSON string literal that starts at byte `start` (which holds `"`), returning
/// its text and the byte index just after the closing quote.
fn string_literal(json: &str, start: usize) -> Option<(String, usize)> {
    let mut text = String::new();
    let mut chars = json[start + 1..].char_indices().peekable();
    while let Some((offset, ch)) = chars.next() {
        match ch {
            '"' => return Some((text, start + 1 + offset + 1)),
            '\\' => match chars.next()?.1 {
                '"' => text.push('"'),
                '\\' => text.push('\\'),
                '/' => text.push('/'),
                'b' => text.push('\u{8}'),
                'f' => text.push('\u{c}'),
                'n' => text.push('\n'),
                'r' => text.push('\r'),
                't' => text.push('\t'),
                'u' => {
                    let mut unit = hex4(&mut chars)?;
                    if (0xd800..0xdc00).contains(&unit) {
                        // A character outside the BMP arrives as two escapes, and only a
                        // following low surrogate makes them one char.
                        let mut look = chars.clone();
                        let low = match look.next() {
                            Some((_, '\\')) => match look.next() {
                                Some((_, 'u')) => hex4(&mut look),
                                _ => None,
                            },
                            _ => None,
                        };
                        match low {
                            Some(low) if (0xdc00..0xe000).contains(&low) => {
                                chars = look;
                                unit = 0x10000 + ((unit - 0xd800) << 10) + (low - 0xdc00);
                            }
                            _ => unit = 0xfffd,
                        }
                    }
                    text.push(char::from_u32(unit).unwrap_or('\u{fffd}'));
                }
                _ => return None,
            },
            ch if (ch as u32) < 0x20 => return None,
            ch => text.push(ch),
        }
    }
    // Unterminated literal.
    None
}

/// Reads four hex digits of a `\uXXXX` escape.
fn hex4(chars: &mut std::iter::Peekable<std::str::CharIndices<'_>>) -> Option<u32> {
    let mut value = 0;
    for _ in 0..4 {
        value = value * 16 + chars.next()?.1.to_digit(16)?;
    }
    Some(value)
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

/// The `tool_outcome` shape: `status` is the closed set of `ToolStatus` names.
fn outcome(status: &str, content: &str) -> String {
    format!(
        "{{\"status\":{},\"content\":{}}}",
        json_text(status),
        json_text(content)
    )
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
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');
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
}
