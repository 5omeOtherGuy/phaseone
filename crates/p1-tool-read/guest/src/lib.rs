//! The guest behaviour of the `read` tool, shared by the native adapter (`ReadTool` in
//! `p1-tool-read`) and the component (`modules/p1-module-read`, `p1/read`).
//!
//! Everything here is pure computation over bytes the caller hands in (decision D-XO-8): input
//! parsing and validation, the declaration, the call and result descriptions, line numbering,
//! truncation and the continuation footer, and the model-facing wording of every outcome. How
//! the bytes are obtained — `std::fs` natively, the `workspace` capability in the component —
//! and where the observation is recorded are the caller's, so both hosts run this same source
//! (decision S0-R3, docs/design/modules/package.md "Shared guest logic").

use serde::Deserialize;

/// The tool's default name.
pub const NAME: &str = "read";
/// The tool's default description.
pub const DESCRIPTION: &str = "Read a UTF-8 text file from the workspace, with numbered lines.\nUse `offset` and `limit` to page through a long file; the last line gives the next offset.\nRead a file before you edit or overwrite it: a mutation is refused until you have seen its current contents.\n`skim` hides comments, docstrings, and blank lines for exploration but never satisfies the full-read prerequisite for mutation.";
/// The verb of every call description (ADR-0057).
pub const VERB: &str = "read";
/// The first line when the input names none.
pub const DEFAULT_OFFSET: i64 = 1;
/// The number of lines when the input names none.
pub const DEFAULT_LIMIT: i64 = 2_000;
/// The most bytes of rendered output, footers aside.
pub const MAX_OUTPUT_BYTES: usize = 50_000;
/// The most rendered lines, whatever `limit` asks for.
pub const MAX_OUTPUT_LINES: usize = 2_000;
/// A NUL anywhere in the first 8 KiB marks the file as binary.
pub const BINARY_SNIFF_BYTES: usize = 8 * 1024;
/// The chunk a caller reads the file in: fixed and small, however large the file is.
pub const READ_BUFFER_BYTES: usize = 64 * 1024;

/// The tool's JSON input schema.
pub fn input_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "file_path": {
                "type": "string",
                "description": "File path, relative to the workspace root or absolute inside it."
            },
            "offset": {
                "type": "integer",
                "minimum": 1,
                "default": 1,
                "description": "First line to return (1-indexed)."
            },
            "limit": {
                "type": "integer",
                "minimum": 1,
                "default": 2000,
                "description": "Maximum number of lines to return."
            },
            "skim": {
                "type": "boolean",
                "default": false,
                "description": "Hide comments, docstrings, and blank lines while preserving original line numbers."
            }
        },
        "required": ["file_path"],
        "additionalProperties": false
    })
}

/// A call's raw input, as either host carries it.
#[derive(Debug, Clone, Copy)]
pub enum RawInput<'a> {
    /// A function call's JSON arguments.
    Json(&'a str),
    /// A freeform call's text, which `read` does not take.
    Text(&'a str),
}

/// The validated input of one call.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReadInput {
    /// As the model wrote it: workspace-relative, or absolute inside the root.
    pub file_path: String,
    /// The first line, one-based.
    #[serde(default)]
    pub offset: Option<i64>,
    /// The most lines to show.
    #[serde(default)]
    pub limit: Option<i64>,
    /// Hide comments, docstrings and blank lines, keeping the original line numbers.
    /// A skimmed read records no observation: it never satisfies read-before-mutate.
    #[serde(default)]
    pub skim: bool,
}

/// Parse and validate `raw` for the tool presented as `tool`.
pub fn parse_input(tool: &str, raw: RawInput<'_>) -> Result<ReadInput, String> {
    let raw = match raw {
        RawInput::Json(raw) => raw,
        RawInput::Text(_) => {
            return Err(invalid(
                tool,
                "expected a JSON object input, got freeform text",
            ));
        }
    };
    let input: ReadInput =
        serde_json::from_str(raw).map_err(|error| invalid(tool, &error.to_string()))?;
    if matches!(input.offset, Some(offset) if offset < 1) {
        return Err(invalid(tool, "`offset` must be at least 1"));
    }
    if matches!(input.limit, Some(limit) if limit < 1) {
        return Err(invalid(tool, "`limit` must be at least 1"));
    }
    Ok(input)
}

/// The invalid-input message the model acts on.
pub fn invalid(tool: &str, reason: &str) -> String {
    format!("Invalid input for {tool}: {reason}")
}

