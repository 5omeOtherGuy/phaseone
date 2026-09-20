//! The `shell` tool: one-shot `bash -lc <command>` execution.
//!
//! Runs from the workspace root with stdin closed and its own process group, so
//! a timeout or cancellation can terminate the whole group (SIGTERM, then
//! SIGKILL) instead of leaving backgrounded children behind. stdout and stderr
//! are captured interleaved in arrival order while the command runs, and the
//! captured bytes are bounded as they are collected so a flooding command cannot
//! exhaust memory. Output bounding and the workspace live in `p1-workspace`.

use std::collections::VecDeque;
use std::ffi::OsString;
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use nix::sys::signal::{Signal, killpg};
use nix::unistd::Pid;
use p1_contracts::{
    BoxFuture, CancellationToken, DeclarationKind, Effect, Tool, ToolCall, ToolContext,
    ToolDeclaration, ToolIdentity, ToolInput, ToolOutcome, ToolStatus,
};
use p1_workspace::{ToolFace, Workspace};
use serde::Deserialize;
use tokio::io::AsyncReadExt;
use tokio::process::{Child, Command};

const NAME: &str = "shell";
const DESCRIPTION: &str = "Run a shell command with `bash -lc` from the workspace root, with stdin closed.\nstdout and stderr are captured together; the last line reports the exit code. Non-zero exits are not tool errors.\nSet `timeout_seconds` for long commands; on timeout or cancellation the whole process group is killed.";
const DEFAULT_TIMEOUT_SECONDS: i64 = 120;
const MIN_TIMEOUT_SECONDS: i64 = 1;
const MAX_TIMEOUT_SECONDS: i64 = 3_600;
const MAX_OUTPUT_BYTES: usize = 50_000;
/// Bytes of the beginning of the output kept in memory.
const HEAD_BYTES: usize = 25_000;
/// Bytes of the end of the output kept in memory.
const TAIL_BYTES: usize = 25_000;
/// Lines kept of the head/tail. The collector also bounds by bytes; the line
/// bound keeps the rendered content inside `bound_output`'s line cap so the
/// omission notice is never what gets cut away.
const HEAD_LINES: usize = 990;
const TAIL_LINES: usize = 990;
/// How long the group is given to exit after SIGTERM before SIGKILL.
const SIGTERM_GRACE: Duration = Duration::from_secs(2);
/// How long to wait for a SIGKILLed group to disappear before giving up on it.
const SIGKILL_WAIT: Duration = Duration::from_secs(2);
const GROUP_POLL: Duration = Duration::from_millis(10);
const READ_BUFFER_BYTES: usize = 16 * 1024;

/// The paragraph the model sees when the host turned the sandbox on. Appended to
/// whatever face the environment gave the tool, so a `with_face` override keeps it.
const SANDBOX_PARAGRAPH: &str = "Commands run in a sandbox: only the workspace and /tmp are writable, the rest of the filesystem is read-only, and most of the home directory is not visible. Do not try to install software outside the workspace.";

/// Home entries the sandbox leaves visible (read-only) even though it hides the
/// rest of the home directory.
pub const DEFAULT_HOME_VISIBLE: &[&str] = &[
    ".cargo",
    ".rustup",
    ".local/bin",
    ".local/lib",
    ".nvm",
    ".gitconfig",
    ".config/git",
];

/// What the sandbox hides, keeps visible and keeps writable. The host chooses
/// this; [`ShellTool::sandboxed`] turns it into a `bwrap` invocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sandbox {
    /// The home directory to hide behind a `tmpfs`.
    pub home: PathBuf,
    /// Paths relative to `home` that stay visible (read-only) if they exist.
    pub home_visible: Vec<PathBuf>,
    /// Extra absolute paths that stay writable if they exist.
    pub writable: Vec<PathBuf>,
}

impl Sandbox {
    /// A sandbox that hides `home` except for [`DEFAULT_HOME_VISIBLE`].
    pub fn for_home(home: impl Into<PathBuf>) -> Self {
        Self {
            home: home.into(),
            home_visible: DEFAULT_HOME_VISIBLE.iter().map(PathBuf::from).collect(),
            writable: Vec::new(),
        }
    }
}

