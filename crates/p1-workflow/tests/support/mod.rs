//! Test support shared by the integration tests: a scripted `StepRunner`, a table
//! resolver, a recording observer and scratch run roots.
#![allow(dead_code)]

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use p1_contracts::{BoxFuture, CancellationToken};
use p1_workflow::{
    InProcessWorkflows, JournalRecord, ModelResolver, ResolvedModel, RoleSpec, RunId, RunReport,
    RunStatus, SchemaCheck, StartRequest, StepEnd, StepLine, StepOutcome, StepRequest, StepRunner,
    WorkerRef, WorkflowObserver, WorkflowService, WorkflowSettings, WorktreeHold, WorktreeInfo,
};
use serde_json::Value;
use tokio::sync::Notify;

// ------------------------------------------------------------------ scratch directories

/// A fresh directory under the system temp dir, removed on drop.
pub struct Scratch(pub PathBuf);

impl Scratch {
    pub fn new() -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let dir = std::env::temp_dir().join(format!(
            "p1-workflow-test-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::SeqCst)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }

    pub fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

// ------------------------------------------------------------------ the scripted runner

/// A step the test holds: `reached` fires when the runner gets it, `release` lets it
/// finish, `dropped` is set when the engine dropped the step's future.
pub struct Hold {
    pub reached: Notify,
    pub release: Notify,
    pub dropped: AtomicBool,
}

/// Marks the hold `dropped` unless the step got to finish.
struct DropFlag {
    hold: Arc<Hold>,
    finished: bool,
}

impl Drop for DropFlag {
    fn drop(&mut self) {
        if !self.finished {
            self.hold.dropped.store(true, Ordering::SeqCst);
        }
    }
}

#[derive(Default)]
struct Script {
    ends: HashMap<String, VecDeque<Result<StepEnd, String>>>,
    repairs: HashMap<String, VecDeque<Result<StepEnd, String>>>,
    holds: HashMap<String, Arc<Hold>>,
    requests: Vec<StepRequest>,
    repair_messages: Vec<(WorkerRef, String)>,
    workers: usize,
    /// Every `worktree` request, in order.
    worktree_requests: Vec<StepRequest>,
}

/// Maps a prompt to queued outcomes; a prompt with nothing queued ends `done` with the
/// summary `did: <prompt>` and passing evidence.
#[derive(Default)]
pub struct ScriptedRunner {
    script: Mutex<Script>,
    /// Steps in flight on engine thunk threads, and the most seen at once.
    live_thunks: AtomicUsize,
    pub max_live_thunks: AtomicUsize,
    /// Worktree slugs held right now: a fake hold removes its slug when dropped.
    pub held_worktrees: Arc<Mutex<Vec<String>>>,
}

/// The fake worktree a `ScriptedRunner` hands out: `/fake-worktrees/<slug>` on
/// `task/<slug>`, head `prepared-<slug>` until the step ends, `ended-<slug>` after.
struct FakeHold {
    info: WorktreeInfo,
    held: Arc<Mutex<Vec<String>>>,
    slug: String,
}

impl WorktreeHold for FakeHold {
    fn info(&self) -> &WorktreeInfo {
        &self.info
    }

    fn settle<'a>(&'a self) -> BoxFuture<'a, Result<WorktreeInfo, String>> {
        Box::pin(async move {
            Ok(WorktreeInfo {
                head: format!("ended-{}", self.slug),
                ..self.info.clone()
            })
        })
    }
}

impl Drop for FakeHold {
    fn drop(&mut self) {
        self.held.lock().unwrap().retain(|slug| *slug != self.slug);
    }
}

pub fn done(summary: &str) -> StepEnd {
    StepEnd::Done {
        summary: summary.to_string(),
        evidence: "commands passed: cargo test".to_string(),
        result: None,
        schema: SchemaCheck::NotRequested,
    }
}

pub fn done_with(result: Value, schema: SchemaCheck) -> StepEnd {
    StepEnd::Done {
        summary: "structured".to_string(),
        evidence: "commands passed: cargo test".to_string(),
        result: Some(result),
        schema,
    }
}

impl ScriptedRunner {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    pub fn queue(&self, prompt: &str, end: StepEnd) {
        self.queue_result(prompt, Ok(end));
    }

