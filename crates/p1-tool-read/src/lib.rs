//! The `read` tool: line-numbered reads of one workspace file.
//!
//! Confinement, atomic writes and observed-file tracking live in `p1-workspace`.
//! This module owns the model-facing declaration, input validation, rendering
//! and the read-before-mutate observation.

use std::io::ErrorKind;

use p1_contracts::{
    BoxFuture, DeclarationKind, Effect, Tool, ToolCall, ToolContext, ToolDeclaration, ToolIdentity,
    ToolInput, ToolOutcome, ToolStatus,
};
use p1_workspace::{ObservedFiles, Workspace, bound_output};
use serde::Deserialize;

pub use p1_workspace::ToolFace;

const NAME: &str = "read";
const DESCRIPTION: &str = "Read a UTF-8 text file from the workspace, with numbered lines.\nUse `offset` and `limit` to page through a long file; the last line gives the next offset.\nRead a file before you edit or overwrite it: a mutation is refused until you have seen its current contents.";
const DEFAULT_OFFSET: i64 = 1;
const DEFAULT_LIMIT: i64 = 2_000;
const MAX_OUTPUT_BYTES: usize = 50_000;
const MAX_OUTPUT_LINES: usize = 2_000;
/// A NUL anywhere in the first 8 KiB marks the file as binary.
const BINARY_SNIFF_BYTES: usize = 8 * 1024;

/// The `read` tool. Holds one agent's workspace and observation store.
pub struct ReadTool {
    workspace: Workspace,
    observed: ObservedFiles,
    declaration: ToolDeclaration,
    identity: ToolIdentity,
}

impl ReadTool {
    /// Build the tool with the default (`read`, Claude-family) face.
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
            "path": {
                "type": "string",
                "description": "File path, relative to the workspace root or absolute inside it."
            },
            "offset": {
                "type": "integer",
                "minimum": 1,
                "default": 1,
                "description": "First line to return (1-indexed)."
            },
            "limit": {
                "type": "integer",
                "minimum": 1,
                "default": 2000,
                "description": "Maximum number of lines to return."
            }
        },
        "required": ["path"],
        "additionalProperties": false
    })
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReadInput {
    path: String,
    #[serde(default)]
    offset: Option<i64>,
    #[serde(default)]
    limit: Option<i64>,
}

impl Tool for ReadTool {
    fn declaration(&self) -> &ToolDeclaration {
        &self.declaration
    }

    fn identity(&self) -> &ToolIdentity {
        &self.identity
    }

    fn effect(&self, _call: &ToolCall) -> Effect {
        Effect::ReadOnly
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
                // `run` bounds the window itself so the continuation trailer survives.
                Ok(Ok(content)) => ToolOutcome::ok(content),
                Ok(Err(message)) => ToolOutcome::error(message),
                Err(error) => ToolOutcome::error(format!("{tool} failed: {error}")),
            }
        })
    }
}

fn parse_input(tool: &str, call: &ToolCall) -> Result<ReadInput, String> {
    let raw = match &call.input {
        ToolInput::Json(raw) => raw,
        ToolInput::Text(_) => {
            return Err(invalid(
                tool,
                "expected a JSON object input, got freeform text",
            ));
        }
    };
    let input: ReadInput =
        serde_json::from_str(raw).map_err(|error| invalid(tool, &error.to_string()))?;
    if matches!(input.offset, Some(offset) if offset < 1) {
        return Err(invalid(tool, "`offset` must be at least 1"));
    }
    if matches!(input.limit, Some(limit) if limit < 1) {
        return Err(invalid(tool, "`limit` must be at least 1"));
    }
    Ok(input)
}

fn invalid(tool: &str, reason: &str) -> String {
    format!("Invalid input for {tool}: {reason}")
}

fn run(
    workspace: &Workspace,
    observed: &ObservedFiles,
    input: &ReadInput,
) -> Result<String, String> {
    let resolved = workspace
        .resolve(&input.path)
        .map_err(|error| error.to_string())?;
    let display = workspace.display(&resolved);

    let metadata = match std::fs::metadata(&resolved) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == ErrorKind::NotFound => {
            return Err(format!("{display} does not exist."));
        }
        Err(error) => return Err(format!("{display} could not be read: {error}")),
    };
    if !metadata.is_file() {
        return Err(format!("{display} is not a regular file."));
    }

    let bytes = std::fs::read(&resolved)
        .map_err(|error| format!("{display} could not be read: {error}"))?;

    if bytes[..bytes.len().min(BINARY_SNIFF_BYTES)].contains(&0) {
        return Err(format!("{display} is a binary file."));
    }
    if bytes.is_empty() {
        // An empty file is a successful read of zero bytes: record it so a
        // later `write` to it is not treated as an unread blind overwrite.
        observed.record(&resolved, &bytes);
        return Ok(format!("{display} is empty."));
    }
    let text = std::str::from_utf8(&bytes).map_err(|_| format!("{display} is not valid UTF-8."))?;

    // A read always observes the FULL file, even when offset/limit windows the
    // returned lines: a later edit compares against the whole file.
    observed.record(&resolved, &bytes);

    render_lines(text, &display, input)
}