/// ADR-0057: the file a call reads, with its line window when the input names one; `None`
/// for input that does not parse — never a guess.
pub fn describe_target(tool: &str, raw: RawInput<'_>) -> Option<String> {
    parse_input(tool, raw).ok().map(|input| {
        let mut target = input.file_path;
        if input.offset.is_some() || input.limit.is_some() {
            let start = input.offset.unwrap_or(DEFAULT_OFFSET);
            let end = start.saturating_add(input.limit.unwrap_or(DEFAULT_LIMIT).saturating_sub(1));
            target = format!("{target}:{start}-{end}");
        }
        target
    })
}

#[cfg(test)]
mod overflow_tests {
    use super::*;
    #[test]
    fn huge_read_window_does_not_overflow() {
        let raw = format!(
            r#"{{"file_path":"a", "offset":{}, "limit":{}}}"#,
            i64::MAX,
            i64::MAX
        );
        assert!(
            describe_target(NAME, RawInput::Json(&raw))
                .unwrap()
                .contains(&i64::MAX.to_string())
        );
    }
}

/// The one-line summary of a result the model was shown: its size for a successful read,
/// otherwise its first line.
pub fn describe_result(content: &str, ok: bool) -> String {
    if !ok {
        return content.lines().next().unwrap_or_default().to_string();
    }
    format!(
        "{} lines · {:.1} kB",
        content.lines().count(),
        content.len() as f64 / 1000.0
    )
}

/// Issue #142: why a credential file is refused, naming the path as the host displays it.
pub fn credential_refusal(display: &str) -> String {
    format!(
        "read refuses credential files ({display}); credentials never enter the model's context"
    )
}

/// The path does not exist.
pub fn missing(display: &str) -> String {
    format!("{display} does not exist.")
}

/// The path resolves outside the workspace: `p1-workspace`'s own wording, which the native
/// tool shows as it is.
pub fn outside_workspace(requested: &str) -> String {
    format!("path escapes workspace: {requested}")
}

/// How the workspace displays `requested` where it names nothing, so the host could not:
/// a relative request with `.` and `..` collapsed as the workspace resolves it. An absolute
/// request stays as written, since the guest does not know the root to strip.
pub fn display_of_request(requested: &str) -> String {
    if requested.starts_with('/') {
        return requested.to_string();
    }
    let mut parts: Vec<&str> = Vec::new();
    for part in requested.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            name => parts.push(name),
        }
    }
    parts.join("/")
}

/// The path is a directory or anything else but a regular file.
pub fn not_a_regular_file(display: &str) -> String {
    format!("{display} is not a regular file.")
}

/// A filesystem failure while reading, as the host words `error`.
pub fn could_not_be_read(display: &str, error: &str) -> String {
    format!("{display} could not be read: {error}")
}

/// The successful read of an empty file.
pub fn empty(display: &str) -> String {
    format!("{display} is empty.")
}

/// How many leading bytes of a `total_len`-byte file [`WindowedRender::start`] sniffs.
pub fn sniff_len(total_len: u64) -> usize {
    total_len.min(BINARY_SNIFF_BYTES as u64) as usize
}

/// Incremental UTF-8 validation with only an incomplete trailing character
/// retained between chunks.
#[derive(Default)]
pub struct Utf8Validator {
    pending: [u8; 4],
    pending_len: usize,
}

impl Utf8Validator {
    /// Validate the next chunk, in file order.
    #[allow(clippy::result_unit_err)]
    pub fn update(&mut self, mut bytes: &[u8]) -> Result<(), ()> {
        if self.pending_len > 0 {
            let character_len = match self.pending[0] {
                0xC2..=0xDF => 2,
                0xE0..=0xEF => 3,
                0xF0..=0xF4 => 4,
                _ => return Err(()),
            };
            let take = bytes.len().min(character_len - self.pending_len);
            self.pending[self.pending_len..self.pending_len + take].copy_from_slice(&bytes[..take]);
            self.pending_len += take;
            bytes = &bytes[take..];

            if self.pending_len == character_len {
                std::str::from_utf8(&self.pending[..self.pending_len]).map_err(|_| ())?;
                self.pending_len = 0;
            }
        }

        if self.pending_len == 0
            && let Err(error) = std::str::from_utf8(bytes)
        {
            if error.error_len().is_some() {
                return Err(());
            }
            let incomplete = &bytes[error.valid_up_to()..];
            self.pending[..incomplete.len()].copy_from_slice(incomplete);
            self.pending_len = incomplete.len();
        }
        Ok(())
    }

    /// Whether the input ended on a character boundary.
    #[allow(clippy::result_unit_err)]
    pub fn finish(self) -> Result<(), ()> {
        if self.pending_len == 0 {
            Ok(())
        } else {
            Err(())
        }
    }
}