    pub fn queue_result(&self, prompt: &str, end: Result<StepEnd, String>) {
        let mut script = self.script.lock().unwrap();
        script
            .ends
            .entry(prompt.to_string())
            .or_default()
            .push_back(end);
    }

    /// A repair of the worker that ran `prompt`.
    pub fn queue_repair(&self, prompt: &str, end: StepEnd) {
        self.queue_repair_result(prompt, Ok(end));
    }

    /// A repair that ends another way: a route failure, or the runner's own `Err`.
    pub fn queue_repair_result(&self, prompt: &str, end: Result<StepEnd, String>) {
        let mut script = self.script.lock().unwrap();
        script
            .repairs
            .entry(prompt.to_string())
            .or_default()
            .push_back(end);
    }

    pub fn hold(&self, prompt: &str) -> Arc<Hold> {
        let hold = Arc::new(Hold {
            reached: Notify::new(),
            release: Notify::new(),
            dropped: AtomicBool::new(false),
        });
        self.script
            .lock()
            .unwrap()
            .holds
            .insert(prompt.to_string(), hold.clone());
        hold
    }

    pub fn requests(&self) -> Vec<StepRequest> {
        self.script.lock().unwrap().requests.clone()
    }

    pub fn prompts(&self) -> Vec<String> {
        self.requests()
            .into_iter()
            .map(|request| request.prompt)
            .collect()
    }

    pub fn repair_messages(&self) -> Vec<(WorkerRef, String)> {
        self.script.lock().unwrap().repair_messages.clone()
    }

    pub fn worktree_requests(&self) -> Vec<StepRequest> {
        self.script.lock().unwrap().worktree_requests.clone()
    }
}

/// Worker ids carry the prompt so a repair finds its queue: `w<n>|<prompt>`.
fn prompt_of(worker: &WorkerRef) -> String {
    worker
        .id
        .split_once('|')
        .map(|(_, prompt)| prompt.to_string())
        .unwrap_or_default()
}

impl StepRunner for ScriptedRunner {
    fn run<'a>(
        &'a self,
        request: &'a StepRequest,
        _cancel: CancellationToken,
    ) -> BoxFuture<'a, Result<StepOutcome, String>> {
        Box::pin(async move {
            let on_thunk = std::thread::current().name() == Some("p1-wf-thunk");
            if on_thunk {
                let live = self.live_thunks.fetch_add(1, Ordering::SeqCst) + 1;
                self.max_live_thunks.fetch_max(live, Ordering::SeqCst);
            }
            let (end, hold, worker) = {
                let mut script = self.script.lock().unwrap();
                script.requests.push(request.clone());
                script.workers += 1;
                let worker = WorkerRef {
                    id: format!("w{}|{}", script.workers, request.prompt),
                    description: format!(
                        "{}/{}",
                        request.model.environment, request.model.wire_model
                    ),
                };
                let end = script
                    .ends
                    .get_mut(&request.prompt)
                    .and_then(VecDeque::pop_front)
                    .unwrap_or_else(|| Ok(done(&format!("did: {}", request.prompt))));
                (end, script.holds.get(&request.prompt).cloned(), worker)
            };
            // Let sibling thunks reach the runner too, so the thread bound is exercised.
            for _ in 0..8 {
                tokio::task::yield_now().await;
            }
            if let Some(hold) = hold {
                let mut flag = DropFlag {
                    hold: hold.clone(),
                    finished: false,
                };
                hold.reached.notify_one();
                hold.release.notified().await;
                flag.finished = true;
            }
            if on_thunk {
                self.live_thunks.fetch_sub(1, Ordering::SeqCst);
            }
            end.map(|end| StepOutcome { worker, end })
        })
    }

    fn repair<'a>(
        &'a self,
        worker: &'a WorkerRef,
        message: String,
        _cancel: CancellationToken,
    ) -> BoxFuture<'a, Result<StepEnd, String>> {
        Box::pin(async move {
            let mut script = self.script.lock().unwrap();
            script.repair_messages.push((worker.clone(), message));
            script
                .repairs
                .get_mut(&prompt_of(worker))
                .and_then(VecDeque::pop_front)
                .unwrap_or_else(|| Err("no repair queued".to_string()))
        })
    }

    fn worktree<'a>(
        &'a self,
        request: &'a StepRequest,
    ) -> BoxFuture<'a, Result<Box<dyn WorktreeHold>, String>> {
        Box::pin(async move {
            self.script
                .lock()
                .unwrap()
                .worktree_requests
                .push(request.clone());
            let slug = request.worktree.clone().unwrap_or_default();
            let mut held = self.held_worktrees.lock().unwrap();
            if held.contains(&slug) {
                return Err(format!("worktree_busy: {slug}"));
            }
            held.push(slug.clone());
            Ok(Box::new(FakeHold {
                info: WorktreeInfo {
                    path: PathBuf::from(format!("/fake-worktrees/{slug}")),
                    branch: format!("task/{slug}"),
                    head: format!("prepared-{slug}"),
                },
                held: self.held_worktrees.clone(),
                slug,
            }) as Box<dyn WorktreeHold>)
        })
    }
}

