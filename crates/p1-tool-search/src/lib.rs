//! The `grep` tool: regex search over the workspace with the ripgrep library
//! crates.
//!
//! `ignore` provides the `.gitignore`-aware walk and the glob filter, and
//! `grep` (regex + searcher) does the matching, so no `rg` binary is needed.
//! Confinement lives in `p1-workspace`. The declaration, input validation, the
//! grouped rendering with its own output bound and footers, and the descriptions
//! live in `p1-tool-search-logic`, the one copy the `grep` component
//! (`modules/p1-module-search/`) runs too: the native tool runs that crate's
//! `execute` over the walk, so native and component run the same code.
//!
//! The walk and the `workspace` capability service that links it into the component
//! are the HOST's (`p1_module_runtime::file_walk`, `p1_module_runtime::file_services`,
//! S7.10-R1, ADR-0094): the native tool calls the same walk through the runtime's
//! types, and re-exports the service for its own tests.

pub use p1_module_runtime::file_services::{SearchCapability, search_services};

use p1_contracts::tool::{ResultDescription, ResultDetail};
use p1_contracts::{
    BoxFuture, CallDescription, CancellationToken, DeclarationKind, Effect, Tool, ToolCall,
    ToolContext, ToolDeclaration, ToolIdentity, ToolInput, ToolOutcome, ToolStatus,
};
use p1_module_runtime::file_walk;
use p1_tool_search_logic::exec::{
    CallInput, Capabilities, Entry, EntryKind, FileMatches, FsError, Outcome, SearchLine,
    SearchQuery, SearchResult,
};
use p1_tool_search_logic::{self as logic, GrepInput, Mode};
use p1_workspace::{FileKind, ToolFace, Workspace, WorkspaceError};

#[cfg(test)]
use p1_tool_search_logic::{newlines, within_bound};

/// The shared output bound (`bound_output`'s defaults). `grep` bounds its own
/// result to it, so the bound is also part of this crate's interface.
pub const MAX_OUTPUT_BYTES: usize = logic::MAX_OUTPUT_BYTES;
pub const MAX_OUTPUT_LINES: usize = logic::MAX_OUTPUT_LINES;

/// The `grep` tool. Holds one agent's workspace.
pub struct GrepTool {
    workspace: Workspace,
    declaration: ToolDeclaration,
    identity: ToolIdentity,
}

impl GrepTool {
    /// Build the tool with the default (`grep`, Claude-family) face.
    pub fn new(workspace: Workspace) -> Self {
        Self {
            workspace,
            declaration: declaration(default_face()),
            identity: identity("claude"),
        }
    }

