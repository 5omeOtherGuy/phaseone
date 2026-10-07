//! The native side of a WebAssembly guest's `process` capability
//! (`modules/wit/process.wit`): [`ProcessCapability`] implements the runtime's
//! [`ProcessService`](crate::ProcessService) trait over the native
//! [`ProcessService`], so a guest's `process.spawn` runs exactly what the native
//! shell tool runs.
//!
//! A guest's command carries only a script and a time limit, and that is all this
//! adapter reads: the program, the environment, the working directory, the sandbox
//! and the output bounds are the service's, fixed when the host assembled it.
//!
//! Given the call's [`CallOutputs`] ([`ProcessCapability::storing`]), every command it starts
//! is also stored, masked and whole, in the host's output store (ADR-0109).

use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::jobs::JobRegistry;
use crate::outputs::CallOutputs;
use crate::{ExitStatus, ProcessCommand, ProcessEvent, RunningProcess};
use p1_contracts::{BoxFuture, CancellationToken};

use super::{ProcessEnd, ProcessRequest, ProcessService, ProcessStream, StreamEvent};

/// What `spawn` answers for a call cancelled before its command started. The runtime
/// never shows it to the guest: it turns a refused start of a cancelled call into
/// `exited(cancelled)` on the resource.
const NOT_STARTED: &str = "the command was not started: its call was already cancelled";

/// Host-observed exits paired with the cancellation token of their export call.
pub type ExitRecords = Arc<Mutex<Vec<(CancellationToken, i32)>>>;

/// The `process` capability a host grants a module, linked to one assembled service.
#[derive(Clone)]
pub struct ProcessCapability {
    service: Arc<ProcessService>,
    evidence: Option<ExitRecords>,
    outputs: Option<CallOutputs>,
    /// The session's job registry to hand a timed-out foreground command over to (ADR-0123).
    /// `None` keeps the old behaviour: the deadline kills the process group.
    jobs: Option<Arc<JobRegistry>>,
}

impl ProcessCapability {
    pub fn new(service: Arc<ProcessService>) -> Self {
        Self {
            service,
            evidence: None,
            outputs: None,
            jobs: None,
        }
    }

    /// Keep observed process exits outside guest-controlled output, scoped by call token.
    pub fn recording(mut self, evidence: ExitRecords) -> Self {
        self.evidence = Some(evidence);
        self
    }

    /// Store every command's output in `outputs`, the current call's (ADR-0109).
    pub fn storing(mut self, outputs: CallOutputs) -> Self {
        self.outputs = Some(outputs);
        self
    }

    /// Hand a foreground command that reaches its `timeout_seconds` over to `jobs` as a
    /// session background job instead of killing it (ADR-0123). Needs [`storing`] so the
    /// adopted job keeps the output the call already produced; without either, the deadline
    /// kills the process group as before.
    ///
    /// [`storing`]: ProcessCapability::storing
    pub fn adopting(mut self, jobs: Arc<JobRegistry>) -> Self {
        self.jobs = Some(jobs);
        self
    }
}