// ------------------------------------------------------------------ resolver, observer

/// Resolves `env/profile[:effort]`; the wire model is the profile, so two profiles of
/// one model need a table entry: `claude/fable-judge` resolves to `claude-fable-5`.
pub struct TableResolver;

impl ModelResolver for TableResolver {
    fn resolve(&self, reference: &str) -> Result<ResolvedModel, String> {
        let (environment, rest) = reference
            .split_once('/')
            .ok_or_else(|| format!("not environment/profile: {reference}"))?;
        if environment == "nowhere" {
            return Err(format!("unknown environment \"{environment}\""));
        }
        let (profile, effort) = match rest.split_once(':') {
            Some((profile, effort)) => (profile, Some(effort.to_string())),
            None => (rest, None),
        };
        let wire_model = match profile {
            "fable-judge" => "claude-fable-5",
            other => other,
        };
        Ok(ResolvedModel {
            reference: reference.to_string(),
            environment: environment.to_string(),
            profile: profile.to_string(),
            effort,
            wire_model: wire_model.to_string(),
        })
    }
}

#[derive(Default)]
pub struct Recorder {
    pub ended: Mutex<Vec<RunReport>>,
    pub steps: Mutex<Vec<StepLine>>,
    pub logs: Mutex<Vec<String>>,
    /// Fires on every log line: how a test knows the script got somewhere.
    pub logged: Notify,
    /// Fires on `run_ended`, which comes AFTER the status a `wait` returns is stored.
    pub run_ended: Notify,
    /// Every `thunk_failed` error, and a notification per failure.
    pub thunk_errors: Mutex<Vec<String>>,
    pub thunk_failed: Notify,
}

impl WorkflowObserver for Recorder {
    fn log(&self, _id: &RunId, text: &str) {
        self.logs.lock().unwrap().push(text.to_string());
        self.logged.notify_one();
    }
    fn step_ended(&self, _id: &RunId, line: &StepLine) {
        self.steps.lock().unwrap().push(line.clone());
    }
    fn thunk_failed(&self, _id: &RunId, error: &str) {
        self.thunk_errors.lock().unwrap().push(error.to_string());
        self.thunk_failed.notify_one();
    }
    fn run_ended(&self, _id: &RunId, report: &RunReport) {
        self.ended.lock().unwrap().push(report.clone());
        self.run_ended.notify_one();
    }
}

