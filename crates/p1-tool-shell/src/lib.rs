//! The `shell` tool: one-shot `bash -lc <command>` execution.
//!
//! Runs from the workspace root with stdin closed and its own process group, so
//! a timeout or cancellation can terminate the whole group (SIGTERM, then
//! SIGKILL) instead of leaving backgrounded children behind. stdout and stderr
//! are captured interleaved in arrival order while the command runs, and the
//! captured bytes are bounded as they are collected so a flooding command cannot
//! exhaust memory. Output bounding and the workspace live in `p1-workspace`.
//!
//! The child never inherits p1's environment: it is cleared and rebuilt from an
//! injected snapshot by [`ENV_ALLOW`], [`ENV_ALLOW_PREFIXES`] and the names added
//! with [`ShellTool::with_env_pass`], sandboxed or not.
//!
//! The crate is split in two. [`ProcessService`] (`process`) is the native part:
//! sandbox, spawn, environment policy, bounded capture, timeout and group kill,
//! all fixed at assembly so a request carries only a command and a timeout.
//! [`ShellTool`] is the tool logic on top of it — input parsing, declaration and
//! identity, destructiveness, output filters and result formatting — and holds no
//! process capability beyond the service it owns.

mod destructive;
mod filter;
mod process;

use std::ffi::OsString;
use std::time::Duration;

use p1_contracts::tool::{ResultDescription, ResultDetail};
use p1_contracts::{
    BoxFuture, CallDescription, DeclarationKind, Effect, Tool, ToolCall, ToolContext,
    ToolDeclaration, ToolIdentity, ToolInput, ToolOutcome, ToolResultItem, ToolStatus,
};
use p1_workspace::{ToolFace, Workspace};
use serde::Deserialize;

pub use process::{
    CREDENTIAL_DIRECTORIES, DEFAULT_HOME_VISIBLE, ENV_ALLOW, ENV_ALLOW_PREFIXES, ProcessEnd,
    ProcessFailure, ProcessOutcome, ProcessRequest, ProcessService, Sandbox, SandboxError,
    bwrap_args,
};

const NAME: &str = "shell";
const DESCRIPTION: &str = "Run a shell command with `bash -lc` from the workspace root, with stdin closed.\nstdout and stderr are captured together; the last line reports the exit code. Non-zero exits are not tool errors.\nSet `timeout_seconds` for long commands; on timeout or cancellation the whole process group is killed.\nThe output of a recognised command (`cargo test`/`build`/`check`/`clippy`, `git status`/`log`/`diff`, `npm`/`pnpm` test) is summarised unless `raw: true` is passed.";
const DEFAULT_TIMEOUT_SECONDS: i64 = 120;
const MIN_TIMEOUT_SECONDS: i64 = 1;
const MAX_TIMEOUT_SECONDS: i64 = 3_600;
const MAX_OUTPUT_BYTES: usize = 50_000;
/// The paragraph the model sees when the host turned the sandbox on. Appended to
/// whatever face the environment gave the tool, so a `with_face` override keeps it.
const SANDBOX_PARAGRAPH: &str = "Commands run in a sandbox: only the workspace and /tmp are writable, the rest of the filesystem is read-only, and most of the home directory is not visible. Do not try to install software outside the workspace.";

/// The `shell` tool. Holds one agent's workspace and the process service that
/// runs its commands, sandboxed when the host chose it.
pub struct ShellTool {
    workspace: Workspace,
    /// The face BEFORE the sandbox paragraph and the variant BEFORE the
    /// `+sandbox` suffix, so `with_face` and `sandboxed` compose in either order
    /// without stacking (requirement 4).
    face: ToolFace,
    variant: String,
    process: ProcessService,
    declaration: ToolDeclaration,
    identity: ToolIdentity,
}

impl ShellTool {
    /// Build the tool with the default (`shell`, Claude-family) face.
    pub fn new(workspace: Workspace) -> Self {
        Self {
            process: ProcessService::new(workspace.root()),
            workspace,
            face: default_face(),
            variant: "claude".to_string(),
            declaration: declaration(default_face()),
            identity: identity("claude"),
        }
        .composed()
    }