impl crate::ProcessService for ProcessCapability {
    /// `Err` is [`ProcessFailure`](super::ProcessFailure)'s text, the one the native
    /// shell tool shows the model. A call already cancelled starts nothing, as the
    /// native tool starts nothing then.
    fn spawn(
        &self,
        command: ProcessCommand,
        cancel: CancellationToken,
    ) -> BoxFuture<'_, Result<Box<dyn RunningProcess>, String>> {
        Box::pin(async move {
            if cancel.is_cancelled() {
                return Err(NOT_STARTED.to_owned());
            }
            let timeout = Duration::from_millis(command.timeout_ms);
            match &self.jobs {
                Some(jobs) => {
                    // ADR-0123: the deadline is NOT the stream's kill deadline. The stream
                    // runs with its own token — linked to the call until handover, then the
                    // job's own — and never expires; this capability races the deadline
                    // beside it and adopts the command if it fires while it still runs.
                    // No earlier call's handover may answer `handed-over` for this one.
                    jobs.handover().clear();
                    let stream_cancel = CancellationToken::new();
                    let started = tokio::time::Instant::now();
                    let mut stream = self
                        .service
                        .start(
                            &command.script,
                            std::future::pending::<()>(),
                            stream_cancel.clone(),
                        )
                        .await
                        .map_err(|failure| failure.to_string())?;
                    // The adopted job keeps the call's output entry; a call with no output
                    // service of its own (a grant without `storing`) records into the
                    // registry's store instead, so the handover still keeps the output.
                    let recorder = match &self.outputs {
                        Some(call_outputs) => call_outputs.record(),
                        None => jobs.recorder(),
                    };
                    let output = recorder.entry();
                    stream.record_into(recorder);
                    let link = {
                        let call = cancel.clone();
                        let stream_cancel = stream_cancel.clone();
                        tokio::spawn(async move {
                            call.cancelled().await;
                            stream_cancel.cancel();
                        })
                    };
                    Ok(Box::new(CapabilityProcess {
                        stream: Some(stream),
                        owed_exit: None,
                        at_line_start: true,
                        evidence: self.evidence.clone(),
                        call: cancel,
                        adoption: Some(Adoption {
                            jobs: jobs.clone(),
                            command: command.script.clone(),
                            deadline: Box::pin(tokio::time::sleep(timeout)),
                            started,
                            output,
                            stream_cancel,
                            link,
                            ended: false,
                        }),
                    }) as Box<dyn RunningProcess>)
                }
                None => {
                    let request = ProcessRequest {
                        command: &command.script,
                        timeout,
                    };
                    match self.service.spawn(request, cancel.clone()).await {
                        Ok(mut stream) => {
                            if let Some(outputs) = &self.outputs {
                                stream.record_into(outputs.record());
                            }
                            Ok(Box::new(CapabilityProcess {
                                stream: Some(stream),
                                owed_exit: None,
                                at_line_start: true,
                                evidence: self.evidence.clone(),
                                call: cancel,
                                adoption: None,
                            }) as Box<dyn RunningProcess>)
                        }
                        Err(failure) => Err(failure.to_string()),
                    }
                }
            }
        })
    }
}

/// One started command as the runtime holds it. Dropping it drops the stream, which
/// ends the process group if the command still runs.
struct CapabilityProcess {
    /// The stream; `None` only once the command was handed over to a job (ADR-0123).
    stream: Option<ProcessStream>,
    /// The exit still owed after a failure was reported as output.
    owed_exit: Option<ExitStatus>,
    /// The output handed out so far ended a line (or there was none).
    at_line_start: bool,
    evidence: Option<ExitRecords>,
    call: CancellationToken,
    /// Present while the command may still be handed over at its deadline (ADR-0123).
    adoption: Option<Adoption>,
}

/// ADR-0123: everything the capability needs to hand a foreground command that reaches its
/// deadline over to the session's job registry as a background job.
struct Adoption {
    jobs: Arc<JobRegistry>,
    command: String,
    /// The foreground deadline, raced beside the stream.
    deadline: Pin<Box<dyn Future<Output = ()> + Send>>,
    started: tokio::time::Instant,
    /// The call's output-store entry, kept whole for the adopted job (ADR-0109).
    output: Arc<crate::outputs::Entry>,
    /// The stream's own token: linked to the call until handover, then the job's.
    stream_cancel: CancellationToken,
    /// Forwards the call's cancellation to [`Adoption::stream_cancel`] until handover.
    link: tokio::task::JoinHandle<()>,
    /// The command had already ended when the deadline fired; it is never adopted.
    ended: bool,
}

