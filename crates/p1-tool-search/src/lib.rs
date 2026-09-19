//! The `grep` tool: regex search over the workspace with the ripgrep library
//! crates.
//!
//! `ignore` provides the `.gitignore`-aware walk and the glob filter, and
//! `grep` (regex + searcher) does the matching, so no `rg` binary is needed.
//! Confinement and output bounding live in `p1-workspace`; this module owns the
//! declaration, input validation and the grouped rendering.

use std::io;
use std::path::{Path, PathBuf};

use grep::regex::{RegexMatcher, RegexMatcherBuilder};
use grep::searcher::{
    BinaryDetection, MmapChoice, Searcher, SearcherBuilder, Sink, SinkContext, SinkMatch,
};
use ignore::WalkBuilder;
use ignore::overrides::{Override, OverrideBuilder};
use p1_contracts::{
    BoxFuture, CancellationToken, DeclarationKind, Effect, Tool, ToolCall, ToolContext,
    ToolDeclaration, ToolIdentity, ToolInput, ToolOutcome, ToolStatus,
};
use p1_workspace::{ToolFace, Workspace, bound_output};
use serde::Deserialize;

const NAME: &str = "grep";
const DESCRIPTION: &str = "Search workspace files with a regular expression.\n`mode:\"content\"` (default) groups matching lines by file, with up to `context` surrounding lines; `mode:\"files\"` lists the matching paths, or every file matching `glob` when `pattern` is empty.\nHonours .gitignore, skips hidden and binary files, and never follows symlinks.";
const MAX_OUTPUT_BYTES: usize = 50_000;
const MAX_OUTPUT_LINES: usize = 2_000;
const DEFAULT_CONTEXT: usize = 0;
const MAX_CONTEXT: usize = 10;
/// A NUL anywhere in this prefix marks a file as binary when listing files.
const BINARY_SNIFF_BYTES: usize = 8 * 1024;

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
            "pattern": {
                "type": "string",
                "description": "Regular expression to search for."
            },
            "path": {
                "type": "string",
                "description": "File or directory to search, relative to the workspace root or absolute inside it. Defaults to the workspace root."
            },
            "glob": {
                "type": "string",
                "description": "Only search files matching this glob, e.g. `*.rs` or `src/**/*.md`."
            },
            "mode": {
                "type": "string",
                "enum": ["content", "files"],
                "default": "content",
                "description": "`content` returns matching lines; `files` returns matching paths."
            },
            "case_insensitive": {
                "type": "boolean",
                "default": false,
                "description": "Match case-insensitively."
            },
            "context": {
                "type": "integer",
                "minimum": 0,
                "maximum": 10,
                "default": 0,
                "description": "Lines of context shown before and after each match."
            }
        },
        "required": ["pattern"],
        "additionalProperties": false
    })
}

#[derive(Debug, Clone, Copy, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum Mode {
    #[default]
    Content,
    Files,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct GrepInput {
    pattern: String,
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    glob: Option<String>,
    #[serde(default)]
    mode: Mode,
    #[serde(default)]
    case_insensitive: bool,
    #[serde(default)]
    context: Option<i64>,
}

/// Why a search did not produce a rendered result.
enum SearchFailure {
    /// A message the model can act on.
    Message(String),
    /// The cancellation token was set; nothing further was touched.
    Cancelled,
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
            let cancel = context.cancel.clone();
            let tool = self.declaration.name.clone();
            // All filesystem work runs on a blocking thread; the async thread is
            // never used for synchronous I/O.
            match tokio::task::spawn_blocking(move || run(&workspace, &input, &cancel)).await {
                Ok(Ok(content)) => {
                    ToolOutcome::ok(bound_output(&content, MAX_OUTPUT_BYTES, MAX_OUTPUT_LINES))
                }
                Ok(Err(SearchFailure::Message(message))) => ToolOutcome::error(message),
                Ok(Err(SearchFailure::Cancelled)) => ToolOutcome {
                    status: ToolStatus::Cancelled,
                    content: String::new(),
                },
                Err(error) => ToolOutcome::error(format!("{tool} failed: {error}")),
            }
        })
    }
}

fn parse_input(tool: &str, call: &ToolCall) -> Result<GrepInput, String> {
    let raw = match &call.input {
        ToolInput::Json(raw) => raw,
        ToolInput::Text(_) => {
            return Err(invalid(
                tool,
                "expected a JSON object input, got freeform text",
            ));
        }
    };
    let input: GrepInput =
        serde_json::from_str(raw).map_err(|error| invalid(tool, &error.to_string()))?;
    if matches!(input.context, Some(context) if !(0..=MAX_CONTEXT as i64).contains(&context)) {
        return Err(invalid(tool, "`context` must be between 0 and 10"));
    }
    Ok(input)
}

fn invalid(tool: &str, reason: &str) -> String {
    format!("Invalid input for {tool}: {reason}")
}

/// One rendered line within a file group.
struct Hit {
    line: u64,
    is_match: bool,
    text: String,
}

/// The matches of a single file, in the order the searcher emitted them.
struct FileHits {
    path: String,
    hits: Vec<Hit>,
}