    /// Replace the environment snapshot the command is rebuilt from. The default
    /// is the process environment at construction; tests inject a snapshot so
    /// they never mutate the process environment. Composes with `with_face` and
    /// `sandboxed` in any order.
    pub fn with_env_snapshot(self, snapshot: Vec<(OsString, OsString)>) -> Self {
        Self {
            process: self.process.with_env_snapshot(snapshot),
            ..self
        }
    }

    /// Add variable NAMES to the allow-list, on top of [`ENV_ALLOW`] and
    /// [`ENV_ALLOW_PREFIXES`]. Composes with `with_face` and `sandboxed` in any
    /// order.
    pub fn with_env_pass(self, names: Vec<String>) -> Self {
        Self {
            process: self.process.with_env_pass(names),
            ..self
        }
    }

    /// Present the same implementation under another name/description and
    /// variant. The input schema and the semantics do not change.
    pub fn with_face(self, face: ToolFace, variant: &str) -> Self {
        Self {
            face,
            variant: variant.to_string(),
            ..self
        }
        .composed()
    }

    /// Put every command in a bubblewrap sandbox.
    ///
    /// Probes ONCE (`bwrap <args> true`), so an unusable sandbox fails assembly,
    /// not the first command. The sandbox's description paragraph and
    /// `+sandbox` variant survive later `with_face` calls and vice versa.
    pub fn sandboxed(self, sandbox: Sandbox) -> Result<Self, SandboxError> {
        Ok(Self {
            process: self.process.sandboxed(sandbox)?,
            ..self
        }
        .composed())
    }

    /// Recompute the declaration and identity from the face, the variant and
    /// whether a sandbox is on. Called by every constructor, so composing the
    /// sandbox and a face in either order never doubles the paragraph or suffix.
    fn composed(mut self) -> Self {
        let sandboxed = self.process.is_sandboxed();
        let description = if sandboxed {
            format!("{}\n{SANDBOX_PARAGRAPH}", self.face.description)
        } else {
            self.face.description.clone()
        };
        let variant = if sandboxed {
            format!("{}+sandbox", self.variant)
        } else {
            self.variant.clone()
        };
        self.declaration = declaration(ToolFace::new(self.face.name.clone(), description));
        self.identity = identity(&variant);
        self
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
            "command": {
                "type": "string",
                "description": "Command line, run with `bash -lc` from the workspace root."
            },
            "timeout_seconds": {
                "type": "integer",
                "minimum": 1,
                "maximum": 3600,
                "default": 120,
                "description": "Seconds before the command and its process group are killed."
            },
            "raw": {
                "type": "boolean",
                "default": false,
                "description": "Return the full, unfiltered output instead of the summary."
            }
        },
        "required": ["command"],
        "additionalProperties": false
    })
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ShellInput {
    command: String,
    #[serde(default)]
    timeout_seconds: Option<i64>,
    /// Skip the structured output filter: the model asked for the full log.
    #[serde(default)]
    raw: bool,
}

impl Tool for ShellTool {
    fn declaration(&self) -> &ToolDeclaration {
        &self.declaration
    }

    fn identity(&self) -> &ToolIdentity {
        &self.identity
    }

    fn effect(&self, _call: &ToolCall) -> Effect {
        Effect::Executes
    }

    /// ADR-0057: the command's first line, trimmed to 80 characters, from the
    /// tool's own parsed input.
    fn describe(&self, call: &ToolCall) -> CallDescription {
        let input = parse_input(&self.declaration.name, call).ok();
        CallDescription {
            verb: "run",
            target: input.as_ref().map(|input| {
                let first = input.command.lines().next().unwrap_or_default().trim();
                first.chars().take(80).collect()
            }),
            edit: None,
            // Invalid input is classified at the tool's worst case, as required
            // by the Tool contract. Execution will still return the input error.
            destructive: input.as_ref().is_none_or(|input| {
                destructive::is_destructive(&input.command, self.workspace.root())
            }),
        }
    }

