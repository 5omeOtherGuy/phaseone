//! The guest behaviour of the `read_output` tool, shared by the native adapter
//! (`ReadOutputTool` in `p1-tool-read-output`) and the component
//! (`modules/p1-module-read-output`, `p1/read-output`).
//!
//! `read_output` pages an output the host stored (ADR-0109) by a zero-based UTF-8 byte cursor.
//! Everything here is pure computation (decision D-XO-8): input parsing and validation, the
//! declaration, the call and result descriptions, the page footer and the model-facing text of
//! every store error. Reading the store is the caller's, through [`Outputs`]: the native adapter
//! calls the host's store service directly, the component calls the `tool-outputs` capability,
//! so both hosts run this same source (decision S0-R3, docs/design/modules/package.md "Shared
//! guest logic").
//!
//! Adapted from iris `src/tools/read_output.rs` (read-only donor, ADR-0001): iris pages a stored
//! result by lines through `read`'s window; p1's store is paged by a byte cursor that never
//! splits a character (`modules/wit/outputs.wit`), so the input and the footer are p1's own.
#![forbid(unsafe_code)]

use serde::Deserialize;

/// The tool's name.
pub const NAME: &str = "read_output";
/// The tool's description.
pub const DESCRIPTION: &str = "Page through the full output of an earlier shell command that the host stored. A shell result whose output was cut or summarised names the stored output's `handle_id` on its `[stored output: ...]` line.\n`offset` is a zero-based byte offset (default 0) and `limit` the most bytes to return (default and maximum 50000). A page never splits a character; its last line gives `next_offset` for the next page, or `end`.\nOnly outputs of the current run are served; the stored text is masked for credentials.";
/// The verb of every call description (ADR-0057).
pub const VERB: &str = "read";
/// The byte limit when the input names none, and the largest one it may name.
pub const MAX_LIMIT: u32 = 50_000;

/// The tool's JSON input schema.
pub fn input_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "handle_id": {
                "type": "string",
                "minLength": 1,
                "description": "The handle a shell result names on its `[stored output: ...]` line."
            },
            "offset": {
                "type": "integer",
                "minimum": 0,
                "default": 0,
                "description": "Zero-based byte offset of the page: 0, or a `next_offset` an earlier page gave."
            },
            "limit": {
                "type": "integer",
                "minimum": 1,
                "maximum": 50000,
                "default": 50000,
                "description": "The most bytes to return."
            }
        },
        "required": ["handle_id"],
        "additionalProperties": false
    })
}

/// A call's raw input, as either host carries it.
#[derive(Debug, Clone, Copy)]
pub enum RawInput<'a> {
    /// A function call's JSON arguments.
    Json(&'a str),
    /// A freeform call's text, which `read_output` does not take.
    Text(&'a str),
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WireInput {
    handle_id: String,
    #[serde(default)]
    offset: Option<i64>,
    #[serde(default)]
    limit: Option<i64>,
}

/// The validated input of one call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReadOutputInput {
    /// The opaque host handle.
    pub handle_id: String,
    /// The zero-based byte offset.
    pub offset: u64,
    /// The most bytes of the page, 1 to [`MAX_LIMIT`].
    pub limit: u32,
}

/// Parse and validate `raw` for the tool presented as `tool`.
pub fn parse_input(tool: &str, raw: RawInput<'_>) -> Result<ReadOutputInput, String> {
    let raw = match raw {
        RawInput::Json(raw) => raw,
        RawInput::Text(_) => {
            return Err(invalid(
                tool,
                "expected a JSON object input, got freeform text",
            ));
        }
    };
    let input: WireInput =
        serde_json::from_str(raw).map_err(|error| invalid(tool, &error.to_string()))?;
    if input.handle_id.is_empty() {
        return Err(invalid(tool, "`handle_id` must not be empty"));
    }
    let offset = match input.offset {
        None => 0,
        Some(offset) => {
            u64::try_from(offset).map_err(|_| invalid(tool, "`offset` must be 0 or more"))?
        }
    };
    let limit = match input.limit {
        None => MAX_LIMIT,
        Some(limit) => u32::try_from(limit)
            .ok()
            .filter(|limit| (1..=MAX_LIMIT).contains(limit))
            .ok_or_else(|| invalid(tool, "`limit` must be between 1 and 50000"))?,
    };
    Ok(ReadOutputInput {
        handle_id: input.handle_id,
        offset,
        limit,
    })
}

/// The invalid-input message the model acts on.
pub fn invalid(tool: &str, reason: &str) -> String {
    format!("Invalid input for {tool}: {reason}")
}

/// ADR-0057: the handle a call reads, with its byte window; `None` for input that does not
/// parse — never a guess.
pub fn describe_target(tool: &str, raw: RawInput<'_>) -> Option<String> {
    parse_input(tool, raw)
        .ok()
        .map(|input| format!("{}:{}+{}", input.handle_id, input.offset, input.limit))
}

/// A result's one-line summary: the footer of a page, the first line of an error.
pub fn describe_result(content: &str, ok: bool) -> String {
    let line = if ok {
        content.lines().last()
    } else {
        content.lines().next()
    };
    line.unwrap_or_default().to_owned()
}

/// How much of an output the store holds (`tool-outputs.capture`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Capture {
    /// Everything the command printed.
    Complete,
    /// The store stopped at its byte cap; what it holds is exact up to there.
    StoredCapReached,
    /// The store fell behind and stopped; what it holds is exact up to there.
    StorageIncomplete,
    /// Nothing is recoverable.
    StorageFailed,
}