/// Why a sandbox cannot be used. Every message names the remedy: the caller has
/// to be able to act on it, and `--sandbox off` always works.
#[derive(Debug, thiserror::Error)]
pub enum SandboxError {
    #[error("bubblewrap (`bwrap`) is not installed: install bubblewrap, or pass --sandbox off")]
    NotInstalled,
    #[error(
        "bubblewrap cannot run here ({0}): enable unprivileged user namespaces, or pass --sandbox off"
    )]
    Unavailable(String),
    #[error(
        "the workspace root {} contains the home directory {}: choose a workspace outside the home, or pass --sandbox off",
        .workspace.display(),
        .home.display()
    )]
    WorkspaceContainsHome { workspace: PathBuf, home: PathBuf },
}

/// The live sandbox: its configuration plus the private `/tmp` the tool owns.
/// The `Arc` keeps the directory alive across `with_face` and any clone.
struct SandboxRuntime {
    sandbox: Sandbox,
    private_tmp: tempfile::TempDir,
}

/// The `shell` tool. Holds one agent's workspace and, when the host chose it,
/// the bubblewrap sandbox around every command.
pub struct ShellTool {
    workspace: Workspace,
    /// The face BEFORE the sandbox paragraph and the variant BEFORE the
    /// `+sandbox` suffix, so `with_face` and `sandboxed` compose in either order
    /// without stacking (requirement 4).
    face: ToolFace,
    variant: String,
    sandbox: Option<Arc<SandboxRuntime>>,
    declaration: ToolDeclaration,
    identity: ToolIdentity,
}

impl ShellTool {
    /// Build the tool with the default (`shell`, Claude-family) face.
    pub fn new(workspace: Workspace) -> Self {
        Self {
            workspace,
            face: default_face(),
            variant: "claude".to_string(),
            sandbox: None,
            declaration: declaration(default_face()),
            identity: identity("claude"),
        }
        .composed()
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
        let workspace = self.workspace.root().to_path_buf();
        let home = std::fs::canonicalize(&sandbox.home).unwrap_or_else(|_| sandbox.home.clone());
        // Hiding the home would hide the workspace with it.
        if home == workspace || home.starts_with(&workspace) {
            return Err(SandboxError::WorkspaceContainsHome { workspace, home });
        }
        let private_tmp = tempfile::Builder::new()
            .prefix("p1-shell-sandbox-")
            .tempdir()
            .map_err(|error| {
                SandboxError::Unavailable(format!("could not create a private /tmp: {error}"))
            })?;
        let args = bwrap_args(&sandbox, &workspace, private_tmp.path());
        let mut probe = std::process::Command::new("bwrap");
        probe
            .args(&args)
            .arg("true")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        match probe.output() {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Err(SandboxError::NotInstalled);
            }
            Err(error) => return Err(SandboxError::Unavailable(error.to_string())),
            Ok(output) if !output.status.success() => {
                return Err(SandboxError::Unavailable(
                    String::from_utf8_lossy(&output.stderr).trim().to_string(),
                ));
            }
            Ok(_) => {}
        }
        Ok(Self {
            sandbox: Some(Arc::new(SandboxRuntime {
                sandbox,
                private_tmp,
            })),
            ..self
        }
        .composed())
    }

    /// Recompute the declaration and identity from the face, the variant and
    /// whether a sandbox is on. Called by every constructor, so composing the
    /// sandbox and a face in either order never doubles the paragraph or suffix.
    fn composed(mut self) -> Self {
        let description = match &self.sandbox {
            Some(_) => format!("{}\n{SANDBOX_PARAGRAPH}", self.face.description),
            None => self.face.description.clone(),
        };
        let variant = match &self.sandbox {
            Some(_) => format!("{}+sandbox", self.variant),
            None => self.variant.clone(),
        };
        self.declaration = declaration(ToolFace::new(self.face.name.clone(), description));
        self.identity = identity(&variant);
        self
    }
}

