//! The target-independent logic of the `apply_patch` tool: the declaration's data, input
//! parsing and validation, the V4A parser and the in-memory plan of every change, the call
//! and result descriptions, every model-facing text, and the component's whole `execute`
//! flow over an abstract [`Host`].
//!
//! Decision S0-R3 (`docs/design/modules/package.md`): the native `p1-tool-patch` and the
//! component `modules/p1-module-patch/` both call this crate, so the texts a model sees are
//! produced by one piece of code whichever of the two runs. It is pure computation: no
//! filesystem, thread, clock or environment. Everything that touches a file goes through a
//! [`Files`] view (the native tool's resolved paths, or the component's imported
//! `workspace`) and, for the component, the [`Host`]'s `workspace-mutation`.

mod execute;
mod patch;
pub mod wire;

use serde::Deserialize;

pub use execute::{
    Entry, EntryKind, FsError, Host, Mutation, Outcome, READ_WINDOW, Status, execute,
};
pub use patch::{
    Change, Files, Hunk, Op, PatchFailure, UpdateGroup, coalesce, hunk_paths, parse_patch,
    patch_is_destructive, plan, success_output,
};
pub use wire::{
    Call, InputKind, ToolResult, call_description_json, outcome_json, parse_call,
    parse_tool_result, result_description_json,
};

/// The model-facing name of the tool's default (GPT-family, freeform) face.
pub const NAME: &str = "apply_patch";

/// What the model is told the tool does.
pub const DESCRIPTION: &str = "Apply a V4A patch to files in the workspace.\nThe patch is validated completely before anything is written; if any hunk fails to match, nothing changes and the error names the file and hunk.\nUse `*** Add File:`, `*** Delete File:` and `*** Update File:` hunks inside `*** Begin Patch` / `*** End Patch`.";

/// The most bytes of output the model is shown.
pub const MAX_OUTPUT_BYTES: usize = 50_000;

/// The most lines of output the model is shown.
pub const MAX_OUTPUT_LINES: usize = 2_000;

/// The verb of every call description (ADR-0057): a patch edits files.
pub const VERB: &str = "edit";

/// The syntax of [`PATCH_GRAMMAR`].
pub const GRAMMAR_SYNTAX: &str = "lark";

/// The published Codex V4A grammar, advertised with the freeform declaration.
pub const PATCH_GRAMMAR: &str = r#"start: begin_patch hunk+ end_patch
begin_patch: "*** Begin Patch" LF
end_patch: "*** End Patch" LF?
hunk: add_hunk | delete_hunk | update_hunk
add_hunk: "*** Add File: " filename LF add_line+
delete_hunk: "*** Delete File: " filename LF
update_hunk: "*** Update File: " filename LF change_move? change?
filename: /(.+)/
add_line: "+" /(.*)/ LF -> line
change_move: "*** Move to: " filename LF
change: (change_context | change_line)+ eof_line?
change_context: ("@@" | "@@ " /(.+)/) LF
change_line: ("+" | "-" | " ") /(.*)/ LF
eof_line: "*** End of File" LF
%import common.LF
"#;

/// The input JSON Schema of the function declaration form, for routes without freeform
/// tools: `{"patch": string}`.
pub fn function_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "patch": { "type": "string" }
        },
        "required": ["patch"],
        "additionalProperties": false
    })
}

/// A call's raw input as the model sent it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RawInput<'a> {
    /// JSON text, the input a function tool receives.
    Json(&'a str),
    /// Freeform text, the input a freeform tool receives.
    Text(&'a str),
}

/// Extract the patch text from a call's input, according to the declaration form
/// (`freeform`); the error is the model-facing text naming `tool`.
pub fn patch_text(tool: &str, freeform: bool, input: RawInput<'_>) -> Result<String, String> {
    if freeform {
        match input {
            RawInput::Text(raw) => Ok(raw.to_string()),
            RawInput::Json(_) => Err(invalid(
                tool,
                "expected freeform text input, got a JSON object",
            )),
        }
    } else {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct FunctionInput {
            patch: String,
        }
        match input {
            RawInput::Json(raw) => {
                let input: FunctionInput =
                    serde_json::from_str(raw).map_err(|error| invalid(tool, &error.to_string()))?;
                Ok(input.patch)
            }
            RawInput::Text(_) => Err(invalid(
                tool,
                "expected a JSON object input, got freeform text",
            )),
        }
    }
}

fn invalid(tool: &str, reason: &str) -> String {
    format!("Invalid input for {tool}: {reason}")
}

/// What a call is about (ADR-0057); its verb is always [`VERB`] and it carries no edit
/// preview.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Description {
    pub target: Option<String>,
    pub destructive: bool,
}

