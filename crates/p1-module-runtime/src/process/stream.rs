//! The streaming form of a run: the same capture, timeout, cancellation and group
//! kill as [`ProcessService::run`](super::ProcessService::run), handed out event by
//! event so a caller (the `process` capability of a WebAssembly guest) can read the
//! output while the command runs.
//!
//! The stream is a state machine polled by [`ProcessStream::next`], not a task of its
//! own: every wait in `next` is cancellation-safe (a pipe read, the child's wait, the
//! timer, the token), and each step's result is recorded before `next` returns, so a
//! caller that drops a pending `next` loses nothing. Termination is the one step that
//! waits across several awaits; it is recorded as decided before it starts and simply
//! runs again (signals are idempotent, a reaped child reports its status again) when a
//! dropped `next` or `kill` left it unfinished.

use std::collections::VecDeque;
use std::os::unix::process::ExitStatusExt;
use std::pin::Pin;

use nix::sys::signal::{Signal, killpg};
use nix::unistd::Pid;
use p1_contracts::CancellationToken;
use tokio::io::AsyncReadExt;
use tokio::process::{Child, ChildStderr, ChildStdout};

use super::{Capture, ProcessEnd, ProcessFailure, READ_BUFFER_BYTES, terminate};

/// One event of a running command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StreamEvent {
    /// Output bytes, stdout and stderr interleaved in arrival order. The head of the
    /// output arrives as it is printed; the omission marker and the tail follow at the
    /// end, so the events of a run concatenate to exactly [`ProcessOutcome::output`].
    ///
    /// [`ProcessOutcome::output`]: super::ProcessOutcome::output
    Output(Vec<u8>),
    /// How the command ended; the last event.
    Exited(ProcessEnd),
}

/// A started command: [`StreamEvent::Output`] events, then exactly one
/// [`StreamEvent::Exited`], then `None`.
///
/// Dropping it while the command runs ends the whole process group (SIGTERM, grace,
/// SIGKILL, reap) on the Tokio runtime it was dropped on, so no command outlives its
/// handle.
pub struct ProcessStream {
    group: Group,
    stdout: ChildStdout,
    stderr: ChildStderr,
    out_open: bool,
    err_open: bool,
    capture: Capture,
    expiry: Pin<Box<dyn Future<Output = ()> + Send>>,
    cancel: CancellationToken,
    phase: Phase,
    /// Events decided but not yet handed out.
    pending: VecDeque<StreamEvent>,
    out_buffer: Box<[u8]>,
    err_buffer: Box<[u8]>,
}

enum Phase {
    /// Reading the pipes.
    Reading,
    /// Both pipes reached EOF; the shell itself may still run.
    Waiting,
    /// The group is being terminated for this end (timeout or cancellation).
    Ending(ProcessEnd),
    /// The exit is queued in `pending`.
    Ended,
}

impl ProcessStream {
    pub(super) fn new(
        child: Child,
        pgid: i32,
        stdout: ChildStdout,
        stderr: ChildStderr,
        expiry: Pin<Box<dyn Future<Output = ()> + Send>>,
        cancel: CancellationToken,
    ) -> Self {
        Self {
            group: Group {
                leader: Some(child),
                pgid,
                settled: false,
            },
            stdout,
            stderr,
            out_open: true,
            err_open: true,
            capture: Capture::default(),
            expiry,
            cancel,
            phase: Phase::Reading,
            pending: VecDeque::new(),
            out_buffer: vec![0; READ_BUFFER_BYTES].into_boxed_slice(),
            err_buffer: vec![0; READ_BUFFER_BYTES].into_boxed_slice(),
        }
    }

    /// The next event. Cancellation-safe: dropping the future loses no event.
    pub async fn next(&mut self) -> Option<StreamEvent> {
        loop {
            if let Some(event) = self.pending.pop_front() {
                return Some(event);
            }
            match &self.phase {
                Phase::Ended => return None,
                Phase::Ending(_) => self.finish_ending().await,
                Phase::Reading => {
                    if !self.out_open && !self.err_open {
                        self.phase = Phase::Waiting;
                        continue;
                    }
                    if let Some(head) = self.read().await {
                        return Some(StreamEvent::Output(head));
                    }
                }
                Phase::Waiting => self.wait().await,
            }
        }
    }

