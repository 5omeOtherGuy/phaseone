//! The native process service: everything that touches a real process.
//!
//! It is assembled once from the workspace root, an environment snapshot, the
//! env-pass names and optionally a [`Sandbox`]; after that a caller can only hand
//! it a command text and a timeout. Program, environment, working directory and
//! sandbox are fixed at assembly, so whatever drives the service (the shell tool
//! today, a WebAssembly guest later) cannot widen them.

mod sandbox;

/// The paragraph the model reads when the host turned the sandbox on (ADR-0035: the
/// description says what the boundary is). It belongs to the side that assembled the
/// sandbox: a tool running over this service cannot know whether it is sandboxed, so
/// whoever presents the tool appends this to the face's description.
pub const SANDBOX_PARAGRAPH: &str = "Commands run in a sandbox: only the workspace and /tmp are writable, the rest of the filesystem is read-only, and most of the home directory is not visible. Do not try to install software outside the workspace.";

/// Appended to a tool's identity variant when its commands run in the sandbox, for the
/// same reason as [`SANDBOX_PARAGRAPH`].
pub const SANDBOX_VARIANT_SUFFIX: &str = "+sandbox";

use std::collections::VecDeque;
use std::ffi::{OsStr, OsString};
use std::os::unix::process::ExitStatusExt;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use nix::sys::signal::{Signal, killpg};
use nix::unistd::Pid;
use p1_contracts::CancellationToken;
use tokio::io::AsyncReadExt;
use tokio::process::{Child, Command};

use sandbox::SandboxRuntime;
pub use sandbox::{
    CREDENTIAL_DIRECTORIES, DEFAULT_HOME_VISIBLE, Sandbox, SandboxError, bwrap_args,
};

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

/// Runs `bash -lc <command>` from the workspace root, directly or inside the
/// assembled bubblewrap sandbox, with stdin closed, in its own process group,
/// with an environment rebuilt from the snapshot's allow-list, and returns only
/// once the command's whole process group is gone.
pub struct ProcessService {
    root: PathBuf,
    /// The environment a command is rebuilt from. Injected so tests never touch
    /// the process environment; the default is the process environment at
    /// construction.
    env_snapshot: Vec<(OsString, OsString)>,
    /// Extra variable NAMES added on top of [`ENV_ALLOW`] and
    /// [`ENV_ALLOW_PREFIXES`].
    env_pass: Vec<String>,
    sandbox: Option<SandboxRuntime>,
}

/// One command to run: the ONLY thing a caller chooses per run.
///
/// There is deliberately no field for the program, the environment, the working
/// directory or the sandbox: those are fixed when the [`ProcessService`] is
/// assembled, so a request cannot switch an assembled sandbox off.
///
/// ```
/// use std::time::Duration;
/// let request = p1_tool_shell::ProcessRequest {
///     command: "echo hi",
///     timeout: Duration::from_secs(1),
/// };
/// # let _ = request;
/// ```
///
/// ```compile_fail,E0560
/// use std::time::Duration;
/// let request = p1_tool_shell::ProcessRequest {
///     command: "echo hi",
///     timeout: Duration::from_secs(1),
///     sandbox: None,
/// };
/// ```
#[derive(Debug, Clone, Copy)]
pub struct ProcessRequest<'a> {
    /// Run as `bash -lc <command>`.
    pub command: &'a str,
    /// After this long the whole process group is terminated.
    pub timeout: Duration,
}

/// What a run produced: the bounded capture (stdout and stderr interleaved in
/// arrival order, with the omission marker where bytes were dropped) and how the
/// run ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessOutcome {
    pub output: Vec<u8>,
    pub end: ProcessEnd,
}

/// How a run ended. Closed: the caller's formatting covers every case.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProcessEnd {
    Exited(i32),
    TerminatedBySignal(i32),
    /// Neither an exit code nor a signal number was reported.
    TerminatedByUnknownSignal,
    TimedOut,
    Cancelled,
    Failed(ProcessFailure),
}

/// Why no complete run could be observed. `program` is the name the failure
/// message has always named: `bwrap` or `bash` for a start, `bash` otherwise.
/// The message is the model-visible error text, so it is the service's to word:
/// the shell's guest behaviour passes it through unchanged.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ProcessFailure {
    #[error("failed to start {program}: {error}")]
    Start {
        program: &'static str,
        error: String,
    },
    #[error("failed to capture {program} {stream}")]
    Capture {
        program: &'static str,
        stream: &'static str,
    },
    #[error("failed to wait for {program}: {error}")]
    Wait {
        program: &'static str,
        error: String,
    },
}

/// How the waiting loop ended.
enum End {
    /// Both output streams reached EOF; the shell may still be running.
    Closed,
    TimedOut,
    Cancelled,
}