fn render_lines(text: &str, display: &str, input: &ReadInput) -> Result<String, String> {
    let mut lines: Vec<&str> = text.split('\n').collect();
    if text.ends_with('\n') {
        lines.pop();
    }
    let total = lines.len();
    let offset = input.offset.unwrap_or(DEFAULT_OFFSET) as usize;
    let limit = input.limit.unwrap_or(DEFAULT_LIMIT) as usize;
    let start = offset - 1;
    if start >= total {
        return Err(format!(
            "offset {offset} is beyond the end of {display} ({total} lines)."
        ));
    }
    // The window is bounded HERE, by whole lines, so that `end` reflects what is
    // actually shown and the continuation trailer always names the right next
    // offset. (Bounding window + trailer afterwards cut the trailer off whenever a
    // full default-sized window was returned.)
    let last = (start + limit.min(MAX_OUTPUT_LINES)).min(total);

    let mut out = String::new();
    let mut end = start;
    for (index, line) in lines[start..last].iter().enumerate() {
        let number = start + index + 1;
        let line = line.strip_suffix('\r').unwrap_or(line);
        let rendered = format!("{number:>6}\t{line}");
        if index > 0 && out.len() + 1 + rendered.len() > MAX_OUTPUT_BYTES {
            break;
        }
        if index > 0 {
            out.push('\n');
        }
        out.push_str(&rendered);
        end = number;
    }
    // A single line larger than the byte budget is cut by the shared bounding rule.
    let mut out = bound_output(&out, MAX_OUTPUT_BYTES, usize::MAX);
    if end < total {
        out.push('\n');
        out.push_str(&format!(
            "[{} more lines; continue with offset={}]",
            total - end,
            end + 1
        ));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::{ReadTool, parse_input};
    use p1_contracts::{
        DeclarationKind, Effect, Tool, ToolCall, ToolContext, ToolInput, ToolOutcome, ToolStatus,
    };
    use p1_workspace::{Observation, ObservedFiles, ToolFace, Workspace};
    use std::path::Path;

    fn workspace(root: &Path) -> Workspace {
        Workspace::new(root).unwrap()
    }

    fn tool(root: &Path) -> (ReadTool, ObservedFiles) {
        let observed = ObservedFiles::new();
        (ReadTool::new(workspace(root), observed.clone()), observed)
    }

    fn call(arguments: &str) -> ToolCall {
        ToolCall {
            call_id: "call-1".into(),
            name: "read".into(),
            input: ToolInput::Json(arguments.to_string()),
        }
    }

    async fn execute(tool: &ReadTool, arguments: &str) -> ToolOutcome {
        let call = call(arguments);
        let context = ToolContext {
            cancel: p1_contracts::CancellationToken::new(),
        };
        tool.execute(&call, context).await
    }

    fn schema(tool: &ReadTool) -> serde_json::Value {
        match &tool.declaration().kind {
            DeclarationKind::Function { input_schema } => input_schema.clone(),
            other => panic!("expected a function declaration, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn declaration_is_a_function_with_the_exact_spec_schema() {
        let dir = tempfile::tempdir().unwrap();
        let (tool, _) = tool(dir.path());

        assert_eq!(tool.declaration().name, "read");
        let schema = schema(&tool);
        assert_eq!(schema["type"], "object");
        assert_eq!(schema["required"], serde_json::json!(["path"]));
        assert_eq!(schema["additionalProperties"], false);
        assert_eq!(schema["properties"]["path"]["type"], "string");
        assert_eq!(schema["properties"]["offset"]["minimum"], 1);
        assert_eq!(schema["properties"]["offset"]["default"], 1);
        assert_eq!(schema["properties"]["limit"]["minimum"], 1);
        assert_eq!(schema["properties"]["limit"]["default"], 2000);
        let properties = schema["properties"].as_object().unwrap();
        assert_eq!(properties.len(), 3);
        assert!(tool.declaration().description.contains("before you edit"));
    }

    #[test]
    fn identity_defaults_to_the_claude_variant_and_survives_a_face_change() {
        let dir = tempfile::tempdir().unwrap();
        let (tool, _) = tool(dir.path());
        assert_eq!(tool.identity().implementation, "p1-tool-read");
        assert_eq!(tool.identity().variant, "claude");

        let reshaped = tool.with_face(ToolFace::new("ReadFile", "custom"), "gpt");
        assert_eq!(reshaped.declaration().name, "ReadFile");
        assert_eq!(reshaped.declaration().description, "custom");
        assert_eq!(reshaped.identity().implementation, "p1-tool-read");
        assert_eq!(reshaped.identity().variant, "gpt");
    }

    #[test]
    fn effect_is_read_only() {
        let dir = tempfile::tempdir().unwrap();
        let (tool, _) = tool(dir.path());
        assert_eq!(tool.effect(&call("{}")), Effect::ReadOnly);
    }

    #[tokio::test]
    async fn read_returns_line_numbered_content() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "alpha\nbeta\ngamma\n").unwrap();
        let (tool, _) = tool(dir.path());

        let outcome = execute(&tool, r#"{"path": "a.txt"}"#).await;

        assert_eq!(outcome.status, ToolStatus::Ok);
        assert_eq!(
            outcome.content,
            "     1\talpha\n     2\tbeta\n     3\tgamma"
        );
    }

    #[tokio::test]
    async fn read_offset_and_limit_window_and_report_more_lines() {
        let dir = tempfile::tempdir().unwrap();
        let body: String = (1..=10).map(|n| format!("line{n}\n")).collect();
        std::fs::write(dir.path().join("b.txt"), body).unwrap();
        let (tool, _) = tool(dir.path());

        let outcome = execute(&tool, r#"{"path": "b.txt", "offset": 3, "limit": 2}"#).await;

        assert_eq!(
            outcome.content,
            "     3\tline3\n     4\tline4\n[6 more lines; continue with offset=5]"
        );
    }

    #[tokio::test]
    async fn read_reports_a_single_remaining_line() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("c.txt"), "one\ntwo\n").unwrap();
        let (tool, _) = tool(dir.path());

        let outcome = execute(&tool, r#"{"path": "c.txt", "limit": 1}"#).await;

        assert_eq!(
            outcome.content,
            "     1\tone\n[1 more lines; continue with offset=2]"
        );
    }

    #[tokio::test]
    async fn read_empty_file_is_a_successful_empty_read() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("empty.txt"), "").unwrap();
        let (tool, observed) = tool(dir.path());

        let outcome = execute(&tool, r#"{"path": "empty.txt"}"#).await;

        assert_eq!(outcome.status, ToolStatus::Ok);
        assert_eq!(outcome.content, "empty.txt is empty.");
        // The empty contents are observed, so a later write is not a blind
        // overwrite.
        assert_eq!(
            observed.check_unchanged(&dir.path().join("empty.txt"), b""),
            Observation::Unchanged
        );
    }

    #[tokio::test]
    async fn read_rejects_a_missing_file_and_a_directory() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("subdir")).unwrap();
        let (tool, _) = tool(dir.path());

        let missing = execute(&tool, r#"{"path": "nope.txt"}"#).await;
        assert_eq!(missing.status, ToolStatus::Error);
        assert_eq!(missing.content, "nope.txt does not exist.");

        let directory = execute(&tool, r#"{"path": "subdir"}"#).await;
        assert_eq!(directory.status, ToolStatus::Error);
        assert_eq!(directory.content, "subdir is not a regular file.");
    }

    #[tokio::test]
    async fn read_rejects_a_binary_file() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("nul.dat"), b"alpha\0beta").unwrap();
        let (tool, _) = tool(dir.path());

        let outcome = execute(&tool, r#"{"path": "nul.dat"}"#).await;

        assert_eq!(outcome.status, ToolStatus::Error);
        assert_eq!(outcome.content, "nul.dat is a binary file.");
    }

    #[tokio::test]
    async fn read_rejects_invalid_utf8() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("binary.dat"), [b'a', 0xFF, b'b']).unwrap();
        let (tool, _) = tool(dir.path());

        let outcome = execute(&tool, r#"{"path": "binary.dat"}"#).await;

        assert_eq!(outcome.status, ToolStatus::Error);
        assert_eq!(outcome.content, "binary.dat is not valid UTF-8.");
    }

    #[tokio::test]
    async fn read_records_the_full_contents_as_observed() {
        let dir = tempfile::tempdir().unwrap();
        let contents = "alpha\nbeta\ngamma\n";
        std::fs::write(dir.path().join("a.txt"), contents).unwrap();
        let (tool, observed) = tool(dir.path());

        // A windowed read still observes the whole file.
        execute(&tool, r#"{"path": "a.txt", "limit": 1}"#).await;

        assert_eq!(
            observed.check_unchanged(&dir.path().join("a.txt"), contents.as_bytes()),
            Observation::Unchanged
        );
    }

    #[tokio::test]
    async fn read_rejects_a_path_outside_the_workspace() {
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("secret.txt"), "secret").unwrap();
        let (tool, _) = tool(dir.path());

        let escaped = execute(&tool, r#"{"path": "../secret.txt"}"#).await;
        assert_eq!(escaped.status, ToolStatus::Error);
        assert!(escaped.content.contains("escapes workspace"), "{escaped:?}");

        let absolute = outside.path().join("secret.txt");
        let absolute = execute(
            &tool,
            &serde_json::json!({ "path": absolute.to_str().unwrap() }).to_string(),
        )
        .await;
        assert_eq!(absolute.status, ToolStatus::Error);
        assert!(
            absolute.content.contains("escapes workspace"),
            "{absolute:?}"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn read_rejects_a_symlink_that_points_outside_the_workspace() {
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("secret.txt"), "secret").unwrap();
        std::os::unix::fs::symlink(outside.path(), dir.path().join("link")).unwrap();
        let (tool, _) = tool(dir.path());

        let outcome = execute(&tool, r#"{"path": "link/secret.txt"}"#).await;

        assert_eq!(outcome.status, ToolStatus::Error);
        assert!(outcome.content.contains("escapes workspace"), "{outcome:?}");
    }

    #[tokio::test]
    async fn read_offset_beyond_the_end_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "one\ntwo\n").unwrap();
        let (tool, _) = tool(dir.path());

        let outcome = execute(&tool, r#"{"path": "a.txt", "offset": 9}"#).await;

        assert_eq!(outcome.status, ToolStatus::Error);
        assert!(outcome.content.contains("beyond the end"), "{outcome:?}");
    }

    #[tokio::test]
    async fn read_bounds_a_single_huge_line() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("long.txt"), "x".repeat(60_000)).unwrap();
        let (tool, _) = tool(dir.path());

        let outcome = execute(&tool, r#"{"path": "long.txt"}"#).await;

        assert_eq!(outcome.status, ToolStatus::Ok);
        assert!(outcome.content.contains("[output truncated: showing"));
        assert!(
            outcome.content.len() < 51_000,
            "len={}",
            outcome.content.len()
        );
    }

    #[tokio::test]
    async fn invalid_input_reports_a_prefix_and_never_panics() {
        let dir = tempfile::tempdir().unwrap();
        let (tool, _) = tool(dir.path());
        let garbage = [
            "",
            "null",
            "[]",
            "{\"path\": 5}",
            "{\"path\":\"a.txt\",\"unknown\":1}",
            "{\"path\":\"a.txt\",\"offset\":0}",
            "{\"path\":\"a.txt\",\"limit\":0}",
            "\u{0}\u{1}{\"path\" garbage",
        ];
        for arguments in garbage {
            let outcome = execute(&tool, arguments).await;
            assert_eq!(outcome.status, ToolStatus::Error, "input: {arguments:?}");
            assert!(
                outcome.content.starts_with("Invalid input for read: "),
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
            name: "read".into(),
            input: ToolInput::Text("path=a.txt".into()),
        };
        let context = ToolContext {
            cancel: p1_contracts::CancellationToken::new(),
        };

        let outcome = tool.execute(&call, context).await;

        assert_eq!(outcome.status, ToolStatus::Error);
        assert!(outcome.content.starts_with("Invalid input for read: "));
    }

    #[tokio::test]
    async fn execute_returns_cancelled_without_touching_the_filesystem() {
        let dir = tempfile::tempdir().unwrap();
        let (tool, _) = tool(dir.path());
        let call = call(r#"{"path": "new.txt"}"#);
        let cancel = p1_contracts::CancellationToken::new();
        cancel.cancel();

        let outcome = tool.execute(&call, ToolContext { cancel }).await;

        assert_eq!(outcome.status, ToolStatus::Cancelled);
        assert!(!dir.path().join("new.txt").exists());
    }

    #[tokio::test]
    async fn file_work_runs_off_the_async_thread() {
        // The synchronous file work is moved to a blocking task, so the
        // returned future is `Send` and `execute` completes on a current-thread
        // runtime without blocking the executor. The blocking pool's thread
        // identity is not observable with the `rt`-only tokio features.
        fn assert_send<T: Send>(_: &T) {}
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "alpha\n").unwrap();
        let (tool, _) = tool(dir.path());
        let call = call(r#"{"path": "a.txt"}"#);
        let context = ToolContext {
            cancel: p1_contracts::CancellationToken::new(),
        };

        let future = tool.execute(&call, context);
        assert_send(&future);
        let outcome = future.await;

        assert_eq!(outcome.status, ToolStatus::Ok);
    }

    #[test]
    fn parse_input_rejects_a_freeform_text_call() {
        let call = ToolCall {
            call_id: "c".into(),
            name: "read".into(),
            input: ToolInput::Text("anything".into()),
        };
        assert!(parse_input("read", &call).is_err());
    }
}
