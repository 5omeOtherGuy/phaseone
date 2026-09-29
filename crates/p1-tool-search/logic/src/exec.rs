//! The `grep` call as a component runs it, over the capabilities its assembly links.
//!
//! [`execute`] is generic over [`Capabilities`] so the guest's order of operations is tested
//! natively, and the native `p1-tool-search` runs this same function over the real workspace
//! service; the component only adapts the WIT imports to this trait.
//!
//! Search is granted `control` and `workspace` only: no `snapshot` and no
//! `workspace-mutation`. Its reads therefore record no observation and it never takes the
//! write gate, so a search can never give an agent the permission `edit` and `write` check
//! (docs/design/modules/workspace-mutation.md). The trait has no method that could.
//!
//! The walk and the matching run on the host behind `workspace.search` and
//! `workspace.list-files`; this module validates the input, asks for them, and renders the
//! model-facing text from what they return. Nothing here spawns a thread or blocks one:
//! every wait is a host import the guest is suspended in, and `cancelled()` is checked
//! between the host calls.

use crate::{
    BINARY_SNIFF_BYTES, GrepInput, MAX_OUTPUT_LINES, Mode, content_section, does_not_exist,
    escapes_workspace, io_failed, kept_matches, parse_json_input, render_content,
    render_content_page, render_count, render_files, render_files_page, text_input_error,
};

/// Why a workspace operation failed: the WIT `fs-error`, mirrored so this crate stays
/// target-independent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FsError {
    OutsideWorkspace,
    NotFound,
    WrongKind,
    AlreadyExists,
    /// A search pattern or glob that does not parse; the message is the model-facing text.
    InvalidPattern(String),
    Cancelled,
    Io(String),
}

/// The WIT `entry-kind`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryKind {
    File,
    Directory,
    Other,
}

/// The WIT `entry` of `workspace.stat`: `path` is relative to the root, `/`-separated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub path: String,
    pub kind: EntryKind,
}

/// The WIT `search-query`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchQuery {
    /// A regular expression in the host's (ripgrep) syntax.
    pub pattern: String,
    /// The file or directory to search; the root when absent.
    pub path: Option<String>,
    pub glob: Option<String>,
    pub case_insensitive: bool,
    /// Lines of context before and after each match.
    pub context: u32,
    /// The most lines (matches and context) the result carries.
    pub max_lines: u32,
}

/// The WIT `search-line`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchLine {
    /// One-based.
    pub line_number: u64,
    pub text: String,
    /// False for a context line.
    pub is_match: bool,
}

/// The WIT `file-matches`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileMatches {
    pub path: String,
    pub lines: Vec<SearchLine>,
}

/// The WIT `search-result`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchResult {
    /// In bytewise displayed-path order (the listing's order).
    pub files: Vec<FileMatches>,
    /// Whether the search stopped at `max-lines` before the walk ended.
    pub truncated: bool,
    /// Matching files the result leaves out, after the last one it carries.
    pub omitted_files: u64,
}

/// The imports a `grep` call uses: `control` and the read side of `workspace`.
pub trait Capabilities {
    fn cancelled(&self) -> bool;
    fn stat(&self, path: &str) -> Result<Entry, FsError>;
    fn read(&self, path: &str, offset: u64, length: u64) -> Result<Vec<u8>, FsError>;
    fn list_files(&self, path: &str, glob: Option<&str>) -> Result<Vec<String>, FsError>;
    fn search(&self, query: &SearchQuery) -> Result<SearchResult, FsError>;
}

/// The input of a tool call, as the wire carries it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CallInput<'a> {
    Json(&'a str),
    Text(&'a str),
}

/// How a `grep` call ended; `content` is exactly what the model is shown.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    Ok(String),
    Error(String),
    /// Empty content, as the native tools return it.
    Cancelled,
}

/// Run one `grep` call; `tool` is the name the model called it by, for input errors.
pub fn execute<C: Capabilities>(caps: &C, tool: &str, input: CallInput<'_>) -> Outcome {
    // Cancellation before any work: touch nothing, not even a stat.
    if caps.cancelled() {
        return Outcome::Cancelled;
    }
    let input = match input {
        CallInput::Json(raw) => match parse_json_input(tool, raw) {
            Ok(input) => input,
            Err(message) => return Outcome::Error(message),
        },
        CallInput::Text(_) => return Outcome::Error(text_input_error(tool)),
    };
    match run(caps, &input) {
        Ok(content) => Outcome::Ok(content),
        Err(Stop::Cancelled) => Outcome::Cancelled,
        Err(Stop::Error(message)) => Outcome::Error(message),
    }
}

enum Stop {
    Cancelled,
    Error(String),
}

/// The line cap every search asks the host for. The rendering never shows
/// `MAX_OUTPUT_LINES` lines or more, so a result cut there renders exactly as the whole one
/// ([`crate::render_content`]), and the guest never holds more than the model can be shown.
const LINE_CAP: u32 = MAX_OUTPUT_LINES as u32;

