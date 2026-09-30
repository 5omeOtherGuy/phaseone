//! The `grep` tool's pure logic: the model-facing declaration, input validation, the grouped
//! rendering of a search result with its own output bound and footers, and the call and
//! result descriptions.
//!
//! It is the one copy of that logic: the native `p1-tool-search` and the component
//! `modules/p1-module-search/` both call it, so their texts cannot drift apart (decision S0-R3
//! in `docs/design/modules/package.md`). It is target-independent by construction: std,
//! serde and serde_json only, and no filesystem, thread or clock. The walk and the matching
//! are the host's (`workspace.search`, `workspace.list-files`); [`exec`] only asks for them.

pub mod exec;
pub mod wire;

use std::collections::BTreeMap;

use serde::Deserialize;

use crate::exec::{FileMatches, SearchLine, SearchResult};

/// The default model-facing tool name.
pub const NAME: &str = "grep";
/// The default model-facing description.
pub const DESCRIPTION: &str = "Search workspace files with a regular expression, or for exact text with `literal:true`.\n`mode:\"content\"` (default) groups matching lines by file, with up to `context` surrounding lines; `mode:\"files\"` lists the matching paths, or every file matching `glob` when `pattern` is empty; `mode:\"count\"` gives each matching file's match count and the total.\n`offset` and `head_limit` page the output (match lines, paths or count lines) and a cut page names the next `offset`; `max_per_file` caps the matches shown per file.\nHonours .gitignore, skips hidden and binary files, and never follows symlinks.";
/// The call-description verb (ADR-0057), one of the closed vocabulary of `protocol.md`.
pub const VERB: &str = "search";
/// The shared output bound (`bound_output`'s defaults). `grep` bounds its own result to it,
/// so the bound is also part of this crate's interface.
pub const MAX_OUTPUT_BYTES: usize = 50_000;
pub const MAX_OUTPUT_LINES: usize = 2_000;
/// Context lines when the input names none.
pub const DEFAULT_CONTEXT: usize = 0;
/// The most context lines an input may ask for.
pub const MAX_CONTEXT: usize = 10;
/// A NUL anywhere in this prefix marks a file as binary when listing files.
pub const BINARY_SNIFF_BYTES: usize = 8 * 1024;

/// The input JSON Schema of the declaration.
pub fn input_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "pattern": {
                "type": "string",
                "description": "Regular expression to search for; exact text when `literal` is true."
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
                "enum": ["content", "files", "count"],
                "default": "content",
                "description": "`content` returns matching lines; `files` returns matching paths; `count` returns `<path>:<matches>` per file and the total."
            },
            "case_insensitive": {
                "type": "boolean",
                "default": false,
                "description": "Match case-insensitively."
            },
            "literal": {
                "type": "boolean",
                "default": false,
                "description": "Match `pattern` as exact text: regular-expression characters such as `.`, `(` and `*` match themselves."
            },
            "context": {
                "type": "integer",
                "minimum": 0,
                "maximum": 10,
                "default": 0,
                "description": "Lines of context shown before and after each match."
            },
            "offset": {
                "type": "integer",
                "minimum": 0,
                "default": 0,
                "description": "Skip this many output entries first: match lines (`content`), paths (`files`) or count lines (`count`)."
            },
            "head_limit": {
                "type": "integer",
                "minimum": 1,
                "description": "Show at most this many output entries; when more remain, the output names the `offset` to continue with."
            },
            "max_per_file": {
                "type": "integer",
                "minimum": 1,
                "description": "`content` mode: show at most this many matches per file and the number of that file's matches left out."
            }
        },
        "required": ["pattern"],
        "additionalProperties": false
    })
}

/// What a search returns: matching lines grouped by file, matching paths, or match counts.
#[derive(Debug, Clone, Copy, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    #[default]
    Content,
    Files,
    Count,
}

/// A validated `grep` input.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GrepInput {
    pub pattern: String,
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default)]
    pub glob: Option<String>,
    #[serde(default)]
    pub mode: Mode,
    #[serde(default)]
    pub case_insensitive: bool,
    #[serde(default)]
    pub literal: bool,
    #[serde(default)]
    pub context: Option<i64>,
    #[serde(default)]
    pub offset: Option<i64>,
    #[serde(default)]
    pub head_limit: Option<i64>,
    #[serde(default)]
    pub max_per_file: Option<i64>,
}

impl GrepInput {
    /// The regular expression the host searches for: `pattern`, escaped when `literal`.
    pub fn regex(&self) -> String {
        if self.literal {
            escape_regex(&self.pattern)
        } else {
            self.pattern.clone()
        }
    }

    /// The context lines asked for; validation keeps it within `0..=MAX_CONTEXT`.
    pub fn context_lines(&self) -> u32 {
        self.context
            .and_then(|context| u32::try_from(context).ok())
            .unwrap_or(DEFAULT_CONTEXT as u32)
    }

    /// The page asked for; validation keeps `offset` non-negative and `head_limit` positive.
    pub fn page(&self) -> Page {
        Page {
            offset: self.offset.map_or(0, saturating_usize),
            head_limit: self.head_limit.map(saturating_usize),
        }
    }

    /// The most matches shown per file in content mode, if capped.
    pub fn max_per_file(&self) -> Option<usize> {
        self.max_per_file.map(saturating_usize)
    }

    /// Whether a paging or per-file parameter changes this call's output. Without one a
    /// content or files call renders exactly as it did before these parameters existed.
    pub fn is_paged(&self) -> bool {
        let page = self.page();
        page.offset > 0
            || page.head_limit.is_some()
            || (self.mode == Mode::Content && self.max_per_file.is_some())
    }
}

/// `text` as a regular expression in the host's (Rust `regex`) syntax that matches exactly
/// it: every character `regex::escape` escapes is escaped the same way.
pub fn escape_regex(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for character in text.chars() {
        if matches!(
            character,
            '\\' | '.'
                | '+'
                | '*'
                | '?'
                | '('
                | ')'
                | '|'
                | '['
                | ']'
                | '{'
                | '}'
                | '^'
                | '$'
                | '#'
                | '&'
                | '-'
                | '~'
        ) {
            out.push('\\');
        }
        out.push(character);
    }
    out
}

/// A validated non-negative count as a `usize`. One too large for the target (a 32-bit
/// guest) saturates, so an offset past every entry stays past them instead of wrapping to 0.
fn saturating_usize(value: i64) -> usize {
    usize::try_from(value).unwrap_or(usize::MAX)
}

/// The window of output entries a call asks for: skip `offset`, then keep at most
/// `head_limit`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Page {
    pub offset: usize,
    pub head_limit: Option<usize>,
}

impl Page {
    /// One past the last entry the page may show; `None` without a limit.
    pub fn end(self) -> Option<usize> {
        self.head_limit
            .map(|limit| self.offset.saturating_add(limit))
    }

    fn contains(self, entry: usize) -> bool {
        entry >= self.offset && self.end().is_none_or(|end| entry < end)
    }
}

/// Parse and validate a JSON input; `tool` is the name the model called, for the message.
pub fn parse_json_input(tool: &str, raw: &str) -> Result<GrepInput, String> {
    let input: GrepInput =
        serde_json::from_str(raw).map_err(|error| invalid(tool, &error.to_string()))?;
    if matches!(input.context, Some(context) if !(0..=MAX_CONTEXT as i64).contains(&context)) {
        return Err(invalid(tool, "`context` must be between 0 and 10"));
    }
    if matches!(input.offset, Some(offset) if offset < 0) {
        return Err(invalid(tool, "`offset` must be 0 or more"));
    }
    if matches!(input.head_limit, Some(limit) if limit < 1) {
        return Err(invalid(tool, "`head_limit` must be at least 1"));
    }
    if matches!(input.max_per_file, Some(cap) if cap < 1) {
        return Err(invalid(tool, "`max_per_file` must be at least 1"));
    }
    Ok(input)
}

