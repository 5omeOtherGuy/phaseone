//! The host side of the `workspace` interface's walk: `workspace.list-files` and
//! `workspace.search`, over the ripgrep library crates.
//!
//! `ignore` provides the `.gitignore`-aware walk and the glob filter, and `grep` (regex +
//! searcher) does the matching, so no `rg` binary is needed. Confinement lives in
//! `p1-workspace`: every path is resolved through [`p1_workspace::Workspace`] first. The walk
//! served two callers from the same code before the file tools became components — the native
//! `grep` and the `p1/search` component's capability service — and it serves them from here
//! now: `p1-tool-search`'s native `grep` runs it through the runtime's types and
//! [`crate::file_services::SearchCapability`] links it into a module, so native and component
//! walk the same way and word every failure the same way (the frozen `fs-error`).

use std::io::{self, Read};
use std::path::{Path, PathBuf};

use grep::regex::{RegexMatcher, RegexMatcherBuilder};
use grep::searcher::{
    BinaryDetection, MmapChoice, Searcher, SearcherBuilder, Sink, SinkContext, SinkMatch,
};
use ignore::WalkBuilder;
use ignore::overrides::{Override, OverrideBuilder};
use p1_contracts::CancellationToken;
use p1_workspace::{Workspace, WorkspaceError};

use crate::capabilities::{FileMatches, FsError, SearchLine, SearchQuery, SearchResult};

/// A workspace failure as the frozen `fs-error`: `io` carries the io error's own text, never
/// a host path. The walk's two callers word `io` that way — the native `grep` and the search
/// component's capability service; the read side, which words it as the native `read` does,
/// has its own mapping ([`crate::file_services`]).
pub(crate) fn workspace_error(error: WorkspaceError) -> FsError {
    match error {
        WorkspaceError::OutsideWorkspace { .. } => FsError::OutsideWorkspace,
        WorkspaceError::NotFound { .. } => FsError::NotFound,
        WorkspaceError::NotADirectory(_) => FsError::WrongKind,
        WorkspaceError::Io { source, .. } => FsError::Io(source.to_string()),
    }
}

/// The host side of `workspace.search`: search file contents under `query.path` (the root
/// when absent) with the walk of [`list_files`].
///
/// Matching files come in walk order, each with its match and context lines; binary files are
/// skipped. At most `query.max_lines` lines are carried: when one more would not fit, the
/// result is `truncated`, the rest of that file is dropped, and the walk goes on only to
/// count the matching files after it (`omitted_files`, which also counts a file the cap left
/// without a line). Every file is still searched whole, exactly as when nothing is cut, so a
/// file is a match here exactly when it would be one in a complete result.
///
/// The failures are the frozen `fs-error`: `outside-workspace`, `not-found` for a missing
/// path, `invalid-pattern` with the model-facing text for a regex or glob that does not parse
/// (in that order), and `cancelled` when `cancel` is set during the walk or the search.
pub fn search(
    workspace: &Workspace,
    query: &SearchQuery,
    cancel: &CancellationToken,
) -> Result<SearchResult, FsError> {
    search_excluding(workspace, query, cancel, |_| false)
}

/// Search like [`search`], omitting paths selected by the caller before reading their contents.
pub(crate) fn search_excluding(
    workspace: &Workspace,
    query: &SearchQuery,
    cancel: &CancellationToken,
    excluded: impl Fn(&Path) -> bool,
) -> Result<SearchResult, FsError> {
    let search_path = scope(workspace, query.path.as_deref())?;
    let matcher = RegexMatcherBuilder::new()
        .case_insensitive(query.case_insensitive)
        .build(&query.pattern)
        .map_err(|error| FsError::InvalidPattern(format!("invalid regex pattern: {error}")))?;
    let overrides = build_overrides(&search_path, query.glob.as_deref())?;
    let mut files = collect_files(workspace, &search_path, overrides, cancel)?;
    files.retain(|(_, path)| !excluded(path));
    search_content(&matcher, query, &files, cancel)
}

/// The host side of `workspace.list-files`: the files under `path` (a directory, or one
/// file), sorted bytewise, relative to the root. The walk honours `.gitignore`, skips hidden
/// entries and never follows symlinks; `glob` keeps only matching files.
pub fn list_files(
    workspace: &Workspace,
    path: &str,
    glob: Option<&str>,
    cancel: &CancellationToken,
) -> Result<Vec<String>, FsError> {
    list_files_excluding(workspace, path, glob, cancel, |_| false)
}

/// List like [`list_files`], omitting paths selected by the caller.
pub(crate) fn list_files_excluding(
    workspace: &Workspace,
    path: &str,
    glob: Option<&str>,
    cancel: &CancellationToken,
    excluded: impl Fn(&Path) -> bool,
) -> Result<Vec<String>, FsError> {
    let search_path = scope(workspace, Some(path))?;
    let overrides = build_overrides(&search_path, glob)?;
    let mut files = collect_files(workspace, &search_path, overrides, cancel)?;
    files.retain(|(_, path)| !excluded(path));
    Ok(files.into_iter().map(|(display, _)| display).collect())
}

/// One window of the file, read directly: the guest reads only the prefix it sniffs for
/// binary content, so the whole file is never loaded. The native `grep` and the search
/// component's capability service (a window of `workspace.read`) share it, so both read the
/// same bytes for the same request.
pub fn read_window(
    workspace: &Workspace,
    path: &str,
    offset: u64,
    length: u64,
) -> Result<Vec<u8>, FsError> {
    let checked = workspace.check_path(path).map_err(workspace_error)?;
    let io = |error: io::Error| FsError::Io(error.to_string());
    let mut file = std::fs::File::open(checked.path()).map_err(io)?;
    io::copy(&mut (&mut file).take(offset), &mut io::sink()).map_err(io)?;
    let mut window = Vec::new();
    file.take(length).read_to_end(&mut window).map_err(io)?;
    Ok(window)
}

/// Resolve the path to search (the root when absent); it must exist.
fn scope(workspace: &Workspace, requested: Option<&str>) -> Result<PathBuf, FsError> {
    let search_path = match requested {
        Some(requested) => workspace.resolve(requested).map_err(workspace_error)?,
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

/// Walk the search path and return `(root-relative display, absolute path)` pairs, sorted
/// bytewise by display path. Hidden entries are skipped by the walker, symlinks are never
/// followed, and binary files are filtered by the callers that read content.
fn collect_files(
    workspace: &Workspace,
    search_path: &Path,
    overrides: Option<Override>,
    cancel: &CancellationToken,
) -> Result<Vec<(String, PathBuf)>, FsError> {
    let mut walk = WalkBuilder::new(search_path);
    // Keep the walker defaults for hidden files (skip them) and follow_links (never); only
    // the gitignore handling is relaxed so a scratch directory without a `.git` still honours
    // its `.gitignore`.
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

/// Collects match and context lines for a single file, up to `room` of them. Past that it
/// keeps searching, carrying nothing, so a binary file is still recognised as one wherever
/// its NUL is.
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