fn run<C: Capabilities>(caps: &C, input: &GrepInput) -> Result<String, Stop> {
    match input.mode {
        Mode::Content if input.is_paged() => content_page(caps, input),
        Mode::Content => {
            let result = caps
                .search(&query(input, None, input.context_lines(), LINE_CAP))
                .map_err(|error| scope_error(caps, error, input))?;
            Ok(render_content(&result))
        }
        // With an empty pattern and a glob, the walk itself is the result.
        Mode::Files if input.pattern.is_empty() => listed_files(caps, input),
        Mode::Files if input.is_paged() => files_page(caps, input),
        Mode::Files => matching_files(caps, input),
        Mode::Count => counts(caps, input),
    }
}

/// The most lines one resumed single-file search carries. Exact counts past it would need
/// every line of the file in guest memory, so a file cut there is marked partial instead.
const FILE_LINE_CAP: u32 = 64 * LINE_CAP;

/// Visit every matching file in displayed-path order, each carrying up to `per_file_lines`
/// of its lines, until `visit` returns false. `visit` also learns whether the file is
/// partial: its search stopped at that budget, so its later lines were never searched.
///
/// One capped search carries the start of the walk. When the host stopped at the cap, its
/// last file may be cut, so unless it already holds what the visitor needs it is searched
/// again on its own, and the rest of the walk is listed and searched file by file. Returns
/// how many matching files could not be reached because the host refused that listing.
fn each_file<C: Capabilities>(
    caps: &C,
    input: &GrepInput,
    context: u32,
    per_file_lines: u32,
    mut visit: impl FnMut(FileMatches, bool) -> bool,
) -> Result<usize, Stop> {
    let first = caps
        .search(&query(input, None, context, LINE_CAP))
        .map_err(|error| scope_error(caps, error, input))?;
    let mut files = first.files;
    let mut cut = None;
    let resume = match files.last() {
        Some(last) if first.truncated => {
            let whole = last.lines.len() >= per_file_lines as usize;
            let path = last.path.clone();
            if !whole {
                cut = files.pop();
            }
            Some((path, whole))
        }
        _ => None,
    };
    for file in files {
        if !visit(file, false) {
            return Ok(0);
        }
    }
    let Some((last, visited)) = resume else {
        return Ok(0);
    };
    if caps.cancelled() {
        return Err(Stop::Cancelled);
    }
    let scope = input.path.as_deref().unwrap_or(".");
    let listed = match caps.list_files(scope, input.glob.as_deref()) {
        Ok(listed) => listed,
        Err(error) if is_bounded_listing_refusal(&error) => {
            // The file the cap cut is still shown for what it carries, marked partial.
            if let Some(file) = cut
                && !visit(file, true)
            {
                return Ok(0);
            }
            return Ok(usize::try_from(first.omitted_files).unwrap_or(usize::MAX));
        }
        Err(error) => return Err(scope_error(caps, error, input)),
    };
    // The listing shares the search's bytewise display order, so the walk resumes at the
    // file the capped search ended in, by value: a vanished path cannot restart it.
    for path in listed.iter().filter(|path| {
        if visited {
            path.as_str() > last.as_str()
        } else {
            path.as_str() >= last.as_str()
        }
    }) {
        if caps.cancelled() {
            return Err(Stop::Cancelled);
        }
        let one = match caps.search(&query(input, Some(path), context, per_file_lines)) {
            Ok(one) => one,
            // Vanished or unreadable since the listing: skipped, as the walk skips it.
            Err(FsError::NotFound | FsError::Io(_)) => continue,
            Err(error) => return Err(path_error(caps, error, path)),
        };
        for file in one.files {
            if !visit(file, one.truncated) {
                return Ok(0);
            }
        }
    }
    Ok(0)
}

/// `mode:"content"` with paging or a per-file cap: only the blocks on the page are kept, and
/// the walk stops once an entry after the page exists or the page alone fills the bound.
fn content_page<C: Capabilities>(caps: &C, input: &GrepInput) -> Result<String, Stop> {
    let page = input.page();
    let cap = input.max_per_file();
    let context = input.context_lines();
    let mut sections = Vec::new();
    let mut seen = 0;
    let mut lines = 0;
    let mut stopped = false;
    let unsearched = each_file(caps, input, context, FILE_LINE_CAP, |file, partial| {
        let first = seen;
        seen += kept_matches(&file, cap);
        if let Some(section) = content_section(&file, first, page, cap, context, partial) {
            lines += section.line_count();
            sections.push(section);
        }
        stopped = page.end().is_some_and(|end| seen > end) || lines >= MAX_OUTPUT_LINES;
        !stopped
    })?;
    let complete = !stopped && unsearched == 0;
    Ok(render_content_page(
        &sections, page, seen, complete, unsearched,
    ))
}

