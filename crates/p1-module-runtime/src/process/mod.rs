//! The native process service: everything that touches a real process.
//!
//! It is assembled once from the workspace root, an environment snapshot, the
//! env-pass names and optionally a [`Sandbox`]; after that a caller can only hand
//! it a command text and a timeout. Program, environment, working directory and
//! sandbox are fixed at assembly, so whatever drives the service (the native shell
//! tool, or a WebAssembly guest through [`ProcessCapability`]) cannot widen them.
//! A run is available whole ([`ProcessService::run`]) or as a stream of events
//! ([`ProcessService::spawn`]); the first is the second drained.
//!
//! It lives with the runtime because the host serves the `process` capability with it;
//! the native `shell` tool (`p1-tool-shell`) re-exports it. The sandbox's presentation
//! (paragraph and variant suffix) is the shell's contract, in `p1-shell-guest`.

mod capability;
mod sandbox;
mod stream;

use std::collections::VecDeque;
use std::ffi::{OsStr, OsString};
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use nix::sys::signal::{Signal, killpg};
use nix::unistd::Pid;
use p1_contracts::CancellationToken;
use tokio::process::{Child, Command};

pub use capability::{ExitRecords, ProcessCapability};
use sandbox::SandboxRuntime;
pub use sandbox::{
    CREDENTIAL_DIRECTORIES, DEFAULT_HOME_VISIBLE, Sandbox, SandboxError, bwrap_args,
};
pub use stream::{ProcessStream, StreamEvent};

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
/// let request = p1_module_runtime::process::ProcessRequest {
///     command: "echo hi",
///     timeout: Duration::from_secs(1),
/// };
/// # let _ = request;
/// ```
///
/// ```compile_fail,E0560
/// use std::time::Duration;
/// let request = p1_module_runtime::process::ProcessRequest {
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

    /// Start one command and hand back its output as a stream of events (see
    /// [`ProcessStream`]). `Err` means nothing runs: the start failed, or its output
    /// could not be captured and what did start was terminated.
    pub async fn spawn(
        &self,
        request: ProcessRequest<'_>,
        cancel: CancellationToken,
    ) -> Result<ProcessStream, ProcessFailure> {
        self.start(request.command, tokio::time::sleep(request.timeout), cancel)
            .await
    }

    /// `expiry` is the timeout as a future, so a test can fire it on an observed
    /// condition instead of racing the shell's start-up against a wall clock. Public
    /// for that test, which formats the run with the shell tool's own footer.
    ///
    /// A run is its stream drained: there is one capture implementation, so what a
    /// streaming caller receives is byte for byte what `run` returns.
    pub async fn run_until(
        &self,
        command: &str,
        expiry: impl Future<Output = ()> + Send + 'static,
        cancel: &CancellationToken,
    ) -> ProcessOutcome {
        let mut stream = match self.start(command, expiry, cancel.clone()).await {
            Ok(stream) => stream,
            Err(failure) => return failed(failure),
        };
        let mut output = Vec::new();
        while let Some(event) = stream.next().await {
            match event {
                StreamEvent::Output(bytes) => output.extend_from_slice(&bytes),
                StreamEvent::Exited(end) => return ProcessOutcome { output, end },
            }
        }
        // Unreachable: a stream yields its exit before it ends. Should that ever
        // break, no exit was observed, which is what this end says.
        ProcessOutcome {
            output,
            end: ProcessEnd::TerminatedByUnknownSignal,
        }
    }

    async fn start(
        &self,
        command: &str,
        expiry: impl Future<Output = ()> + Send + 'static,
        cancel: CancellationToken,
    ) -> Result<ProcessStream, ProcessFailure> {
        let root = self.root.as_path();
        // The sandboxed and unsandboxed paths differ only in the spawned program;
        // process group, stdin, capture, timeout and kill are shared.
        let mut builder = match &self.sandbox {
            Some(runtime) => {
                runtime
                    .validate_launcher(root)
                    .map_err(|error| ProcessFailure::Start {
                        program: "bwrap",
                        error: error.to_string(),
                    })?;
                let mut bwrap = Command::new(&runtime.bwrap_path);
                bwrap
                    .args(
                        bwrap_args(&runtime.sandbox, root, runtime.private_tmp.path()).map_err(
                            |error| ProcessFailure::Start {
                                program: "bwrap",
                                error: error.to_string(),
                            },
                        )?,
                    )
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
                return Err(ProcessFailure::Start {
                    program,
                    error: error.to_string(),
                });
            }
        };
        let pgid = child.id().map(|id| id as i32).unwrap_or(0);

        let Some(stdout) = child.stdout.take() else {
            let _ = terminate(&mut child, pgid).await;
            return Err(ProcessFailure::Capture {
                program: "bash",
                stream: "stdout",
            });
        };
        let Some(stderr) = child.stderr.take() else {
            let _ = terminate(&mut child, pgid).await;
            return Err(ProcessFailure::Capture {
                program: "bash",
                stream: "stderr",
            });
        };
        Ok(ProcessStream::new(
            child,
            pgid,
            stdout,
            stderr,
            Box::pin(expiry),
            cancel,
        ))
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

/// The leader's state observed WITHOUT reaping it, so its process-group ID stays
/// reserved while the group is signalled.
#[allow(dead_code)]
enum LeaderExit {
    /// Still running.
    Running,
    /// Exited; its zombie is left in place and still reserves the group ID.
    Exited(std::process::ExitStatus),
    /// Already reaped, so this run no longer reserves the group ID.
    Reaped,
    /// The platform cannot observe without reaping.
    Unknown,
}

/// Observe the leader without reaping it where the platform allows. `WNOWAIT`
/// leaves the zombie, which keeps the group ID reserved for signalling; macOS has
/// no `waitid` in `nix`, so it reports [`LeaderExit::Unknown`] and the caller keeps
/// the reap-then-probe behaviour.
#[cfg(any(
    target_os = "android",
    target_os = "freebsd",
    target_os = "haiku",
    all(target_os = "linux", not(target_env = "uclibc")),
))]
fn observe_leader(pid: i32) -> LeaderExit {
    use nix::errno::Errno;
    use nix::sys::wait::{Id, WaitPidFlag, WaitStatus, waitid};
    use std::os::unix::process::ExitStatusExt;
    let flags = WaitPidFlag::WEXITED | WaitPidFlag::WNOHANG | WaitPidFlag::WNOWAIT;
    match waitid(Id::Pid(Pid::from_raw(pid)), flags) {
        Ok(WaitStatus::StillAlive) => LeaderExit::Running,
        Ok(WaitStatus::Exited(_, code)) => {
            LeaderExit::Exited(std::process::ExitStatus::from_raw(code << 8))
        }
        Ok(WaitStatus::Signaled(_, signal, core)) => {
            let raw = signal as i32 | if core { 0x80 } else { 0 };
            LeaderExit::Exited(std::process::ExitStatus::from_raw(raw))
        }
        Ok(_) => LeaderExit::Running,
        Err(Errno::ECHILD) => LeaderExit::Reaped,
        Err(_) => LeaderExit::Unknown,
    }
}

