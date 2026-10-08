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
pub const DESCRIPTION: &str = "Read a UTF-8 text file from the workspace, with numbered lines.\nUse `offset` and `limit` to page through a long file; the last line gives the next offset.\nRead a file before you edit or overwrite it: a mutation is refused until you have seen its current contents.\n`skim` hides comments, docstrings, and blank lines for exploration but never satisfies the full-read prerequisite for mutation.\nTo read several files, or several ranges of one file, put them into one call with `files` (up to 10 entries, each with `file_path` and its own `offset`, `limit` and `skim`) rather than one call each.";
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
/// The most entries of `files` in one call (ADR-0125).
pub const MAX_FILES: usize = 10;
/// The line an entry of `files` gets once the call's [`MAX_OUTPUT_BYTES`] is spent.
pub const NOT_READ: &str =
    "not read: this call's output limit was reached; read it in another call";
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
            },
            "files": {
                "type": "array",
                "minItems": 1,
                "maxItems": MAX_FILES,
                "description": "Several files or ranges in one call, instead of `file_path`; each entry takes the same fields as a single read.",
                "items": {
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
                }
            }
        },
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
    check_window(&input).map_err(|reason| invalid(tool, &reason))?;
    Ok(input)
}

fn check_window(input: &ReadInput) -> Result<(), String> {
    if matches!(input.offset, Some(offset) if offset < 1) {
        return Err("`offset` must be at least 1".into());
    }
    if matches!(input.limit, Some(limit) if limit < 1) {
        return Err("`limit` must be at least 1".into());
    }
    Ok(())
}

/// A call's validated input in either form (ADR-0125).
#[derive(Debug)]
pub enum ReadRequest {
    /// `file_path` with its own `offset`, `limit` and `skim`: read exactly as before.
    Single(ReadInput),
    /// `files`: 1 to [`MAX_FILES`] entries, read in the order given.
    Several(Vec<ReadInput>),
}

/// The `files` form: nothing else beside it.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SeveralInput {
    files: Vec<ReadInput>,
}

/// Parse and validate `raw` in either form. An object naming `file_path` and not `files`
/// goes through [`parse_input`] unchanged, so the single form's input errors stay as they
/// were.
pub fn parse_request(tool: &str, raw: RawInput<'_>) -> Result<ReadRequest, String> {
    let RawInput::Json(json) = raw else {
        return parse_input(tool, raw).map(ReadRequest::Single);
    };
    let Ok(object) = serde_json::from_str::<serde_json::Map<String, serde_json::Value>>(json)
    else {
        return parse_input(tool, raw).map(ReadRequest::Single);
    };
    match (
        object.contains_key("file_path"),
        object.contains_key("files"),
    ) {
        (true, false) => parse_input(tool, raw).map(ReadRequest::Single),
        (true, true) => Err(invalid(
            tool,
            "give either `file_path` (one file) or `files` (several files or ranges), not both",
        )),
        (false, false) => Err(invalid(
            tool,
            "give either `file_path` (one file) or `files` (several files or ranges)",
        )),
        (false, true) => {
            let input: SeveralInput =
                serde_json::from_str(json).map_err(|error| invalid(tool, &error.to_string()))?;
            if input.files.is_empty() || input.files.len() > MAX_FILES {
                return Err(invalid(
                    tool,
                    &format!(
                        "`files` takes 1 to {MAX_FILES} entries, got {}",
                        input.files.len()
                    ),
                ));
            }
            for (index, entry) in input.files.iter().enumerate() {
                check_window(entry)
                    .map_err(|reason| invalid(tool, &format!("`files[{index}]`: {reason}")))?;
            }
            Ok(ReadRequest::Several(input.files))
        }
    }
}

/// The sections of a `files` call, in the order given, under the call's one
/// [`MAX_OUTPUT_BYTES`] (ADR-0125). Each entry is read by the single-read path; this only
/// heads, joins and budgets what it rendered.
#[derive(Debug, Default)]
pub struct Sections {
    out: String,
    /// Bytes of section bodies so far: rendered windows with their footers, error lines.
    used: usize,
    reached: bool,
    any_read: bool,
}

impl Sections {
    /// An empty call.
    pub fn new() -> Self {
        Self::default()
    }

    /// The bytes the next entry's window may render, or `None` once the call's limit is
    /// reached: the entry is then not read and gets [`Self::not_read`].
    pub fn budget(&self) -> Option<usize> {
        (!self.reached && self.used < MAX_OUTPUT_BYTES).then(|| MAX_OUTPUT_BYTES - self.used)
    }

    /// One entry's outcome as a single read of it renders: its output, or its error line.
    /// `capped` says its window stopped at the budget it was given.
    pub fn push(&mut self, file_path: &str, outcome: Result<String, String>, capped: bool) {
        let body = match outcome {
            Ok(output) => {
                self.any_read = true;
                output
            }
            Err(message) => message,
        };
        self.used += body.len();
        self.reached |= capped;
        self.section(file_path, &body);
    }

    /// An entry the call's output limit left unread.
    pub fn not_read(&mut self, file_path: &str) {
        self.section(file_path, NOT_READ);
    }

    fn section(&mut self, file_path: &str, body: &str) {
        if !self.out.is_empty() {
            self.out.push_str("\n\n");
        }
        self.out.push_str(&format!("==> {file_path} <==\n{body}"));
    }

    /// The call's result, and whether any entry was read: a call is an error only when
    /// every entry failed.
    pub fn finish(self) -> (String, bool) {
        (self.out, self.any_read)
    }
}

/// The invalid-input message the model acts on.
pub fn invalid(tool: &str, reason: &str) -> String {
    format!("Invalid input for {tool}: {reason}")
}

