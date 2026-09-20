//! The `read` tool: line-numbered reads of one workspace file.
//!
//! Confinement, atomic writes and observed-file tracking live in `p1-workspace`.
//! This module owns the model-facing declaration, input validation, rendering
//! and the read-before-mutate observation.

use std::io::{BufRead, BufReader, ErrorKind, Read};
use std::path::Path;

use p1_contracts::{
    BoxFuture, DeclarationKind, Effect, Tool, ToolCall, ToolContext, ToolDeclaration, ToolIdentity,
    ToolInput, ToolOutcome, ToolStatus,
};
use p1_workspace::{ObservedFiles, StreamingHash, Workspace, bound_output};
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
/// The internal buffer of the streaming line reader: fixed and small, however
/// large the file is.
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
            "path": {
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
        "required": ["path"],
        "additionalProperties": false
    })
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReadInput {
    path: String,
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
        .resolve(&input.path)
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

/// Stream `reader` (exactly `total_len` bytes) line by line, collecting at
/// most the requested window while still validating and hashing every byte,
/// and counting (never storing) the lines that follow it.
fn read_windowed<R: Read>(
    mut reader: R,
    total_len: u64,
    resolved: &Path,
    display: &str,
    input: &ReadInput,
    observed: &ObservedFiles,
) -> Result<String, String> {
    // The binary sniff must run to completion, over exactly the bytes it
    // would see reading the whole file at once, before anything else is
    // checked — otherwise a NUL later in the file could race a UTF-8 error
    // from an earlier chunk and change which error is reported. `chain` lets
    // the sniffed bytes stand back in front of the stream for the real pass,
    // without needing `Seek` (a synthetic or piped source may not have one).
    let sniff_len = BINARY_SNIFF_BYTES.min(total_len as usize);
    let mut sniff = vec![0u8; sniff_len];
    reader
        .read_exact(&mut sniff)
        .map_err(|error| format!("{display} could not be read: {error}"))?;
    if sniff.contains(&0) {
        return Err(format!("{display} is a binary file."));
    }
    let chained = std::io::Cursor::new(sniff).chain(reader);
    let mut lines = BufReader::with_capacity(READ_BUFFER_BYTES, chained);

    let offset = input.offset.unwrap_or(DEFAULT_OFFSET) as usize;
    let limit = input.limit.unwrap_or(DEFAULT_LIMIT) as usize;
    let start = offset - 1;
    let window_cap = limit.min(MAX_OUTPUT_LINES);

    let mut hash = StreamingHash::new();
    let mut raw = Vec::new();
    let mut line_number = 0usize;
    let mut out = String::new();
    let mut emitted = 0usize;
    let mut stop_collecting = false;
    let mut end = start;

    loop {
        raw.clear();
        let bytes_read = lines
            .read_until(b'\n', &mut raw)
            .map_err(|error| format!("{display} could not be read: {error}"))?;
        if bytes_read == 0 {
            break;
        }
        // The hash covers every raw byte of the file, in order, exactly as
        // `ObservedFiles::record` would from the whole file at once.
        hash.update(&raw);
        // `\n` (0x0A) never appears inside a multi-byte UTF-8 sequence, so
        // checking each line chunk is equivalent to validating the whole
        // file as one string, and fails on the same first bad byte.
        let text =
            std::str::from_utf8(&raw).map_err(|_| format!("{display} is not valid UTF-8."))?;

        line_number += 1;
        if line_number <= start {
            continue;
        }
        if stop_collecting || emitted >= window_cap {
            continue;
        }
        let content = text.strip_suffix('\n').unwrap_or(text);
        let content = content.strip_suffix('\r').unwrap_or(content);
        let rendered = format!("{line_number:>6}\t{content}");
        if emitted > 0 && out.len() + 1 + rendered.len() > MAX_OUTPUT_BYTES {
            stop_collecting = true;
            continue;
        }
        if emitted > 0 {
            out.push('\n');
        }
        out.push_str(&rendered);
        end = line_number;
        emitted += 1;
    }
    let total = line_number;

    if start >= total {
        return Err(format!(
            "offset {offset} is beyond the end of {display} ({total} lines)."
        ));
    }
    // A read always observes the FULL file, even when offset/limit windows the
    // returned lines: a later edit compares against the whole file.
    observed.record_streamed(resolved, hash);

    // A single line larger than the byte budget is cut by the shared bounding rule.
    let mut out = bound_output(&out, MAX_OUTPUT_BYTES, usize::MAX);
    if end < total {
        out.push('\n');
        out.push_str(&format!(
            "[{} more lines; continue with offset={}]",
            total - end,
            end + 1
        ));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::{READ_BUFFER_BYTES, ReadInput, ReadTool, parse_input};
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
        assert_eq!(schema["required"], serde_json::json!(["path"]));
        assert_eq!(schema["additionalProperties"], false);
        assert_eq!(schema["properties"]["path"]["type"], "string");
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

    #[tokio::test]
    async fn read_returns_line_numbered_content() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "alpha\nbeta\ngamma\n").unwrap();
        let (tool, _) = tool(dir.path());

        let outcome = execute(&tool, r#"{"path": "a.txt"}"#).await;

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

        let outcome = execute(&tool, r#"{"path": "b.txt", "offset": 3, "limit": 2}"#).await;

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

        let outcome = execute(&tool, r#"{"path": "c.txt", "limit": 1}"#).await;

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

        let outcome = execute(&tool, r#"{"path": "empty.txt"}"#).await;

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

        let missing = execute(&tool, r#"{"path": "nope.txt"}"#).await;
        assert_eq!(missing.status, ToolStatus::Error);
        assert_eq!(missing.content, "nope.txt does not exist.");

        let directory = execute(&tool, r#"{"path": "subdir"}"#).await;
        assert_eq!(directory.status, ToolStatus::Error);
        assert_eq!(directory.content, "subdir is not a regular file.");
    }

    #[tokio::test]
    async fn read_rejects_a_binary_file() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("nul.dat"), b"alpha\0beta").unwrap();
        let (tool, _) = tool(dir.path());

        let outcome = execute(&tool, r#"{"path": "nul.dat"}"#).await;

        assert_eq!(outcome.status, ToolStatus::Error);
        assert_eq!(outcome.content, "nul.dat is a binary file.");
    }

    #[tokio::test]
    async fn read_rejects_invalid_utf8() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("binary.dat"), [b'a', 0xFF, b'b']).unwrap();
        let (tool, _) = tool(dir.path());

        let outcome = execute(&tool, r#"{"path": "binary.dat"}"#).await;

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
        execute(&tool, r#"{"path": "a.txt", "limit": 1}"#).await;

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

        let escaped = execute(&tool, r#"{"path": "../secret.txt"}"#).await;
        assert_eq!(escaped.status, ToolStatus::Error);
        assert!(escaped.content.contains("escapes workspace"), "{escaped:?}");

        let absolute = outside.path().join("secret.txt");
        let absolute = execute(
            &tool,
            &serde_json::json!({ "path": absolute.to_str().unwrap() }).to_string(),
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

        let outcome = execute(&tool, r#"{"path": "link/secret.txt"}"#).await;

        assert_eq!(outcome.status, ToolStatus::Error);
        assert!(outcome.content.contains("escapes workspace"), "{outcome:?}");
    }

    #[tokio::test]
    async fn read_offset_beyond_the_end_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "one\ntwo\n").unwrap();
        let (tool, _) = tool(dir.path());

        let outcome = execute(&tool, r#"{"path": "a.txt", "offset": 9}"#).await;

        assert_eq!(outcome.status, ToolStatus::Error);
        assert!(outcome.content.contains("beyond the end"), "{outcome:?}");
    }

    #[tokio::test]
    async fn read_bounds_a_single_huge_line() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("long.txt"), "x".repeat(60_000)).unwrap();
        let (tool, _) = tool(dir.path());

        let outcome = execute(&tool, r#"{"path": "long.txt"}"#).await;

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
            "{\"path\": 5}",
            "{\"path\":\"a.txt\",\"unknown\":1}",
            "{\"path\":\"a.txt\",\"offset\":0}",
            "{\"path\":\"a.txt\",\"limit\":0}",
            "\u{0}\u{1}{\"path\" garbage",
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
        let call = call(r#"{"path": "new.txt"}"#);
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
        let call = call(r#"{"path": "a.txt"}"#);
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
        // A 22 MB "file" of uniform 11-byte lines: far bigger than the
        // window and the byte cap, so a regression back to loading the whole
        // file (e.g. `fs::read`, whose single big read matches the file's
        // size) would show up as a huge `max_requested`.
        const LINE: &[u8] = b"abcdefghij\n";
        const TOTAL_LINES: u64 = 2_000_000;
        let total_len = LINE.len() as u64 * TOTAL_LINES;
        let source = RepeatingPattern {
            pattern: LINE,
            produced: 0,
            remaining: total_len,
        };
        let mut tracked = TrackingReader {
            inner: source,
            max_requested: 0,
        };

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("huge.txt");
        let observed = ObservedFiles::new();
        let input = ReadInput {
            path: "huge.txt".into(),
            offset: Some(1),
            limit: Some(2),
        };

        // `read_windowed` only needs a reader and the declared length: feed
        // it the synthetic source directly, wrapped so its request sizes are
        // observable, without ever materializing the 22 MB "file" anywhere.
        let out = super::read_windowed(
            &mut tracked,
            total_len,
            &path,
            "huge.txt",
            &input,
            &observed,
        )
        .unwrap();

        assert_eq!(
            out,
            "     1\tabcdefghij\n     2\tabcdefghij\n[1999998 more lines; continue with offset=3]"
        );
        assert!(
            tracked.max_requested <= 2 * READ_BUFFER_BYTES,
            "a single read asked for {} bytes out of a {total_len}-byte source; \
             the window builder must never buffer more than a small fixed amount",
            tracked.max_requested,
        );
        assert_eq!(
            observed.check_unchanged(&path, &[]),
            p1_workspace::Observation::ChangedSinceObserved,
            "the huge file must be observed by its real content, computed from the stream"
        );
    }
}
