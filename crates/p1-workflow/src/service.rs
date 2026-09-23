//! `InProcessWorkflows`: the `WorkflowService` that runs scripts in this process
//! (ADR-0053 item 1). Each run gets a directory, a journal, a script thread and a status
//! watch; runs are retained for the service's lifetime.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, AtomicUsize};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::JoinHandle;

use p1_contracts::{BoxFuture, CancellationToken};
use serde_json::Value;
use tokio::runtime::Handle;
use tokio::sync::watch;

use crate::api::{
    JournalRecord, ModelResolver, RunId, RunReport, RunStatus, StartRequest, StepRunner,
    WorkflowError, WorkflowObserver, WorkflowService, WorkflowSettings,
};
use crate::caps::CapCounter;
use crate::engine::{self, Role, RunState, forbidden_tool};
use crate::journal::{JournalWriter, Replay, dispatch_charges, read_journal, script_hash};

pub struct InProcessWorkflows {
    runner: Arc<dyn StepRunner>,
    resolver: Arc<dyn ModelResolver>,
    observer: Arc<dyn WorkflowObserver>,
    settings: WorkflowSettings,
    run_root: PathBuf,
    inner: Mutex<Inner>,
}

struct Inner {
    /// The highest `wf<N>` seen or allocated; the next run is `N + 1`.
    last_number: u64,
    runs: Vec<RunEntry>,
    shut_down: bool,
}

struct RunEntry {
    state: Arc<RunState>,
    thread: Option<JoinHandle<()>>,
}

impl InProcessWorkflows {
    /// `run_root` is where every run gets its directory `run_root/<run id>/`. Numbering
    /// continues after the highest `wf<N>` already there, so ids stay unique across
    /// processes and `resume_from: wf3` always names `run_root/wf3`.
    pub fn new(
        runner: Arc<dyn StepRunner>,
        resolver: Arc<dyn ModelResolver>,
        observer: Arc<dyn WorkflowObserver>,
        settings: WorkflowSettings,
        run_root: PathBuf,
    ) -> Arc<Self> {
        let last_number = highest_run_number(&run_root);
        Arc::new(Self {
            runner,
            resolver,
            observer,
            settings,
            run_root,
            inner: Mutex::new(Inner {
                last_number,
                runs: Vec::new(),
                shut_down: false,
            }),
        })
    }

    /// Cancel every running run (each journals `Ended { outcome: Cancelled }`), join the
    /// script threads. Afterwards every fallible call returns `ShutDown`.
    pub async fn shutdown(&self) {
        let threads: Vec<JoinHandle<()>> = {
            let mut inner = self.lock();
            inner.shut_down = true;
            inner
                .runs
                .iter_mut()
                .filter_map(|entry| {
                    entry.state.token.cancel();
                    entry.thread.take()
                })
                .collect()
        };
        for thread in threads {
            // Joining blocks; keep it off the async worker.
            let _ = tokio::task::spawn_blocking(move || thread.join()).await;
        }
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
    }

    fn find(&self, id: &RunId) -> Result<Arc<RunState>, WorkflowError> {
        let inner = self.lock();
        if inner.shut_down {
            return Err(WorkflowError::ShutDown);
        }
        inner
            .runs
            .iter()
            .find(|entry| entry.state.id == *id)
            .map(|entry| entry.state.clone())
            .ok_or(WorkflowError::UnknownRun)
    }

    /// The settings roles with `role_models` applied, each resolved. Every role is
    /// resolved, used by the script or not: a broken table fails before anything runs.
    fn resolve_roles(
        &self,
        role_models: &BTreeMap<String, String>,
    ) -> Result<BTreeMap<String, Role>, WorkflowError> {
        let preflight = WorkflowError::Preflight;
        let mut specs = self.settings.roles.clone();
        for (role, model) in role_models {
            let spec = specs
                .get_mut(role)
                .ok_or_else(|| preflight(format!("role_models names unknown role \"{role}\"")))?;
            spec.model = model.clone();
        }
        let mut roles = BTreeMap::new();
        for (name, spec) in specs {
            if spec.tools.is_empty() {
                return Err(preflight(format!(
                    "role \"{name}\" has an empty tool grant"
                )));
            }
            if let Some(tool) = spec.tools.iter().find(|tool| forbidden_tool(tool)) {
                return Err(preflight(format!(
                    "role \"{name}\" grants \"{tool}\", which a step may not have"
                )));
            }
            let model = self.resolver.resolve(&spec.model).map_err(|reason| {
                preflight(format!("role \"{name}\" ({}): {reason}", spec.model))
            })?;
            roles.insert(
                name,
                Role {
                    model,
                    tools: spec.tools,
                },
            );
        }
        Ok(roles)
    }