/// ADR-0057: the file a call reads, with its line window when the input names one; `None`
/// for input that does not parse — never a guess.
pub fn describe_target(tool: &str, raw: RawInput<'_>) -> Option<String> {
    match parse_request(tool, raw).ok()? {
        ReadRequest::Single(input) => Some(entry_target(input)),
        ReadRequest::Several(files) => Some(
            files
                .into_iter()
                .map(entry_target)
                .collect::<Vec<_>>()
                .join(", "),
        ),
    }
}

fn entry_target(input: ReadInput) -> String {
    let mut target = input.file_path;
    if input.offset.is_some() || input.limit.is_some() {
        let start = input.offset.unwrap_or(DEFAULT_OFFSET);
        let end = start.saturating_add(input.limit.unwrap_or(DEFAULT_LIMIT).saturating_sub(1));
        target = format!("{target}:{start}-{end}");
    }
    target
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
    /// The string-literal syntax, so a literal's lines are never taken for comments.
    lexer: Lexer,
}

const fn c_style(lexer: Lexer) -> Rules {
    Rules {
        line: &["//"],
        block: Some(("/*", "*/")),
        docstrings: false,
        lexer,
    }
}
const RUST: Rules = c_style(Lexer::Rust);
const C: Rules = c_style(Lexer::C);
const JAVA: Rules = c_style(Lexer::Java);
const SCRIPT: Rules = c_style(Lexer::Script);
const GO: Rules = c_style(Lexer::Go);
const PYTHON: Rules = Rules {
    line: &["#"],
    block: None,
    docstrings: true,
    lexer: Lexer::Python,
};
const RUBY: Rules = Rules {
    line: &["#"],
    block: Some(("=begin", "=end")),
    docstrings: false,
    lexer: Lexer::Ruby,
};
const HASH: Rules = Rules {
    line: &["#"],
    block: None,
    docstrings: false,
    lexer: Lexer::Shell,
};

/// Comment syntax by file extension (lowercased). `None` means "never strip": data
/// formats (JSON/YAML/TOML/XML/CSV), prose and unknown extensions pass through untouched
/// — a comment-shaped line in data is data.
fn rules(extension: &str) -> Option<&'static Rules> {
    match extension.to_ascii_lowercase().as_str() {
        "rs" => Some(&RUST),
        "js" | "mjs" | "cjs" | "jsx" | "ts" | "tsx" => Some(&SCRIPT),
        "go" => Some(&GO),
        "c" | "h" | "cpp" | "cc" | "cxx" | "hpp" | "hh" => Some(&C),
        "java" => Some(&JAVA),
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

/// The string-literal syntax of a language family (#509): which delimiters open a literal
/// that may run past the end of its line. Only as much as decides whether a line lies inside
/// such a literal; a doubtful case keeps lines, never hides them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Lexer {
    /// `"…"` across lines, raw `r#"…"#`, char literals beside lifetimes.
    Rust,
    /// C and C++: `"…"`, raw `R"delim(…)delim"`, char literals.
    C,
    /// `"…"`, text blocks `"""…"""`, char literals.
    Java,
    /// JavaScript and TypeScript: `"…"`, `'…'` and template literals `` `…` ``.
    Script,
    /// `"…"`, raw `` `…` `` without escapes, rune literals.
    Go,
    /// `"…"`, `'…'`, `"""…"""`, `'''…'''` (the RTK donor's scan).
    Python,
    /// `"…"`, `'…'`, `` `…` `` and heredocs `<<~ID`, `<<-ID`, `<<ID`.
    Ruby,
    /// `"…"`, `'…'`, `$'…'` and heredocs `<<ID`, `<<-ID`.
    Shell,
}

/// A literal still open at the end of a line: every following line up to its end is text.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Literal {
    /// Ends at `close`; a backslash escapes the next byte when `escapes`.
    Quoted { close: &'static str, escapes: bool },
    /// Ends at the exact text, with no escapes (raw strings).
    Raw(String),
    /// Heredoc bodies still to come, in order: each ends at a line equal to its tag, after
    /// the indentation its form allows.
    Heredoc(Vec<(String, Indent)>),
    /// A JS/TS template literal: the innermost frame last, so `${ … `inner` … }` nests.
    Template(Vec<Frame>),
    /// A block comment opened mid-line and not closed on it: the lines up to `close`
    /// are comment, so nothing in them opens a literal (#509 repair).
    Comment(&'static str),
}

/// One level of a template literal: its text, or a `${ … }` expression with the
/// count of `{` opened inside it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Frame {
    Text,
    Expression(u32),
}

/// The indentation a heredoc's closing line may carry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Indent {
    None,
    Tabs,
    Any,
}

/// What the scan does at one position of code.
enum Step {
    Skip(usize),
    Open(Literal, usize),
    Heredoc(String, Indent, usize),
    /// A comment starts: nothing after it on the line is code.
    Stop,
}

/// The literal open at the end of `line`, scanning from byte `from` with `open` still open
/// there. Heredocs named on the line start with the next line.
fn scan_literals(
    line: &str,
    from: usize,
    mut open: Option<Literal>,
    lexer: Lexer,
) -> Option<Literal> {
    let bytes = line.as_bytes();
    let mut i = from.min(bytes.len());
    let mut heredocs = Vec::new();
    while i < bytes.len() {
        let rest = &bytes[i..];
        match &open {
            Some(Literal::Quoted { close, escapes }) => {
                if *escapes && rest[0] == b'\\' {
                    i += 2;
                } else if rest.starts_with(close.as_bytes()) {
                    i += close.len();
                    open = None;
                } else {
                    i += 1;
                }
                continue;
            }
            Some(Literal::Raw(close)) => {
                if rest.starts_with(close.as_bytes()) {
                    i += close.len();
                    open = None;
                } else {
                    i += 1;
                }
                continue;
            }
            Some(Literal::Comment(close)) => {
                if rest.starts_with(close.as_bytes()) {
                    i += close.len();
                    open = None;
                } else {
                    i += 1;
                }
                continue;
            }
            Some(Literal::Template(frames)) => {
                let mut frames = frames.clone();
                i += template_step(&mut frames, bytes, i);
                open = (!frames.is_empty()).then_some(Literal::Template(frames));
                continue;
            }
            Some(Literal::Heredoc(_)) => return open,
            None => {}
        }
        match step(bytes, i, lexer) {
            Step::Skip(length) => i += length.max(1),
            Step::Open(literal, length) => {
                open = Some(literal);
                i += length;
            }
            Step::Heredoc(tag, indent, length) => {
                heredocs.push((tag, indent));
                i += length;
            }
            Step::Stop => break,
        }
    }
    match open {
        None if !heredocs.is_empty() => Some(Literal::Heredoc(heredocs)),
        open => open,
    }
}

