//! The target-independent logic of the `write` tool: input parsing and validation, the
//! call and result descriptions, the model-facing texts, and the whole `execute` flow over
//! an abstract [`Host`].
//!
//! Decision S0-R3 (`docs/design/modules/package.md`): the native `p1-tool-write` and the
//! component `modules/p1-module-write/` both call this crate, so the texts a model sees are
//! produced by one piece of code whichever of the two runs. It is pure computation: no
//! filesystem, thread, clock or environment. Everything that touches a file goes through
//! the [`Host`] the caller supplies (the component's imported `workspace`, `snapshot` and
//! `workspace-mutation` interfaces).

mod execute;
pub mod wire;

use serde::Deserialize;

pub use execute::{
    Entry, EntryKind, FsError, Host, Mutation, Observation, Outcome, READ_WINDOW, Status, execute,
};
pub use wire::{
    Call, InputKind, ToolResult, call_description_json, outcome_json, parse_call,
    parse_tool_result, result_description_json,
};

/// The model-facing name of the tool.
pub const NAME: &str = "write";

/// What the model is told the tool does.
pub const DESCRIPTION: &str = "Create or replace a workspace file atomically, creating missing parent directories.\nOverwriting an existing file requires that you read its current contents first.\nPrefer `edit` for small changes: `write` replaces the whole file.";

/// The most bytes of output the model is shown.
pub const MAX_OUTPUT_BYTES: usize = 50_000;

/// The most lines of output the model is shown.
pub const MAX_OUTPUT_LINES: usize = 2_000;

/// The verb of every call description (ADR-0057): a write is an edit of one file.
pub const VERB: &str = "edit";

/// The input JSON Schema of the declaration.
pub fn input_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "file_path": {
                "type": "string",
                "description": "File path, relative to the workspace root or absolute inside it."
            },
            "content": {
                "type": "string",
                "description": "Complete file contents; replaces any existing file."
            }
        },
        "required": ["file_path", "content"],
        "additionalProperties": false
    })
}

/// A validated `write` input.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WriteInput {
    pub file_path: String,
    pub content: String,
}

/// A call's raw input as the model sent it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RawInput<'a> {
    /// JSON text, the input a function tool receives.
    Json(&'a str),
    /// Freeform text, which this function tool refuses.
    Text(&'a str),
}

/// Parse and validate `input`; the error is the model-facing text naming `tool`.
pub fn parse_input(tool: &str, input: RawInput<'_>) -> Result<WriteInput, String> {
    let raw = match input {
        RawInput::Json(raw) => raw,
        RawInput::Text(_) => {
            return Err(invalid(
                tool,
                "expected a JSON object input, got freeform text",
            ));
        }
    };
    serde_json::from_str(raw).map_err(|error| invalid(tool, &error.to_string()))
}

fn invalid(tool: &str, reason: &str) -> String {
    format!("Invalid input for {tool}: {reason}")
}

/// The preview of the change a call makes: a write replaces the whole file, so the old
/// side is empty.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EditPreview {
    pub path: String,
    pub old: String,
    pub new: String,
}

/// What a call is about (ADR-0057); its verb is always [`VERB`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Description {
    pub target: Option<String>,
    pub edit: Option<EditPreview>,
    pub destructive: bool,
}

/// Describe a call from its own parsed input. `escapes` says whether a path leaves the
/// workspace: the native tool resolves it, a component decides it lexically
/// ([`lexically_confined`]) because its `describe` runs with no capability.
pub fn describe(input: RawInput<'_>, escapes: impl FnOnce(&str) -> bool) -> Description {
    let parsed = parse_input(NAME, input).ok();
    let destructive = parsed
        .as_ref()
        .is_some_and(|input| escapes(&input.file_path));
    Description {
        target: parsed.as_ref().map(|input| input.file_path.clone()),
        edit: parsed.map(|input| EditPreview {
            path: input.file_path,
            old: String::new(),
            new: input.content,
        }),
        destructive,
    }
}

/// The diff a successful write shows: from nothing to the new contents.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResultDiff {
    pub path: String,
    pub before: String,
    pub after: String,
}

