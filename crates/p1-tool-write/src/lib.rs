//! The `write` tool: atomic create-or-replace of one workspace file.
//!
//! Confinement, atomic replacement and observed-file tracking live in
//! `p1-workspace`. This module owns the model-facing declaration, input
//! validation and the read-before-mutate guard for an existing target.

use p1_contracts::{
    BoxFuture, DeclarationKind, Effect, Tool, ToolCall, ToolContext, ToolDeclaration, ToolIdentity,
    ToolInput, ToolOutcome, ToolStatus,
};
use p1_workspace::{Observation, ObservedFiles, Workspace, bound_output, write_atomic};
use serde::Deserialize;

pub use p1_workspace::ToolFace;

const NAME: &str = "write";
const DESCRIPTION: &str = "Create or replace a workspace file atomically, creating missing parent directories.\nOverwriting an existing file requires that you read its current contents first.\nPrefer `edit` for small changes: `write` replaces the whole file.";
const MAX_OUTPUT_BYTES: usize = 50_000;
const MAX_OUTPUT_LINES: usize = 2_000;

/// The `write` tool. Holds one agent's workspace and observation store.
pub struct WriteTool {
    workspace: Workspace,
    observed: ObservedFiles,
    declaration: ToolDeclaration,
    identity: ToolIdentity,
}

impl WriteTool {
    /// Build the tool with the default (`write`, Claude-family) face.
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
            "content": {
                "type": "string",
                "description": "Complete file contents; replaces any existing file."
            }
        },
        "required": ["file_path", "content"],
        "additionalProperties": false
    })
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct WriteInput {
    file_path: String,
    content: String,
}