impl ProcessService {
    /// A service running unsandboxed from `root`, rebuilding each command's
    /// environment from the process environment as it is now.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            env_snapshot: std::env::vars_os().collect(),
            env_pass: Vec::new(),
            sandbox: None,
        }
    }

    /// Replace the environment snapshot the command is rebuilt from.
    pub fn with_env_snapshot(mut self, snapshot: Vec<(OsString, OsString)>) -> Self {
        self.env_snapshot = snapshot;
        self
    }

    /// Add variable NAMES to the allow-list, on top of [`ENV_ALLOW`] and
    /// [`ENV_ALLOW_PREFIXES`].
    pub fn with_env_pass(mut self, names: Vec<String>) -> Self {
        self.env_pass.extend(names);
        self
    }

    /// Put every command in a bubblewrap sandbox, probed once here.
    pub fn sandboxed(self, sandbox: Sandbox) -> Result<Self, SandboxError> {
        let runtime = SandboxRuntime::assemble(sandbox, &self.root)?;
        Ok(Self {
            sandbox: Some(runtime),
            ..self
        })
    }

    pub fn is_sandboxed(&self) -> bool {
        self.sandbox.is_some()
    }

    /// Run one command until it exits, times out after `request.timeout`, or
    /// `cancel` fires.
    pub async fn run(
        &self,
        request: ProcessRequest<'_>,
        cancel: &CancellationToken,
    ) -> ProcessOutcome {
        self.run_until(request.command, tokio::time::sleep(request.timeout), cancel)
            .await
    }

    /// `expiry` is the timeout as a future, so a test can fire it on an observed
    /// condition instead of racing the shell's start-up against a wall clock.
    pub(crate) async fn run_until(
        &self,
        command: &str,
        expiry: impl Future<Output = ()>,
        cancel: &CancellationToken,
    ) -> ProcessOutcome {
        let mut expiry = std::pin::pin!(expiry);
        let root = self.root.as_path();
        // The sandboxed and unsandboxed paths differ only in the spawned program;
        // process group, stdin, capture, timeout and kill are shared.
        let mut builder = match &self.sandbox {
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
            .envs(self.allowed_env())
            .current_dir(root)
            // No terminal and no input: a command that reads stdin sees EOF.
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0);
        let mut child = match builder.spawn() {
            Ok(child) => child,
            Err(error) => {
                let program = if self.sandbox.is_some() {
                    "bwrap"
                } else {
                    "bash"
                };
                return failed(ProcessFailure::Start {
                    program,
                    error: error.to_string(),
                });
            }
        };
        let pgid = child.id().map(|id| id as i32).unwrap_or(0);

        let Some(mut stdout) = child.stdout.take() else {
            terminate(&mut child, pgid).await;
            return failed(ProcessFailure::Capture {
                program: "bash",
                stream: "stdout",
            });
        };
        let Some(mut stderr) = child.stderr.take() else {
            terminate(&mut child, pgid).await;
            return failed(ProcessFailure::Capture {
                program: "bash",
                stream: "stderr",
            });
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

        match end {
            End::Cancelled => return capture.ended(ProcessEnd::Cancelled),
            End::TimedOut => return capture.ended(ProcessEnd::TimedOut),
            End::Closed => {}
        }

        // The pipes are done; the shell itself may still run (it closed its output)
        // or may have exited. Wait for it, still honouring cancel/timeout.
        let status = tokio::select! {
            biased;
            _ = cancel.cancelled() => {
                terminate(&mut child, pgid).await;
                return capture.ended(ProcessEnd::Cancelled);
            }
            _ = &mut expiry => {
                terminate(&mut child, pgid).await;
                return capture.ended(ProcessEnd::TimedOut);
            }
            status = child.wait() => status,
        };

        let end = match status {
            Ok(status) => {
                if let Some(code) = status.code() {
                    ProcessEnd::Exited(code)
                } else if let Some(signal) = status.signal() {
                    ProcessEnd::TerminatedBySignal(signal)
                } else {
                    ProcessEnd::TerminatedByUnknownSignal
                }
            }
            Err(error) => ProcessEnd::Failed(ProcessFailure::Wait {
                program: "bash",
                error: error.to_string(),
            }),
        };
        capture.ended(end)
    }

    /// The child environment: the snapshot filtered by [`ENV_ALLOW`],
    /// [`ENV_ALLOW_PREFIXES`] and the names added with
    /// [`ProcessService::with_env_pass`]. A name the snapshot does not hold is
    /// simply absent; nothing is invented for it.
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
}

fn failed(failure: ProcessFailure) -> ProcessOutcome {
    ProcessOutcome {
        output: Vec::new(),
        end: ProcessEnd::Failed(failure),
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

    fn ended(self, end: ProcessEnd) -> ProcessOutcome {
        ProcessOutcome {
            output: self.into_bytes(),
            end,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{ENV_ALLOW, ENV_ALLOW_PREFIXES, ProcessService};
    use std::ffi::OsString;

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
        let service = ProcessService::new(dir.path())
            .with_env_snapshot(vec![(OsString::from("LC_ALL"), OsString::from("C"))]);

        assert_eq!(
            service.allowed_env(),
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
        let service = ProcessService::new(dir.path())
            .with_env_snapshot(snapshot)
            .with_env_pass(vec!["MY_TOOL_HOME".to_string()]);

        let names: Vec<String> = service
            .allowed_env()
            .iter()
            .map(|(name, _)| name.to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, ["PATH", "LC_MESSAGES", "MY_TOOL_HOME"]);
    }
}