impl Capture {
    /// The footer's words for the state.
    fn words(self) -> &'static str {
        match self {
            Self::Complete => "capture complete",
            Self::StoredCapReached => {
                "capture stopped at the store's cap (the command printed more)"
            }
            Self::StorageIncomplete => {
                "capture incomplete (the store fell behind; later output was not stored)"
            }
            Self::StorageFailed => "capture failed",
        }
    }
}

/// What the store holds under a handle (`tool-outputs.output-info`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutputInfo {
    /// The opaque host handle.
    pub handle: String,
    /// The stored bytes of masked UTF-8 text.
    pub stored_bytes: u64,
    /// How much of the output that is.
    pub capture: Capture,
}

/// One page (`tool-outputs.output-page`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutputPage {
    /// The text from the requested offset; never a split character.
    pub text: String,
    /// The requested offset plus the bytes of `text`.
    pub next_offset: u64,
    /// `next_offset` is the end of the stored output.
    pub at_end: bool,
}

/// Why the store could not answer (`tool-outputs.output-error`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OutputError {
    /// No output under the handle.
    UnknownOutput,
    /// The limit is smaller than the character at the offset.
    LimitTooSmall,
    /// The offset lies past the end; the output's stored bytes.
    OffsetPastEnd(u64),
    /// The offset lies inside a character.
    OffsetInsideCharacter,
    /// The store could not be read, as the host words it.
    ReadFailed(String),
}

/// The store as the tool reads it: the host's service natively, the `tool-outputs` capability
/// in the component.
pub trait Outputs {
    /// What the store holds under `handle`.
    fn describe(&self, handle: &str) -> Result<OutputInfo, OutputError>;
    /// Up to `limit` bytes of the output under `handle` from byte `offset`.
    fn page(&self, handle: &str, offset: u64, limit: u32) -> Result<OutputPage, OutputError>;
}

/// How a call ended, as the model is told.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Ok,
    Error,
}

/// A call's result: `content` is exactly what the model sees.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outcome {
    pub status: Status,
    pub content: String,
}

impl Outcome {
    fn error(content: String) -> Self {
        Self {
            status: Status::Error,
            content,
        }
    }
}

/// Runs one call of the tool presented as `tool` over `outputs`: the page text, then one footer
/// line with the byte range, the stored bytes, the capture state and `next_offset` or `end`.
pub fn execute(tool: &str, raw: RawInput<'_>, outputs: &impl Outputs) -> Outcome {
    let input = match parse_input(tool, raw) {
        Ok(input) => input,
        Err(message) => return Outcome::error(message),
    };
    let fail = |error| Outcome::error(error_text(tool, &input, error));
    let info = match outputs.describe(&input.handle_id) {
        Ok(info) => info,
        Err(error) => return fail(error),
    };
    if info.capture == Capture::StorageFailed {
        // The host never serves such an output (ADR-0109 item 5); say so the same way.
        return fail(OutputError::UnknownOutput);
    }
    let page = match outputs.page(&input.handle_id, input.offset, input.limit) {
        Ok(page) => page,
        Err(error) => return fail(error),
    };
    // A store that grew after `describe` (a command still running) is reported as it was
    // described; the page's own end is what `next_offset` says.
    let stored = info.stored_bytes.max(page.next_offset);
    let cursor = if page.at_end {
        "end".to_owned()
    } else {
        format!("next_offset {}", page.next_offset)
    };
    let footer = format!(
        "[{tool}: bytes {}-{} of {stored} stored; {}; {cursor}]",
        input.offset,
        page.next_offset,
        info.capture.words(),
    );
    let content = if page.text.is_empty() {
        footer
    } else {
        format!("{}\n{footer}", page.text)
    };
    Outcome {
        status: Status::Ok,
        content,
    }
}

