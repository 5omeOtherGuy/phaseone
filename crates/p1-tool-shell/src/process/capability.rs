//! The native side of a WebAssembly guest's `process` capability
//! (`modules/wit/process.wit`): [`ProcessCapability`] implements the runtime's
//! [`ProcessService`](p1_module_runtime::ProcessService) over this crate's
//! [`ProcessService`], so a guest's `process.spawn` runs exactly what the native
//! shell tool runs.
//!
//! A guest's command carries only a script and a time limit, and that is all this
//! adapter reads: the program, the environment, the working directory, the sandbox
//! and the output bounds are the service's, fixed when the host assembled it.

use std::sync::Arc;
use std::time::Duration;

use p1_contracts::{BoxFuture, CancellationToken};
use p1_module_runtime::{ExitStatus, ProcessCommand, ProcessEvent, RunningProcess};

use super::{ProcessEnd, ProcessRequest, ProcessService, ProcessStream, StreamEvent};

/// What `spawn` answers for a call cancelled before its command started. The runtime
/// never shows it to the guest: it turns a refused start of a cancelled call into
/// `exited(cancelled)` on the resource.
const NOT_STARTED: &str = "the command was not started: its call was already cancelled";

/// The `process` capability a host grants a module, linked to one assembled service.
#[derive(Clone)]
pub struct ProcessCapability {
    service: Arc<ProcessService>,
}

impl ProcessCapability {
    pub fn new(service: Arc<ProcessService>) -> Self {
        Self { service }
    }
}

impl p1_module_runtime::ProcessService for ProcessCapability {
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
            match self.service.spawn(request, cancel).await {
                Ok(stream) => {
                    Ok(Box::new(CapabilityProcess::new(stream)) as Box<dyn RunningProcess>)
                }
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
}

impl CapabilityProcess {
    fn new(stream: ProcessStream) -> Self {
        Self {
            stream,
            owed_exit: None,
            at_line_start: true,
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
                ProcessEnd::Exited(code) => ProcessEvent::Exited(ExitStatus::Code(code)),
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
        let mut process = CapabilityProcess::new(stream);
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