/// Why a skim request was answered with the full window: a skim that cannot help is
/// never worth a second look from the model, so it is reported in one line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkimFallback {
    /// The file type is never skimmed: data formats and unknown extensions.
    NeverSkimmed,
    /// Stripping removed every line of a non-empty window.
    Emptied,
    /// The skimmed window was no smaller than the full one.
    NotSmaller,
}

impl SkimFallback {
    /// The reason as the model reads it.
    pub fn reason(self) -> &'static str {
        match self {
            SkimFallback::NeverSkimmed => "this file type is never skimmed",
            SkimFallback::Emptied => "the skim emptied the window",
            SkimFallback::NotSmaller => "the skim was not smaller",
        }
    }
}

/// Whole-line comment syntax for one language family: extension to rules, resolved once
/// per read. Ported from the donor's `skim.rs` (a parts donor, ADR-0001), which ports
/// RTK's `MinimalFilter`; the same two deliberate deviations are kept: doc comments and
/// docstrings are stripped too (skim is for exploration, not API reading), and a line is a
/// comment only when its *trimmed* text starts with the marker, so mid-line markers never
/// strip code.
struct Rules {
    /// A line whose trimmed text starts with one of these is a comment line.
    line: &'static [&'static str],
    /// Block comment delimiters, entered only when the trimmed line *starts* with the opener.
    block: Option<(&'static str, &'static str)>,
    /// Python-style triple-quoted docstrings (`"""` / `'''`).
    docstrings: bool,
}

const C_STYLE: Rules = Rules {
    line: &["//"],
    block: Some(("/*", "*/")),
    docstrings: false,
};
const PYTHON: Rules = Rules {
    line: &["#"],
    block: None,
    docstrings: true,
};
const RUBY: Rules = Rules {
    line: &["#"],
    block: Some(("=begin", "=end")),
    docstrings: false,
};
const HASH: Rules = Rules {
    line: &["#"],
    block: None,
    docstrings: false,
};

/// Comment syntax by file extension (lowercased). `None` means "never strip": data
/// formats (JSON/YAML/TOML/XML/CSV), prose and unknown extensions pass through untouched
/// — a comment-shaped line in data is data.
fn rules(extension: &str) -> Option<&'static Rules> {
    match extension.to_ascii_lowercase().as_str() {
        "rs" | "js" | "mjs" | "cjs" | "jsx" | "ts" | "tsx" | "go" | "c" | "h" | "cpp" | "cc"
        | "cxx" | "hpp" | "hh" | "java" => Some(&C_STYLE),
        "py" | "pyw" => Some(&PYTHON),
        "rb" => Some(&RUBY),
        "sh" | "bash" | "zsh" => Some(&HASH),
        _ => None,
    }
}