/// The error for a freeform text input, which this function tool never accepts.
pub fn text_input_error(tool: &str) -> String {
    invalid(tool, "expected a JSON object input, got freeform text")
}

fn invalid(tool: &str, reason: &str) -> String {
    format!("Invalid input for {tool}: {reason}")
}

/// The search path does not exist.
pub fn does_not_exist(display: &str) -> String {
    format!("{display} does not exist.")
}

/// The confinement refusal, worded as the workspace service words it.
pub fn escapes_workspace(requested: &str) -> String {
    format!("path escapes workspace: {requested}")
}

/// A filesystem failure while resolving the search path, worded as the workspace service
/// words it, with the root-relative path in place of a host path the guest never sees.
pub fn io_failed(display: &str, error: &str) -> String {
    format!("workspace I/O failed for {display}: {error}")
}

/// The call-description target (ADR-0057): the pattern and the scope searched.
pub fn describe_target(input: &GrepInput) -> String {
    let scope = input.path.as_deref().unwrap_or(".");
    format!("{} {scope}", input.pattern)
}

/// One rendered result unit — a whole file block in content mode, one path in files mode —
/// with the path a footer can name and the newlines its text contains (the units the shared
/// bound counts).
struct Block<'a> {
    path: &'a str,
    text: String,
    newlines: usize,
}

/// The model-facing text of a content-mode search, bounded.
///
/// `result.files` are the matching files in displayed-path order and `result.omitted_files`
/// the matching files after them. A result the host stopped at its line cap renders exactly as
/// the complete one would as long as the cap is at least [`MAX_OUTPUT_LINES`]: whole blocks
/// are kept only while fewer than that many newlines are shown, so the block the host cut is
/// never kept whole, and a first block over the bound shows fewer lines than the cap.
pub fn render_content(result: &SearchResult) -> String {
    let groups = &result.files;
    let total = groups.len() + usize::try_from(result.omitted_files).unwrap_or(usize::MAX);
    if groups.is_empty() {
        return "No matches.".to_string();
    }
    let blocks: Vec<Block<'_>> = groups
        .iter()
        .map(|group| Block {
            path: group.path.as_str(),
            text: render_block(group),
            newlines: group.lines.len(),
        })
        .collect();
    if !result.truncated && total == groups.len() {
        let joined = join_blocks(&blocks, "\n\n");
        if within_bound(joined.len(), newlines(&joined)) {
            return joined;
        }
    }
    match keep_whole_blocks(&blocks, "\n\n", total) {
        Some(bounded) => bounded,
        // Not even the first file's block fits.
        None => cut_first_block(&groups[0], total - 1),
    }
}

/// The model-facing text of a files-mode search, bounded: `matched` are the first matching
/// paths in displayed-path order and `total` how many paths match in all.
///
/// `matched` must hold at least `min(total, MAX_OUTPUT_LINES)` paths: no more than
/// `MAX_OUTPUT_LINES - 1` of them can be shown, and only the count of the rest is.
pub fn render_files(matched: &[String], total: usize) -> String {
    if total == 0 || matched.is_empty() {
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
    if matched.len() == total {
        let joined = join_blocks(&blocks, "\n");
        if within_bound(joined.len(), newlines(&joined)) {
            return joined;
        }
    }
    match keep_whole_blocks(&blocks, "\n", total) {
        Some(bounded) => bounded,
        // A single path is far shorter than the bound, so this is unreachable; keeping it
        // whole is the only honest answer if it ever happened.
        None => blocks[0].text.clone(),
    }
}

/// One file's block: the path on its own line, then a hit line per match or context line.
fn render_block(group: &FileMatches) -> String {
    let mut block = String::with_capacity(group.path.len());
    block.push_str(&group.path);
    for hit in &group.lines {
        block.push('\n');
        push_hit(&mut block, hit);
    }
    block
}

fn push_hit(out: &mut String, hit: &SearchLine) {
    let separator = if hit.is_match { ':' } else { '-' };
    out.push_str(&format!("{}{separator}{}", hit.line_number, hit.text));
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

/// How many newlines `text` holds: the unit the shared line bound counts.
pub fn newlines(text: &str) -> usize {
    text.matches('\n').count()
}

/// The shared output bound, for a result that ends with the footer line and so has no
/// trailing newline: at most `MAX_OUTPUT_BYTES` bytes and fewer than `MAX_OUTPUT_LINES`
/// newlines. This is exactly the set of results `bound_output` hands back unchanged.
pub fn within_bound(bytes: usize, newlines: usize) -> bool {
    bytes <= MAX_OUTPUT_BYTES && newlines < MAX_OUTPUT_LINES
}

/// Keep whole blocks, in displayed-path order, while each one still leaves room for the
/// footer that replaces everything after it; `total` counts the matching files the footer
/// accounts for. `None` when not even the first block fits.
fn keep_whole_blocks(blocks: &[Block<'_>], separator: &str, total: usize) -> Option<String> {
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
        let footer = footer_after(block.path, total - index - 1);
        if !within_bound(kept.len() + 1 + footer.len(), kept_newlines + 1) {
            kept.truncate(before);
            break;
        }
        last = Some(index);
    }
    let index = last?;
    Some(format!(
        "{kept}\n{}",
        footer_after(blocks[index].path, total - index - 1)
    ))
}

/// The first block alone is over the bound: show its path line and as many whole hit lines
/// as fit, and name the last line shown. A line that does not fit is dropped, so the cut
/// stays at a line boundary; only the first line has no boundary before it, and when it
/// alone is larger than the whole bound its text is cut instead — on a character boundary,
/// like every other bounded tool.
fn cut_first_block(group: &FileMatches, more_files: usize) -> String {
    let path = group.path.as_str();
    let mut body = path.to_string();
    let mut body_newlines = 0;
    let mut shown_line = None;
    for hit in &group.lines {
        let footer = footer_inside(path, hit.line_number, more_files);
        let mut line = String::new();
        push_hit(&mut line, hit);
        let mut candidate = String::with_capacity(body.len() + 1 + line.len());
        candidate.push_str(&body);
        candidate.push('\n');
        candidate.push_str(&line);
        if within_bound(candidate.len() + 1 + footer.len(), body_newlines + 2) {
            body = candidate;
            body_newlines += 1;
            shown_line = Some(hit.line_number);
            continue;
        }
        if shown_line.is_none()
            && let Some(prefix) = cut_to_fit(&body, body_newlines, &line, &footer)
        {
            body.push('\n');
            body.push_str(&prefix);
            shown_line = Some(hit.line_number);
        }
        break;
    }
    // Every group has at least one hit. When even the path line fills the bound, the footer
    // still names the first line that did not fit.
    let line = shown_line.unwrap_or_else(|| group.lines.first().map_or(0, |hit| hit.line_number));
    format!("{body}\n{}", footer_inside(path, line, more_files))
}

/// The longest character-boundary prefix of one rendered `line` that still leaves room for
/// `footer` after `body`, or `None` when none does.
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

/// The footer when whole blocks were kept: the last path shown, and how many matching files
/// follow it.
fn footer_after(last_path: &str, more_files: usize) -> String {
    format!(
        "[truncated after {last_path}; {more_files} more matching files not shown; narrow with path or glob]"
    )
}

/// The footer when even the first block did not fit: the path, the last line shown inside
/// it, and how many matching files follow it.
fn footer_inside(path: &str, line: u64, more_files: usize) -> String {
    format!(
        "[truncated inside {path} after line {line}; {more_files} more matching files not shown; narrow with path, glob or a stricter pattern]"
    )
}

/// One line of a paged render; `entry` marks the lines `offset` and `head_limit` count, and
/// `note` a file's count of matches not shown, which a cut keeps with its entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct OutLine {
    text: String,
    entry: bool,
    note: bool,
}

/// Lines kept together under one heading: a content file block, or one path or count line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Section {
    lines: Vec<OutLine>,
}