/// `mode:"count"`: every matching file's match count, so the whole walk is searched; a file
/// past the per-file line budget counts as a lower bound.
fn counts<C: Capabilities>(caps: &C, input: &GrepInput) -> Result<String, Stop> {
    let mut counts = Vec::new();
    let unsearched = each_file(caps, input, 0, FILE_LINE_CAP, |file, partial| {
        let matches = file.lines.iter().filter(|hit| hit.is_match).count();
        if matches > 0 {
            counts.push((file.path, matches, partial));
        }
        true
    })?;
    Ok(render_count(&counts, input.page(), unsearched))
}

/// `mode:"files"` with a pattern and paging: every matching path, for the exact total and
/// the directory summary of the paths the page leaves out.
fn files_page<C: Capabilities>(caps: &C, input: &GrepInput) -> Result<String, Stop> {
    let mut paths = Vec::new();
    let unknown = each_file(caps, input, 0, 1, |file, _| {
        paths.push(file.path);
        true
    })?;
    Ok(render_files_page(&paths, unknown, input.page()))
}

fn query(input: &GrepInput, path: Option<&str>, context: u32, max_lines: u32) -> SearchQuery {
    SearchQuery {
        pattern: input.pattern.clone(),
        path: path.or(input.path.as_deref()).map(str::to_string),
        glob: if path.is_some() {
            None
        } else {
            input.glob.clone()
        },
        case_insensitive: input.case_insensitive,
        context,
        max_lines,
    }
}

/// `mode:"files"` with an empty pattern: every listed file that does not look binary.
fn listed_files<C: Capabilities>(caps: &C, input: &GrepInput) -> Result<String, Stop> {
    let scope = input.path.as_deref().unwrap_or(".");
    let listed = caps
        .list_files(scope, input.glob.as_deref())
        .map_err(|error| scope_error(caps, error, input))?;
    let mut matched = Vec::new();
    for path in listed {
        if caps.cancelled() {
            return Err(Stop::Cancelled);
        }
        if !looks_binary(caps, &path) {
            matched.push(path);
        }
    }
    if input.is_paged() {
        return Ok(render_files_page(&matched, 0, input.page()));
    }
    let total = matched.len();
    Ok(render_files(&matched, total))
}

/// A NUL in the first [`BINARY_SNIFF_BYTES`] bytes. A file that cannot be read is listed, as
/// the native tool always listed it.
fn looks_binary<C: Capabilities>(caps: &C, path: &str) -> bool {
    caps.read(path, 0, BINARY_SNIFF_BYTES as u64)
        .is_ok_and(|prefix| prefix.contains(&0))
}

/// `mode:"files"` with a pattern: the files the search matched.
///
/// One search names every matching file unless the host stopped at the line cap first; its
/// count of the files it left out is exact, so only their paths are missing. Only as many
/// are needed as the output can show, so the rest of the walk is listed and searched file by
/// file, one line each, until that many are known.
fn matching_files<C: Capabilities>(caps: &C, input: &GrepInput) -> Result<String, Stop> {
    let result = caps
        .search(&query(input, None, 0, LINE_CAP))
        .map_err(|error| scope_error(caps, error, input))?;
    let omitted = usize::try_from(result.omitted_files).unwrap_or(usize::MAX);
    let total = result.files.len().saturating_add(omitted);
    let mut matched: Vec<String> = result.files.into_iter().map(|file| file.path).collect();
    let wanted = total.min(MAX_OUTPUT_LINES);
    if matched.len() < wanted {
        if caps.cancelled() {
            return Err(Stop::Cancelled);
        }
        let scope = input.path.as_deref().unwrap_or(".");
        let listed = match caps.list_files(scope, input.glob.as_deref()) {
            Ok(listed) => listed,
            // The host refuses an exceptionally large listing rather than accumulating it.
            // Files mode never needs the whole listing: the first search's exact total is
            // known, so render the carried paths and count the rest instead of failing the
            // already-bounded result.
            Err(error) if is_bounded_listing_refusal(&error) => {
                return Ok(render_files(&matched, total));
            }
            Err(error) => return Err(scope_error(caps, error, input)),
        };
        // The initial search and the listing now share the bytewise display order, but skip
        // every path already carried by value rather than resuming after the last one: a
        // lookup by value that misses (the file was deleted or renamed between the search and
        // the listing) must not restart at the top, or files already in `matched` would be
        // searched and appended a second time.
        let mut seen: std::collections::HashSet<String> = matched.iter().cloned().collect();
        for path in &listed {
            if matched.len() >= wanted {
                break;
            }
            if seen.contains(path.as_str()) {
                continue;
            }
            if caps.cancelled() {
                return Err(Stop::Cancelled);
            }
            let one = match caps.search(&query(input, Some(path), 0, 1)) {
                Ok(one) => one,
                // The listed file vanished (or became unreadable) before this search could
                // read it. The initial search above and the native walk both skip a file
                // they cannot read mid-walk, so a race must not fail the whole call.
                Err(FsError::NotFound | FsError::Io(_)) => continue,
                Err(error) => return Err(path_error(caps, error, path)),
            };
            for file in one.files {
                if seen.insert(file.path.clone()) {
                    matched.push(file.path);
                }
            }
        }
    }
    Ok(render_files(&matched, total))
}