/// Advance a template literal by one step at `i`, returning the bytes consumed. An
/// emptied frame stack closes the literal.
fn template_step(frames: &mut Vec<Frame>, bytes: &[u8], i: usize) -> usize {
    let rest = &bytes[i..];
    match frames.last_mut() {
        Some(Frame::Text) => match rest[0] {
            b'\\' => 2,
            b'`' => {
                frames.pop();
                1
            }
            b'$' if rest.get(1) == Some(&b'{') => {
                frames.push(Frame::Expression(0));
                2
            }
            _ => 1,
        },
        Some(Frame::Expression(depth)) => match rest[0] {
            b'`' => {
                frames.push(Frame::Text);
                1
            }
            b'{' => {
                *depth += 1;
                1
            }
            b'}' if *depth == 0 => {
                frames.pop();
                1
            }
            b'}' => {
                *depth -= 1;
                1
            }
            // A quoted string inside the expression, to its close on the line.
            quote @ (b'"' | b'\'') => {
                let mut end = 1;
                while end < rest.len() && rest[end] != quote {
                    end += if rest[end] == b'\\' { 2 } else { 1 };
                }
                (end + 1).min(rest.len())
            }
            _ => 1,
        },
        None => 1,
    }
}

fn is_word(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
}

fn quoted(close: &'static str, escapes: bool) -> Literal {
    Literal::Quoted { close, escapes }
}