impl Section {
    fn entry(text: String) -> Self {
        Self {
            lines: vec![OutLine {
                text,
                entry: true,
                note: false,
            }],
        }
    }

    /// Lines of the section; the unit the paged walk stops on.
    pub(crate) fn line_count(&self) -> usize {
        self.lines.len()
    }
}

/// How many of `file`'s matches content mode shows under the per-file `cap`.
pub(crate) fn kept_matches(file: &FileMatches, cap: Option<usize>) -> usize {
    let matches = file.lines.iter().filter(|hit| hit.is_match).count();
    cap.map_or(matches, |cap| matches.min(cap))
}

/// The block of `file` a content page shows, or `None` when none of its kept matches is on
/// the page. `first_entry` numbers the file's first match among all output entries. A
/// context line is shown when it lies within `context` lines of a shown match; matches the
/// cap drops are counted on the line after the file's last kept match. `partial` says the
/// host stopped the file's search at its line budget, so later matches were never searched.
pub(crate) fn content_section(
    file: &FileMatches,
    first_entry: usize,
    page: Page,
    cap: Option<usize>,
    context: u32,
    partial: bool,
) -> Option<Section> {
    let matches: Vec<usize> = (0..file.lines.len())
        .filter(|&index| file.lines[index].is_match)
        .collect();
    let kept = cap.map_or(matches.len(), |cap| matches.len().min(cap));
    let shown: Vec<usize> = (0..kept)
        .filter(|&number| page.contains(first_entry + number))
        .map(|number| matches[number])
        .collect();
    if shown.is_empty() {
        return None;
    }
    let shown_numbers: Vec<u64> = shown
        .iter()
        .map(|&index| file.lines[index].line_number)
        .collect();
    let near_a_shown_match = |line_number: u64| {
        let after = shown_numbers.partition_point(|&number| number < line_number);
        let context = u64::from(context);
        let before_ok = after > 0 && line_number - shown_numbers[after - 1] <= context;
        let after_ok = after < shown_numbers.len() && shown_numbers[after] - line_number <= context;
        before_ok || after_ok
    };
    let mut lines = vec![OutLine {
        text: file.path.clone(),
        entry: false,
        note: false,
    }];
    for (index, hit) in file.lines.iter().enumerate() {
        let show = if hit.is_match {
            shown.binary_search(&index).is_ok()
        } else {
            near_a_shown_match(hit.line_number)
        };
        if show {
            let mut text = String::new();
            push_hit(&mut text, hit);
            lines.push(OutLine {
                text,
                entry: hit.is_match,
                note: false,
            });
        }
    }
    let omitted = matches.len() - kept;
    if (omitted > 0 || partial) && page.contains(first_entry + kept - 1) {
        let last_line = file.lines.last().map_or(0, |hit| hit.line_number);
        lines.push(OutLine {
            text: omitted_matches(omitted, partial, last_line),
            entry: false,
            note: true,
        });
    }
    Some(Section { lines })
}

/// The count of a file's matches the per-file cap left out, as the donor's `grep` words it;
/// a lower bound when the file's search stopped at the host line budget after `last_line`.
fn omitted_matches(omitted: usize, partial: bool, last_line: u64) -> String {
    let noun = if omitted == 1 { "match" } else { "matches" };
    match (partial, omitted) {
        (false, _) => format!("… {omitted} more {noun} in this file"),
        (true, 0) => format!("… matches after line {last_line} not searched"),
        (true, _) => format!("… at least {omitted} more {noun} in this file"),
    }
}

/// A content page: `sections` are the blocks of the files on the page, `seen` counts the
/// entries (kept matches) the walk passed, and `complete` says the walk reached its end, so
/// `seen` is the total. `unsearched` counts matching files the walk could not reach.
pub(crate) fn render_content_page(
    sections: &[Section],
    page: Page,
    seen: usize,
    complete: bool,
    unsearched: usize,
) -> String {
    let total = complete.then_some(seen);
    if sections.is_empty() {
        // An incomplete walk knows no last entry, so only the unsearched note is honest.
        let first = match (complete, seen) {
            (false, _) => String::new(),
            (true, 0) => "No matches.".to_string(),
            (true, _) => past_end("matches", page.offset, seen),
        };
        return join_lines(&[first, unsearched_note(unsearched)]);
    }
    fit(sections, "\n\n", &|shown| {
        let more = !complete || seen > page.offset + shown;
        join_lines(&[
            if more {
                page_footer("matches", page, shown, total)
            } else {
                String::new()
            },
            unsearched_note(unsearched),
        ])
    })
}

/// A count page: one `<path>:<n>` line per matching file, then the total line. A count whose
/// file search stopped at the host line budget (`partial`) is a lower bound, marked `+`.
pub(crate) fn render_count(
    counts: &[(String, usize, bool)],
    page: Page,
    unsearched: usize,
) -> String {
    if counts.is_empty() && unsearched == 0 {
        return "No matches.".to_string();
    }
    let matches: usize = counts.iter().map(|(_, count, _)| count).sum();
    let partial = counts.iter().any(|(_, _, partial)| *partial);
    let total = total_line(matches, partial, counts.len());
    if page.offset >= counts.len() && !counts.is_empty() {
        return join_lines(&[
            past_end("files", page.offset, counts.len()),
            total,
            unsearched_note(unsearched),
        ]);
    }
    let end = page.end().map_or(counts.len(), |end| end.min(counts.len()));
    let sections: Vec<Section> = counts[page.offset.min(end)..end]
        .iter()
        .map(|(path, count, partial)| {
            let plus = if *partial { "+" } else { "" };
            Section::entry(format!("{path}:{count}{plus}"))
        })
        .collect();
    fit(&sections, "\n", &|shown| {
        let more = page.offset + shown < counts.len();
        join_lines(&[
            total.clone(),
            if more {
                page_footer("files", page, shown, Some(counts.len()))
            } else {
                String::new()
            },
            unsearched_note(unsearched),
        ])
    })
}

/// A files page over the matching `paths`, of which `unknown` more exist whose paths the walk
/// could not reach. A page that leaves paths out ends with the exact total and the
/// directories holding most of the paths not shown.
pub(crate) fn render_files_page(paths: &[String], unknown: usize, page: Page) -> String {
    let total = paths.len() + unknown;
    if total == 0 {
        return "No matches.".to_string();
    }
    if page.offset >= paths.len() {
        return if page.offset >= total {
            past_end("files", page.offset, total)
        } else {
            unsearched_note(unknown)
        };
    }
    let end = page.end().map_or(paths.len(), |end| end.min(paths.len()));
    let sections: Vec<Section> = paths[page.offset..end]
        .iter()
        .map(|path| Section::entry(path.clone()))
        .collect();
    fit(&sections, "\n", &|shown| {
        if page.offset + shown >= total {
            return String::new();
        }
        join_lines(&[
            page_footer("files", page, shown, Some(total)),
            directory_summary(paths, page.offset, shown, unknown),
            unsearched_note(unknown),
        ])
    })
}

