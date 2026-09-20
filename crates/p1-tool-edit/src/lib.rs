//! The `edit` tool: exact string replacement in one workspace file.
//!
//! Confinement, atomic replacement and observed-file tracking live in
//! `p1-workspace`. This module owns the model-facing declaration, input
//! validation, the exact-match search and line-ending preservation.

use std::io::ErrorKind;

use p1_contracts::{
    BoxFuture, DeclarationKind, Effect, Tool, ToolCall, ToolContext, ToolDeclaration, ToolIdentity,
    ToolInput, ToolOutcome, ToolStatus,
};
use p1_workspace::{Observation, ObservedFiles, Workspace, bound_output, write_atomic};
use serde::Deserialize;

pub use p1_workspace::ToolFace;

const NAME: &str = "edit";
const DESCRIPTION: &str = "Replace an exact string in an existing workspace file.\n`old_string` must match uniquely unless `replace_all` is set; it must differ from `new_string`.\nRead the file first: the edit is refused if you have never read it, or if it changed on disk since you did.\nThe file's line endings and final newline are preserved.";
const MAX_OUTPUT_BYTES: usize = 50_000;
const MAX_OUTPUT_LINES: usize = 2_000;

/// The `edit` tool. Holds one agent's workspace and observation store.
pub struct EditTool {
    workspace: Workspace,
    observed: ObservedFiles,
    declaration: ToolDeclaration,
    identity: ToolIdentity,
}

impl EditTool {
    /// Build the tool with the default (`edit`, Claude-family) face.
    pub fn new(workspace: Workspace, observed: ObservedFiles) -> Self {
        Self {
            workspace,
            observed,
            declaration: declaration(default_face()),
            identity: identity("claude"),
        }
    }

    /// Present the same implementation under another name/description and
    /// variant. The input schema and the semantics do not change.
    pub fn with_face(self, face: ToolFace, variant: &str) -> Self {
        Self {
            workspace: self.workspace,
            observed: self.observed,
            declaration: declaration(face),
            identity: identity(variant),
        }
    }
}

fn default_face() -> ToolFace {
    ToolFace::new(NAME, DESCRIPTION)
}

fn declaration(face: ToolFace) -> ToolDeclaration {
    ToolDeclaration {
        name: face.name,
        description: face.description,
        kind: DeclarationKind::Function {
            input_schema: input_schema(),
        },
    }
}

fn identity(variant: &str) -> ToolIdentity {
    ToolIdentity {
        implementation: env!("CARGO_PKG_NAME").to_string(),
        variant: variant.to_string(),
    }
}

fn input_schema() -> serde_json::Value {
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

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct EditInput {
    file_path: String,
    old_string: String,
    new_string: String,
    #[serde(default)]
    replace_all: bool,
}

impl Tool for EditTool {
    fn declaration(&self) -> &ToolDeclaration {
        &self.declaration
    }

    fn identity(&self) -> &ToolIdentity {
        &self.identity
    }

    fn effect(&self, _call: &ToolCall) -> Effect {
        Effect::WritesFiles
    }

    fn execute<'a>(
        &'a self,
        call: &'a ToolCall,
        context: ToolContext,
    ) -> BoxFuture<'a, ToolOutcome> {
        Box::pin(async move {
            // Cancellation before any work: touch nothing, not even a stat.
            if context.cancel.is_cancelled() {
                return ToolOutcome {
                    status: ToolStatus::Cancelled,
                    content: String::new(),
                };
            }
            let input = match parse_input(&self.declaration.name, call) {
                Ok(input) => input,
                Err(message) => return ToolOutcome::error(message),
            };
            let workspace = self.workspace.clone();
            let observed = self.observed.clone();
            let tool = self.declaration.name.clone();
            // All filesystem work runs on a blocking thread; the async thread
            // is never used for synchronous I/O.
            match tokio::task::spawn_blocking(move || run(&workspace, &observed, &input)).await {
                Ok(Ok(content)) => {
                    ToolOutcome::ok(bound_output(&content, MAX_OUTPUT_BYTES, MAX_OUTPUT_LINES))
                }
                Ok(Err(message)) => ToolOutcome::error(message),
                Err(error) => ToolOutcome::error(format!("{tool} failed: {error}")),
            }
        })
    }
}