fn run(
    workspace: &Workspace,
    input: &GrepInput,
    cancel: &CancellationToken,
) -> Result<String, SearchFailure> {
    let search_path = match &input.path {
        Some(requested) => workspace
            .resolve(requested)
            .map_err(|error| SearchFailure::Message(error.to_string()))?,
        None => workspace.root().to_path_buf(),
    };
    if !search_path.exists() {
        return Err(SearchFailure::Message(format!(
            "{} does not exist.",
            workspace.display(&search_path)
        )));
    }

    let context = input.context.unwrap_or(DEFAULT_CONTEXT as i64) as usize;
    let matcher = RegexMatcherBuilder::new()
        .case_insensitive(input.case_insensitive)
        .build(&input.pattern)
        .map_err(|error| SearchFailure::Message(format!("invalid regex pattern: {error}")))?;
    let overrides = build_overrides(&search_path, input.glob.as_deref())?;
    let files = collect_files(workspace, &search_path, overrides, cancel)?;

    match input.mode {
        Mode::Content => search_content(&matcher, context, &files, cancel),
        Mode::Files if input.pattern.is_empty() => {
            // With an empty pattern and a glob, the walk itself is the result.
            let matched: Vec<String> = files
                .into_iter()
                .filter(|(_, path)| !looks_binary(path))
                .map(|(display, _)| display)
                .collect();
            Ok(render_files(matched))
        }
        Mode::Files => search_files(&matcher, &files, cancel),
    }
}

fn build_overrides(
    search_path: &Path,
    glob: Option<&str>,
) -> Result<Option<Override>, SearchFailure> {
    match glob {
        Some(glob) => {
            let mut builder = OverrideBuilder::new(search_path);
            builder.add(glob).map_err(|error| {
                SearchFailure::Message(format!("invalid glob pattern: {error}"))
            })?;
            let overrides = builder.build().map_err(|error| {
                SearchFailure::Message(format!("invalid glob pattern: {error}"))
            })?;
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
) -> Result<Vec<(String, PathBuf)>, SearchFailure> {
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
            return Err(SearchFailure::Cancelled);
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
    context: usize,
    files: &[(String, PathBuf)],
    cancel: &CancellationToken,
) -> Result<String, SearchFailure> {
    let mut searcher = content_searcher(context);
    let mut groups = Vec::new();
    for (display, path) in files {
        if cancel.is_cancelled() {
            return Err(SearchFailure::Cancelled);
        }
        let mut sink = MatchSink::default();
        if searcher.search_path(matcher, path, &mut sink).is_err() {
            continue;
        }
        if sink.binary || sink.hits.is_empty() {
            continue;
        }
        groups.push(FileHits {
            path: display.clone(),
            hits: sink.hits,
        });
    }
    if groups.is_empty() {
        return Ok("No matches.".to_string());
    }
    Ok(render_content(&groups))
}

fn search_files(
    matcher: &RegexMatcher,
    files: &[(String, PathBuf)],
    cancel: &CancellationToken,
) -> Result<String, SearchFailure> {
    let mut searcher = plain_searcher();
    let mut matched = Vec::new();
    for (display, path) in files {
        if cancel.is_cancelled() {
            return Err(SearchFailure::Cancelled);
        }
        let mut sink = FirstMatchSink::default();
        if searcher.search_path(matcher, path, &mut sink).is_err() {
            continue;
        }
        if sink.binary {
            continue;
        }
        if sink.matched {
            matched.push(display.clone());
        }
    }
    Ok(render_files(matched))
}

fn render_content(groups: &[FileHits]) -> String {
    let mut blocks = Vec::with_capacity(groups.len());
    for group in groups {
        let mut block = group.path.clone();
        for hit in &group.hits {
            block.push('\n');
            let separator = if hit.is_match { ':' } else { '-' };
            block.push_str(&format!("{}{separator}{}", hit.line, hit.text));
        }
        blocks.push(block);
    }
    blocks.join("\n\n")
}

fn render_files(matched: Vec<String>) -> String {
    if matched.is_empty() {
        return "No matches.".to_string();
    }
    matched.join("\n")
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

fn plain_searcher() -> Searcher {
    let mut builder = SearcherBuilder::new();
    builder
        .line_number(true)
        .binary_detection(BinaryDetection::quit(b'\0'))
        .memory_map(MmapChoice::never());
    builder.build()
}

fn looks_binary(path: &Path) -> bool {
    match std::fs::File::open(path) {
        Ok(mut file) => {
            let mut prefix = [0u8; BINARY_SNIFF_BYTES];
            let read = std::io::Read::read(&mut file, &mut prefix).unwrap_or(0);
            prefix[..read].contains(&0)
        }
        Err(_) => false,
    }
}

/// Collects match and context lines for a single file.
#[derive(Default)]
struct MatchSink {
    hits: Vec<Hit>,
    binary: bool,
}

impl MatchSink {
    fn push(&mut self, number: Option<u64>, is_match: bool, bytes: &[u8]) {
        let text = String::from_utf8_lossy(bytes)
            .trim_end_matches(['\n', '\r'])
            .to_string();
        self.hits.push(Hit {
            line: number.unwrap_or(0),
            is_match,
            text,
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

/// Stops at the first match in a file, for `mode:"files"`.
#[derive(Default)]
struct FirstMatchSink {
    matched: bool,
    binary: bool,
}

impl Sink for FirstMatchSink {
    type Error = io::Error;

    fn matched(&mut self, _searcher: &Searcher, _mat: &SinkMatch<'_>) -> Result<bool, io::Error> {
        self.matched = true;
        Ok(false)
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
    use super::{GrepTool, parse_input};
    use p1_contracts::{
        CancellationToken, DeclarationKind, Effect, Tool, ToolCall, ToolContext, ToolInput,
        ToolOutcome, ToolStatus,
    };
    use p1_workspace::{ToolFace, Workspace};
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
}
