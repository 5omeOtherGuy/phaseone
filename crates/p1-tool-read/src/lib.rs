//! The `read` tool: line-numbered reads of one workspace file.
//!
//! Confinement, atomic writes and observed-file tracking live in `p1-workspace`.
//! The model-facing declaration, input validation and rendering live in
//! `p1-read-guest`, which the component (`p1/read`) runs too. This crate is the
//! native adapter over both, owns the credential refusal (issue #142), and
//! provides the `workspace` and `snapshot` capabilities the component reads
//! through ([`capability`]).

pub mod capability;

use std::io::{ErrorKind, Read};
use std::path::{Path, PathBuf};

use p1_contracts::tool::ResultDescription;
use p1_contracts::{
    BoxFuture, CallDescription, DeclarationKind, Effect, Tool, ToolCall, ToolContext,
    ToolDeclaration, ToolIdentity, ToolInput, ToolOutcome, ToolStatus,
};
use p1_read_guest::{
    DESCRIPTION, NAME, RawInput, ReadInput, WindowedRender, input_schema, sniff_len,
};
use p1_workspace::{ObservedFiles, StreamingHash, Workspace};

pub use capability::{ReadCapability, capability_services};
pub use p1_workspace::ToolFace;

/// The internal read buffer: fixed and small, however large the file is.
const READ_BUFFER_BYTES: usize = p1_read_guest::READ_BUFFER_BYTES;
// The unit tests below predate the guest crate and name these as this crate's items.
#[cfg(test)]
use p1_read_guest::{BINARY_SNIFF_BYTES, MAX_OUTPUT_BYTES, Utf8Validator};

/// The `read` tool. Holds one agent's workspace and observation store.
pub struct ReadTool {
    workspace: Workspace,
    observed: ObservedFiles,
    declaration: ToolDeclaration,
    identity: ToolIdentity,
    /// `HOME` (or an injected temp home, in tests). The credential files under it are
    /// refused even with full access.
    home: Option<PathBuf>,
    /// Extra credential FILES from the XDG overrides p1-auth also honours, captured
    /// from the process environment when the tool is built.
    xdg_credentials: Vec<PathBuf>,
}

impl ReadTool {
    /// Build the tool with the default (`read`, Claude-family) face.
    pub fn new(workspace: Workspace, observed: ObservedFiles) -> Self {
        Self {
            workspace,
            observed,
            declaration: declaration(default_face()),
            identity: identity("claude"),
            home: env_path("HOME"),
            xdg_credentials: xdg_credentials(),
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
            home: self.home,
            xdg_credentials: self.xdg_credentials,
        }
    }

    /// Override the home directory whose credential files are refused. The host
    /// passes its injected `HOME`; a test passes a temp home.
    pub fn with_home(mut self, home: Option<PathBuf>) -> Self {
        self.home = home;
        self
    }
}

/// Whether `candidate` is one of the credential files `read` always refuses: the p1
/// auth store, anything under `~/.config/keys/`, and the other tools' auth files.
/// Compared on the canonicalised form, so a symlink or a relative path cannot slip
/// past. An empty `home` refuses only the XDG-named stores.
pub(crate) fn refuses_credentials(
    candidate: &Path,
    home: Option<&Path>,
    xdg_credentials: &[PathBuf],
) -> bool {
    let candidate = canonical_best_effort(candidate);
    if xdg_credentials
        .iter()
        .any(|path| canonical_best_effort(path) == candidate)
    {
        return true;
    }
    let Some(home) = home else {
        return false;
    };
    let home = canonical_best_effort(home);
    let keys = canonical_best_effort(&home.join(".config").join("keys"));
    if candidate == keys || candidate.starts_with(&keys) {
        return true;
    }
    [
        home.join(".config/p1/auth.json"),
        home.join(".codex/auth.json"),
        home.join(".claude/.credentials.json"),
        home.join(".local/share/opencode/auth.json"),
        home.join(".pi/agent/auth.json"),
    ]
    .iter()
    .any(|path| canonical_best_effort(path) == candidate)
}

