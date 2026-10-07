//! The `edit` tool: exact string replacement in one workspace file.
//!
//! Confinement, atomic replacement and observed-file tracking live in
//! `p1-workspace`. The model-facing declaration, input validation, the
//! exact-match search, line-ending preservation, the output text and the
//! descriptions live in `p1-tool-edit-logic`, the one copy the `edit`
//! component (`modules/p1-module-edit/`) runs too; this crate adapts them to
//! the native `Tool` contract.

use p1_contracts::tool::{ResultDescription, ResultDetail};
use p1_contracts::{
    BoxFuture, CallDescription, DeclarationKind, EditPreview, Effect, Tool, ToolCall, ToolContext,
    ToolDeclaration, ToolIdentity, ToolInput, ToolOutcome, ToolStatus,
};
use p1_tool_edit_logic::{self as logic, EditInput};
use p1_workspace::{MutationPolicy, ObservedFiles, Workspace};

pub use p1_workspace::ToolFace;

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
    ToolFace::new(logic::NAME, logic::DESCRIPTION)
}

fn declaration(face: ToolFace) -> ToolDeclaration {
    ToolDeclaration {
        name: face.name,
        description: face.description,
        kind: DeclarationKind::Function {
            input_schema: logic::input_schema(),
        },
    }
}

fn identity(variant: &str) -> ToolIdentity {
    ToolIdentity {
        implementation: env!("CARGO_PKG_NAME").to_string(),
        variant: variant.to_string(),
    }
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

    /// ADR-0057: the file this call edits, from the tool's own parsed input.
    fn describe(&self, call: &ToolCall) -> CallDescription {
        let parsed = parse_input(&self.declaration.name, call).ok();
        // Natively the real resolution decides, symlinks included; the component, which
        // cannot resolve on the restricted path, decides lexically.
        let destructive = parsed
            .as_ref()
            .is_some_and(|input| self.workspace.resolve(&input.file_path).is_err());
        CallDescription {
            verb: logic::VERB,
            target: parsed.as_ref().map(|input| input.file_path.clone()),
            edit: parsed.map(|input| EditPreview {
                path: input.file_path,
                old: input.old_string,
                new: input.new_string,
            }),
            destructive,
        }
    }

    fn describe_result(
        &self,
        call: &ToolCall,
        result: &p1_contracts::ToolResultItem,
    ) -> ResultDescription {
        let described = logic::describe_result(
            parse_input(&self.declaration.name, call).ok(),
            result.status == ToolStatus::Ok,
            &result.content,
        );
        ResultDescription {
            summary: described.summary,
            detail: described.diff.map(|diff| ResultDetail::Diff {
                path: diff.path,
                before: diff.before,
                after: diff.after,
            }),
        }
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
            // A no-op edit succeeds without touching the file or its read state.
            if logic::is_no_change(&input) {
                return ToolOutcome::ok(logic::no_change(&input.file_path));
            }
            let workspace = self.workspace.clone();
            let observed = self.observed.clone();
            let tool = self.declaration.name.clone();
            let cancel = context.cancel.clone();
            // All filesystem work runs on a blocking thread; the async thread
            // is never used for synchronous I/O.
            match tokio::task::spawn_blocking(move || run(&workspace, &observed, &input, &cancel))
                .await
            {
                Ok(Ok(content)) => ToolOutcome::ok(content),
                Ok(Err(message)) if message == "cancelled" => ToolOutcome {
                    status: ToolStatus::Cancelled,
                    content: String::new(),
                },
                Ok(Err(message)) => ToolOutcome::error(message),
                Err(error) => ToolOutcome::error(format!("{tool} failed: {error}")),
            }
        })
    }
}

fn parse_input(tool: &str, call: &ToolCall) -> Result<EditInput, String> {
    match &call.input {
        ToolInput::Json(raw) => logic::parse_json_input(tool, raw),
        ToolInput::Text(_) => Err(logic::text_input_error(tool)),
    }
}

fn run(
    workspace: &Workspace,
    observed: &ObservedFiles,
    input: &EditInput,
    cancel: &p1_contracts::CancellationToken,
) -> Result<String, String> {
    run_with(workspace, observed, input, cancel, || {})
}

fn run_with(
    workspace: &Workspace,
    observed: &ObservedFiles,
    input: &EditInput,
    cancel: &p1_contracts::CancellationToken,
    before_commit: impl FnOnce(),
) -> Result<String, String> {
    if cancel.is_cancelled() {
        return Err("cancelled".into());
    }
    workspace.refuse_mutation_credentials(&input.file_path)?;
    let resolved = workspace
        .resolve(&input.file_path)
        .map_err(|error| error.to_string())?;
    let display = workspace.display(&resolved);

    let snapshot = match workspace.read_unobserved_checked(&input.file_path, cancel) {
        Ok(snapshot) => snapshot,
        Err(p1_workspace::WorkspaceError::NotFound { .. }) => {
            return Err(logic::does_not_exist(&display));
        }
        Err(p1_workspace::WorkspaceError::NotADirectory(_)) => {
            // The path opened but is not a regular file (a directory, FIFO or
            // other object). The native tool reads with `std::fs::read`, whose
            // EISDIR text the model sees; `open_file_at_with_path` folds that
            // and its other wrong-kind cases into `NotADirectory` (the frozen
            // FIFO test), so restore the directory wording here.
            return Err(logic::could_not_read(
                &display,
                "Is a directory (os error 21)",
            ));
        }
        Err(p1_workspace::WorkspaceError::Io { source, .. }) => {
            return Err(logic::could_not_read(&display, &source.to_string()));
        }
        Err(error) => return Err(logic::could_not_read(&display, &error.to_string())),
    };
    let bytes = snapshot.read(0, usize::MAX);
    match observed.check_unchanged(&resolved, bytes) {
        p1_workspace::Observation::NeverObserved => {
            return Err(logic::never_observed(&display));
        }
        p1_workspace::Observation::ChangedSinceObserved => {
            return Err(logic::changed_since_observed(&display));
        }
        p1_workspace::Observation::Unchanged => {}
    }
    // The commit rechecks the observation and source bytes under the write gate.
    let edited = logic::edit_text(&display, bytes, input)?;
    before_commit();
    // Bind the commit to the file this call actually read. The read record remembers
    // which file `input.file_path` resolved to, so a symlink retargeted after the
    // snapshot is refused as stale instead of redirecting the edit to another file that
    // happens to hold the same bytes (the component's `ReadRecord` path check does the
    // same). `write_cancellable` rechecks the token after the gate opens and after
    // staging, so a queued cancelled edit writes nothing.
    let reads = p1_workspace::ReadRecord::new();
    reads.record_read(
        &workspace.spelling(&input.file_path),
        &resolved,
        snapshot.metadata().content_hash,
    );
    let held = tokio::runtime::Handle::current().block_on(workspace.begin_owned(
        observed,
        &reads,
        MutationPolicy::Observed,
    ));
    if cancel.is_cancelled() {
        return Err("cancelled".into());
    }
    held.write_cancellable(&input.file_path, edited.contents.as_bytes(), cancel)
        .map_err(|error| error.to_string())?;

    Ok(logic::edited_output(
        &display,
        edited.replacements,
        edited.applied_region.as_deref(),
    ))
}

