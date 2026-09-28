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
    search_excluding(workspace, query, cancel, |_| Ok(false))
}

/// Search like [`search`], omitting paths selected by the caller before reading their contents.
pub(crate) fn search_excluding(
    workspace: &Workspace,
    query: &SearchQuery,
    cancel: &CancellationToken,
    excluded: impl Fn(&Path) -> Result<bool, FsError>,
) -> Result<SearchResult, FsError> {
    let search_path = scope(workspace, query.path.as_deref())?;
    let matcher = RegexMatcherBuilder::new()
        .case_insensitive(query.case_insensitive)
        .build(&query.pattern)
        .map_err(|error| FsError::InvalidPattern(format!("invalid regex pattern: {error}")))?;
    search_streaming(
        workspace,
        &search_path,
        &matcher,
        query,
        cancel,
        &excluded,
        &|path, _| excluded(path),
    )
}

/// Like [`search_excluding`], with a fresh check on each opened file.
pub(crate) fn search_excluding_opened(
    workspace: &Workspace,
    query: &SearchQuery,
    cancel: &CancellationToken,
    excluded: impl Fn(&Path) -> Result<bool, FsError>,
    opened_excluded: impl Fn(&Path, &std::fs::File) -> Result<bool, FsError>,
) -> Result<SearchResult, FsError> {
    let search_path = scope(workspace, query.path.as_deref())?;
    let matcher = RegexMatcherBuilder::new()
        .case_insensitive(query.case_insensitive)
        .build(&query.pattern)
        .map_err(|error| FsError::InvalidPattern(format!("invalid regex pattern: {error}")))?;
    search_streaming(
        workspace,
        &search_path,
        &matcher,
        query,
        cancel,
        &excluded,
        &opened_excluded,
    )
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
    list_files_excluding(workspace, path, glob, cancel, |_| Ok(false))
}

/// List like [`list_files`], omitting paths selected by the caller.
pub(crate) fn list_files_excluding(
    workspace: &Workspace,
    path: &str,
    glob: Option<&str>,
    cancel: &CancellationToken,
    excluded: impl Fn(&Path) -> Result<bool, FsError>,
) -> Result<Vec<String>, FsError> {
    let search_path = scope(workspace, Some(path))?;
    let overrides = build_overrides(&search_path, glob)?;
    let files = collect_files(workspace, &search_path, overrides, cancel)?;
    let files = exclude_files(files, cancel, &excluded)?;
    Ok(files.into_iter().map(|(display, _)| display).collect())
}

/// Remove paths selected by the policy, stopping promptly if the request is cancelled.
fn exclude_files(
    files: Vec<(String, PathBuf)>,
    cancel: &CancellationToken,
    excluded: &impl Fn(&Path) -> Result<bool, FsError>,
) -> Result<Vec<(String, PathBuf)>, FsError> {
    let mut included = Vec::with_capacity(files.len());
    for file in files {
        if cancel.is_cancelled() {
            return Err(FsError::Cancelled);
        }
        let exclude = excluded(&file.1)?;
        if cancel.is_cancelled() {
            return Err(FsError::Cancelled);
        }
        if !exclude {
            included.push(file);
        }
    }
    Ok(included)
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
    read_window_inner(workspace, path, offset, length, None)
}

pub(crate) fn read_window_excluding(
    workspace: &Workspace,
    path: &str,
    offset: u64,
    length: u64,
    excluded: &impl Fn(&Path, &std::fs::File) -> Result<bool, FsError>,
) -> Result<Vec<u8>, FsError> {
    read_window_inner(workspace, path, offset, length, Some(excluded))
}

/// A check of an opened file: its real path and the open handle.
type OpenedCheck<'a> = &'a dyn Fn(&Path, &std::fs::File) -> Result<bool, FsError>;

