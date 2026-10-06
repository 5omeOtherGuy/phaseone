//! Session-owned background commands (ADR-0117).
use crate::process::{ProcessEnd, ProcessService, StreamEvent};
use crate::{ExitStatus, OutputStore};
use p1_contracts::{BoxFuture, CancellationToken};
use p1_redact::SecretSet;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::watch;
use tokio::time::Instant;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobEnd {
    pub status: ExitStatus,
    pub elapsed_ms: u64,
    pub output: String,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JobState {
    Running { elapsed_ms: u64, output_bytes: u64 },
    Ended(JobEnd),
}
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum JobError {
    #[error("unknown-job")]
    UnknownJob,
    #[error("{0}")]
    StartFailed(String),
}
#[derive(Debug, Clone)]
pub struct JobFinished {
    pub id: String,
    pub command: String,
    pub end: JobEnd,
    pub output_bytes: u64,
    pub tail: String,
}
pub trait JobObserver: Send + Sync {
    fn started(&self) -> Option<u64>;
    fn ended(&self, command: &str, baseline: Option<u64>, status: &ExitStatus);
    fn finished(&self, job: JobFinished);
}
pub trait ProcessJobsService: Send + Sync {
    fn start(
        &self,
        script: String,
        timeout_ms: Option<u64>,
    ) -> BoxFuture<'_, Result<String, JobError>>;
    fn status(&self, id: &str) -> Result<JobState, JobError>;
    fn cancel<'a>(&'a self, id: &'a str) -> BoxFuture<'a, Result<JobState, JobError>>;
}
struct Entry {
    started: Instant,
    output: Arc<crate::outputs::Entry>,
    cancel: CancellationToken,
    state: watch::Receiver<Option<JobEnd>>,
    delivered: watch::Receiver<bool>,
    kill: crate::process::stream::GroupKill,
}
struct RegistryState {
    next: u64,
    closed: bool,
    entries: HashMap<String, Arc<Entry>>,
}
pub struct JobRegistry {
    process: Arc<ProcessService>,
    outputs: Arc<OutputStore>,
    secrets: SecretSet,
    state: Mutex<RegistryState>,
    observer: Mutex<Option<Arc<dyn JobObserver>>>,
}
impl JobRegistry {
    pub fn new(
        process: Arc<ProcessService>,
        outputs: Arc<OutputStore>,
        secrets: SecretSet,
    ) -> Self {
        Self {
            process,
            outputs,
            secrets,
            state: Mutex::new(RegistryState {
                next: 0,
                closed: false,
                entries: HashMap::new(),
            }),
            observer: Mutex::new(None),
        }
    }
    pub fn observe(&self, observer: Arc<dyn JobObserver>) {
        *self.observer.lock().unwrap() = Some(observer);
    }
    pub fn running(&self) -> usize {
        self.state
            .lock()
            .unwrap()
            .entries
            .values()
            .filter(|e| !*e.delivered.borrow())
            .count()
    }
    pub async fn start(
        &self,
        command: String,
        timeout_ms: Option<u64>,
    ) -> Result<String, JobError> {
        let observer = self.observer.lock().unwrap().clone();
        let baseline = observer.as_ref().and_then(|o| o.started());
        let cancel = CancellationToken::new();
        let started = Instant::now();
        let expiry = async move {
            match timeout_ms {
                Some(ms) => tokio::time::sleep(Duration::from_millis(ms)).await,
                None => std::future::pending().await,
            }
        };
        let mut stream = self
            .process
            .start(&command, expiry, cancel.clone())
            .await
            .map_err(|e| JobError::StartFailed(e.to_string()))?;
        let recorder =
            crate::outputs::CallOutputs::new(self.outputs.clone(), self.secrets.clone()).record();
        let output = recorder.entry();
        stream.record_into(recorder);
        let kill = stream.group_kill();
        let (send, receive) = watch::channel(None);
        let (delivered, delivery) = watch::channel(false);
        let id = {
            let mut state = self.state.lock().unwrap();
            if state.closed {
                return Err(JobError::StartFailed("session ended".into()));
            }
            state.next += 1;
            let id = format!("j{}", state.next);
            state.entries.insert(
                id.clone(),
                Arc::new(Entry {
                    started,
                    output: output.clone(),
                    cancel,
                    state: receive,
                    delivered: delivery,
                    kill,
                }),
            );
            id
        };
        let task_id = id.clone();
        let store = self.outputs.clone();
        tokio::spawn(async move {
            let mut status = ExitStatus::UnknownSignal;
            while let Some(event) = stream.next().await {
                if let StreamEvent::Exited(end) = event {
                    status = exit_status(end);
                    break;
                }
            }
            let elapsed_ms = started.elapsed().as_millis().min(u64::MAX as u128) as u64;
            if let Some(observer) = &observer {
                observer.ended(&command, baseline, &status);
            }
            drop(stream);
            let info = tokio::task::spawn_blocking(move || output.report())
                .await
                .expect("output writer panicked");
            let end = JobEnd {
                status,
                elapsed_ms,
                output: info.handle.clone(),
            };
            // Fix the terminal state before the owning agent sees any announcement.
            send.send_replace(Some(end.clone()));
            if let Some(observer) = observer {
                let mut offset = info.stored_bytes.saturating_sub(2000);
                let tail = loop {
                    match store.page(&info.handle, offset, 2000) {
                        Ok(page) => break page.text,
                        Err(crate::outputs::OutputError::OffsetInsideCharacter)
                            if offset < info.stored_bytes =>
                        {
                            offset += 1
                        }
                        _ => break String::new(),
                    }
                };
                observer.finished(JobFinished {
                    id: task_id,
                    command,
                    end,
                    output_bytes: info.stored_bytes,
                    tail,
                });
            }
            delivered.send_replace(true);
        });
        Ok(id)
    }
    fn entry(&self, id: &str) -> Result<Arc<Entry>, JobError> {
        self.state
            .lock()
            .unwrap()
            .entries
            .get(id)
            .cloned()
            .ok_or(JobError::UnknownJob)
    }
    pub fn status(&self, id: &str) -> Result<JobState, JobError> {
        let e = self.entry(id)?;
        let end = e.state.borrow().clone();
        Ok(match end {
            Some(end) => JobState::Ended(end),
            None => JobState::Running {
                elapsed_ms: e.started.elapsed().as_millis().min(u64::MAX as u128) as u64,
                output_bytes: e.output.progress_bytes(),
            },
        })
    }
    pub async fn cancel(&self, id: &str) -> Result<JobState, JobError> {
        let e = self.entry(id)?;
        e.cancel.cancel();
        let mut state = e.delivered.clone();
        state
            .wait_for(|s| *s)
            .await
            .map_err(|_| JobError::UnknownJob)?;
        self.status(id)
    }
    pub fn cancel_all(&self) {
        let mut state = self.state.lock().unwrap();
        state.closed = true;
        for e in state.entries.values() {
            e.cancel.cancel();
            e.kill.kill();
        }
    }
    pub async fn shutdown(&self) {
        let entries = {
            let mut state = self.state.lock().unwrap();
            state.closed = true;
            state.entries.values().cloned().collect::<Vec<_>>()
        };
        for e in &entries {
            e.cancel.cancel();
        }
        for e in entries {
            let mut state = e.delivered.clone();
            let _ = state.wait_for(|s| *s).await;
        }
    }
}
impl Drop for JobRegistry {
    fn drop(&mut self) {
        for e in self.state.get_mut().unwrap().entries.values() {
            e.cancel.cancel();
            e.kill.kill();
        }
    }
}
impl ProcessJobsService for JobRegistry {
    fn start(
        &self,
        script: String,
        timeout_ms: Option<u64>,
    ) -> BoxFuture<'_, Result<String, JobError>> {
        Box::pin(self.start(script, timeout_ms))
    }
    fn status(&self, id: &str) -> Result<JobState, JobError> {
        self.status(id)
    }
    fn cancel<'a>(&'a self, id: &'a str) -> BoxFuture<'a, Result<JobState, JobError>> {
        Box::pin(self.cancel(id))
    }
}
fn exit_status(end: ProcessEnd) -> ExitStatus {
    match end {
        ProcessEnd::Exited(n) => ExitStatus::Code(n),
        ProcessEnd::TerminatedBySignal(n) => ExitStatus::Signal(n),
        ProcessEnd::TimedOut => ExitStatus::TimedOut,
        ProcessEnd::Cancelled => ExitStatus::Cancelled,
        _ => ExitStatus::UnknownSignal,
    }
}

