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
//! The tool is split along the WebAssembly boundary (ADR-0071). [`ProcessService`]
//! (`process`) is the native part and stays native: sandbox, spawn, environment
//! policy, bounded capture, timeout and group kill, all fixed at assembly so a
//! request carries only a command and a timeout. The guest behaviour — input
//! parsing, declaration, destructiveness, output filters and result formatting —
//! is `p1-shell-guest`, which the `p1/shell` component also runs. [`ShellTool`] is
//! the native adapter over the two: the same code the component ships, driven by
//! the native `Tool` contract, so the frozen tests of this crate prove it.
//! [`ProcessCapability`] links the same service to the component's `process`
//! import (the runtime's `ProcessService` and `RunningProcess` traits).

mod process;

use std::ffi::OsString;
use std::time::Duration;

use p1_contracts::tool::{ResultDescription, ResultDetail};
use p1_contracts::{
    BoxFuture, CallDescription, DeclarationKind, Effect, Tool, ToolCall, ToolContext,
    ToolDeclaration, ToolIdentity, ToolInput, ToolOutcome, ToolResultItem, ToolStatus,
};
use p1_shell_guest::{End, Outcome, RawInput, ShellInput, Status};
use p1_workspace::{ToolFace, Workspace};

pub use process::{
    CREDENTIAL_DIRECTORIES, DEFAULT_HOME_VISIBLE, ENV_ALLOW, ENV_ALLOW_PREFIXES, ProcessCapability,
    ProcessEnd, ProcessFailure, ProcessOutcome, ProcessRequest, ProcessService, ProcessStream,
    SANDBOX_PARAGRAPH, SANDBOX_VARIANT_SUFFIX, Sandbox, SandboxError, StreamEvent, bwrap_args,
};

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
            format!("{}{SANDBOX_VARIANT_SUFFIX}", self.variant)
        } else {
            self.variant.clone()
        };
        self.declaration = declaration(ToolFace::new(self.face.name.clone(), description));
        self.identity = identity(&variant);
        self
    }
}

fn default_face() -> ToolFace {
    ToolFace::new(p1_shell_guest::NAME, p1_shell_guest::DESCRIPTION)
}

fn declaration(face: ToolFace) -> ToolDeclaration {
    ToolDeclaration {
        name: face.name,
        description: face.description,
        kind: DeclarationKind::Function {
            input_schema: p1_shell_guest::input_schema(),
        },
    }
}

fn identity(variant: &str) -> ToolIdentity {
    ToolIdentity {
        implementation: env!("CARGO_PKG_NAME").to_string(),
        variant: variant.to_string(),
    }
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
        let summary = p1_shell_guest::describe(raw_input(call), Some(self.workspace.root()));
        CallDescription {
            verb: summary.verb,
            target: summary.target,
            edit: None,
            destructive: summary.destructive,
        }
    }

    fn describe_result(&self, _call: &ToolCall, result: &ToolResultItem) -> ResultDescription {
        let summary =
            p1_shell_guest::describe_result(&result.content, result.status == ToolStatus::Ok);
        ResultDescription {
            summary: summary.summary,
            detail: Some(ResultDetail::Command {
                exit_code: summary.exit_code,
                elapsed_ms: None,
                tail: summary.tail,
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
                return tool_outcome(Outcome::cancelled_before_start());
            }
            let input = match parse_input(&self.declaration.name, call) {
                Ok(input) => input,
                Err(message) => return ToolOutcome::error(message),
            };
            let timeout = Duration::from_secs(input.timeout_seconds());
            let request = ProcessRequest {
                command: &input.command,
                timeout,
            };
            let run = self.process.run(request, &context.cancel).await;
            outcome(run, &input.command, timeout, input.raw)
        })
    }
}

fn raw_input(call: &ToolCall) -> RawInput<'_> {
    match &call.input {
        ToolInput::Json(raw) => RawInput::Json(raw),
        ToolInput::Text(raw) => RawInput::Text(raw),
    }
}

fn parse_input(tool: &str, call: &ToolCall) -> Result<ShellInput, String> {
    p1_shell_guest::parse_input(tool, raw_input(call))
}

/// The model-visible result of a run. `timeout` is only what a timed-out footer reports.
fn outcome(run: ProcessOutcome, command: &str, timeout: Duration, raw: bool) -> ToolOutcome {
    let ProcessOutcome { output, end } = run;
    let end = match end {
        ProcessEnd::Exited(code) => End::Exited(code),
        ProcessEnd::Cancelled => End::Cancelled,
        ProcessEnd::TimedOut => End::TimedOut,
        ProcessEnd::TerminatedBySignal(signal) => End::TerminatedBySignal(signal),
        ProcessEnd::TerminatedByUnknownSignal => End::TerminatedByUnknownSignal,
        ProcessEnd::Failed(failure) => return ToolOutcome::error(failure.to_string()),
    };
    tool_outcome(p1_shell_guest::finished(
        &output,
        end,
        command,
        timeout.as_secs(),
        raw,
    ))
}

fn tool_outcome(outcome: Outcome) -> ToolOutcome {
    ToolOutcome {
        status: match outcome.status {
            Status::Ok => ToolStatus::Ok,
            Status::Error => ToolStatus::Error,
            Status::Cancelled => ToolStatus::Cancelled,
        },
        content: outcome.content,
    }
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
