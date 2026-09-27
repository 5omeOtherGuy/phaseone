//! The `edit` tool's pure logic: the model-facing declaration, input validation, the
//! exact-match replacement with its BOM and line-ending handling, the output text and the
//! call and result descriptions.
//!
//! It is the one copy of that logic: the native `p1-tool-edit` and the component
//! `modules/p1-module-edit/` both call it, so their texts cannot drift apart (decision S0-R3
//! in `docs/design/modules/package.md`). It is target-independent by construction: std,
//! serde and serde_json only, and no filesystem, thread or clock. Everything that touches a
//! file goes through the capabilities [`exec`] is handed.

pub mod exec;
pub mod wire;

use serde::Deserialize;

/// The default model-facing tool name.
pub const NAME: &str = "edit";
/// The default model-facing description.
pub const DESCRIPTION: &str = "Replace an exact string in an existing workspace file.\n`old_string` must match uniquely unless `replace_all` is set; it must differ from `new_string`.\nRead the file first: the edit is refused if you have never read it, or if it changed on disk since you did.\nThe file's line endings and final newline are preserved.";
/// The call-description verb (ADR-0057), one of the closed vocabulary of `protocol.md`.
pub const VERB: &str = "edit";
/// The most output bytes the model is shown.
pub const MAX_OUTPUT_BYTES: usize = 50_000;
/// The most output lines the model is shown.
pub const MAX_OUTPUT_LINES: usize = 2_000;

/// The input JSON Schema of the declaration.
pub fn input_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "file_path": {
                "type": "string",
                "description": "File path, relative to the workspace root or absolute inside it."
            },
            "old_string": {
                "type": "string",
                "minLength": 1,
                "description": "Exact text to replace; must be unique unless replace_all is set."
            },
            "new_string": {
                "type": "string",
                "description": "Replacement text; must differ from old_string."
            },
            "replace_all": {
                "type": "boolean",
                "default": false,
                "description": "Replace every occurrence instead of requiring a unique match."
            }
        },
        "required": ["file_path", "old_string", "new_string"],
        "additionalProperties": false
    })
}

/// A validated `edit` input.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EditInput {
    pub file_path: String,
    pub old_string: String,
    pub new_string: String,
    #[serde(default)]
    pub replace_all: bool,
}

/// Parse and validate a JSON input; `tool` is the name the model called, for the message.
pub fn parse_json_input(tool: &str, raw: &str) -> Result<EditInput, String> {
    let input: EditInput =
        serde_json::from_str(raw).map_err(|error| invalid(tool, &error.to_string()))?;
    if input.old_string.is_empty() {
        return Err(invalid(tool, "`old_string` must not be empty"));
    }
    if input.old_string == input.new_string {
        return Err(invalid(tool, "`old_string` and `new_string` must differ"));
    }
    Ok(input)
}

/// The error for a freeform text input, which this function tool never accepts.
pub fn text_input_error(tool: &str) -> String {
    invalid(tool, "expected a JSON object input, got freeform text")
}

fn invalid(tool: &str, reason: &str) -> String {
    format!("Invalid input for {tool}: {reason}")
}

/// The target does not exist.
pub fn does_not_exist(display: &str) -> String {
    format!("{display} does not exist.")
}

/// The target exists but could not be read.
pub fn could_not_read(display: &str, error: &str) -> String {
    format!("{display} could not be read: {error}")
}

/// Read-before-mutate: this agent never observed the target.
pub fn never_observed(display: &str) -> String {
    format!("You must read {display} before changing it.")
}

/// Read-before-mutate: the target changed since this agent observed it.
pub fn changed_since_observed(display: &str) -> String {
    format!("{display} changed on disk since you last read it; read it again.")
}

/// The replacement could not be written.
pub fn failed_to_write(display: &str, error: &str) -> String {
    format!("failed to write {display}: {error}")
}

/// The confinement refusal, worded as the workspace service words it.
pub fn escapes_workspace(requested: &str) -> String {
    format!("path escapes workspace: {requested}")
}

/// The new contents of an edited file and how many occurrences were replaced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Edited {
    pub contents: String,
    pub replacements: usize,
}