/// The host's bounded-listing refusal (`file_walk::collect_files`). A files-mode top-up
/// treats it as "no more paths are available" rather than failing the bounded result.
const BOUNDED_LISTING_MARKER: &str = "bounded search budget";

fn is_bounded_listing_refusal(error: &FsError) -> bool {
    matches!(error, FsError::Io(message) if message.contains(BOUNDED_LISTING_MARKER))
}

/// A failure of the search scope the input names: the native texts, with the path shown as
/// the workspace service shows it.
fn scope_error<C: Capabilities>(caps: &C, error: FsError, input: &GrepInput) -> Stop {
    path_error(caps, error, input.path.as_deref().unwrap_or("."))
}

fn path_error<C: Capabilities>(caps: &C, error: FsError, requested: &str) -> Stop {
    match error {
        FsError::Cancelled => Stop::Cancelled,
        FsError::OutsideWorkspace => Stop::Error(escapes_workspace(requested)),
        FsError::NotFound => Stop::Error(does_not_exist(&root_relative(caps, requested))),
        FsError::InvalidPattern(message) => Stop::Error(message),
        FsError::Io(message) => Stop::Error(io_failed(&root_relative(caps, requested), &message)),
        // Neither is an answer the read side gives a search; shown plainly if one ever is.
        FsError::WrongKind => Stop::Error(io_failed(
            &root_relative(caps, requested),
            "not the kind of path this search needs",
        )),
        FsError::AlreadyExists => {
            Stop::Error(io_failed(&root_relative(caps, requested), "already exists"))
        }
    }
}

