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

mod filter;

use std::collections::VecDeque;
use std::ffi::{OsStr, OsString};
use std::os::unix::process::ExitStatusExt;
use std::path::{Component, Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use nix::sys::signal::{Signal, killpg};
use nix::unistd::Pid;
use p1_contracts::{
    BoxFuture, CallDescription, CancellationToken, DeclarationKind, Effect, Tool, ToolCall,
    ToolContext, ToolDeclaration, ToolIdentity, ToolInput, ToolOutcome, ToolStatus,
};
use p1_workspace::{ToolFace, Workspace};
use serde::Deserialize;
use tokio::io::AsyncReadExt;
use tokio::process::{Child, Command};

const NAME: &str = "shell";
const DESCRIPTION: &str = "Run a shell command with `bash -lc` from the workspace root, with stdin closed.\nstdout and stderr are captured together; the last line reports the exit code. Non-zero exits are not tool errors.\nSet `timeout_seconds` for long commands; on timeout or cancellation the whole process group is killed.\nThe output of a recognised command (`cargo test`/`build`/`check`/`clippy`, `git status`/`log`/`diff`, `npm`/`pnpm` test) is summarised unless `raw: true` is passed.";
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

/// Variable names every command keeps from the snapshot. Everything else the p1
/// process holds (`API` keys, tokens, agent sockets) is dropped: the shell never
/// inherits p1's environment.
pub const ENV_ALLOW: &[&str] = &[
    "PATH",
    "HOME",
    "USER",
    "LOGNAME",
    "SHELL",
    "LANG",
    "LANGUAGE",
    "TERM",
    "TZ",
    "COLORTERM",
    "NO_COLOR",
    "CARGO_HOME",
    "RUSTUP_HOME",
    "RUSTUP_TOOLCHAIN",
    "RUSTFLAGS",
    "CARGO_TARGET_DIR",
    "CARGO_BUILD_JOBS",
    "P1_BUILD_LOCK_DIR",
    "P1_RUSTC_SLOTS",
    "VIRTUAL_ENV",
    "NVM_DIR",
    "JAVA_HOME",
    "GOPATH",
    "GOROOT",
];

/// Variable-name PREFIXES every command keeps from the snapshot.
pub const ENV_ALLOW_PREFIXES: &[&str] = &["LC_"];

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

/// Home-relative directories [`Sandbox::readable`] must NEVER expose: they hold
/// credentials or agent logins.
pub const CREDENTIAL_DIRECTORIES: &[&str] = &[
    ".ssh",
    ".claude",
    ".codex",
    ".gnupg",
    ".local/share/opencode",
    ".pi",
    ".config/gh",
    ".config/p1",
];

/// What the sandbox hides, keeps visible and keeps writable. The host chooses
/// this; [`ShellTool::sandboxed`] turns it into a `bwrap` invocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sandbox {
    /// The home directory to hide behind a `tmpfs` (canonical once sandboxed).
    pub home: PathBuf,
    /// Paths relative to `home` that stay visible (read-only) if they exist.
    pub home_visible: Vec<PathBuf>,
    /// Extra absolute paths that stay visible READ-ONLY if they exist. A git
    /// worktree keeps its metadata outside the workspace, in the main checkout's
    /// git directory, so a job there needs this to run `git status`/`git diff`.
    /// [`ShellTool::sandboxed`] refuses a path equal to, inside or containing a
    /// [`CREDENTIAL_DIRECTORIES`] entry of the home, and a path containing the
    /// home itself.
    pub readable: Vec<PathBuf>,
    /// Extra absolute paths that stay writable if they exist.
    pub writable: Vec<PathBuf>,
    /// A private runtime directory to replace (an empty `tmpfs`), when set and
    /// existing. The host fills it from `XDG_RUNTIME_DIR`; `for_home` leaves it
    /// `None`.
    pub runtime_dir: Option<PathBuf>,
}