/// What a result is about: a one-line summary and, for a successful write, its diff.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResultSummary {
    pub summary: String,
    pub diff: Option<ResultDiff>,
}

/// Describe the result of a call whose input is `input`; `ok` is whether its status is
/// `ok` and `content` is exactly what the model was shown.
pub fn describe_result(input: RawInput<'_>, ok: bool, content: &str) -> ResultSummary {
    if !ok {
        return plain_result(content);
    }
    let Ok(input) = parse_input(NAME, input) else {
        return plain_result(content);
    };
    let lines = input.content.lines().count();
    let summary = parenthesized_count(content, "bytes").map_or_else(
        || format!("{lines} lines"),
        |bytes| format!("{lines} lines · {:.1} kB", bytes as f64 / 1000.0),
    );
    ResultSummary {
        summary,
        diff: Some(ResultDiff {
            path: input.file_path,
            before: String::new(),
            after: input.content,
        }),
    }
}

/// A result shown as its first line, with no detail.
pub fn plain_result(content: &str) -> ResultSummary {
    ResultSummary {
        summary: content.lines().next().unwrap_or_default().to_string(),
        diff: None,
    }
}

/// The count in the last `(<digits> <unit>` of `content`, if there is one.
fn parenthesized_count(content: &str, unit: &str) -> Option<usize> {
    let rest = &content[content.rfind('(')? + 1..];
    let digits_end = rest.find(|c: char| !c.is_ascii_digit())?;
    let count = rest[..digits_end].parse().ok()?;
    rest[digits_end..]
        .trim_start()
        .starts_with(unit)
        .then_some(count)
}

/// Whether `path` stays inside the workspace by its spelling alone: relative, and no `..`
/// climbs above its start. An absolute path is not provably inside without knowing the
/// root, and a symlink is invisible here, so both are the capability's to enforce at
/// `execute`; this answer is only the restricted path's worst-case guess.
pub fn lexically_confined(path: &str) -> bool {
    relative_display(path).is_some()
}

/// The root-relative form of a relative `request`, normalized lexically as the native
/// `Workspace::resolve` and `Workspace::display` do, or `None` when the request is
/// absolute or a `..` climbs above its start (only the host knows where that lands).
pub(crate) fn relative_display(request: &str) -> Option<String> {
    if request.starts_with('/') {
        return None;
    }
    let mut parts: Vec<&str> = Vec::new();
    for part in request.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop()?;
            }
            name => parts.push(name),
        }
    }
    Some(display_of(&parts))
}

/// `parts` joined as the native display writes a path: `/` separators, and a backslash
/// shown as `/` too (the native display replaces it).
pub(crate) fn display_of(parts: &[&str]) -> String {
    parts.join("/").replace('\\', "/")
}

/// The refusal for an existing target this agent never read.
pub fn never_observed(display: &str) -> String {
    format!("You must read {display} before changing it.")
}

/// The refusal for an existing target that changed since this agent read it.
pub fn changed_since_observed(display: &str) -> String {
    format!("{display} changed on disk since you last read it; read it again.")
}

/// The failure to read an existing target's current contents.
pub fn could_not_be_read(display: &str, reason: &str) -> String {
    format!("{display} could not be read: {reason}")
}

/// The failure of the write itself.
pub fn failed_to_write(display: &str, reason: &str) -> String {
    format!("failed to write {display}: {reason}")
}

/// The success text: the file and the bytes written.
pub fn wrote(display: &str, bytes: usize) -> String {
    format!("Wrote {display} ({bytes} bytes).")
}

/// `text` bounded to what the model is shown ([`MAX_OUTPUT_BYTES`], [`MAX_OUTPUT_LINES`]).
pub fn bounded(text: &str) -> String {
    bound_output(text, MAX_OUTPUT_BYTES, MAX_OUTPUT_LINES)
}