#[cfg(not(any(
    target_os = "android",
    target_os = "freebsd",
    target_os = "haiku",
    all(target_os = "linux", not(target_env = "uclibc")),
)))]
fn observe_leader(_pid: i32) -> LeaderExit {
    LeaderExit::Unknown
}

/// Terminate the child's whole process group and reap the child, returning the
/// status it observed within its deadlines.
///
/// SIGTERM first so cooperative processes can exit. The shell's own exit says
/// nothing about its descendants — one that ignores SIGTERM outlives a shell that
/// honours it — so the GROUP is watched, not the child: whatever is left of it
/// after [`SIGTERM_GRACE`] is SIGKILLed, and the function returns only once the
/// group is empty (bounded by [`SIGKILL_WAIT`]). The child is always reaped, but
/// every wait is bounded: a leader stuck in uninterruptible kernel work cannot
/// exit and must not block the caller forever. `None` means the leader could not
/// be reaped within the deadlines (or there was no group to signal), and the
/// caller must not wait for it again without a bound.
///
/// While the leader is still running it is deliberately NOT reaped before the
/// final signal: an unreaped leader (live, or a zombie left by `WNOWAIT`) reserves
/// this run's group ID, so SIGTERM and SIGKILL cannot land on an unrelated group
/// whose ID was reused. The signal-zero probe is used only when the leader was
/// already reaped and the group is therefore pinned by descendants alone.
async fn terminate(
    child: &mut Child,
    pgid: i32,
) -> Option<std::io::Result<std::process::ExitStatus>> {
    if pgid <= 0 {
        // No group to signal (the pid was already gone at spawn time). `kill`
        // awaits the child, so signal without waiting and reap under the deadline.
        let _ = child.start_kill();
        return bounded_reap(child.wait(), tokio::time::Instant::now() + SIGKILL_WAIT).await;
    }
    let group = Pid::from_raw(pgid);
    let grace_end = tokio::time::Instant::now() + SIGTERM_GRACE;
    if matches!(observe_leader(pgid), LeaderExit::Running) {
        // The leader keeps `pgid` reserved across the grace, whether it stays
        // running or becomes a zombie, so both signals below reach THIS run's group
        // and cannot reach a reused one.
        let _ = killpg(group, Signal::SIGTERM);
        while matches!(observe_leader(pgid), LeaderExit::Running)
            && tokio::time::Instant::now() < grace_end
        {
            tokio::time::sleep(GROUP_POLL).await;
        }
        let _ = killpg(group, Signal::SIGKILL);
    } else {
        // The leader was already reaped (or the platform cannot observe without
        // reaping), so the group is pinned by descendants alone: reap a zombie so
        // `group_exists` sees real membership, and probe before each signal.
        let _ = child.try_wait();
        signal_group_if_present(group, Signal::SIGTERM);
        wait_for_empty_group(group, grace_end).await;
        signal_group_if_present(group, Signal::SIGKILL);
    }
    let kill_end = tokio::time::Instant::now() + SIGKILL_WAIT;
    let status = bounded_reap(child.wait(), kill_end).await;
    wait_for_empty_group(group, kill_end).await;
    status
}

