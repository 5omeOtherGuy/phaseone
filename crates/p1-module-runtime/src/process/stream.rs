//! The streaming form of a run: the same capture, timeout, cancellation and group
//! kill as [`ProcessService::run`](super::ProcessService::run), handed out event by
//! event so a caller (the `process` capability of a WebAssembly guest) can read the
//! output while the command runs.
//!
//! The stream is a state machine polled by [`ProcessStream::next`], with a separate
//! timeout watchdog independent of polling: every wait in `next` is cancellation-safe
//! (a pipe read, the child's wait, the timer, the token), and each step's result is recorded before `next` returns, so a
//! caller that drops a pending `next` loses nothing. Termination is the one step that
//! waits across several awaits; it is recorded as decided before it starts and simply
//! runs again (signals are idempotent, a reaped child reports its status again) when a
//! dropped `next` or `kill` left it unfinished.

use nix::sys::signal::Signal;
use nix::unistd::Pid;
use p1_contracts::CancellationToken;
use std::collections::VecDeque;
use std::os::unix::process::ExitStatusExt;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
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
/// Dropping it sends SIGKILL to the group synchronously; only reaping runs on the
/// Tokio runtime afterward. Call [`ProcessStream::kill`] for graceful termination.
pub struct ProcessStream {
    group: Group,
    stdout: ChildStdout,
    stderr: ChildStderr,
    out_open: bool,
    err_open: bool,
    capture: Capture,
    expiry: CancellationToken,
    watchdog: tokio::task::JoinHandle<()>,
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
        let expired = CancellationToken::new();
        let notify = expired.clone();
        let leader = Arc::new(tokio::sync::Mutex::new(Some(child)));
        // Whether the deadline found the process group still running. A group that
        // already finished keeps the exit it had; only a group still alive at the
        // deadline is a timeout.
        let alive_at_expiry = Arc::new(AtomicBool::new(false));
        let watched = leader.clone();
        let alive = alive_at_expiry.clone();
        let watchdog = tokio::spawn(async move {
            expiry.await;
            // Reap the leader here when it already exited: a zombie still answers
            // signal 0, so only the reaped status can tell a finished run from one
            // still running at its deadline. `try_wait` caches the status, so the
            // stream's own later wait observes that same exit.
            let locked = watched.try_lock();
            let leader_exited = match locked {
                Ok(mut leader) => match leader.as_mut() {
                    Some(child) => matches!(child.try_wait(), Ok(Some(_))),
                    None => true,
                },
                // The stream is awaiting the leader: it has not exited.
                Err(_) => false,
            };
            let group_alive = if leader_exited {
                pgid > 0 && super::group_exists(Pid::from_raw(pgid))
            } else {
                true
            };
            alive.store(group_alive, Ordering::SeqCst);
            notify.cancel();
            if pgid > 0 && group_alive {
                let group = Pid::from_raw(pgid);
                super::signal_group_if_present(group, Signal::SIGTERM);
                tokio::time::sleep(super::SIGTERM_GRACE).await;
                super::signal_group_if_present(group, Signal::SIGKILL);
            }
        });
        Self {
            group: Group {
                leader,
                pgid,
                settled: false,
                alive_at_expiry,
            },
            stdout,
            stderr,
            out_open: true,
            err_open: true,
            capture: Capture::default(),
            expiry: expired,
            watchdog,
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
            () = self.expiry.cancelled() => {
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
            status = self.group.wait() => {
                self.phase = Phase::Ending(process_end(status));
                return None;
            }
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
            () = self.expiry.cancelled() => {
                self.phase = Phase::Ending(ProcessEnd::TimedOut);
                return;
            }
            status = self.group.wait() => status,
        };
        self.phase = Phase::Ending(process_end(status));
    }