    fn describe_result(&self, _call: &ToolCall, result: &ToolResultItem) -> ResultDescription {
        let mut lines: Vec<&str> = result.content.lines().collect();
        let exit_code = lines.last().and_then(|line| {
            line.strip_prefix("[exit code: ")
                .and_then(|code| code.strip_suffix(']'))
                .and_then(|code| code.parse().ok())
        });
        // Shell terminal lines are framing, not command output. Only an exit
        // footer carries an exit code, but timeout/cancellation/signal footers
        // must not leak into the command tail either.
        if lines.last().is_some_and(|line| {
            exit_code.is_some()
                || matches!(*line, "[cancelled]" | "[terminated by an unknown signal]")
                || line.starts_with("[timed out after ")
                || line.starts_with("[terminated by signal ")
        }) {
            lines.pop();
        }
        let line_count = lines.len();
        let summary = if result.status == ToolStatus::Ok {
            match exit_code {
                Some(code) => format!("exit {code} · {line_count} lines"),
                None => format!("{line_count} lines"),
            }
        } else {
            result
                .content
                .lines()
                .next()
                .unwrap_or_default()
                .to_string()
        };
        ResultDescription {
            summary,
            detail: Some(ResultDetail::Command {
                exit_code,
                elapsed_ms: None,
                tail: lines.into_iter().map(str::to_string).collect(),
            }),
        }
    }

    fn execute<'a>(
        &'a self,
        call: &'a ToolCall,
        context: ToolContext,
    ) -> BoxFuture<'a, ToolOutcome> {
        Box::pin(async move {
            // Cancellation before any work: no process is started.
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
            let timeout = Duration::from_secs(
                input
                    .timeout_seconds
                    .unwrap_or(DEFAULT_TIMEOUT_SECONDS)
                    .unsigned_abs(),
            );
            let request = ProcessRequest {
                command: &input.command,
                timeout,
            };
            let run = self.process.run(request, &context.cancel).await;
            outcome(run, &input.command, timeout, input.raw)
        })
    }
}

fn parse_input(tool: &str, call: &ToolCall) -> Result<ShellInput, String> {
    let raw = match &call.input {
        ToolInput::Json(raw) => raw,
        ToolInput::Text(_) => {
            return Err(invalid(
                tool,
                "expected a JSON object input, got freeform text",
            ));
        }
    };
    let input: ShellInput =
        serde_json::from_str(raw).map_err(|error| invalid(tool, &error.to_string()))?;
    if matches!(
        input.timeout_seconds,
        Some(seconds) if !(MIN_TIMEOUT_SECONDS..=MAX_TIMEOUT_SECONDS).contains(&seconds)
    ) {
        return Err(invalid(
            tool,
            "`timeout_seconds` must be between 1 and 3600",
        ));
    }
    Ok(input)
}

fn invalid(tool: &str, reason: &str) -> String {
    format!("Invalid input for {tool}: {reason}")
}

/// A completed command whose output may be summarised: what ran, whether the
/// model asked for the full log, and whether it exited 0. Absent for an
/// incomplete run (cancellation, timeout, a signal) — there is no complete
/// output to summarise then.
#[derive(Clone, Copy)]
struct Filter<'a> {
    command: &'a str,
    raw: bool,
    exit_ok: bool,
}

impl Filter<'_> {
    /// The model-visible body: the summary plus its marker line when a filter
    /// applied, the raw body when none did (unrecognised command, decline,
    /// panic, no reduction) or `raw: true` was passed.
    fn apply<'a>(&self, body: &'a str) -> std::borrow::Cow<'a, str> {
        if self.raw {
            return std::borrow::Cow::Borrowed(body);
        }
        match filter::filter_output(self.command, body, self.exit_ok) {
            Some(summary) => std::borrow::Cow::Owned(format!("{summary}\n{FILTERED_MARKER}")),
            None => std::borrow::Cow::Borrowed(body),
        }
    }
}