    /// Kill the process group, with the same SIGTERM, grace, SIGKILL and reap as a
    /// cancelled run, and return once the group is gone. `next` then yields the output
    /// that remains and `Exited(Cancelled)`. A command that already ended is left as
    /// it ended.
    pub async fn kill(&mut self) {
        if matches!(self.phase, Phase::Reading | Phase::Waiting) {
            self.phase = Phase::Ending(ProcessEnd::Cancelled);
        }
        if matches!(self.phase, Phase::Ending(_)) {
            self.finish_ending().await;
        }
    }

    /// One round of reading: both pipes, the token and the timer, unbiased so neither
    /// stream is starved. Returns the head bytes of a chunk when there are any.
    async fn read(&mut self) -> Option<Vec<u8>> {
        let (from_stdout, count) = tokio::select! {
            () = self.cancel.cancelled() => {
                self.phase = Phase::Ending(ProcessEnd::Cancelled);
                return None;
            }
            () = &mut self.expiry => {
                self.phase = Phase::Ending(ProcessEnd::TimedOut);
                return None;
            }
            read = self.stdout.read(&mut self.out_buffer), if self.out_open => match read {
                Ok(0) | Err(_) => {
                    self.out_open = false;
                    return None;
                }
                Ok(count) => (true, count),
            },
            read = self.stderr.read(&mut self.err_buffer), if self.err_open => match read {
                Ok(0) | Err(_) => {
                    self.err_open = false;
                    return None;
                }
                Ok(count) => (false, count),
            },
        };
        let chunk = if from_stdout {
            &self.out_buffer[..count]
        } else {
            &self.err_buffer[..count]
        };
        let taken = self.capture.push(chunk);
        (taken > 0).then(|| chunk[..taken].to_vec())
    }

    /// The pipes are done; wait for the shell, still honouring cancel and timeout.
    async fn wait(&mut self) {
        let status = tokio::select! {
            biased;
            () = self.cancel.cancelled() => {
                self.phase = Phase::Ending(ProcessEnd::Cancelled);
                return;
            }
            () = &mut self.expiry => {
                self.phase = Phase::Ending(ProcessEnd::TimedOut);
                return;
            }
            status = self.group.wait() => status,
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
        // The shell was reaped (or cannot be waited for); what it left running without
        // holding the pipes is not this run's to end, as it never was.
        self.group.settled = true;
        self.ended(end);
    }

    async fn finish_ending(&mut self) {
        let Phase::Ending(end) = &self.phase else {
            return;
        };
        let end = end.clone();
        self.group.terminate().await;
        self.ended(end);
    }

    /// Queue what remains of the output and the exit.
    fn ended(&mut self, end: ProcessEnd) {
        let rest = self.capture.take_rest();
        if !rest.is_empty() {
            self.pending.push_back(StreamEvent::Output(rest));
        }
        self.pending.push_back(StreamEvent::Exited(end));
        self.phase = Phase::Ended;
    }
}

/// The command's process group and its leader, the shell (or bwrap). Ended on drop
/// unless the run already settled it.
struct Group {
    /// Always present; an `Option` only so `Drop` can move it into the task that
    /// terminates the group.
    leader: Option<Child>,
    pgid: i32,
    /// The leader was reaped by a completed wait or the group was terminated.
    settled: bool,
}

impl Group {
    async fn wait(&mut self) -> std::io::Result<std::process::ExitStatus> {
        match &mut self.leader {
            Some(leader) => leader.wait().await,
            None => Err(std::io::Error::other("the shell was already handed off")),
        }
    }

    async fn terminate(&mut self) {
        if let Some(leader) = &mut self.leader {
            terminate(leader, self.pgid).await;
        }
        self.settled = true;
    }
}

impl Drop for Group {
    fn drop(&mut self) {
        if self.settled {
            return;
        }
        let Some(mut leader) = self.leader.take() else {
            return;
        };
        let pgid = self.pgid;
        // Termination waits (the grace, the reaping), which a destructor cannot, so it
        // runs on the runtime the handle was dropped on. Outside any runtime only the
        // immediate SIGKILL of the whole group is possible; Tokio reaps the leader.
        match tokio::runtime::Handle::try_current() {
            Ok(runtime) => {
                runtime.spawn(async move { terminate(&mut leader, pgid).await });
            }
            Err(_) => {
                if pgid > 0 {
                    let _ = killpg(Pid::from_raw(pgid), Signal::SIGKILL);
                }
            }
        }
    }
}