impl Sandbox {
    /// A sandbox that hides `home` except for [`DEFAULT_HOME_VISIBLE`].
    pub fn for_home(home: impl Into<PathBuf>) -> Self {
        Self {
            home: home.into(),
            home_visible: DEFAULT_HOME_VISIBLE.iter().map(PathBuf::from).collect(),
            readable: Vec::new(),
            writable: Vec::new(),
            runtime_dir: None,
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
    #[error(
        "the sandbox readable path {} would uncover the credential directory {}: choose another path, or pass --sandbox off",
        .path.display(),
        .directory.display()
    )]
    ReadableCredential { path: PathBuf, directory: PathBuf },
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
    /// The environment a command is rebuilt from. Injected so tests never touch
    /// the process environment; the default is the process environment at
    /// construction.
    env_snapshot: Vec<(OsString, OsString)>,
    /// Extra variable NAMES the host added on top of [`ENV_ALLOW`] and
    /// [`ENV_ALLOW_PREFIXES`].
    env_pass: Vec<String>,
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
            env_snapshot: std::env::vars_os().collect(),
            env_pass: Vec::new(),
            declaration: declaration(default_face()),
            identity: identity("claude"),
        }
        .composed()
    }

    /// Replace the environment snapshot the command is rebuilt from. The default
    /// is the process environment at construction; tests inject a snapshot so
    /// they never mutate the process environment. Composes with `with_face` and
    /// `sandboxed` in any order.
    pub fn with_env_snapshot(mut self, snapshot: Vec<(OsString, OsString)>) -> Self {
        self.env_snapshot = snapshot;
        self
    }

    /// Add variable NAMES to the allow-list, on top of [`ENV_ALLOW`] and
    /// [`ENV_ALLOW_PREFIXES`]. Composes with `with_face` and `sandboxed` in any
    /// order.
    pub fn with_env_pass(mut self, names: Vec<String>) -> Self {
        self.env_pass.extend(names);
        self
    }

    /// The child environment: the snapshot filtered by [`ENV_ALLOW`],
    /// [`ENV_ALLOW_PREFIXES`] and the names added with
    /// [`ShellTool::with_env_pass`]. A name the snapshot does not hold is simply
    /// absent; nothing is invented for it.
    fn allowed_env(&self) -> Vec<(OsString, OsString)> {
        self.env_snapshot
            .iter()
            .filter(|(name, _)| self.allows(name))
            .cloned()
            .collect()
    }

    fn allows(&self, name: &OsStr) -> bool {
        // A non-UTF-8 name cannot match the (UTF-8) allow-list, so it is dropped.
        let Some(name) = name.to_str() else {
            return false;
        };
        ENV_ALLOW.contains(&name)
            || ENV_ALLOW_PREFIXES
                .iter()
                .any(|prefix| name.starts_with(prefix))
            || self.env_pass.iter().any(|passed| passed.as_str() == name)
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
        // One canonical home for BOTH the containment check and the mounts: a home
        // reached through a symlink must be hidden at the path bwrap is told about.
        let home = std::fs::canonicalize(&sandbox.home).unwrap_or_else(|_| sandbox.home.clone());
        if home == workspace || home.starts_with(&workspace) {
            return Err(SandboxError::WorkspaceContainsHome { workspace, home });
        }
        let sandbox = Sandbox { home, ..sandbox };
        // A readable path must never uncover a credential directory, whatever the
        // caller asks for. The check is here, before the probe, so it costs nothing
        // and fails assembly with a message naming the path.
        for readable in &sandbox.readable {
            if let Some(directory) = credential_directory(&sandbox.home, readable) {
                return Err(SandboxError::ReadableCredential {
                    path: readable.clone(),
                    directory,
                });
            }
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
/// workspace may itself live under `/tmp` or under the home), the read-only
/// `readable` paths are mounted BEFORE every writable bind, and the token masks
/// are emitted AFTER every writable bind so no writable directory can uncover
/// them. The workspace bind comes after the home's `tmpfs` but before
/// `--remount-ro`. `bwrap` creates missing mount points itself.
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
    // 3. Hide the home behind a tmpfs, then put back only what stays visible —
    //    the allow-list entries, then the caller's read-only `readable` paths
    //    (e.g. a git worktree's common directory) — and replace the runtime
    //    directory (agent sockets and keyrings). The readable binds come BEFORE
    //    the writable binds and the token masks below, so no readable path can
    //    uncover `~/.cargo/credentials*`. The runtime `tmpfs` comes last, so a
    //    readable path can never re-expose an agent socket. (`ShellTool::sandboxed`
    //    also refuses a readable path that would contain a credential directory.)
    args.push("--tmpfs".into());
    args.push(home.into());
    for entry in &sandbox.home_visible {
        let path = home.join(entry);
        if path.exists() {
            push_ro_bind(&mut args, &path);
        }
    }
    for readable in &sandbox.readable {
        if readable.exists() {
            push_ro_bind(&mut args, readable);
        }
    }
    if let Some(runtime_dir) = &sandbox.runtime_dir
        && runtime_dir.exists()
    {
        args.push("--tmpfs".into());
        args.push(runtime_dir.into());
    }
    // 4. Extra writable paths, if they exist; THEN the token masks, so a writable
    //    bind (e.g. `--sandbox-write ~/.cargo`) cannot uncover a credential file.
    for writable in &sandbox.writable {
        if writable.exists() {
            args.push("--bind".into());
            args.push(writable.into());
            args.push(writable.into());
        }
    }
    for name in ["credentials.toml", "credentials"] {
        let path = home.join(".cargo").join(name);
        if path.exists() {
            args.push("--ro-bind".into());
            args.push("/dev/null".into());
            args.push(path.into());
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

/// The credential directory of `home` that `readable` would uncover, if any.
///
/// A readable path uncovers a credential directory when it is equal to it, inside
/// it, OR an ancestor of it (including the home itself and `/`): any of the three
/// re-exposes the credentials. Literal and resolved forms are compared, so a
/// symlink cannot smuggle one into view. Existence never matters: a credential
/// directory may be created after the sandbox is assembled.
fn credential_directory(home: &Path, readable: &Path) -> Option<PathBuf> {
    let forms = readable_forms(readable);
    CREDENTIAL_DIRECTORIES.iter().find_map(|entry| {
        let literal = home.join(entry);
        let canonical = std::fs::canonicalize(&literal).unwrap_or_else(|_| literal.clone());
        forms
            .iter()
            .any(|form| {
                form.starts_with(&literal)
                    || form.starts_with(&canonical)
                    || literal.starts_with(form)
                    || canonical.starts_with(form)
            })
            .then_some(literal)
    })
}

/// Every filesystem location `readable` can denote: the path as given, its
/// canonical form when it exists, and the chain of symlink targets, each made
/// absolute and lexically normalised. Following the links by hand (rather than
/// only `canonicalize`, which needs the whole path to exist) catches a symlink
/// whose target is created later. Bounded depth: a symlink loop must not hang.
fn readable_forms(readable: &Path) -> Vec<PathBuf> {
    let mut forms = vec![readable.to_path_buf()];
    let mut current = readable.to_path_buf();
    for _ in 0..8 {
        if let Ok(canonical) = std::fs::canonicalize(&current)
            && !forms.contains(&canonical)
        {
            forms.push(canonical);
        }
        let Ok(metadata) = std::fs::symlink_metadata(&current) else {
            break;
        };
        if !metadata.file_type().is_symlink() {
            break;
        }
        let Ok(target) = std::fs::read_link(&current) else {
            break;
        };
        let next = lexical_normalize(&if target.is_absolute() {
            target
        } else {
            current.parent().unwrap_or(Path::new("/")).join(target)
        });
        if forms.contains(&next) {
            break;
        }
        forms.push(next.clone());
        current = next;
    }
    forms
}

/// Resolve `.` and `..` lexically, without touching the filesystem (a symlink
/// target may not exist, so `canonicalize` cannot be used).
fn lexical_normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
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
        CallDescription {
            verb: "run",
            target: parse_input(&self.declaration.name, call).ok().map(|input| {
                let first = input.command.lines().next().unwrap_or_default().trim();
                first.chars().take(80).collect()
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
            run(
                self.workspace.root(),
                &input.command,
                timeout,
                tokio::time::sleep(timeout),
                &context.cancel,
                Spawn {
                    sandbox: self.sandbox.as_deref(),
                    env: &self.allowed_env(),
                },
                input.raw,
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

/// How the command's process is spawned: the environment it is rebuilt from
/// and, when the host turned it on, the sandbox around it.
struct Spawn<'a> {
    sandbox: Option<&'a SandboxRuntime>,
    env: &'a [(OsString, OsString)],
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
    spawn: Spawn<'_>,
    raw: bool,
) -> ToolOutcome {
    let mut expiry = std::pin::pin!(expiry);
    // The sandboxed and unsandboxed paths differ only in the spawned program;
    // process group, stdin, capture, timeout, kill and footers are shared.
    let mut builder = match spawn.sandbox {
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
    // The command NEVER inherits p1's environment: the child's is cleared and
    // rebuilt from the snapshot's allow-list. For bwrap this is the bwrap
    // process's environment, which it passes on; its `--setenv TMPDIR /tmp` is
    // still applied inside, after the allow-list.
    builder
        .env_clear()
        .envs(spawn.env.iter().cloned())
        .current_dir(root)
        // No terminal and no input: a command that reads stdin sees EOF.
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0);
    let mut child = match builder.spawn() {
        Ok(child) => child,
        Err(error) => {
            let program = if spawn.sandbox.is_some() {
                "bwrap"
            } else {
                "bash"
            };
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
    // A cancelled or timed-out command is an incomplete run with no exit
    // status: its output is never summarised.
    match end {
        End::Cancelled => return render(capture, "[cancelled]", ToolStatus::Cancelled, None),
        End::TimedOut => return render(capture, &timed_out_footer, ToolStatus::Error, None),
        End::Closed => {}
    }

    // The pipes are done; the shell itself may still run (it closed its output)
    // or may have exited. Wait for it, still honouring cancel/timeout.
    let status = tokio::select! {
        biased;
        _ = cancel.cancelled() => {
            terminate(&mut child, pgid).await;
            return render(capture, "[cancelled]", ToolStatus::Cancelled, None);
        }
        _ = &mut expiry => {
            terminate(&mut child, pgid).await;
            return render(capture, &timed_out_footer, ToolStatus::Error, None);
        }
        status = child.wait() => status,
    };

    match status {
        Ok(status) => {
            if let Some(code) = status.code() {
                // The seam: the filter only ever sees a COMPLETED command, and
                // `exit_ok` is true only for exit code 0.
                let filter = Filter {
                    command,
                    raw,
                    exit_ok: code == 0,
                };
                render(
                    capture,
                    &format!("[exit code: {code}]"),
                    ToolStatus::Ok,
                    Some(filter),
                )
            } else if let Some(signal) = status.signal() {
                // Killed before it could exit: no output to summarise.
                render(
                    capture,
                    &format!("[terminated by signal {signal}]"),
                    ToolStatus::Error,
                    None,
                )
            } else {
                render(
                    capture,
                    "[terminated by an unknown signal]",
                    ToolStatus::Error,
                    None,
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

/// Render captured output plus a footer as the model-visible content.
fn render(
    capture: Capture,
    footer: &str,
    status: ToolStatus,
    filter: Option<Filter<'_>>,
) -> ToolOutcome {
    let bytes = capture.into_bytes();
    let text = String::from_utf8_lossy(&bytes);
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
    use super::{ENV_ALLOW, ENV_ALLOW_PREFIXES, ShellTool, Spawn, parse_input, run};
    use nix::sys::signal::{Signal, kill};
    use nix::unistd::Pid;
    use p1_contracts::{
        CancellationToken, DeclarationKind, Effect, Tool, ToolCall, ToolContext, ToolInput,
        ToolOutcome, ToolStatus,
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

        let outcome = run(
            dir.path(),
            "sleep 30 & echo $! > pid; wait",
            Duration::from_secs(1),
            published,
            &CancellationToken::new(),
            Spawn {
                sandbox: None,
                env: &[(OsString::from("PATH"), OsString::from("/usr/bin:/bin"))],
            },
            false,
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

    /// The allow-list is exactly the spec's list; a later change has to update
    /// this test rather than widen the boundary silently.
    #[test]
    fn env_allow_is_exactly_the_spec_list() {
        assert_eq!(
            ENV_ALLOW,
            &[
                "PATH",
                "HOME",
                "USER",
                "LOGNAME",
                "SHELL",
                "LANG",
                "LANGUAGE",
                "TERM",
                "TZ",
                "COLORTERM",
                "NO_COLOR",
                "CARGO_HOME",
                "RUSTUP_HOME",
                "RUSTUP_TOOLCHAIN",
                "RUSTFLAGS",
                "CARGO_TARGET_DIR",
                "CARGO_BUILD_JOBS",
                "P1_BUILD_LOCK_DIR",
                "P1_RUSTC_SLOTS",
                "VIRTUAL_ENV",
                "NVM_DIR",
                "JAVA_HOME",
                "GOPATH",
                "GOROOT",
            ]
        );
        assert_eq!(ENV_ALLOW_PREFIXES, &["LC_"]);
    }

    /// Requirement 4: a snapshot with no `PATH` passes nothing for it. The
    /// filter is the only place that decides, so it is asserted directly here;
    /// the integration test observes bash's own default instead.
    #[test]
    fn a_missing_path_is_not_invented() {
        let dir = tempfile::tempdir().unwrap();
        let tool = ShellTool::new(Workspace::new(dir.path()).unwrap())
            .with_env_snapshot(vec![(OsString::from("LC_ALL"), OsString::from("C"))]);

        assert_eq!(
            tool.allowed_env(),
            vec![(OsString::from("LC_ALL"), OsString::from("C"))]
        );
    }

    #[test]
    fn the_allow_list_keeps_names_prefixes_and_passed_names() {
        let dir = tempfile::tempdir().unwrap();
        let snapshot = vec![
            (OsString::from("PATH"), OsString::from("/bin")),
            (OsString::from("LC_MESSAGES"), OsString::from("C")),
            (OsString::from("CANARY_TOKEN"), OsString::from("secret-1")),
            (OsString::from("SSH_AUTH_SOCK"), OsString::from("/x")),
            (OsString::from("MY_TOOL_HOME"), OsString::from("/opt/t")),
        ];
        let tool = ShellTool::new(Workspace::new(dir.path()).unwrap())
            .with_env_snapshot(snapshot)
            .with_env_pass(vec!["MY_TOOL_HOME".to_string()]);

        let names: Vec<String> = tool
            .allowed_env()
            .iter()
            .map(|(name, _)| name.to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, ["PATH", "LC_MESSAGES", "MY_TOOL_HOME"]);
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