/// Signal `group` only while it still has a member. Reaping the leader releases its
/// group ID for reuse, so an emptied group's ID may already belong to an unrelated
/// process group; it must not receive this run's SIGTERM/SIGKILL. Returns whether a
/// signal was sent.
fn signal_group_if_present(group: Pid, signal: Signal) -> bool {
    if !group_exists(group) {
        return false;
    }
    let _ = killpg(group, signal);
    true
}

/// Reaping cannot extend the post-SIGKILL deadline even if the leader cannot exit.
async fn bounded_reap<F: Future>(wait: F, deadline: tokio::time::Instant) -> Option<F::Output> {
    tokio::time::timeout_at(deadline, wait).await.ok()
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
/// command prints. The head is final the moment it arrives, so it is handed on at
/// once and only counted here; the tail is known only at the end and is held.
#[derive(Default)]
struct Capture {
    head_len: usize,
    head_newlines: usize,
    tail: VecDeque<u8>,
    tail_newlines: usize,
    total: u64,
}

impl Capture {
    /// Take `chunk` in. Returns how many of its leading bytes belong to the head;
    /// the rest went to the tail.
    fn push(&mut self, chunk: &[u8]) -> usize {
        self.total += chunk.len() as u64;
        let mut taken = 0;
        for &byte in chunk {
            if self.head_len >= HEAD_BYTES || self.head_newlines >= HEAD_LINES {
                break;
            }
            if byte == b'\n' {
                self.head_newlines += 1;
            }
            self.head_len += 1;
            taken += 1;
        }
        for &byte in &chunk[taken..] {
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
        taken
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
        self.total - (self.head_len + self.tail.len()) as u64
    }

    /// What follows the head once the command is over: the omission marker where
    /// bytes were dropped, then the tail.
    fn take_rest(&mut self) -> Vec<u8> {
        let dropped = self.dropped();
        let mut out = Vec::new();
        if dropped > 0 {
            out.extend_from_slice(format!("\n[… {dropped} bytes omitted …]\n").as_bytes());
        }
        out.extend(std::mem::take(&mut self.tail));
        self.tail_newlines = 0;
        // Everything is accounted for now; a second call yields nothing.
        self.total = self.head_len as u64;
        out
    }
}

#[cfg(test)]
mod tests {
    use super::{ENV_ALLOW, ENV_ALLOW_PREFIXES, ProcessService};
    use std::ffi::OsString;

    /// The allow-list is exactly the spec's list; a later change has to update
    /// this test rather than widen the boundary silently.
    #[tokio::test]
    async fn post_sigkill_reap_cannot_outlive_deadline() {
        let deadline = tokio::time::Instant::now();
        let never = std::future::pending::<()>();
        assert!(super::bounded_reap(never, deadline).await.is_none());
    }

    /// A group whose leader was reaped and which has no member left must never be
    /// signalled: its ID is free and may already belong to an unrelated process group.
    #[tokio::test]
    async fn a_vanished_group_is_never_signalled() {
        let mut child = tokio::process::Command::new("true")
            .process_group(0)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let pgid = child.id().unwrap() as i32;
        let _ = child.wait().await.unwrap();
        assert!(
            !super::signal_group_if_present(
                nix::unistd::Pid::from_raw(pgid),
                nix::sys::signal::Signal::SIGTERM,
            ),
            "a reaped, empty group must not be signalled"
        );
    }

    /// The guard only skips a vanished group: a live one is still signalled.
    #[tokio::test]
    async fn a_live_group_is_still_signalled() {
        let mut child = tokio::process::Command::new("sleep")
            .arg("30")
            .process_group(0)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let pgid = child.id().unwrap() as i32;
        assert!(super::signal_group_if_present(
            nix::unistd::Pid::from_raw(pgid),
            nix::sys::signal::Signal::SIGTERM,
        ));
        let _ = child.wait().await.unwrap();
    }

    /// PR #473 Codex P1: the leader is observed WITHOUT reaping it, so its group ID
    /// stays reserved for the signal that follows. Reaping it frees the ID, which is
    /// exactly the state that let `killpg` reach a reused group.
    #[cfg(any(
        target_os = "android",
        target_os = "freebsd",
        target_os = "haiku",
        all(target_os = "linux", not(target_env = "uclibc")),
    ))]
    #[tokio::test]
    async fn observing_the_leader_leaves_its_group_id_reserved() {
        let mut child = tokio::process::Command::new("true")
            .process_group(0)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let pgid = child.id().unwrap() as i32;
        let group = nix::unistd::Pid::from_raw(pgid);
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            if matches!(super::observe_leader(pgid), super::LeaderExit::Exited(_)) {
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "the leader never exited"
            );
            tokio::time::sleep(super::GROUP_POLL).await;
        }
        assert!(
            super::group_exists(group),
            "an unreaped zombie must still reserve its group ID"
        );
        let _ = child.wait().await.unwrap();
        assert!(
            !super::group_exists(group),
            "reaping the last member must free the group ID"
        );
    }

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