/// Lay `sections` out within the shared bound. `tail(shown)` is what follows the body when
/// `shown` entries are shown. A cut body ends at the last entry (and its note) that leaves
/// room for the tail; a leading context line with no room is skipped, and only a first
/// entry line with no room at all is itself cut, on a character boundary, as every bounded
/// tool cuts.
fn fit(sections: &[Section], separator: &str, tail: &dyn Fn(usize) -> String) -> String {
    let mut whole = String::new();
    let mut entries = 0;
    for (index, section) in sections.iter().enumerate() {
        for (line_index, line) in section.lines.iter().enumerate() {
            if line_index > 0 {
                whole.push('\n');
            } else if index > 0 {
                whole.push_str(separator);
            }
            whole.push_str(&line.text);
            entries += usize::from(line.entry);
        }
    }
    let text = join_lines(&[whole, tail(entries)]);
    if within_bound(text.len(), newlines(&text)) {
        return text;
    }
    let mut body = String::new();
    let mut body_newlines = 0;
    let mut shown = 0;
    // Where the body ends when cut: after its last entry (with its note), so no heading or
    // context line dangles without the match it belongs to.
    let mut kept = 0;
    // The body before the first entry's leading context: empty, or the first file's heading.
    let mut lead = (0, 0);
    'sections: for section in sections {
        let mut line_index = 0;
        while line_index < section.lines.len() {
            let line = &section.lines[line_index];
            // An entry carries the lines after it up to its file's note, if one follows
            // before the next entry, so a cut never drops the note of an entry it shows.
            let group_end = if line.entry {
                let next = section.lines[line_index + 1..]
                    .iter()
                    .position(|next| next.entry || next.note)
                    .map(|offset| line_index + 1 + offset);
                match next {
                    Some(note) if section.lines[note].note => note,
                    _ => line_index,
                }
            } else {
                line_index
            };
            let prefix = match (body.is_empty(), line_index) {
                (true, _) => "",
                (false, 0) => separator,
                (false, _) => "\n",
            };
            let trailer: String = section.lines[line_index + 1..=group_end]
                .iter()
                .map(|next| format!("\n{}", next.text))
                .collect();
            let after = shown + usize::from(line.entry);
            let rest = tail(after);
            let added_newlines = newlines(prefix) + newlines(&line.text) + newlines(&trailer);
            let lines = body_newlines + added_newlines + 1 + newlines(&rest);
            let bytes =
                body.len() + prefix.len() + line.text.len() + trailer.len() + 1 + rest.len();
            if within_bound(bytes, lines) {
                body.push_str(prefix);
                body.push_str(&line.text);
                body.push_str(&trailer);
                body_newlines += added_newlines;
                if line.entry {
                    shown = after;
                    kept = body.len();
                } else if shown == 0 && line_index == 0 {
                    lead = (body.len(), body_newlines);
                }
                line_index = group_end + 1;
                continue;
            }
            if shown == 0 && line.entry && body.len() > lead.0 {
                // The page's first match does not fit after its leading context: drop that
                // context rather than cut the match or show none (#509), and try it again.
                body.truncate(lead.0);
                body_newlines = lead.1;
                continue;
            }
            if shown == 0 && !line.entry {
                // A leading context line too long for the bound is skipped, so the page still
                // shows its first match and the continuation offset advances.
                line_index += 1;
                continue;
            }
            if shown == 0 && lines < MAX_OUTPUT_LINES {
                let room = MAX_OUTPUT_BYTES
                    .saturating_sub(body.len() + prefix.len() + trailer.len() + 1 + rest.len());
                let mut end = room.min(line.text.len());
                while end > 0 && !line.text.is_char_boundary(end) {
                    end -= 1;
                }
                if end > 0 {
                    body.push_str(prefix);
                    body.push_str(&line.text[..end]);
                    body.push_str(&trailer);
                    shown = after;
                    kept = body.len();
                }
            }
            break 'sections;
        }
    }
    body.truncate(kept);
    join_lines(&[body, tail(shown)])
}

/// The non-empty `parts`, one per line.
fn join_lines(parts: &[String]) -> String {
    parts
        .iter()
        .filter(|part| !part.is_empty())
        .map(String::as_str)
        .collect::<Vec<_>>()
        .join("\n")
}

/// The footer of a page that leaves entries after it: the entries shown, their total when
/// known, and the `offset` that continues.
fn page_footer(noun: &str, page: Page, shown: usize, total: Option<usize>) -> String {
    let next = page.offset + shown;
    let of = total.map_or_else(String::new, |total| format!(" of {total}"));
    format!(
        "[showing {noun} {}-{next}{of}; continue with offset={next}]",
        page.offset + 1
    )
}

/// The answer to an `offset` at or past the last entry.
fn past_end(noun: &str, offset: usize, total: usize) -> String {
    format!("[showing no {noun}: offset {offset} is past the last of {total}]")
}

/// Matching files the walk could not reach: the host refused the listing a resumed walk
/// needs, as it refuses an exceptionally large one.
fn unsearched_note(unsearched: usize) -> String {
    if unsearched == 0 {
        return String::new();
    }
    format!("[{unsearched} more matching files not searched; narrow with path or glob]")
}

fn total_line(matches: usize, partial: bool, files: usize) -> String {
    let plus = if partial { "+" } else { "" };
    format!("[total: {matches}{plus} matches in {files} files]")
}

/// The directories a cut files page names, as the donor's `find` names them.
const SUMMARY_TOP_DIRS: usize = 5;

/// The exact total of a cut files page and the directories holding most of the paths it
/// does not show (those before `first` and after the `shown` ones), so the next call can
/// narrow to one of them. `unknown` paths exist whose directories are not known.
fn directory_summary(paths: &[String], first: usize, shown: usize, unknown: usize) -> String {
    let total = paths.len() + unknown;
    let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
    let after = (first + shown).min(paths.len());
    for path in paths[..first].iter().chain(&paths[after..]) {
        let parent = path.rsplit_once('/').map_or("", |(parent, _)| parent);
        *counts.entry(parent).or_default() += 1;
    }
    let mut dirs: Vec<(&str, usize)> = counts.into_iter().collect();
    // Most omitted paths first; ties by directory, for stable output.
    dirs.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(b.0)));
    let mut top: Vec<String> = dirs
        .iter()
        .take(SUMMARY_TOP_DIRS)
        .map(|(dir, count)| {
            let dir = if dir.is_empty() { "." } else { dir };
            format!("{dir}/ ({count})")
        })
        .collect();
    if dirs.len() > SUMMARY_TOP_DIRS || unknown > 0 {
        top.push("...".to_string());
    }
    format!(
        "[{total} matching files, {shown} shown, {} omitted; omitted by directory: {}]",
        total - shown,
        top.join(", ")
    )
}

/// What a result describes (ADR-0059): a one-line summary and, for a successful search, the
/// matches it reports.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResultSummary {
    pub summary: String,
    pub matches: Option<Matches>,
}

/// The hits and the files of one search result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Matches {
    pub count: usize,
    pub files: Vec<String>,
}

/// Describe the result `content` of a call; `mode` is the mode its input asked for (the
/// default when it did not parse), `paged` whether it named a paging parameter
/// ([`GrepInput::is_paged`]), `ok` whether the result's status is ok.
pub fn describe_result(mode: Mode, paged: bool, ok: bool, content: &str) -> ResultSummary {
    if !ok {
        return ResultSummary {
            summary: content.lines().next().unwrap_or_default().to_string(),
            matches: None,
        };
    }
    let (count, files, file_count) = describe_matches(mode, paged, content);
    let summary = if mode == Mode::Files {
        format!("{} files", files.len())
    } else {
        format!("{count} hits · {file_count} files")
    };
    ResultSummary {
        summary,
        matches: Some(Matches { count, files }),
    }
}

