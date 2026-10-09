//! Agent-local job services and completion delivery.
use crate::activity::ActivityLog;
use p1_contracts::BoxFuture;
use p1_module_runtime::jobs::{
    Handover, JobError, JobFinished, JobObserver, JobRegistry, JobState, ProcessJobsService,
};
use p1_redact::MaskCounter;
use std::collections::HashMap;
use std::sync::{Arc, Mutex, Weak};

struct SessionJobs {
    mask: Weak<MaskCounter>,
    registry: Option<Arc<JobRegistry>>,
    observer: Option<Arc<dyn JobObserver>>,
}
#[derive(Default)]
pub(crate) struct JobHub(Mutex<HashMap<usize, SessionJobs>>);
impl JobHub {
    pub fn install(&self, mask: &Arc<MaskCounter>, registry: Arc<JobRegistry>) -> Arc<JobRegistry> {
        let mut entries = self.0.lock().unwrap();
        entries.retain(|_, session| session.mask.strong_count() > 0);
        let session = entries
            .entry(Arc::as_ptr(mask) as usize)
            .or_insert_with(|| SessionJobs {
                mask: Arc::downgrade(mask),
                registry: None,
                observer: None,
            });
        let registry = session.registry.get_or_insert(registry).clone();
        if let Some(observer) = &session.observer {
            registry.observe(observer.clone());
        }
        registry
    }
    pub fn get(&self, mask: &Arc<MaskCounter>) -> Option<Arc<JobRegistry>> {
        self.0
            .lock()
            .unwrap()
            .get(&(Arc::as_ptr(mask) as usize))
            .and_then(|session| session.registry.clone())
    }
    pub fn service(self: &Arc<Self>, mask: &Arc<MaskCounter>) -> Arc<dyn ProcessJobsService> {
        Arc::new(JobView {
            hub: self.clone(),
            mask: mask.clone(),
        })
    }
    pub fn bind(
        self: &Arc<Self>,
        mask: &Arc<MaskCounter>,
        inbox: p1_core::Inbox,
        log: Arc<ActivityLog>,
    ) -> JobGuard {
        let observer: Arc<dyn JobObserver> = Arc::new(Completion {
            inbox,
            log,
            secrets: mask.secrets().clone(),
        });
        let key = Arc::as_ptr(mask) as usize;
        let mut entries = self.0.lock().unwrap();
        let session = entries.entry(key).or_insert_with(|| SessionJobs {
            mask: Arc::downgrade(mask),
            registry: None,
            observer: None,
        });
        if let Some(jobs) = &session.registry {
            jobs.observe(observer.clone());
        }
        session.observer = Some(observer);
        JobGuard {
            hub: self.clone(),
            key,
        }
    }
    pub async fn shutdown(&self) {
        let registries = self
            .0
            .lock()
            .unwrap()
            .values()
            .filter_map(|session| session.registry.clone())
            .collect::<Vec<_>>();
        for registry in registries {
            registry.shutdown().await;
        }
    }
}
struct JobView {
    hub: Arc<JobHub>,
    mask: Arc<MaskCounter>,
}
impl ProcessJobsService for JobView {
    fn start(&self, _: String, _: Option<u64>) -> BoxFuture<'_, Result<String, JobError>> {
        Box::pin(async {
            Err(JobError::StartFailed(
                "start is granted only to shell".into(),
            ))
        })
    }
    fn status(&self, id: &str) -> Result<JobState, JobError> {
        self.hub
            .get(&self.mask)
            .ok_or(JobError::UnknownJob)?
            .status(id)
    }
    fn cancel<'a>(&'a self, id: &'a str) -> BoxFuture<'a, Result<JobState, JobError>> {
        Box::pin(async move {
            self.hub
                .get(&self.mask)
                .ok_or(JobError::UnknownJob)?
                .cancel(id)
                .await
        })
    }
    fn handed_over(&self) -> Option<String> {
        // `shell_job` inspects jobs; it did not start the call's command, so it has none.
        None
    }
}
struct Completion {
    inbox: p1_core::Inbox,
    log: Arc<ActivityLog>,
    secrets: p1_redact::SecretSet,
}
impl JobObserver for Completion {
    fn started(&self) -> Option<u64> {
        self.log.job_started()
    }
    fn ended(&self, command: &str, baseline: Option<u64>, status: &p1_module_runtime::ExitStatus) {
        let exit = match status {
            p1_module_runtime::ExitStatus::Code(code) => Some(*code),
            _ => None,
        };
        self.log.job_finished(command.to_owned(), baseline, exit);
    }
    fn finished(&self, job: JobFinished) {
        let text = format!(
            "Background job {} ended\ncommand: {}\nexit status: {:?}\nelapsed: {} ms\noutput bytes: {}\noutput handle: {}\n{}",
            job.id,
            job.command,
            job.end.status,
            job.end.elapsed_ms,
            job.output_bytes,
            job.end.output,
            job.tail
        );
        self.inbox.send(
            p1_contracts::InboxKind::Notification,
            p1_redact::redact_with(&text, &self.secrets).text,
        );
    }
}
pub(crate) struct JobGuard {
    hub: Arc<JobHub>,
    key: usize,
}
impl Drop for JobGuard {
    fn drop(&mut self) {
        let session = self.hub.0.lock().unwrap().remove(&self.key);
        if let Some(jobs) = session.and_then(|session| session.registry) {
            jobs.cancel_all();
        }
    }
}

