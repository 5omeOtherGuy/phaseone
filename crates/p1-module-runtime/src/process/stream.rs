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
use tokio::io::AsyncReadExt;
use tokio::process::{Child, ChildStderr, ChildStdout};

use super::{Capture, ProcessEnd, ProcessFailure, READ_BUFFER_BYTES, terminate};
use crate::outputs::OutputRecorder;

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
    /// The output store's tee (ADR-0109): every chunk goes through it, whole, before the
    /// capture keeps only its head and tail.
    recorder: Option<OutputRecorder>,
    expiry: CancellationToken,
    watchdog: tokio::task::JoinHandle<()>,
    /// The watchdog's verdict on the deadline: `0` not decided, `1` the group was
    /// still running, `2` it had already finished. `finish_ending` waits for the
    /// verdict before it terminates, so a reap can never erase the deadline's state.
    expiry_decision: tokio::sync::watch::Receiver<u8>,
    cancel: CancellationToken,
    phase: Phase,
    /// Events decided but not yet handed out.
    pending: VecDeque<StreamEvent>,
    out_buffer: Box<[u8]>,
    err_buffer: Box<[u8]>,
    /// Fake reader work between drain reads, reproducing a loaded executor.
    #[cfg(test)]
    drain_read_delay: Option<std::time::Duration>,
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
        // The watchdog publishes its deadline verdict here. A pending `next()` holds
        // the leader lock while it awaits the child, so a `try_lock` probe could miss
        // a child that exited just before the deadline; `finish_ending` instead waits
        // for this verdict, taken only after the deadline cancelled the wait.
        let (decision, expiry_decision) = tokio::sync::watch::channel(0u8);
        let watched = leader.clone();
        let watchdog = tokio::spawn(async move {
            expiry.await;
            // Wake a pending `next()` first: it holds the leader lock while it awaits
            // the child, and cancelling the deadline makes it drop that wait and
            // release the lock. Only then is the leader observable here.
            notify.cancel();
            let mut leader = watched.lock().await;
            // Reap the leader when it already exited: a zombie still answers signal
            // 0, so only the reaped status can tell a finished run from one still
            // running at its deadline. `try_wait` caches the status, so the stream's
            // own later wait observes that same exit.
            let leader_exited = match leader.as_mut() {
                Some(child) => matches!(child.try_wait(), Ok(Some(_))),
                None => true,
            };
            let group_alive = if leader_exited {
                pgid > 0 && super::group_exists(Pid::from_raw(pgid))
            } else {
                true
            };
            drop(leader);
            let _ = decision.send(if group_alive { 1 } else { 2 });
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
                kill: Arc::new(std::sync::Mutex::new(Some(pgid))),
                #[cfg(test)]
                unreapable_leader: false,
            },
            stdout,
            stderr,
            out_open: true,
            err_open: true,
            capture: Capture::default(),
            recorder: None,
            expiry: expired,
            watchdog,
            expiry_decision,
            cancel,
            phase: Phase::Reading,
            pending: VecDeque::new(),
            out_buffer: vec![0; READ_BUFFER_BYTES].into_boxed_slice(),
            err_buffer: vec![0; READ_BUFFER_BYTES].into_boxed_slice(),
            #[cfg(test)]
            drain_read_delay: None,
        }
    }

    pub(crate) fn group_kill(&self) -> GroupKill {
        GroupKill(self.group.kill.clone())
    }

    /// Store every chunk of the output through `recorder` from now on. Set before the first
    /// `next`, it sees the whole output.
    pub(crate) fn record_into(&mut self, recorder: OutputRecorder) {
        self.recorder = Some(recorder);
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
                Ok(0) => {
                    self.out_open = false;
                    return None;
                }
                Err(_) => {
                    self.mark_capture_incomplete();
                    self.out_open = false;
                    return None;
                }
                Ok(count) => (true, count),
            },
            read = self.stderr.read(&mut self.err_buffer), if self.err_open => match read {
                Ok(0) => {
                    self.err_open = false;
                    return None;
                }
                Err(_) => {
                    self.mark_capture_incomplete();
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
        if let Some(recorder) = self.recorder.as_mut() {
            recorder.write(chunk);
        }
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
        // Wait for the watchdog's deadline verdict BEFORE terminating: terminating
        // first would reap the leader and empty the group, so a group that was still
        // running at the deadline would look like a command that had finished.
        let alive_at_expiry = if !self.cancel.is_cancelled() && self.expiry.is_cancelled() {
            Some(self.await_expiry_decision().await)
        } else {
            None
        };
        // Terminate the group while watching for the deadline to fire mid-cleanup.
        // A leader that exited normally does not let the run report its exit as a
        // success when the group outlived it past the deadline: the design contract
        // makes a cleanup that crosses the timeout an end of `TimedOut`, not the
        // leader's exit. `terminate` holds the leader lock the watchdog needs for its
        // own verdict, so this is the only place that can see the crossing.
        let mut deadline_crossed_cleanup = self.expiry.is_cancelled();
        let leader_status = {
            let terminate = self.group.terminate();
            tokio::pin!(terminate);
            loop {
                tokio::select! {
                    biased;
                    status = &mut terminate => break status,
                    () = self.expiry.cancelled(), if !deadline_crossed_cleanup => {
                        deadline_crossed_cleanup = true;
                    }
                }
            }
        };
        self.drain_after_termination().await;
        let end = if self.cancel.is_cancelled() || matches!(&end, ProcessEnd::Cancelled) {
            ProcessEnd::Cancelled
        } else if let Some(alive) = alive_at_expiry {
            if alive {
                ProcessEnd::TimedOut
            } else {
                // The group had already finished when its deadline arrived: report
                // the exit it had, not a timeout for a command that completed.
                match leader_status {
                    Some(status) => process_end(status),
                    None => end,
                }
            }
        } else if deadline_crossed_cleanup {
            ProcessEnd::TimedOut
        } else {
            end
        };
        self.ended(end);
    }

    /// Await the watchdog's deadline verdict: `true` when the group was still
    /// running. Only called once `expiry` is cancelled, so the watchdog has fired.
    async fn await_expiry_decision(&mut self) -> bool {
        let mut decision = self.expiry_decision.clone();
        loop {
            let value = *decision.borrow_and_update();
            if value != 0 {
                return value == 1;
            }
            if decision.changed().await.is_err() {
                // The watchdog ended without a verdict; a deadline is a timeout.
                return true;
            }
        }
    }

    /// Capture bytes already in the pipes after the leader and its group end.
    /// An unrelated writer holding a duplicated fd must not keep this call alive.
    async fn drain_after_termination(&mut self) {
        let until = tokio::time::Instant::now() + super::SIGKILL_WAIT;
        while self.out_open || self.err_open {
            // Reader work and ready pipes cannot extend the bounded drain.
            if tokio::time::Instant::now() >= until {
                break;
            }
            let read = tokio::select! {
                result = self.stdout.read(&mut self.out_buffer), if self.out_open => (true, result),
                result = self.stderr.read(&mut self.err_buffer), if self.err_open => (false, result),
                () = tokio::time::sleep_until(until) => break,
            };
            let (stdout, count) = read;
            let Ok(count) = count else {
                self.mark_capture_incomplete();
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
            if let Some(recorder) = self.recorder.as_mut() {
                recorder.write(bytes);
            }
            let taken = self.capture.push(bytes);
            #[cfg(test)]
            if let Some(delay) = self.drain_read_delay {
                tokio::time::advance(delay).await;
            }
            if taken > 0 {
                self.pending
                    .push_back(StreamEvent::Output(bytes[..taken].to_vec()));
            }
        }
    }

    fn mark_capture_incomplete(&mut self) {
        if let Some(recorder) = self.recorder.as_mut() {
            recorder.mark_incomplete();
        }
    }

    /// Queue what remains of the output and the exit.
    fn ended(&mut self, end: ProcessEnd) {
        // Child exit is not pipe EOF: the bounded drain may leave written bytes unread.
        if self.out_open || self.err_open {
            self.mark_capture_incomplete();
        }
        // Finish before publishing exit so `produced` waits for the writer's final flush.
        if let Some(mut recorder) = self.recorder.take() {
            recorder.finish();
        }
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

/// A session destructor can signal its group without waiting for the reader task.
pub(crate) struct GroupKill(Arc<std::sync::Mutex<Option<i32>>>);
impl GroupKill {
    pub(crate) fn kill(&self) {
        if let Some(pgid) = self.0.lock().unwrap().take()
            && pgid > 0
        {
            super::signal_group_if_present(Pid::from_raw(pgid), Signal::SIGKILL);
        }
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
    kill: Arc<std::sync::Mutex<Option<i32>>>,
    /// Inject a bounded reap with no status; keep the real child alive so a second
    /// Child::wait would remain pending. Used only by the stream lifecycle probe.
    #[cfg(test)]
    unreapable_leader: bool,
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
            Some(leader) => {
                #[cfg(test)]
                if self.unreapable_leader {
                    super::bounded_reap(
                        std::future::pending::<std::io::Result<std::process::ExitStatus>>(),
                        tokio::time::Instant::now(),
                    )
                    .await
                } else {
                    terminate(leader, self.pgid).await
                }
                #[cfg(not(test))]
                {
                    terminate(leader, self.pgid).await
                }
            }
            None => None,
        };
        *self.kill.lock().unwrap() = None;
        self.settled = true;
        status
    }
}

impl Drop for ProcessStream {
    fn drop(&mut self) {
        // Dropping an unconsumed stream closes the recorder without proving pipe EOF.
        self.mark_capture_incomplete();
        self.watchdog.abort();
    }
}

impl Drop for Group {
    fn drop(&mut self) {
        if self.settled {
            return;
        }
        let leader = self.leader.clone();
        // A destructor cannot await the grace period. Kill synchronously so no
        // process remains runnable when the owning call returns; only reap later.
        // The guard keeps a group already emptied by a reaped leader from taking an
        // unrelated group's SIGKILL.
        GroupKill(self.kill.clone()).kill();
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

    /// PR #473 Codex P2: the watchdog must not read a completed command as still
    /// running just because a pending `next()` holds the leader lock while it awaits
    /// the child. Holding that lock here (exactly what the pending wait does) at the
    /// deadline must still report the command's own exit, never `TimedOut`.
    #[tokio::test]
    async fn a_held_leader_lock_does_not_turn_a_completed_command_into_a_timeout() {
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
        // A pending `next()` holds the leader lock across its await of the child.
        let held = stream.group.leader.clone();
        let guard = held.lock().await;
        // The command exits while the lock is held, so the watchdog cannot reap it.
        tokio::time::timeout(Duration::from_secs(5), async {
            while runnable(pid) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the command never exited");
        fire.send(()).unwrap();
        // Let the watchdog cancel and queue on the lock, then release it.
        tokio::task::yield_now().await;
        drop(guard);
        assert_eq!(
            stream.next().await,
            Some(StreamEvent::Exited(ProcessEnd::Exited(7)))
        );
        assert_eq!(stream.next().await, None);
    }

    /// Drive kill -> finish_ending -> Group::terminate with a bounded reap that
    /// returns no status. Kernel-stuck children cannot be manufactured unprivileged;
    /// the healthy leader stays alive to make any second wait observably block.
    #[tokio::test(start_paused = true)]
    async fn a_leader_the_deadline_cannot_reap_yields_no_status_not_a_second_wait() {
        let dir = tempfile::tempdir().unwrap();
        let service = ProcessService::new(dir.path());
        let mut stream = service
            .start(
                "echo ready; exec sleep 30",
                std::future::pending(),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(matches!(stream.next().await, Some(StreamEvent::Output(_))));
        let pid = stream.group.pgid;
        stream.group.unreapable_leader = true;
        let outcome = tokio::time::timeout(Duration::from_secs(5), async {
            stream.kill().await;
            assert!(stream.group.settled);
            assert!(matches!(stream.phase, Phase::Ended));
            assert_eq!(
                stream.next().await,
                Some(StreamEvent::Exited(ProcessEnd::Cancelled))
            );
            assert_eq!(stream.next().await, None);
        })
        .await;
        // The injected terminator deliberately left the real child running and cleared
        // the group's kill handle. Re-arm both so the drop still terminates the fixture,
        // then assert the leader really stopped, as the removed healthy-reap probe did.
        stream.group.settled = false;
        *stream.group.kill.lock().unwrap() = Some(pid);
        drop(stream);
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while runnable(pid) && std::time::Instant::now() < deadline {
            tokio::task::yield_now().await;
        }
        assert!(!runnable(pid), "the leader survived the terminated group");
        outcome.expect("stream cleanup added a second wait after bounded reap returned no status");
    }

    /// #549: every byte was written before draining starts, but reader work on a
    /// loaded executor can use up the fixed deadline while bytes remain buffered.
    #[tokio::test(start_paused = true)]
    async fn a_slow_drain_with_unread_bytes_cannot_report_complete() {
        use crate::outputs::{CallOutputs, Capture, OutputCaps, OutputStore, ToolOutputsService};
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(OutputStore::temporary(OutputCaps::DEFAULT));
        let outputs = CallOutputs::new(store.clone(), p1_redact::SecretSet::new());
        let service = ProcessService::new(dir.path())
            .with_env_snapshot(vec![("HOME".into(), dir.path().into())]);
        let mut stream = service
            .start(
                "printf 'all bytes written\\n'",
                std::future::pending(),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        stream.record_into(outputs.record());
        // Reap before reading: all fixture bytes now sit in stdout's pipe.
        assert!(stream.group.wait().await.unwrap().success());
        stream.out_buffer = vec![0; 1].into_boxed_slice();
        stream.drain_read_delay = Some(super::super::SIGKILL_WAIT);
        stream.phase = Phase::Ending(ProcessEnd::Exited(0));
        while stream.next().await.is_some() {}
        let info = outputs.produced().remove(0);
        assert_eq!(info.stored_bytes, 1, "deadline left written bytes unread");
        assert_eq!(info.capture, Capture::StorageIncomplete);
        assert_eq!(store.page(&info.handle, 0, 100).unwrap().text, "a");
    }

    /// #549: post-termination draining has a deadline. Inject an unreapable leader so
    /// its pipes remain open, and let fake time expire the drain without an EOF.
    #[tokio::test(start_paused = true)]
    async fn a_drain_deadline_without_pipe_eof_cannot_report_complete() {
        use crate::outputs::{CallOutputs, Capture, OutputCaps, OutputStore, ToolOutputsService};
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(OutputStore::temporary(OutputCaps::DEFAULT));
        let outputs = CallOutputs::new(store.clone(), p1_redact::SecretSet::new());
        let service = ProcessService::new(dir.path())
            .with_env_snapshot(vec![("HOME".into(), dir.path().into())]);
        let mut stream = service
            .start(
                "printf 'ready\\n'; exec sleep 30",
                std::future::pending(),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        stream.record_into(outputs.record());
        assert_eq!(
            stream.next().await,
            Some(StreamEvent::Output(b"ready\n".to_vec()))
        );
        stream.group.unreapable_leader = true;
        stream.kill().await;
        // Injection left the real child alive; restore drop cleanup before asserting.
        stream.group.settled = false;
        drop(stream);
        let info = outputs.produced().remove(0);
        assert_eq!(info.capture, Capture::StorageIncomplete);
        assert_eq!(info.stored_bytes, 6);
        assert_eq!(store.page(&info.handle, 0, 100).unwrap().text, "ready\n");
    }

    /// #549: dropping a partially consumed process closes its store queue, but does
    /// not prove EOF. Its recoverable prefix must never be labelled complete.
    #[tokio::test]
    async fn dropping_before_eof_stores_an_incomplete_prefix() {
        use crate::outputs::{CallOutputs, Capture, OutputCaps, OutputStore, ToolOutputsService};
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(OutputStore::temporary(OutputCaps::DEFAULT));
        let outputs = CallOutputs::new(store.clone(), p1_redact::SecretSet::new());
        let service = ProcessService::new(dir.path())
            .with_env_snapshot(vec![("HOME".into(), dir.path().into())]);
        let mut stream = service
            .start(
                "printf 'ready\\n'; exec sleep 30",
                std::future::pending(),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        stream.record_into(outputs.record());
        assert_eq!(
            stream.next().await,
            Some(StreamEvent::Output(b"ready\n".to_vec()))
        );
        drop(stream);
        let info = outputs.produced().remove(0);
        assert_eq!(info.capture, Capture::StorageIncomplete);
        assert_eq!(store.page(&info.handle, 0, 100).unwrap().text, "ready\n");
    }

    /// ADR-0109 item 1, #510 definition of done 6: a command printing 200 MiB is stored
    /// whole, and what the host holds in memory for it stays at today's head and tail plus the
    /// tee's bounded hold-back, whatever the command prints. Buffer sizes are asserted after
    /// every event, not the process's RSS.
    #[tokio::test]
    async fn a_200_mib_output_is_stored_while_the_resident_capture_stays_bounded() {
        use crate::outputs::{CallOutputs, OutputCaps, OutputStore, ToolOutputsService};
        const PRINTED: u64 = 200 * 1024 * 1024;
        let dir = tempfile::tempdir().unwrap();
        let scratch = tempfile::tempdir().unwrap();
        let store = Arc::new(OutputStore::in_directory(
            scratch.path().join("session.jsonl.outputs"),
            OutputCaps {
                per_output: 2 * PRINTED,
                per_session: 2 * PRINTED,
            },
        ));
        let outputs = CallOutputs::new(store.clone(), p1_redact::SecretSet::new());
        let service = ProcessService::new(dir.path());
        let mut stream = service
            .spawn(
                ProcessRequest {
                    command: &format!(
                        "yes 0123456789abcdefghijklmnopqrstuvwxyz | head -c {PRINTED}"
                    ),
                    timeout: Duration::from_secs(600),
                },
                CancellationToken::new(),
            )
            .await
            .unwrap();
        stream.record_into(outputs.record());
        // The tee's own bound: the redactor's hold-back plus one read and the write buffer.
        let tee_bound = crate::outputs::MAX_HELD_BYTES + READ_BUFFER_BYTES + 64 * 1024;
        let mut shown = 0;
        let mut end = None;
        while let Some(event) = stream.next().await {
            assert!(stream.capture.head_len <= super::super::HEAD_BYTES);
            assert!(stream.capture.tail.len() <= super::super::TAIL_BYTES);
            if let Some(recorder) = &stream.recorder {
                assert!(recorder.held() <= tee_bound, "{}", recorder.held());
            }
            match event {
                StreamEvent::Output(bytes) => shown += bytes.len(),
                StreamEvent::Exited(exit) => end = Some(exit),
            }
        }
        assert_eq!(end, Some(ProcessEnd::Exited(0)));
        assert!(
            shown <= super::super::HEAD_BYTES + super::super::TAIL_BYTES + 200,
            "{shown}"
        );
        let produced = outputs.produced();
        assert_eq!(produced.len(), 1);
        // The disk writes on its own thread and never holds the stream up: an output it could
        // not keep up with says so, and holds an exact prefix.
        match produced[0].capture {
            crate::outputs::Capture::Complete => assert_eq!(produced[0].stored_bytes, PRINTED),
            crate::outputs::Capture::StorageIncomplete => {
                assert!(produced[0].stored_bytes < PRINTED)
            }
            other => panic!("{other:?}"),
        }
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