/// The model-visible result of a run: the captured output plus the footer for
/// how it ended. `timeout` is only what a timed-out footer reports.
fn outcome(run: ProcessOutcome, command: &str, timeout: Duration, raw: bool) -> ToolOutcome {
    let ProcessOutcome { output, end } = run;
    match end {
        ProcessEnd::Exited(code) => {
            // The seam: the filter only ever sees a COMPLETED command, and
            // `exit_ok` is true only for exit code 0.
            let filter = Filter {
                command,
                raw,
                exit_ok: code == 0,
            };
            render(
                &output,
                &format!("[exit code: {code}]"),
                ToolStatus::Ok,
                Some(filter),
            )
        }
        // A cancelled or timed-out command is an incomplete run with no exit
        // status, and a signalled one was killed before it could exit: their
        // output is never summarised.
        ProcessEnd::Cancelled => render(&output, "[cancelled]", ToolStatus::Cancelled, None),
        ProcessEnd::TimedOut => render(
            &output,
            &format!("[timed out after {} s]", timeout.as_secs()),
            ToolStatus::Error,
            None,
        ),
        ProcessEnd::TerminatedBySignal(signal) => render(
            &output,
            &format!("[terminated by signal {signal}]"),
            ToolStatus::Error,
            None,
        ),
        ProcessEnd::TerminatedByUnknownSignal => render(
            &output,
            "[terminated by an unknown signal]",
            ToolStatus::Error,
            None,
        ),
        ProcessEnd::Failed(failure) => ToolOutcome::error(match failure {
            ProcessFailure::Start { program, error } => {
                format!("failed to start {program}: {error}")
            }
            ProcessFailure::Capture { program, stream } => {
                format!("failed to capture {program} {stream}")
            }
            ProcessFailure::Wait { program, error } => {
                format!("failed to wait for {program}: {error}")
            }
        }),
    }
}

/// Render captured output plus a footer as the model-visible content.
fn render(
    bytes: &[u8],
    footer: &str,
    status: ToolStatus,
    filter: Option<Filter<'_>>,
) -> ToolOutcome {
    let text = String::from_utf8_lossy(bytes);
    // The footer goes on its own line without an extra blank line after the
    // command's usual trailing newline.
    let body = text.trim_end_matches('\n');
    // The structured filter runs BEFORE the byte bound: a summary is the
    // smaller, more useful body to squeeze when it is still too long.
    let body = match filter {
        Some(filter) => filter.apply(body),
        None => std::borrow::Cow::Borrowed(body),
    };
    // Lossy decoding can TRIPLE the size of binary output (each bad byte becomes
    // U+FFFD), pushing already-capped bytes past the content bound. Squeeze the body
    // — head and tail kept, like the collector — and never bound the footer: the exit
    // code must survive however noisy the output was.
    let body = squeeze(&body, MAX_OUTPUT_BYTES - FOOTER_RESERVE);
    let content = if body.is_empty() {
        footer.to_string()
    } else {
        format!("{body}\n{footer}")
    };
    ToolOutcome { status, content }
}

const FOOTER_RESERVE: usize = 2_000;

/// Last line of a summarised result, before the exit-code footer. The raw log
/// stays one call away, which is what makes summarising safe.
const FILTERED_MARKER: &str = "[output filtered; pass raw:true for the full log]";

/// Keep the first and last halves of `text` (cut on char boundaries) when it exceeds `max`.
fn squeeze(text: &str, max: usize) -> std::borrow::Cow<'_, str> {
    if text.len() <= max {
        return std::borrow::Cow::Borrowed(text);
    }
    let half = max / 2;
    let mut head_end = half;
    while !text.is_char_boundary(head_end) {
        head_end -= 1;
    }
    let mut tail_start = text.len() - half;
    while !text.is_char_boundary(tail_start) {
        tail_start += 1;
    }
    std::borrow::Cow::Owned(format!(
        "{}\n[… {} bytes omitted …]\n{}",
        &text[..head_end],
        tail_start - head_end,
        &text[tail_start..]
    ))
}

#[cfg(test)]
mod tests {
    use super::{ProcessService, ShellTool, outcome, parse_input};
    use nix::sys::signal::{Signal, kill};
    use nix::unistd::Pid;
    use p1_contracts::tool::ResultDetail;
    use p1_contracts::{
        CancellationToken, DeclarationKind, Effect, Tool, ToolCall, ToolContext, ToolInput,
        ToolOutcome, ToolResultItem, ToolStatus,
    };
    use p1_workspace::{ToolFace, Workspace};
    use std::ffi::OsString;
    use std::path::Path;
    use std::time::{Duration, Instant};

    fn tool(root: &Path) -> ShellTool {
        ShellTool::new(Workspace::new(root).unwrap())
    }

    fn call(arguments: &str) -> ToolCall {
        ToolCall {
            call_id: "call-1".into(),
            name: "shell".into(),
            input: ToolInput::Json(arguments.to_string()),
        }
    }

    async fn execute(tool: &ShellTool, arguments: &str) -> ToolOutcome {
        let call = call(arguments);
        let context = ToolContext {
            cancel: CancellationToken::new(),
        };
        tool.execute(&call, context).await
    }

