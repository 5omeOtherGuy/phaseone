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

use serde::Deserialize;

use crate::exec::{FileMatches, SearchLine, SearchResult};

/// The default model-facing tool name.
pub const NAME: &str = "grep";
/// The default model-facing description.
pub const DESCRIPTION: &str = "Search workspace files with a regular expression.\n`mode:\"content\"` (default) groups matching lines by file, with up to `context` surrounding lines; `mode:\"files\"` lists the matching paths, or every file matching `glob` when `pattern` is empty.\nHonours .gitignore, skips hidden and binary files, and never follows symlinks.";
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

/// What a search returns: matching lines grouped by file, or matching paths.
#[derive(Debug, Clone, Copy, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    #[default]
    Content,
    Files,
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
    pub context: Option<i64>,
}

impl GrepInput {
    /// The context lines asked for; validation keeps it within `0..=MAX_CONTEXT`.
    pub fn context_lines(&self) -> u32 {
        self.context
            .and_then(|context| u32::try_from(context).ok())
            .unwrap_or(DEFAULT_CONTEXT as u32)
    }
}

/// Parse and validate a JSON input; `tool` is the name the model called, for the message.
pub fn parse_json_input(tool: &str, raw: &str) -> Result<GrepInput, String> {
    let input: GrepInput =
        serde_json::from_str(raw).map_err(|error| invalid(tool, &error.to_string()))?;
    if matches!(input.context, Some(context) if !(0..=MAX_CONTEXT as i64).contains(&context)) {
        return Err(invalid(tool, "`context` must be between 0 and 10"));
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
/// `result.files` are the matching files in walk order and `result.omitted_files` the
/// matching files after them. A result the host stopped at its line cap renders exactly as
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
/// paths in walk order and `total` how many paths match in all.
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

/// Keep whole blocks, in walk order, while each one still leaves room for the footer that
/// replaces everything after it; `total` counts the matching files the footer accounts for.
/// `None` when not even the first block fits.
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

/// Describe the result `content` of a call; `files_mode` is whether its input parsed and
/// asked for `mode:"files"`, `ok` whether the result's status is ok.
pub fn describe_result(files_mode: bool, ok: bool, content: &str) -> ResultSummary {
    if !ok {
        return ResultSummary {
            summary: content.lines().next().unwrap_or_default().to_string(),
            matches: None,
        };
    }
    let (count, files) = describe_matches(content, files_mode);
    let summary = if files_mode {
        format!("{} files", files.len())
    } else {
        format!("{count} hits · {} files", files.len())
    };
    ResultSummary {
        summary,
        matches: Some(Matches { count, files }),
    }
}

fn describe_matches(content: &str, files_mode: bool) -> (usize, Vec<String>) {
    if content.trim() == "No matches." {
        return (0, Vec::new());
    }
    if files_mode {
        let mut lines: Vec<&str> = content.lines().collect();
        if lines.len() > 1 && lines.last().is_some_and(|line| is_truncation_footer(line)) {
            lines.pop();
        }
        let files = lines.into_iter().map(str::to_string).collect::<Vec<_>>();
        return (files.len(), files);
    }
    let blocks = content.split("\n\n").collect::<Vec<_>>();
    let count = blocks
        .iter()
        .map(|block| {
            let mut lines: Vec<&str> = block.lines().skip(1).collect();
            if lines.last().is_some_and(|line| is_truncation_footer(line)) {
                lines.pop();
            }
            lines.len()
        })
        .sum();
    let files = blocks
        .iter()
        .filter_map(|block| block.lines().next())
        .map(str::to_string)
        .collect();
    (count, files)
}

fn is_truncation_footer(line: &str) -> bool {
    let rest = line
        .strip_prefix("[truncated after ")
        .or_else(|| line.strip_prefix("[truncated inside "));
    let Some((_, tail)) = rest.and_then(|rest| rest.rsplit_once("; ")) else {
        return false;
    };
    // A footer is a complete renderer sentinel, not an arbitrary filename with
    // the same prefix. Only the final line of a block can be a control line.
    let suffix = tail == "narrow with path or glob]"
        || tail == "narrow with path, glob or a stricter pattern]";
    suffix && line.contains(" more matching files not shown; ")
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
        let files = describe_result(true, true, &render_files(&[filename.into()], 1));
        assert_eq!(files.matches.unwrap().files, vec![filename]);
        let content = render_content(&result(
            vec![file("notes", vec![line(1, "[truncated after notes]")])],
            false,
            0,
        ));
        let matches = describe_result(false, true, &content).matches.unwrap();
        assert_eq!(matches.count, 1);
        assert_eq!(matches.files, vec!["notes"]);
    }

    #[test]
    fn descriptions_exclude_truncation_footer() {
        let paths: Vec<String> = (0..MAX_OUTPUT_LINES + 5)
            .map(|i| format!("file-{i}"))
            .collect();
        let description = describe_result(true, true, &render_files(&paths, paths.len()));
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
        let description = describe_result(false, true, &text);
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
    fn the_schema_is_the_declared_one() {
        let schema = input_schema();
        assert_eq!(schema["required"], serde_json::json!(["pattern"]));
        assert_eq!(schema["additionalProperties"], false);
        assert_eq!(schema["properties"].as_object().unwrap().len(), 6);
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

        let content = describe_result(false, true, "a.rs\n1:x\n2:y\n\nb.rs\n3:z");
        assert_eq!(content.summary, "3 hits · 2 files");
        assert_eq!(
            content.matches,
            Some(Matches {
                count: 3,
                files: vec!["a.rs".into(), "b.rs".into()],
            })
        );
        let files = describe_result(true, true, "a.rs\nb.rs");
        assert_eq!(files.summary, "2 files");
        let none = describe_result(false, true, "No matches.");
        assert_eq!(none.summary, "0 hits · 0 files");
        let failed = describe_result(false, false, "nope does not exist.\nmore");
        assert_eq!(failed.summary, "nope does not exist.");
        assert_eq!(failed.matches, None);
    }

    #[test]
    fn lexical_normalization() {
        assert_eq!(lexical_normalize("./a//b/../c"), "a/c");
        assert_eq!(lexical_normalize("nope"), "nope");
    }
}