// ------------------------------------------------------------------ harness

pub fn settings() -> WorkflowSettings {
    let role = |model: &str, tools: &[&str]| RoleSpec {
        model: model.to_string(),
        // No fallback by default (ADR-0054 item 2): a test that wants a chain sets one.
        fallback: Vec::new(),
        tools: tools.iter().map(|tool| tool.to_string()).collect(),
    };
    let mut roles = BTreeMap::new();
    roles.insert(
        "worker".to_string(),
        role("claude/claude-opus-5-5", &["read", "edit", "shell"]),
    );
    roles.insert(
        "reviewer".to_string(),
        role("claude/claude-opus-5-5:high", &["read", "grep"]),
    );
    roles.insert(
        "judge".to_string(),
        role("claude/claude-fable-5", &["read"]),
    );
    roles.insert(
        "second_judge".to_string(),
        role("claude/fable-judge", &["read"]),
    );
    WorkflowSettings {
        roles,
        caps: BTreeMap::from([("claude-fable-5".to_string(), 3)]),
        max_steps: 200,
        max_threads: 8,
    }
}

pub struct Harness {
    pub runner: Arc<ScriptedRunner>,
    pub recorder: Arc<Recorder>,
    pub service: Arc<InProcessWorkflows>,
    pub root: Scratch,
}

impl Harness {
    pub fn new() -> Self {
        Self::with(settings())
    }

    pub fn with(settings: WorkflowSettings) -> Self {
        Self::in_root(settings, Scratch::new(), ScriptedRunner::new())
    }

    /// Another service over the same run root and runner, as a second process would be.
    pub fn in_root(settings: WorkflowSettings, root: Scratch, runner: Arc<ScriptedRunner>) -> Self {
        let recorder = Arc::new(Recorder::default());
        let service = InProcessWorkflows::new(
            runner.clone(),
            Arc::new(TableResolver),
            recorder.clone(),
            settings,
            root.path().to_path_buf(),
        );
        Self {
            runner,
            recorder,
            service,
            root,
        }
    }

    pub async fn start(&self, script: &str) -> RunId {
        self.start_request(request(script)).await
    }

    pub async fn start_request(&self, request: StartRequest) -> RunId {
        self.service.start(request).await.expect("start")
    }

    pub async fn wait(&self, id: &RunId) -> RunReport {
        match self
            .service
            .wait(id, CancellationToken::new())
            .await
            .expect("wait")
        {
            RunStatus::Ended(report) => report,
            RunStatus::Running(progress) => panic!("still running: {progress:?}"),
        }
    }

    /// Start and wait.
    pub async fn run(&self, script: &str) -> RunReport {
        let id = self.start(script).await;
        self.wait(&id).await
    }

    pub fn journal(&self, id: &RunId) -> Vec<JournalRecord> {
        journal(&self.root.path().join(&id.0))
    }
}

pub fn request(script: &str) -> StartRequest {
    StartRequest {
        script: script.to_string(),
        args: Value::Null,
        resume_from: None,
        role_models: BTreeMap::new(),
        workspace: None,
        base: None,
    }
}

pub fn journal(run_dir: &Path) -> Vec<JournalRecord> {
    std::fs::read_to_string(run_dir.join("journal.jsonl"))
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

pub fn kinds(records: &[JournalRecord]) -> Vec<&'static str> {
    records
        .iter()
        .map(|record| match record {
            JournalRecord::Started { .. } => "started",
            JournalRecord::Phase { .. } => "phase",
            JournalRecord::Dispatch { .. } => "dispatch",
            JournalRecord::Capped { .. } => "capped",
            JournalRecord::Fallback { .. } => "fallback",
            JournalRecord::Replayed { .. } => "replayed",
            JournalRecord::Result { .. } => "result",
            JournalRecord::Ended { .. } => "ended",
        })
        .collect()
}