/// The argument vector passed to `bwrap` before `bash -lc <command>`.
///
/// Pure, and the ORDER is part of the contract: a later mount covers an earlier
/// one, so the private `/tmp` is mounted before the home and the workspace (a
/// workspace may itself live under `/tmp` or under the home), the writable paths
/// are bound back before the home is made read-only, and the workspace bind comes
/// after the home's `tmpfs` but before `--remount-ro`. `bwrap` creates missing
/// mount points itself.
pub fn bwrap_args(sandbox: &Sandbox, workspace_root: &Path, private_tmp: &Path) -> Vec<OsString> {
    let home = &sandbox.home;
    let mut args: Vec<OsString> = Vec::new();
    // 1. The host filesystem, read-only, with fresh /dev and /proc.
    for arg in ["--ro-bind", "/", "/", "--dev", "/dev", "--proc", "/proc"] {
        args.push(arg.into());
    }
    // 2. A fresh, private /tmp; TMPDIR points every command at it.
    args.push("--bind".into());
    args.push(private_tmp.into());
    args.push("/tmp".into());
    for arg in ["--setenv", "TMPDIR", "/tmp"] {
        args.push(arg.into());
    }
    // 3. Hide the home behind a tmpfs, then put back only what stays visible.
    args.push("--tmpfs".into());
    args.push(home.into());
    for entry in &sandbox.home_visible {
        let path = home.join(entry);
        if path.exists() {
            push_ro_bind(&mut args, &path);
        }
    }
    // A visible `.cargo` must not leak a registry token.
    for name in ["credentials.toml", "credentials"] {
        let path = home.join(".cargo").join(name);
        if path.exists() {
            args.push("--ro-bind".into());
            args.push("/dev/null".into());
            args.push(path.into());
        }
    }
    // 4. Extra writable paths, if they exist.
    for writable in &sandbox.writable {
        if writable.exists() {
            args.push("--bind".into());
            args.push(writable.into());
            args.push(writable.into());
        }
    }
    // 5. The workspace, after the mounts that could cover it.
    args.push("--bind".into());
    args.push(workspace_root.into());
    args.push(workspace_root.into());
    // 6. Only now make the home read-only: writes fail loudly instead of
    //    vanishing into the tmpfs. Child mounts (the workspace) stay writable.
    args.push("--remount-ro".into());
    args.push(home.into());
    // 7. A pid namespace so a detached process still dies with the sandbox.
    for arg in ["--unshare-pid", "--die-with-parent", "--chdir"] {
        args.push(arg.into());
    }
    args.push(workspace_root.into());
    args
}