fn read_window_inner(
    workspace: &Workspace,
    path: &str,
    offset: u64,
    length: u64,
    excluded: Option<OpenedCheck<'_>>,
) -> Result<Vec<u8>, FsError> {
    let checked = workspace.check_path(path).map_err(workspace_error)?;
    let io = |error: io::Error| FsError::Io(error.to_string());
    let mut file = workspace
        .open_file_at(checked.path())
        .map_err(workspace_error)?;
    if let Some(excluded) = excluded {
        let refusal = || {
            FsError::Io(p1_workspace::credential_refusal(
                &workspace.display(checked.path()),
            ))
        };
        let opened_path = opened_object_path(&file, checked.path()).map_err(|_| refusal())?;
        if excluded(&opened_path, &file)? {
            return Err(refusal());
        }
    }
    io::copy(&mut (&mut file).take(offset), &mut io::sink()).map_err(io)?;
    let mut window = Vec::new();
    file.take(length).read_to_end(&mut window).map_err(io)?;
    Ok(window)
}

/// The real path of the object `file` refers to, so policy checks see what was opened.
pub(crate) fn opened_object_path(file: &std::fs::File, opened_path: &Path) -> io::Result<PathBuf> {
    #[cfg(target_os = "linux")]
    {
        use std::os::fd::AsRawFd;
        let _ = opened_path;
        std::fs::read_link(format!("/proc/self/fd/{}", file.as_raw_fd()))
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = file;
        canonicalize_opened_path(opened_path)
    }
}

/// Fallback for systems without /proc: resolve the spelling only after the open.
#[cfg(any(test, not(target_os = "linux")))]
fn canonicalize_opened_path(opened_path: &Path) -> io::Result<PathBuf> {
    std::fs::canonicalize(opened_path)
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

/// Until the workspace interface offers paginated walking, refuse exceptionally large
/// listings rather than accumulating unbounded path data in host and guest memory.
const MAX_WALK_FILES: usize = 4_096;
const MAX_WALK_PATH_BYTES: usize = 512 * 1024;

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
    let mut path_bytes = 0usize;
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
        path_bytes = path_bytes
            .saturating_add(display.len())
            .saturating_add(path.as_os_str().len());
        if files.len() >= MAX_WALK_FILES || path_bytes > MAX_WALK_PATH_BYTES {
            return Err(FsError::Io(
                "workspace listing exceeds the bounded search budget; narrow with path or glob"
                    .into(),
            ));
        }
        files.push((display, path));
    }
    files.sort_by(|a, b| a.0.as_bytes().cmp(b.0.as_bytes()));
    Ok(files)
}

/// Search in a sorted streaming walk, keeping only the bounded match lines and
/// counting omitted matching files without retaining the complete path listing.
fn search_streaming(
    workspace: &Workspace,
    search_path: &Path,
    matcher: &RegexMatcher,
    query: &SearchQuery,
    cancel: &CancellationToken,
    excluded: &impl Fn(&Path) -> Result<bool, FsError>,
    opened_excluded: &impl Fn(&Path, &std::fs::File) -> Result<bool, FsError>,
) -> Result<SearchResult, FsError> {
    let overrides = build_overrides(search_path, query.glob.as_deref())?;
    let mut walk = WalkBuilder::new(search_path);
    walk.require_git(false);
    if let Some(overrides) = overrides {
        walk.overrides(overrides);
    }
    walk.sort_by_file_path(|left, right| left.as_os_str().cmp(right.as_os_str()));
    let mut searcher = content_searcher(query.context as usize);
    let mut result = SearchResult {
        files: Vec::new(),
        truncated: false,
        omitted_files: 0,
    };
    let mut room = query.max_lines as usize;
    for entry in walk.build() {
        if cancel.is_cancelled() {
            return Err(FsError::Cancelled);
        }
        let Ok(entry) = entry else { continue };
        if !entry.file_type().is_some_and(|kind| kind.is_file()) {
            continue;
        }
        let path = entry.path();
        if excluded(path)? {
            continue;
        }
        let Ok(file) = workspace.open_file_at(path) else {
            continue;
        };
        let Ok(opened_path) = opened_object_path(&file, path) else {
            continue;
        };
        if opened_excluded(&opened_path, &file)? {
            continue;
        }
        let mut sink = MatchSink::with_room(room);
        if searcher.search_file(matcher, &file, &mut sink).is_err() || sink.binary || !sink.seen {
            continue;
        }
        if sink.overflowed {
            result.truncated = true;
        }
        if sink.lines.is_empty() {
            result.omitted_files = result.omitted_files.saturating_add(1);
            result.truncated = true;
            continue;
        }
        room -= sink.lines.len();
        result.files.push(FileMatches {
            path: workspace.display(path),
            lines: sink.lines,
        });
    }
    Ok(result)
}