    fn start_now(&self, request: StartRequest) -> Result<RunId, WorkflowError> {
        let preflight = WorkflowError::Preflight;
        if self.lock().shut_down {
            return Err(WorkflowError::ShutDown);
        }
        let handle = Handle::try_current()
            .map_err(|_| preflight("start must be called inside a tokio runtime".into()))?;

        let engine = engine::sandboxed_engine();
        let ast = engine::compile(&engine, &request.script)?;
        let roles = self.resolve_roles(&request.role_models)?;
        let args = match request.args {
            Value::Null => Value::Object(serde_json::Map::new()),
            Value::Object(map) => Value::Object(map),
            other => return Err(preflight(format!("args must be an object, not {other}"))),
        };
        let args_dynamic =
            rhai::serde::to_dynamic(&args).map_err(|error| preflight(format!("args: {error}")))?;
        let (replay, charged) = match &request.resume_from {
            None => (Replay::none(), BTreeMap::new()),
            Some(from) => {
                let path = self.run_root.join(&from.0).join("journal.jsonl");
                if from.0.contains(['/', '\\']) || from.0.starts_with('.') || !path.is_file() {
                    return Err(preflight(format!(
                        "resume_from {}: no such run journal",
                        from.0
                    )));
                }
                let records = read_journal(&path).map_err(preflight)?;
                (
                    Replay::from_records(from.clone(), &records),
                    dispatch_charges(&records),
                )
            }
        };

        // From here on a run exists. The lock is held through the spawn so a concurrent
        // `shutdown` either refuses this start or sees and cancels the run.
        let mut inner = self.lock();
        if inner.shut_down {
            return Err(WorkflowError::ShutDown);
        }
        let (id, run_dir) = allocate_run_dir(&self.run_root, &mut inner.last_number)?;
        let io =
            |error: std::io::Error| WorkflowError::Io(format!("{}: {error}", run_dir.display()));
        std::fs::write(run_dir.join("script.rhai"), &request.script).map_err(io)?;
        let args_text = serde_json::to_vec_pretty(&args)
            .map_err(|error| WorkflowError::Io(error.to_string()))?;
        std::fs::write(run_dir.join("args.json"), args_text).map_err(io)?;
        let journal = JournalWriter::create(&run_dir.join("journal.jsonl")).map_err(io)?;
        journal
            .append(&JournalRecord::Started {
                run: id.clone(),
                script_hash: script_hash(&request.script),
                args,
                resumed_from: request.resume_from.clone(),
            })
            .map_err(io)?;

        let (ended, _) = watch::channel(None);
        let state = Arc::new(RunState {
            id: id.clone(),
            run_dir: run_dir.clone(),
            resumed_from: request.resume_from.clone(),
            runner: self.runner.clone(),
            observer: self.observer.clone(),
            roles,
            caps: CapCounter::new(self.settings.caps.clone(), charged),
            max_steps: self.settings.max_steps,
            workspace: request.workspace,
            token: CancellationToken::new(),
            handle,
            journal,
            replay: Mutex::new(replay),
            free_threads: AtomicUsize::new(self.settings.max_threads),
            calls: AtomicU32::new(0),
            record: Mutex::default(),
            ended,
        });
        let script = engine::prepare(engine, ast, state.clone());
        self.observer.run_started(&id, state.resumed_from.as_ref());
        let thread = std::thread::Builder::new()
            .name("p1-wf-script".to_string())
            .spawn(move || engine::execute(script, args_dynamic));
        match thread {
            Ok(thread) => {
                inner.runs.push(RunEntry {
                    state,
                    thread: Some(thread),
                });
                Ok(id)
            }
            Err(error) => {
                let message = format!("cannot start the script thread: {error}");
                state.end(
                    Value::Null,
                    Some((crate::api::RunOutcome::Failed, message.clone())),
                );
                Err(WorkflowError::Io(message))
            }
        }
    }
}