/// Describe a call by parsing its patch the same way `execute` does: the first file it
/// touches, or the count for a multi-file patch. `escapes` says whether a path leaves the
/// workspace: the native tool resolves it, a component decides it lexically
/// ([`lexically_confined`]) because its `describe` runs with no capability.
pub fn describe(
    freeform: bool,
    input: RawInput<'_>,
    escapes: impl FnMut(&str) -> bool,
) -> Description {
    // The tool name only words an input error, which a description never shows.
    let hunks = patch_text(NAME, freeform, input)
        .ok()
        .and_then(|text| parse_patch(&text).ok());
    let target = match hunks.as_deref().map(hunk_paths).as_deref() {
        None | Some([]) => None,
        Some([only]) => Some(only.clone()),
        Some(paths) => Some(format!("{} files", paths.len())),
    };
    Description {
        target,
        destructive: hunks
            .as_deref()
            .is_some_and(|hunks| patch_is_destructive(hunks, escapes)),
    }
}

/// What a result is about: a one-line summary and, for a successful patch, one line per
/// file (`<path>\t<facts>`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResultSummary {
    pub summary: String,
    pub files: Option<Vec<String>>,
}

/// Describe the result of a call whose input is `input`; `ok` is whether its status is
/// `ok` and `content` is exactly what the model was shown.
pub fn describe_result(
    freeform: bool,
    input: RawInput<'_>,
    ok: bool,
    content: &str,
) -> ResultSummary {
    if !ok {
        return plain_result(content);
    }
    let Ok(text) = patch_text(NAME, freeform, input) else {
        return plain_result(content);
    };
    let files = describe_patch_files(&text);
    let added: usize = files.iter().map(|file| file.added).sum();
    let removed = files
        .iter()
        .map(|file| file.removed)
        .try_fold(0usize, |total, count| count.map(|count| total + count));
    let summary = match removed {
        Some(removed) => format!("+{added} −{removed} · {} files", files.len()),
        None => format!("+{added} · {} files", files.len()),
    };
    ResultSummary {
        summary,
        files: Some(
            files
                .into_iter()
                .map(|file| {
                    let facts = if file.kind == 'D' {
                        "D".to_string()
                    } else {
                        match file.removed {
                            Some(removed) => format!("+{} −{removed}", file.added),
                            None => format!("+{}", file.added),
                        }
                    };
                    format!("{}\t{facts}", file.path)
                })
                .collect(),
        ),
    }
}

/// A result shown as its first line, with no detail.
pub fn plain_result(content: &str) -> ResultSummary {
    ResultSummary {
        summary: content.lines().next().unwrap_or_default().to_string(),
        files: None,
    }
}

struct DescribedPatchFile {
    path: String,
    kind: char,
    added: usize,
    removed: Option<usize>,
}