#[cfg(test)]
fn search_content(
    workspace: &Workspace,
    matcher: &RegexMatcher,
    query: &SearchQuery,
    files: &[(String, PathBuf)],
    cancel: &CancellationToken,
    excluded: impl Fn(&Path, &std::fs::File) -> Result<bool, FsError>,
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
        let Ok(file) = workspace.open_file_at(path) else {
            continue;
        };
        let Ok(opened_path) = opened_object_path(&file, path) else {
            continue;
        };
        if excluded(&opened_path, &file)? {
            continue;
        }
        if searcher.search_file(matcher, &file, &mut sink).is_err() {
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

#[cfg(test)]
mod tests {
    use super::{canonicalize_opened_path, collect_files, search_content, search_excluding_opened};
    use grep::regex::RegexMatcher;
    use p1_contracts::CancellationToken;
    use p1_workspace::{CredentialPolicy, Workspace};
    use std::ffi::OsStr;
    use std::fs;
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::symlink;

    use crate::capabilities::SearchQuery;

    #[test]
    fn listing_refuses_before_collecting_unbounded_paths() {
        let dir = tempfile::tempdir().unwrap();
        for i in 0..=super::MAX_WALK_FILES {
            fs::write(dir.path().join(format!("f{i:05}")), b"text").unwrap();
        }
        let workspace = Workspace::new(dir.path()).unwrap();
        let cancel = CancellationToken::new();
        let error = super::list_files(&workspace, ".", None, &cancel).unwrap_err();
        assert!(
            matches!(error, crate::capabilities::FsError::Io(message) if message.contains("bounded search budget"))
        );
    }

    #[test]
    fn broad_content_search_streams_past_listing_budget() {
        let dir = tempfile::tempdir().unwrap();
        for i in 0..=super::MAX_WALK_FILES {
            fs::write(
                dir.path().join(format!("f{i:05}")),
                if i == 0 { "needle" } else { "other" },
            )
            .unwrap();
        }
        let workspace = Workspace::new(dir.path()).unwrap();
        let query = SearchQuery {
            pattern: "needle".into(),
            path: None,
            glob: None,
            case_insensitive: false,
            context: 0,
            max_lines: 2,
        };
        let result = super::search(&workspace, &query, &CancellationToken::new()).unwrap();
        assert_eq!(result.files.len(), 1);
        assert_eq!(result.files[0].path, "f00000");
        // Files mode starts with this same capped host search; no full listing is
        // needed when its one matching path already fits in the carried prefix.
        assert_eq!(result.omitted_files, 0);
    }

    #[test]
    fn search_finds_file_with_invalid_utf8_name() {
        let workspace_dir = tempfile::tempdir().unwrap();
        let name = OsStr::from_bytes(b"invalid-\xff.txt");
        fs::write(
            workspace_dir.path().join(name),
            "needle in non-UTF-8 name\n",
        )
        .unwrap();
        let workspace = Workspace::new(workspace_dir.path()).unwrap();
        let cancel = CancellationToken::new();
        let files = collect_files(&workspace, workspace.root(), None, &cancel).unwrap();
        let query = SearchQuery {
            pattern: "needle".into(),
            path: None,
            glob: None,
            case_insensitive: false,
            context: 0,
            max_lines: 10,
        };
        let matcher = RegexMatcher::new("needle").unwrap();

        let result = search_content(&workspace, &matcher, &query, &files, &cancel, |_, _| {
            Ok(false)
        })
        .unwrap();

        assert_eq!(result.files.len(), 1);
        assert_eq!(result.files[0].lines[0].text, "needle in non-UTF-8 name");
    }

    #[test]
    fn search_rechecks_credentials_after_opening_swapped_symlink() {
        let home = tempfile::tempdir().unwrap();
        let credential_path = home.path().join(".codex/auth.json");
        fs::create_dir_all(credential_path.parent().unwrap()).unwrap();
        fs::write(&credential_path, "needle in credential\n").unwrap();
        let workspace = Workspace::new(home.path()).unwrap();
        let policy = CredentialPolicy::new(Some(home.path()), &[]);
        let notes_path = home.path().join("notes.txt");
        fs::write(&notes_path, "needle in notes\n").unwrap();
        let cancel = CancellationToken::new();
        let files = collect_files(&workspace, workspace.root(), None, &cancel).unwrap();

        fs::remove_file(&notes_path).unwrap();
        symlink(&credential_path, &notes_path).unwrap();

        let query = SearchQuery {
            pattern: "needle".into(),
            path: None,
            glob: None,
            case_insensitive: false,
            context: 0,
            max_lines: 10,
        };
        let matcher = RegexMatcher::new("needle").unwrap();
        let result = search_content(
            &workspace,
            &matcher,
            &query,
            &files,
            &cancel,
            |candidate, _| Ok(policy.refuses(candidate)),
        )
        .unwrap();

        assert!(
            result.files.is_empty(),
            "opened credential symlink was searched"
        );
    }

    #[test]
    fn canonical_fallback_checks_credential_and_ordinary_paths() {
        let home = tempfile::tempdir().unwrap();
        let credential = home.path().join(".codex/auth.json");
        fs::create_dir_all(credential.parent().unwrap()).unwrap();
        fs::write(&credential, "fixture").unwrap();
        let ordinary = home.path().join("notes.txt");
        fs::write(&ordinary, "ordinary").unwrap();
        let policy = CredentialPolicy::new(Some(home.path()), &[]);
        assert!(policy.refuses(&canonicalize_opened_path(&credential).unwrap()));
        assert!(!policy.refuses(&canonicalize_opened_path(&ordinary).unwrap()));
    }

    #[test]
    fn search_refreshes_policy_after_config_retarget() {
        let home = tempfile::tempdir().unwrap();
        let old = home.path().join("old");
        let new = home.path().join("new");
        fs::create_dir_all(old.join("p1")).unwrap();
        fs::create_dir_all(new.join("p1")).unwrap();
        fs::write(old.join("p1/auth.json"), "old").unwrap();
        fs::write(new.join("p1/auth.json"), "needle secret").unwrap();
        symlink(&old, home.path().join(".config")).unwrap();
        fs::hard_link(new.join("p1/auth.json"), home.path().join("notes.txt")).unwrap();
        let workspace = Workspace::new(home.path()).unwrap();
        let old_policy = CredentialPolicy::new(Some(home.path()), &[]);
        let query = SearchQuery {
            pattern: "needle".into(),
            path: None,
            glob: None,
            case_insensitive: false,
            context: 0,
            max_lines: 10,
        };
        let changed = std::cell::Cell::new(false);
        let result = search_excluding_opened(
            &workspace,
            &query,
            &CancellationToken::new(),
            |candidate| {
                if !changed.replace(true) {
                    fs::remove_file(home.path().join(".config")).unwrap();
                    symlink(&new, home.path().join(".config")).unwrap();
                }
                Ok(old_policy.refuses(candidate))
            },
            |candidate, file| {
                Ok(CredentialPolicy::new(Some(home.path()), &[]).refuses_opened(candidate, file))
            },
        )
        .unwrap();
        assert!(
            result.files.is_empty(),
            "retargeted credential content leaked"
        );
    }

    #[test]
    fn search_skips_file_swapped_for_outside_symlink_after_walk() {
        let workspace_dir = tempfile::tempdir().unwrap();
        let outside_dir = tempfile::tempdir().unwrap();
        fs::write(workspace_dir.path().join("needle.txt"), "inside\n").unwrap();
        fs::write(outside_dir.path().join("secret.txt"), "needle outside\n").unwrap();
        let workspace = Workspace::new(workspace_dir.path()).unwrap();
        let cancel = CancellationToken::new();
        let files = collect_files(&workspace, workspace.root(), None, &cancel).unwrap();

        fs::remove_file(workspace_dir.path().join("needle.txt")).unwrap();
        symlink(
            outside_dir.path().join("secret.txt"),
            workspace_dir.path().join("needle.txt"),
        )
        .unwrap();

        let query = SearchQuery {
            pattern: "needle".into(),
            path: None,
            glob: None,
            case_insensitive: false,
            context: 0,
            max_lines: 10,
        };
        let matcher = RegexMatcher::new("needle").unwrap();
        let result = search_content(&workspace, &matcher, &query, &files, &cancel, |_, _| {
            Ok(false)
        })
        .unwrap();
        assert!(
            result.files.is_empty(),
            "outside symlink target was searched"
        );
    }
}
