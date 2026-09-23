//! The `grep` tool: regex search over the workspace with the ripgrep library
//! crates.
//!
//! `ignore` provides the `.gitignore`-aware walk and the glob filter, and
//! `grep` (regex + searcher) does the matching, so no `rg` binary is needed.
//! Confinement lives in `p1-workspace`; this module owns the declaration, input
//! validation, the grouped rendering and the bounding of its own result — the
//! shared output bound is applied to whole file blocks, and a footer says what
//! is missing.

use std::io;
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
use p1_workspace::{ToolFace, Workspace};
use serde::Deserialize;

const NAME: &str = "grep";
const DESCRIPTION: &str = "Search workspace files with a regular expression.\n`mode:\"content\"` (default) groups matching lines by file, with up to `context` surrounding lines; `mode:\"files\"` lists the matching paths, or every file matching `glob` when `pattern` is empty.\nHonours .gitignore, skips hidden and binary files, and never follows symlinks.";
/// The shared output bound (`bound_output`'s defaults). `grep` bounds its own
/// result to it, so the bound is also part of this crate's interface.
pub const MAX_OUTPUT_BYTES: usize = 50_000;
pub const MAX_OUTPUT_LINES: usize = 2_000;
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

    /// ADR-0057: the pattern and scope searched, from the tool's own parsed
    /// input.
    fn describe(&self, call: &ToolCall) -> CallDescription {
        CallDescription {
            verb: "search",
            target: parse_input(&self.declaration.name, call).ok().map(|input| {
                let scope = input.path.unwrap_or_else(|| ".".to_string());
                format!("{} {scope}", input.pattern)
            }),
            edit: None,
            destructive: false,
        }
    }

    fn describe_result(
        &self,
        call: &ToolCall,
        result: &p1_contracts::ToolResultItem,
    ) -> ResultDescription {
        if result.status != ToolStatus::Ok {
            return plain_result(result);
        }
        let files_mode =
            parse_input(&self.declaration.name, call).is_ok_and(|input| input.mode == Mode::Files);
        let (count, files) = describe_matches(&result.content, files_mode);
        let summary = if files_mode {
            format!("{} files", files.len())
        } else {
            format!("{count} hits · {} files", files.len())
        };
        ResultDescription {
            summary,
            detail: Some(ResultDetail::Matches { count, files }),
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
            let workspace = self.workspace.clone();
            let cancel = context.cancel.clone();
            let tool = self.declaration.name.clone();
            // All filesystem work runs on a blocking thread; the async thread is
            // never used for synchronous I/O.
            match tokio::task::spawn_blocking(move || run(&workspace, &input, &cancel)).await {
                // `run` bounds its own rendering, so the footer that says what
                // is missing survives.
                Ok(Ok(content)) => ToolOutcome::ok(content),
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

fn plain_result(result: &p1_contracts::ToolResultItem) -> ResultDescription {
    ResultDescription {
        summary: result
            .content
            .lines()
            .next()
            .unwrap_or_default()
            .to_string(),
        detail: None,
    }
}

fn describe_matches(content: &str, files_mode: bool) -> (usize, Vec<String>) {
    if content.trim() == "No matches." {
        return (0, Vec::new());
    }
    if files_mode {
        let files = content.lines().map(str::to_string).collect::<Vec<_>>();
        return (files.len(), files);
    }
    let blocks = content.split("\n\n").collect::<Vec<_>>();
    let count = blocks
        .iter()
        .map(|block| block.lines().count().saturating_sub(1))
        .sum();
    let files = blocks
        .iter()
        .filter_map(|block| block.lines().next())
        .map(str::to_string)
        .collect();
    (count, files)
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
            Ok(render_files(&matched))
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
    Ok(render_files(&matched))
}

/// One rendered result unit — a whole file block in content mode, one path in
/// files mode — with the path a footer can name and the newlines its text
/// contains (the units the shared bound counts).
struct Block<'a> {
    path: &'a str,
    text: String,
    newlines: usize,
}

fn render_content(groups: &[FileHits]) -> String {
    let blocks: Vec<Block<'_>> = groups
        .iter()
        .map(|group| Block {
            path: group.path.as_str(),
            text: render_block(group),
            newlines: group.hits.len(),
        })
        .collect();
    let joined = join_blocks(&blocks, "\n\n");
    if within_bound(joined.len(), newlines(&joined)) {
        return joined;
    }
    match keep_whole_blocks(&blocks, "\n\n") {
        Some(bounded) => bounded,
        // Not even the first file's block fits.
        None => cut_first_block(&groups[0], groups.len() - 1),
    }
}

fn render_files(matched: &[String]) -> String {
    if matched.is_empty() {
        return "No matches.".to_string();
    }
    let blocks: Vec<Block<'_>> = matched
        .iter()
        .map(|path| Block {
            path: path.as_str(),
            text: path.clone(),
            newlines: 0,
        })
        .collect();
    let joined = join_blocks(&blocks, "\n");
    if within_bound(joined.len(), newlines(&joined)) {
        return joined;
    }
    match keep_whole_blocks(&blocks, "\n") {
        Some(bounded) => bounded,
        // A single path is far shorter than the bound, so this is unreachable;
        // keeping it whole is the only honest answer if it ever happened.
        None => blocks[0].text.clone(),
    }
}

/// One file's block: the path on its own line, then a hit line per match or
/// context line.
fn render_block(group: &FileHits) -> String {
    let mut block = String::with_capacity(group.path.len());
    block.push_str(&group.path);
    for hit in &group.hits {
        block.push('\n');
        push_hit(&mut block, hit);
    }
    block
}

fn push_hit(out: &mut String, hit: &Hit) {
    let separator = if hit.is_match { ':' } else { '-' };
    out.push_str(&format!("{}{separator}{}", hit.line, hit.text));
}

fn join_blocks(blocks: &[Block<'_>], separator: &str) -> String {
    let mut out = String::new();
    for (index, block) in blocks.iter().enumerate() {
        if index > 0 {
            out.push_str(separator);
        }
        out.push_str(&block.text);
    }
    out
}

fn newlines(text: &str) -> usize {
    text.matches('\n').count()
}

/// The shared output bound, for a result that ends with the footer line and so
/// has no trailing newline: at most `MAX_OUTPUT_BYTES` bytes and fewer than
/// `MAX_OUTPUT_LINES` newlines. This is exactly the set of results
/// `bound_output` hands back unchanged.
fn within_bound(bytes: usize, newlines: usize) -> bool {
    bytes <= MAX_OUTPUT_BYTES && newlines < MAX_OUTPUT_LINES
}

/// Keep whole blocks, in walk order, while each one still leaves room for the
/// footer that replaces everything after it. `None` when not even the first
/// block fits.
fn keep_whole_blocks(blocks: &[Block<'_>], separator: &str) -> Option<String> {
    let separator_newlines = newlines(separator);
    let mut kept = String::new();
    let mut kept_newlines = 0;
    let mut last = None;
    for (index, block) in blocks.iter().enumerate() {
        let before = kept.len();
        if index > 0 {
            kept.push_str(separator);
            kept_newlines += separator_newlines;
        }
        kept.push_str(&block.text);
        kept_newlines += block.newlines;
        let footer = footer_after(block.path, blocks.len() - index - 1);
        if !within_bound(kept.len() + 1 + footer.len(), kept_newlines + 1) {
            kept.truncate(before);
            break;
        }
        last = Some(index);
    }
    let index = last?;
    Some(format!(
        "{kept}\n{}",
        footer_after(blocks[index].path, blocks.len() - index - 1)
    ))
}

/// The first block alone is over the bound: show its path line and as many
/// whole hit lines as fit, and name the last line shown. A line that does not
/// fit is dropped, so the cut stays at a line boundary; only the first line has
/// no boundary before it, and when it alone is larger than the whole bound its
/// text is cut instead — on a character boundary, like every other bounded
/// tool.
fn cut_first_block(group: &FileHits, more_files: usize) -> String {
    let path = group.path.as_str();
    let mut body = path.to_string();
    let mut body_newlines = 0;
    let mut shown_line = None;
    for hit in &group.hits {
        let footer = footer_inside(path, hit.line, more_files);
        let mut line = String::new();
        push_hit(&mut line, hit);
        let mut candidate = String::with_capacity(body.len() + 1 + line.len());
        candidate.push_str(&body);
        candidate.push('\n');
        candidate.push_str(&line);
        if within_bound(candidate.len() + 1 + footer.len(), body_newlines + 2) {
            body = candidate;
            body_newlines += 1;
            shown_line = Some(hit.line);
            continue;
        }
        if shown_line.is_none()
            && let Some(prefix) = cut_to_fit(&body, body_newlines, &line, &footer)
        {
            body.push('\n');
            body.push_str(&prefix);
            shown_line = Some(hit.line);
        }
        break;
    }
    // Every group has at least one hit. When even the path line fills the
    // bound, the footer still names the first line that did not fit.
    let line = shown_line.unwrap_or_else(|| group.hits.first().map_or(0, |hit| hit.line));
    format!("{body}\n{}", footer_inside(path, line, more_files))
}

/// The longest character-boundary prefix of one rendered `line` that still
/// leaves room for `footer` after `body`, or `None` when none does.
fn cut_to_fit(body: &str, body_newlines: usize, line: &str, footer: &str) -> Option<String> {
    let room = MAX_OUTPUT_BYTES.saturating_sub(body.len() + 2 + footer.len());
    let mut end = room.min(line.len());
    while end > 0 && !line.is_char_boundary(end) {
        end -= 1;
    }
    if end == 0 || !within_bound(body.len() + 1 + end + 1 + footer.len(), body_newlines + 2) {
        return None;
    }
    Some(line[..end].to_string())
}

/// The footer when whole blocks were kept: the last path shown, and how many
/// matching files follow it.
fn footer_after(last_path: &str, more_files: usize) -> String {
    format!(
        "[truncated after {last_path}; {more_files} more matching files not shown; narrow with path or glob]"
    )
}

/// The footer when even the first block did not fit: the path, the last line
/// shown inside it, and how many matching files follow it.
fn footer_inside(path: &str, line: u64, more_files: usize) -> String {
    format!(
        "[truncated inside {path} after line {line}; {more_files} more matching files not shown; narrow with path, glob or a stricter pattern]"
    )
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
        texts.push("x\n".repeat(MAX_OUTPUT_LINES - 1) + &"x".repeat(MAX_OUTPUT_BYTES));

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
