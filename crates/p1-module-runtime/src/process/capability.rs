//! The native side of a WebAssembly guest's `process` capability
//! (`modules/wit/process.wit`): [`ProcessCapability`] implements the runtime's
//! [`ProcessService`](crate::ProcessService) trait over the native
//! [`ProcessService`], so a guest's `process.spawn` runs exactly what the native
//! shell tool runs.
//!
//! A guest's command carries only a script and a time limit, and that is all this
//! adapter reads: the program, the environment, the working directory, the sandbox
//! and the output bounds are the service's, fixed when the host assembled it.

use std::sync::{Arc, Mutex};
use std::time::Duration;

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
}

impl ProcessCapability {
    pub fn new(service: Arc<ProcessService>) -> Self {
        Self {
            service,
            evidence: None,
        }
    }

    /// Keep observed process exits outside guest-controlled output, scoped by call token.
    pub fn recording(mut self, evidence: ExitRecords) -> Self {
        self.evidence = Some(evidence);
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
            let request = ProcessRequest {
                command: &command.script,
                timeout: Duration::from_millis(command.timeout_ms),
            };
            match self.service.spawn(request, cancel.clone()).await {
                Ok(stream) => Ok(Box::new(CapabilityProcess::new(
                    stream,
                    self.evidence.clone(),
                    cancel,
                )) as Box<dyn RunningProcess>),
                Err(failure) => Err(failure.to_string()),
            }
        })
    }
}

/// One started command as the runtime holds it. Dropping it drops the stream, which
/// ends the process group if the command still runs.
struct CapabilityProcess {
    stream: ProcessStream,
    /// The exit still owed after a failure was reported as output.
    owed_exit: Option<ExitStatus>,
    /// The output handed out so far ended a line (or there was none).
    at_line_start: bool,
    evidence: Option<ExitRecords>,
    call: CancellationToken,
}

impl CapabilityProcess {
    fn new(stream: ProcessStream, evidence: Option<ExitRecords>, call: CancellationToken) -> Self {
        Self {
            stream,
            owed_exit: None,
            at_line_start: true,
            evidence,
            call,
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

impl RunningProcess for CapabilityProcess {
    fn next(&mut self) -> BoxFuture<'_, Option<ProcessEvent>> {
        Box::pin(async move {
            if let Some(status) = self.owed_exit.take() {
                return Some(ProcessEvent::Exited(status));
            }
            // The stream's `next` is cancellation-safe and nothing here awaits after it,
            // so a dropped future loses no event.
            let event = self.stream.next().await?;
            Some(self.event(event))
        })
    }

    fn kill(&mut self) -> BoxFuture<'_, ()> {
        Box::pin(self.stream.kill())
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
        let mut process = CapabilityProcess::new(stream, None, CancellationToken::new());
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
}