/// The hits, the files shown, and how many files the result speaks of: a count page's total
/// line names every matching file, not only the ones on the page.
fn describe_matches(mode: Mode, paged: bool, content: &str) -> (usize, Vec<String>, usize) {
    if content.trim() == "No matches." {
        return (0, Vec::new(), 0);
    }
    let mut lines: Vec<&str> = content.lines().collect();
    if mode == Mode::Files {
        if paged {
            strip_files_page_tail(&mut lines);
        }
        // A renderer footer follows and names the path on the line before it. A real final
        // filename that merely spells a footer names a different path, so it is kept.
        let strip_footer =
            lines.len() >= 2 && is_after_footer_for(lines[lines.len() - 1], lines[lines.len() - 2]);
        if strip_footer {
            lines.pop();
        }
        let files = lines.into_iter().map(str::to_string).collect::<Vec<_>>();
        let count = files.len();
        return (count, files, count);
    }
    // Only a paged or count render ends in these lines. A content hit line starts with its
    // line number and a count line ends in its count, so neither can spell one.
    let mut total = None;
    while (paged || mode == Mode::Count)
        && let Some(last) = lines.last()
    {
        if mode == Mode::Count
            && let Some(counts) = total_line_counts(last)
        {
            total = Some(counts);
        } else if !is_page_footer(last) && !is_directory_summary(last) && !is_unsearched_note(last)
        {
            break;
        }
        lines.pop();
    }
    if mode == Mode::Count {
        let files: Vec<String> = lines
            .iter()
            .map(|line| match line.rsplit_once(':') {
                Some((path, count)) if is_digits(count.strip_suffix('+').unwrap_or(count)) => {
                    path.to_string()
                }
                _ => (*line).to_string(),
            })
            .collect();
        let (count, file_count) = total.unwrap_or((0, files.len()));
        return (count, files, file_count);
    }
    if lines.is_empty() {
        return (0, Vec::new(), 0);
    }
    let content = lines.join("\n");
    let blocks = content.split("\n\n").collect::<Vec<_>>();
    let count = blocks
        .iter()
        .map(|block| {
            let mut lines: Vec<&str> = block.lines().skip(1).collect();
            let strip_footer = lines.last().is_some_and(|last| {
                let path = block.lines().next().unwrap_or_default();
                is_after_footer_for(last, path) || is_inside_footer_for(last, path)
            });
            if strip_footer {
                lines.pop();
            }
            if lines.last().is_some_and(|last| is_omitted_matches(last)) {
                lines.pop();
            }
            lines.len()
        })
        .sum();
    let files: Vec<String> = blocks
        .iter()
        .filter_map(|block| block.lines().next())
        .map(str::to_string)
        .collect();
    let file_count = files.len();
    (count, files, file_count)
}

fn is_digits(text: &str) -> bool {
    !text.is_empty() && text.chars().all(|character| character.is_ascii_digit())
}

/// `[showing <noun> <a>-<b>[ of <t>]; continue with offset=<b>]` or
/// `[showing no <noun>: offset <n> is past the last of <t>]`.
fn is_page_footer(line: &str) -> bool {
    let Some(rest) = line
        .strip_prefix("[showing ")
        .and_then(|rest| rest.strip_suffix(']'))
    else {
        return false;
    };
    if let Some(rest) = rest.strip_prefix("no ") {
        let Some((noun, rest)) = rest.split_once(": offset ") else {
            return false;
        };
        let Some((offset, total)) = rest.split_once(" is past the last of ") else {
            return false;
        };
        return is_noun(noun) && is_digits(offset) && is_digits(total);
    }
    let Some((range, next)) = rest.split_once("; continue with offset=") else {
        return false;
    };
    let Some((noun, range)) = range.split_once(' ') else {
        return false;
    };
    let range = match range.split_once(" of ") {
        Some((range, total)) if is_digits(total) => range,
        Some(_) => return false,
        None => range,
    };
    let Some((first, last)) = range.split_once('-') else {
        return false;
    };
    is_noun(noun) && is_digits(first) && is_digits(last) && last == next
}

/// Remove the tail a files page ends in, only when it is exactly the renderer's: a lone
/// past-the-end footer or unsearched note, or a footer whose range counts the paths above
/// it, then the directory summary and an optional unsearched note. A real filename that
/// merely spells one of these lines does not satisfy the count, so it is kept.
fn strip_files_page_tail(lines: &mut Vec<&str>) {
    if lines.len() == 1 && (is_page_footer(lines[0]) || is_unsearched_note(lines[0])) {
        let shown = page_footer_range(lines[0]);
        if shown.is_none() {
            lines.clear();
        }
        return;
    }
    let mut end = lines.len();
    if end > 0 && is_unsearched_note(lines[end - 1]) {
        end -= 1;
    }
    if end < 2 || !is_directory_summary(lines[end - 1]) {
        return;
    }
    let footer = end - 2;
    if page_footer_range(lines[footer]) == Some(footer) {
        lines.truncate(footer);
    }
}

/// How many entries a `[showing <noun> <a>-<b>...]` footer says were shown.
fn page_footer_range(line: &str) -> Option<usize> {
    if !is_page_footer(line) {
        return None;
    }
    let range = line.split_once(' ')?.1.split_once(' ')?.1;
    let range = range.split([' ', ';']).next()?;
    let (first, last) = range.split_once('-')?;
    let (first, last): (usize, usize) = (first.parse().ok()?, last.parse().ok()?);
    last.checked_sub(first)?.checked_add(1)
}

fn is_noun(noun: &str) -> bool {
    matches!(noun, "matches" | "files")
}

/// `[<t> matching files, <s> shown, <o> omitted; omitted by directory: ...]`.
fn is_directory_summary(line: &str) -> bool {
    let Some(rest) = line
        .strip_prefix('[')
        .and_then(|rest| rest.strip_suffix(']'))
    else {
        return false;
    };
    let Some((total, rest)) = rest.split_once(" matching files, ") else {
        return false;
    };
    let Some((shown, rest)) = rest.split_once(" shown, ") else {
        return false;
    };
    let Some((omitted, _)) = rest.split_once(" omitted; omitted by directory: ") else {
        return false;
    };
    is_digits(total) && is_digits(shown) && is_digits(omitted)
}

fn is_unsearched_note(line: &str) -> bool {
    line.strip_prefix('[')
        .and_then(|rest| {
            rest.strip_suffix(" more matching files not searched; narrow with path or glob]")
        })
        .is_some_and(is_digits)
}

fn total_line_counts(line: &str) -> Option<(usize, usize)> {
    let rest = line.strip_prefix("[total: ")?.strip_suffix(" files]")?;
    let (matches, files) = rest.split_once(" matches in ")?;
    let matches = matches.strip_suffix('+').unwrap_or(matches);
    if !is_digits(matches) || !is_digits(files) {
        return None;
    }
    Some((matches.parse().ok()?, files.parse().ok()?))
}

/// `… <n> more match(es) in this file`, the last line of a capped file block.
fn is_omitted_matches(line: &str) -> bool {
    let Some(rest) = line.strip_prefix("… ") else {
        return false;
    };
    if let Some(number) = rest
        .strip_prefix("matches after line ")
        .and_then(|rest| rest.strip_suffix(" not searched"))
    {
        return is_digits(number);
    }
    let rest = rest.strip_prefix("at least ").unwrap_or(rest);
    rest.strip_suffix(" more matches in this file")
        .or_else(|| rest.strip_suffix(" more match in this file"))
        .is_some_and(is_digits)
}

/// True when `line` is exactly the files-mode footer `footer_after(path, n)` the renderer
/// emits. Both the path and the digit counts are checked, so an arbitrary filename that
/// merely spells a footer is not treated as a control line.
fn is_after_footer_for(line: &str, path: &str) -> bool {
    let prefix = format!("[truncated after {path}; ");
    let Some(count) = line.strip_prefix(prefix.as_str()).and_then(|rest| {
        rest.strip_suffix(" more matching files not shown; narrow with path or glob]")
    }) else {
        return false;
    };
    !count.is_empty() && count.chars().all(|character| character.is_ascii_digit())
}