/// How the workspace service shows a path that may not exist: its root-relative form.
///
/// A relative request displays as its lexical normalization. An absolute one inside the root
/// has the root as a literal prefix (the service confines lexically first), and the guest
/// never sees the root: the shortest prefix of the request that `stat` answers is the root
/// (every shorter one is outside it), and what follows it is the display form. When no
/// prefix answers, the request is shown as given.
fn root_relative<C: Capabilities>(caps: &C, requested: &str) -> String {
    if !requested.starts_with('/') {
        return crate::lexical_normalize(requested);
    }
    let normalized = crate::lexical_normalize(requested);
    let components: Vec<&str> = if normalized.is_empty() {
        Vec::new()
    } else {
        normalized.split('/').collect()
    };
    for keep in 0..=components.len() {
        let prefix = format!("/{}", components[..keep].join("/"));
        if let Ok(entry) = caps.stat(&prefix) {
            let rest = components[keep..].join("/");
            return match (entry.path.as_str(), rest.as_str()) {
                ("" | ".", _) => rest,
                (base, "") => base.to_string(),
                (base, _) => format!("{base}/{rest}"),
            };
        }
    }
    requested.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::collections::BTreeMap;

    /// An in-memory host rooted at `/root`: files by root-relative path, a log of every
    /// import call, and a content search that honours the line cap as the host does.
    #[derive(Default)]
    struct Host {
        files: BTreeMap<String, Vec<u8>>,
        log: RefCell<Vec<String>>,
        cancel_after: Option<usize>,
        cancel_calls: RefCell<usize>,
        refuse: Option<FsError>,
        /// Refuses the top-up listing only, as the host's size budget does on a large walk.
        refuse_listing: Option<FsError>,
        /// Paths the listing leaves out, as if deleted between the search and the listing.
        vanish: Vec<String>,
        /// Paths a single-file (top-up) search refuses with `NotFound`, as if the listed
        /// file vanished before it could be read.
        unreadable: Vec<String>,
    }

    impl Host {
        fn with(files: &[(&str, &str)]) -> Self {
            Self {
                files: files
                    .iter()
                    .map(|(path, contents)| (path.to_string(), contents.as_bytes().to_vec()))
                    .collect(),
                ..Self::default()
            }
        }

        fn log(&self) -> Vec<String> {
            self.log.borrow().clone()
        }

        fn in_scope(&self, scope: Option<&str>) -> Vec<(&String, &Vec<u8>)> {
            let scope = scope.map(crate::lexical_normalize).unwrap_or_default();
            self.files
                .iter()
                .filter(|(path, _)| {
                    scope.is_empty() || **path == scope || path.starts_with(&format!("{scope}/"))
                })
                .collect()
        }
    }

    impl Capabilities for Host {
        fn cancelled(&self) -> bool {
            let mut calls = self.cancel_calls.borrow_mut();
            *calls += 1;
            self.cancel_after.is_some_and(|after| *calls > after)
        }

        fn stat(&self, path: &str) -> Result<Entry, FsError> {
            self.log.borrow_mut().push(format!("stat {path}"));
            let relative = match path.strip_prefix("/root") {
                Some(rest) => rest.trim_start_matches('/'),
                None if path.starts_with('/') => return Err(FsError::OutsideWorkspace),
                None => path,
            };
            if relative.is_empty() {
                return Ok(Entry {
                    path: String::new(),
                    kind: EntryKind::Directory,
                });
            }
            if self.files.contains_key(relative) {
                return Ok(Entry {
                    path: relative.to_string(),
                    kind: EntryKind::File,
                });
            }
            Err(FsError::NotFound)
        }

        fn read(&self, path: &str, offset: u64, length: u64) -> Result<Vec<u8>, FsError> {
            self.log.borrow_mut().push(format!("read {path}"));
            let bytes = self.files.get(path).ok_or(FsError::NotFound)?;
            let start = (offset as usize).min(bytes.len());
            let end = (start + length as usize).min(bytes.len());
            Ok(bytes[start..end].to_vec())
        }

        fn list_files(&self, path: &str, glob: Option<&str>) -> Result<Vec<String>, FsError> {
            self.log.borrow_mut().push(format!("list {path}"));
            if let Some(error) = &self.refuse_listing {
                return Err(error.clone());
            }
            if let Some(error) = &self.refuse {
                return Err(error.clone());
            }
            Ok(self
                .in_scope(Some(path))
                .into_iter()
                .filter(|(path, _)| !self.vanish.contains(path))
                .filter(|(path, _)| {
                    glob.is_none_or(|glob| path.ends_with(glob.trim_start_matches('*')))
                })
                .map(|(path, _)| path.clone())
                .collect())
        }

        /// A literal-substring search, enough to exercise the guest's use of the result.
        fn search(&self, query: &SearchQuery) -> Result<SearchResult, FsError> {
            self.log
                .borrow_mut()
                .push(format!("search {:?} {}", query.path, query.max_lines));
            if let Some(error) = &self.refuse {
                return Err(error.clone());
            }
            if query
                .path
                .as_deref()
                .is_some_and(|path| self.unreadable.iter().any(|p| p == path))
            {
                return Err(FsError::NotFound);
            }
            let mut result = SearchResult {
                files: Vec::new(),
                truncated: false,
                omitted_files: 0,
            };
            let mut room = query.max_lines as usize;
            for (path, bytes) in self.in_scope(query.path.as_deref()) {
                if bytes.contains(&0) {
                    continue;
                }
                let text = String::from_utf8_lossy(bytes);
                let hits: Vec<SearchLine> = text
                    .lines()
                    .enumerate()
                    .filter(|(_, line)| line.contains(&query.pattern))
                    .map(|(index, line)| SearchLine {
                        line_number: index as u64 + 1,
                        text: line.to_string(),
                        is_match: true,
                    })
                    .collect();
                if hits.is_empty() {
                    continue;
                }
                if room == 0 {
                    result.truncated = true;
                    result.omitted_files += 1;
                    continue;
                }
                if hits.len() > room {
                    result.truncated = true;
                }
                let kept: Vec<SearchLine> = hits.into_iter().take(room).collect();
                room -= kept.len();
                result.files.push(FileMatches {
                    path: path.clone(),
                    lines: kept,
                });
            }
            Ok(result)
        }
    }

    fn run_json(host: &Host, raw: &str) -> Outcome {
        execute(host, "grep", CallInput::Json(raw))
    }

    #[test]
    fn content_mode_is_one_search_rendered_by_file() {
        let host = Host::with(&[("a.rs", "x\nbeta\n"), ("b.rs", "beta\n"), ("c.rs", "no\n")]);
        assert_eq!(
            run_json(&host, r#"{"pattern":"beta"}"#),
            Outcome::Ok("a.rs\n2:beta\n\nb.rs\n1:beta".into())
        );
        assert_eq!(host.log(), vec!["search None 2000".to_string()]);
    }

    #[test]
    fn input_errors_and_cancellation_touch_nothing() {
        let host = Host::with(&[("a.rs", "beta\n")]);
        assert_eq!(
            execute(&host, "grep", CallInput::Text("beta")),
            Outcome::Error(
                "Invalid input for grep: expected a JSON object input, got freeform text".into()
            )
        );
        assert!(matches!(
            run_json(&host, r#"{"pattern":"a","context":-1}"#),
            Outcome::Error(message) if message.starts_with("Invalid input for grep: ")
        ));
        let cancelled = Host {
            cancel_after: Some(0),
            ..Host::with(&[("a.rs", "beta\n")])
        };
        assert_eq!(
            run_json(&cancelled, r#"{"pattern":"a"}"#),
            Outcome::Cancelled
        );
        assert!(host.log().is_empty() && cancelled.log().is_empty());
    }

    #[test]
    fn files_mode_with_an_empty_pattern_lists_and_skips_binary_files() {
        let host = Host::with(&[("a.md", "x\n"), ("b.md", "\0bin"), ("c.txt", "x\n")]);
        assert_eq!(
            run_json(&host, r#"{"pattern":"","mode":"files","glob":"*.md"}"#),
            Outcome::Ok("a.md".into())
        );
        assert_eq!(host.log(), vec!["list .", "read a.md", "read b.md"]);
    }

    #[test]
    fn files_mode_names_the_files_a_capped_search_left_out() {
        // Thirty files of a hundred hits: the first search stops after the twentieth file,
        // and the other ten are found one search each.
        let text = "beta\n".repeat(100);
        let names: Vec<String> = (0..30).map(|index| format!("f{index:02}.txt")).collect();
        let files: Vec<(&str, &str)> = names
            .iter()
            .map(|name| (name.as_str(), text.as_str()))
            .collect();
        let host = Host::with(&files);
        assert_eq!(
            run_json(&host, r#"{"pattern":"beta","mode":"files"}"#),
            Outcome::Ok(names.join("\n"))
        );
        let log = host.log();
        assert_eq!(log[0], "search None 2000");
        assert_eq!(log[1], "list .");
        assert_eq!(log[2], "search Some(\"f20.txt\") 1");
        assert_eq!(log.len(), 2 + 10);
    }

    /// A file deleted or renamed between the capped search and the listing must not make the
    /// top-up restart at the first listed path and append the files already carried.
    #[test]
    fn files_mode_resumes_after_a_vanished_last_path() {
        let text = "beta\n".repeat(100);
        let names: Vec<String> = (0..30).map(|index| format!("f{index:02}.txt")).collect();
        let files: Vec<(&str, &str)> = names
            .iter()
            .map(|name| (name.as_str(), text.as_str()))
            .collect();
        // The capped search carries f00..f19; f19.txt is gone from the fresh listing.
        let host = Host {
            vanish: vec!["f19.txt".to_string()],
            ..Host::with(&files)
        };
        assert_eq!(
            run_json(&host, r#"{"pattern":"beta","mode":"files"}"#),
            Outcome::Ok(names.join("\n"))
        );
    }

    /// A listed file that vanished before its top-up search read it is skipped, as the native
    /// walk skipped a file it could not read mid-walk, instead of failing the whole call.
    #[test]
    fn files_mode_skips_a_file_that_vanished_before_the_top_up_search() {
        let text = "beta\n".repeat(100);
        let names: Vec<String> = (0..30).map(|index| format!("f{index:02}.txt")).collect();
        let files: Vec<(&str, &str)> = names
            .iter()
            .map(|name| (name.as_str(), text.as_str()))
            .collect();
        let host = Host {
            unreadable: vec!["f20.txt".to_string()],
            ..Host::with(&files)
        };
        let outcome = run_json(&host, r#"{"pattern":"beta","mode":"files"}"#);
        let Outcome::Ok(content) = outcome else {
            panic!("a vanished file must not fail the call: {outcome:?}");
        };
        // f20.txt was skipped, so the footer names the one path the output cannot show.
        assert!(
            content.ends_with("1 more matching files not shown; narrow with path or glob]"),
            "{content}"
        );
        assert!(!content.contains("f20.txt"), "{content}");
    }

    #[test]
    fn files_mode_keeps_the_bounded_result_when_the_top_up_listing_is_too_large() {
        // 1_500 files of two hits each: the capped search carries 1_000 paths and omits 500,
        // so it needs a top-up listing; that listing hits the host's size budget. The call
        // must keep the bounded result, not fail.
        let text = "beta\nbeta\n";
        let names: Vec<String> = (0..1_500).map(|index| format!("f{index:05}.txt")).collect();
        let files: Vec<(&str, &str)> = names.iter().map(|name| (name.as_str(), text)).collect();
        let host = Host {
            refuse_listing: Some(FsError::Io(
                "workspace listing exceeds the bounded search budget; narrow with path or glob"
                    .into(),
            )),
            ..Host::with(&files)
        };
        let outcome = run_json(&host, r#"{"pattern":"beta","mode":"files"}"#);
        let Outcome::Ok(content) = outcome else {
            panic!("a bounded listing refusal must not fail the search: {outcome:?}");
        };
        assert!(content.starts_with("f00000.txt"), "{content}");
        assert!(
            content.ends_with("500 more matching files not shown; narrow with path or glob]"),
            "{content}"
        );
    }

    #[test]
    fn scope_errors_are_the_native_texts() {
        let refused = |error: FsError, raw: &str| {
            let host = Host {
                refuse: Some(error),
                ..Host::default()
            };
            run_json(&host, raw)
        };
        assert_eq!(
            refused(FsError::OutsideWorkspace, r#"{"pattern":"a","path":"../"}"#),
            Outcome::Error("path escapes workspace: ../".into())
        );
        assert_eq!(
            refused(FsError::NotFound, r#"{"pattern":"a","path":"./nope/x/.."}"#),
            Outcome::Error("nope does not exist.".into())
        );
        assert_eq!(
            refused(
                FsError::NotFound,
                r#"{"pattern":"a","path":"/root/sub/nope"}"#
            ),
            Outcome::Error("sub/nope does not exist.".into())
        );
        assert_eq!(
            refused(
                FsError::InvalidPattern("invalid regex pattern: x".into()),
                r#"{"pattern":"(","mode":"files"}"#
            ),
            Outcome::Error("invalid regex pattern: x".into())
        );
        assert_eq!(
            refused(FsError::Cancelled, r#"{"pattern":"","mode":"files"}"#),
            Outcome::Cancelled
        );
    }

    fn hundred_hit_files(count: usize) -> (Vec<String>, String) {
        let names = (0..count).map(|index| format!("f{index:02}.txt")).collect();
        (names, "beta\n".repeat(100))
    }

    fn host_of(names: &[String], text: &str) -> Host {
        let files: Vec<(&str, &str)> = names.iter().map(|name| (name.as_str(), text)).collect();
        Host::with(&files)
    }

    /// Item 1: one `<path>:<n>` line per matching file and the total, exact even past the
    /// host's line cap (thirty files of a hundred hits: the capped search carries twenty).
    #[test]
    fn count_mode_counts_every_match_past_the_line_cap() {
        let host = Host::with(&[
            ("a.rs", "beta\nx\nbeta\n"),
            ("b.rs", "beta\n"),
            ("c.rs", "no\n"),
        ]);
        assert_eq!(
            run_json(&host, r#"{"pattern":"beta","mode":"count"}"#),
            Outcome::Ok("a.rs:2\nb.rs:1\n[total: 3 matches in 2 files]".into())
        );
        let (names, text) = hundred_hit_files(30);
        let host = host_of(&names, &text);
        let mut expected: Vec<String> = names.iter().map(|name| format!("{name}:100")).collect();
        expected.push("[total: 3000 matches in 30 files]".into());
        assert_eq!(
            run_json(&host, r#"{"pattern":"beta","mode":"count"}"#),
            Outcome::Ok(expected.join("\n"))
        );
        // The capped search ended inside f19.txt, so the walk resumes with it, searched whole.
        let log = host.log();
        assert_eq!(log[0], "search None 2000");
        assert_eq!(log[1], "list .");
        assert_eq!(log[2], format!("search Some(\"f19.txt\") {FILE_LINE_CAP}"));
        assert_eq!(log.len(), 2 + 11);
        assert_eq!(
            run_json(
                &Host::with(&[("a.rs", "x\n")]),
                r#"{"pattern":"beta","mode":"count"}"#
            ),
            Outcome::Ok("No matches.".into())
        );
    }

    #[test]
    fn count_mode_names_the_files_a_refused_listing_left_unsearched() {
        let (names, text) = hundred_hit_files(30);
        let host = Host {
            refuse_listing: Some(FsError::Io(
                "workspace listing exceeds the bounded search budget; narrow with path or glob"
                    .into(),
            )),
            ..host_of(&names, &text)
        };
        let Outcome::Ok(content) = run_json(&host, r#"{"pattern":"beta","mode":"count"}"#) else {
            panic!("a refused listing must not fail the count");
        };
        // f00..f18 are whole; f19 may have been cut by the cap, so its count is a lower
        // bound, and f20..f29 were never reached.
        assert!(
            content.ends_with(
                "f18.txt:100\nf19.txt:100+\n[total: 2000+ matches in 20 files]\n[10 more matching files not searched; narrow with path or glob]"
            ),
            "{content}"
        );
    }

    /// Item 2's example end to end: seven matching files, offset 2, head_limit 3.
    #[test]
    fn files_mode_pages_through_the_matching_files() {
        let names: Vec<String> = (1..=7).map(|index| format!("f{index}.txt")).collect();
        let mut files: Vec<(&str, &str)> =
            names.iter().map(|name| (name.as_str(), "beta\n")).collect();
        files.push(("other.txt", "no\n"));
        let host = Host::with(&files);
        assert_eq!(
            run_json(
                &host,
                r#"{"pattern":"beta","mode":"files","offset":2,"head_limit":3}"#
            ),
            Outcome::Ok(
                "f3.txt\nf4.txt\nf5.txt\n[showing files 3-5 of 7; continue with offset=5]\n[7 matching files, 3 shown, 4 omitted; omitted by directory: ./ (4)]"
                    .into()
            )
        );
        assert_eq!(
            run_json(
                &host,
                r#"{"pattern":"beta","mode":"files","offset":5,"head_limit":3}"#
            ),
            Outcome::Ok("f6.txt\nf7.txt".into())
        );
        // The listing mode pages the same way.
        assert_eq!(
            run_json(
                &host,
                r#"{"pattern":"","mode":"files","glob":"*.txt","head_limit":2}"#
            ),
            Outcome::Ok(
                "f1.txt\nf2.txt\n[showing files 1-2 of 8; continue with offset=2]\n[8 matching files, 2 shown, 6 omitted; omitted by directory: ./ (6)]"
                    .into()
            )
        );
    }

    /// Item 4 past the line cap: the summary's total and directories count every matching
    /// file, including the ones only the resumed walk found.
    #[test]
    fn a_cut_files_page_counts_every_matching_file() {
        let text = "beta\n".repeat(100);
        let names: Vec<String> = (0..30)
            .map(|index| format!("{}/f{index:02}.txt", if index < 25 { "a" } else { "b" }))
            .collect();
        let host = host_of(&names, &text);
        let Outcome::Ok(content) =
            run_json(&host, r#"{"pattern":"beta","mode":"files","head_limit":4}"#)
        else {
            panic!("expected a page");
        };
        assert!(
            content.ends_with(
                "a/f03.txt\n[showing files 1-4 of 30; continue with offset=4]\n[30 matching files, 4 shown, 26 omitted; omitted by directory: a/ (21), b/ (5)]"
            ),
            "{content}"
        );
    }

    /// Item 2 over content entries, past the line cap: the page starts at the offset-th match
    /// line of the whole walk, not of the capped search.
    #[test]
    fn content_mode_pages_past_the_line_cap() {
        let (names, text) = hundred_hit_files(30);
        let host = host_of(&names, &text);
        assert_eq!(
            run_json(&host, r#"{"pattern":"beta","offset":2550,"head_limit":2}"#),
            Outcome::Ok(
                "f25.txt\n51:beta\n52:beta\n[showing matches 2551-2552; continue with offset=2552]"
                    .into()
            )
        );
        let small = Host::with(&[("a.rs", "beta\nbeta\n"), ("b.rs", "beta\n")]);
        assert_eq!(
            run_json(&small, r#"{"pattern":"beta","offset":1}"#),
            Outcome::Ok("a.rs\n2:beta\n\nb.rs\n1:beta".into())
        );
        assert_eq!(
            run_json(&small, r#"{"pattern":"beta","offset":3}"#),
            Outcome::Ok("[showing no matches: offset 3 is past the last of 3]".into())
        );
    }

    /// Item 3 past the line cap: the file the capped search cut is searched whole, so its
    /// omitted count is exact.
    #[test]
    fn a_per_file_cap_counts_exactly_past_the_line_cap() {
        let big = "beta\n".repeat(3_000);
        let host = Host::with(&[("a.txt", big.as_str()), ("b.txt", "beta\n")]);
        assert_eq!(
            run_json(&host, r#"{"pattern":"beta","max_per_file":2}"#),
            Outcome::Ok(
                "a.txt\n1:beta\n2:beta\n… 2998 more matches in this file\n\nb.txt\n1:beta".into()
            )
        );
    }

    fn refusing_listing(host: Host) -> Host {
        Host {
            refuse_listing: Some(FsError::Io(
                "workspace listing exceeds the bounded search budget; narrow with path or glob"
                    .into(),
            )),
            ..host
        }
    }

    /// Review H1: a resumed single-file search asks for at most the per-file budget, never
    /// every line, and a file past it is shown as a lower bound.
    #[test]
    fn a_file_past_the_line_budget_is_searched_to_the_budget_only() {
        let big = "x\n".repeat(FILE_LINE_CAP as usize + 5);
        let host = Host::with(&[("big.txt", big.as_str())]);
        assert_eq!(
            run_json(&host, r#"{"pattern":"x","head_limit":1}"#),
            Outcome::Ok("big.txt\n1:x\n[showing matches 1-1; continue with offset=1]".into())
        );
        assert_eq!(
            host.log()[1..],
            [
                "list .".to_string(),
                format!("search Some(\"big.txt\") {FILE_LINE_CAP}")
            ]
        );
        assert_eq!(
            run_json(&host, r#"{"pattern":"x","mode":"count"}"#),
            Outcome::Ok(format!(
                "big.txt:{FILE_LINE_CAP}+\n[total: {FILE_LINE_CAP}+ matches in 1 files]"
            ))
        );
    }

    /// Review H4, end to end: an offset beyond 32 bits is past the end, not the first page.
    #[test]
    fn an_offset_beyond_32_bits_is_past_the_end() {
        let host = Host::with(&[("a.rs", "x\n")]);
        assert_eq!(
            run_json(&host, r#"{"pattern":"x","offset":4294967296}"#),
            Outcome::Ok("[showing no matches: offset 4294967296 is past the last of 1]".into())
        );
    }

    /// Review H5: an incomplete walk does not claim the offset is past the last match.
    #[test]
    fn an_incomplete_walk_names_no_last_match() {
        let (names, text) = hundred_hit_files(30);
        let host = refusing_listing(host_of(&names, &text));
        assert_eq!(
            run_json(&host, r#"{"pattern":"beta","offset":2000,"head_limit":1}"#),
            Outcome::Ok("[10 more matching files not searched; narrow with path or glob]".into())
        );
    }

    /// Review H6: a files page whose walk was cut short says how many files it never
    /// searched, below the summary whose directories only cover the known paths.
    #[test]
    fn a_files_page_names_the_files_it_could_not_search() {
        let (names, text) = hundred_hit_files(30);
        let host = refusing_listing(host_of(&names, &text));
        assert_eq!(
            run_json(&host, r#"{"pattern":"beta","mode":"files","head_limit":1}"#),
            Outcome::Ok(
                "f00.txt\n[showing files 1-1 of 30; continue with offset=1]\n[30 matching files, 1 shown, 29 omitted; omitted by directory: ./ (19), ...]\n[10 more matching files not searched; narrow with path or glob]"
                    .into()
            )
        );
    }
}