#[cfg(test)]
mod tests {
    use super::EditTool;
    use p1_contracts::tool::ResultDetail;
    use p1_contracts::{
        DeclarationKind, EditPreview, Effect, Tool, ToolCall, ToolContext, ToolInput, ToolOutcome,
        ToolResultItem, ToolStatus,
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

    /// ADR-0057: the description comes from this tool's own parsed input, and a
    /// renamed face does not change it.
    #[test]
    fn describe_names_the_file_it_edits_under_any_face() {
        let dir = tempfile::tempdir().unwrap();
        let (tool, _) = tool(dir.path());
        let call = call(r#"{"file_path": "src/a.rs", "old_string": "a", "new_string": "b"}"#);
        assert_eq!(tool.describe(&call).verb, "edit");
        assert_eq!(tool.describe(&call).target.as_deref(), Some("src/a.rs"));
        assert_eq!(
            tool.describe(&call).edit,
            Some(EditPreview {
                path: "src/a.rs".into(),
                old: "a".into(),
                new: "b".into(),
            })
        );
        assert!(!tool.describe(&call).destructive);
        assert!(
            !tool
                .describe(&super::tests::call(
                    &serde_json::json!({
                        "file_path": dir.path().join("inside.rs"),
                        "old_string": "a",
                        "new_string": "b"
                    })
                    .to_string()
                ))
                .destructive
        );
        assert!(
            tool.describe(&super::tests::call(
                r#"{"file_path": "../a.rs", "old_string": "a", "new_string": "b"}"#
            ))
            .destructive
        );
        let absolute = dir.path().parent().unwrap().join("a.rs");
        assert!(
            tool.describe(&super::tests::call(
                &serde_json::json!({
                    "file_path": absolute,
                    "old_string": "a",
                    "new_string": "b"
                })
                .to_string()
            ))
            .destructive
        );

        let renamed = tool.with_face(ToolFace::new("EditFile", "custom"), "gpt");
        let described = renamed.describe(&call);
        assert_eq!(described.verb, "edit");
        assert_eq!(described.target.as_deref(), Some("src/a.rs"));
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
        let result = ToolResultItem {
            call_id: "call-1".into(),
            name: "edit".into(),
            status: outcome.status,
            content: outcome.content,
        };
        let described = tool.describe_result(
            &call(r#"{"file_path": "d.txt", "old_string": "two", "new_string": "TWO"}"#),
            &result,
        );
        assert_eq!(described.summary, "+1 −1");
        assert_eq!(
            described.detail,
            Some(ResultDetail::Diff {
                path: "d.txt".into(),
                before: "two".into(),
                after: "TWO".into(),
            })
        );
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

    /// A symlink in `file_path` retargeted after the snapshot is read must not redirect
    /// the edit: the commit carries the resolved file's identity, so the new target is
    /// refused as stale even when it holds the same bytes and was observed too.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_symlink_retargeted_after_the_snapshot_is_refused() {
        use std::os::unix::fs::symlink;
        let dir = tempfile::tempdir().unwrap();
        let workspace = Workspace::new(dir.path()).unwrap();
        let observed = ObservedFiles::new();
        std::fs::write(dir.path().join("a"), "hello").unwrap();
        std::fs::write(dir.path().join("b"), "hello").unwrap();
        symlink("a", dir.path().join("link")).unwrap();
        read(&observed, &dir.path().join("a"));
        read(&observed, &dir.path().join("b"));
        let input = p1_tool_edit_logic::EditInput {
            file_path: "link".into(),
            old_string: "hello".into(),
            new_string: "world".into(),
            replace_all: false,
        };
        let cancel = p1_contracts::CancellationToken::new();
        let root = dir.path().to_path_buf();
        let result = tokio::task::spawn_blocking(move || {
            super::run_with(&workspace, &observed, &input, &cancel, || {
                std::fs::remove_file(root.join("link")).unwrap();
                symlink("b", root.join("link")).unwrap();
            })
        })
        .await
        .unwrap();
        assert!(result.is_err(), "{result:?}");
        assert_eq!(
            std::fs::read_to_string(dir.path().join("a")).unwrap(),
            "hello"
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join("b")).unwrap(),
            "hello"
        );
    }
}
