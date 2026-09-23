//! The `read` tool: line-numbered reads of one workspace file.
//!
//! Confinement, atomic writes and observed-file tracking live in `p1-workspace`.
//! This module owns the model-facing declaration, input validation, rendering
//! and the read-before-mutate observation.

use std::io::{ErrorKind, Read};
use std::path::Path;

use p1_contracts::{
    BoxFuture, CallDescription, DeclarationKind, Effect, Tool, ToolCall, ToolContext,
    ToolDeclaration, ToolIdentity, ToolInput, ToolOutcome, ToolStatus,
};
use p1_workspace::{ObservedFiles, StreamingHash, Workspace};
use serde::Deserialize;

pub use p1_workspace::ToolFace;

const NAME: &str = "read";
const DESCRIPTION: &str = "Read a UTF-8 text file from the workspace, with numbered lines.\nUse `offset` and `limit` to page through a long file; the last line gives the next offset.\nRead a file before you edit or overwrite it: a mutation is refused until you have seen its current contents.";
const DEFAULT_OFFSET: i64 = 1;
const DEFAULT_LIMIT: i64 = 2_000;
const MAX_OUTPUT_BYTES: usize = 50_000;
const MAX_OUTPUT_LINES: usize = 2_000;
/// A NUL anywhere in the first 8 KiB marks the file as binary.
const BINARY_SNIFF_BYTES: usize = 8 * 1024;
/// The internal read buffer: fixed and small, however large the file is.
const READ_BUFFER_BYTES: usize = 64 * 1024;

/// The `read` tool. Holds one agent's workspace and observation store.
pub struct ReadTool {
    workspace: Workspace,
    observed: ObservedFiles,
    declaration: ToolDeclaration,
    identity: ToolIdentity,
}

impl ReadTool {
    /// Build the tool with the default (`read`, Claude-family) face.
    pub fn new(workspace: Workspace, observed: ObservedFiles) -> Self {
        Self {
            workspace,
            observed,
            declaration: declaration(default_face()),
            identity: identity("claude"),
        }
    }