fn parse_input(tool: &str, call: &ToolCall) -> Result<EditInput, String> {
    let raw = match &call.input {
        ToolInput::Json(raw) => raw,
        ToolInput::Text(_) => {
            return Err(invalid(
                tool,
                "expected a JSON object input, got freeform text",
            ));
        }
    };
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

fn invalid(tool: &str, reason: &str) -> String {
    format!("Invalid input for {tool}: {reason}")
}

fn run(
    workspace: &Workspace,
    observed: &ObservedFiles,
    input: &EditInput,
) -> Result<String, String> {
    let resolved = workspace
        .resolve(&input.file_path)
        .map_err(|error| error.to_string())?;
    let display = workspace.display(&resolved);

    // Check and write are one step for every agent sharing this gate: another
    // agent's write cannot land between the staleness check and ours.
    let _mutation = workspace.begin_mutation();
    let bytes = match std::fs::read(&resolved) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == ErrorKind::NotFound => {
            return Err(format!("{display} does not exist."));
        }
        Err(error) => return Err(format!("{display} could not be read: {error}")),
    };

    // Read-before-mutate: refuse to touch a file this agent has not seen, or
    // whose contents changed since it last saw them.
    match observed.check_unchanged(&resolved, &bytes) {
        Observation::NeverObserved => {
            return Err(format!("You must read {display} before changing it."));
        }
        Observation::ChangedSinceObserved => {
            return Err(format!(
                "{display} changed on disk since you last read it; read it again."
            ));
        }
        Observation::Unchanged => {}
    }

    let text = std::str::from_utf8(&bytes).map_err(|_| format!("{display} is not valid UTF-8."))?;
    let (body, had_bom) = strip_bom(text);
    // Matching happens on LF-normalized text; the file's own ending is
    // restored on write, so a CRLF file stays CRLF and a missing final newline
    // stays missing.
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
    let final_text = if had_bom {
        format!("\u{FEFF}{restored}")
    } else {
        restored
    };

    write_atomic(&resolved, final_text.as_bytes())
        .map_err(|error| format!("failed to write {display}: {error}"))?;
    // A successful mutation records the new contents, so consecutive edits need
    // no re-read.
    observed.record(&resolved, final_text.as_bytes());

    let plural = if replacements == 1 { "" } else { "s" };
    Ok(format!(
        "Edited {display} ({replacements} replacement{plural})."
    ))
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

#[cfg(test)]
mod tests {
    use super::EditTool;
    use p1_contracts::{
        DeclarationKind, Effect, Tool, ToolCall, ToolContext, ToolInput, ToolOutcome, ToolStatus,
    };
    use p1_workspace::{Observation, ObservedFiles, ToolFace, Workspace};
    use std::path::Path;

    fn tool(root: &Path) -> (EditTool, ObservedFiles) {
        let observed = ObservedFiles::new();
        (
            EditTool::new(Workspace::new(root).unwrap(), observed.clone()),
            observed,
        )
    }

    fn call(arguments: &str) -> ToolCall {
        ToolCall {
            call_id: "call-1".into(),
            name: "edit".into(),
            input: ToolInput::Json(arguments.to_string()),
        }
    }

    async fn execute(tool: &EditTool, arguments: &str) -> ToolOutcome {
        let call = call(arguments);
        let context = ToolContext {
            cancel: p1_contracts::CancellationToken::new(),
        };
        tool.execute(&call, context).await
    }

    fn schema(tool: &EditTool) -> serde_json::Value {
        match &tool.declaration().kind {
            DeclarationKind::Function { input_schema } => input_schema.clone(),
            other => panic!("expected a function declaration, got {other:?}"),
        }
    }

    /// A prior successful read, simulated directly on the shared registry.
    fn read(observed: &ObservedFiles, path: &Path) {
        observed.record(path, &std::fs::read(path).unwrap());
    }

    #[test]
    fn declaration_is_a_function_with_the_exact_spec_schema() {
        let dir = tempfile::tempdir().unwrap();
        let (tool, _) = tool(dir.path());

        assert_eq!(tool.declaration().name, "edit");
        let schema = schema(&tool);
        assert_eq!(schema["type"], "object");
        assert_eq!(
            schema["required"],
            serde_json::json!(["file_path", "old_string", "new_string"])
        );
        assert_eq!(schema["additionalProperties"], false);
        assert_eq!(schema["properties"]["file_path"]["type"], "string");
        assert_eq!(schema["properties"]["old_string"]["minLength"], 1);
        assert_eq!(schema["properties"]["new_string"]["type"], "string");
        assert_eq!(schema["properties"]["replace_all"]["default"], false);
        assert_eq!(schema["properties"].as_object().unwrap().len(), 4);
    }

    #[test]
    fn identity_defaults_to_the_claude_variant_and_survives_a_face_change() {
        let dir = tempfile::tempdir().unwrap();
        let (tool, _) = tool(dir.path());
        assert_eq!(tool.identity().implementation, "p1-tool-edit");
        assert_eq!(tool.identity().variant, "claude");

        let reshaped = tool.with_face(ToolFace::new("EditFile", "custom"), "gpt");
        assert_eq!(reshaped.declaration().name, "EditFile");
        assert_eq!(reshaped.identity().variant, "gpt");
    }

    #[test]
    fn effect_writes_files() {
        let dir = tempfile::tempdir().unwrap();
        let (tool, _) = tool(dir.path());
        assert_eq!(tool.effect(&call("{}")), Effect::WritesFiles);
    }

    #[tokio::test]
    async fn edit_rejects_a_file_that_was_never_read() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("d.txt");
        std::fs::write(&path, "one\ntwo\n").unwrap();
        let (tool, _) = tool(dir.path());

        let outcome = execute(
            &tool,
            r#"{"file_path": "d.txt", "old_string": "two", "new_string": "TWO"}"#,
        )
        .await;

        assert_eq!(outcome.status, ToolStatus::Error);
        assert_eq!(outcome.content, "You must read d.txt before changing it.");
        // The refused mutation left the file byte-identical.
        assert_eq!(std::fs::read(&path).unwrap(), b"one\ntwo\n");
    }

    #[tokio::test]
    async fn edit_rejects_a_file_that_changed_on_disk_since_it_was_observed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("d.txt");
        std::fs::write(&path, "one\ntwo\n").unwrap();
        let (tool, observed) = tool(dir.path());
        read(&observed, &path);
        std::fs::write(&path, "one\ntwo\nthree\n").unwrap();

        let outcome = execute(
            &tool,
            r#"{"file_path": "d.txt", "old_string": "two", "new_string": "TWO"}"#,
        )
        .await;

        assert_eq!(outcome.status, ToolStatus::Error);
        assert_eq!(
            outcome.content,
            "d.txt changed on disk since you last read it; read it again."
        );
    }

    #[tokio::test]
    async fn edit_replaces_a_unique_match_and_reports_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("d.txt");
        std::fs::write(&path, "one\ntwo\nthree\n").unwrap();
        let (tool, observed) = tool(dir.path());
        read(&observed, &path);

        let outcome = execute(
            &tool,
            r#"{"file_path": "d.txt", "old_string": "two", "new_string": "TWO"}"#,
        )
        .await;

        assert_eq!(outcome.status, ToolStatus::Ok);
        assert_eq!(outcome.content, "Edited d.txt (1 replacement).");
        assert_eq!(std::fs::read(&path).unwrap(), b"one\nTWO\nthree\n");
    }

    #[tokio::test]
    async fn edit_rejects_an_ambiguous_match_with_the_spec_message() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("e.txt");
        std::fs::write(&path, "dup\ndup\n").unwrap();
        let (tool, observed) = tool(dir.path());
        read(&observed, &path);

        let outcome = execute(
            &tool,
            r#"{"file_path": "e.txt", "old_string": "dup", "new_string": "x"}"#,
        )
        .await;

        assert_eq!(outcome.status, ToolStatus::Error);
        assert_eq!(
            outcome.content,
            "old_string occurs 2 times in e.txt; add context to make it unique or set replace_all."
        );
    }

    #[tokio::test]
    async fn edit_replace_all_replaces_every_occurrence() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f.txt");
        std::fs::write(&path, "dup\ndup\ndup\n").unwrap();
        let (tool, observed) = tool(dir.path());
        read(&observed, &path);

        let outcome = execute(
            &tool,
            r#"{"file_path": "f.txt", "old_string": "dup", "new_string": "x", "replace_all": true}"#,
        )
        .await;

        assert_eq!(outcome.content, "Edited f.txt (3 replacements).");
        assert_eq!(std::fs::read(&path).unwrap(), b"x\nx\nx\n");
    }

    #[tokio::test]
    async fn edit_replace_all_with_a_single_match_reports_one_replacement() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("h.txt");
        std::fs::write(&path, "a\nb\nc\n").unwrap();
        let (tool, observed) = tool(dir.path());
        read(&observed, &path);

        let outcome = execute(
            &tool,
            r#"{"file_path": "h.txt", "old_string": "b", "new_string": "B", "replace_all": true}"#,
        )
        .await;

        assert_eq!(outcome.content, "Edited h.txt (1 replacement).");
        assert_eq!(std::fs::read(&path).unwrap(), b"a\nB\nc\n");
    }

    #[tokio::test]
    async fn edit_reports_a_missing_old_string_with_the_spec_message() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("g.txt");
        std::fs::write(&path, "alpha\n").unwrap();
        let (tool, observed) = tool(dir.path());
        read(&observed, &path);

        let outcome = execute(
            &tool,
            r#"{"file_path": "g.txt", "old_string": "beta", "new_string": "x"}"#,
        )
        .await;

        assert_eq!(outcome.status, ToolStatus::Error);
        assert_eq!(outcome.content, "old_string was not found in g.txt.");
    }

    #[tokio::test]
    async fn a_successful_edit_records_the_new_contents() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("h.txt");
        std::fs::write(&path, "one\ntwo\n").unwrap();
        let (tool, observed) = tool(dir.path());
        read(&observed, &path);

        execute(
            &tool,
            r#"{"file_path": "h.txt", "old_string": "two", "new_string": "TWO"}"#,
        )
        .await;

        assert_eq!(
            observed.check_unchanged(&path, b"one\nTWO\n"),
            Observation::Unchanged
        );
        // A second edit therefore needs no re-read.
        let second = execute(
            &tool,
            r#"{"file_path": "h.txt", "old_string": "one", "new_string": "ONE"}"#,
        )
        .await;
        assert_eq!(second.status, ToolStatus::Ok);
        assert_eq!(std::fs::read(&path).unwrap(), b"ONE\nTWO\n");
    }

    #[tokio::test]
    async fn edit_preserves_crlf_line_endings() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("crlf.txt");
        std::fs::write(&path, b"one\r\ntwo\r\n").unwrap();
        let (tool, observed) = tool(dir.path());
        read(&observed, &path);

        let outcome = execute(
            &tool,
            r#"{"file_path": "crlf.txt", "old_string": "two", "new_string": "TWO"}"#,
        )
        .await;

        assert_eq!(outcome.status, ToolStatus::Ok);
        assert_eq!(std::fs::read(&path).unwrap(), b"one\r\nTWO\r\n");
    }

    #[tokio::test]
    async fn edit_preserves_a_missing_trailing_newline() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("no-newline.txt");
        std::fs::write(&path, b"one\ntwo").unwrap();
        let (tool, observed) = tool(dir.path());
        read(&observed, &path);

        let outcome = execute(
            &tool,
            r#"{"file_path": "no-newline.txt", "old_string": "two", "new_string": "TWO"}"#,
        )
        .await;

        assert_eq!(outcome.status, ToolStatus::Ok);
        assert_eq!(std::fs::read(&path).unwrap(), b"one\nTWO");
    }

    #[tokio::test]
    async fn edit_preserves_a_utf8_bom() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bom.txt");
        std::fs::write(&path, "\u{FEFF}alpha\n").unwrap();
        let (tool, observed) = tool(dir.path());
        read(&observed, &path);

        let outcome = execute(
            &tool,
            r#"{"file_path": "bom.txt", "old_string": "alpha", "new_string": "beta"}"#,
        )
        .await;

        assert_eq!(outcome.status, ToolStatus::Ok);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "\u{FEFF}beta\n");
    }

    #[tokio::test]
    async fn failed_edit_leaves_the_target_byte_identical_and_no_temp_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("keep.txt");
        std::fs::write(&path, "alpha\n").unwrap();
        let (tool, observed) = tool(dir.path());
        read(&observed, &path);

        let outcome = execute(
            &tool,
            r#"{"file_path": "keep.txt", "old_string": "beta", "new_string": "x"}"#,
        )
        .await;

        assert_eq!(outcome.status, ToolStatus::Error);
        assert_eq!(std::fs::read(&path).unwrap(), b"alpha\n");
        let entries: Vec<String> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(entries, vec!["keep.txt".to_string()]);
    }

    #[tokio::test]
    async fn edit_rejects_a_path_outside_the_workspace() {
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let victim = outside.path().join("victim.txt");
        std::fs::write(&victim, "original\n").unwrap();
        let (tool, _) = tool(dir.path());

        let outcome = execute(
            &tool,
            &serde_json::json!({
                "file_path": victim.to_str().unwrap(),
                "old_string": "original",
                "new_string": "changed"
            })
            .to_string(),
        )
        .await;

        assert_eq!(outcome.status, ToolStatus::Error);
        assert!(outcome.content.contains("escapes workspace"), "{outcome:?}");
        assert_eq!(std::fs::read(&victim).unwrap(), b"original\n");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn edit_rejects_a_symlink_that_points_outside_the_workspace() {
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("victim.txt"), "original\n").unwrap();
        std::os::unix::fs::symlink(outside.path(), dir.path().join("link")).unwrap();
        let (tool, _) = tool(dir.path());

        let outcome = execute(
            &tool,
            r#"{"file_path": "link/victim.txt", "old_string": "original", "new_string": "changed"}"#,
        )
        .await;

        assert_eq!(outcome.status, ToolStatus::Error);
        assert!(outcome.content.contains("escapes workspace"), "{outcome:?}");
        assert_eq!(
            std::fs::read(outside.path().join("victim.txt")).unwrap(),
            b"original\n"
        );
    }

    #[tokio::test]
    async fn file_work_runs_off_the_async_thread() {
        // The synchronous file work is moved to a blocking task, so the
        // returned future is `Send` and `execute` completes on a current-thread
        // runtime without blocking the executor.
        fn assert_send<T: Send>(_: &T) {}
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("d.txt");
        std::fs::write(&path, "original\n").unwrap();
        let (tool, observed) = tool(dir.path());
        read(&observed, &path);
        let call =
            call(r#"{"file_path": "d.txt", "old_string": "original", "new_string": "changed"}"#);
        let context = ToolContext {
            cancel: p1_contracts::CancellationToken::new(),
        };

        let future = tool.execute(&call, context);
        assert_send(&future);
        let outcome = future.await;

        assert_eq!(outcome.status, ToolStatus::Ok);
    }

    #[tokio::test]
    async fn invalid_input_reports_a_prefix_and_never_panics() {
        let dir = tempfile::tempdir().unwrap();
        let (tool, _) = tool(dir.path());
        let garbage = [
            "",
            "null",
            "[]",
            "{\"file_path\": 5}",
            "{\"file_path\":\"a.txt\",\"old_string\":\"a\",\"new_string\":\"b\",\"extra\":1}",
            "{\"file_path\":\"a.txt\",\"old_string\":\"\",\"new_string\":\"b\"}",
            "{\"file_path\":\"a.txt\",\"old_string\":\"a\",\"new_string\":\"a\"}",
            "\u{0}\u{1}{garbage",
        ];
        for arguments in garbage {
            let outcome = execute(&tool, arguments).await;
            assert_eq!(outcome.status, ToolStatus::Error, "input: {arguments:?}");
            assert!(
                outcome.content.starts_with("Invalid input for edit: "),
                "input: {arguments:?} -> {outcome:?}"
            );
        }
    }

    #[tokio::test]
    async fn text_input_is_invalid_input() {
        let dir = tempfile::tempdir().unwrap();
        let (tool, _) = tool(dir.path());
        let call = ToolCall {
            call_id: "call-1".into(),
            name: "edit".into(),
            input: ToolInput::Text("file_path=a.txt".into()),
        };
        let cancel = p1_contracts::CancellationToken::new();
        let outcome = tool.execute(&call, ToolContext { cancel }).await;

        assert_eq!(outcome.status, ToolStatus::Error);
        assert!(outcome.content.starts_with("Invalid input for edit: "));
    }

    #[tokio::test]
    async fn execute_returns_cancelled_without_touching_the_filesystem() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("d.txt");
        std::fs::write(&path, "original\n").unwrap();
        let (tool, observed) = tool(dir.path());
        read(&observed, &path);
        let call =
            call(r#"{"file_path": "d.txt", "old_string": "original", "new_string": "changed"}"#);
        let cancel = p1_contracts::CancellationToken::new();
        cancel.cancel();

        let outcome = tool.execute(&call, ToolContext { cancel }).await;

        assert_eq!(outcome.status, ToolStatus::Cancelled);
        assert_eq!(std::fs::read(&path).unwrap(), b"original\n");
    }
}