impl CapabilityProcess {
    /// The next raw stream event: while the command may still be handed over, the deadline
    /// raced beside the stream, adopting the command if it fires first (ADR-0123).
    async fn advance(&mut self) -> Option<StreamEvent> {
        // A cancelled call is never handed over: its cancellation already ends the command,
        // and the deadline race is skipped so a command killed at its deadline cannot be
        // adopted.
        if self.adoption.as_ref().is_none_or(|adoption| adoption.ended) || self.call.is_cancelled()
        {
            let stream = self.stream.as_mut()?;
            return stream.next().await;
        }
        enum Raced {
            Stream(Option<StreamEvent>),
            Deadline,
        }
        let raced = {
            let adoption = self.adoption.as_mut().expect("checked above");
            let stream = self
                .stream
                .as_mut()
                .expect("a foreground stream until handover");
            // `biased` polls the deadline first, so a command that floods its output cannot
            // starve it; the stream is polled while the deadline is pending.
            tokio::select! {
                biased;
                () = &mut adoption.deadline => Raced::Deadline,
                event = stream.next() => Raced::Stream(event),
            }
        };
        match raced {
            Raced::Stream(event) => event,
            Raced::Deadline => {
                // A command that already ended by its deadline is not handed over: the
                // stream reports its own output and exit, and the race is not run again.
                let stream = self
                    .stream
                    .as_mut()
                    .expect("a foreground stream until handover");
                if stream.has_ended() {
                    self.adoption.as_mut().expect("checked above").ended = true;
                    return stream.next().await;
                }
                self.hand_over().await;
                Some(StreamEvent::Exited(ProcessEnd::TimedOut))
            }
        }
    }

    /// Adopt the still-running command as a session job (ADR-0123), severing the call's
    /// cancellation link. A registry already closed leaves the command to the stream's drop,
    /// which ends its group.
    async fn hand_over(&mut self) {
        let Some(adoption) = self.adoption.take() else {
            return;
        };
        adoption.link.abort();
        let Some(stream) = self.stream.take() else {
            return;
        };
        if let Ok(id) = adoption.jobs.adopt(
            adoption.command,
            stream,
            adoption.stream_cancel,
            adoption.started,
            adoption.output,
        ) {
            adoption.jobs.handover().set(id);
        } else {
            // A closed registry dropped the stream, which ended its group: nothing was
            // handed over, so the call must render a plain timeout.
            adoption.jobs.handover().clear();
        }
    }

    fn event(&mut self, event: StreamEvent) -> ProcessEvent {
        match event {
            StreamEvent::Output(bytes) => {
                if let Some(&last) = bytes.last() {
                    self.at_line_start = last == b'\n';
                }
                ProcessEvent::Output(bytes)
            }
            StreamEvent::Exited(end) => match end {
                ProcessEnd::Exited(code) => {
                    if let Some(evidence) = &self.evidence {
                        evidence.lock().unwrap().push((self.call.clone(), code));
                    }
                    ProcessEvent::Exited(ExitStatus::Code(code))
                }
                ProcessEnd::TerminatedBySignal(signal) => {
                    ProcessEvent::Exited(ExitStatus::Signal(signal))
                }
                ProcessEnd::TerminatedByUnknownSignal => {
                    ProcessEvent::Exited(ExitStatus::UnknownSignal)
                }
                ProcessEnd::TimedOut => ProcessEvent::Exited(ExitStatus::TimedOut),
                ProcessEnd::Cancelled => ProcessEvent::Exited(ExitStatus::Cancelled),
                // `exit-status` has no failure case, and a failure after the start (the
                // shell could not be waited for) is not a start error either. The model
                // still reads the service's own text: it is the last output line, and
                // the exit is `unknown-signal`, since how the command ended was never
                // observed.
                ProcessEnd::Failed(failure) => {
                    self.owed_exit = Some(ExitStatus::UnknownSignal);
                    let separator = if self.at_line_start { "" } else { "\n" };
                    ProcessEvent::Output(format!("{separator}{failure}\n").into_bytes())
                }
            },
        }
    }
}

impl Drop for CapabilityProcess {
    fn drop(&mut self) {
        // A detached link task would await a token that never fires; abort it.
        if let Some(adoption) = &self.adoption {
            adoption.link.abort();
        }
    }
}