    /// Present the same implementation under another name/description and
    /// variant. The input schema and the semantics do not change.
    pub fn with_face(self, face: ToolFace, variant: &str) -> Self {
        Self {
            workspace: self.workspace,
            observed: self.observed,
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

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReadInput {
    file_path: String,
    #[serde(default)]
    offset: Option<i64>,
    #[serde(default)]
    limit: Option<i64>,
}

impl Tool for ReadTool {
    fn declaration(&self) -> &ToolDeclaration {
        &self.declaration
    }

    fn identity(&self) -> &ToolIdentity {
        &self.identity
    }

    fn effect(&self, _call: &ToolCall) -> Effect {
        Effect::ReadOnly
    }

    /// ADR-0057: the file this call reads, from the tool's own parsed input.
    fn describe(&self, call: &ToolCall) -> CallDescription {
        CallDescription {
            verb: "read",
            target: parse_input(&self.declaration.name, call)
                .ok()
                .map(|input| input.file_path),
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
            let observed = self.observed.clone();
            let tool = self.declaration.name.clone();
            // All filesystem work runs on a blocking thread; the async thread
            // is never used for synchronous I/O.
            match tokio::task::spawn_blocking(move || run(&workspace, &observed, &input)).await {
                // `run` bounds the window itself so the continuation trailer survives.
                Ok(Ok(content)) => ToolOutcome::ok(content),
                Ok(Err(message)) => ToolOutcome::error(message),
                Err(error) => ToolOutcome::error(format!("{tool} failed: {error}")),
            }
        })
    }
}

fn parse_input(tool: &str, call: &ToolCall) -> Result<ReadInput, String> {
    let raw = match &call.input {
        ToolInput::Json(raw) => raw,
        ToolInput::Text(_) => {
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

fn invalid(tool: &str, reason: &str) -> String {
    format!("Invalid input for {tool}: {reason}")
}

fn run(
    workspace: &Workspace,
    observed: &ObservedFiles,
    input: &ReadInput,
) -> Result<String, String> {
    let resolved = workspace
        .resolve(&input.file_path)
        .map_err(|error| error.to_string())?;
    let display = workspace.display(&resolved);

    let metadata = match std::fs::metadata(&resolved) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == ErrorKind::NotFound => {
            return Err(format!("{display} does not exist."));
        }
        Err(error) => return Err(format!("{display} could not be read: {error}")),
    };
    if !metadata.is_file() {
        return Err(format!("{display} is not a regular file."));
    }
    if metadata.len() == 0 {
        // An empty file is a successful read of zero bytes: record it so a
        // later `write` to it is not treated as an unread blind overwrite.
        observed.record(&resolved, b"");
        return Ok(format!("{display} is empty."));
    }

    let file = std::fs::File::open(&resolved)
        .map_err(|error| format!("{display} could not be read: {error}"))?;

    // Only the requested window (plus small fixed buffers) is ever held in
    // memory: the file is streamed line by line, never loaded whole.
    read_windowed(file, metadata.len(), &resolved, &display, input, observed)
}

/// Incremental UTF-8 validation with only an incomplete trailing character
/// retained between chunks.
#[derive(Default)]
struct Utf8Validator {
    pending: [u8; 4],
    pending_len: usize,
}

impl Utf8Validator {
    fn update(&mut self, mut bytes: &[u8]) -> Result<(), ()> {
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

    fn finish(self) -> Result<(), ()> {
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

struct WindowedRead {
    output: String,
    #[cfg(test)]
    max_line_buffer_bytes: usize,
}

/// Stream `reader` (exactly `total_len` bytes), retaining a bounded prefix of
/// the current line and requested window while validating and hashing every
/// byte and counting the lines that follow it.
fn read_windowed<R: Read>(
    reader: R,
    total_len: u64,
    resolved: &Path,
    display: &str,
    input: &ReadInput,
    observed: &ObservedFiles,
) -> Result<String, String> {
    read_windowed_impl(reader, total_len, resolved, display, input, observed)
        .map(|result| result.output)
}

fn read_windowed_impl<R: Read>(
    mut reader: R,
    total_len: u64,
    resolved: &Path,
    display: &str,
    input: &ReadInput,
    observed: &ObservedFiles,
) -> Result<WindowedRead, String> {
    // The binary sniff must run to completion, over exactly the bytes it
    // would see reading the whole file at once, before anything else is
    // checked — otherwise a NUL later in the file could race a UTF-8 error
    // from an earlier chunk and change which error is reported. The sniffed
    // bytes are fed through the normal pass before reading the rest, without
    // needing `Seek` (a synthetic or piped source may not have one).
    let sniff_len = total_len.min(BINARY_SNIFF_BYTES as u64) as usize;
    let mut sniff = vec![0u8; sniff_len];
    reader
        .read_exact(&mut sniff)
        .map_err(|error| format!("{display} could not be read: {error}"))?;
    if sniff.contains(&0) {
        return Err(format!("{display} is a binary file."));
    }

    let offset = input.offset.unwrap_or(DEFAULT_OFFSET) as usize;
    let limit = input.limit.unwrap_or(DEFAULT_LIMIT) as usize;
    let start = offset - 1;
    let mut hash = StreamingHash::new();
    let mut utf8 = Utf8Validator::default();
    let mut line = LineBuffer::new();
    let mut window = Window {
        start,
        cap: limit.min(MAX_OUTPUT_LINES),
        line_number: 0,
        emitted: 0,
        end: start,
        stop_collecting: false,
        out: String::new(),
    };
    #[cfg(test)]
    let mut max_line_buffer_bytes = 0;

    {
        let mut process = |chunk: &[u8]| -> Result<(), String> {
            hash.update(chunk);
            utf8.update(chunk)
                .map_err(|_| format!("{display} is not valid UTF-8."))?;
            let mut remaining = chunk;
            while let Some(newline) = remaining.iter().position(|byte| *byte == b'\n') {
                line.push(&remaining[..newline], window.wants_current_line());
                #[cfg(test)]
                {
                    max_line_buffer_bytes = max_line_buffer_bytes.max(line.shown.len());
                }
                window.finish_line(&mut line);
                remaining = &remaining[newline + 1..];
            }
            line.push(remaining, window.wants_current_line());
            #[cfg(test)]
            {
                max_line_buffer_bytes = max_line_buffer_bytes.max(line.shown.len());
            }
            Ok(())
        };

        process(&sniff)?;
        drop(sniff);
        let mut buffer = [0u8; READ_BUFFER_BYTES];
        loop {
            let bytes_read = reader
                .read(&mut buffer)
                .map_err(|error| format!("{display} could not be read: {error}"))?;
            if bytes_read == 0 {
                break;
            }
            process(&buffer[..bytes_read])?;
        }
    }
    utf8.finish()
        .map_err(|_| format!("{display} is not valid UTF-8."))?;
    if line.content_bytes > 0 {
        window.finish_line(&mut line);
    }
    let total = window.line_number;

    if start >= total {
        return Err(format!(
            "offset {offset} is beyond the end of {display} ({total} lines)."
        ));
    }
    // A read always observes the FULL file, even when offset/limit windows the
    // returned lines: a later edit compares against the whole file.
    observed.record_streamed(resolved, hash);

    if window.end < total {
        window.out.push('\n');
        window.out.push_str(&format!(
            "[{} more lines; continue with offset={}]",
            total - window.end,
            window.end + 1
        ));
    }
    Ok(WindowedRead {
        output: window.out,
        #[cfg(test)]
        max_line_buffer_bytes,
    })
}

#[cfg(test)]
mod tests {
    use super::{MAX_OUTPUT_BYTES, READ_BUFFER_BYTES, ReadInput, ReadTool, parse_input};
    use p1_contracts::{
        DeclarationKind, Effect, Tool, ToolCall, ToolContext, ToolInput, ToolOutcome, ToolStatus,
    };
    use p1_workspace::{Observation, ObservedFiles, ToolFace, Workspace};
    use std::path::Path;

    fn workspace(root: &Path) -> Workspace {
        Workspace::new(root).unwrap()
    }

    fn tool(root: &Path) -> (ReadTool, ObservedFiles) {
        let observed = ObservedFiles::new();
        (ReadTool::new(workspace(root), observed.clone()), observed)
    }

    fn call(arguments: &str) -> ToolCall {
        ToolCall {
            call_id: "call-1".into(),
            name: "read".into(),
            input: ToolInput::Json(arguments.to_string()),
        }
    }

    async fn execute(tool: &ReadTool, arguments: &str) -> ToolOutcome {
        let call = call(arguments);
        let context = ToolContext {
            cancel: p1_contracts::CancellationToken::new(),
        };
        tool.execute(&call, context).await
    }

    fn schema(tool: &ReadTool) -> serde_json::Value {
        match &tool.declaration().kind {
            DeclarationKind::Function { input_schema } => input_schema.clone(),
            other => panic!("expected a function declaration, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn declaration_is_a_function_with_the_exact_spec_schema() {
        let dir = tempfile::tempdir().unwrap();
        let (tool, _) = tool(dir.path());

        assert_eq!(tool.declaration().name, "read");
        let schema = schema(&tool);
        assert_eq!(schema["type"], "object");
        assert_eq!(schema["required"], serde_json::json!(["file_path"]));
        assert_eq!(schema["additionalProperties"], false);
        assert_eq!(schema["properties"]["file_path"]["type"], "string");
        assert_eq!(schema["properties"]["offset"]["minimum"], 1);
        assert_eq!(schema["properties"]["offset"]["default"], 1);
        assert_eq!(schema["properties"]["limit"]["minimum"], 1);
        assert_eq!(schema["properties"]["limit"]["default"], 2000);
        let properties = schema["properties"].as_object().unwrap();
        assert_eq!(properties.len(), 3);
        assert!(tool.declaration().description.contains("before you edit"));
    }

    #[test]
    fn identity_defaults_to_the_claude_variant_and_survives_a_face_change() {
        let dir = tempfile::tempdir().unwrap();
        let (tool, _) = tool(dir.path());
        assert_eq!(tool.identity().implementation, "p1-tool-read");
        assert_eq!(tool.identity().variant, "claude");

        let reshaped = tool.with_face(ToolFace::new("ReadFile", "custom"), "gpt");
        assert_eq!(reshaped.declaration().name, "ReadFile");
        assert_eq!(reshaped.declaration().description, "custom");
        assert_eq!(reshaped.identity().implementation, "p1-tool-read");
        assert_eq!(reshaped.identity().variant, "gpt");
    }

    #[test]
    fn effect_is_read_only() {
        let dir = tempfile::tempdir().unwrap();
        let (tool, _) = tool(dir.path());
        assert_eq!(tool.effect(&call("{}")), Effect::ReadOnly);
    }

    /// ADR-0057: the description comes from this tool's own parsed input.
    #[test]
    fn describe_names_the_file_it_reads() {
        let dir = tempfile::tempdir().unwrap();
        let (tool, _) = tool(dir.path());

        let described = tool.describe(&call(r#"{"file_path": "src/a.rs"}"#));
        assert_eq!(described.verb, "read");
        assert_eq!(described.target.as_deref(), Some("src/a.rs"));
        // Invalid input has no target — never a panic, never a guess.
        assert_eq!(tool.describe(&call("not json")).target, None);
    }

    #[tokio::test]
    async fn read_returns_line_numbered_content() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "alpha\nbeta\ngamma\n").unwrap();
        let (tool, _) = tool(dir.path());

        let outcome = execute(&tool, r#"{"file_path": "a.txt"}"#).await;

        assert_eq!(outcome.status, ToolStatus::Ok);
        assert_eq!(
            outcome.content,
            "     1\talpha\n     2\tbeta\n     3\tgamma"
        );
    }

    #[tokio::test]
    async fn read_offset_and_limit_window_and_report_more_lines() {
        let dir = tempfile::tempdir().unwrap();
        let body: String = (1..=10).map(|n| format!("line{n}\n")).collect();
        std::fs::write(dir.path().join("b.txt"), body).unwrap();
        let (tool, _) = tool(dir.path());

        let outcome = execute(&tool, r#"{"file_path": "b.txt", "offset": 3, "limit": 2}"#).await;

        assert_eq!(
            outcome.content,
            "     3\tline3\n     4\tline4\n[6 more lines; continue with offset=5]"
        );
    }

    #[tokio::test]
    async fn read_reports_a_single_remaining_line() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("c.txt"), "one\ntwo\n").unwrap();
        let (tool, _) = tool(dir.path());

        let outcome = execute(&tool, r#"{"file_path": "c.txt", "limit": 1}"#).await;

        assert_eq!(
            outcome.content,
            "     1\tone\n[1 more lines; continue with offset=2]"
        );
    }

    #[tokio::test]
    async fn read_empty_file_is_a_successful_empty_read() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("empty.txt"), "").unwrap();
        let (tool, observed) = tool(dir.path());

        let outcome = execute(&tool, r#"{"file_path": "empty.txt"}"#).await;

        assert_eq!(outcome.status, ToolStatus::Ok);
        assert_eq!(outcome.content, "empty.txt is empty.");
        // The empty contents are observed, so a later write is not a blind
        // overwrite.
        assert_eq!(
            observed.check_unchanged(&dir.path().join("empty.txt"), b""),
            Observation::Unchanged
        );
    }

    #[tokio::test]
    async fn read_rejects_a_missing_file_and_a_directory() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("subdir")).unwrap();
        let (tool, _) = tool(dir.path());

        let missing = execute(&tool, r#"{"file_path": "nope.txt"}"#).await;
        assert_eq!(missing.status, ToolStatus::Error);
        assert_eq!(missing.content, "nope.txt does not exist.");

        let directory = execute(&tool, r#"{"file_path": "subdir"}"#).await;
        assert_eq!(directory.status, ToolStatus::Error);
        assert_eq!(directory.content, "subdir is not a regular file.");
    }

    #[tokio::test]
    async fn read_rejects_a_binary_file() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("nul.dat"), b"alpha\0beta").unwrap();
        let (tool, _) = tool(dir.path());

        let outcome = execute(&tool, r#"{"file_path": "nul.dat"}"#).await;

        assert_eq!(outcome.status, ToolStatus::Error);
        assert_eq!(outcome.content, "nul.dat is a binary file.");
    }

    #[tokio::test]
    async fn read_rejects_invalid_utf8() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("binary.dat"), [b'a', 0xFF, b'b']).unwrap();
        let (tool, _) = tool(dir.path());

        let outcome = execute(&tool, r#"{"file_path": "binary.dat"}"#).await;

        assert_eq!(outcome.status, ToolStatus::Error);
        assert_eq!(outcome.content, "binary.dat is not valid UTF-8.");
    }

    #[tokio::test]
    async fn read_records_the_full_contents_as_observed() {
        let dir = tempfile::tempdir().unwrap();
        let contents = "alpha\nbeta\ngamma\n";
        std::fs::write(dir.path().join("a.txt"), contents).unwrap();
        let (tool, observed) = tool(dir.path());

        // A windowed read still observes the whole file.
        execute(&tool, r#"{"file_path": "a.txt", "limit": 1}"#).await;

        assert_eq!(
            observed.check_unchanged(&dir.path().join("a.txt"), contents.as_bytes()),
            Observation::Unchanged
        );
    }

    #[tokio::test]
    async fn read_rejects_a_path_outside_the_workspace() {
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("secret.txt"), "secret").unwrap();
        let (tool, _) = tool(dir.path());

        let escaped = execute(&tool, r#"{"file_path": "../secret.txt"}"#).await;
        assert_eq!(escaped.status, ToolStatus::Error);
        assert!(escaped.content.contains("escapes workspace"), "{escaped:?}");

        let absolute = outside.path().join("secret.txt");
        let absolute = execute(
            &tool,
            &serde_json::json!({ "file_path": absolute.to_str().unwrap() }).to_string(),
        )
        .await;
        assert_eq!(absolute.status, ToolStatus::Error);
        assert!(
            absolute.content.contains("escapes workspace"),
            "{absolute:?}"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn read_rejects_a_symlink_that_points_outside_the_workspace() {
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("secret.txt"), "secret").unwrap();
        std::os::unix::fs::symlink(outside.path(), dir.path().join("link")).unwrap();
        let (tool, _) = tool(dir.path());

        let outcome = execute(&tool, r#"{"file_path": "link/secret.txt"}"#).await;

        assert_eq!(outcome.status, ToolStatus::Error);
        assert!(outcome.content.contains("escapes workspace"), "{outcome:?}");
    }

    #[tokio::test]
    async fn read_offset_beyond_the_end_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "one\ntwo\n").unwrap();
        let (tool, _) = tool(dir.path());

        let outcome = execute(&tool, r#"{"file_path": "a.txt", "offset": 9}"#).await;

        assert_eq!(outcome.status, ToolStatus::Error);
        assert!(outcome.content.contains("beyond the end"), "{outcome:?}");
    }

    #[tokio::test]
    async fn read_bounds_a_single_huge_line() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("long.txt"), "x".repeat(60_000)).unwrap();
        let (tool, _) = tool(dir.path());

        let outcome = execute(&tool, r#"{"file_path": "long.txt"}"#).await;

        assert_eq!(outcome.status, ToolStatus::Ok);
        assert!(outcome.content.contains("[output truncated: showing"));
        assert!(
            outcome.content.len() < 51_000,
            "len={}",
            outcome.content.len()
        );
    }

    #[tokio::test]
    async fn invalid_input_reports_a_prefix_and_never_panics() {
        let dir = tempfile::tempdir().unwrap();
        let (tool, _) = tool(dir.path());
        let garbage = [
            "",
            "null",
            "[]",
            "{\"file_path\": 5}",
            "{\"file_path\":\"a.txt\",\"unknown\":1}",
            "{\"file_path\":\"a.txt\",\"offset\":0}",
            "{\"file_path\":\"a.txt\",\"limit\":0}",
            "\u{0}\u{1}{\"file_path\" garbage",
        ];
        for arguments in garbage {
            let outcome = execute(&tool, arguments).await;
            assert_eq!(outcome.status, ToolStatus::Error, "input: {arguments:?}");
            assert!(
                outcome.content.starts_with("Invalid input for read: "),
                "input: {arguments:?} -> {outcome:?}"
            );
        }
    }

    #[tokio::test]
    async fn text_input_is_invalid_input() {
        let dir = tempfile::tempdir().unwrap();
        let (tool, _) = tool(dir.path());
        let call = ToolCall {
            call_id: "call-1".into(),
            name: "read".into(),
            input: ToolInput::Text("path=a.txt".into()),
        };
        let context = ToolContext {
            cancel: p1_contracts::CancellationToken::new(),
        };

        let outcome = tool.execute(&call, context).await;

        assert_eq!(outcome.status, ToolStatus::Error);
        assert!(outcome.content.starts_with("Invalid input for read: "));
    }

    #[tokio::test]
    async fn execute_returns_cancelled_without_touching_the_filesystem() {
        let dir = tempfile::tempdir().unwrap();
        let (tool, _) = tool(dir.path());
        let call = call(r#"{"file_path": "new.txt"}"#);
        let cancel = p1_contracts::CancellationToken::new();
        cancel.cancel();

        let outcome = tool.execute(&call, ToolContext { cancel }).await;

        assert_eq!(outcome.status, ToolStatus::Cancelled);
        assert!(!dir.path().join("new.txt").exists());
    }

    #[tokio::test]
    async fn file_work_runs_off_the_async_thread() {
        // The synchronous file work is moved to a blocking task, so the
        // returned future is `Send` and `execute` completes on a current-thread
        // runtime without blocking the executor. The blocking pool's thread
        // identity is not observable with the `rt`-only tokio features.
        fn assert_send<T: Send>(_: &T) {}
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "alpha\n").unwrap();
        let (tool, _) = tool(dir.path());
        let call = call(r#"{"file_path": "a.txt"}"#);
        let context = ToolContext {
            cancel: p1_contracts::CancellationToken::new(),
        };

        let future = tool.execute(&call, context);
        assert_send(&future);
        let outcome = future.await;

        assert_eq!(outcome.status, ToolStatus::Ok);
    }

    #[test]
    fn parse_input_rejects_a_freeform_text_call() {
        let call = ToolCall {
            call_id: "c".into(),
            name: "read".into(),
            input: ToolInput::Text("anything".into()),
        };
        assert!(parse_input("read", &call).is_err());
    }

    #[test]
    fn utf8_validator_matches_std_for_every_split_and_small_chunk_size() {
        fn validate_incrementally(bytes: &[u8], split: usize, chunk_size: usize) -> bool {
            let mut validator = super::Utf8Validator::default();
            if validator.update(&bytes[..split]).is_err() {
                return false;
            }
            for chunk in bytes[split..].chunks(chunk_size) {
                if validator.update(chunk).is_err() {
                    return false;
                }
            }
            validator.finish().is_ok()
        }

        fn check_every_chunking(bytes: &[u8]) {
            let expected = std::str::from_utf8(bytes).is_ok();
            for split in 0..=bytes.len() {
                for chunk_size in 1..=7 {
                    assert_eq!(
                        validate_incrementally(bytes, split, chunk_size),
                        expected,
                        "split={split}, chunk_size={chunk_size}, bytes={bytes:?}"
                    );
                }
            }
        }

        let mixed = "aé€🦀Z¢水𐍈".as_bytes();
        check_every_chunking(mixed);
        for invalid_at in 0..=mixed.len() {
            let mut invalid = mixed.to_vec();
            invalid.insert(invalid_at, 0xff);
            check_every_chunking(&invalid);
        }
    }

    /// A synthetic, effectively unbounded source: it computes each byte from
    /// a repeating pattern rather than holding the "file" anywhere, so a
    /// multi-hundred-megabyte read never allocates megabytes to produce it.
    struct RepeatingPattern {
        pattern: &'static [u8],
        produced: u64,
        remaining: u64,
    }

    impl std::io::Read for RepeatingPattern {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            let want = (buf.len() as u64).min(self.remaining) as usize;
            for (index, slot) in buf[..want].iter_mut().enumerate() {
                let position = self.produced + index as u64;
                *slot = self.pattern[(position % self.pattern.len() as u64) as usize];
            }
            self.produced += want as u64;
            self.remaining -= want as u64;
            Ok(want)
        }
    }

    /// Wraps a reader and records the largest single buffer any caller ever
    /// asked it to fill, so a test can assert on peak buffered bytes instead
    /// of timing or RSS.
    struct TrackingReader<R> {
        inner: R,
        max_requested: usize,
    }

    impl<R: std::io::Read> std::io::Read for TrackingReader<R> {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            self.max_requested = self.max_requested.max(buf.len());
            self.inner.read(buf)
        }
    }

    #[test]
    fn read_windowed_never_asks_for_more_than_a_small_fixed_buffer() {
        // A synthetic 50 MB single line exercises both buffers that matter:
        // reads from the source and bytes retained from the current line.
        const TOTAL_LEN: u64 = 50 * 1024 * 1024;
        let source = RepeatingPattern {
            pattern: b"x",
            produced: 0,
            remaining: TOTAL_LEN,
        };
        let mut tracked = TrackingReader {
            inner: source,
            max_requested: 0,
        };

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("huge.txt");
        let observed = ObservedFiles::new();
        let input = ReadInput {
            file_path: "huge.txt".into(),
            offset: Some(1),
            limit: Some(2),
        };

        // `read_windowed` only needs a reader and the declared length: feed
        // it the synthetic source directly, wrapped so its request sizes are
        // observable, without ever materializing the 50 MB "file" anywhere.
        let result = super::read_windowed_impl(
            &mut tracked,
            TOTAL_LEN,
            &path,
            "huge.txt",
            &input,
            &observed,
        )
        .unwrap();

        assert!(
            result
                .output
                .contains("[52378807 bytes omitted from line 1]"),
            "{}",
            result.output
        );
        assert!(
            tracked.max_requested <= READ_BUFFER_BYTES,
            "a single read asked for {} bytes out of a {TOTAL_LEN}-byte source",
            tracked.max_requested,
        );
        assert!(
            result.max_line_buffer_bytes <= MAX_OUTPUT_BYTES,
            "the line buffer retained {} bytes",
            result.max_line_buffer_bytes
        );
        assert_eq!(
            observed.check_unchanged(&path, &[]),
            p1_workspace::Observation::ChangedSinceObserved,
            "the huge file must be observed by its real content, computed from the stream"
        );
    }

    #[test]
    fn read_windowed_accepts_a_character_split_across_chunks() {
        let mut contents = vec![b'a'; super::BINARY_SNIFF_BYTES - 1];
        contents.extend_from_slice("€tail".as_bytes());
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("split.txt");
        let observed = ObservedFiles::new();
        let input = ReadInput {
            file_path: "split.txt".into(),
            offset: Some(1),
            limit: Some(1),
        };

        let output = super::read_windowed(
            std::io::Cursor::new(&contents),
            contents.len() as u64,
            &path,
            "split.txt",
            &input,
            &observed,
        )
        .unwrap();

        assert!(output.ends_with("€tail"));
    }

    #[test]
    fn read_windowed_rejects_an_invalid_byte_deep_inside_a_long_line() {
        let mut contents = vec![b'a'; READ_BUFFER_BYTES * 3 + 17];
        contents.push(0xff);
        contents.extend_from_slice(b"tail");
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("invalid.txt");
        let observed = ObservedFiles::new();
        let input = ReadInput {
            file_path: "invalid.txt".into(),
            offset: Some(1),
            limit: Some(1),
        };

        let error = super::read_windowed(
            std::io::Cursor::new(&contents),
            contents.len() as u64,
            &path,
            "invalid.txt",
            &input,
            &observed,
        )
        .unwrap_err();

        assert_eq!(error, "invalid.txt is not valid UTF-8.");
    }

    #[test]
    fn long_line_streamed_hash_matches_observed_files_record() {
        let contents = vec![b'z'; MAX_OUTPUT_BYTES * 4 + 13];
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("long.txt");
        let streamed = ObservedFiles::new();
        let whole = ObservedFiles::new();
        let input = ReadInput {
            file_path: "long.txt".into(),
            offset: Some(1),
            limit: Some(1),
        };
        whole.record(&path, &contents);

        super::read_windowed(
            std::io::Cursor::new(&contents),
            contents.len() as u64,
            &path,
            "long.txt",
            &input,
            &streamed,
        )
        .unwrap();

        assert_eq!(
            streamed.check_unchanged(&path, &contents),
            whole.check_unchanged(&path, &contents)
        );
        assert_eq!(
            streamed.check_unchanged(&path, &contents),
            p1_workspace::Observation::Unchanged
        );
    }
}