/// Apply `input` to `bytes`, the current contents of the file shown as `display`.
///
/// Matching happens on LF-normalized text; the file's own ending is restored on write, so a
/// CRLF file stays CRLF and a missing final newline stays missing. A UTF-8 BOM is kept.
pub fn edit_text(display: &str, bytes: &[u8], input: &EditInput) -> Result<Edited, String> {
    let text = std::str::from_utf8(bytes).map_err(|_| format!("{display} is not valid UTF-8."))?;
    let (body, had_bom) = strip_bom(text);
    let ending = detect_line_ending(body);
    let normalized = normalize_to_lf(body);
    let old_string = normalize_to_lf(&input.old_string);
    let matches = find_all(&normalized, &old_string);
    if matches.is_empty() {
        return Err(format!("old_string was not found in {display}."));
    }
    if matches.len() > 1 && !input.replace_all {
        return Err(format!(
            "old_string occurs {} times in {display}; add context to make it unique or set replace_all.",
            matches.len()
        ));
    }
    let replacements = if input.replace_all { matches.len() } else { 1 };

    let new_string = normalize_to_lf(&input.new_string);
    let mut replaced = String::with_capacity(normalized.len());
    let mut cursor = 0;
    for start in matches {
        replaced.push_str(&normalized[cursor..start]);
        replaced.push_str(&new_string);
        cursor = start + old_string.len();
    }
    replaced.push_str(&normalized[cursor..]);

    let restored = restore_line_endings(&replaced, ending);
    let contents = if had_bom {
        format!("\u{FEFF}{restored}")
    } else {
        restored
    };
    Ok(Edited {
        contents,
        replacements,
    })
}

/// The model-facing output of a successful edit, already bounded.
pub fn edited_output(display: &str, replacements: usize) -> String {
    let plural = if replacements == 1 { "" } else { "s" };
    bound_output(
        &format!("Edited {display} ({replacements} replacement{plural})."),
        MAX_OUTPUT_BYTES,
        MAX_OUTPUT_LINES,
    )
}

/// All non-overlapping occurrences of `needle`, as ascending byte offsets.
fn find_all(haystack: &str, needle: &str) -> Vec<usize> {
    if needle.is_empty() {
        return Vec::new();
    }
    let mut positions = Vec::new();
    let mut from = 0;
    while let Some(relative) = haystack[from..].find(needle) {
        let absolute = from + relative;
        positions.push(absolute);
        from = absolute + needle.len();
    }
    positions
}

fn strip_bom(text: &str) -> (&str, bool) {
    text.strip_prefix('\u{FEFF}')
        .map_or((text, false), |stripped| (stripped, true))
}

/// The file's dominant line ending, taken from its first line break.
fn detect_line_ending(text: &str) -> &'static str {
    let bytes = text.as_bytes();
    for (index, byte) in bytes.iter().enumerate() {
        match byte {
            b'\r' => {
                return if bytes.get(index + 1) == Some(&b'\n') {
                    "\r\n"
                } else {
                    "\r"
                };
            }
            b'\n' => return "\n",
            _ => {}
        }
    }
    "\n"
}

fn normalize_to_lf(text: &str) -> String {
    if !text.contains('\r') {
        return text.to_string();
    }
    let mut out = String::with_capacity(text.len());
    let mut characters = text.chars().peekable();
    while let Some(character) = characters.next() {
        if character == '\r' {
            out.push('\n');
            if characters.peek() == Some(&'\n') {
                characters.next();
            }
        } else {
            out.push(character);
        }
    }
    out
}

fn restore_line_endings(text: &str, ending: &str) -> String {
    match ending {
        "\r\n" => text.replace('\n', "\r\n"),
        "\r" => text.replace('\n', "\r"),
        _ => text.to_string(),
    }
}

/// Bound text shown to the model to `max_bytes` bytes and `max_lines` lines, cutting on a
/// char boundary, with a trailer when anything was cut.
///
/// The same algorithm and trailer as `p1_workspace::bound_output`, which the native file
/// tools share: this crate may not depend on `p1-workspace`, and the component must print
/// exactly what the native tool prints. `p1-tool-edit`'s tests hold the two equal.
pub fn bound_output(text: &str, max_bytes: usize, max_lines: usize) -> String {
    let total_bytes = text.len();
    let mut end = total_bytes;
    let mut truncated = false;

    // A trailing newline does not create an extra line, matching how the file tools count.
    if max_lines > 0 {
        let mut newlines = 0usize;
        for (index, character) in text.char_indices() {
            if character == '\n' {
                newlines += 1;
                if newlines == max_lines {
                    let next_line = index + 1;
                    if next_line < total_bytes {
                        end = next_line;
                        truncated = true;
                    }
                    break;
                }
            }
        }
    }

    if max_bytes < end {
        let mut cut = max_bytes;
        while cut > 0 && !text.is_char_boundary(cut) {
            cut -= 1;
        }
        end = cut;
        truncated = true;
    }

    if !truncated {
        return text.to_string();
    }

    let mut out = String::with_capacity(end + 64);
    out.push_str(&text[..end]);
    // The trailer goes on its own line without a blank line when a line-cap cut already
    // ends the kept slice with a newline.
    if !out.ends_with('\n') {
        out.push('\n');
    }
    out.push_str(&format!(
        "[output truncated: showing {end} of {total_bytes} bytes]"
    ));
    out
}