    async fn finish_ending(&mut self) {
        let Phase::Ending(end) = &self.phase else {
            return;
        };
        let end = end.clone();
        let leader_status = self.group.terminate().await;
        self.drain_after_termination().await;
        let end = if self.cancel.is_cancelled() {
            ProcessEnd::Cancelled
        } else if self.expiry.is_cancelled() {
            if self.group.alive_at_expiry.load(Ordering::SeqCst) {
                ProcessEnd::TimedOut
            } else {
                // The group had already finished when its deadline arrived: report
                // the exit it had, not a timeout for a command that completed.
                match leader_status {
                    Some(status) => process_end(status),
                    None => end,
                }
            }
        } else {
            end
        };
        self.ended(end);
    }

    /// Capture bytes already in the pipes after the leader and its group end.
    /// An unrelated writer holding a duplicated fd must not keep this call alive.
    async fn drain_after_termination(&mut self) {
        let until = tokio::time::Instant::now() + super::SIGKILL_WAIT;
        while self.out_open || self.err_open {
            let read = tokio::select! {
                result = self.stdout.read(&mut self.out_buffer), if self.out_open => (true, result),
                result = self.stderr.read(&mut self.err_buffer), if self.err_open => (false, result),
                () = tokio::time::sleep_until(until) => break,
            };
            let (stdout, count) = read;
            let Ok(count) = count else {
                if stdout {
                    self.out_open = false
                } else {
                    self.err_open = false
                }
                continue;
            };
            if count == 0 {
                if stdout {
                    self.out_open = false
                } else {
                    self.err_open = false
                }
                continue;
            }
            let bytes = if stdout {
                &self.out_buffer[..count]
            } else {
                &self.err_buffer[..count]
            };
            let taken = self.capture.push(bytes);
            if taken > 0 {
                self.pending
                    .push_back(StreamEvent::Output(bytes[..taken].to_vec()));
            }
        }
    }

    /// Queue what remains of the output and the exit.
    fn ended(&mut self, end: ProcessEnd) {
        let rest = self.capture.take_rest();
        if !rest.is_empty() {
            self.pending.push_back(StreamEvent::Output(rest));
        }
        self.pending.push_back(StreamEvent::Exited(end));
        self.phase = Phase::Ended;
        self.watchdog.abort();
    }
}

fn process_end(status: std::io::Result<std::process::ExitStatus>) -> ProcessEnd {
    match status {
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
    }
}

/// The leader's reaped status, once it has exited; `None` when no leader was left to
/// reap.
type LeaderStatus = Option<std::io::Result<std::process::ExitStatus>>;

/// The command's process group and its leader, the shell (or bwrap). Ended on drop
/// unless the run already settled it.
struct Group {
    /// The leader, shared with the watchdog so it can reap an already-exited leader
    /// and read that cached status here. The `Option` exists only for the handoff in
    /// `Drop`.
    leader: Arc<tokio::sync::Mutex<Option<Child>>>,
    pgid: i32,
    /// The whole group was terminated.
    settled: bool,
    /// The deadline found the group still running (set by the watchdog before it
    /// cancels `expiry`).
    alive_at_expiry: Arc<AtomicBool>,
}

impl Group {
    async fn wait(&mut self) -> std::io::Result<std::process::ExitStatus> {
        let mut leader = self.leader.lock().await;
        match leader.as_mut() {
            Some(leader) => leader.wait().await,
            None => Err(std::io::Error::other("the shell was already handed off")),
        }
    }

    /// Terminate the group and reap the leader, returning its status so a run whose
    /// deadline found the group already finished still reports the exit it had.
    /// `terminate` bounds every wait it starts, so this must NOT add a second,
    /// unbounded `Child::wait` for a leader it could not reap: that would let a
    /// leader stuck in uninterruptible kernel work hang timeout, cancellation and
    /// kill, defeating the bounded post-SIGKILL cleanup.
    async fn terminate(&mut self) -> LeaderStatus {
        let mut leader = self.leader.lock().await;
        let status = match leader.as_mut() {
            Some(leader) => terminate(leader, self.pgid).await,
            None => None,
        };
        self.settled = true;
        status
    }
}