impl Tool for WriteTool {
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

fn parse_input(tool: &str, call: &ToolCall) -> Result<WriteInput, String> {
    let raw = match &call.input {
        ToolInput::Json(raw) => raw,
        ToolInput::Text(_) => {
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

fn run(
    workspace: &Workspace,
    observed: &ObservedFiles,
    input: &WriteInput,
) -> Result<String, String> {
    let resolved = workspace
        .resolve(&input.file_path)
        .map_err(|error| error.to_string())?;
    let display = workspace.display(&resolved);

    // Check and write are one step for every agent sharing this gate: another
    // agent's write (or create) cannot land between the check and ours.
    let _mutation = workspace.begin_mutation();
    // Read-before-mutate applies only when the target already exists: creating
    // a new file is a blind create, which is allowed.
    if resolved.exists() {
        let bytes = std::fs::read(&resolved)
            .map_err(|error| format!("{display} could not be read: {error}"))?;
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
    }

    write_atomic(&resolved, input.content.as_bytes())
        .map_err(|error| format!("failed to write {display}: {error}"))?;
    // A successful mutation records the new contents, so a follow-up edit or
    // write needs no re-read.
    observed.record(&resolved, input.content.as_bytes());

    Ok(format!("Wrote {display} ({} bytes).", input.content.len()))
}

#[cfg(test)]
mod tests {
    use super::WriteTool;
    use p1_contracts::{
        DeclarationKind, Effect, Tool, ToolCall, ToolContext, ToolInput, ToolOutcome, ToolStatus,
    };
    use p1_workspace::{Observation, ObservedFiles, ToolFace, Workspace};
    use std::path::Path;

    fn tool(root: &Path) -> (WriteTool, ObservedFiles) {
        let observed = ObservedFiles::new();
        (
            WriteTool::new(Workspace::new(root).unwrap(), observed.clone()),
            observed,
        )
    }

    fn call(arguments: &str) -> ToolCall {
        ToolCall {
            call_id: "call-1".into(),
            name: "write".into(),
            input: ToolInput::Json(arguments.to_string()),
        }
    }

    async fn execute(tool: &WriteTool, arguments: &str) -> ToolOutcome {
        let call = call(arguments);
        let context = ToolContext {
            cancel: p1_contracts::CancellationToken::new(),
        };
        tool.execute(&call, context).await
    }

    fn schema(tool: &WriteTool) -> serde_json::Value {
        match &tool.declaration().kind {
            DeclarationKind::Function { input_schema } => input_schema.clone(),
            other => panic!("expected a function declaration, got {other:?}"),
        }
    }

    fn read(observed: &ObservedFiles, path: &Path) {
        observed.record(path, &std::fs::read(path).unwrap());
    }

    #[test]
    fn declaration_is_a_function_with_the_exact_spec_schema() {
        let dir = tempfile::tempdir().unwrap();
        let (tool, _) = tool(dir.path());

        assert_eq!(tool.declaration().name, "write");
        let schema = schema(&tool);
        assert_eq!(schema["type"], "object");
        assert_eq!(
            schema["required"],
            serde_json::json!(["file_path", "content"])
        );
        assert_eq!(schema["additionalProperties"], false);
        assert_eq!(schema["properties"]["file_path"]["type"], "string");
        assert_eq!(schema["properties"]["content"]["type"], "string");
        assert_eq!(schema["properties"].as_object().unwrap().len(), 2);
    }

    #[test]
    fn identity_defaults_to_the_claude_variant_and_survives_a_face_change() {
        let dir = tempfile::tempdir().unwrap();
        let (tool, _) = tool(dir.path());
        assert_eq!(tool.identity().implementation, "p1-tool-write");
        assert_eq!(tool.identity().variant, "claude");

        let reshaped = tool.with_face(ToolFace::new("WriteFile", "custom"), "gpt");
        assert_eq!(reshaped.declaration().name, "WriteFile");
        assert_eq!(reshaped.identity().variant, "gpt");
    }

    #[test]
    fn effect_writes_files() {
        let dir = tempfile::tempdir().unwrap();
        let (tool, _) = tool(dir.path());
        assert_eq!(tool.effect(&call("{}")), Effect::WritesFiles);
    }

    #[tokio::test]
    async fn write_creates_parent_dirs_and_reports_the_byte_count() {
        let dir = tempfile::tempdir().unwrap();
        let (tool, _) = tool(dir.path());

        let outcome = execute(
            &tool,
            r#"{"file_path": "nested/dir/file.txt", "content": "hello"}"#,
        )
        .await;

        assert_eq!(outcome.status, ToolStatus::Ok);
        assert_eq!(outcome.content, "Wrote nested/dir/file.txt (5 bytes).");
        assert_eq!(
            std::fs::read(dir.path().join("nested/dir/file.txt")).unwrap(),
            b"hello"
        );
    }

    #[tokio::test]
    async fn write_rejects_an_existing_file_that_was_never_read() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("out.txt");
        std::fs::write(&path, "old").unwrap();
        let (tool, _) = tool(dir.path());

        let outcome = execute(&tool, r#"{"file_path": "out.txt", "content": "new"}"#).await;

        assert_eq!(outcome.status, ToolStatus::Error);
        assert_eq!(outcome.content, "You must read out.txt before changing it.");
        assert_eq!(std::fs::read(&path).unwrap(), b"old");
    }

    #[tokio::test]
    async fn write_rejects_an_existing_file_that_changed_since_it_was_observed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("out.txt");
        std::fs::write(&path, "old").unwrap();
        let (tool, observed) = tool(dir.path());
        read(&observed, &path);
        std::fs::write(&path, "externally changed").unwrap();

        let outcome = execute(&tool, r#"{"file_path": "out.txt", "content": "new"}"#).await;

        assert_eq!(outcome.status, ToolStatus::Error);
        assert_eq!(
            outcome.content,
            "out.txt changed on disk since you last read it; read it again."
        );
        assert_eq!(std::fs::read(&path).unwrap(), b"externally changed");
    }

    #[tokio::test]
    async fn write_to_a_new_file_needs_no_prior_read() {
        let dir = tempfile::tempdir().unwrap();
        let (tool, _) = tool(dir.path());

        let outcome = execute(&tool, r#"{"file_path": "fresh.txt", "content": "hi"}"#).await;

        assert_eq!(outcome.status, ToolStatus::Ok);
        assert_eq!(std::fs::read(dir.path().join("fresh.txt")).unwrap(), b"hi");
    }

    #[tokio::test]
    async fn a_successful_write_records_the_new_contents() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("out.txt");
        std::fs::write(&path, "old").unwrap();
        let (tool, observed) = tool(dir.path());
        read(&observed, &path);

        execute(&tool, r#"{"file_path": "out.txt", "content": "new"}"#).await;

        assert_eq!(
            observed.check_unchanged(&path, b"new"),
            Observation::Unchanged
        );
        // A second write therefore needs no re-read.
        let second = execute(&tool, r#"{"file_path": "out.txt", "content": "newer"}"#).await;
        assert_eq!(second.status, ToolStatus::Ok);
        assert_eq!(std::fs::read(&path).unwrap(), b"newer");
    }

    #[tokio::test]
    async fn failed_write_leaves_the_target_byte_identical_and_no_temp_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("out.txt");
        std::fs::write(&path, "old").unwrap();
        let (tool, observed) = tool(dir.path());
        read(&observed, &path);
        std::fs::write(&path, "changed elsewhere").unwrap();

        let outcome = execute(&tool, r#"{"file_path": "out.txt", "content": "new"}"#).await;

        assert_eq!(outcome.status, ToolStatus::Error);
        assert_eq!(std::fs::read(&path).unwrap(), b"changed elsewhere");
        let entries: Vec<String> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(entries, vec!["out.txt".to_string()]);
    }

    #[tokio::test]
    async fn write_rejects_a_path_outside_the_workspace() {
        // The workspace is nested so `../evil.txt` lands in a tempdir we own,
        // never in a shared system directory.
        let container = tempfile::tempdir().unwrap();
        let root = container.path().join("ws");
        std::fs::create_dir(&root).unwrap();
        let outside = tempfile::tempdir().unwrap();
        let (tool, _) = tool(&root);

        let escaped = execute(&tool, r#"{"file_path": "../evil.txt", "content": "x"}"#).await;
        assert_eq!(escaped.status, ToolStatus::Error);
        assert!(escaped.content.contains("escapes workspace"), "{escaped:?}");
        assert!(!container.path().join("evil.txt").exists());

        let absolute = outside.path().join("evil.txt");
        let outcome = execute(
            &tool,
            &serde_json::json!({
                "file_path": absolute.to_str().unwrap(),
                "content": "x"
            })
            .to_string(),
        )
        .await;
        assert_eq!(outcome.status, ToolStatus::Error);
        assert!(!absolute.exists());
        // The rejected write left no temp file anywhere, inside or outside.
        assert!(
            std::fs::read_dir(outside.path()).unwrap().next().is_none(),
            "the outside directory must stay empty"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn write_rejects_a_symlink_that_points_outside_the_workspace() {
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(outside.path(), dir.path().join("link")).unwrap();
        let (tool, _) = tool(dir.path());

        let outcome = execute(&tool, r#"{"file_path": "link/evil.txt", "content": "x"}"#).await;

        assert_eq!(outcome.status, ToolStatus::Error);
        assert!(outcome.content.contains("escapes workspace"), "{outcome:?}");
        assert!(std::fs::read_dir(outside.path()).unwrap().next().is_none());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn write_through_an_inside_symlink_updates_the_target_and_keeps_the_link() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("target.txt");
        std::fs::write(&target, "old").unwrap();
        std::os::unix::fs::symlink(&target, dir.path().join("link.txt")).unwrap();
        let (tool, observed) = tool(dir.path());
        // Observing either the link or the target records the same canonical
        // path, so a prior read through the link satisfies the guard.
        observed.record(&dir.path().join("link.txt"), b"old");

        let outcome = execute(&tool, r#"{"file_path": "link.txt", "content": "new"}"#).await;

        assert_eq!(outcome.status, ToolStatus::Ok);
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "new");
        assert!(
            std::fs::symlink_metadata(dir.path().join("link.txt"))
                .unwrap()
                .file_type()
                .is_symlink()
        );
    }

    #[tokio::test]
    async fn file_work_runs_off_the_async_thread() {
        // The synchronous file work is moved to a blocking task, so the
        // returned future is `Send` and `execute` completes on a current-thread
        // runtime without blocking the executor.
        fn assert_send<T: Send>(_: &T) {}
        let dir = tempfile::tempdir().unwrap();
        let (tool, _) = tool(dir.path());
        let call = call(r#"{"file_path": "fresh.txt", "content": "hi"}"#);
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
            "{\"file_path\":\"a.txt\",\"content\":\"x\",\"extra\":1}",
            "\u{0}\u{1}{garbage",
        ];
        for arguments in garbage {
            let outcome = execute(&tool, arguments).await;
            assert_eq!(outcome.status, ToolStatus::Error, "input: {arguments:?}");
            assert!(
                outcome.content.starts_with("Invalid input for write: "),
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
            name: "write".into(),
            input: ToolInput::Text("file_path=a.txt".into()),
        };
        let cancel = p1_contracts::CancellationToken::new();
        let outcome = tool.execute(&call, ToolContext { cancel }).await;

        assert_eq!(outcome.status, ToolStatus::Error);
        assert!(outcome.content.starts_with("Invalid input for write: "));
    }

    #[tokio::test]
    async fn execute_returns_cancelled_without_touching_the_filesystem() {
        let dir = tempfile::tempdir().unwrap();
        let (tool, _) = tool(dir.path());
        let call = call(r#"{"file_path": "fresh.txt", "content": "hi"}"#);
        let cancel = p1_contracts::CancellationToken::new();
        cancel.cancel();

        let outcome = tool.execute(&call, ToolContext { cancel }).await;

        assert_eq!(outcome.status, ToolStatus::Cancelled);
        assert!(!dir.path().join("fresh.txt").exists());
    }
}