/// Whether `requested` leaves the workspace by its spelling alone: `..` that climbs above the
/// root, or an absolute path.
///
/// A component describes a call on the restricted path, with no capability, so it cannot
/// resolve symlinks or learn the root; an absolute path is therefore flagged, because the
/// guest cannot show it is inside. Confinement itself is enforced again by the host at
/// `execute` whatever this says. The native tool decides with the real resolution instead.
pub fn escapes_lexically(requested: &str) -> bool {
    if requested.starts_with('/') {
        return true;
    }
    let mut depth = 0usize;
    for component in requested.split('/') {
        match component {
            "" | "." => {}
            ".." => match depth.checked_sub(1) {
                Some(up) => depth = up,
                None => return true,
            },
            _ => depth += 1,
        }
    }
    false
}

/// `requested` with `.` and `..` collapsed and empty components dropped, `/`-separated; for a
/// relative request inside the root this is how the workspace service displays it.
pub fn lexical_normalize(requested: &str) -> String {
    let mut out: Vec<&str> = Vec::new();
    for component in requested.split('/') {
        match component {
            "" | "." => {}
            ".." => {
                out.pop();
            }
            other => out.push(other),
        }
    }
    out.join("/")
}

/// What a result describes (ADR-0059): a one-line summary and, for a successful edit, the
/// diff of the replaced text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResultSummary {
    pub summary: String,
    pub diff: Option<Diff>,
}

/// The replaced text of one edit call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diff {
    pub path: String,
    pub before: String,
    pub after: String,
}

/// Describe the result `content` of a call whose parsed input is `input` (`None` when the
/// input does not parse); `ok` is whether the result's status is ok.
pub fn describe_result(input: Option<EditInput>, ok: bool, content: &str) -> ResultSummary {
    let (true, Some(input)) = (ok, input) else {
        return ResultSummary {
            summary: content.lines().next().unwrap_or_default().to_string(),
            diff: None,
        };
    };
    let replacements = parenthesized_count(content, "replacement").unwrap_or(1);
    ResultSummary {
        summary: format!(
            "+{} −{}",
            input.new_string.lines().count() * replacements,
            input.old_string.lines().count() * replacements
        ),
        diff: Some(Diff {
            path: input.file_path,
            before: input.old_string,
            after: input.new_string,
        }),
    }
}

