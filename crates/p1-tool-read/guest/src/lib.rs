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
pub const DESCRIPTION: &str = "Read a UTF-8 text file from the workspace, with numbered lines.\nUse `offset` and `limit` to page through a long file; the last line gives the next offset.\nRead a file before you edit or overwrite it: a mutation is refused until you have seen its current contents.";
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
            let end = start + input.limit.unwrap_or(DEFAULT_LIMIT) - 1;
            target = format!("{target}:{start}-{end}");
        }
        target
    })
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

/// The bounded portion of the line currently being scanned.
struct LineBuffer {
    shown: Vec<u8>,
    content_bytes: usize,
    last_byte: Option<u8>,
}

impl LineBuffer {
    fn new() -> Self {
        Self {
            shown: Vec::with_capacity(MAX_OUTPUT_BYTES),
            content_bytes: 0,
            last_byte: None,
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
            self.shown.extend_from_slice(&bytes[..keep]);
        }
    }

    fn reset(&mut self) {
        self.shown.clear();
        self.content_bytes = 0;
        self.last_byte = None;
    }
}

struct Window {
    start: usize,
    cap: usize,
    line_number: usize,
    emitted: usize,
    end: usize,
    stop_collecting: bool,
    out: String,
}

impl Window {
    fn wants_current_line(&self) -> bool {
        self.line_number + 1 > self.start && !self.stop_collecting && self.emitted < self.cap
    }

    fn finish_line(&mut self, line: &mut LineBuffer) {
        self.line_number += 1;
        if !self.wants_finished_line() {
            line.reset();
            return;
        }

        let content_bytes = line.content_bytes - usize::from(line.last_byte == Some(b'\r'));
        line.shown.truncate(line.shown.len().min(content_bytes));
        let shown_end = match std::str::from_utf8(&line.shown) {
            Ok(_) => line.shown.len(),
            Err(error) => error.valid_up_to(),
        };
        line.shown.truncate(shown_end);

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
            self.out
                .push_str(std::str::from_utf8(&line.shown).expect("validated line prefix"));
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
        line.reset();
    }

    fn wants_finished_line(&self) -> bool {
        self.line_number > self.start && !self.stop_collecting && self.emitted < self.cap
    }
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
        let mut render = Self {
            display: display.to_string(),
            offset,
            start,
            utf8: Utf8Validator::default(),
            line: LineBuffer::new(),
            window: Window {
                start,
                cap: limit.min(MAX_OUTPUT_LINES),
                line_number: 0,
                emitted: 0,
                end: start,
                stop_collecting: false,
                out: String::new(),
            },
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
            self.line
                .push(&remaining[..newline], self.window.wants_current_line());
            self.peak_line_bytes = self.peak_line_bytes.max(self.line.shown.len());
            self.window.finish_line(&mut self.line);
            remaining = &remaining[newline + 1..];
        }
        self.line.push(remaining, self.window.wants_current_line());
        self.peak_line_bytes = self.peak_line_bytes.max(self.line.shown.len());
        Ok(())
    }

    /// The most bytes of one line this read has retained so far: bounded by
    /// [`MAX_OUTPUT_BYTES`] however long the line is.
    pub fn peak_line_bytes(&self) -> usize {
        self.peak_line_bytes
    }

    /// End the read: the rendered window with its continuation footer, or why the read
    /// fails. A caller records the observation only on `Ok`.
    pub fn finish(mut self) -> Result<String, String> {
        self.utf8
            .finish()
            .map_err(|_| format!("{} is not valid UTF-8.", self.display))?;
        if self.line.content_bytes > 0 {
            self.window.finish_line(&mut self.line);
        }
        let total = self.window.line_number;
        if self.start >= total {
            return Err(format!(
                "offset {} is beyond the end of {} ({total} lines).",
                self.offset, self.display
            ));
        }
        let mut out = self.window.out;
        if self.window.end < total {
            out.push('\n');
            out.push_str(&format!(
                "[{} more lines; continue with offset={}]",
                total - self.window.end,
                self.window.end + 1
            ));
        }
        Ok(out)
    }
}

/// Render `contents`, a whole file already in memory, in [`READ_BUFFER_BYTES`] chunks
/// exactly as a streamed read renders it.
pub fn render(contents: &[u8], display: &str, input: &ReadInput) -> Result<String, String> {
    let sniffed = sniff_len(contents.len() as u64);
    let mut render = WindowedRender::start(&contents[..sniffed], display, input)?;
    for chunk in contents[sniffed..].chunks(READ_BUFFER_BYTES) {
        render.feed(chunk)?;
    }
    render.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input(offset: Option<i64>, limit: Option<i64>) -> ReadInput {
        ReadInput {
            file_path: "a.txt".into(),
            offset,
            limit,
        }
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