/// A non-empty environment variable as a path, or `None`.
fn env_path(name: &str) -> Option<PathBuf> {
    std::env::var_os(name)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

/// The p1 and OpenCode stores move with their XDG override (p1-auth); a home-based
/// path below covers the default. `None` when the variable is unset.
pub(crate) fn xdg_credentials() -> Vec<PathBuf> {
    let mut files = Vec::new();
    if let Some(config) = env_path("XDG_CONFIG_HOME") {
        files.push(config.join("p1/auth.json"));
    }
    if let Some(data) = env_path("XDG_DATA_HOME") {
        files.push(data.join("opencode/auth.json"));
    }
    files
}

/// The canonical form of `path`, or — when it does not exist yet — its canonical
/// parent with the file name appended, so a refusal never falls back to a lexical
/// comparison against the whole path.
fn canonical_best_effort(path: &Path) -> PathBuf {
    match path.canonicalize() {
        Ok(canonical) => canonical,
        Err(_) => match path.parent().and_then(|parent| parent.canonicalize().ok()) {
            Some(parent) => parent.join(path.file_name().unwrap_or_default()),
            None => path.to_path_buf(),
        },
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
            verb: p1_read_guest::VERB,
            target: p1_read_guest::describe_target(&self.declaration.name, raw_input(call)),
            edit: None,
            destructive: false,
        }
    }

    fn describe_result(
        &self,
        _call: &ToolCall,
        result: &p1_contracts::ToolResultItem,
    ) -> ResultDescription {
        ResultDescription {
            summary: p1_read_guest::describe_result(
                &result.content,
                result.status == ToolStatus::Ok,
            ),
            detail: None,
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
            let home = self.home.clone();
            let xdg_credentials = self.xdg_credentials.clone();
            // All filesystem work runs on a blocking thread; the async thread
            // is never used for synchronous I/O.
            match tokio::task::spawn_blocking(move || {
                run(
                    &workspace,
                    &observed,
                    &input,
                    home.as_deref(),
                    &xdg_credentials,
                )
            })
            .await
            {
                // `run` bounds the window itself so the continuation trailer survives.
                Ok(Ok(content)) => ToolOutcome::ok(content),
                Ok(Err(message)) => ToolOutcome::error(message),
                Err(error) => ToolOutcome::error(format!("{tool} failed: {error}")),
            }
        })
    }
}

fn raw_input(call: &ToolCall) -> RawInput<'_> {
    match &call.input {
        ToolInput::Json(raw) => RawInput::Json(raw),
        ToolInput::Text(raw) => RawInput::Text(raw),
    }
}

fn parse_input(tool: &str, call: &ToolCall) -> Result<ReadInput, String> {
    p1_read_guest::parse_input(tool, raw_input(call))
}

fn run(
    workspace: &Workspace,
    observed: &ObservedFiles,
    input: &ReadInput,
    home: Option<&Path>,
    xdg_credentials: &[PathBuf],
) -> Result<String, String> {
    refuse_credentials(workspace, &input.file_path, home, xdg_credentials)?;

    let resolved = workspace
        .resolve(&input.file_path)
        .map_err(|error| error.to_string())?;
    let display = workspace.display(&resolved);

    let metadata = match std::fs::metadata(&resolved) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == ErrorKind::NotFound => {
            return Err(p1_read_guest::missing(&display));
        }
        Err(error) => {
            return Err(p1_read_guest::could_not_be_read(
                &display,
                &error.to_string(),
            ));
        }
    };
    if !metadata.is_file() {
        return Err(p1_read_guest::not_a_regular_file(&display));
    }
    if metadata.len() == 0 {
        // An empty file is a successful read of zero bytes: record it so a
        // later `write` to it is not treated as an unread blind overwrite.
        observed.record(&resolved, b"");
        return Ok(p1_read_guest::empty(&display));
    }

    let file = std::fs::File::open(&resolved)
        .map_err(|error| p1_read_guest::could_not_be_read(&display, &error.to_string()))?;

    // Only the requested window (plus small fixed buffers) is ever held in
    // memory: the file is streamed line by line, never loaded whole.
    read_windowed(file, metadata.len(), &resolved, &display, input, observed)
}

/// Issue #142: a credential file is refused even with full access, BEFORE any
/// confinement error, so the model is told why and never sees the file's bytes.
/// The component's `workspace` capability applies the same rule ([`capability`]).
pub(crate) fn refuse_credentials(
    workspace: &Workspace,
    requested: &str,
    home: Option<&Path>,
    xdg_credentials: &[PathBuf],
) -> Result<(), String> {
    let candidate = if Path::new(requested).is_absolute() {
        PathBuf::from(requested)
    } else {
        workspace.root().join(requested)
    };
    if refuses_credentials(&candidate, home, xdg_credentials) {
        return Err(p1_read_guest::credential_refusal(
            &workspace.display(&candidate),
        ));
    }
    Ok(())
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
    // The sniffed bytes are read first and fed through the normal pass before
    // the rest, without needing `Seek` (a synthetic or piped source may not
    // have one); see `WindowedRender::start` for why the sniff goes first.
    let mut sniff = vec![0u8; sniff_len(total_len)];
    reader
        .read_exact(&mut sniff)
        .map_err(|error| p1_read_guest::could_not_be_read(display, &error.to_string()))?;
    let mut render = WindowedRender::start(&sniff, display, input)?;
    let mut hash = StreamingHash::new();
    hash.update(&sniff);
    drop(sniff);

    let mut buffer = [0u8; READ_BUFFER_BYTES];
    loop {
        let bytes_read = reader
            .read(&mut buffer)
            .map_err(|error| p1_read_guest::could_not_be_read(display, &error.to_string()))?;
        if bytes_read == 0 {
            break;
        }
        hash.update(&buffer[..bytes_read]);
        render.feed(&buffer[..bytes_read])?;
    }
    #[cfg(test)]
    let max_line_buffer_bytes = render.peak_line_bytes();
    let output = render.finish()?;
    // A read always observes the FULL file, even when offset/limit windows the
    // returned lines: a later edit compares against the whole file.
    observed.record_streamed(resolved, hash);

    Ok(WindowedRead {
        output,
        #[cfg(test)]
        max_line_buffer_bytes,
    })
}