fn parenthesized_count(content: &str, unit: &str) -> Option<usize> {
    let rest = &content[content.rfind('(')? + 1..];
    let digits_end = rest.find(|c: char| !c.is_ascii_digit())?;
    let count = rest[..digits_end].parse().ok()?;
    rest[digits_end..]
        .trim_start()
        .starts_with(unit)
        .then_some(count)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input(old: &str, new: &str, replace_all: bool) -> EditInput {
        EditInput {
            file_path: "f.txt".into(),
            old_string: old.into(),
            new_string: new.into(),
            replace_all,
        }
    }

    #[test]
    fn the_schema_is_the_declared_one() {
        let schema = input_schema();
        assert_eq!(
            schema["required"],
            serde_json::json!(["file_path", "old_string", "new_string"])
        );
        assert_eq!(schema["additionalProperties"], false);
        assert_eq!(schema["properties"].as_object().unwrap().len(), 4);
    }

    #[test]
    fn invalid_inputs_name_the_tool_and_the_reason() {
        assert_eq!(
            parse_json_input(
                "edit",
                r#"{"file_path":"a","old_string":"","new_string":"b"}"#
            ),
            Err("Invalid input for edit: `old_string` must not be empty".into())
        );
        assert_eq!(
            parse_json_input(
                "EditFile",
                r#"{"file_path":"a","old_string":"x","new_string":"x"}"#
            ),
            Err("Invalid input for EditFile: `old_string` and `new_string` must differ".into())
        );
        let unknown = parse_json_input(
            "edit",
            r#"{"file_path":"a","old_string":"x","new_string":"y","z":1}"#,
        )
        .unwrap_err();
        assert!(
            unknown.starts_with("Invalid input for edit: unknown field `z`"),
            "{unknown}"
        );
        assert_eq!(
            text_input_error("edit"),
            "Invalid input for edit: expected a JSON object input, got freeform text"
        );
    }

    #[test]
    fn replaces_a_unique_match() {
        let edited = edit_text("f.txt", b"one\ntwo\nthree\n", &input("two", "TWO", false)).unwrap();
        assert_eq!(edited.contents, "one\nTWO\nthree\n");
        assert_eq!(edited.replacements, 1);
        assert_eq!(edited_output("f.txt", 1), "Edited f.txt (1 replacement).");
        assert_eq!(edited_output("f.txt", 3), "Edited f.txt (3 replacements).");
    }

    #[test]
    fn refuses_missing_and_ambiguous_matches_and_non_utf8() {
        assert_eq!(
            edit_text("g.txt", b"alpha\n", &input("beta", "x", false)),
            Err("old_string was not found in g.txt.".into())
        );
        assert_eq!(
            edit_text("e.txt", b"dup\ndup\n", &input("dup", "x", false)),
            Err(
                "old_string occurs 2 times in e.txt; add context to make it unique or set replace_all."
                    .into()
            )
        );
        assert_eq!(
            edit_text("b.bin", &[0xff, 0xfe, b'a'], &input("a", "b", false)),
            Err("b.bin is not valid UTF-8.".into())
        );
    }

    #[test]
    fn replace_all_counts_every_occurrence() {
        let edited = edit_text("f.txt", b"dup\ndup\ndup\n", &input("dup", "x", true)).unwrap();
        assert_eq!(edited.contents, "x\nx\nx\n");
        assert_eq!(edited.replacements, 3);
    }

    #[test]
    fn keeps_crlf_lone_cr_a_missing_final_newline_and_a_bom() {
        let crlf = edit_text("f", b"one\r\ntwo\r\n", &input("one\ntwo", "1\n2", false)).unwrap();
        assert_eq!(crlf.contents, "1\r\n2\r\n");
        let cr = edit_text("f", b"one\rtwo\r", &input("two", "TWO", false)).unwrap();
        assert_eq!(cr.contents, "one\rTWO\r");
        let bare = edit_text("f", b"one\ntwo", &input("two", "TWO", false)).unwrap();
        assert_eq!(bare.contents, "one\nTWO");
        let bom = edit_text(
            "f",
            "\u{FEFF}alpha\n".as_bytes(),
            &input("alpha", "beta", false),
        )
        .unwrap();
        assert_eq!(bom.contents, "\u{FEFF}beta\n");
        // A CRLF old_string matches an LF file too: matching is on normalized text.
        let lf = edit_text("f", b"a\nb\n", &input("a\r\nb", "c", false)).unwrap();
        assert_eq!(lf.contents, "c\n");
    }

    #[test]
    fn bounds_long_output_with_a_trailer() {
        assert_eq!(bound_output("short", 10, 10), "short");
        assert_eq!(
            bound_output("a\nb\nc\n", 100, 2),
            "a\nb\n[output truncated: showing 4 of 6 bytes]"
        );
        assert_eq!(
            bound_output("héllo", 2, 0),
            "h\n[output truncated: showing 1 of 6 bytes]"
        );
    }

    #[test]
    fn lexical_escape_and_normalization() {
        assert!(!escapes_lexically("src/a.rs"));
        assert!(!escapes_lexically("a/../b"));
        assert!(!escapes_lexically(""));
        assert!(escapes_lexically("../a.rs"));
        assert!(escapes_lexically("a/../../b"));
        assert!(escapes_lexically("/etc/passwd"));
        assert_eq!(lexical_normalize("./a//b/../c.txt"), "a/c.txt");
        assert_eq!(lexical_normalize("d.txt"), "d.txt");
    }

    #[test]
    fn describes_results() {
        let described = describe_result(
            Some(input("a\nb", "c", false)),
            true,
            "Edited f.txt (2 replacements).",
        );
        assert_eq!(described.summary, "+2 −4");
        assert_eq!(
            described.diff,
            Some(Diff {
                path: "f.txt".into(),
                before: "a\nb".into(),
                after: "c".into(),
            })
        );
        let failed = describe_result(Some(input("a", "b", false)), false, "first\nsecond");
        assert_eq!(failed.summary, "first");
        assert_eq!(failed.diff, None);
        assert_eq!(describe_result(None, true, "").summary, "");
    }
}