/// One step of the scan over code at `i`.
fn step(bytes: &[u8], i: usize, lexer: Lexer) -> Step {
    let rest = &bytes[i..];
    let boundary = i == 0 || !is_word(bytes[i - 1]);
    match lexer {
        Lexer::Rust | Lexer::C | Lexer::Java | Lexer::Script | Lexer::Go => {
            if rest.starts_with(b"//") {
                return Step::Stop;
            }
            // `\/*` in a JS regular expression opens no comment.
            if rest.starts_with(b"/*") && (i == 0 || bytes[i - 1] != b'\\') {
                // A comment that ends on the line is skipped; one that does not carries on.
                return match find(&rest[2..], b"*/") {
                    Some(end) => Step::Skip(end + 4),
                    None => Step::Open(Literal::Comment("*/"), 2),
                };
            }
            match (lexer, rest[0]) {
                (Lexer::Rust, b'r' | b'b' | b'c') if boundary => {
                    let prefix = if rest[0] == b'r' { 1 } else { 2 };
                    if prefix == 2 && rest.get(1) != Some(&b'r') {
                        return Step::Skip(1);
                    }
                    let hashes = rest[prefix..].iter().take_while(|&&b| b == b'#').count();
                    if rest.get(prefix + hashes) == Some(&b'"') {
                        let close = format!("\"{}", "#".repeat(hashes));
                        return Step::Open(Literal::Raw(close), prefix + hashes + 1);
                    }
                    Step::Skip(1)
                }
                (Lexer::C, b'R') if rest.get(1) == Some(&b'"') && c_raw_prefix(bytes, i) => {
                    let delimiter: Vec<u8> = rest[2..]
                        .iter()
                        .take(17)
                        .take_while(|&&b| b != b'(')
                        .copied()
                        .collect();
                    let valid = delimiter.len() <= 16
                        && rest.get(2 + delimiter.len()) == Some(&b'(')
                        && !delimiter
                            .iter()
                            .any(|b| b.is_ascii_whitespace() || matches!(b, b')' | b'\\' | b'"'));
                    if !valid {
                        return Step::Skip(1);
                    }
                    let close = format!("){}\"", String::from_utf8_lossy(&delimiter));
                    Step::Open(Literal::Raw(close), 3 + delimiter.len())
                }
                (Lexer::Java, b'"') if rest.starts_with(b"\"\"\"") => {
                    Step::Open(quoted("\"\"\"", true), 3)
                }
                (_, b'"') => Step::Open(quoted("\"", true), 1),
                (Lexer::Script, b'\'') => Step::Open(quoted("'", true), 1),
                (Lexer::Script, b'`') => Step::Open(Literal::Template(vec![Frame::Text]), 1),
                (Lexer::Go, b'`') => Step::Open(quoted("`", false), 1),
                (_, b'\'') => Step::Skip(char_literal(rest)),
                _ => Step::Skip(1),
            }
        }
        Lexer::Python => match rest[0] {
            b'#' => Step::Stop,
            b'"' if rest.starts_with(b"\"\"\"") => Step::Open(quoted("\"\"\"", true), 3),
            b'\'' if rest.starts_with(b"'\'\'") => Step::Open(quoted("'\'\'", true), 3),
            quote @ (b'"' | b'\'') => Step::Skip(python_string(bytes, i, quote)),
            _ => Step::Skip(1),
        },
        Lexer::Ruby => match rest[0] {
            b'#' => Step::Stop,
            b'"' => Step::Open(quoted("\"", true), 1),
            b'\'' => Step::Open(quoted("'", true), 1),
            b'`' => Step::Open(quoted("`", true), 1),
            b'<' if rest.starts_with(b"<<") => ruby_heredoc(rest),
            _ => Step::Skip(1),
        },
        Lexer::Shell => match rest[0] {
            b'#' if i == 0 || matches!(bytes[i - 1], b' ' | b'\t' | b';' | b'&' | b'|' | b'(') => {
                Step::Stop
            }
            b'\\' => Step::Skip(2),
            b'$' if rest.get(1) == Some(&b'\'') => Step::Open(quoted("'", true), 2),
            b'\'' => Step::Open(quoted("'", false), 1),
            b'"' => Step::Open(quoted("\"", true), 1),
            b'`' => Step::Open(quoted("`", true), 1),
            b'<' if rest.starts_with(b"<<<") => Step::Skip(3),
            b'<' if rest.starts_with(b"<<") => shell_heredoc(rest),
            _ => Step::Skip(1),
        },
    }
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

/// Whether the `R` at `i` starts a C++ raw string: alone, or after `u8`, `u`, `U` or `L`.
fn c_raw_prefix(bytes: &[u8], i: usize) -> bool {
    let start = bytes[..i]
        .iter()
        .rposition(|&b| !is_word(b))
        .map_or(0, |p| p + 1);
    matches!(&bytes[start..i], b"" | b"u8" | b"u" | b"U" | b"L")
}

/// The length of a char literal at the start of `rest` (`'a'`, `'\n'`, `'é'`), or 1 when
/// the quote is not one (a Rust lifetime or label).
fn char_literal(rest: &[u8]) -> usize {
    if rest.get(1) == Some(&b'\\') {
        return rest
            .iter()
            .skip(3)
            .take(10)
            .position(|&b| b == b'\'')
            .map_or(1, |end| end + 4);
    }
    let width = match rest.get(1) {
        Some(&b) if b < 0x80 => 1,
        Some(&b) if b >= 0xF0 => 4,
        Some(&b) if b >= 0xE0 => 3,
        Some(_) => 2,
        None => return 1,
    };
    if rest.get(1 + width) == Some(&b'\'') {
        2 + width
    } else {
        1
    }
}

/// The length of a one-line Python string starting at `i` with `quote`, to its closing
/// quote or the end of the line. Adapted from RTK `src/core/filter.rs` (`advance_triple_quote`):
/// an f- or t-string's replacement field may reuse the outer quote (PEP 701), so the quote
/// only ends the string outside `{…}`.
fn python_string(bytes: &[u8], start: usize, quote: u8) -> usize {
    let word = bytes[..start]
        .iter()
        .rposition(|&b| !is_word(b))
        .map_or(0, |p| p + 1);
    let interpolated = matches!(
        bytes[word..start].to_ascii_lowercase().as_slice(),
        b"f" | b"fr" | b"rf" | b"t" | b"tr" | b"rt"
    );
    let mut depth = 0usize;
    let mut i = start + 1;
    while i < bytes.len() {
        match bytes[i] {
            b'\\' => i += 1,
            b'{' if interpolated => {
                if depth == 0 && bytes.get(i + 1) == Some(&b'{') {
                    i += 1;
                } else {
                    depth += 1;
                }
            }
            b'}' if interpolated && depth > 0 => depth -= 1,
            b if b == quote && depth == 0 => return i + 1 - start,
            _ => {}
        }
        i += 1;
    }
    bytes.len() - start
}

/// The heredoc tag after `<<` (with `-` or `~` already consumed): a word, possibly quoted.
fn heredoc_tag(rest: &[u8]) -> Option<(String, usize)> {
    // A quoted tag is any text up to its closing quote, spaces included.
    if let Some(&quote @ (b'\'' | b'"')) = rest.first() {
        let length = rest[1..].iter().position(|&b| b == quote)?;
        if length == 0 {
            return None;
        }
        return Some((
            String::from_utf8_lossy(&rest[1..1 + length]).into_owned(),
            length + 2,
        ));
    }
    let from = usize::from(rest.first() == Some(&b'\\'));
    let length = rest[from..]
        .iter()
        .take_while(|&&b| is_word(b) || b == b'-' || b == b'.')
        .count();
    if length == 0 || !(rest[from].is_ascii_alphabetic() || rest[from] == b'_') {
        return None;
    }
    Some((
        String::from_utf8_lossy(&rest[from..from + length]).into_owned(),
        from + length,
    ))
}

/// A Ruby heredoc at `<<`: `<<~ID` and `<<-ID` close on an indented line, bare `<<ID` (an
/// A Ruby heredoc at `<<`: `<<~ID` and `<<-ID` close on an indented line, bare `<<ID`
/// (lower-case too, `<<doc`) on a line of its own. `a <<b` written as a shift is read
/// as a heredoc: that keeps lines until a line `b`, never hides one.
fn ruby_heredoc(rest: &[u8]) -> Step {
    let (indent, from) = match rest.get(2) {
        Some(b'~' | b'-') => (Indent::Any, 3),
        _ => (Indent::None, 2),
    };
    match heredoc_tag(&rest[from..]) {
        Some((tag, used)) => Step::Heredoc(tag, indent, from + used),
        None => Step::Skip(2),
    }
}

/// A shell heredoc at `<<`: `<<-WORD` closes on a tab-indented line, `<<WORD` on a line of
/// its own; the word may be quoted or escaped and follow blanks.
fn shell_heredoc(rest: &[u8]) -> Step {
    let (indent, mut from) = match rest.get(2) {
        Some(b'-') => (Indent::Tabs, 3),
        _ => (Indent::None, 2),
    };
    from += rest[from..]
        .iter()
        .take_while(|&&b| b == b' ' || b == b'\t')
        .count();
    match heredoc_tag(&rest[from..]) {
        Some((tag, used)) => Step::Heredoc(tag, indent, from + used),
        None => Step::Skip(2),
    }
}

/// The literal still open after `line`, a line inside `open`.
fn continue_literal(line: &str, open: Literal, lexer: Lexer) -> Option<Literal> {
    let Literal::Heredoc(mut tags) = open else {
        return scan_literals(line, 0, Some(open), lexer);
    };
    let (tag, indent) = &tags[0];
    let text = line.trim_end_matches('\r');
    let text = match indent {
        Indent::None => text,
        Indent::Tabs => text.trim_start_matches('\t'),
        Indent::Any => text.trim(),
    };
    if text == tag {
        tags.remove(0);
    }
    (!tags.is_empty()).then_some(Literal::Heredoc(tags))
}

/// The per-line keep/strip decision for one skimmed read, over the *original* file lines in
/// order: kept lines are rendered with their true line numbers, so offsets and follow-up
/// full reads stay coherent. Fed every line of the file, including lines outside the
/// window, because a block comment or docstring carries its state across them.
pub struct SkimFilter {
    rules: &'static Rules,
    state: SkimState,
    /// Whether the next statement is the first of a module, `class` or `def` body: the
    /// only place a triple-quoted string is a docstring (#509 repair).
    docstring_next: bool,
    /// The bracket depth of a `def`/`class` header still open over several lines.
    header: Option<i64>,
    /// A string literal open at the end of the last line: its lines are text, kept whole.
    literal: Option<Literal>,
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
        docstring_next: true,
        header: None,
        literal: None,
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
        // A line inside a string literal is text, whatever it looks like: a `#` or `//`
        // line, a blank line or a `"""` in it is never taken for a comment (#509).
        if let Some(open) = self.literal.take() {
            self.literal = continue_literal(line, open, self.rules.lexer);
            self.carry_comment();
            self.docstring_next = false;
            return true;
        }
        // The kept line's code starts at this byte of `line`, if any code is on it.
        let lead = line.len() - line.trim_start().len();
        let (keep, code) = match self.state {
            SkimState::Block(end) => match trimmed.find(end) {
                Some(pos) => {
                    self.state = SkimState::Code;
                    // Code after the closing delimiter: keep the line.
                    let after = pos + end.len();
                    let code = !trimmed[after..].trim().is_empty();
                    (code || !complete, code.then_some(lead + after))
                }
                // The closer may lie past a truncated prefix: keep the line, resume as code.
                None if !complete => {
                    self.state = SkimState::Code;
                    (true, None)
                }
                None => (false, None),
            },
            SkimState::Docstring(delim) => match trimmed.find(delim) {
                Some(pos) => {
                    self.state = SkimState::Code;
                    let after = pos + delim.len();
                    let code = !trimmed[after..].trim().is_empty();
                    (code || !complete, code.then_some(lead + after))
                }
                None if !complete => {
                    self.state = SkimState::Code;
                    (true, None)
                }
                None => (false, None),
            },
            SkimState::Code => {
                if trimmed.is_empty() {
                    // A blank-looking truncated prefix may hide code past it (review H1).
                    (!complete, None)
                }
                // Shebangs carry meaning; never strip line 1's `#!`.
                else if index == 0 && trimmed.starts_with("#!") {
                    (true, None)
                } else if let Some((start, end)) = self.rules.block
                    && let Some(rest) = trimmed.strip_prefix(start)
                {
                    match rest.find(end) {
                        // Closes on the same line: keep only if code follows the delimiter.
                        Some(pos) => {
                            let after = start.len() + pos + end.len();
                            let code = !trimmed[after..].trim().is_empty();
                            (code || !complete, code.then_some(lead + after))
                        }
                        None if !complete => (true, None),
                        None => {
                            self.state = SkimState::Block(end);
                            (false, None)
                        }
                    }
                } else if self.rules.docstrings
                    && self.docstring_next
                    && let Some((delim, rest)) = ["\"\"\"", "'\'\'"]
                        .iter()
                        .find_map(|d| trimmed.strip_prefix(d).map(|rest| (*d, rest)))
                {
                    self.docstring_next = false;
                    match rest.find(delim) {
                        Some(pos) => {
                            let after = delim.len() + pos + delim.len();
                            let code = !trimmed[after..].trim().is_empty();
                            (code || !complete, code.then_some(lead + after))
                        }
                        None if !complete => (true, None),
                        None => {
                            self.state = SkimState::Docstring(delim);
                            (false, None)
                        }
                    }
                } else {
                    let comment = self
                        .rules
                        .line
                        .iter()
                        .any(|marker| trimmed.starts_with(marker));
                    (!comment, (!comment).then_some(0))
                }
            }
        };
        if let Some(from) = code {
            self.literal = scan_literals(line, from, None, self.rules.lexer);
            self.carry_comment();
        }
        if keep && !(index == 0 && trimmed.starts_with("#!")) {
            self.after_statement_line(trimmed);
        }
        keep
    }

    /// A block comment the scan left open mid-line continues as a block comment.
    fn carry_comment(&mut self) {
        if let Some(Literal::Comment(close)) = self.literal {
            self.literal = None;
            self.state = SkimState::Block(close);
        }
    }

    /// Track docstring positions over a kept line: only a complete `def`, `async def` or
    /// `class` header ending in `:` makes the next statement a body's first.
    fn after_statement_line(&mut self, trimmed: &str) {
        let code = python_code(trimmed);
        let starts_header = ["def ", "async def ", "class "]
            .iter()
            .any(|keyword| code.starts_with(keyword));
        if self.header.is_none() && !starts_header {
            self.docstring_next = false;
            return;
        }
        let depth = self.header.unwrap_or(0) + bracket_delta(code);
        if depth > 0 {
            self.header = Some(depth);
            self.docstring_next = false;
        } else {
            self.header = None;
            self.docstring_next = code.trim_end().ends_with(':');
        }
    }
}