    /// Present the same implementation under another name/description and
    /// variant. The input schema and the semantics do not change.
    pub fn with_face(self, face: ToolFace, variant: &str) -> Self {
        Self {
            workspace: self.workspace,
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

impl Tool for GrepTool {
    fn declaration(&self) -> &ToolDeclaration {
        &self.declaration
    }

    fn identity(&self) -> &ToolIdentity {
        &self.identity
    }

    fn effect(&self, _call: &ToolCall) -> Effect {
        Effect::ReadOnly
    }

    /// ADR-0057: the pattern and scope searched, from the tool's own parsed
    /// input.
    fn describe(&self, call: &ToolCall) -> CallDescription {
        CallDescription {
            verb: logic::VERB,
            target: parse_input(&self.declaration.name, call)
                .ok()
                .map(|input| logic::describe_target(&input)),
            edit: None,
            destructive: false,
        }
    }

    fn describe_result(
        &self,
        call: &ToolCall,
        result: &p1_contracts::ToolResultItem,
    ) -> ResultDescription {
        let files_mode =
            parse_input(&self.declaration.name, call).is_ok_and(|input| input.mode == Mode::Files);
        let described =
            logic::describe_result(files_mode, result.status == ToolStatus::Ok, &result.content);
        ResultDescription {
            summary: described.summary,
            detail: described.matches.map(|matches| ResultDetail::Matches {
                count: matches.count,
                files: matches.files,
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
            let host = NativeHost {
                workspace: self.workspace.clone(),
                cancel: context.cancel.clone(),
            };
            let tool = self.declaration.name.clone();
            let input = call.input.clone();
            let name = tool.clone();
            // All filesystem work runs on a blocking thread; the async thread is
            // never used for synchronous I/O. This is the native tool's thread,
            // never a guest's: the component runs the same `execute` over host
            // imports it is suspended in.
            let outcome = tokio::task::spawn_blocking(move || {
                let input = match &input {
                    ToolInput::Json(raw) => CallInput::Json(raw),
                    ToolInput::Text(raw) => CallInput::Text(raw),
                };
                logic::exec::execute(&host, &name, input)
            })
            .await;
            match outcome {
                // The logic bounds its own rendering, so the footer that says
                // what is missing survives.
                Ok(Outcome::Ok(content)) => ToolOutcome::ok(content),
                Ok(Outcome::Error(message)) => ToolOutcome::error(message),
                Ok(Outcome::Cancelled) => ToolOutcome {
                    status: ToolStatus::Cancelled,
                    content: String::new(),
                },
                Err(error) => ToolOutcome::error(format!("{tool} failed: {error}")),
            }
        })
    }
}

fn parse_input(tool: &str, call: &ToolCall) -> Result<GrepInput, String> {
    match &call.input {
        ToolInput::Json(raw) => logic::parse_json_input(tool, raw),
        ToolInput::Text(_) => Err(logic::text_input_error(tool)),
    }
}

/// The capabilities the native tool gives the shared `execute`: the same
/// read side the host links into the component, over this agent's workspace.
/// Like the component's, it records no observation and holds no write gate.
struct NativeHost {
    workspace: Workspace,
    cancel: CancellationToken,
}

impl Capabilities for NativeHost {
    fn cancelled(&self) -> bool {
        self.cancel.is_cancelled()
    }

    fn stat(&self, path: &str) -> Result<Entry, FsError> {
        let checked = self.workspace.check_path(path).map_err(fs_error)?;
        let stat = self.workspace.stat(path).map_err(fs_error)?;
        Ok(Entry {
            path: checked.display().to_string(),
            kind: match stat.kind {
                FileKind::File => EntryKind::File,
                FileKind::Directory => EntryKind::Directory,
                _ => EntryKind::Other,
            },
        })
    }

    fn read(&self, path: &str, offset: u64, length: u64) -> Result<Vec<u8>, FsError> {
        read_window(&self.workspace, path, offset, length)
    }

    fn list_files(&self, path: &str, glob: Option<&str>) -> Result<Vec<String>, FsError> {
        list_files(&self.workspace, path, glob, &self.cancel)
    }

    fn search(&self, query: &SearchQuery) -> Result<SearchResult, FsError> {
        search(&self.workspace, query, &self.cancel)
    }
}

/// One window of the file, read directly: the guest reads only the prefix
/// it sniffs for binary content, so the whole file is never loaded. This is the
/// host walk's read ([`p1_module_runtime::file_walk::read_window`], the copy the
/// `p1/search` component's capability service reads through), so native and component
/// read the same bytes for the same request.
pub(crate) fn read_window(
    workspace: &Workspace,
    path: &str,
    offset: u64,
    length: u64,
) -> Result<Vec<u8>, FsError> {
    file_walk::read_window(workspace, path, offset, length).map_err(from_walk)
}

/// The host walk's `workspace.list-files` ([`p1_module_runtime::file_walk`], the one copy the
/// `p1/search` component's capability service also runs) as this crate's logic speaks it: the
/// same files, one error variant per variant.
pub fn list_files(
    workspace: &Workspace,
    path: &str,
    glob: Option<&str>,
    cancel: &CancellationToken,
) -> Result<Vec<String>, FsError> {
    file_walk::list_files(workspace, path, glob, cancel).map_err(from_walk)
}

/// The host walk's `workspace.search`, as [`list_files`] is the host walk's `list-files`: the
/// query and the result are converted at this one boundary, and every failure keeps the text
/// the walk worded it with.
pub fn search(
    workspace: &Workspace,
    query: &SearchQuery,
    cancel: &CancellationToken,
) -> Result<SearchResult, FsError> {
    let found = file_walk::search(workspace, &runtime_query(query), cancel).map_err(from_walk)?;
    Ok(SearchResult {
        files: found
            .files
            .into_iter()
            .map(|file| FileMatches {
                path: file.path,
                lines: file
                    .lines
                    .into_iter()
                    .map(|line| SearchLine {
                        line_number: line.line_number,
                        text: line.text,
                        is_match: line.is_match,
                    })
                    .collect(),
            })
            .collect(),
        truncated: found.truncated,
        omitted_files: found.omitted_files,
    })
}

/// The logic crate's `search-query` as the runtime's, for the host's walk.
fn runtime_query(query: &SearchQuery) -> p1_module_runtime::capabilities::SearchQuery {
    p1_module_runtime::capabilities::SearchQuery {
        pattern: query.pattern.clone(),
        path: query.path.clone(),
        glob: query.glob.clone(),
        case_insensitive: query.case_insensitive,
        context: query.context,
        max_lines: query.max_lines,
    }
}

/// The walk's failures as the frozen `fs-error` the logic crate speaks: one variant per
/// variant, and the message of `invalid-pattern` and `io` exactly as the walk worded it.
fn from_walk(error: p1_module_runtime::FsError) -> FsError {
    match error {
        p1_module_runtime::FsError::OutsideWorkspace => FsError::OutsideWorkspace,
        p1_module_runtime::FsError::NotFound => FsError::NotFound,
        p1_module_runtime::FsError::WrongKind => FsError::WrongKind,
        p1_module_runtime::FsError::AlreadyExists => FsError::AlreadyExists,
        p1_module_runtime::FsError::InvalidPattern(message) => FsError::InvalidPattern(message),
        p1_module_runtime::FsError::Cancelled => FsError::Cancelled,
        p1_module_runtime::FsError::Io(message) => FsError::Io(message),
    }
}

/// The workspace service's failures as the frozen `fs-error`
/// (docs/design/modules/workspace-mutation.md): `io` carries the io error's
/// own text, never a host path.
fn fs_error(error: WorkspaceError) -> FsError {
    match error {
        WorkspaceError::OutsideWorkspace { .. } => FsError::OutsideWorkspace,
        WorkspaceError::NotFound { .. } => FsError::NotFound,
        WorkspaceError::NotADirectory(_) => FsError::WrongKind,
        WorkspaceError::Io { source, .. } => FsError::Io(source.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::{
        GrepTool, MAX_OUTPUT_BYTES, MAX_OUTPUT_LINES, newlines, parse_input, within_bound,
    };
    use p1_contracts::tool::ResultDetail;
    use p1_contracts::{
        CancellationToken, DeclarationKind, Effect, Tool, ToolCall, ToolContext, ToolInput,
        ToolOutcome, ToolResultItem, ToolStatus,
    };
    use p1_workspace::{ToolFace, Workspace, bound_output};
    use std::path::Path;

    fn workspace(root: &Path) -> Workspace {
        Workspace::new(root).unwrap()
    }

    fn tool(root: &Path) -> GrepTool {
        GrepTool::new(workspace(root))
    }

    fn call(arguments: &str) -> ToolCall {
        ToolCall {
            call_id: "call-1".into(),
            name: "grep".into(),
            input: ToolInput::Json(arguments.to_string()),
        }
    }

    async fn execute(tool: &GrepTool, arguments: &str) -> ToolOutcome {
        let call = call(arguments);
        let context = ToolContext {
            cancel: CancellationToken::new(),
        };
        tool.execute(&call, context).await
    }

    fn schema(tool: &GrepTool) -> serde_json::Value {
        match &tool.declaration().kind {
            DeclarationKind::Function { input_schema } => input_schema.clone(),
            other => panic!("expected a function declaration, got {other:?}"),
        }
    }

    /// The must-pass tree: an ignored `target/`, a hidden directory, and two
    /// source files matching `beta`.
    fn must_pass_tree(root: &Path) {
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::create_dir_all(root.join("target")).unwrap();
        std::fs::create_dir_all(root.join(".hidden")).unwrap();
        std::fs::write(root.join(".gitignore"), "target/\n").unwrap();
        std::fs::write(root.join("src/a.rs"), "fn alpha() {}\nfn beta() {}\n").unwrap();
        std::fs::write(root.join("src/b.rs"), "fn beta() {}\n").unwrap();
        std::fs::write(root.join("target/x.rs"), "fn beta() {}\n").unwrap();
        std::fs::write(root.join(".hidden/c.rs"), "fn beta() {}\n").unwrap();
    }

    #[tokio::test]
    async fn content_search_groups_matches_by_file_in_sorted_order() {
        let dir = tempfile::tempdir().unwrap();
        must_pass_tree(dir.path());
        let tool = tool(dir.path());

        let outcome = execute(&tool, r#"{"pattern": "beta"}"#).await;

        assert_eq!(outcome.status, ToolStatus::Ok);
        assert_eq!(
            outcome.content,
            "src/a.rs\n2:fn beta() {}\n\nsrc/b.rs\n1:fn beta() {}"
        );
        let result = ToolResultItem {
            call_id: "call-1".into(),
            name: "grep".into(),
            status: outcome.status,
            content: outcome.content,
        };
        let described = tool.describe_result(&call(r#"{"pattern": "beta"}"#), &result);
        assert_eq!(described.summary, "2 hits · 2 files");
        assert_eq!(
            described.detail,
            Some(ResultDetail::Matches {
                count: 2,
                files: vec!["src/a.rs".into(), "src/b.rs".into()],
            })
        );
    }

    #[tokio::test]
    async fn glob_that_matches_nothing_reports_no_matches() {
        let dir = tempfile::tempdir().unwrap();
        must_pass_tree(dir.path());
        let tool = tool(dir.path());

        let outcome = execute(&tool, r#"{"pattern": "beta", "glob": "*.md"}"#).await;

        assert_eq!(outcome.status, ToolStatus::Ok);
        assert_eq!(outcome.content, "No matches.");
    }

    #[tokio::test]
    async fn invalid_regex_is_an_error_naming_the_problem() {
        let dir = tempfile::tempdir().unwrap();
        must_pass_tree(dir.path());
        let tool = tool(dir.path());

        let outcome = execute(&tool, r#"{"pattern": "("}"#).await;

        assert_eq!(outcome.status, ToolStatus::Error);
        assert!(outcome.content.contains("regex"), "{outcome:?}");
    }

    #[tokio::test]
    async fn path_outside_the_workspace_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        must_pass_tree(dir.path());
        let tool = tool(dir.path());

        let outcome = execute(&tool, r#"{"pattern": "beta", "path": "../"}"#).await;

        assert_eq!(outcome.status, ToolStatus::Error);
        assert!(outcome.content.contains("escapes workspace"), "{outcome:?}");
    }

    #[tokio::test]
    async fn case_insensitive_matches_mixed_case() {
        let dir = tempfile::tempdir().unwrap();
        must_pass_tree(dir.path());
        let tool = tool(dir.path());

        let outcome = execute(&tool, r#"{"pattern": "BETA", "case_insensitive": true}"#).await;

        assert_eq!(outcome.status, ToolStatus::Ok);
        assert!(outcome.content.contains("2:fn beta() {}"), "{outcome:?}");
    }

    #[tokio::test]
    async fn context_lines_are_rendered_with_a_dash_separator() {
        let dir = tempfile::tempdir().unwrap();
        must_pass_tree(dir.path());
        let tool = tool(dir.path());

        let outcome = execute(&tool, r#"{"pattern": "beta", "context": 1}"#).await;

        assert_eq!(outcome.status, ToolStatus::Ok);
        assert_eq!(
            outcome.content,
            "src/a.rs\n1-fn alpha() {}\n2:fn beta() {}\n\nsrc/b.rs\n1:fn beta() {}"
        );
    }

    #[tokio::test]
    async fn files_mode_lists_matching_paths_sorted() {
        let dir = tempfile::tempdir().unwrap();
        must_pass_tree(dir.path());
        let tool = tool(dir.path());

        let outcome = execute(&tool, r#"{"pattern": "beta", "mode": "files"}"#).await;

        assert_eq!(outcome.status, ToolStatus::Ok);
        assert_eq!(outcome.content, "src/a.rs\nsrc/b.rs");
    }

    #[tokio::test]
    async fn files_mode_with_an_empty_pattern_lists_files_by_glob() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.md"), "anything\n").unwrap();
        std::fs::create_dir_all(dir.path().join("sub")).unwrap();
        std::fs::write(dir.path().join("sub/b.md"), "anything\n").unwrap();
        std::fs::write(dir.path().join("c.txt"), "anything\n").unwrap();
        let tool = tool(dir.path());

        let outcome = execute(&tool, r#"{"pattern": "", "mode": "files", "glob": "*.md"}"#).await;

        assert_eq!(outcome.status, ToolStatus::Ok);
        assert_eq!(outcome.content, "a.md\nsub/b.md");
    }

    #[tokio::test]
    async fn binary_files_are_skipped() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("bin.dat"), b"beta\0hidden\n").unwrap();
        std::fs::write(dir.path().join("text.txt"), "beta\n").unwrap();
        let tool = tool(dir.path());

        let outcome = execute(&tool, r#"{"pattern": "beta"}"#).await;

        assert_eq!(outcome.status, ToolStatus::Ok);
        assert_eq!(outcome.content, "text.txt\n1:beta");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn symlinks_are_not_followed() {
        use std::os::unix::fs::symlink;
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("secret.txt"), "beta\n").unwrap();
        std::fs::write(dir.path().join("real.txt"), "beta\n").unwrap();
        symlink(
            outside.path().join("secret.txt"),
            dir.path().join("linked.txt"),
        )
        .unwrap();
        symlink(outside.path(), dir.path().join("linked_dir")).unwrap();
        let tool = tool(dir.path());

        let outcome = execute(&tool, r#"{"pattern": "beta"}"#).await;

        assert_eq!(outcome.status, ToolStatus::Ok);
        assert_eq!(outcome.content, "real.txt\n1:beta");
    }

    #[tokio::test]
    async fn path_scopes_the_search_and_stays_root_relative() {
        let dir = tempfile::tempdir().unwrap();
        must_pass_tree(dir.path());
        std::fs::write(dir.path().join("top.rs"), "fn beta() {}\n").unwrap();
        let tool = tool(dir.path());

        let outcome = execute(&tool, r#"{"pattern": "beta", "path": "src"}"#).await;

        assert_eq!(
            outcome.content,
            "src/a.rs\n2:fn beta() {}\n\nsrc/b.rs\n1:fn beta() {}"
        );
    }

    #[tokio::test]
    async fn a_missing_search_path_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let tool = tool(dir.path());

        let outcome = execute(&tool, r#"{"pattern": "x", "path": "nope"}"#).await;

        assert_eq!(outcome.status, ToolStatus::Error);
        assert_eq!(outcome.content, "nope does not exist.");
    }

    #[test]
    fn declaration_is_a_function_with_the_spec_schema() {
        let dir = tempfile::tempdir().unwrap();
        let tool = tool(dir.path());

        assert_eq!(tool.declaration().name, "grep");
        let schema = schema(&tool);
        assert_eq!(schema["type"], "object");
        assert_eq!(schema["required"], serde_json::json!(["pattern"]));
        assert_eq!(schema["additionalProperties"], false);
        assert_eq!(schema["properties"]["pattern"]["type"], "string");
        assert_eq!(schema["properties"]["path"]["type"], "string");
        assert_eq!(schema["properties"]["glob"]["type"], "string");
        assert_eq!(
            schema["properties"]["mode"]["enum"],
            serde_json::json!(["content", "files"])
        );
        assert_eq!(schema["properties"]["mode"]["default"], "content");
        assert_eq!(schema["properties"]["case_insensitive"]["default"], false);
        assert_eq!(schema["properties"]["context"]["minimum"], 0);
        assert_eq!(schema["properties"]["context"]["maximum"], 10);
        assert_eq!(schema["properties"]["context"]["default"], 0);
        assert!(
            !schema["properties"]["pattern"]["description"]
                .as_str()
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn identity_defaults_to_claude_and_survives_a_face_change() {
        let dir = tempfile::tempdir().unwrap();
        let tool = tool(dir.path());
        assert_eq!(tool.identity().implementation, "p1-tool-search");
        assert_eq!(tool.identity().variant, "claude");

        let reshaped = tool.with_face(ToolFace::new("Search", "custom"), "gpt");
        assert_eq!(reshaped.declaration().name, "Search");
        assert_eq!(reshaped.declaration().description, "custom");
        assert_eq!(reshaped.identity().implementation, "p1-tool-search");
        assert_eq!(reshaped.identity().variant, "gpt");
    }

    #[test]
    fn effect_is_read_only() {
        let dir = tempfile::tempdir().unwrap();
        let tool = tool(dir.path());
        assert_eq!(tool.effect(&call("{}")), Effect::ReadOnly);
    }

    /// ADR-0057: the pattern searched, or the scope when the pattern is empty.
    #[test]
    fn describe_names_the_pattern_or_the_scope() {
        let dir = tempfile::tempdir().unwrap();
        let tool = tool(dir.path());
        let described = tool.describe(&call(r#"{"pattern": "beta", "path": "src"}"#));
        assert_eq!(described.verb, "search");
        assert_eq!(described.target.as_deref(), Some("beta src"));
        assert!(!described.destructive);
        assert_eq!(
            tool.describe(&call(r#"{"pattern": "beta"}"#))
                .target
                .as_deref(),
            Some("beta .")
        );
        let scoped = tool.describe(&call(r#"{"pattern": "", "path": "src"}"#));
        assert_eq!(scoped.target.as_deref(), Some(" src"));
    }

    #[tokio::test]
    async fn invalid_input_reports_a_prefix_and_never_panics() {
        let dir = tempfile::tempdir().unwrap();
        let tool = tool(dir.path());
        let garbage = [
            "",
            "null",
            "[]",
            "{}",
            "{\"pattern\": 5}",
            "{\"pattern\":\"a\",\"unknown\":1}",
            "{\"pattern\":\"a\",\"context\":11}",
            "{\"pattern\":\"a\",\"context\":-1}",
            "{\"pattern\":\"a\",\"mode\":\"count\"}",
            "\u{0}\u{1}{\"pattern\" garbage",
        ];
        for arguments in garbage {
            let outcome = execute(&tool, arguments).await;
            assert_eq!(outcome.status, ToolStatus::Error, "input: {arguments:?}");
            assert!(
                outcome.content.starts_with("Invalid input for grep: "),
                "input: {arguments:?} -> {outcome:?}"
            );
        }
    }

    #[tokio::test]
    async fn text_input_is_invalid_input() {
        let dir = tempfile::tempdir().unwrap();
        let tool = tool(dir.path());
        let call = ToolCall {
            call_id: "call-1".into(),
            name: "grep".into(),
            input: ToolInput::Text("pattern=beta".into()),
        };
        let context = ToolContext {
            cancel: CancellationToken::new(),
        };

        let outcome = tool.execute(&call, context).await;

        assert_eq!(outcome.status, ToolStatus::Error);
        assert!(outcome.content.starts_with("Invalid input for grep: "));
    }

    #[tokio::test]
    async fn execute_returns_cancelled_without_touching_the_filesystem() {
        let dir = tempfile::tempdir().unwrap();
        let tool = tool(dir.path());
        let call = call(r#"{"pattern": "beta"}"#);
        let cancel = CancellationToken::new();
        cancel.cancel();

        let outcome = tool.execute(&call, ToolContext { cancel }).await;

        assert_eq!(outcome.status, ToolStatus::Cancelled);
        assert_eq!(outcome.content, "");
    }

    #[test]
    fn parse_input_rejects_a_freeform_text_call() {
        let call = ToolCall {
            call_id: "c".into(),
            name: "grep".into(),
            input: ToolInput::Text("anything".into()),
        };
        assert!(parse_input("grep", &call).is_err());
    }

    /// `within_bound` must be exactly the set of results `bound_output` hands
    /// back unchanged, or a result the tool kept would gain the shared footer.
    #[test]
    fn within_bound_matches_bound_output_for_footer_terminated_results() {
        let mut texts = vec![
            String::new(),
            "a".to_string(),
            "a\nb".to_string(),
            "a\nb\nc".to_string(),
        ];
        // Around both limits, always without a trailing newline.
        for lines in [MAX_OUTPUT_LINES - 1, MAX_OUTPUT_LINES, MAX_OUTPUT_LINES + 1] {
            texts.push("x\n".repeat(lines) + "x");
        }
        for bytes in [MAX_OUTPUT_BYTES - 1, MAX_OUTPUT_BYTES, MAX_OUTPUT_BYTES + 1] {
            texts.push("x".repeat(bytes));
        }
        // `.as_str()`: a crate in the dependency graph adds another `Add` impl for `String`,
        // so `+ &String` no longer coerces to `&str` by inference.
        texts.push("x\n".repeat(MAX_OUTPUT_LINES - 1) + "x".repeat(MAX_OUTPUT_BYTES).as_str());

        for text in &texts {
            assert_eq!(
                within_bound(text.len(), newlines(text)),
                bound_output(text, MAX_OUTPUT_BYTES, MAX_OUTPUT_LINES) == *text,
                "{} bytes, {} newlines",
                text.len(),
                newlines(text)
            );
        }
    }
}