/// Bound text shown to the model to `max_bytes` bytes and `max_lines` lines, cutting on a
/// char boundary. When anything was cut, a trailer reports the bytes actually shown
/// against the original total.
///
/// The same algorithm and trailer as `p1_workspace::bound_output`, copied here because
/// this crate may not depend on a host crate (S0-R3); the tests below pin the texts.
pub fn bound_output(text: &str, max_bytes: usize, max_lines: usize) -> String {
    let total_bytes = text.len();
    let mut end = total_bytes;
    let mut truncated = false;

    // Keep the first `max_lines` lines. A trailing newline does not create an extra line,
    // matching how the file tools count lines.
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
    // Put the trailer on its own line without emitting a blank line when the kept slice
    // already ends with a newline (a line-cap cut does).
    if !out.ends_with('\n') {
        out.push('\n');
    }
    out.push_str(&format!(
        "[output truncated: showing {end} of {total_bytes} bytes]"
    ));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_guest_path_starts_no_thread() {
        // The component runs on the host's one executor and has no thread; a blocking
        // hand-off or a thread on its path would be a native habit that cannot run there.
        // Spelled in pieces so this test does not match itself.
        let forbidden = [
            concat!("spawn", "_blocking"),
            concat!("std::", "thread"),
            concat!("thread::", "spawn"),
            concat!("tok", "io"),
        ];
        let sources = [
            ("logic/src/lib.rs", include_str!("lib.rs")),
            ("logic/src/execute.rs", include_str!("execute.rs")),
            ("logic/src/wire.rs", include_str!("wire.rs")),
            (
                "modules/p1-module-write/src/lib.rs",
                include_str!("../../../../modules/p1-module-write/src/lib.rs"),
            ),
        ];
        for (name, source) in sources {
            for word in forbidden {
                assert!(!source.contains(word), "{name} contains {word}");
            }
        }
    }

    #[test]
    fn the_schema_requires_exactly_the_two_fields() {
        let schema = input_schema();
        assert_eq!(schema["type"], "object");
        assert_eq!(
            schema["required"],
            serde_json::json!(["file_path", "content"])
        );
        assert_eq!(schema["additionalProperties"], false);
        assert_eq!(schema["properties"].as_object().unwrap().len(), 2);
    }

    #[test]
    fn parse_accepts_the_two_fields_and_names_the_tool_on_refusal() {
        assert_eq!(
            parse_input(NAME, RawInput::Json(r#"{"file_path":"a","content":"b"}"#)),
            Ok(WriteInput {
                file_path: "a".into(),
                content: "b".into()
            })
        );
        assert_eq!(
            parse_input("WriteFile", RawInput::Text("x")),
            Err(
                "Invalid input for WriteFile: expected a JSON object input, got freeform text"
                    .into()
            )
        );
        assert_eq!(
            parse_input(NAME, RawInput::Json("")),
            Err("Invalid input for write: EOF while parsing a value at line 1 column 0".into())
        );
        let extra = parse_input(
            NAME,
            RawInput::Json(r#"{"file_path":"a","content":"x","extra":1}"#),
        )
        .unwrap_err();
        assert!(
            extra.starts_with("Invalid input for write: unknown field `extra`"),
            "{extra}"
        );
    }

    #[test]
    fn describe_previews_the_whole_file_and_asks_about_escapes() {
        let input = RawInput::Json(r#"{"file_path":"out.txt","content":"hi"}"#);
        let described = describe(input, |_| false);
        assert_eq!(described.target.as_deref(), Some("out.txt"));
        assert_eq!(
            described.edit,
            Some(EditPreview {
                path: "out.txt".into(),
                old: String::new(),
                new: "hi".into()
            })
        );
        assert!(!described.destructive);
        assert!(describe(input, |path| path == "out.txt").destructive);
        // Unparsable input names nothing and is not destructive, as natively.
        let unparsed = describe(RawInput::Json("{"), |_| true);
        assert_eq!(
            unparsed,
            Description {
                target: None,
                edit: None,
                destructive: false
            }
        );
    }

    #[test]
    fn the_restricted_describe_answers_lexically_as_the_component_contract() {
        // `modules/p1-module-write`'s `describe` runs on the restricted path with no
        // capability (`logic::lexically_confined`), so it can only answer lexically: an
        // absolute path is flagged even when the root would hold it, and a relative path
        // through a symlink that escapes is not flagged. That worst-case answer diverges
        // from the native resolve-based one and is the component's accepted contract; it
        // is pinned here so it cannot drift silently, and U-write.2/U-desc checks it
        // against the production loader.
        let absolute_inside = RawInput::Json(r#"{"file_path":"/ws/a.txt","content":"x"}"#);
        assert!(describe(absolute_inside, |path| !lexically_confined(path)).destructive);

        let through_an_escaping_link =
            RawInput::Json(r#"{"file_path":"link/a.txt","content":"x"}"#);
        assert!(!describe(through_an_escaping_link, |path| !lexically_confined(path)).destructive);
    }

    #[test]
    fn describe_result_counts_lines_and_reported_bytes() {
        let input = RawInput::Json(r#"{"file_path":"a.txt","content":"x\ny\n"}"#);
        let described = describe_result(input, true, "Wrote a.txt (4000 bytes).");
        assert_eq!(described.summary, "2 lines · 4.0 kB");
        assert_eq!(
            described.diff,
            Some(ResultDiff {
                path: "a.txt".into(),
                before: String::new(),
                after: "x\ny\n".into()
            })
        );
        assert_eq!(describe_result(input, true, "no count").summary, "2 lines");
        assert_eq!(
            describe_result(input, false, "first\nsecond"),
            ResultSummary {
                summary: "first".into(),
                diff: None
            }
        );
        assert_eq!(
            describe_result(RawInput::Text("x"), true, "Wrote a (1 bytes).").summary,
            "Wrote a (1 bytes)."
        );
    }

    #[test]
    fn lexical_confinement_refuses_climbing_and_absolute_paths() {
        assert!(lexically_confined("a/b.txt"));
        assert!(lexically_confined("./a/../b.txt"));
        assert!(lexically_confined(""));
        assert!(!lexically_confined("../outside.txt"));
        assert!(!lexically_confined("a/../../x"));
        assert!(!lexically_confined("/tmp/ws/a.txt"));
    }

    #[test]
    fn relative_display_normalizes_as_the_native_display() {
        assert_eq!(
            relative_display("a//b/./c.txt").as_deref(),
            Some("a/b/c.txt")
        );
        assert_eq!(relative_display("a/../b.txt").as_deref(), Some("b.txt"));
        assert_eq!(relative_display("dir/").as_deref(), Some("dir"));
        assert_eq!(relative_display("a\\b.txt").as_deref(), Some("a/b.txt"));
        assert_eq!(relative_display("../x"), None);
    }

    #[test]
    fn messages_are_the_native_texts() {
        assert_eq!(
            never_observed("out.txt"),
            "You must read out.txt before changing it."
        );
        assert_eq!(
            changed_since_observed("out.txt"),
            "out.txt changed on disk since you last read it; read it again."
        );
        assert_eq!(
            could_not_be_read("d", "Is a directory (os error 21)"),
            "d could not be read: Is a directory (os error 21)"
        );
        assert_eq!(failed_to_write("a", "boom"), "failed to write a: boom");
        assert_eq!(wrote("a/b.txt", 5), "Wrote a/b.txt (5 bytes).");
    }

    #[test]
    fn bound_output_cuts_lines_and_bytes_with_a_trailer() {
        assert_eq!(bound_output("short", 100, 10), "short");
        assert_eq!(
            bound_output("a\nb\nc\n", 100, 2),
            "a\nb\n[output truncated: showing 4 of 6 bytes]"
        );
        assert_eq!(
            bound_output("abcdef", 3, 10),
            "abc\n[output truncated: showing 3 of 6 bytes]"
        );
        // A cut never splits a character.
        assert_eq!(
            bound_output("aé", 2, 10),
            "a\n[output truncated: showing 1 of 3 bytes]"
        );
        assert_eq!(bounded("Wrote a (1 bytes)."), "Wrote a (1 bytes).");
    }
}