impl Drop for ProcessStream {
    fn drop(&mut self) {
        self.watchdog.abort();
    }
}

impl Drop for Group {
    fn drop(&mut self) {
        if self.settled {
            return;
        }
        let pgid = self.pgid;
        let leader = self.leader.clone();
        // A destructor cannot await the grace period. Kill synchronously so no
        // process remains runnable when the owning call returns; only reap later.
        // The guard keeps a group already emptied by a reaped leader from taking an
        // unrelated group's SIGKILL.
        if pgid > 0 {
            super::signal_group_if_present(Pid::from_raw(pgid), Signal::SIGKILL);
        }
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(async move {
                let mut leader = leader.lock().await;
                if let Some(leader) = leader.as_mut() {
                    let _ = tokio::time::timeout(super::SIGKILL_WAIT, leader.wait()).await;
                }
            });
        }
    }
}

#[cfg(test)]
mod lifecycle_tests {
    use super::*;
    use crate::process::{ProcessRequest, ProcessService};
    use std::time::Duration;

    // A killed orphan can remain as a zombie until its parent reaps it. It must
    // never be runnable after the resource is dropped.
    fn runnable(pid: i32) -> bool {
        let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
            return false;
        };
        !matches!(
            stat.rsplit_once(") ")
                .and_then(|(_, rest)| rest.chars().next()),
            Some('Z' | 'X')
        )
    }

    #[tokio::test]
    async fn resource_drop_kills_group_before_returning() {
        let dir = tempfile::tempdir().unwrap();
        let service = ProcessService::new(dir.path());
        let mut stream = service
            .spawn(
                ProcessRequest {
                    command: "trap '' TERM; echo ready; exec sleep 30",
                    timeout: Duration::from_secs(60),
                },
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(matches!(stream.next().await, Some(StreamEvent::Output(_))));
        let pid = stream.group.pgid;
        drop(stream);
        // SIGKILL is synchronous; scheduling delivery may take a turn.
        tokio::time::timeout(Duration::from_secs(2), async {
            while runnable(pid) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("dropped group is still runnable");
    }

    #[tokio::test]
    async fn timeout_kills_group_without_polling_stream() {
        let dir = tempfile::tempdir().unwrap();
        let service = ProcessService::new(dir.path());
        let (fire, expiry) = tokio::sync::oneshot::channel::<()>();
        let mut stream = service
            .start(
                "echo ready; exec sleep 30",
                async move {
                    let _ = expiry.await;
                },
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(matches!(stream.next().await, Some(StreamEvent::Output(_))));
        let pid = stream.group.pgid;
        fire.send(()).unwrap();
        // Do not call `next` until the watchdog has terminated the group.
        tokio::time::timeout(Duration::from_secs(3), async {
            while runnable(pid) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("timeout must not depend on polling");
        assert_eq!(
            stream.next().await,
            Some(StreamEvent::Exited(ProcessEnd::TimedOut))
        );
    }

    #[tokio::test]
    async fn inherited_pipes_do_not_delay_leader_cleanup_until_timeout() {
        let dir = tempfile::tempdir().unwrap();
        let service = ProcessService::new(dir.path());
        let outcome = service
            .run(
                ProcessRequest {
                    command: "sleep 30 & echo ready",
                    timeout: Duration::from_secs(10),
                },
                &CancellationToken::new(),
            )
            .await;
        assert_eq!(outcome.end, ProcessEnd::Exited(0));
        assert!(String::from_utf8_lossy(&outcome.output).contains("ready"));
    }

    #[tokio::test]
    async fn cleanup_past_timeout_cannot_report_success() {
        let dir = tempfile::tempdir().unwrap();
        let service = ProcessService::new(dir.path());
        let marker = dir.path().join("background-started");
        let command = format!(
            "trap '' TERM; sleep 30 </dev/null >/dev/null 2>&1 & echo ready; touch '{}'",
            marker.display()
        );
        // The deadline is injected on an observed condition — the marker exists only
        // once the shell has launched the TERM-ignoring background process — never a
        // wall clock.
        let outcome = service
            .run_until(
                &command,
                async move {
                    while !marker.exists() {
                        tokio::time::sleep(Duration::from_millis(5)).await;
                    }
                },
                &CancellationToken::new(),
            )
            .await;
        assert_eq!(outcome.end, ProcessEnd::TimedOut);
    }

    /// A command that finished before its deadline keeps its exit code even when the
    /// guest does not poll `next` until after the deadline: the watchdog must not turn
    /// an already-exited leader into `TimedOut`.
    #[tokio::test]
    async fn a_completed_command_keeps_its_exit_after_the_deadline() {
        let dir = tempfile::tempdir().unwrap();
        let service = ProcessService::new(dir.path());
        let (fire, expiry) = tokio::sync::oneshot::channel::<()>();
        let mut stream = service
            .start(
                "exit 7",
                async move {
                    let _ = expiry.await;
                },
                CancellationToken::new(),
            )
            .await
            .unwrap();
        let pid = stream.group.pgid;
        // The leader exits (a zombie until it is reaped), then the deadline fires with
        // no poll in between: exactly the guest that pauses too long.
        tokio::time::timeout(Duration::from_secs(5), async {
            while runnable(pid) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the command never exited");
        fire.send(()).unwrap();
        // The watchdog records whether the group was alive before it cancels `expiry`.
        tokio::time::timeout(Duration::from_secs(5), async {
            while !stream.expiry.is_cancelled() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the deadline never fired");
        assert_eq!(
            stream.next().await,
            Some(StreamEvent::Exited(ProcessEnd::Exited(7)))
        );
        assert_eq!(stream.next().await, None);
    }

    /// PR #473 Codex P1: `Group::terminate` returns the status its bounded reap
    /// observed and never adds a second, unbounded `Child::wait` for a leader the
    /// deadline could not reap. A leader stuck in uninterruptible kernel work cannot
    /// be created unprivileged, so the second wait's BOUND is pinned where
    /// `terminate` applies it: past the deadline, a wait that never resolves yields
    /// no status instead of blocking.
    #[tokio::test]
    async fn a_leader_the_deadline_cannot_reap_yields_no_status_not_a_second_wait() {
        let dir = tempfile::tempdir().unwrap();
        let service = ProcessService::new(dir.path());
        let mut stream = service
            .spawn(
                ProcessRequest {
                    command: "echo ready; sleep 30",
                    timeout: Duration::from_secs(60),
                },
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(matches!(stream.next().await, Some(StreamEvent::Output(_))));
        let status = stream.group.terminate().await;
        assert!(matches!(status, Some(Ok(_))));
        assert!(stream.group.settled);
        let never = std::future::pending::<std::io::Result<std::process::ExitStatus>>();
        let reaped: Option<std::io::Result<std::process::ExitStatus>> = tokio::time::timeout(
            Duration::from_secs(5),
            super::super::bounded_reap(never, tokio::time::Instant::now()),
        )
        .await
        .expect("the reap must not outlive its deadline");
        assert!(reaped.is_none());
    }

    #[tokio::test]
    async fn successful_leader_exit_ends_background_group() {
        let dir = tempfile::tempdir().unwrap();
        let service = ProcessService::new(dir.path());
        let outcome = service
            .run(
                ProcessRequest {
                    command: "sleep 30 </dev/null >/dev/null 2>&1 & echo $!",
                    timeout: Duration::from_secs(10),
                },
                &CancellationToken::new(),
            )
            .await;
        assert_eq!(outcome.end, ProcessEnd::Exited(0));
        let pid: i32 = String::from_utf8(outcome.output)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        assert!(!runnable(pid), "background group survived normal exit");
    }
}