    fn schema(tool: &ShellTool) -> serde_json::Value {
        match &tool.declaration().kind {
            DeclarationKind::Function { input_schema } => input_schema.clone(),
            other => panic!("expected a function declaration, got {other:?}"),
        }
    }

    fn pid_file(root: &Path, name: &str) -> Option<i32> {
        let text = std::fs::read_to_string(root.join(name)).ok()?;
        text.trim().parse().ok()
    }

    /// Wait (bounded) until `pid` is gone.
    fn wait_for_gone(pid: i32) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if kill(Pid::from_raw(pid), None::<Signal>).is_err() {
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        panic!("process {pid} is still alive");
    }

    #[tokio::test]
    async fn echo_reports_output_and_exit_code() {
        let dir = tempfile::tempdir().unwrap();
        let tool = tool(dir.path());

        let outcome = execute(&tool, r#"{"command": "echo hi"}"#).await;

        assert_eq!(outcome.status, ToolStatus::Ok);
        assert_eq!(outcome.content, "hi\n[exit code: 0]");
    }

    #[tokio::test]
    async fn stdout_and_stderr_are_captured_together() {
        let dir = tempfile::tempdir().unwrap();
        let tool = tool(dir.path());

        let outcome = execute(&tool, r#"{"command": "echo out; echo err >&2; exit 3"}"#).await;

        assert_eq!(outcome.status, ToolStatus::Ok);
        assert!(outcome.content.contains("out"), "{outcome:?}");
        assert!(outcome.content.contains("err"), "{outcome:?}");
        assert!(outcome.content.contains("[exit code: 3]"), "{outcome:?}");
    }

    #[tokio::test]
    async fn commands_run_from_the_workspace_root() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let tool = tool(dir.path());

        let outcome = execute(&tool, r#"{"command": "pwd"}"#).await;

        assert_eq!(outcome.status, ToolStatus::Ok);
        assert_eq!(
            outcome.content,
            format!("{}\n[exit code: 0]", root.display())
        );
    }

    #[tokio::test]
    async fn stdin_is_closed_so_cat_returns_immediately() {
        let dir = tempfile::tempdir().unwrap();
        let tool = tool(dir.path());
        let started = Instant::now();

        let outcome = execute(&tool, r#"{"command": "cat"}"#).await;

        assert!(started.elapsed() < Duration::from_secs(5));
        assert_eq!(outcome.status, ToolStatus::Ok);
        assert_eq!(outcome.content, "[exit code: 0]");
    }

    /// The timeout fires only once the background child has published its pid, so
    /// a slow shell start-up (CI) cannot make the precondition race the clock.
    #[tokio::test]
    async fn a_timeout_kills_the_whole_group() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let published = async move {
            while pid_file(&root, "pid").is_none() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        };

        let command = "sleep 30 & echo $! > pid; wait";
        let service = ProcessService::new(dir.path()).with_env_snapshot(vec![(
            OsString::from("PATH"),
            OsString::from("/usr/bin:/bin"),
        )]);
        let run = service
            .run_until(command, published, &CancellationToken::new())
            .await;
        let outcome = outcome(run, command, Duration::from_secs(1), false);

        assert_eq!(outcome.status, ToolStatus::Error);
        assert!(
            outcome.content.contains("[timed out after 1 s]"),
            "{outcome:?}"
        );
        let pid = pid_file(dir.path(), "pid").expect("the child wrote its pid");
        wait_for_gone(pid);
    }

    /// `timeout_seconds` is wired to a real clock. Nothing has to happen before it
    /// fires, so there is no start-up race here.
    #[tokio::test]
    async fn timeout_seconds_stops_a_long_command() {
        let dir = tempfile::tempdir().unwrap();
        let tool = tool(dir.path());

        let outcome = execute(&tool, r#"{"command": "sleep 30", "timeout_seconds": 1}"#).await;

        assert_eq!(outcome.status, ToolStatus::Error);
        assert!(
            outcome.content.contains("[timed out after 1 s]"),
            "{outcome:?}"
        );
    }

    #[tokio::test]
    async fn cancellation_kills_the_whole_group() {
        let dir = tempfile::tempdir().unwrap();
        let tool = tool(dir.path());
        let cancel = CancellationToken::new();
        let control = cancel.clone();
        let script = "sleep 300 & echo $! > pid1; sleep 300 & echo $! > pid2; wait";

        // Cancel only once both background sleeps are running, so the group
        // really has children to kill.
        let root = dir.path().to_path_buf();
        let stopper = async move {
            for _ in 0..500 {
                if pid_file(&root, "pid1").is_some() && pid_file(&root, "pid2").is_some() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            control.cancel();
        };
        let call = call(&serde_json::json!({ "command": script }).to_string());
        let context = ToolContext {
            cancel: cancel.clone(),
        };
        let (outcome, ()) = tokio::join!(tool.execute(&call, context), stopper);

        assert_eq!(outcome.status, ToolStatus::Cancelled);
        assert!(outcome.content.contains("[cancelled]"), "{outcome:?}");
        let first = pid_file(dir.path(), "pid1").expect("pid1 was written");
        let second = pid_file(dir.path(), "pid2").expect("pid2 was written");
        wait_for_gone(first);
        wait_for_gone(second);
    }

    #[tokio::test]
    async fn high_volume_output_is_bounded_while_collected() {
        let dir = tempfile::tempdir().unwrap();
        let tool = tool(dir.path());

        let outcome = execute(&tool, r#"{"command": "yes | head -c 5000000"}"#).await;

        assert_eq!(outcome.status, ToolStatus::Ok);
        assert!(
            outcome.content.len() < 60_000,
            "len={}",
            outcome.content.len()
        );
        assert!(outcome.content.contains("bytes omitted"), "{outcome:?}");
    }

    #[tokio::test]
    async fn a_signal_termination_is_reported() {
        let dir = tempfile::tempdir().unwrap();
        let tool = tool(dir.path());

        let outcome = execute(&tool, r#"{"command": "kill -TERM $$"}"#).await;

        assert_eq!(outcome.status, ToolStatus::Error);
        assert!(
            outcome.content.contains("[terminated by signal 15]"),
            "{outcome:?}"
        );
    }

    #[test]
    fn declaration_is_a_function_with_the_spec_schema() {
        let dir = tempfile::tempdir().unwrap();
        let tool = tool(dir.path());

        assert_eq!(tool.declaration().name, "shell");
        let schema = schema(&tool);
        assert_eq!(schema["type"], "object");
        assert_eq!(schema["required"], serde_json::json!(["command"]));
        assert_eq!(schema["additionalProperties"], false);
        assert_eq!(schema["properties"]["command"]["type"], "string");
        assert_eq!(schema["properties"]["timeout_seconds"]["minimum"], 1);
        assert_eq!(schema["properties"]["timeout_seconds"]["maximum"], 3600);
        assert_eq!(schema["properties"]["timeout_seconds"]["default"], 120);
        assert_eq!(schema["properties"]["raw"]["type"], "boolean");
        assert_eq!(schema["properties"]["raw"]["default"], false);
    }

    #[test]
    fn identity_defaults_to_claude_and_survives_a_face_change() {
        let dir = tempfile::tempdir().unwrap();
        let tool = tool(dir.path());
        assert_eq!(tool.identity().implementation, "p1-tool-shell");
        assert_eq!(tool.identity().variant, "claude");

        let reshaped = tool.with_face(ToolFace::new("Run", "custom"), "gpt");
        assert_eq!(reshaped.declaration().name, "Run");
        assert_eq!(reshaped.declaration().description, "custom");
        assert_eq!(reshaped.identity().implementation, "p1-tool-shell");
        assert_eq!(reshaped.identity().variant, "gpt");
    }

    #[test]
    fn effect_is_executes() {
        let dir = tempfile::tempdir().unwrap();
        let tool = tool(dir.path());
        assert_eq!(tool.effect(&call("{}")), Effect::Executes);
    }

    /// ADR-0057: the command's first line, trimmed to 80 characters.
    #[test]
    fn describe_names_the_command_first_line_bounded() {
        let dir = tempfile::tempdir().unwrap();
        let tool = tool(dir.path());
        let described = tool.describe(&call(r#"{"command": "echo one\necho two"}"#));
        assert_eq!(described.verb, "run");
        assert_eq!(described.target.as_deref(), Some("echo one"));

        let long = format!("echo {}", "x".repeat(200));
        let call = call(&serde_json::json!({ "command": long }).to_string());
        assert_eq!(tool.describe(&call).target.unwrap().chars().count(), 80);
    }

    #[test]
    fn describe_classifies_destructive_real_call_inputs() {
        let dir = tempfile::tempdir().unwrap();
        let tool = tool(dir.path());
        for command in [
            "rm -rf target/",
            "git push --force origin main",
            "git reset --hard",
            "git clean -f",
            "echo secret > /tmp/out",
            "cargo test | tee ../outside.log",
        ] {
            let input = serde_json::json!({ "command": command }).to_string();
            assert!(tool.describe(&call(&input)).destructive, "{command:?}");
        }
        for command in [
            "grep -r needle target/",
            "git push origin main",
            "echo 'rm -rf /'",
            "echo output > target/out",
        ] {
            let input = serde_json::json!({ "command": command }).to_string();
            assert!(!tool.describe(&call(&input)).destructive, "{command:?}");
        }
    }

    #[tokio::test]
    async fn describe_result_parses_a_real_shell_output() {
        let dir = tempfile::tempdir().unwrap();
        let tool = tool(dir.path());
        let call = call(r#"{"command":"printf 'one\\ntwo\\n'; exit 7"}"#);
        let outcome = tool
            .execute(
                &call,
                ToolContext {
                    cancel: CancellationToken::new(),
                },
            )
            .await;
        let result = ToolResultItem {
            call_id: call.call_id.clone(),
            name: call.name.clone(),
            status: outcome.status,
            content: outcome.content,
        };

        let described = tool.describe_result(&call, &result);

        assert_eq!(described.summary, "exit 7 · 2 lines");
        assert_eq!(
            described.detail,
            Some(ResultDetail::Command {
                exit_code: Some(7),
                elapsed_ms: None,
                tail: vec!["one".into(), "two".into()],
            })
        );
    }

    #[tokio::test]
    async fn invalid_input_reports_a_prefix_and_never_panics() {
        let dir = tempfile::tempdir().unwrap();
        let tool = tool(dir.path());
        let garbage = [
            "",
            "null",
            "[]",
            "{}",
            "{\"command\": 5}",
            "{\"command\":\"echo hi\",\"unknown\":1}",
            "{\"command\":\"echo hi\",\"timeout_seconds\":0}",
            "{\"command\":\"echo hi\",\"timeout_seconds\":3601}",
            "{\"command\":\"echo hi\",\"timeout_seconds\":-1}",
            "\u{0}\u{1}{\"command\" garbage",
        ];
        for arguments in garbage {
            let outcome = execute(&tool, arguments).await;
            assert_eq!(outcome.status, ToolStatus::Error, "input: {arguments:?}");
            assert!(
                outcome.content.starts_with("Invalid input for shell: "),
                "input: {arguments:?} -> {outcome:?}"
            );
        }
    }

    #[tokio::test]
    async fn text_input_is_invalid_input() {
        let dir = tempfile::tempdir().unwrap();
        let tool = tool(dir.path());
        let call = ToolCall {
            call_id: "call-1".into(),
            name: "shell".into(),
            input: ToolInput::Text("echo hi".into()),
        };
        let context = ToolContext {
            cancel: CancellationToken::new(),
        };

        let outcome = tool.execute(&call, context).await;

        assert_eq!(outcome.status, ToolStatus::Error);
        assert!(outcome.content.starts_with("Invalid input for shell: "));
    }

    #[tokio::test]
    async fn execute_returns_cancelled_without_starting_a_process() {
        let dir = tempfile::tempdir().unwrap();
        let tool = tool(dir.path());
        let call = call(r#"{"command": "touch started"}"#);
        let cancel = CancellationToken::new();
        cancel.cancel();

        let outcome = tool.execute(&call, ToolContext { cancel }).await;

        assert_eq!(outcome.status, ToolStatus::Cancelled);
        assert_eq!(outcome.content, "");
        assert!(!dir.path().join("started").exists());
    }

    #[test]
    fn parse_input_rejects_a_freeform_text_call() {
        let call = ToolCall {
            call_id: "c".into(),
            name: "shell".into(),
            input: ToolInput::Text("anything".into()),
        };
        assert!(parse_input("shell", &call).is_err());
    }
}