pub(crate) fn link(
    linker: &mut wasmtime::component::Linker<crate::capabilities::CallState>,
) -> wasmtime::Result<()> {
    use wasmtime::component::Val;
    let mut interface = linker.instance(&crate::loader::interface_import("process-jobs"))?;
    for operation in ["start", "status", "cancel"] {
        interface.func_new_async(operation, move |store, _, params, results| {
            let service = store.data().process_jobs.clone();
            let cancel = store.data().cancel.clone();
            let request = crate::capabilities::check_arity(
                operation,
                params,
                results,
                if operation == "start" { 2 } else { 1 },
                1,
            )
            .and_then(|()| {
                let Val::String(text) = &params[0] else {
                    wasmtime::bail!("process-jobs: expected string");
                };
                let timeout = if operation == "start" {
                    match &params[1] {
                        Val::Option(None) => None,
                        Val::Option(Some(v)) => match **v {
                            Val::U64(ms) => Some(ms),
                            _ => wasmtime::bail!("process-jobs: expected timeout"),
                        },
                        _ => wasmtime::bail!("process-jobs: expected option"),
                    }
                } else {
                    None
                };
                Ok((text.clone(), timeout))
            });
            Box::new(async move {
                let (text, timeout) = request?;
                let service = service
                    .ok_or_else(|| wasmtime::format_err!("process-jobs: missing service"))?;
                let result = match operation {
                    "start" => {
                        if cancel.is_cancelled() {
                            Err(JobError::StartFailed("cancelled before start".into()))
                        } else {
                            service.start(text, timeout).await.map(Val::String)
                        }
                    }
                    "status" => service.status(&text).map(state_val),
                    _ => service.cancel(&text).await.map(state_val),
                };
                results[0] = match result {
                    Ok(v) => Val::Result(Ok(Some(Box::new(v)))),
                    Err(e) => Val::Result(Err(Some(Box::new(match e {
                        JobError::UnknownJob => Val::Variant("unknown-job".into(), None),
                        JobError::StartFailed(s) => {
                            Val::Variant("start-failed".into(), Some(Box::new(Val::String(s))))
                        }
                    })))),
                };
                Ok(())
            })
        })?;
    }
    Ok(())
}
fn state_val(state: JobState) -> wasmtime::component::Val {
    use wasmtime::component::Val;
    let (name, fields) = match state {
        JobState::Running {
            elapsed_ms,
            output_bytes,
        } => (
            "running",
            vec![
                ("elapsed-ms".into(), Val::U64(elapsed_ms)),
                ("output-bytes".into(), Val::U64(output_bytes)),
            ],
        ),
        JobState::Ended(end) => {
            let (name, payload) = match end.status {
                ExitStatus::Code(n) => ("code", Some(Val::S32(n))),
                ExitStatus::Signal(n) => ("signal", Some(Val::S32(n))),
                ExitStatus::UnknownSignal => ("unknown-signal", None),
                ExitStatus::TimedOut => ("timed-out", None),
                ExitStatus::Cancelled => ("cancelled", None),
            };
            (
                "ended",
                vec![
                    (
                        "status".into(),
                        Val::Variant(name.into(), payload.map(Box::new)),
                    ),
                    ("elapsed-ms".into(), Val::U64(end.elapsed_ms)),
                    ("output".into(), Val::String(end.output)),
                ],
            )
        }
    };
    Val::Variant(name.into(), Some(Box::new(Val::Record(fields))))
}
#[cfg(test)]
mod tests {
    use super::*;
    struct Observer(tokio::sync::mpsc::UnboundedSender<JobFinished>);
    impl JobObserver for Observer {
        fn started(&self) -> Option<u64> {
            None
        }
        fn ended(&self, _: &str, _: Option<u64>, _: &ExitStatus) {}
        fn finished(&self, job: JobFinished) {
            self.0.send(job).unwrap();
        }
    }
    fn registry(dir: &std::path::Path) -> JobRegistry {
        JobRegistry::new(
            Arc::new(ProcessService::new(dir)),
            Arc::new(OutputStore::in_directory(
                dir.join("outputs"),
                Default::default(),
            )),
            SecretSet::new(),
        )
    }
    #[tokio::test]
    async fn background_start_returns_and_ends_without_status_polling() {
        let dir = tempfile::tempdir().unwrap();
        let jobs = registry(dir.path());
        let (send, mut receive) = tokio::sync::mpsc::unbounded_channel();
        jobs.observe(Arc::new(Observer(send)));
        // A named pipe blocks the command until this test explicitly releases it.
        nix::unistd::mkfifo(&dir.path().join("release"), nix::sys::stat::Mode::S_IRWXU).unwrap();
        let id = jobs
            .start("read answer < release; printf done".into(), None)
            .await
            .unwrap();
        assert_eq!(id, "j1");
        assert!(matches!(jobs.status(&id), Ok(JobState::Running { .. })));
        let release = dir.path().join("release");
        tokio::task::spawn_blocking(move || std::fs::write(release, "go\n"))
            .await
            .unwrap()
            .unwrap();
        let notified = receive.recv().await.unwrap();
        assert_eq!(notified.tail, "done");
        assert_eq!(notified.end.status, ExitStatus::Code(0));
        assert_eq!(jobs.status(&id), Ok(JobState::Ended(notified.end)));
        assert!(receive.try_recv().is_err());
        jobs.shutdown().await;
    }
    #[tokio::test]
    async fn background_cancel_is_session_owned_and_shutdown_removes_group() {
        let dir = tempfile::tempdir().unwrap();
        let jobs = registry(dir.path());
        let other = registry(dir.path());
        let id = jobs
            .start("echo $$ > group; tail -f /dev/null & wait".into(), None)
            .await
            .unwrap();
        assert_eq!(other.cancel(&id).await, Err(JobError::UnknownJob));
        let own = other
            .start("exec tail -f /dev/null".into(), None)
            .await
            .unwrap();
        assert_eq!(own, id, "job ids are explicitly local to a registry");
        other.cancel(&own).await.unwrap();
        assert!(matches!(jobs.status(&id), Ok(JobState::Running { .. })));
        other.shutdown().await;
        let group = dir.path().join("group");
        let pgid: i32 = tokio::time::timeout(Duration::from_secs(30), async {
            loop {
                if let Ok(text) = std::fs::read_to_string(&group)
                    && let Ok(pid) = text.trim().parse()
                {
                    break pid;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        jobs.shutdown().await;
        assert_eq!(
            nix::sys::signal::killpg(nix::unistd::Pid::from_raw(pgid), None),
            Err(nix::errno::Errno::ESRCH)
        );
        assert!(matches!(
            jobs.status(&id),
            Ok(JobState::Ended(JobEnd {
                status: ExitStatus::Cancelled,
                ..
            }))
        ));
    }
    #[tokio::test]
    async fn background_failed_exit_keeps_redacted_full_output() {
        let dir = tempfile::tempdir().unwrap();
        let jobs = registry(dir.path());
        let (send, mut receive) = tokio::sync::mpsc::unbounded_channel();
        jobs.observe(Arc::new(Observer(send)));
        let id = jobs
            .start("printf 'one\\ntwo\\n'; exit 7".into(), None)
            .await
            .unwrap();
        let notification = receive.recv().await.unwrap();
        assert_eq!(notification.end.status, ExitStatus::Code(7));
        assert_eq!(
            jobs.outputs
                .page(&notification.end.output, 0, 2000)
                .unwrap()
                .text,
            "one\ntwo\n"
        );
        assert_eq!(jobs.status(&id).unwrap(), JobState::Ended(notification.end));
        jobs.shutdown().await;
    }
    #[tokio::test(start_paused = true)]
    async fn background_timeout_ends_without_polling() {
        let dir = tempfile::tempdir().unwrap();
        let jobs = registry(dir.path());
        let (send, mut receive) = tokio::sync::mpsc::unbounded_channel();
        jobs.observe(Arc::new(Observer(send)));
        let id = jobs
            .start("exec tail -f /dev/null".into(), Some(100))
            .await
            .unwrap();
        tokio::time::advance(Duration::from_millis(100)).await;
        let notification = receive.recv().await.unwrap();
        assert_eq!(notification.end.status, ExitStatus::TimedOut);
        assert_eq!(jobs.status(&id).unwrap(), JobState::Ended(notification.end));
        jobs.shutdown().await;
    }
    #[tokio::test]
    async fn background_drop_kills_group_even_before_reader_runs() {
        let dir = tempfile::tempdir().unwrap();
        let jobs = registry(dir.path());
        let id = jobs
            .start("exec tail -f /dev/null".into(), None)
            .await
            .unwrap();
        let entry = jobs.entry(&id).unwrap();
        drop(jobs);
        let mut state = entry.delivered.clone();
        tokio::time::timeout(Duration::from_secs(30), state.wait_for(|s| *s))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            entry.state.borrow().as_ref().unwrap().status,
            ExitStatus::Cancelled
        );
    }
    #[tokio::test]
    async fn background_output_is_redacted_and_tail_is_from_storage() {
        let dir = tempfile::tempdir().unwrap();
        let jobs = registry(dir.path());
        // Ordinary text registered solely to exercise masking; not a credential fixture.
        jobs.secrets.register("ordinary-marker");
        let (send, mut receive) = tokio::sync::mpsc::unbounded_channel();
        jobs.observe(Arc::new(Observer(send)));
        jobs.start(
            "printf 'ordinary-marker\\n'; printf '%03000d' 0".into(),
            None,
        )
        .await
        .unwrap();
        let notification = receive.recv().await.unwrap();
        let page = jobs
            .outputs
            .page(&notification.end.output, 0, 50000)
            .unwrap();
        assert!(!page.text.contains("ordinary-marker"));
        assert!(page.text.contains("redacted"));
        assert_eq!(
            notification.tail.as_bytes(),
            &page.text.as_bytes()[page.text.len() - 2000..]
        );
        assert_eq!(notification.output_bytes, page.text.len() as u64);
        jobs.shutdown().await;
    }
}