enum SkimState {
    Code,
    Block(&'static str),
    Docstring(&'static str),
}

/// The per-line keep/strip decision for one skimmed read, over the *original* file lines in
/// order: kept lines are rendered with their true line numbers, so offsets and follow-up
/// full reads stay coherent. Fed every line of the file, including lines outside the
/// window, because a block comment or docstring carries its state across them.
pub struct SkimFilter {
    rules: &'static Rules,
    state: SkimState,
    /// Whether the last kept code line ends with `:` (`None` before any kept line). Gates
    /// docstring detection to docstring positions.
    prev_ends_colon: Option<bool>,
    index: usize,
}

/// The filter for `path`'s extension, or `None` when its type is never skimmed. The
/// extension is taken from the requested path so both hosts decide alike, without either
/// resolving the workspace itself.
pub fn skim_filter(path: &str) -> Option<SkimFilter> {
    let extension = std::path::Path::new(path).extension()?.to_str()?;
    Some(SkimFilter {
        rules: rules(extension)?,
        state: SkimState::Code,
        prev_ends_colon: None,
        index: 0,
    })
}

impl SkimFilter {
    /// Whether the next line survives, given its text. `complete` is false when only a
    /// bounded prefix of a very long line was scanned: the line is then kept, because
    /// stripping it would decide on bytes this read no longer holds.
    pub fn keep(&mut self, line: &str, complete: bool) -> bool {
        let trimmed = line.trim();
        let index = self.index;
        self.index += 1;
        let keep = match self.state {
            SkimState::Block(end) => match trimmed.find(end) {
                Some(pos) => {
                    self.state = SkimState::Code;
                    // Code after the closing delimiter: keep the line.
                    !trimmed[pos + end.len()..].trim().is_empty() || !complete
                }
                None => false,
            },
            SkimState::Docstring(delim) => match trimmed.find(delim) {
                Some(pos) => {
                    self.state = SkimState::Code;
                    !trimmed[pos + delim.len()..].trim().is_empty() || !complete
                }
                None => false,
            },
            SkimState::Code => {
                if trimmed.is_empty() {
                    false
                }
                // Shebangs carry meaning; never strip line 1's `#!`.
                else if index == 0 && trimmed.starts_with("#!") {
                    true
                } else if let Some((start, end)) = self.rules.block
                    && let Some(rest) = trimmed.strip_prefix(start)
                {
                    match rest.find(end) {
                        // Closes on the same line: keep only if code follows the delimiter.
                        Some(pos) => !rest[pos + end.len()..].trim().is_empty() || !complete,
                        None => {
                            self.state = SkimState::Block(end);
                            false
                        }
                    }
                } else if self.rules.docstrings
                    && self.prev_ends_colon.is_none_or(|colon| colon)
                    && let Some((delim, rest)) = ["\"\"\"", "'''"]
                        .iter()
                        .find_map(|d| trimmed.strip_prefix(d).map(|rest| (*d, rest)))
                {
                    match rest.find(delim) {
                        Some(pos) => !rest[pos + delim.len()..].trim().is_empty() || !complete,
                        None => {
                            self.state = SkimState::Docstring(delim);
                            false
                        }
                    }
                } else {
                    !self
                        .rules
                        .line
                        .iter()
                        .any(|marker| trimmed.starts_with(marker))
                }
            }
        };
        if keep {
            self.prev_ends_colon = Some(trimmed.ends_with(':'));
        }
        keep
    }
}

/// The longest whole-character prefix of `bytes`.
fn valid_utf8_prefix(bytes: &[u8]) -> &str {
    let end = match std::str::from_utf8(bytes) {
        Ok(_) => bytes.len(),
        Err(error) => error.valid_up_to(),
    };
    std::str::from_utf8(&bytes[..end]).expect("validated prefix")
}

/// The bounded portion of the line currently being scanned.
struct LineBuffer {
    shown: Vec<u8>,
    content_bytes: usize,
    last_byte: Option<u8>,
    /// The retained prefix is shorter than the line: a decision that would need the
    /// dropped bytes must keep the line instead of guessing.
    truncated: bool,
}

impl LineBuffer {
    fn new() -> Self {
        Self {
            shown: Vec::with_capacity(MAX_OUTPUT_BYTES),
            content_bytes: 0,
            last_byte: None,
            truncated: false,
        }
    }

    fn push(&mut self, bytes: &[u8], retain: bool) {
        self.content_bytes += bytes.len();
        if let Some(last) = bytes.last() {
            self.last_byte = Some(*last);
        }
        if retain {
            let keep = bytes
                .len()
                .min(MAX_OUTPUT_BYTES.saturating_sub(self.shown.len()));
            self.truncated |= keep < bytes.len();
            self.shown.extend_from_slice(&bytes[..keep]);
        }
    }

    /// The retained prefix as text: a skim decision reads it whole.
    fn text(&self) -> &str {
        valid_utf8_prefix(&self.shown)
    }

    fn reset(&mut self) {
        self.shown.clear();
        self.content_bytes = 0;
        self.last_byte = None;
        self.truncated = false;
    }
}

struct Window {
    start: usize,
    cap: usize,
    line_number: usize,
    emitted: usize,
    /// Lines of the window passed so far, shown or hidden by a skim: `limit` counts the
    /// file's ORIGINAL lines, as the donor's masked window does.
    consumed: usize,
    end: usize,
    stop_collecting: bool,
    out: String,
}

impl Window {
    /// A window over a `limit`-line request starting at zero-based line `start`.
    fn new(start: usize, limit: usize) -> Self {
        Self {
            start,
            cap: limit.min(MAX_OUTPUT_LINES),
            line_number: 0,
            emitted: 0,
            consumed: 0,
            end: start,
            stop_collecting: false,
            out: String::new(),
        }
    }

    /// The rendered lines with the continuation footer, which counts the file's ORIGINAL
    /// lines: an offset a model feeds back is a real line number, not a skimmed one.
    fn rendered(self, total: usize) -> String {
        let mut out = self.out;
        if self.end < total {
            out.push('\n');
            out.push_str(&format!(
                "[{} more lines; continue with offset={}]",
                total - self.end,
                self.end + 1
            ));
        }
        out
    }

    fn wants_current_line(&self) -> bool {
        self.line_number + 1 > self.start && !self.stop_collecting && self.consumed < self.cap
    }

    fn finish_line(&mut self, line: &mut LineBuffer, keep: bool) {
        self.line_number += 1;
        if !self.wants_finished_line() {
            line.reset();
            return;
        }
        if !keep {
            // A hidden line still spends the window, so the footer's offset follows it.
            self.end = self.line_number;
            self.consumed += 1;
            line.reset();
            return;
        }

        let content_bytes = line.content_bytes - usize::from(line.last_byte == Some(b'\r'));
        line.shown.truncate(line.shown.len().min(content_bytes));
        line.shown.truncate(valid_utf8_prefix(&line.shown).len());

        let prefix = format!("{:>6}\t", self.line_number);
        let rendered_bytes = prefix.len() + content_bytes;
        if self.emitted > 0 && self.out.len() + 1 + rendered_bytes > MAX_OUTPUT_BYTES {
            self.stop_collecting = true;
            line.reset();
            return;
        }

        if self.emitted > 0 {
            self.out.push('\n');
        }
        self.out.push_str(&prefix);
        if rendered_bytes <= MAX_OUTPUT_BYTES {
            self.out.push_str(valid_utf8_prefix(&line.shown));
        } else {
            let available = MAX_OUTPUT_BYTES.saturating_sub(self.out.len());
            let mut display_end = available.min(line.shown.len());
            while display_end > 0 && std::str::from_utf8(&line.shown[..display_end]).is_err() {
                display_end -= 1;
            }
            self.out
                .push_str(std::str::from_utf8(&line.shown[..display_end]).expect("UTF-8 boundary"));
            let shown_bytes = self.out.len();
            self.out.push('\n');
            self.out.push_str(&format!(
                "[output truncated: showing {shown_bytes} of {rendered_bytes} bytes]"
            ));
            self.out.push('\n');
            self.out.push_str(&format!(
                "[{} bytes omitted from line {}]",
                content_bytes - display_end,
                self.line_number
            ));
            self.stop_collecting = true;
        }
        self.end = self.line_number;
        self.emitted += 1;
        self.consumed += 1;
        line.reset();
    }

    fn wants_finished_line(&self) -> bool {
        self.line_number > self.start && !self.stop_collecting && self.consumed < self.cap
    }
}

/// The skimmed half of a read: the filter, the line it decides on, and the window of kept
/// lines it renders. The full window is rendered alongside it, because a skim that would
/// not help falls back to exactly that rendering.
struct SkimWindow {
    filter: SkimFilter,
    line: LineBuffer,
    window: Window,
}

/// One read of one file, fed in file order: it validates every byte, counts every line and
/// retains only a bounded prefix of the current line and of the requested window.
pub struct WindowedRender {
    display: String,
    offset: usize,
    start: usize,
    utf8: Utf8Validator,
    line: LineBuffer,
    window: Window,
    /// `Some` when a skim was asked for and can be applied.
    skim: Option<SkimWindow>,
    /// `Some` when a skim was asked for and cannot be applied at all.
    skim_refused: Option<SkimFallback>,
    peak_line_bytes: usize,
}

impl WindowedRender {
    /// Start a read with the file's first [`sniff_len`] bytes.
    ///
    /// The binary sniff runs to completion over exactly the bytes it would see reading the
    /// whole file at once, before anything else is checked — otherwise a NUL later in the
    /// file could race a UTF-8 error from an earlier chunk and change which error is
    /// reported. The sniffed bytes then go through the normal pass.
    pub fn start(sniff: &[u8], display: &str, input: &ReadInput) -> Result<Self, String> {
        if sniff.contains(&0) {
            return Err(format!("{display} is a binary file."));
        }
        let offset = input.offset.unwrap_or(DEFAULT_OFFSET) as usize;
        let limit = input.limit.unwrap_or(DEFAULT_LIMIT) as usize;
        let start = offset - 1;
        let skim = if input.skim {
            match skim_filter(&input.file_path) {
                Some(filter) => Some(SkimWindow {
                    filter,
                    line: LineBuffer::new(),
                    window: Window::new(start, limit),
                }),
                None => None,
            }
        } else {
            None
        };
        let mut render = Self {
            display: display.to_string(),
            offset,
            start,
            utf8: Utf8Validator::default(),
            line: LineBuffer::new(),
            window: Window::new(start, limit),
            skim_refused: (input.skim && skim.is_none()).then_some(SkimFallback::NeverSkimmed),
            skim,
            peak_line_bytes: 0,
        };
        render.feed(sniff)?;
        Ok(render)
    }

    /// Feed the next chunk after the sniffed bytes.
    pub fn feed(&mut self, chunk: &[u8]) -> Result<(), String> {
        self.utf8
            .update(chunk)
            .map_err(|_| format!("{} is not valid UTF-8.", self.display))?;
        let mut remaining = chunk;
        while let Some(newline) = remaining.iter().position(|byte| *byte == b'\n') {
            self.push_segment(&remaining[..newline]);
            self.finish_line();
            remaining = &remaining[newline + 1..];
        }
        self.push_segment(remaining);
        self.peak_line_bytes = self
            .peak_line_bytes
            .max(self.line.shown.len())
            .max(self.skim.as_ref().map_or(0, |skim| skim.line.shown.len()));
        Ok(())
    }

    /// Buffer one line's next segment. The skimmed half retains the prefix of EVERY line,
    /// windowed or not: the filter's block-comment and docstring state spans the whole file.
    fn push_segment(&mut self, bytes: &[u8]) {
        self.line.push(bytes, self.window.wants_current_line());
        if let Some(skim) = &mut self.skim {
            skim.line.push(bytes, true);
        }
    }

    /// Count the finished line in both windows, rendering it in each that wants it.
    fn finish_line(&mut self) {
        if let Some(skim) = &mut self.skim {
            let keep = skim.filter.keep(skim.line.text(), !skim.line.truncated);
            skim.window.finish_line(&mut skim.line, keep);
        }
        // The full window renders every line: it is the fallback rendering.
        self.window.finish_line(&mut self.line, true);
    }

    /// The most bytes of one line this read has retained so far: bounded by
    /// [`MAX_OUTPUT_BYTES`] however long the line is.
    pub fn peak_line_bytes(&self) -> usize {
        self.peak_line_bytes
    }

    /// End the read: the rendered window with its continuation footer, or why the read
    /// fails. A caller records the observation only on `Ok`, and only for a full read: a
    /// skimmed read shows filtered content and never satisfies read-before-mutate.
    pub fn finish(mut self) -> Result<String, String> {
        // Taken out rather than moved: the last line still has to be rendered below.
        std::mem::take(&mut self.utf8)
            .finish()
            .map_err(|_| format!("{} is not valid UTF-8.", self.display))?;
        if self.line.content_bytes > 0 {
            self.finish_line();
        }
        let total = self.window.line_number;
        if self.start >= total {
            return Err(format!(
                "offset {} is beyond the end of {} ({total} lines).",
                self.offset, self.display
            ));
        }
        let full_shown = self.window.emitted;
        let full = self.window.rendered(total);
        let Some(skim) = self.skim else {
            return match self.skim_refused {
                Some(reason) => Ok(fallback(reason, &full)),
                None => Ok(full),
            };
        };
        let shown = skim.window.emitted;
        if shown == 0 && full_shown > 0 {
            return Ok(fallback(SkimFallback::Emptied, &full));
        }
        // Counted in the skim's own window: the full one may stop earlier at the byte cap.
        let hidden = skim.window.consumed - shown;
        let mut skimmed = skim.window.rendered(total);
        skimmed.push_str(&format!(
            "\n[skim: {hidden} lines hidden; read the file in full before editing it]"
        ));
        if skimmed.len() >= full.len() {
            return Ok(fallback(SkimFallback::NotSmaller, &full));
        }
        Ok(skimmed)
    }
}

/// A skim that cannot help is answered with the full window and one line saying so.
fn fallback(reason: SkimFallback, full: &str) -> String {
    format!("{full}\n[skim: {}; showing the full read]", reason.reason())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `contents`, a whole file in memory, rendered in [`READ_BUFFER_BYTES`] chunks as a
    /// streamed read renders it.
    fn render(contents: &[u8], display: &str, input: &ReadInput) -> Result<String, String> {
        let sniffed = sniff_len(contents.len() as u64);
        let mut render = WindowedRender::start(&contents[..sniffed], display, input)?;
        for chunk in contents[sniffed..].chunks(READ_BUFFER_BYTES) {
            render.feed(chunk)?;
        }
        render.finish()
    }

    fn input(offset: Option<i64>, limit: Option<i64>) -> ReadInput {
        ReadInput {
            file_path: "a.txt".into(),
            offset,
            limit,
            skim: false,
        }
    }

    fn skim_input(path: &str, offset: Option<i64>, limit: Option<i64>) -> ReadInput {
        ReadInput {
            file_path: path.into(),
            offset,
            limit,
            skim: true,
        }
    }

    /// The original line numbers a skim of `src` keeps, 1-based.
    fn kept(src: &str, ext: &str) -> Vec<usize> {
        let mut filter = skim_filter(&format!("a.{ext}")).expect("a skimmable extension");
        src.lines()
            .enumerate()
            .filter_map(|(index, line)| filter.keep(line, true).then_some(index + 1))
            .collect()
    }

    #[test]
    fn rust_strips_comments_doc_comments_blanks_and_block_comments() {
        let src = "//! module doc\n\n/// doc comment\nfn main() {\n    // inline note\n    println!(\"hi\"); // trailing comment kept with its code\n}\n";
        assert_eq!(kept(src, "rs"), vec![4, 6, 7]);
        let block = "/* start\n   middle\n   end */\nfn f() {}\n";
        assert_eq!(kept(block, "rs"), vec![4]);
        // A closing delimiter with code after it keeps the whole line.
        assert_eq!(kept("/* comment\n*/ let x = 1;\n", "rs"), vec![2]);
        // A mid-line marker never strips code.
        assert_eq!(
            kept(
                "let glob = \"packages/*\"; /* trailing */\nlet y = 2;\n",
                "rs"
            ),
            vec![1, 2]
        );
    }

    #[test]
    fn python_strips_docstrings_but_never_a_string_literal() {
        let src = "# comment\ndef f():\n    \"\"\"Docstring.\n\n    More doc.\n    \"\"\"\n    return 1\n";
        assert_eq!(kept(src, "py"), vec![2, 7]);
        assert_eq!(
            kept("def f():\n    '''one-liner'''\n    return 1\n", "py"),
            vec![1, 3]
        );
        assert_eq!(
            kept(
                "x = \"\"\"not a docstring\nstill string\n\"\"\"\ny = 1\n",
                "py"
            ),
            vec![1, 2, 3, 4]
        );
        assert_eq!(
            kept("\"\"\"Module doc.\n\nMore.\n\"\"\"\nimport os\n", "py"),
            vec![5]
        );
    }

    #[test]
    fn shell_keeps_the_shebang_ruby_strips_begin_end_typescript_strips_jsdoc() {
        assert_eq!(kept("#!/bin/sh\n# setup\necho hi\n", "sh"), vec![1, 3]);
        assert_eq!(kept("=begin\nblock doc\n=end\nputs 1\n", "rb"), vec![4]);
        assert_eq!(
            kept(
                "/** JSDoc\n * @param x\n */\nexport function f(x: number) {}\n",
                "ts"
            ),
            vec![4]
        );
    }

    #[test]
    fn data_formats_and_unknown_extensions_are_never_skimmed() {
        for ext in [
            "json", "yaml", "yml", "toml", "xml", "csv", "md", "txt", "lock", "weird",
        ] {
            assert!(
                skim_filter(&format!("a.{ext}")).is_none(),
                "{ext} must not be skimmed"
            );
        }
        assert!(skim_filter("noextension").is_none());
        assert!(
            skim_filter("a.RS").is_some(),
            "the extension match is case-insensitive"
        );
    }

    #[test]
    fn a_skim_hides_comments_and_blanks_keeping_the_original_line_numbers() {
        let src = "// top comment explaining the module in some detail\n\nfn main() {\n    // inner note about the call below\n    body();\n}\n";
        let out = render(src.as_bytes(), "s.rs", &skim_input("s.rs", None, None)).unwrap();
        assert_eq!(
            out,
            "     3\tfn main() {\n     5\t    body();\n     6\t}\n[skim: 3 lines hidden; read the file in full before editing it]"
        );
    }

    #[test]
    fn a_skim_absent_is_byte_identical_to_a_full_read() {
        let src = "// c\nfn f() {}\n";
        let full = render(src.as_bytes(), "s.rs", &input(None, None)).unwrap();
        let explicit_false = render(
            src.as_bytes(),
            "s.rs",
            &ReadInput {
                file_path: "s.rs".into(),
                offset: None,
                limit: None,
                skim: false,
            },
        )
        .unwrap();
        assert_eq!(full, explicit_false);
        assert!(full.contains("     1\t// c"));
    }

    #[test]
    fn a_skim_of_a_data_format_falls_back_to_the_full_read_with_a_note() {
        let src = "{\n  \"glob\": \"packages/*\"\n}\n";
        let out = render(src.as_bytes(), "d.json", &skim_input("d.json", None, None)).unwrap();
        assert_eq!(
            out,
            render(src.as_bytes(), "d.json", &input(None, None)).unwrap()
                + "\n[skim: this file type is never skimmed; showing the full read]"
        );
    }

    #[test]
    fn a_skim_that_empties_a_non_empty_window_falls_back_to_the_full_read() {
        let out = render(
            b"// only\n// comments\n",
            "c.rs",
            &skim_input("c.rs", None, None),
        )
        .unwrap();
        assert_eq!(
            out,
            "     1\t// only\n     2\t// comments\n[skim: the skim emptied the window; showing the full read]"
        );
    }

    #[test]
    fn a_skim_that_is_not_smaller_falls_back_to_the_full_read() {
        let src = "fn a() {}\nfn b() {}\n";
        let out = render(src.as_bytes(), "n.rs", &skim_input("n.rs", None, None)).unwrap();
        assert_eq!(
            out,
            render(src.as_bytes(), "n.rs", &input(None, None)).unwrap()
                + "\n[skim: the skim was not smaller; showing the full read]"
        );
    }

    #[test]
    fn a_skim_window_and_its_footer_count_original_lines() {
        let src = "// long leading comment about function a below\nfn a() {}\n// long comment describing function b below\nfn b() {}\nfn c() {}\n";
        let out = render(
            src.as_bytes(),
            "w.rs",
            &skim_input("w.rs", Some(1), Some(4)),
        )
        .unwrap();
        assert!(out.contains("     2\tfn a() {}"), "{out}");
        assert!(out.contains("     4\tfn b() {}"), "{out}");
        // The continuation offset counts original file lines, not kept ones.
        assert!(
            out.contains("[1 more lines; continue with offset=5]"),
            "{out}"
        );
    }

    #[test]
    fn a_skim_counts_its_hidden_lines_even_when_the_full_window_hits_the_byte_cap_first() {
        let comment = format!("// {}\n", "c".repeat(1_000));
        let src: String = (0..60).map(|_| format!("{comment}fn f() {{}}\n")).collect();
        let full = render(src.as_bytes(), "big.rs", &input(None, None)).unwrap();
        assert!(
            full.contains("continue with offset="),
            "the full window byte-stops"
        );
        let out = render(src.as_bytes(), "big.rs", &skim_input("big.rs", None, None)).unwrap();
        assert!(
            out.starts_with("     2\tfn f() {}\n     4\tfn f() {}\n"),
            "{out}"
        );
        assert!(out.contains("   120\tfn f() {}"), "{out}");
        assert!(
            out.ends_with("\n[skim: 60 lines hidden; read the file in full before editing it]"),
            "{out}"
        );
    }

    #[test]
    fn a_skim_of_an_empty_window_after_the_end_is_still_the_offset_error() {
        let out = render(
            b"// c\nfn f() {}\n",
            "s.rs",
            &skim_input("s.rs", Some(9), None),
        )
        .unwrap_err();
        assert_eq!(out, "offset 9 is beyond the end of s.rs (2 lines).");
    }

    #[test]
    fn chunking_never_changes_the_rendering() {
        let contents: String = (1..=5_000).map(|n| format!("line {n} €\n")).collect();
        let whole = render(contents.as_bytes(), "a.txt", &input(Some(3), Some(10))).unwrap();
        let bytes = contents.as_bytes();
        let sniffed = sniff_len(bytes.len() as u64);
        let mut streamed =
            WindowedRender::start(&bytes[..sniffed], "a.txt", &input(Some(3), Some(10))).unwrap();
        for chunk in bytes[sniffed..].chunks(7) {
            streamed.feed(chunk).unwrap();
        }
        assert_eq!(streamed.finish().unwrap(), whole);
        assert!(whole.starts_with("     3\tline 3 €\n"), "{whole}");
        assert!(whole.ends_with("[4988 more lines; continue with offset=13]"));
    }

    #[test]
    fn a_nul_in_the_sniff_wins_over_a_later_utf8_error() {
        assert_eq!(
            render(b"a\0\xff", "x.bin", &input(None, None)).unwrap_err(),
            "x.bin is a binary file."
        );
    }

    #[test]
    fn a_missing_request_is_displayed_as_the_workspace_resolves_it() {
        assert_eq!(display_of_request("nope.txt"), "nope.txt");
        assert_eq!(display_of_request("./a//b/../nope.txt"), "a/nope.txt");
        assert_eq!(display_of_request("/abs/nope.txt"), "/abs/nope.txt");
    }

    #[test]
    fn describe_result_summarizes_ok_and_quotes_the_first_error_line() {
        assert_eq!(
            describe_result("     1\ta\n     2\tb", true),
            "2 lines · 0.0 kB"
        );
        assert_eq!(
            describe_result("nope.txt does not exist.\n", false),
            "nope.txt does not exist."
        );
    }
}