/// `line` up to a `#` comment outside quotes.
fn python_code(line: &str) -> &str {
    let mut quote = None;
    for (index, character) in line.char_indices() {
        match (quote, character) {
            (None, '#') => return &line[..index],
            (None, '"' | '\'') => quote = Some(character),
            (Some(open), _) if character == open => quote = None,
            _ => {}
        }
    }
    line
}

/// Opening minus closing brackets of `code` outside quotes.
fn bracket_delta(code: &str) -> i64 {
    let mut quote = None;
    let mut delta = 0;
    for character in code.chars() {
        match (quote, character) {
            (None, '(' | '[' | '{') => delta += 1,
            (None, ')' | ']' | '}') => delta -= 1,
            (None, '"' | '\'') => quote = Some(character),
            (Some(open), _) if character == open => quote = None,
            _ => {}
        }
    }
    delta
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
    /// The most bytes this window renders, footers aside: [`MAX_OUTPUT_BYTES`] for a single
    /// read, what the call has left for an entry of `files` (ADR-0125).
    max_bytes: usize,
}

impl Window {
    /// A window over a `limit`-line request starting at zero-based line `start`.
    fn new(start: usize, limit: usize, max_bytes: usize) -> Self {
        Self {
            start,
            cap: limit.min(MAX_OUTPUT_LINES),
            line_number: 0,
            emitted: 0,
            consumed: 0,
            end: start,
            stop_collecting: false,
            out: String::new(),
            max_bytes,
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
        if self.emitted > 0 && self.out.len() + 1 + rendered_bytes > self.max_bytes {
            self.stop_collecting = true;
            line.reset();
            return;
        }

        if self.emitted > 0 {
            self.out.push('\n');
        }
        self.out.push_str(&prefix);
        if rendered_bytes <= self.max_bytes {
            self.out.push_str(valid_utf8_prefix(&line.shown));
        } else {
            let available = self.max_bytes.saturating_sub(self.out.len());
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
        Self::start_within(sniff, display, input, MAX_OUTPUT_BYTES)
    }

    /// [`Self::start`] for an entry of `files`, whose window renders at most `max_bytes`:
    /// what the call's [`MAX_OUTPUT_BYTES`] has left (ADR-0125).
    pub fn start_within(
        sniff: &[u8],
        display: &str,
        input: &ReadInput,
        max_bytes: usize,
    ) -> Result<Self, String> {
        if sniff.contains(&0) {
            return Err(format!("{display} is a binary file."));
        }
        let offset = input.offset.unwrap_or(DEFAULT_OFFSET) as usize;
        let limit = input.limit.unwrap_or(DEFAULT_LIMIT) as usize;
        let start = offset - 1;
        let skim = if input.skim {
            skim_filter(&input.file_path).map(|filter| SkimWindow {
                filter,
                line: LineBuffer::new(),
                window: Window::new(start, limit, max_bytes),
            })
        } else {
            None
        };
        let mut render = Self {
            display: display.to_string(),
            offset,
            start,
            utf8: Utf8Validator::default(),
            line: LineBuffer::new(),
            window: Window::new(start, limit, max_bytes),
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
        Ok(())
    }

    /// Buffer one line's next segment. The skimmed half retains the prefix of EVERY line,
    /// windowed or not: the filter's block-comment and docstring state spans the whole file.
    fn push_segment(&mut self, bytes: &[u8]) {
        self.line.push(bytes, self.window.wants_current_line());
        if let Some(skim) = &mut self.skim {
            skim.line.push(bytes, true);
        }
        // Measured after every push, before `finish_line` resets the buffers (review M1).
        self.peak_line_bytes = self
            .peak_line_bytes
            .max(self.line.shown.len())
            .max(self.skim.as_ref().map_or(0, |skim| skim.line.shown.len()));
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
    pub fn finish(self) -> Result<String, String> {
        self.finish_within().map(|(output, _)| output)
    }

    /// [`Self::finish`], and whether a window stopped at its byte limit: for an entry of
    /// `files`, that limit is the call's, so the entries after it are not read (ADR-0125).
    pub fn finish_within(mut self) -> Result<(String, bool), String> {
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
        let mut capped = self.window.stop_collecting;
        let full = self.window.rendered(total);
        let Some(skim) = self.skim else {
            return match self.skim_refused {
                Some(reason) => Ok((fallback(reason, &full), capped)),
                None => Ok((full, capped)),
            };
        };
        capped |= skim.window.stop_collecting;
        let shown = skim.window.emitted;
        if shown == 0 && full_shown > 0 {
            return Ok((fallback(SkimFallback::Emptied, &full), capped));
        }
        // Counted in the skim's own window: the full one may stop earlier at the byte cap.
        let hidden = skim.window.consumed - shown;
        let mut skimmed = skim.window.rendered(total);
        skimmed.push_str(&format!(
            "\n[skim: {hidden} lines hidden; read the file in full before editing it]"
        ));
        if skimmed.len() >= full.len() {
            return Ok((fallback(SkimFallback::NotSmaller, &full), capped));
        }
        Ok((skimmed, capped))
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

    // #509 item 4: the lines of a string literal are code, never a comment, a docstring or
    // a blank to hide, in every skimmed language whose literals can span lines.

    #[test]
    fn rust_keeps_multi_line_and_raw_string_literals() {
        let src = "let usage = \"\n// not a comment\n/* nor this */\n\n\";\n// hidden\nfn f() {}\n";
        assert_eq!(kept(src, "rs"), vec![1, 2, 3, 4, 5, 7]);
        let raw = "let q = r#\"\n// inside\n\"quoted\" still inside\n\"#;\n// hidden\nx();\n";
        assert_eq!(kept(raw, "rs"), vec![1, 2, 3, 4, 6]);
        // Lifetimes and char literals open no string.
        let quotes = "fn f<'a>(s: &'a str) -> &'a str { s }\n// hidden\nlet c = '\"';\n// hidden\n";
        assert_eq!(kept(quotes, "rs"), vec![1, 3]);
    }

    #[test]
    fn c_and_cpp_keep_raw_string_literals() {
        let src = "const char* s = R\"sql(\n// inside\n/* inside */\n)sql\";\n// hidden\nint x;\n";
        assert_eq!(kept(src, "cpp"), vec![1, 2, 3, 4, 6]);
        assert_eq!(kept("char q = '\"';\n// hidden\nint y;\n", "c"), vec![1, 3]);
    }

    #[test]
    fn java_keeps_text_blocks() {
        let src =
            "String t = \"\"\"\n    // inside\n    /* inside */\n    \"\"\";\n// hidden\nint x;\n";
        assert_eq!(kept(src, "java"), vec![1, 2, 3, 4, 6]);
    }

    #[test]
    fn typescript_keeps_template_literals() {
        let src = "const q = `\n// inside\n/* inside */\n\n`;\n// hidden\nf();\n";
        assert_eq!(kept(src, "ts"), vec![1, 2, 3, 4, 5, 7]);
        assert_eq!(kept(src, "js"), vec![1, 2, 3, 4, 5, 7]);
    }

    #[test]
    fn go_keeps_raw_string_literals() {
        let src = "var s = `\n// inside\n`\n// hidden\nfunc f() {}\n";
        assert_eq!(kept(src, "go"), vec![1, 2, 3, 5]);
    }

    #[test]
    fn python_keeps_a_triple_quoted_value_and_hides_only_docstrings() {
        // On main the `#` and blank lines were hidden, and the closing `"""` after `key:`
        // opened a "docstring" that hid the function below it.
        let src = "QUERY = \"\"\"\n# not a comment\n\nkey:\n\"\"\"\ndef f():\n    return 1\n";
        assert_eq!(kept(src, "py"), vec![1, 2, 3, 4, 5, 6, 7]);
        let doc = "def f():\n    \"\"\"Doc.\n    # in the docstring\n    \"\"\"\n    return 1\n";
        assert_eq!(kept(doc, "py"), vec![1, 5]);
        let fstring = "msg = f\"{x['a']}\" # note\n# hidden\ny = '\"\"\"'\n# hidden\n";
        assert_eq!(kept(fstring, "py"), vec![1, 3]);
    }

    #[test]
    fn ruby_keeps_heredocs() {
        let src = "sql = <<~SQL\n  # not a comment\n  =begin\n  SQL\n# hidden\nputs sql\n";
        assert_eq!(kept(src, "rb"), vec![1, 2, 3, 4, 6]);
        assert_eq!(kept("x = a << b\n# hidden\n", "rb"), vec![1]);
    }

    #[test]
    fn shell_keeps_heredocs_and_multi_line_strings() {
        let src = "cat <<EOF\n# not a comment\n\nEOF\n# hidden\necho done\n";
        assert_eq!(kept(src, "sh"), vec![1, 2, 3, 4, 6]);
        let tabs = "cat <<-'EOF' > out\n\t# inside\n\tEOF\necho x\n";
        assert_eq!(kept(tabs, "bash"), vec![1, 2, 3, 4]);
        let quoted = "echo \"a\n# inside\n\"\n# hidden\necho ${#x} # note\n";
        assert_eq!(kept(quoted, "sh"), vec![1, 2, 3, 5]);
    }

    // #509 repair 1: the review's cases.

    #[test]
    fn a_template_literal_nested_in_a_template_expression_keeps_its_lines() {
        let src = "const t = `outer ${`inner\n// literal data\n`}`;\n// hidden\nf();\n";
        assert_eq!(kept(src, "ts"), vec![1, 2, 3, 5]);
        let braces = "const u = `${ {a: 1}.a } and ${fn({b: `x`})}\n// still text\n`;\n// hidden\n";
        assert_eq!(kept(braces, "js"), vec![1, 2, 3]);
    }

    #[test]
    fn quoted_heredoc_tags_with_spaces_and_lower_case_ruby_tags_keep_their_bodies() {
        let shell = "cat <<'END TEXT'\n# literal data\nEND TEXT\n# hidden\necho x\n";
        assert_eq!(kept(shell, "sh"), vec![1, 2, 3, 5]);
        let ruby = "s = <<doc\n# literal data\ndoc\n# hidden\nputs s\n";
        assert_eq!(kept(ruby, "rb"), vec![1, 2, 3, 5]);
    }

    #[test]
    fn only_a_body_s_first_string_is_a_docstring() {
        let value = "DATA = {\n    \"key\":\n    \"\"\"value\n    # literal data\n    \"\"\"\n}\n";
        assert_eq!(kept(value, "py"), vec![1, 2, 3, 4, 5, 6]);
        let header =
            "def f(\n    a,\n) -> int:  # note\n    \"\"\"Doc.\n    \"\"\"\n    return a\n";
        assert_eq!(kept(header, "py"), vec![1, 2, 3, 6]);
        let class = "class A:\n    '''Doc.'''\n    x = 1\n";
        assert_eq!(kept(class, "py"), vec![1, 3]);
        let branch = "if x:\n    \"\"\"not a docstring\n    \"\"\"\n";
        assert_eq!(kept(branch, "py"), vec![1, 2, 3]);
        let one_line = "def f(): return 1\n\"\"\"value\n\"\"\"\n";
        assert_eq!(kept(one_line, "py"), vec![1, 2, 3]);
    }

    #[test]
    fn a_block_comment_opened_mid_line_is_a_comment_to_its_close() {
        let src = "let n = 0; /*\nr#\"\n// actual comment\n\"#\n*/\nlet m = 1;\n";
        assert_eq!(kept(src, "rs"), vec![1, 6]);
        let closer = "int a; /* note\n\"not a string\n*/ char *s = \"\n// in string\n\";\n";
        assert_eq!(kept(closer, "c"), vec![1, 3, 4, 5]);
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
    fn a_truncated_line_whose_retained_prefix_looks_blank_or_unclosed_is_kept() {
        // Review H1: code past the retained prefix must never be hidden unseen.
        for head in [
            " ".repeat(MAX_OUTPUT_BYTES),
            format!("/* {}", "c".repeat(MAX_OUTPUT_BYTES)),
        ] {
            let src = format!("{head}fn hidden() {{}}\nfn visible() {{}}\n");
            let out = render(src.as_bytes(), "a.rs", &skim_input("a.rs", None, None)).unwrap();
            assert!(
                out.starts_with("     1\t"),
                "line 1 must be shown: {out:.80}"
            );
            assert!(out.contains("[output truncated: "), "{out:.80}");
            // A truncated line ends the window, as in a full read.
            assert!(out.contains("continue with offset=2]"), "{out:.80}");
        }
        let mut filter = skim_filter("a.rs").unwrap();
        assert!(!filter.keep("/* open", true));
        assert!(filter.keep("still comment, cut short", false));
        assert!(filter.keep("fn after() {}", true), "resumes as code");
    }

    #[test]
    fn the_peak_line_bytes_count_lines_finished_within_a_chunk() {
        let mut render = WindowedRender::start(b"", "a.rs", &input(None, None)).unwrap();
        render.feed(b"abcdef\n").unwrap();
        assert_eq!(render.peak_line_bytes(), 6);
        let mut render =
            WindowedRender::start(b"", "a.rs", &skim_input("a.rs", None, None)).unwrap();
        render.feed(b"// abcdefgh\n").unwrap();
        assert_eq!(render.peak_line_bytes(), 11);
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

    /// `contents` rendered with a chunk boundary at every file offset in `boundaries`, the
    /// sniff aside: no boundaries is the one-chunk read.
    fn render_split(contents: &[u8], input: &ReadInput, boundaries: &[usize]) -> String {
        let sniffed = sniff_len(contents.len() as u64);
        let mut render = WindowedRender::start(&contents[..sniffed], "f", input).unwrap();
        let mut from = sniffed;
        for &boundary in boundaries.iter().filter(|&&b| b > sniffed) {
            render.feed(&contents[from..boundary]).unwrap();
            from = boundary;
        }
        render.feed(&contents[from..]).unwrap();
        render.finish().unwrap()
    }

    /// A file of `first`, hidden comment lines made of `comment`, `block` and `last`, sized
    /// so that byte `at` of `block` lies at file offset `boundary`.
    fn straddling(
        first: &str,
        comment: &str,
        block: &str,
        last: &str,
        boundary: usize,
        at: usize,
    ) -> String {
        let mut remaining = boundary - at - first.len();
        let mut src = first.to_string();
        while remaining >= 200 {
            src.push_str(&format!("{comment} {}\n", "x".repeat(98 - comment.len())));
            remaining -= 100;
        }
        src.push_str(&format!(
            "{comment}{}\n",
            "x".repeat(remaining - comment.len() - 1)
        ));
        assert_eq!(src.len() + at, boundary);
        src + block + last
    }

    #[test]
    fn a_comment_or_docstring_across_a_chunk_boundary_skims_as_in_one_chunk() {
        // Issue #506 M3: the component reads in READ_BUFFER_BYTES chunks from the file start,
        // the native tool in READ_BUFFER_BYTES chunks after the sniff; every byte of the
        // block or docstring, its delimiters included, is put on each kind of boundary.
        let cases = [
            (
                "rs",
                "fn first() {}\n",
                "//",
                "/* opening line of a block comment\n   let hidden = 1;\n   still inside */\nfn after() {}\n",
                "fn last() {}\n",
                ["\tfn first() {}", "\tfn after() {}", "\tfn last() {}"],
            ),
            (
                "py",
                "import os\n",
                "#",
                "def after():\n    \"\"\"Docstring opening line.\n    let_hidden = 'looks like code'\n    \"\"\"\n",
                "    return 1\n",
                ["\timport os", "\tdef after():", "\t    return 1"],
            ),
        ];
        for (ext, first, comment, block, last, shown) in cases {
            let path = format!("a.{ext}");
            let input = skim_input(&path, None, None);
            for boundary in [READ_BUFFER_BYTES, BINARY_SNIFF_BYTES + READ_BUFFER_BYTES] {
                for at in 0..block.len() {
                    let src = straddling(first, comment, block, last, boundary, at);
                    let bytes = src.as_bytes();
                    let one_chunk = render_split(bytes, &input, &[]);
                    let component: Vec<usize> = (1..=bytes.len() / READ_BUFFER_BYTES)
                        .map(|n| n * READ_BUFFER_BYTES)
                        .collect();
                    let context = format!("{ext}: byte {at} of the block at offset {boundary}");
                    assert_eq!(
                        render_split(bytes, &input, &component),
                        one_chunk,
                        "{context}"
                    );
                    assert_eq!(
                        render(bytes, &path, &input).unwrap(),
                        one_chunk,
                        "{context}"
                    );
                    for line in shown {
                        assert!(one_chunk.contains(line), "{context}: {line} in {one_chunk}");
                    }
                    assert!(!one_chunk.contains("hidden ="), "{context}: {one_chunk}");
                    assert!(
                        !one_chunk.contains("opening line"),
                        "{context}: {one_chunk}"
                    );
                    assert!(
                        one_chunk
                            .ends_with(" lines hidden; read the file in full before editing it]"),
                        "{context}: {one_chunk}"
                    );
                }
            }
        }
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