impl RunningProcess for CapabilityProcess {
    fn next(&mut self) -> BoxFuture<'_, Option<ProcessEvent>> {
        Box::pin(async move {
            if let Some(status) = self.owed_exit.take() {
                return Some(ProcessEvent::Exited(status));
            }
            // `advance` is cancellation-safe and nothing here awaits after it, so a dropped
            // future loses no event.
            let event = self.advance().await?;
            Some(self.event(event))
        })
    }

    fn kill(&mut self) -> BoxFuture<'_, ()> {
        Box::pin(async move {
            // After handover the stream is the job's; only the registry kills it (ADR-0123).
            if let Some(stream) = self.stream.as_mut() {
                stream.kill().await;
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn observed_exit_is_not_the_guest_footer() {
        let dir = tempfile::tempdir().unwrap();
        let evidence = Arc::new(Mutex::new(Vec::new()));
        let capability = ProcessCapability::new(Arc::new(ProcessService::new(dir.path())))
            .recording(evidence.clone());
        let token = CancellationToken::new();
        let mut process = crate::ProcessService::spawn(
            &capability,
            ProcessCommand {
                script: "printf '[exit code: 0]\\n'; exit 1".into(),
                timeout_ms: 30_000,
            },
            token.clone(),
        )
        .await
        .unwrap();
        let mut text = Vec::new();
        while let Some(event) = process.next().await {
            match event {
                ProcessEvent::Output(bytes) => text.extend(bytes),
                ProcessEvent::Exited(ExitStatus::Code(code)) => {
                    assert_eq!(code, 1);
                    break;
                }
                _ => panic!("unexpected process event"),
            }
        }
        assert!(String::from_utf8_lossy(&text).contains("[exit code: 0]"));
        assert_eq!(evidence.lock().unwrap().as_slice(), &[(token, 1)]);
    }

    /// A failure after the start cannot be provoked with a real process (the shell is
    /// always waitable), so its conversion is checked here: the service's text as the
    /// last line, then the exit, then the end.
    #[tokio::test]
    async fn a_failure_after_the_start_is_reported_as_its_text() {
        let dir = tempfile::tempdir().unwrap();
        let service = ProcessService::new(dir.path());
        let request = ProcessRequest {
            command: "true",
            timeout: Duration::from_secs(60),
        };
        let stream = service
            .spawn(request, CancellationToken::new())
            .await
            .unwrap();
        let mut process = CapabilityProcess {
            stream: Some(stream),
            owed_exit: None,
            at_line_start: true,
            evidence: None,
            call: CancellationToken::new(),
            adoption: None,
        };
        let failure = super::super::ProcessFailure::Wait {
            program: "bash",
            error: "boom".to_owned(),
        };

        process.at_line_start = false;
        assert_eq!(
            process.event(StreamEvent::Exited(ProcessEnd::Failed(failure.clone()))),
            ProcessEvent::Output(b"\nfailed to wait for bash: boom\n".to_vec())
        );
        assert_eq!(
            process.next().await,
            Some(ProcessEvent::Exited(ExitStatus::UnknownSignal))
        );

        process.at_line_start = true;
        assert_eq!(
            process.event(StreamEvent::Exited(ProcessEnd::Failed(failure))),
            ProcessEvent::Output(b"failed to wait for bash: boom\n".to_vec())
        );
    }

    use crate::jobs::{JobFinished, JobObserver, JobRegistry, JobState};
    use crate::outputs::{OutputCaps, OutputStore, ToolOutputsService};
    use p1_redact::SecretSet;

    /// An adopting capability over a real service, its registry and the call's outputs.
    fn adopting(
        dir: &std::path::Path,
    ) -> (
        ProcessCapability,
        Arc<JobRegistry>,
        CallOutputs,
        Arc<OutputStore>,
    ) {
        let service = Arc::new(ProcessService::new(dir));
        let store = Arc::new(OutputStore::temporary(OutputCaps::DEFAULT));
        let secrets = SecretSet::new();
        let jobs = Arc::new(JobRegistry::new(
            service.clone(),
            store.clone(),
            secrets.clone(),
        ));
        let outputs = CallOutputs::new(store.clone(), secrets);
        let capability = ProcessCapability::new(service)
            .storing(outputs.clone())
            .adopting(jobs.clone());
        (capability, jobs, outputs, store)
    }

    struct Notify(tokio::sync::mpsc::UnboundedSender<JobFinished>);
    impl JobObserver for Notify {
        fn started(&self) -> Option<u64> {
            None
        }
        fn ended(&self, _: &str, _: Option<u64>, _: &ExitStatus) {}
        fn finished(&self, job: JobFinished) {
            self.0.send(job).unwrap();
        }
    }

    /// ADR-0123: a command that outlives its deadline is adopted as the next job, keeping the
    /// id, the output it already produced and the one completion notification; the foreground
    /// call ends `timed-out` with no exit of its own.
    #[tokio::test]
    async fn a_command_that_outlives_its_deadline_is_adopted_not_killed() {
        let dir = tempfile::tempdir().unwrap();
        // A named pipe the command blocks on: it is alive at its deadline, with no sleep.
        let release = dir.path().join("release");
        nix::unistd::mkfifo(&release, nix::sys::stat::Mode::S_IRWXU).unwrap();
        let script = format!("echo ready; read answer < {}", release.display());
        let (capability, jobs, outputs, store) = adopting(dir.path());
        let (send, mut receive) = tokio::sync::mpsc::unbounded_channel();
        jobs.observe(Arc::new(Notify(send)));
        let mut process = crate::ProcessService::spawn(
            &capability,
            ProcessCommand {
                script,
                timeout_ms: 300,
            },
            CancellationToken::new(),
        )
        .await
        .unwrap();
        let mut text = Vec::new();
        let mut end = None;
        while let Some(event) = process.next().await {
            match event {
                ProcessEvent::Output(bytes) => text.extend(bytes),
                ProcessEvent::Exited(status) => {
                    end = Some(status);
                    break;
                }
            }
        }
        assert_eq!(end, Some(ExitStatus::TimedOut), "the call ends timed-out");
        assert!(String::from_utf8_lossy(&text).contains("ready"));
        assert_eq!(jobs.handover().get().as_deref(), Some("j1"));
        assert!(matches!(jobs.status("j1"), Ok(JobState::Running { .. })));
        // Let the adopted command finish: it ends on its own with one notification.
        let released = release.clone();
        tokio::task::spawn_blocking(move || std::fs::write(released, "go\n"))
            .await
            .unwrap()
            .unwrap();
        let notified = receive.recv().await.unwrap();
        assert_eq!(notified.id, "j1");
        assert_eq!(notified.end.status, ExitStatus::Code(0));
        assert!(notified.tail.contains("ready"), "{:?}", notified.tail);
        // The adopted job kept the call's own output entry (ADR-0109), whole: the "ready" the
        // call had already produced is still in the stored output the job reports.
        let info = outputs.produced().pop().expect("a stored output");
        assert_eq!(
            notified.end.output, info.handle,
            "the job kept the call's entry"
        );
        assert!(
            store
                .page(&info.handle, 0, 1000)
                .unwrap()
                .text
                .contains("ready")
        );
        jobs.shutdown().await;
    }

    /// A timeout that cannot be handed over (the session's registry is closed) never reports an
    /// earlier call's job id: the slot is cleared at every foreground start and on a failed
    /// adoption, so the guest renders a plain timeout.
    #[tokio::test]
    async fn a_failed_handover_never_reports_an_earlier_job() {
        let dir = tempfile::tempdir().unwrap();
        let (capability, jobs, _, _) = adopting(dir.path());
        jobs.handover().set("j7".into());
        jobs.cancel_all();
        let mut process = crate::ProcessService::spawn(
            &capability,
            ProcessCommand {
                script: "exec tail -f /dev/null".into(),
                timeout_ms: 200,
            },
            CancellationToken::new(),
        )
        .await
        .unwrap();
        let mut end = None;
        while let Some(event) = process.next().await {
            if let ProcessEvent::Exited(status) = event {
                end = Some(status);
                break;
            }
        }
        assert_eq!(end, Some(ExitStatus::TimedOut));
        assert_eq!(jobs.handover().get(), None, "no stale job id");
        jobs.shutdown().await;
    }

    /// ADR-0123 point 1: a command that has already ended when the deadline fires is not
    /// adopted; the call reports its own output and exit, and no job exists.
    #[tokio::test]
    async fn a_command_ended_by_its_deadline_keeps_its_own_exit() {
        let dir = tempfile::tempdir().unwrap();
        let (capability, jobs, _, _) = adopting(dir.path());
        let mut process = crate::ProcessService::spawn(
            &capability,
            ProcessCommand {
                script: "echo done; exit 3".into(),
                timeout_ms: 100,
            },
            CancellationToken::new(),
        )
        .await
        .unwrap();
        // Both have happened before the first poll: the biased race sees the deadline first.
        tokio::time::sleep(Duration::from_millis(500)).await;
        let mut output = Vec::new();
        let mut end = None;
        while let Some(event) = process.next().await {
            match event {
                ProcessEvent::Output(bytes) => output.extend(bytes),
                ProcessEvent::Exited(status) => end = Some(status),
            }
        }
        assert_eq!(end, Some(ExitStatus::Code(3)));
        assert_eq!(output, b"done\n");
        assert_eq!(jobs.handover().get(), None, "nothing was handed over");
        assert_eq!(jobs.running(), 0);
        jobs.shutdown().await;
    }

    /// ADR-0123 point 2 / requirement 3: a call cancelled while the command is still foreground
    /// is never handed over; the cancellation kills its group, exactly as before.
    #[tokio::test]
    async fn a_cancelled_foreground_call_kills_its_group_and_is_never_handed_over() {
        let dir = tempfile::tempdir().unwrap();
        let group = dir.path().join("group");
        let (capability, jobs, _, _) = adopting(dir.path());
        let token = CancellationToken::new();
        let mut process = crate::ProcessService::spawn(
            &capability,
            ProcessCommand {
                script: "echo ready; echo $$ > group; exec tail -f /dev/null".into(),
                timeout_ms: 30_000,
            },
            token.clone(),
        )
        .await
        .unwrap();
        assert!(matches!(
            process.next().await,
            Some(ProcessEvent::Output(_))
        ));
        let pgid = read_pgid(&group).await;
        token.cancel();
        let mut end = None;
        while let Some(event) = process.next().await {
            if let ProcessEvent::Exited(status) = event {
                end = Some(status);
            }
        }
        assert_eq!(end, Some(ExitStatus::Cancelled));
        assert_eq!(
            jobs.handover().get(),
            None,
            "a cancelled call is never handed over"
        );
        assert_group_gone(pgid).await;
    }

    /// ADR-0117's rule reached through an adopted job: the session's end kills what it adopted.
    #[tokio::test]
    async fn an_adopted_hung_job_is_killed_at_session_end() {
        let dir = tempfile::tempdir().unwrap();
        let release = dir.path().join("release");
        nix::unistd::mkfifo(&release, nix::sys::stat::Mode::S_IRWXU).unwrap();
        let group = dir.path().join("group");
        let script = format!(
            "echo ready; echo $$ > group; read answer < {}",
            release.display()
        );
        let (capability, jobs, _, _) = adopting(dir.path());
        let mut process = crate::ProcessService::spawn(
            &capability,
            ProcessCommand {
                script,
                timeout_ms: 300,
            },
            CancellationToken::new(),
        )
        .await
        .unwrap();
        while let Some(event) = process.next().await {
            if matches!(event, ProcessEvent::Exited(_)) {
                break;
            }
        }
        assert!(matches!(jobs.status("j1"), Ok(JobState::Running { .. })));
        let pgid = read_pgid(&group).await;
        jobs.shutdown().await;
        assert_group_gone(pgid).await;
    }

    async fn read_pgid(group: &std::path::Path) -> i32 {
        tokio::time::timeout(Duration::from_secs(30), async {
            loop {
                if let Ok(text) = std::fs::read_to_string(group)
                    && let Ok(pid) = text.trim().parse()
                {
                    break pid;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the command never published its group")
    }

    async fn assert_group_gone(pgid: i32) {
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while nix::sys::signal::killpg(nix::unistd::Pid::from_raw(pgid), None)
            != Err(nix::errno::Errno::ESRCH)
        {
            assert!(
                std::time::Instant::now() < deadline,
                "the group {pgid} survived"
            );
            tokio::task::yield_now().await;
        }
    }
}