fn describe_patch_files(text: &str) -> Vec<DescribedPatchFile> {
    let mut files: Vec<DescribedPatchFile> = Vec::new();
    for line in patch::strip_wrapper(text).lines() {
        if let Some(path) = line.strip_prefix("*** Add File: ") {
            files.push(DescribedPatchFile {
                path: path.to_string(),
                kind: 'A',
                added: 0,
                removed: Some(0),
            });
        } else if let Some(path) = line.strip_prefix("*** Delete File: ") {
            files.push(DescribedPatchFile {
                path: path.to_string(),
                kind: 'D',
                added: 0,
                removed: None,
            });
        } else if let Some(path) = line.strip_prefix("*** Update File: ") {
            files.push(DescribedPatchFile {
                path: path.to_string(),
                kind: 'M',
                added: 0,
                removed: Some(0),
            });
        } else if line.starts_with("*** Move to: ")
            || line.starts_with("*** End of File")
            || line.starts_with("@@")
            || line.starts_with("*** Begin Patch")
            || line.starts_with("*** End Patch")
        {
            continue;
        } else if let Some(file) = files.last_mut() {
            match (file.kind, line.chars().next()) {
                ('A', _) => file.added += 1,
                ('M', Some('+')) => file.added += 1,
                ('M', Some('-')) => *file.removed.get_or_insert(0) += 1,
                _ => {}
            }
        }
    }
    files
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

/// The refusal to add, or move onto, a path that is taken.
pub fn already_exists(display: &str) -> String {
    format!("{display} already exists.")
}

/// The refusal to update or delete a path where nothing is.
pub fn does_not_exist(display: &str) -> String {
    format!("{display} does not exist.")
}

/// The refusal to patch something that is not a regular file.
pub fn not_a_regular_file(display: &str) -> String {
    format!("{display} is not a regular file.")
}

/// The refusal to patch a file that is not UTF-8 text.
pub fn not_valid_utf8(display: &str) -> String {
    format!("{display} is not valid UTF-8.")
}

/// The failure to read a file the patch needs.
pub fn could_not_be_read(display: &str, reason: &str) -> String {
    format!("{display} could not be read: {reason}")
}

/// The failure of a write.
pub fn failed_to_write(display: &str, reason: &str) -> String {
    format!("failed to write {display}: {reason}")
}

/// The failure of a deletion.
pub fn failed_to_delete(display: &str, reason: &str) -> String {
    format!("failed to delete {display}: {reason}")
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
/// this crate may not depend on a host crate (S0-R3); `tests/guest_parity.rs` of
/// `p1-tool-patch` compares the two.
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
            ("logic/src/patch.rs", include_str!("patch.rs")),
            ("logic/src/execute.rs", include_str!("execute.rs")),
            ("logic/src/wire.rs", include_str!("wire.rs")),
            (
                "modules/p1-module-patch/src/lib.rs",
                include_str!("../../../../modules/p1-module-patch/src/lib.rs"),
            ),
        ];
        for (name, source) in sources {
            for word in forbidden {
                assert!(!source.contains(word), "{name} contains {word}");
            }
        }
    }

    #[test]
    fn the_function_schema_takes_exactly_the_patch() {
        assert_eq!(
            function_schema(),
            serde_json::json!({
                "type": "object",
                "properties": { "patch": { "type": "string" } },
                "required": ["patch"],
                "additionalProperties": false
            })
        );
    }

    #[test]
    fn patch_text_follows_the_declaration_form_and_names_the_tool() {
        assert_eq!(
            patch_text(NAME, true, RawInput::Text("p")),
            Ok("p".to_string())
        );
        assert_eq!(
            patch_text(NAME, true, RawInput::Json("{}")),
            Err(
                "Invalid input for apply_patch: expected freeform text input, got a JSON object"
                    .into()
            )
        );
        assert_eq!(
            patch_text("Patch", false, RawInput::Json(r#"{"patch":"p"}"#)),
            Ok("p".to_string())
        );
        assert_eq!(
            patch_text("Patch", false, RawInput::Text("p")),
            Err("Invalid input for Patch: expected a JSON object input, got freeform text".into())
        );
        let extra =
            patch_text(NAME, false, RawInput::Json(r#"{"patch":"x","extra":1}"#)).unwrap_err();
        assert!(
            extra.starts_with("Invalid input for apply_patch: unknown field `extra`"),
            "{extra}"
        );
    }

    #[test]
    fn describe_names_the_file_or_the_count_and_asks_about_escapes() {
        let one = describe(
            true,
            RawInput::Text(
                "*** Begin Patch\n*** Update File: src/a.rs\n@@\n-a\n+b\n*** End Patch\n",
            ),
            |_| false,
        );
        assert_eq!(
            one,
            Description {
                target: Some("src/a.rs".into()),
                destructive: false
            }
        );
        let two = describe(
            true,
            RawInput::Text(
                "*** Begin Patch\n*** Add File: a\n+x\n*** Add File: b\n+y\n*** End Patch\n",
            ),
            |path| path == "b",
        );
        assert_eq!(
            two,
            Description {
                target: Some("2 files".into()),
                destructive: true
            }
        );
        let function = describe(
            false,
            RawInput::Json(
                r#"{"patch":"*** Begin Patch\n*** Delete File: old.rs\n*** End Patch\n"}"#,
            ),
            |_| false,
        );
        assert_eq!(function.target.as_deref(), Some("old.rs"));
        // An unparsable patch, or the wrong input kind, names nothing and is not destructive.
        for (freeform, input) in [
            (true, RawInput::Text("garbage")),
            (true, RawInput::Json("{}")),
            (false, RawInput::Text("*** Begin Patch")),
        ] {
            assert_eq!(
                describe(freeform, input, |_| true),
                Description {
                    target: None,
                    destructive: false
                }
            );
        }
    }

    #[test]
    fn the_restricted_describe_answers_lexically_as_the_component_contract() {
        // `modules/p1-module-patch`'s `describe` runs on the restricted path with no
        // capability, so it can only answer lexically: an absolute path is flagged even when
        // the root would hold it, and a relative path through a symlink that escapes is not.
        // That worst-case answer diverges from the native resolve-based one and is the
        // component's accepted contract, pinned here; U-patch.2/U-desc check it on the loader.
        let absolute_inside =
            RawInput::Text("*** Begin Patch\n*** Add File: /ws/a.txt\n+x\n*** End Patch\n");
        assert!(describe(true, absolute_inside, |path| !lexically_confined(path)).destructive);
        let through_a_link =
            RawInput::Text("*** Begin Patch\n*** Add File: link/a.txt\n+x\n*** End Patch\n");
        assert!(!describe(true, through_a_link, |path| !lexically_confined(path)).destructive);
    }

    #[test]
    fn describe_result_counts_lines_per_file() {
        let patch = "*** Begin Patch\n*** Update File: path/to/file.rs\n@@ fn existing_function\n unchanged context line\n-removed line\n+added line\n*** Add File: path/to/new_file.rs\n+first line\n*** Delete File: path/to/old_file.rs\n*** End Patch\n";
        let described = describe_result(true, RawInput::Text(patch), true, "M path/to/file.rs");
        assert_eq!(
            described,
            ResultSummary {
                summary: "+2 · 3 files".into(),
                files: Some(vec![
                    "path/to/file.rs\t+1 −1".into(),
                    "path/to/new_file.rs\t+1 −0".into(),
                    "path/to/old_file.rs\tD".into(),
                ]),
            }
        );
        let modify_only = describe_result(
            true,
            RawInput::Text("*** Begin Patch\n*** Update File: f\n-a\n-b\n+c\n*** End Patch\n"),
            true,
            "M f",
        );
        assert_eq!(modify_only.summary, "+1 −2 · 1 files");
        assert_eq!(
            describe_result(true, RawInput::Text(patch), false, "first\nsecond"),
            ResultSummary {
                summary: "first".into(),
                files: None
            }
        );
        assert_eq!(
            describe_result(true, RawInput::Json("{}"), true, "M f").files,
            None
        );
    }

    #[test]
    fn lexical_confinement_refuses_climbing_and_absolute_paths() {
        assert!(lexically_confined("a/b.txt"));
        assert!(lexically_confined("./a/../b.txt"));
        assert!(!lexically_confined("../outside.txt"));
        assert!(!lexically_confined("a/../../x"));
        assert!(!lexically_confined("/tmp/ws/a.txt"));
        assert_eq!(
            relative_display("a//b/./c.txt").as_deref(),
            Some("a/b/c.txt")
        );
        assert_eq!(relative_display("a\\b.txt").as_deref(), Some("a/b.txt"));
    }

    #[test]
    fn messages_are_the_native_texts() {
        assert_eq!(already_exists("a"), "a already exists.");
        assert_eq!(does_not_exist("a"), "a does not exist.");
        assert_eq!(not_a_regular_file("a"), "a is not a regular file.");
        assert_eq!(not_valid_utf8("a"), "a is not valid UTF-8.");
        assert_eq!(
            could_not_be_read("a", "Not a directory (os error 20)"),
            "a could not be read: Not a directory (os error 20)"
        );
        assert_eq!(failed_to_write("a", "boom"), "failed to write a: boom");
        assert_eq!(failed_to_delete("a", "boom"), "failed to delete a: boom");
    }

    #[test]
    fn bound_output_cuts_lines_and_bytes_with_a_trailer() {
        assert_eq!(bound_output("short", 100, 10), "short");
        assert_eq!(
            bound_output("a\nb\nc\n", 100, 2),
            "a\nb\n[output truncated: showing 4 of 6 bytes]"
        );
        assert_eq!(
            bound_output("aé", 2, 10),
            "a\n[output truncated: showing 1 of 3 bytes]"
        );
    }
}