#[cfg(feature = "delegation")]
pub(crate) struct WorkerJobsReport {
    pub report: Arc<dyn Fn() -> p1_workers::WorkerReport + Send + Sync>,
    pub jobs: JobGuard,
}
#[cfg(feature = "delegation")]
impl p1_workers::ChildReport for WorkerJobsReport {
    fn snapshot(&self) -> p1_workers::WorkerReport {
        let _keep_jobs_until_child_drops = &self.jobs;
        (self.report)()
    }
    fn turn_ended(&self) -> BoxFuture<'_, Vec<String>> {
        Box::pin(async {
            let registry = self
                .jobs
                .hub
                .0
                .lock()
                .unwrap()
                .get(&self.jobs.key)
                .and_then(|session| session.registry.clone());
            match registry {
                Some(registry) => registry.cancel_running().await,
                None => Vec::new(),
            }
        })
    }
}

/// Shell holds the start side only; shell_job holds the inspection side. The second field
/// is the call's own handover slot, shared with its process capability, so concurrent shell
/// calls (ADR-0118) never answer `handed-over` with each other's job.
pub(crate) struct JobStarter(pub Arc<JobRegistry>, pub Handover);
impl ProcessJobsService for JobStarter {
    fn start(
        &self,
        script: String,
        timeout_ms: Option<u64>,
    ) -> BoxFuture<'_, Result<String, JobError>> {
        Box::pin(self.0.start(script, timeout_ms))
    }
    fn status(&self, _: &str) -> Result<JobState, JobError> {
        Err(JobError::UnknownJob)
    }
    fn cancel<'a>(&'a self, _: &'a str) -> BoxFuture<'a, Result<JobState, JobError>> {
        Box::pin(async { Err(JobError::UnknownJob) })
    }
    fn handed_over(&self) -> Option<String> {
        // The shell asks its own `process-jobs` service (this) which job it got (ADR-0123).
        self.1.get()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use p1_contracts::{CancellationToken, InboxKind, ModelOptions, RecordBody};
    use p1_core::{Agent, AgentParts};
    use p1_testkit::{
        PassthroughContext, RecordingEvents, RecordingJournal, ScriptedAuthorization,
        ScriptedProvider, text_response,
    };
    fn agent(journal: Arc<RecordingJournal>) -> Agent {
        Agent::new(AgentParts {
            provider: Arc::new(ScriptedProvider::new(vec![text_response("received")])),
            tools: vec![],
            system_prompt: String::new(),
            options: ModelOptions::default(),
            context: Arc::new(PassthroughContext),
            authorization: Arc::new(ScriptedAuthorization::permit_all()),
            journal,
            events: Arc::new(RecordingEvents::new()),
        })
        .unwrap()
    }
    #[cfg(feature = "delegation")]
    async fn worker_turn_settles_jobs(cancelled: bool) {
        use p1_workers::{ChildAgent, ChildStatus, InProcessWorkers, WorkerReport, WorkerService};
        let dir = tempfile::tempdir().unwrap();
        let hub = Arc::new(JobHub::default());
        let mask = Arc::new(MaskCounter::new());
        let jobs = hub.install(
            &mask,
            Arc::new(JobRegistry::new(
                Arc::new(p1_module_runtime::process::ProcessService::new(dir.path())),
                Arc::new(p1_module_runtime::OutputStore::in_directory(
                    dir.path().join("outputs"),
                    Default::default(),
                )),
                Default::default(),
            )),
        );
        jobs.start("echo $$ > group; tail -f /dev/null & wait".into(), None)
            .await
            .unwrap();
        let group = dir.path().join("group");
        let pgid: i32 = tokio::time::timeout(std::time::Duration::from_secs(30), async {
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
        let provider = Arc::new(ScriptedProvider::new(if cancelled {
            vec![p1_testkit::Step::EventsThenAwaitCancel(vec![])]
        } else {
            vec![text_response("done")]
        }));
        let child = Agent::new(AgentParts {
            provider,
            tools: vec![],
            system_prompt: String::new(),
            options: ModelOptions::default(),
            context: Arc::new(PassthroughContext),
            authorization: Arc::new(ScriptedAuthorization::permit_all()),
            journal: Arc::new(RecordingJournal::new()),
            events: Arc::new(RecordingEvents::new()),
        })
        .unwrap();
        let guard = hub.bind(&mask, child.inbox(), Arc::new(ActivityLog::default()));
        let child = ChildAgent {
            agent: child,
            description: "test".into(),
            regrant: None,
            fallback: None,
            report: Arc::new(WorkerJobsReport {
                report: Arc::new(WorkerReport::default),
                jobs: guard,
            }),
        };
        let workers = InProcessWorkers::new(Arc::new(|_| Err("unused".into())), 1);
        let id = workers
            .start_prepared(
                p1_workers::PreparedStart {
                    task: "work".into(),
                    ..Default::default()
                },
                move |_| Ok(child),
            )
            .await
            .unwrap();
        if cancelled {
            workers.cancel(&id).await.unwrap();
        }
        let status = workers.wait(&id, CancellationToken::new()).await.unwrap();
        let evidence = format!("{status:?}");
        let job_end = jobs.status("j1").unwrap();
        let probe = std::process::Command::new("kill")
            .args(["-0", "--", &format!("-{pgid}")])
            .output()
            .unwrap();
        jobs.shutdown().await;
        workers.shutdown().await;
        assert!(
            matches!(
                job_end,
                JobState::Ended(p1_module_runtime::jobs::JobEnd {
                    status: p1_module_runtime::ExitStatus::Cancelled,
                    ..
                })
            ),
            "job must end before result publication: {job_end:?}"
        );
        assert!(
            !probe.status.success(),
            "process group survives worker result"
        );
        assert!(
            evidence.contains("j1"),
            "result must name cancelled job: {evidence}"
        );
        if cancelled {
            assert!(
                matches!(status, ChildStatus::Finished(ref result) if result.turn_end == p1_contracts::TurnEnd::Cancelled)
            );
        }
        assert!(jobs.start("true".into(), None).await.is_err());
    }
    #[cfg(feature = "delegation")]
    #[tokio::test]
    async fn worker_completed_cancels_background_jobs_before_result() {
        worker_turn_settles_jobs(false).await;
    }
    #[cfg(feature = "delegation")]
    #[tokio::test]
    async fn worker_cancelled_cancels_background_jobs_before_result() {
        worker_turn_settles_jobs(true).await;
    }

    #[tokio::test]
    async fn background_late_shell_grant_binds_inbox_and_drop_kills_job() {
        let dir = tempfile::tempdir().unwrap();
        let hub = Arc::new(JobHub::default());
        let mask = Arc::new(MaskCounter::new());
        let owner = agent(Arc::new(RecordingJournal::new()));
        // A worker can acquire shell on a later grant, after its session was bound.
        let guard = hub.bind(&mask, owner.inbox(), Arc::new(ActivityLog::default()));
        let jobs = hub.install(
            &mask,
            Arc::new(JobRegistry::new(
                Arc::new(p1_module_runtime::process::ProcessService::new(dir.path())),
                Arc::new(p1_module_runtime::OutputStore::in_directory(
                    dir.path().join("outputs"),
                    Default::default(),
                )),
                p1_redact::SecretSet::new(),
            )),
        );
        jobs.start("printf late".into(), None).await.unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(30), owner.inbox_ready())
            .await
            .unwrap();
        assert!(matches!(jobs.status("j1"), Ok(JobState::Ended(_))));
        jobs.start("exec tail -f /dev/null".into(), None)
            .await
            .unwrap();
        drop(guard);
        assert!(hub.get(&mask).is_none());
        jobs.shutdown().await;
        assert!(matches!(
            jobs.status("j2"),
            Ok(JobState::Ended(p1_module_runtime::jobs::JobEnd {
                status: p1_module_runtime::ExitStatus::Cancelled,
                ..
            }))
        ));
    }
    #[tokio::test]
    async fn background_inbox_is_owning_agent_only_and_guard_retires_session() {
        let dir = tempfile::tempdir().unwrap();
        let hub = Arc::new(JobHub::default());
        let parent = Arc::new(MaskCounter::new());
        let child = Arc::new(MaskCounter::new());
        let jobs = hub.install(
            &parent,
            Arc::new(JobRegistry::new(
                Arc::new(p1_module_runtime::process::ProcessService::new(dir.path())),
                Arc::new(p1_module_runtime::OutputStore::in_directory(
                    dir.path().join("outputs"),
                    Default::default(),
                )),
                p1_redact::SecretSet::new(),
            )),
        );
        let journal = Arc::new(RecordingJournal::new());
        let mut owner = agent(journal.clone());
        let other = agent(Arc::new(RecordingJournal::new()));
        let log = Arc::new(ActivityLog::default());
        let guard = hub.bind(&parent, owner.inbox(), log.clone());
        assert_eq!(
            hub.service(&child).cancel("j1").await,
            Err(JobError::UnknownJob)
        );
        let id = jobs.start("printf output".into(), None).await.unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(30), owner.inbox_ready())
            .await
            .unwrap();
        let end = jobs.status(&id).unwrap();
        assert!(matches!(end, JobState::Ended(_)));
        assert!(!other.has_pending_inbox());
        owner
            .run_inbox_turn(CancellationToken::new())
            .await
            .unwrap();
        let records = journal.records();
        let notifications = records
            .iter()
            .filter_map(|record| match &record.body {
                RecordBody::Inbox { kind, text } if *kind == InboxKind::Notification => Some(text),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(notifications.len(), 1);
        let JobState::Ended(end) = end else {
            unreachable!()
        };
        assert!(notifications[0].contains(&end.output));
        assert!(notifications[0].contains("Code(0)"));
        assert!(notifications[0].ends_with("output"));
        assert_eq!(log.evidence_runs().last().unwrap().exit_code, Some(0));
        assert!(!owner.has_pending_inbox());
        drop(guard);
        assert!(hub.get(&parent).is_none());
        assert!(jobs.start("true".into(), None).await.is_err());
        jobs.shutdown().await;
    }
}