fn highest_run_number(run_root: &Path) -> u64 {
    let Ok(entries) = std::fs::read_dir(run_root) else {
        return 0;
    };
    entries
        .filter_map(|entry| entry.ok())
        .filter_map(|entry| run_number(&entry.file_name().to_string_lossy()))
        .max()
        .unwrap_or(0)
}

fn run_number(name: &str) -> Option<u64> {
    let digits = name.strip_prefix("wf")?;
    if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    digits.parse().ok()
}

/// `create_dir`, not `create_dir_all`, for the run itself: another process that took the
/// same number makes it fail, and the next number is tried.
fn allocate_run_dir(run_root: &Path, last: &mut u64) -> Result<(RunId, PathBuf), WorkflowError> {
    std::fs::create_dir_all(run_root)
        .map_err(|error| WorkflowError::Io(format!("{}: {error}", run_root.display())))?;
    loop {
        *last += 1;
        let id = format!("wf{last}");
        let dir = run_root.join(&id);
        match std::fs::create_dir(&dir) {
            Ok(()) => return Ok((RunId(id), dir)),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(WorkflowError::Io(format!("{}: {error}", dir.display())));
            }
        }
    }
}

fn status_of(state: &RunState) -> RunStatus {
    match state.ended.borrow().as_ref() {
        Some(report) => RunStatus::Ended(report.clone()),
        None => RunStatus::Running(state.progress()),
    }
}

/// Resolves once the run has ended, or `None` when `cancel` fires first.
async fn ended(state: &RunState, cancel: &CancellationToken) -> Option<RunReport> {
    // Subscribe BEFORE reading: an end landing between the two marks the watch changed,
    // so no wake-up is lost.
    let mut rx = state.ended.subscribe();
    loop {
        if let Some(report) = rx.borrow_and_update().as_ref() {
            return Some(report.clone());
        }
        tokio::select! {
            biased;
            _ = cancel.cancelled() => return None,
            changed = rx.changed() => {
                // The state owns the sender, and we hold the state.
                if changed.is_err() {
                    return rx.borrow().clone();
                }
            }
        }
    }
}

impl WorkflowService for InProcessWorkflows {
    fn start<'a>(&'a self, request: StartRequest) -> BoxFuture<'a, Result<RunId, WorkflowError>> {
        Box::pin(async move { self.start_now(request) })
    }

    fn status<'a>(&'a self, id: &'a RunId) -> BoxFuture<'a, Result<RunStatus, WorkflowError>> {
        Box::pin(async move {
            let state = self.find(id)?;
            Ok(status_of(&state))
        })
    }

    fn wait<'a>(
        &'a self,
        id: &'a RunId,
        cancel: CancellationToken,
    ) -> BoxFuture<'a, Result<RunStatus, WorkflowError>> {
        Box::pin(async move {
            let state = self.find(id)?;
            Ok(match ended(&state, &cancel).await {
                Some(report) => RunStatus::Ended(report),
                None => status_of(&state),
            })
        })
    }

    fn cancel<'a>(&'a self, id: &'a RunId) -> BoxFuture<'a, Result<(), WorkflowError>> {
        Box::pin(async move {
            let state = self.find(id)?;
            state.token.cancel();
            // A spinning script dies within microseconds and a blocked step is dropped
            // at once, so returning only once `Ended` is journalled costs nothing.
            ended(&state, &CancellationToken::new()).await;
            Ok(())
        })
    }

    fn list<'a>(&'a self) -> BoxFuture<'a, Vec<(RunId, RunStatus)>> {
        Box::pin(async move {
            let states: Vec<Arc<RunState>> = self
                .lock()
                .runs
                .iter()
                .map(|entry| entry.state.clone())
                .collect();
            states
                .iter()
                .map(|state| (state.id.clone(), status_of(state)))
                .collect()
        })
    }
}
