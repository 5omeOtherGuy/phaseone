//! The `grep` tool: regex search over the workspace with the ripgrep library
//! crates.
//!
//! `ignore` provides the `.gitignore`-aware walk and the glob filter, and
//! `grep` (regex + searcher) does the matching, so no `rg` binary is needed.
//! Confinement lives in `p1-workspace`. The walk and the matching are this
//! crate's [`search`] and [`list_files`], the host side of the `workspace`
//! interface's `search` and `list-files`. The declaration, input validation,
//! the grouped rendering with its own output bound and footers, and the
//! descriptions live in `p1-tool-search-logic`, the one copy the `grep`
//! component (`modules/p1-module-search/`) runs too: the native tool runs that
//! crate's `execute` over these two functions, so native and component run the
//! same code.

mod capability;

pub use capability::{SearchCapability, search_services};

use std::io::{self, Read};
use std::path::{Path, PathBuf};

use grep::regex::{RegexMatcher, RegexMatcherBuilder};
use grep::searcher::{
    BinaryDetection, MmapChoice, Searcher, SearcherBuilder, Sink, SinkContext, SinkMatch,
};
use ignore::WalkBuilder;
use ignore::overrides::{Override, OverrideBuilder};
use p1_contracts::tool::{ResultDescription, ResultDetail};
use p1_contracts::{
    BoxFuture, CallDescription, CancellationToken, DeclarationKind, Effect, Tool, ToolCall,
    ToolContext, ToolDeclaration, ToolIdentity, ToolInput, ToolOutcome, ToolStatus,
};
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
/// it sniffs for binary content, so the whole file is never loaded.
pub(crate) fn read_window(
    workspace: &Workspace,
    path: &str,
    offset: u64,
    length: u64,
) -> Result<Vec<u8>, FsError> {
    let checked = workspace.check_path(path).map_err(fs_error)?;
    let io = |error: io::Error| FsError::Io(error.to_string());
    let mut file = std::fs::File::open(checked.path()).map_err(io)?;
    io::copy(&mut (&mut file).take(offset), &mut io::sink()).map_err(io)?;
    let mut window = Vec::new();
    file.take(length).read_to_end(&mut window).map_err(io)?;
    Ok(window)
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

/// The host side of `workspace.search`: search file contents under
/// `query.path` (the root when absent) with the walk of [`list_files`].
///
/// Matching files come in walk order, each with its match and context lines;
/// binary files are skipped. At most `query.max_lines` lines are carried: when
/// one more would not fit, the result is `truncated`, the rest of that file is
/// dropped, and the walk goes on only to count the matching files after it
/// (`omitted_files`, which also counts a file the cap left without a line).
/// Every file is still searched whole, exactly as when nothing is cut, so a
/// file is a match here exactly when it would be one in a complete result.
///
/// The failures are the frozen `fs-error`: `outside-workspace`, `not-found`
/// for a missing path, `invalid-pattern` with the model-facing text for a
/// regex or glob that does not parse (in that order), and `cancelled` when
/// `cancel` is set during the walk or the search.
pub fn search(
    workspace: &Workspace,
    query: &SearchQuery,
    cancel: &CancellationToken,
) -> Result<SearchResult, FsError> {
    let search_path = scope(workspace, query.path.as_deref())?;
    let matcher = RegexMatcherBuilder::new()
        .case_insensitive(query.case_insensitive)
        .build(&query.pattern)
        .map_err(|error| FsError::InvalidPattern(format!("invalid regex pattern: {error}")))?;
    let overrides = build_overrides(&search_path, query.glob.as_deref())?;
    let files = collect_files(workspace, &search_path, overrides, cancel)?;
    search_content(&matcher, query, &files, cancel)
}

/// The host side of `workspace.list-files`: the files under `path` (a
/// directory, or one file), sorted bytewise, relative to the root. The walk
/// honours `.gitignore`, skips hidden entries and never follows symlinks;
/// `glob` keeps only matching files.
pub fn list_files(
    workspace: &Workspace,
    path: &str,
    glob: Option<&str>,
    cancel: &CancellationToken,
) -> Result<Vec<String>, FsError> {
    let search_path = scope(workspace, Some(path))?;
    let overrides = build_overrides(&search_path, glob)?;
    let files = collect_files(workspace, &search_path, overrides, cancel)?;
    Ok(files.into_iter().map(|(display, _)| display).collect())
}

/// Resolve the path to search (the root when absent); it must exist.
fn scope(workspace: &Workspace, requested: Option<&str>) -> Result<PathBuf, FsError> {
    let search_path = match requested {
        Some(requested) => workspace.resolve(requested).map_err(fs_error)?,
        None => workspace.root().to_path_buf(),
    };
    if !search_path.exists() {
        return Err(FsError::NotFound);
    }
    Ok(search_path)
}

fn build_overrides(search_path: &Path, glob: Option<&str>) -> Result<Option<Override>, FsError> {
    match glob {
        Some(glob) => {
            let invalid = |error: ignore::Error| {
                FsError::InvalidPattern(format!("invalid glob pattern: {error}"))
            };
            let mut builder = OverrideBuilder::new(search_path);
            builder.add(glob).map_err(invalid)?;
            let overrides = builder.build().map_err(invalid)?;
            Ok(Some(overrides))
        }
        None => Ok(None),
    }
}

/// Walk the search path and return `(root-relative display, absolute path)`
/// pairs, sorted bytewise by display path. Hidden entries are skipped by the
/// walker, symlinks are never followed, and binary files are filtered by the
/// callers that read content.
fn collect_files(
    workspace: &Workspace,
    search_path: &Path,
    overrides: Option<Override>,
    cancel: &CancellationToken,
) -> Result<Vec<(String, PathBuf)>, FsError> {
    let mut walk = WalkBuilder::new(search_path);
    // Keep the walker defaults for hidden files (skip them) and follow_links
    // (never); only the gitignore handling is relaxed so a scratch directory
    // without a `.git` still honours its `.gitignore`.
    walk.require_git(false);
    if let Some(overrides) = overrides {
        walk.overrides(overrides);
    }

    let mut files = Vec::new();
    for entry in walk.build() {
        if cancel.is_cancelled() {
            return Err(FsError::Cancelled);
        }
        let Ok(entry) = entry else { continue };
        // `is_file` is false for symlinks, so links are never followed.
        if !entry.file_type().is_some_and(|kind| kind.is_file()) {
            continue;
        }
        let path = entry.into_path();
        let display = workspace.display(&path);
        files.push((display, path));
    }
    files.sort_by(|a, b| a.0.as_bytes().cmp(b.0.as_bytes()));
    Ok(files)
}

fn search_content(
    matcher: &RegexMatcher,
    query: &SearchQuery,
    files: &[(String, PathBuf)],
    cancel: &CancellationToken,
) -> Result<SearchResult, FsError> {
    let mut searcher = content_searcher(query.context as usize);
    let mut result = SearchResult {
        files: Vec::new(),
        truncated: false,
        omitted_files: 0,
    };
    let mut room = query.max_lines as usize;
    for (display, path) in files {
        if cancel.is_cancelled() {
            return Err(FsError::Cancelled);
        }
        let mut sink = MatchSink::with_room(room);
        if searcher.search_path(matcher, path, &mut sink).is_err() {
            continue;
        }
        if sink.binary || !sink.seen {
            continue;
        }
        if sink.overflowed {
            result.truncated = true;
        }
        if sink.lines.is_empty() {
            result.omitted_files += 1;
            continue;
        }
        room -= sink.lines.len();
        result.files.push(FileMatches {
            path: display.clone(),
            lines: sink.lines,
        });
    }
    Ok(result)
}

fn content_searcher(context: usize) -> Searcher {
    let mut builder = SearcherBuilder::new();
    builder
        .line_number(true)
        .before_context(context)
        .after_context(context)
        .binary_detection(BinaryDetection::quit(b'\0'))
        .memory_map(MmapChoice::never());
    builder.build()
}

/// Collects match and context lines for a single file, up to `room` of them.
/// Past that it keeps searching, carrying nothing, so a binary file is still
/// recognised as one wherever its NUL is.
struct MatchSink {
    lines: Vec<SearchLine>,
    room: usize,
    /// Any match or context line was seen, carried or not.
    seen: bool,
    /// A line was seen with no room left for it.
    overflowed: bool,
    binary: bool,
}

impl MatchSink {
    fn with_room(room: usize) -> Self {
        Self {
            lines: Vec::new(),
            room,
            seen: false,
            overflowed: false,
            binary: false,
        }
    }

    fn push(&mut self, number: Option<u64>, is_match: bool, bytes: &[u8]) {
        self.seen = true;
        if self.lines.len() >= self.room {
            self.overflowed = true;
            return;
        }
        let text = String::from_utf8_lossy(bytes)
            .trim_end_matches(['\n', '\r'])
            .to_string();
        self.lines.push(SearchLine {
            line_number: number.unwrap_or(0),
            text,
            is_match,
        });
    }
}

impl Sink for MatchSink {
    type Error = io::Error;

    fn matched(&mut self, _searcher: &Searcher, mat: &SinkMatch<'_>) -> Result<bool, io::Error> {
        self.push(mat.line_number(), true, mat.bytes());
        Ok(true)
    }

    fn context(&mut self, _searcher: &Searcher, ctx: &SinkContext<'_>) -> Result<bool, io::Error> {
        self.push(ctx.line_number(), false, ctx.bytes());
        Ok(true)
    }

    fn binary_data(
        &mut self,
        _searcher: &Searcher,
        _binary_byte_offset: u64,
    ) -> Result<bool, io::Error> {
        self.binary = true;
        Ok(false)
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