fn push_ro_bind(args: &mut Vec<OsString>, path: &Path) {
    args.push("--ro-bind".into());
    args.push(path.into());
    args.push(path.into());
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
            run(
                self.workspace.root(),
                &input.command,
                timeout,
                tokio::time::sleep(timeout),
                &context.cancel,
                self.sandbox.as_deref(),
            )
            .await
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

/// How the waiting loop ended.
enum End {
    /// Both output streams reached EOF; the shell may still be running.
    Closed,
    TimedOut,
    Cancelled,
}

/// `expiry` is the timeout as a future, so a test can fire it on an observed
/// condition instead of racing the shell's start-up against a wall clock;
/// `timeout` is only what the footer reports.
async fn run(
    root: &Path,
    command: &str,
    timeout: Duration,
    expiry: impl Future<Output = ()>,
    cancel: &CancellationToken,
    sandbox: Option<&SandboxRuntime>,
) -> ToolOutcome {
    let mut expiry = std::pin::pin!(expiry);
    // The sandboxed and unsandboxed paths differ only in the spawned program;
    // process group, stdin, capture, timeout, kill and footers are shared.
    let mut builder = match sandbox {
        Some(runtime) => {
            let mut bwrap = Command::new("bwrap");
            bwrap
                .args(bwrap_args(
                    &runtime.sandbox,
                    root,
                    runtime.private_tmp.path(),
                ))
                .arg("bash")
                .arg("-lc")
                .arg(command);
            bwrap
        }
        None => {
            let mut bash = Command::new("bash");
            bash.arg("-lc").arg(command);
            bash
        }
    };
    builder
        .current_dir(root)
        // No terminal and no input: a command that reads stdin sees EOF.
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0);
    let mut child = match builder.spawn() {
        Ok(child) => child,
        Err(error) => {
            let program = if sandbox.is_some() { "bwrap" } else { "bash" };
            return ToolOutcome::error(format!("failed to start {program}: {error}"));
        }
    };
    let pgid = child.id().map(|id| id as i32).unwrap_or(0);

    let Some(mut stdout) = child.stdout.take() else {
        terminate(&mut child, pgid).await;
        return ToolOutcome::error("failed to capture bash stdout");
    };
    let Some(mut stderr) = child.stderr.take() else {
        terminate(&mut child, pgid).await;
        return ToolOutcome::error("failed to capture bash stderr");
    };

    let mut capture = Capture::default();
    let mut out_buffer = [0u8; READ_BUFFER_BYTES];
    let mut err_buffer = [0u8; READ_BUFFER_BYTES];
    let mut out_open = true;
    let mut err_open = true;

    // Drain both pipes concurrently. Each ready half wakes the task, so chunks
    // are appended in arrival order. Cancellation and the timeout are checked
    // in the same select, so they interrupt a blocked read promptly.
    let end = loop {
        if !out_open && !err_open {
            break End::Closed;
        }
        // Unbiased so neither stream is starved; whichever pipe has data is
        // appended as it arrives. Cancellation and the timeout are polled in
        // the same round and fire on the next loop iteration.
        tokio::select! {
            _ = cancel.cancelled() => {
                terminate(&mut child, pgid).await;
                break End::Cancelled;
            }
            _ = &mut expiry => {
                terminate(&mut child, pgid).await;
                break End::TimedOut;
            }
            read = stdout.read(&mut out_buffer), if out_open => match read {
                Ok(0) | Err(_) => out_open = false,
                Ok(count) => capture.push(&out_buffer[..count]),
            },
            read = stderr.read(&mut err_buffer), if err_open => match read {
                Ok(0) | Err(_) => err_open = false,
                Ok(count) => capture.push(&err_buffer[..count]),
            },
        }
    };

    let timed_out_footer = format!("[timed out after {} s]", timeout.as_secs());
    match end {
        End::Cancelled => return render(capture, "[cancelled]", ToolStatus::Cancelled),
        End::TimedOut => return render(capture, &timed_out_footer, ToolStatus::Error),
        End::Closed => {}
    }

    // The pipes are done; the shell itself may still run (it closed its output)
    // or may have exited. Wait for it, still honouring cancel/timeout.
    let status = tokio::select! {
        biased;
        _ = cancel.cancelled() => {
            terminate(&mut child, pgid).await;
            return render(capture, "[cancelled]", ToolStatus::Cancelled);
        }
        _ = &mut expiry => {
            terminate(&mut child, pgid).await;
            return render(capture, &timed_out_footer, ToolStatus::Error);
        }
        status = child.wait() => status,
    };

    match status {
        Ok(status) => {
            if let Some(code) = status.code() {
                render(capture, &format!("[exit code: {code}]"), ToolStatus::Ok)
            } else if let Some(signal) = status.signal() {
                render(
                    capture,
                    &format!("[terminated by signal {signal}]"),
                    ToolStatus::Error,
                )
            } else {
                render(
                    capture,
                    "[terminated by an unknown signal]",
                    ToolStatus::Error,
                )
            }
        }
        Err(error) => ToolOutcome::error(format!("failed to wait for bash: {error}")),
    }
}

/// Terminate the child's whole process group and reap the child.
///
/// SIGTERM first so cooperative processes can exit. The shell's own exit says
/// nothing about its descendants — one that ignores SIGTERM outlives a shell that
/// honours it — so the GROUP is watched, not the child: whatever is left of it
/// after [`SIGTERM_GRACE`] is SIGKILLed, and the function returns only once the
/// group is empty (bounded by [`SIGKILL_WAIT`]). The child is always reaped.
async fn terminate(child: &mut Child, pgid: i32) {
    if pgid <= 0 {
        // No group to signal (the pid was already gone at spawn time).
        let _ = child.kill().await;
        return;
    }
    let group = Pid::from_raw(pgid);
    let _ = killpg(group, Signal::SIGTERM);
    let grace_end = tokio::time::Instant::now() + SIGTERM_GRACE;
    // Reap the shell first: an unreaped group leader keeps the group alive.
    let reaped = tokio::time::timeout_at(grace_end, child.wait())
        .await
        .is_ok();
    wait_for_empty_group(group, grace_end).await;
    if group_exists(group) {
        let _ = killpg(group, Signal::SIGKILL);
    }
    if !reaped {
        let _ = child.wait().await;
    }
    wait_for_empty_group(group, tokio::time::Instant::now() + SIGKILL_WAIT).await;
}