#[cfg(test)]
mod tests {
    use super::{MAX_OUTPUT_BYTES, READ_BUFFER_BYTES, ReadInput, ReadTool, parse_input};
    use p1_contracts::{
        DeclarationKind, Effect, Tool, ToolCall, ToolContext, ToolInput, ToolOutcome,
        ToolResultItem, ToolStatus,
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
        assert_eq!(
            tool.describe(&call(r#"{"file_path": "src/a.rs", "offset": 10}"#))
                .target
                .as_deref(),
            Some("src/a.rs:10-2009")
        );
        assert_eq!(
            tool.describe(&call(r#"{"file_path": "src/a.rs", "limit": 25}"#))
                .target
                .as_deref(),
            Some("src/a.rs:1-25")
        );
        assert!(!described.destructive);
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
        let result = ToolResultItem {
            call_id: "call-1".into(),
            name: "read".into(),
            status: outcome.status,
            content: outcome.content,
        };
        let described = tool.describe_result(&call(r#"{"file_path": "a.txt"}"#), &result);
        assert_eq!(described.summary, "3 lines · 0.0 kB");
        assert_eq!(described.detail, None);
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

    /// The credential files `read` refuses, relative to the home it was given.
    const CREDENTIAL_PATHS: [&str; 7] = [
        ".config/p1/auth.json",
        ".config/keys/tool.key",
        ".config/keys/nested/deeper.key",
        ".codex/auth.json",
        ".claude/.credentials.json",
        ".local/share/opencode/auth.json",
        ".pi/agent/auth.json",
    ];

    /// A temp home with every refused credential file, and the tool pointed at it.
    /// The workspace IS the home, so a relative credential path is inside the
    /// confinement and only the refusal can stop it.
    fn home_with_credentials() -> (tempfile::TempDir, ReadTool) {
        let home = tempfile::tempdir().unwrap();
        for relative in CREDENTIAL_PATHS {
            let path = home.path().join(relative);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, "{}\n").unwrap();
        }
        let observed = ObservedFiles::new();
        let tool = ReadTool::new(workspace(home.path()), observed)
            .with_home(Some(home.path().to_path_buf()));
        (home, tool)
    }

    #[tokio::test]
    async fn refuses_every_credential_path_inside_the_workspace() {
        let (_home, tool) = home_with_credentials();

        for relative in CREDENTIAL_PATHS {
            let outcome = execute(&tool, &format!("{{\"file_path\": \"{relative}\"}}")).await;
            assert_eq!(outcome.status, ToolStatus::Error, "{relative}");
            assert!(
                outcome.content.contains("read refuses credential files"),
                "{relative}: {}",
                outcome.content
            );
            assert!(
                outcome
                    .content
                    .contains("credentials never enter the model's context"),
                "{relative}: {}",
                outcome.content
            );
        }
    }

    #[tokio::test]
    async fn refuses_a_credential_file_outside_the_workspace_before_confinement() {
        let (home, _tool) = home_with_credentials();
        let elsewhere = tempfile::tempdir().unwrap();
        let observed = ObservedFiles::new();
        // The workspace does not contain the home at all, so the ordinary
        // confinement would reject the path anyway; the refusal must come first and
        // name the credential rule.
        let tool = ReadTool::new(workspace(elsewhere.path()), observed)
            .with_home(Some(home.path().to_path_buf()));
        let absolute = home.path().join(".codex/auth.json");

        let outcome = execute(
            &tool,
            &format!("{{\"file_path\": \"{}\"}}", absolute.display()),
        )
        .await;

        assert_eq!(outcome.status, ToolStatus::Error);
        assert!(
            outcome.content.contains("read refuses credential files"),
            "{}",
            outcome.content
        );
    }

    #[tokio::test]
    async fn still_reads_an_ordinary_file_from_the_same_home() {
        let (home, tool) = home_with_credentials();
        std::fs::write(home.path().join("notes.txt"), "alpha\nbeta\n").unwrap();

        let outcome = execute(&tool, r#"{"file_path": "notes.txt"}"#).await;

        assert_eq!(outcome.status, ToolStatus::Ok);
        assert_eq!(outcome.content, "     1\talpha\n     2\tbeta");
    }
}