/// True when `line` is exactly the content-mode footer `footer_inside(path, n, m)`.
fn is_inside_footer_for(line: &str, path: &str) -> bool {
    let prefix = format!("[truncated inside {path} after line ");
    let Some(count) = line.strip_prefix(prefix.as_str()).and_then(|rest| {
        rest.strip_suffix(
            " more matching files not shown; narrow with path, glob or a stricter pattern]",
        )
    }) else {
        return false;
    };
    let Some((line_number, files)) = count.split_once("; ") else {
        return false;
    };
    !line_number.is_empty()
        && line_number
            .chars()
            .all(|character| character.is_ascii_digit())
        && !files.is_empty()
        && files.chars().all(|character| character.is_ascii_digit())
}

/// `requested` with `.` and `..` collapsed and empty components dropped, `/`-separated; for a
/// relative request inside the root this is how the workspace service displays it.
pub fn lexical_normalize(requested: &str) -> String {
    let mut out: Vec<&str> = Vec::new();
    for component in requested.split('/') {
        match component {
            "" | "." => {}
            ".." => {
                out.pop();
            }
            other => out.push(other),
        }
    }
    out.join("/")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(line_number: u64, text: &str) -> SearchLine {
        SearchLine {
            line_number,
            text: text.to_string(),
            is_match: true,
        }
    }

    fn file(path: &str, lines: Vec<SearchLine>) -> FileMatches {
        FileMatches {
            path: path.to_string(),
            lines,
        }
    }

    fn result(files: Vec<FileMatches>, truncated: bool, omitted_files: u64) -> SearchResult {
        SearchResult {
            files,
            truncated,
            omitted_files,
        }
    }

    #[test]
    fn footer_like_filenames_and_hit_lines_are_preserved() {
        let filename = "[truncated after notes]";
        let files = describe_result(
            Mode::Files,
            false,
            true,
            &render_files(&[filename.into()], 1),
        );
        assert_eq!(files.matches.unwrap().files, vec![filename]);
        let content = render_content(&result(
            vec![file("notes", vec![line(1, "[truncated after notes]")])],
            false,
            0,
        ));
        let matches = describe_result(Mode::Content, false, true, &content)
            .matches
            .unwrap();
        assert_eq!(matches.count, 1);
        assert_eq!(matches.files, vec!["notes"]);
    }

    #[test]
    fn descriptions_exclude_truncation_footer() {
        let paths: Vec<String> = (0..MAX_OUTPUT_LINES + 5)
            .map(|i| format!("file-{i}"))
            .collect();
        let description =
            describe_result(Mode::Files, false, true, &render_files(&paths, paths.len()));
        let matches = description.matches.unwrap();
        assert!(
            !matches
                .files
                .iter()
                .any(|path| path.starts_with("[truncated"))
        );
        assert_eq!(matches.count, matches.files.len());
        let hits = result(
            vec![file(
                "a",
                (1..=MAX_OUTPUT_LINES as u64 + 10)
                    .map(|i| line(i, "hit"))
                    .collect(),
            )],
            true,
            0,
        );
        let text = render_content(&hits);
        let description = describe_result(Mode::Content, false, true, &text);
        assert!(
            !description
                .matches
                .unwrap()
                .files
                .iter()
                .any(|path| path.starts_with("[truncated"))
        );
    }

    #[test]
    fn a_real_filename_equal_to_a_footer_is_kept() {
        let filename =
            "[truncated after x; 1 more matching files not shown; narrow with path or glob]";
        let paths = vec!["a".to_string(), filename.to_string()];
        let description =
            describe_result(Mode::Files, false, true, &render_files(&paths, paths.len()));
        assert_eq!(description.matches.unwrap().files, paths);
    }

    #[test]
    fn the_schema_is_the_declared_one() {
        let schema = input_schema();
        assert_eq!(schema["required"], serde_json::json!(["pattern"]));
        assert_eq!(schema["additionalProperties"], false);
        assert_eq!(schema["properties"].as_object().unwrap().len(), 10);
        assert_eq!(schema["properties"]["literal"]["default"], false);
        assert_eq!(
            schema["properties"]["mode"]["enum"],
            serde_json::json!(["content", "files", "count"])
        );
        assert_eq!(schema["properties"]["offset"]["minimum"], 0);
        assert_eq!(schema["properties"]["head_limit"]["minimum"], 1);
        assert_eq!(schema["properties"]["max_per_file"]["minimum"], 1);
    }

    /// #509 item 1: `literal` escapes every metacharacter of the host's syntax, and without
    /// it the pattern is passed on unchanged.
    #[test]
    fn a_literal_pattern_is_escaped_for_the_host() {
        assert_eq!(escape_regex(r"a.b("), r"a\.b\(");
        assert_eq!(
            escape_regex(r"\.+*?()|[]{}^$#&-~ x_1/é"),
            r"\\\.\+\*\?\(\)\|\[\]\{\}\^\$\#\&\-\~ x_1/é"
        );
        let literal = parse_json_input("grep", r#"{"pattern":"a.b(","literal":true}"#).unwrap();
        assert_eq!(literal.regex(), r"a\.b\(");
        let plain = parse_json_input("grep", r#"{"pattern":"a.b("}"#).unwrap();
        assert!(!plain.literal);
        assert_eq!(plain.regex(), "a.b(");
    }

    #[test]
    fn invalid_inputs_name_the_tool_and_the_reason() {
        assert_eq!(
            parse_json_input("grep", r#"{"pattern":"a","context":11}"#),
            Err("Invalid input for grep: `context` must be between 0 and 10".into())
        );
        let unknown = parse_json_input("Search", r#"{"pattern":"a","z":1}"#).unwrap_err();
        assert!(
            unknown.starts_with("Invalid input for Search: unknown field `z`"),
            "{unknown}"
        );
        assert_eq!(
            text_input_error("grep"),
            "Invalid input for grep: expected a JSON object input, got freeform text"
        );
        let input = parse_json_input("grep", r#"{"pattern":"a","context":3}"#).unwrap();
        assert_eq!(input.context_lines(), 3);
        assert_eq!(
            parse_json_input("grep", r#"{"pattern":"a"}"#)
                .unwrap()
                .context_lines(),
            0
        );
    }

    #[test]
    fn renders_content_grouped_by_file() {
        let mut context = line(1, "fn alpha() {}");
        context.is_match = false;
        let rendered = render_content(&result(
            vec![
                file("src/a.rs", vec![context, line(2, "fn beta() {}")]),
                file("src/b.rs", vec![line(1, "fn beta() {}")]),
            ],
            false,
            0,
        ));
        assert_eq!(
            rendered,
            "src/a.rs\n1-fn alpha() {}\n2:fn beta() {}\n\nsrc/b.rs\n1:fn beta() {}"
        );
        assert_eq!(render_content(&result(Vec::new(), false, 0)), "No matches.");
    }

    #[test]
    fn a_result_stopped_at_the_line_cap_renders_as_the_complete_one() {
        // 1_500 files of one hit each, then one of 800: complete, and cut by the host at
        // exactly MAX_OUTPUT_LINES lines with two more matching files after the cut one.
        let many = |count: usize| -> Vec<FileMatches> {
            (0..count)
                .map(|index| file(&format!("f{index:05}.txt"), vec![line(1, "needle")]))
                .collect()
        };
        let mut complete = many(1_500);
        complete.push(file(
            "g.txt",
            (1..=800).map(|number| line(number, "needle")).collect(),
        ));
        complete.push(file("h.txt", vec![line(1, "needle")]));
        complete.push(file("i.txt", vec![line(1, "needle")]));
        let whole = render_content(&result(complete.clone(), false, 0));

        let mut cut = complete[..1_501].to_vec();
        cut[1_500].lines.truncate(MAX_OUTPUT_LINES - 1_500);
        assert_eq!(render_content(&result(cut, true, 2)), whole);
        assert!(whole.ends_with("; 837 more matching files not shown; narrow with path or glob]"));
    }

    #[test]
    fn a_first_block_over_the_bound_is_cut_inside_it() {
        let lines: Vec<SearchLine> = (1..=3_000).map(|number| line(number, "x")).collect();
        let whole = render_content(&result(vec![file("big.txt", lines.clone())], false, 0));
        let cut = render_content(&result(
            vec![file("big.txt", lines[..MAX_OUTPUT_LINES].to_vec())],
            true,
            1,
        ));
        assert!(whole.ends_with(
            "[truncated inside big.txt after line 1998; 0 more matching files not shown; narrow with path, glob or a stricter pattern]"
        ));
        assert_eq!(cut, whole.replace("; 0 more", "; 1 more"));
    }

    #[test]
    fn renders_files_and_counts_the_ones_not_shown() {
        let paths = vec!["a.md".to_string(), "sub/b.md".to_string()];
        assert_eq!(render_files(&paths, 2), "a.md\nsub/b.md");
        assert_eq!(render_files(&[], 0), "No matches.");
        let many: Vec<String> = (0..2_500).map(|index| format!("f{index:05}")).collect();
        let whole = render_files(&many, many.len());
        assert_eq!(render_files(&many[..MAX_OUTPUT_LINES], many.len()), whole);
        assert!(whole.ends_with(
            "[truncated after f01998; 501 more matching files not shown; narrow with path or glob]"
        ));
    }

    #[test]
    fn describes_targets_and_results() {
        let input = parse_json_input("grep", r#"{"pattern":"beta","path":"src"}"#).unwrap();
        assert_eq!(describe_target(&input), "beta src");
        let input = parse_json_input("grep", r#"{"pattern":"beta"}"#).unwrap();
        assert_eq!(describe_target(&input), "beta .");

        let content = describe_result(Mode::Content, false, true, "a.rs\n1:x\n2:y\n\nb.rs\n3:z");
        assert_eq!(content.summary, "3 hits · 2 files");
        assert_eq!(
            content.matches,
            Some(Matches {
                count: 3,
                files: vec!["a.rs".into(), "b.rs".into()],
            })
        );
        let files = describe_result(Mode::Files, false, true, "a.rs\nb.rs");
        assert_eq!(files.summary, "2 files");
        let none = describe_result(Mode::Content, false, true, "No matches.");
        assert_eq!(none.summary, "0 hits · 0 files");
        let failed = describe_result(Mode::Content, false, false, "nope does not exist.\nmore");
        assert_eq!(failed.summary, "nope does not exist.");
        assert_eq!(failed.matches, None);
    }

    #[test]
    fn lexical_normalization() {
        assert_eq!(lexical_normalize("./a//b/../c"), "a/c");
        assert_eq!(lexical_normalize("nope"), "nope");
    }

    #[test]
    fn paging_inputs_are_validated() {
        for (raw, reason) in [
            (
                r#"{"pattern":"a","offset":-1}"#,
                "`offset` must be 0 or more",
            ),
            (
                r#"{"pattern":"a","head_limit":0}"#,
                "`head_limit` must be at least 1",
            ),
            (
                r#"{"pattern":"a","max_per_file":0}"#,
                "`max_per_file` must be at least 1",
            ),
        ] {
            assert_eq!(
                parse_json_input("grep", raw),
                Err(format!("Invalid input for grep: {reason}"))
            );
        }
        let input =
            parse_json_input("grep", r#"{"pattern":"a","offset":2,"head_limit":3}"#).unwrap();
        assert_eq!(
            input.page(),
            Page {
                offset: 2,
                head_limit: Some(3)
            }
        );
        assert!(input.is_paged());
        // An explicit offset of 0, or a per-file cap outside content mode, changes nothing.
        let plain = parse_json_input("grep", r#"{"pattern":"a","offset":0}"#).unwrap();
        assert!(!plain.is_paged());
        let files =
            parse_json_input("grep", r#"{"pattern":"a","mode":"files","max_per_file":1}"#).unwrap();
        assert!(!files.is_paged());
    }

    fn page(offset: usize, head_limit: Option<usize>) -> Page {
        Page { offset, head_limit }
    }

    fn context_line(line_number: u64, text: &str) -> SearchLine {
        SearchLine {
            is_match: false,
            ..line(line_number, text)
        }
    }

    /// Item 3: a per-file cap keeps the first N matches with their context and names the exact
    /// number of that file's matches left out.
    #[test]
    fn a_per_file_cap_shows_n_matches_and_counts_the_rest() {
        let big = file(
            "a.rs",
            vec![
                context_line(1, "before"),
                line(2, "hit"),
                context_line(3, "between"),
                line(4, "hit"),
                context_line(5, "after"),
                line(6, "hit"),
                line(7, "hit"),
            ],
        );
        let small = file("b.rs", vec![line(9, "hit")]);
        let all = page(0, None);
        let section = content_section(&big, 0, all, Some(2), 1, false).unwrap();
        assert_eq!(kept_matches(&big, Some(2)), 2);
        let second = content_section(&small, 2, all, Some(2), 1, false).unwrap();
        let text = render_content_page(&[section, second], all, 3, true, 0);
        assert_eq!(
            text,
            "a.rs\n1-before\n2:hit\n3-between\n4:hit\n5-after\n… 2 more matches in this file\n\nb.rs\n9:hit"
        );
        let described = describe_result(Mode::Content, true, true, &text);
        assert_eq!(described.summary, "6 hits · 2 files");
        let one_left = content_section(
            &file("c.rs", vec![line(1, "x"), line(2, "x")]),
            0,
            all,
            Some(1),
            0,
            false,
        )
        .unwrap();
        assert_eq!(
            render_content_page(&[one_left], all, 1, true, 0),
            "c.rs\n1:x\n… 1 more match in this file"
        );
    }

    /// Item 2 over content entries: match lines are counted across files.
    #[test]
    fn a_content_page_counts_match_lines_across_files() {
        let files = [
            file("a.rs", vec![line(1, "x"), line(2, "x")]),
            file("b.rs", vec![line(3, "x"), line(4, "x")]),
        ];
        let window = page(1, Some(2));
        let sections: Vec<Section> = [(0, &files[0]), (2, &files[1])]
            .into_iter()
            .filter_map(|(first, group)| content_section(group, first, window, None, 0, false))
            .collect();
        let text = render_content_page(&sections, window, 4, true, 0);
        assert_eq!(
            text,
            "a.rs\n2:x\n\nb.rs\n3:x\n[showing matches 2-3 of 4; continue with offset=3]"
        );
        let described = describe_result(Mode::Content, true, true, &text);
        assert_eq!(described.summary, "2 hits · 2 files");
        assert_eq!(
            render_content_page(&[], page(9, Some(2)), 4, true, 0),
            "[showing no matches: offset 9 is past the last of 4]"
        );
    }

    /// Item 1's rendering and description.
    #[test]
    fn count_lines_page_and_describe() {
        let counts = vec![
            ("a.rs".to_string(), 3, false),
            ("b.rs".to_string(), 1, false),
            ("c.rs".to_string(), 2, false),
        ];
        let whole = render_count(&counts, Page::default(), 0);
        assert_eq!(
            whole,
            "a.rs:3\nb.rs:1\nc.rs:2\n[total: 6 matches in 3 files]"
        );
        let described = describe_result(Mode::Count, false, true, &whole);
        assert_eq!(described.summary, "6 hits · 3 files");
        assert_eq!(
            described.matches.unwrap().files,
            vec!["a.rs", "b.rs", "c.rs"]
        );
        let paged = render_count(&counts, page(0, Some(2)), 0);
        assert_eq!(
            paged,
            "a.rs:3\nb.rs:1\n[total: 6 matches in 3 files]\n[showing files 1-2 of 3; continue with offset=2]"
        );
        let described = describe_result(Mode::Count, true, true, &paged);
        assert_eq!(described.summary, "6 hits · 3 files");
        assert_eq!(described.matches.unwrap().files, vec!["a.rs", "b.rs"]);
        assert_eq!(render_count(&[], Page::default(), 0), "No matches.");
    }

    /// Item 2's example: seven matching files, offset 2, head_limit 3.
    #[test]
    fn a_files_page_shows_files_three_to_five_and_names_offset_five() {
        let paths: Vec<String> = (1..=7).map(|index| format!("f{index}")).collect();
        let text = render_files_page(&paths, 0, page(2, Some(3)));
        assert_eq!(
            text,
            "f3\nf4\nf5\n[showing files 3-5 of 7; continue with offset=5]\n[7 matching files, 3 shown, 4 omitted; omitted by directory: ./ (4)]"
        );
        let described = describe_result(Mode::Files, true, true, &text);
        assert_eq!(described.matches.unwrap().files, vec!["f3", "f4", "f5"]);
        // The last page has nothing after it, so no footer and no summary.
        assert_eq!(render_files_page(&paths, 0, page(5, Some(3))), "f6\nf7");
        assert_eq!(
            render_files_page(&paths, 0, page(7, None)),
            "[showing no files: offset 7 is past the last of 7]"
        );
    }

    /// Item 4: a files listing cut by head_limit names the exact total and the top five
    /// directories by omitted paths, most first, ties by name.
    #[test]
    fn a_cut_files_page_summarizes_the_omitted_directories() {
        let mut paths = Vec::new();
        for (dir, count) in [("a", 1), ("b", 6), ("c", 2), ("d", 3), ("e", 2), ("f", 1)] {
            for index in 0..count {
                paths.push(format!("{dir}/x{index}"));
            }
        }
        paths.push("top".to_string());
        paths.sort();
        let text = render_files_page(&paths, 0, page(0, Some(1)));
        assert!(text.starts_with("a/x0\n[showing files 1-1 of 16; continue with offset=1]\n"));
        assert!(
            text.ends_with(
                "[16 matching files, 1 shown, 15 omitted; omitted by directory: b/ (6), d/ (3), c/ (2), e/ (2), ./ (1), ...]"
            ),
            "{text}"
        );
        // Paths the walk could not reach still count in the total, and the unsearched note
        // follows the summary (review H6).
        let unknown = render_files_page(&paths[..2], 3, page(0, Some(1)));
        assert!(
            unknown.ends_with(
                "[5 matching files, 1 shown, 4 omitted; omitted by directory: b/ (1), ...]\n[3 more matching files not searched; narrow with path or glob]"
            ),
            "{unknown}"
        );
    }

    /// Item 4 under the byte/line bound: the page stops where the footer and the summary still
    /// fit, and the summary counts exactly what it left out.
    #[test]
    fn a_files_page_cut_by_the_bound_keeps_room_for_its_summary() {
        let paths: Vec<String> = (0..2_500)
            .map(|index| format!("d{}/file-{index:05}", index % 7))
            .collect();
        let text = render_files_page(&paths, 0, page(0, Some(2_400)));
        assert!(within_bound(text.len(), newlines(&text)), "over the bound");
        let lines: Vec<&str> = text.lines().collect();
        let shown = lines.len() - 2;
        assert_eq!(
            lines[..shown],
            paths
                .iter()
                .take(shown)
                .map(String::as_str)
                .collect::<Vec<_>>()[..]
        );
        assert_eq!(
            lines[shown],
            format!("[showing files 1-{shown} of 2500; continue with offset={shown}]")
        );
        assert!(lines[shown + 1].starts_with(&format!(
            "[2500 matching files, {shown} shown, {} omitted; omitted by directory: ",
            2_500 - shown
        )));
        assert!(shown < MAX_OUTPUT_LINES);
    }

    /// Review H2: a leading context line over the bound is skipped, so the page shows its
    /// match and the continuation offset advances instead of repeating offset 0.
    #[test]
    fn an_oversized_leading_context_line_does_not_stall_the_page() {
        let group = file(
            "a.txt",
            vec![context_line(1, &"y".repeat(60_000)), line(2, "needle")],
        );
        let window = page(0, Some(1));
        let section = content_section(&group, 0, window, None, 1, false).unwrap();
        let text = render_content_page(&[section], window, 1, true, 0);
        assert_eq!(text, "a.txt\n2:needle");
    }

    /// Review H3: the bound never separates a shown entry from its file's omission note.
    #[test]
    fn a_cut_keeps_the_note_of_the_last_entry_shown() {
        let all = Page::default();
        let a = file("a.txt", vec![line(1, "x"), line(2, "x")]);
        let b = file("b.txt", vec![line(1, &"x".repeat(60_000))]);
        let sections = [
            content_section(&a, 0, all, Some(1), 0, false).unwrap(),
            content_section(&b, 1, all, Some(1), 0, false).unwrap(),
        ];
        assert_eq!(
            render_content_page(&sections, all, 2, true, 0),
            "a.txt\n1:x\n… 1 more match in this file\n[showing matches 1-1 of 2; continue with offset=1]"
        );
    }

    /// Review H4: a count too large for a 32-bit guest saturates rather than becoming 0.
    #[test]
    fn a_huge_offset_saturates() {
        let input = parse_json_input("grep", r#"{"pattern":"x","offset":4294967296}"#).unwrap();
        assert!(input.page().offset >= u32::MAX as usize);
        assert_eq!(
            saturating_usize(i64::MAX),
            usize::try_from(i64::MAX).unwrap_or(usize::MAX)
        );
        assert_eq!(saturating_usize(7), 7);
    }

    /// Review H7: only a paged call's own tail is stripped from a files description, so a
    /// real filename spelling a footer stays a file.
    #[test]
    fn a_filename_spelling_a_page_footer_is_kept() {
        let name = "[showing files 1-1 of 2; continue with offset=1]";
        let unpaged = describe_result(Mode::Files, false, true, name);
        assert_eq!(unpaged.matches.unwrap().files, vec![name]);
        let paged = describe_result(Mode::Files, true, true, name);
        assert_eq!(paged.matches.unwrap().files, vec![name]);
        // The renderer's own tail is still recognized, its range counting the paths above.
        let paths = vec![name.to_string(), "b".to_string()];
        let text = render_files_page(&paths, 0, page(0, Some(1)));
        let described = describe_result(Mode::Files, true, true, &text);
        assert_eq!(described.matches.unwrap().files, vec![name]);
    }

    #[test]
    fn partial_counts_and_notes_are_lower_bounds() {
        let counts = vec![
            ("a.rs".to_string(), 5, true),
            ("b.rs".to_string(), 1, false),
        ];
        let text = render_count(&counts, Page::default(), 0);
        assert_eq!(text, "a.rs:5+\nb.rs:1\n[total: 6+ matches in 2 files]");
        let described = describe_result(Mode::Count, false, true, &text);
        assert_eq!(described.summary, "6 hits · 2 files");
        assert_eq!(described.matches.unwrap().files, vec!["a.rs", "b.rs"]);
        let all = Page::default();
        let partial = file("a.rs", vec![line(1, "x"), line(2, "x")]);
        let capped = content_section(&partial, 0, all, Some(1), 0, true).unwrap();
        let uncapped = content_section(&partial, 0, all, None, 0, true).unwrap();
        assert_eq!(
            render_content_page(&[capped], all, 1, true, 0),
            "a.rs\n1:x\n… at least 1 more match in this file"
        );
        let text = render_content_page(&[uncapped], all, 2, true, 0);
        assert_eq!(text, "a.rs\n1:x\n2:x\n… matches after line 2 not searched");
        assert_eq!(
            describe_result(Mode::Content, true, true, &text).summary,
            "2 hits · 1 files"
        );
    }
}