/// Signal 0 probes without signalling: only ESRCH means no process is left in the group.
fn group_exists(group: Pid) -> bool {
    !matches!(killpg(group, None), Err(nix::errno::Errno::ESRCH))
}

async fn wait_for_empty_group(group: Pid, until: tokio::time::Instant) {
    while group_exists(group) && tokio::time::Instant::now() < until {
        tokio::time::sleep(GROUP_POLL).await;
    }
}

/// Render captured output plus a footer as the model-visible content.
fn render(capture: Capture, footer: &str, status: ToolStatus) -> ToolOutcome {
    let bytes = capture.into_bytes();
    let text = String::from_utf8_lossy(&bytes);
    // The footer goes on its own line without an extra blank line after the
    // command's usual trailing newline.
    let body = text.trim_end_matches('\n');
    // Lossy decoding can TRIPLE the size of binary output (each bad byte becomes
    // U+FFFD), pushing already-capped bytes past the content bound. Squeeze the body
    // — head and tail kept, like the collector — and never bound the footer: the exit
    // code must survive however noisy the output was.
    let body = squeeze(body, MAX_OUTPUT_BYTES - FOOTER_RESERVE);
    let content = if body.is_empty() {
        footer.to_string()
    } else {
        format!("{body}\n{footer}")
    };
    ToolOutcome { status, content }
}

const FOOTER_RESERVE: usize = 2_000;

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

/// Keeps the first [`HEAD_BYTES`]/[`HEAD_LINES`] and the last
/// [`TAIL_BYTES`]/[`TAIL_LINES`] of the captured output, no matter how much the
/// command prints.
#[derive(Default)]
struct Capture {
    head: Vec<u8>,
    head_newlines: usize,
    tail: VecDeque<u8>,
    tail_newlines: usize,
    total: u64,
}

impl Capture {
    fn push(&mut self, chunk: &[u8]) {
        self.total += chunk.len() as u64;
        let mut rest = chunk;
        if self.head.len() < HEAD_BYTES && self.head_newlines < HEAD_LINES {
            let mut taken = 0;
            for &byte in rest {
                if self.head.len() >= HEAD_BYTES || self.head_newlines >= HEAD_LINES {
                    break;
                }
                if byte == b'\n' {
                    self.head_newlines += 1;
                }
                self.head.push(byte);
                taken += 1;
            }
            rest = &rest[taken..];
        }
        for &byte in rest {
            self.tail.push_back(byte);
            if byte == b'\n' {
                self.tail_newlines += 1;
            }
            while self.tail.len() > TAIL_BYTES || self.tail_newlines > TAIL_LINES {
                if !self.pop_tail_front() {
                    break;
                }
            }
        }
    }

    fn pop_tail_front(&mut self) -> bool {
        match self.tail.pop_front() {
            Some(b'\n') => {
                self.tail_newlines -= 1;
                true
            }
            Some(_) => true,
            None => false,
        }
    }

    fn dropped(&self) -> u64 {
        self.total - (self.head.len() + self.tail.len()) as u64
    }

    fn into_bytes(self) -> Vec<u8> {
        let dropped = self.dropped();
        let mut out = self.head;
        if dropped > 0 {
            out.extend_from_slice(format!("\n[… {dropped} bytes omitted …]\n").as_bytes());
        }
        out.extend(self.tail);
        out
    }
}

#[cfg(test)]
mod tests {
    use super::{ShellTool, parse_input, run};
    use nix::sys::signal::{Signal, kill};
    use nix::unistd::Pid;
    use p1_contracts::{
        CancellationToken, DeclarationKind, Effect, Tool, ToolCall, ToolContext, ToolInput,
        ToolOutcome, ToolStatus,
    };
    use p1_workspace::{ToolFace, Workspace};
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

        let outcome = run(
            dir.path(),
            "sleep 30 & echo $! > pid; wait",
            Duration::from_secs(1),
            published,
            &CancellationToken::new(),
            None,
        )
        .await;

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