/// The page text of a successful result: everything before the footer line. Byte-exact,
/// because [`execute`] puts exactly one line break between the page and its footer.
pub fn page_text(content: &str) -> &str {
    content.rsplit_once('\n').map_or("", |(text, _)| text)
}

/// The model-facing text of a store error for `input`.
fn error_text(tool: &str, input: &ReadOutputInput, error: OutputError) -> String {
    let handle = &input.handle_id;
    let offset = input.offset;
    match error {
        OutputError::UnknownOutput => format!(
            "{tool}: no stored output has the handle `{handle}`. Handles come from shell results of this run; an output of an earlier run, of another session, or one the host could not store is not served."
        ),
        OutputError::LimitTooSmall => format!(
            "{tool}: `limit` {} is smaller than the character at offset {offset}; ask again with a limit of at least 4.",
            input.limit
        ),
        OutputError::OffsetPastEnd(stored) => format!(
            "{tool}: offset {offset} is past the end of `{handle}`, which holds {stored} bytes; offsets run from 0 to {stored}."
        ),
        OutputError::OffsetInsideCharacter => format!(
            "{tool}: offset {offset} lies inside a multi-byte character of `{handle}`; use 0 or a `next_offset` an earlier page gave."
        ),
        OutputError::ReadFailed(reason) => {
            format!("{tool}: the stored output `{handle}` could not be read: {reason}")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    /// A store over one text, paged exactly as the host pages (`outputs/store.rs`): a page ends
    /// on a character boundary, a limit smaller than the next character is refused.
    struct Fake {
        text: String,
        capture: Capture,
        calls: RefCell<Vec<(u64, u32)>>,
    }

    impl Fake {
        fn new(text: &str) -> Self {
            Self {
                text: text.to_owned(),
                capture: Capture::Complete,
                calls: RefCell::new(Vec::new()),
            }
        }
    }

    impl Outputs for Fake {
        fn describe(&self, handle: &str) -> Result<OutputInfo, OutputError> {
            if handle != "out-1" {
                return Err(OutputError::UnknownOutput);
            }
            Ok(OutputInfo {
                handle: handle.to_owned(),
                stored_bytes: self.text.len() as u64,
                capture: self.capture,
            })
        }

        fn page(&self, _handle: &str, offset: u64, limit: u32) -> Result<OutputPage, OutputError> {
            self.calls.borrow_mut().push((offset, limit));
            let len = self.text.len() as u64;
            if offset > len {
                return Err(OutputError::OffsetPastEnd(len));
            }
            let start = offset as usize;
            if !self.text.is_char_boundary(start) {
                return Err(OutputError::OffsetInsideCharacter);
            }
            let mut end = (start + limit as usize).min(self.text.len());
            while !self.text.is_char_boundary(end) {
                end -= 1;
            }
            if end == start && start < self.text.len() {
                return Err(OutputError::LimitTooSmall);
            }
            Ok(OutputPage {
                text: self.text[start..end].to_owned(),
                next_offset: end as u64,
                at_end: end == self.text.len(),
            })
        }
    }

    fn run(fake: &Fake, input: serde_json::Value) -> Outcome {
        execute(NAME, RawInput::Json(&input.to_string()), fake)
    }

    #[test]
    fn the_schema_is_the_audit_schema() {
        let schema = input_schema();
        assert_eq!(schema["required"], serde_json::json!(["handle_id"]));
        assert_eq!(schema["additionalProperties"], false);
        assert_eq!(schema["properties"]["handle_id"]["minLength"], 1);
        assert_eq!(schema["properties"]["offset"]["minimum"], 0);
        assert_eq!(schema["properties"]["offset"]["default"], 0);
        assert_eq!(schema["properties"]["limit"]["minimum"], 1);
        assert_eq!(schema["properties"]["limit"]["maximum"], 50_000);
        assert_eq!(schema["properties"]["limit"]["default"], 50_000);
    }

    #[test]
    fn input_is_validated() {
        let parse = |raw: &str| parse_input(NAME, RawInput::Json(raw));
        assert_eq!(
            parse(r#"{"handle_id":"h"}"#).unwrap(),
            ReadOutputInput {
                handle_id: "h".into(),
                offset: 0,
                limit: 50_000
            }
        );
        for (raw, reason) in [
            (r#"{"handle_id":""}"#, "`handle_id` must not be empty"),
            (
                r#"{"handle_id":"h","offset":-1}"#,
                "`offset` must be 0 or more",
            ),
            (
                r#"{"handle_id":"h","limit":0}"#,
                "`limit` must be between 1 and 50000",
            ),
            (
                r#"{"handle_id":"h","limit":50001}"#,
                "`limit` must be between 1 and 50000",
            ),
        ] {
            assert_eq!(parse(raw).unwrap_err(), invalid(NAME, reason), "{raw}");
        }
        for raw in [
            r#"{"handle_id":"h","extra":1}"#,
            r#"{}"#,
            r#"{"handle_id":5}"#,
            "not json",
        ] {
            assert!(
                parse(raw)
                    .unwrap_err()
                    .starts_with("Invalid input for read_output: "),
                "{raw}"
            );
        }
        assert!(parse_input(NAME, RawInput::Text("h")).is_err());
    }

    #[test]
    fn a_page_ends_with_one_footer_line() {
        let fake = Fake::new("one\ntwo\nthree\n");
        let first = run(&fake, serde_json::json!({"handle_id": "out-1", "limit": 8}));
        assert_eq!(first.status, Status::Ok);
        assert_eq!(
            first.content,
            "one\ntwo\n\n[read_output: bytes 0-8 of 14 stored; capture complete; next_offset 8]"
        );
        assert_eq!(page_text(&first.content), "one\ntwo\n");
        let last = run(
            &fake,
            serde_json::json!({"handle_id": "out-1", "offset": 8}),
        );
        assert_eq!(
            last.content,
            "three\n\n[read_output: bytes 8-14 of 14 stored; capture complete; end]"
        );
        let empty = run(
            &fake,
            serde_json::json!({"handle_id": "out-1", "offset": 14}),
        );
        assert_eq!(
            empty.content,
            "[read_output: bytes 14-14 of 14 stored; capture complete; end]"
        );
        assert_eq!(page_text(&empty.content), "");
    }

    #[test]
    fn every_store_error_has_its_own_text() {
        let fake = Fake::new("é");
        let unknown = run(&fake, serde_json::json!({"handle_id": "out-9"}));
        assert_eq!(unknown.status, Status::Error);
        assert!(
            unknown
                .content
                .contains("no stored output has the handle `out-9`")
        );
        let small = run(&fake, serde_json::json!({"handle_id": "out-1", "limit": 1}));
        assert!(
            small
                .content
                .contains("`limit` 1 is smaller than the character at offset 0")
        );
        let past = run(
            &fake,
            serde_json::json!({"handle_id": "out-1", "offset": 3}),
        );
        assert!(
            past.content
                .contains("offset 3 is past the end of `out-1`, which holds 2 bytes")
        );
        let inside = run(
            &fake,
            serde_json::json!({"handle_id": "out-1", "offset": 1}),
        );
        assert!(
            inside
                .content
                .contains("offset 1 lies inside a multi-byte character")
        );
        assert_eq!(
            error_text(
                NAME,
                &parse_input(NAME, RawInput::Json(r#"{"handle_id":"h"}"#)).unwrap(),
                OutputError::ReadFailed("disk gone".into())
            ),
            "read_output: the stored output `h` could not be read: disk gone"
        );
    }

    #[test]
    fn a_failed_capture_is_never_paged() {
        let mut fake = Fake::new("x");
        fake.capture = Capture::StorageFailed;
        let outcome = run(&fake, serde_json::json!({"handle_id": "out-1"}));
        assert_eq!(outcome.status, Status::Error);
        assert!(fake.calls.borrow().is_empty());
    }

    #[test]
    fn the_capture_state_is_in_the_footer() {
        let mut fake = Fake::new("x");
        fake.capture = Capture::StoredCapReached;
        let outcome = run(&fake, serde_json::json!({"handle_id": "out-1"}));
        assert!(
            outcome
                .content
                .ends_with("capture stopped at the store's cap (the command printed more); end]")
        );
        fake.capture = Capture::StorageIncomplete;
        let outcome = run(&fake, serde_json::json!({"handle_id": "out-1"}));
        assert!(outcome.content.contains("capture incomplete"));
    }

    /// #526: the search fields are additive; the schema stays closed.
    #[test]
    fn the_schema_offers_a_pattern_and_grep_s_literal_flag() {
        let schema = input_schema();
        assert_eq!(schema["properties"]["pattern"]["type"], "string");
        assert_eq!(schema["properties"]["pattern"]["minLength"], 1);
        assert_eq!(schema["properties"]["literal"]["type"], "boolean");
        assert_eq!(schema["properties"]["literal"]["default"], false);
        assert_eq!(schema["required"], serde_json::json!(["handle_id"]));
        assert_eq!(schema["additionalProperties"], false);
    }

    /// #526: each matching line is listed with the byte offset it starts at, a later page
    /// from that offset starts with that line, and the footer names the bytes scanned.
    #[test]
    fn a_pattern_lists_the_matching_lines_with_their_byte_offsets() {
        let text = "ok 1\nerror: é broke\nok 2\r\nerror: last";
        let fake = Fake::new(text);
        let outcome = run(
            &fake,
            serde_json::json!({"handle_id": "out-1", "pattern": "^error: \\S+"}),
        );
        assert_eq!(outcome.status, Status::Ok, "{}", outcome.content);
        assert_eq!(
            outcome.content,
            "5: error: é broke\n27: error: last\n[read_output: 2 matches in bytes 0-38 of 38 stored; capture complete; end]"
        );
        let page = run(
            &fake,
            serde_json::json!({"handle_id": "out-1", "offset": 5, "limit": 16}),
        );
        assert_eq!(page_text(&page.content), "error: é broke\n");
        // A carriage return before the line break is not part of the line: `$` matches.
        let crlf = run(
            &fake,
            serde_json::json!({"handle_id": "out-1", "pattern": "^ok 2$"}),
        );
        assert_eq!(
            crlf.content,
            "21: ok 2\n[read_output: 1 match in bytes 0-38 of 38 stored; capture complete; end]"
        );
        assert_eq!(
            describe_result(&outcome.content, true),
            "[read_output: 2 matches in bytes 0-38 of 38 stored; capture complete; end]"
        );
    }

    #[test]
    fn a_pattern_without_a_match_says_so_in_its_footer() {
        let fake = Fake::new("one\ntwo\n");
        let outcome = run(
            &fake,
            serde_json::json!({"handle_id": "out-1", "pattern": "three"}),
        );
        assert_eq!(outcome.status, Status::Ok);
        assert_eq!(
            outcome.content,
            "[read_output: 0 matches in bytes 0-8 of 8 stored; capture complete; end]"
        );
    }

    /// #526: the scan starts at `offset`; an offset inside a character is the store's error.
    #[test]
    fn a_pattern_scan_starts_at_the_offset() {
        let fake = Fake::new("error a\nerror é\nerror c\n");
        let outcome = run(
            &fake,
            serde_json::json!({"handle_id": "out-1", "pattern": "error", "offset": 8}),
        );
        assert_eq!(
            outcome.content,
            "8: error é\n17: error c\n[read_output: 2 matches in bytes 8-25 of 25 stored; capture complete; end]"
        );
        let inside = run(
            &fake,
            serde_json::json!({"handle_id": "out-1", "pattern": "error", "offset": 15}),
        );
        assert_eq!(inside.status, Status::Error);
        assert!(
            inside
                .content
                .contains("offset 15 lies inside a multi-byte character"),
            "{}",
            inside.content
        );
    }

    /// #526: `literal` follows grep: the pattern's regular-expression characters match
    /// themselves; without it the pattern is a regular expression and a bad one is invalid input.
    #[test]
    fn literal_matches_exact_text_as_grep_does() {
        let fake = Fake::new("a.b(\naxb(\n");
        let literal = run(
            &fake,
            serde_json::json!({"handle_id": "out-1", "pattern": "a.b(", "literal": true}),
        );
        assert_eq!(
            literal.content,
            "0: a.b(\n[read_output: 1 match in bytes 0-10 of 10 stored; capture complete; end]"
        );
        let regex = run(
            &fake,
            serde_json::json!({"handle_id": "out-1", "pattern": "a.b\\("}),
        );
        assert!(regex.content.starts_with("0: a.b(\n5: axb(\n"), "{}", regex.content);
        let bad = run(
            &fake,
            serde_json::json!({"handle_id": "out-1", "pattern": "a.b("}),
        );
        assert_eq!(bad.status, Status::Error);
        assert!(
            bad.content
                .starts_with("Invalid input for read_output: `pattern` is not a valid regular expression"),
            "{}",
            bad.content
        );
        assert!(fake.calls.borrow().len() == 2, "a bad pattern reads nothing");
    }

    /// #526: inputs that mix the search with paging, or name an empty pattern, are refused.
    #[test]
    fn search_input_is_validated() {
        let fake = Fake::new("x\n");
        for (input, reason) in [
            (
                serde_json::json!({"handle_id": "out-1", "pattern": ""}),
                "`pattern` must not be empty",
            ),
            (
                serde_json::json!({"handle_id": "out-1", "literal": true}),
                "`literal` applies only with `pattern`",
            ),
            (
                serde_json::json!({"handle_id": "out-1", "pattern": "x", "limit": 10}),
                "`limit` pages bytes and does not apply with `pattern`",
            ),
        ] {
            let outcome = run(&fake, input.clone());
            assert_eq!(outcome.status, Status::Error, "{input}");
            assert_eq!(outcome.content, invalid(NAME, reason), "{input}");
        }
        let unknown = run(
            &fake,
            serde_json::json!({"handle_id": "out-1", "pattern": "x", "context": 2}),
        );
        assert!(
            unknown
                .content
                .starts_with("Invalid input for read_output: unknown field `context`"),
            "{}",
            unknown.content
        );
        assert!(fake.calls.borrow().is_empty());
    }

    /// #526: at most 30 matches, each line cut to 200 characters (never inside a character);
    /// the footer names the offset to continue from, and continuing finds the rest.
    #[test]
    fn a_pattern_result_is_bounded_and_resumable() {
        let long = format!("hit {}\n", "é".repeat(300));
        let mut text = String::new();
        for index in 0..45 {
            text.push_str(&format!("hit {index}\nmiss\n"));
        }
        text.push_str(&long);
        let fake = Fake::new(&text);
        let first = run(
            &fake,
            serde_json::json!({"handle_id": "out-1", "pattern": "hit"}),
        );
        let lines: Vec<&str> = first.content.lines().collect();
        assert_eq!(lines.len(), 31, "{}", first.content);
        assert_eq!(lines[29], format!("{}: hit 29", text.find("hit 29").unwrap()));
        let resume = text.find("miss\nhit 30").unwrap() as u64;
        assert_eq!(
            lines[30],
            format!(
                "[read_output: 30 matches in bytes 0-{resume} of {} stored; capture complete; match limit 30 reached, continue with offset {resume}]",
                text.len()
            )
        );
        let rest = run(
            &fake,
            serde_json::json!({"handle_id": "out-1", "pattern": "hit", "offset": resume}),
        );
        let lines: Vec<&str> = rest.content.lines().collect();
        assert_eq!(lines.len(), 17, "{}", rest.content);
        assert_eq!(lines[0], format!("{}: hit 30", resume + 5));
        let cut = format!("hit {}…", "é".repeat(196));
        assert_eq!(lines[15], format!("{}: {cut}", text.find(&long).unwrap()));
        assert!(lines[16].ends_with("; capture complete; end]"), "{}", lines[16]);
    }

    #[test]
    fn descriptions_come_from_the_input_and_the_footer() {
        assert_eq!(
            describe_target(NAME, RawInput::Json(r#"{"handle_id":"out-1","offset":5}"#)),
            Some("out-1:5+50000".into())
        );
        assert_eq!(describe_target(NAME, RawInput::Json("{")), None);
        assert_eq!(describe_result("a\nb\n[footer]", true), "[footer]");
        assert_eq!(describe_result("bad\nmore", false), "bad");
    }
}
