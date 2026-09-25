//! Workflows composed into the host (ADR-0053 items 2, 4, 7): the host's side of the
//! `p1-workflow` seams. Roles resolve through the host's environments and routes, a step
//! is a worker of the SAME service and built by the SAME child builder as a direct
//! `worker_start`, and a run's lines and its one end notification go through the front
//! end and the parent's inbox.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use p1_contracts::{BoxFuture, CancellationToken, InboxKind};
use p1_core::Inbox;
use p1_tool_finish::{FinishOutcome, OutputContract};
use p1_workers::{
    ChildId, ChildStatus, InProcessWorkers, PreparedStart, WorkerError, WorkerService,
};
use p1_workflow::{
    InProcessWorkflows, ModelResolver, ResolvedModel, RunId, RunReport, RunStatus, SchemaCheck,
    StartRequest, StepEnd, StepLine, StepOutcome, StepRequest, StepRunner, StepStatus, WorkerRef,
    WorkflowError, WorkflowObserver, WorkflowService, WorktreeHold,
};

use crate::HostDeps;
use crate::catalog::build_catalog;
use crate::child_assembly::{ChildBuilder, TurnEndCell, compose_children};
use crate::cli::{self, Options};
use crate::frontend::{
    FrontEnd, LineFrontEnd, WorkflowRunEnded, WorkflowRunStarted, WorkflowStepEnded,
    WorkflowStepStarted,
};
use crate::run::{
    EXIT_CANCELLED, EXIT_FAILURE, EXIT_OK, EXIT_USAGE, RunError, resolve_workspace,
    spawn_interrupt, write_stderr, write_stdout,
};

/// The evidence of a `done` whose outcome established none (ADR-0051 item 3).
const NOT_VERIFIED: &str = "not verified; parent verification required";

// ------------------------------------------------------------------ roles (item 4)

/// Resolves a role's `environment/profile[:effort]` against the host's environments and
/// routes. The wire model is what the route binds the profile to, because caps count
/// the model actually served, whichever environment or profile named it.
pub struct HostModelResolver {
    pub environment_dirs: Vec<PathBuf>,
}

impl ModelResolver for HostModelResolver {
    fn resolve(&self, reference: &str) -> Result<ResolvedModel, String> {
        // A bare profile would resolve against "the current environment", and a
        // settings file has none: a role must say which environment it means.
        let pair = reference
            .split_once(':')
            .map_or(reference, |(pair, _)| pair);
        if !pair.contains('/') {
            return Err(format!(
                "a role names environment/profile[:effort], got \"{reference}\""
            ));
        }
        let models = crate::models::enumerate(&self.environment_dirs)?;
        let resolved = crate::models::resolve(reference, "", &models)?;
        let environment =
            p1_assembly::load_environment(&resolved.environment, &self.environment_dirs)
                .map_err(|error| error.to_string())?;
        let route = crate::routes::load_route_by_id(&self.environment_dirs, &environment.provider)?;
        let wire_model = route.binding(&resolved.profile)?.wire_model.clone();
        Ok(ResolvedModel {
            reference: reference.to_string(),
            environment: resolved.environment,
            profile: resolved.profile,
            effort: resolved
                .effort
                .map(|effort| crate::models::effort_name(effort).to_string()),
            wire_model,
        })
    }
}

// ------------------------------------------------------------------ steps (item 2)

/// Starts each step as a worker of the host's service through the prepared seam, so
/// steps and direct workers share one concurrency bound and one id sequence.
pub struct HostStepRunner {
    builder: Arc<ChildBuilder>,
    service: Arc<InProcessWorkers>,
    /// What the runner keeps about one step worker: the `finish` outcome cell the
    /// structured result is read from after the start AND after a repair (another turn
    /// of the same worker), the model the step ran on — the chain's fallback needs it —
    /// and the worker's last turn end (ADR-0054 item 3).
    workers: Mutex<HashMap<String, StepWorker>>,
    /// What a blocked step asked for, by worker id: the step line carries no `needs`,
    /// and the observer words the line.
    needs: Arc<Mutex<HashMap<String, String>>>,
    /// The worktrees running steps hold (ADR-0073 item 4).
    worktrees: Arc<crate::worktree::Worktrees>,
}

/// One step worker, as its runner keeps it.
struct StepWorker {
    outcome: FinishOutcome,
    /// The `environment/profile[:effort]` reference this worker ran on.
    model: String,
    turn_end: TurnEndCell,
}

impl HostStepRunner {
    fn not_started_cancelled() -> StepOutcome {
        StepOutcome {
            worker: WorkerRef {
                id: "-".to_string(),
                description: "not started".to_string(),
            },
            end: StepEnd::Cancelled,
        }
    }

    /// One turn of `id` to its end. The engine drops this future when the run is
    /// cancelled, so the guard, not the cancel branch, is what stops the worker then.
    async fn turn(&self, id: &ChildId, cancel: CancellationToken) -> Result<StepEnd, String> {
        let mut guard = CancelOnDrop {
            service: Some(self.service.clone()),
            id: id.clone(),
        };
        let status = self
            .service
            .wait(id, cancel.clone())
            .await
            .map_err(|error| error.to_string())?;
        let end = if matches!(status, ChildStatus::Running) {
            // `wait` resolves `Running` only when `cancel` fired first.
            let _ = self.service.cancel(id).await;
            let _ = self.service.wait(id, CancellationToken::new()).await;
            StepEnd::Cancelled
        } else {
            self.step_end(id, status)
        };
        guard.service = None;
        Ok(end)
    }

    /// One worker's kept state, read under one lock: the `finish` outcome cell, the model
    /// it ran on and whether its last turn ended on a provider failure — the ROUTE's
    /// failure, and the one end that walks a fallback chain (ADR-0054 item 3).
    fn worker_state(
        &self,
        id: &ChildId,
    ) -> (Option<p1_tool_finish::StructuredResult>, String, bool) {
        let workers = self.workers.lock().unwrap();
        let Some(worker) = workers.get(&id.0) else {
            return (None, String::new(), false);
        };
        let route_failed = matches!(
            worker.turn_end.lock().unwrap().as_ref(),
            Some(p1_contracts::TurnEnd::ProviderFailed { .. })
        );
        (
            worker.outcome.structured(),
            worker.model.clone(),
            route_failed,
        )
    }

    fn step_end(&self, id: &ChildId, status: ChildStatus) -> StepEnd {
        let (structured, model, route_failed) = self.worker_state(id);
        match status {
            ChildStatus::Finished(result) => match &result.report.finish {
                Some(finish) if finish.status == "blocked" => {
                    let needs = finish.needs.clone().unwrap_or_default();
                    self.needs
                        .lock()
                        .unwrap()
                        .insert(id.0.clone(), needs.clone());
                    StepEnd::Blocked {
                        summary: finish.summary.clone().unwrap_or_default(),
                        needs,
                    }
                }
                Some(finish) => StepEnd::Done {
                    summary: finish.summary.clone().unwrap_or_default(),
                    evidence: finish
                        .evidence
                        .clone()
                        .unwrap_or_else(|| NOT_VERIFIED.to_string()),
                    result: structured.as_ref().and_then(|s| s.value.clone()),
                    schema: match structured.map(|s| s.schema) {
                        None | Some(p1_tool_finish::SchemaCheck::NotRequested) => {
                            SchemaCheck::NotRequested
                        }
                        Some(p1_tool_finish::SchemaCheck::Passed) => SchemaCheck::Passed,
                        Some(p1_tool_finish::SchemaCheck::Failed(errors)) => {
                            SchemaCheck::Failed(errors)
                        }
                    },
                },
                None => StepEnd::EndedWithoutFinish {
                    text: result.final_text,
                },
            },
            // A turn that ended on a provider failure is the ROUTE's failure, whatever
            // kind (ADR-0046's exhausted account included): the only failure a
            // fallback chain is for (ADR-0054 item 3). The stall guard's sentence, a
            // panic in the child's own task or a change of the worker's own grant are
            // NOT — they stay an ordinary `Failed`.
            ChildStatus::Failed(message) if route_failed => StepEnd::RouteFailed {
                model,
                error: message,
            },
            ChildStatus::Failed(message) => StepEnd::Failed(message),
            ChildStatus::Cancelled | ChildStatus::Running => StepEnd::Cancelled,
        }
    }
}

/// Cancels a step worker whose turn is abandoned: the engine drops a step's future
/// when its run is cancelled, and a dropped `wait` alone would leave the worker running.
struct CancelOnDrop {
    service: Option<Arc<InProcessWorkers>>,
    id: ChildId,
}

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        let Some(service) = self.service.take() else {
            return;
        };
        let id = self.id.clone();
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(async move {
                let _ = service.cancel(&id).await;
            });
        }
    }
}

impl StepRunner for HostStepRunner {
    fn run<'a>(
        &'a self,
        request: &'a StepRequest,
        cancel: CancellationToken,
    ) -> BoxFuture<'a, Result<StepOutcome, String>> {
        Box::pin(async move {
            // A malformed schema or effort fails the step before any worker exists.
            let contract = request
                .schema
                .clone()
                .map(OutputContract::new)
                .transpose()?;
            let effort = request
                .model
                .effort
                .as_deref()
                .map(crate::models::parse_effort)
                .transpose()?;
            let choice = crate::models::Choice {
                environment: request.model.environment.clone(),
                profile: Some(request.model.profile.clone()),
                effort,
            };
            let workspace = request
                .workspace
                .clone()
                .unwrap_or_else(|| self.builder.parent_workspace.clone());
            let id = loop {
                if cancel.is_cancelled() {
                    return Ok(Self::not_started_cancelled());
                }
                match self.service.wait_for_capacity(cancel.clone()).await {
                    Ok(true) => {}
                    Ok(false) => return Ok(Self::not_started_cancelled()),
                    Err(error) => return Err(error.to_string()),
                }
                let mut built: Option<FinishOutcome> = None;
                // The worker's last turn end, read after the turn: a route failure is
                // the one failure a fallback chain is for (ADR-0054 item 3).
                let turn_end: TurnEndCell = Arc::new(Mutex::new(None));
                let started = self
                    .service
                    .start_prepared(
                        PreparedStart {
                            task: request.prompt.clone(),
                            tools: request.tools.clone(),
                            // The run's end is the parent's one notification.
                            notify_parent: false,
                        },
                        |id: &ChildId| {
                            let (child, outcome) = self.builder.build_child(
                                &choice.environment,
                                Some(&choice),
                                &request.tools,
                                &workspace,
                                &id.0,
                                contract.clone(),
                                true,
                                Some(turn_end.clone()),
                            )?;
                            built = Some(outcome);
                            Ok(child)
                        },
                    )
                    .await;
                match started {
                    Ok(id) => {
                        if let Some(outcome) = built {
                            self.workers.lock().unwrap().insert(
                                id.0.clone(),
                                StepWorker {
                                    outcome,
                                    model: request.model.reference.clone(),
                                    turn_end,
                                },
                            );
                        }
                        // The engine reports a step's start only after its first turn
                        // (it needs the worker's ref); the live tree hears it the moment
                        // the worker exists (ADR-0075). The engine's call updates this row.
                        self.builder
                            .front_end
                            .workflow_step_started(&step_started(request, Some(&id.0)));
                        break id;
                    }
                    // A slot seen free is not reserved: a direct start took it.
                    Err(WorkerError::LimitReached { .. }) => continue,
                    Err(error) => return Err(error.to_string()),
                }
            };
            let end = self.turn(&id, cancel).await?;
            let description = self.service.describe(&id).await.unwrap_or_default();
            Ok(StepOutcome {
                worker: WorkerRef {
                    id: id.0,
                    description,
                },
                end,
            })
        })
    }

    fn repair<'a>(
        &'a self,
        worker: &'a WorkerRef,
        message: String,
        cancel: CancellationToken,
    ) -> BoxFuture<'a, Result<StepEnd, String>> {
        Box::pin(async move {
            let id = ChildId(worker.id.clone());
            self.service
                .continue_child(&id, message, Vec::new())
                .await
                .map_err(|error| error.to_string())?;
            self.turn(&id, cancel).await
        })
    }

    fn worktree<'a>(
        &'a self,
        request: &'a StepRequest,
    ) -> BoxFuture<'a, Result<Box<dyn WorktreeHold>, String>> {
        Box::pin(async move {
            let slug = request.worktree.clone().unwrap_or_default();
            let Some(base) = request.base.clone() else {
                return Err(format!("worktree: {slug}: the run has no base commit"));
            };
            // The run's workspace, as a step without a worktree would resolve it.
            let run_workspace = request
                .workspace
                .clone()
                .unwrap_or_else(|| self.builder.parent_workspace.clone());
            let worktrees = self.worktrees.clone();
            // git runs off the executor; the hold's lock serialises `git worktree add`.
            let guard = tokio::task::spawn_blocking(move || {
                worktrees.acquire(&run_workspace, &slug, &base)
            })
            .await
            .map_err(|error| format!("worktree: {error}"))??;
            Ok(Box::new(guard) as Box<dyn WorktreeHold>)
        })
    }
}

/// The service the `workflow_*` tools start runs through: the tool names no workspace,
/// so the run's base commit (ADR-0073 item 2) is `HEAD` of the workspace its steps fall
/// back to, resolved here, in the host — the engine never runs git.
struct BasedWorkflows {
    inner: Arc<InProcessWorkflows>,
    workspace: PathBuf,
}

impl WorkflowService for BasedWorkflows {
    fn start<'a>(
        &'a self,
        mut request: StartRequest,
    ) -> BoxFuture<'a, Result<RunId, WorkflowError>> {
        Box::pin(async move {
            if request.base.is_none() {
                let workspace = request
                    .workspace
                    .clone()
                    .unwrap_or_else(|| self.workspace.clone());
                request.base = crate::worktree::run_base_async(workspace).await;
            }
            self.inner.start(request).await
        })
    }

    fn status<'a>(&'a self, id: &'a RunId) -> BoxFuture<'a, Result<RunStatus, WorkflowError>> {
        self.inner.status(id)
    }

    fn wait<'a>(
        &'a self,
        id: &'a RunId,
        cancel: CancellationToken,
    ) -> BoxFuture<'a, Result<RunStatus, WorkflowError>> {
        self.inner.wait(id, cancel)
    }

    fn cancel<'a>(&'a self, id: &'a RunId) -> BoxFuture<'a, Result<(), WorkflowError>> {
        self.inner.cancel(id)
    }

    fn list<'a>(&'a self) -> BoxFuture<'a, Vec<(RunId, RunStatus)>> {
        self.inner.list()
    }
}

// ------------------------------------------------------------------ lines (item 7)

/// Shows a run's lines through the front end and wakes the parent once, at the run's
/// end. It also knows which runs have not yet delivered that end, which is what the
/// host waits on: a run counts until its notification is in the inbox.
pub struct HostWorkflowObserver {
    front_end: Arc<dyn FrontEnd>,
    inbox: Mutex<Option<Inbox>>,
    needs: Arc<Mutex<HashMap<String, String>>>,
    in_flight: Mutex<HashSet<String>>,
    settled: tokio::sync::Notify,
}

impl HostWorkflowObserver {
    /// The parent's inbox, once the parent agent exists (as `InProcessWorkers` gets it).
    pub fn set_parent_inbox(&self, inbox: Inbox) {
        *self.inbox.lock().unwrap() = Some(inbox);
    }

    /// Runs started whose end has not been delivered yet.
    pub fn running(&self) -> usize {
        self.in_flight.lock().unwrap().len()
    }

    /// Resolves once `id`'s end line and notification are out, so a caller that saw
    /// the run end through the service does not print ahead of them.
    pub async fn settled(&self, id: &RunId) {
        loop {
            let notified = self.settled.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if !self.in_flight.lock().unwrap().contains(&id.0) {
                return;
            }
            notified.await;
        }
    }
}

/// A resolved model as the tree shows it: `environment/profile[:effort]`.
fn model_text(model: &ResolvedModel) -> String {
    match &model.effort {
        Some(effort) => format!("{}/{}:{effort}", model.environment, model.profile),
        None => format!("{}/{}", model.environment, model.profile),
    }
}

/// A step's start as the TUI's tree takes it (ADR-0075).
fn step_started(request: &StepRequest, worker_id: Option<&str>) -> WorkflowStepStarted {
    WorkflowStepStarted {
        run: request.run.0.clone(),
        // The same ordinal whether the runner announces the step early or the engine
        // reports it: one step, one row (ADR-0075).
        ordinal: request.ordinal,
        call: request.call.0.clone(),
        label: request.label.clone(),
        phase: request.phase.clone(),
        role: request.role.clone(),
        model: model_text(&request.model),
        worker_id: worker_id.map(str::to_string),
        attempt: request.attempt,
        prompt: request.prompt.clone(),
    }
}

/// A step line's worker id: `w3` of `w3 (claude/opus)`.
fn line_worker(line: &StepLine) -> Option<&str> {
    line.worker
        .as_deref()
        .and_then(|worker| worker.split_whitespace().next())
}

impl WorkflowObserver for HostWorkflowObserver {
    fn run_started(&self, id: &RunId, resumed_from: Option<&RunId>) {
        self.in_flight.lock().unwrap().insert(id.0.clone());
        self.front_end.workflow_run_started(&WorkflowRunStarted {
            id: id.0.clone(),
            resumed_from: resumed_from.map(|from| from.0.clone()),
        });
    }

    fn phase(&self, id: &RunId, name: &str) {
        self.front_end
            .workflow_line(&format!("workflow {} phase: {name}", id.0));
        self.front_end.workflow_phase(&id.0, name);
    }

    fn log(&self, id: &RunId, text: &str) {
        self.front_end
            .workflow_line(&format!("workflow {}: {text}", id.0));
        self.front_end.workflow_log(&id.0, text);
    }

    fn jobs_queued(&self, id: &RunId, count: usize) {
        self.front_end.workflow_jobs_queued(&id.0, count);
    }

    fn step_started(&self, _id: &RunId, request: &StepRequest, worker: &WorkerRef) {
        self.front_end
            .workflow_step_started(&step_started(request, Some(&worker.id)));
    }

    fn thunk_failed(&self, id: &RunId, error: &str) {
        self.front_end.workflow_thunk_failed(&id.0, error);
    }

    fn step_ended(&self, id: &RunId, line: &StepLine) {
        let worker = line_worker(line);
        let needs = worker.and_then(|worker| self.needs.lock().unwrap().get(worker).cloned());
        self.front_end
            .workflow_line(&crate::render::workflow_step_note(
                &id.0,
                line,
                needs.as_deref(),
            ));
        self.front_end.workflow_step_ended(&WorkflowStepEnded {
            run: id.0.clone(),
            ordinal: line.ordinal,
            call: line.call.0.clone(),
            label: line.label.clone(),
            // The model the step ended on: the last link of its chain.
            model: line
                .models
                .last()
                .map_or_else(|| line.model.clone(), |tried| tried.model.clone()),
            status: match line.status {
                StepStatus::Done => "done",
                StepStatus::Blocked => "blocked",
                StepStatus::Failed => "failed",
                StepStatus::Cancelled => "cancelled",
            }
            .to_string(),
            attempts: line.attempts,
            replayed: line.replayed,
            error: line.error.clone(),
            worker_id: worker.map(str::to_string),
        });
    }

    fn run_ended(&self, id: &RunId, report: &RunReport) {
        self.front_end
            .workflow_line(&crate::render::workflow_run_note(report));
        let outcome = serde_json::to_value(report.outcome)
            .ok()
            .and_then(|value| value.as_str().map(str::to_string))
            .unwrap_or_default();
        self.front_end.workflow_run_ended(&WorkflowRunEnded {
            id: id.0.clone(),
            outcome: outcome.clone(),
            error: report.error.clone(),
        });
        if let Some(inbox) = self.inbox.lock().unwrap().as_ref() {
            inbox.send(
                InboxKind::Notification,
                format!(
                    "Workflow {} ended ({outcome}). Use workflow_result to read its result.",
                    id.0
                ),
            );
        }
        self.in_flight.lock().unwrap().remove(&id.0);
        self.settled.notify_waiters();
    }
}

// ------------------------------------------------------------------ composition (item 7)

/// The workflow half of a run's composition.
pub struct Workflows {
    pub service: Arc<InProcessWorkflows>,
    pub observer: Arc<HostWorkflowObserver>,
}

/// Where this run's workflow directories go: next to the session's worker journals
/// (`FILE.workflows/`) when there is a session file, the user's state directory
/// otherwise. `compose` creates it.
pub fn run_root(deps: &HostDeps, session: Option<&Path>) -> PathBuf {
    if let Some(session) = session {
        let mut path = session.as_os_str().to_os_string();
        path.push(".workflows");
        return PathBuf::from(path);
    }
    crate::models::workflow_state_root(&crate::auth::locations(deps))
        // A host with neither variable still has somewhere to journal.
        .unwrap_or_else(|| std::env::temp_dir().join("p1").join("workflows"))
}

/// Build the workflow service over the worker service and the child builder, and put
/// it in `deps` so the catalog registers the `workflow_*` tools.
pub(crate) fn compose(
    deps: &mut HostDeps,
    builder: Arc<ChildBuilder>,
    workers: Arc<InProcessWorkers>,
    run_root: PathBuf,
) -> Result<Workflows, String> {
    let settings = crate::models::workflow_settings(&crate::auth::locations(deps))?;
    // Created up front so a misplaced `--out`/state directory fails the composition,
    // not the first `workflow_start` an hour into a session.
    std::fs::create_dir_all(&run_root).map_err(|error| {
        format!(
            "cannot create the workflow directory {}: {error}",
            run_root.display()
        )
    })?;
    let needs = Arc::new(Mutex::new(HashMap::new()));
    let observer = Arc::new(HostWorkflowObserver {
        front_end: builder.front_end.clone(),
        inbox: Mutex::new(None),
        needs: needs.clone(),
        in_flight: Mutex::new(HashSet::new()),
        settled: tokio::sync::Notify::new(),
    });
    let resolver = Arc::new(HostModelResolver {
        environment_dirs: deps.environment_dirs.clone(),
    });
    let parent_workspace = builder.parent_workspace.clone();
    let runner = Arc::new(HostStepRunner {
        builder,
        service: workers,
        workers: Mutex::new(HashMap::new()),
        needs,
        worktrees: Arc::default(),
    });
    let service = InProcessWorkflows::new(runner, resolver, observer.clone(), settings, run_root);
    deps.workflow_service = Some(Arc::new(BasedWorkflows {
        inner: service.clone(),
        workspace: parent_workspace,
    }) as Arc<dyn WorkflowService>);
    deps.workflow_observer = Some(observer.clone());
    Ok(Workflows { service, observer })
}

/// The service `p1 env show` binds the `workflow_*` tools to: showing an environment
/// must assemble them, and must never start a run.
pub(crate) struct RefusingWorkflows;

impl WorkflowService for RefusingWorkflows {
    fn start<'a>(
        &'a self,
        _request: p1_workflow::StartRequest,
    ) -> BoxFuture<'a, Result<RunId, p1_workflow::WorkflowError>> {
        Box::pin(async {
            Err(p1_workflow::WorkflowError::Preflight(
                "`p1 env show` does not start workflows".to_string(),
            ))
        })
    }

    fn status<'a>(
        &'a self,
        _id: &'a RunId,
    ) -> BoxFuture<'a, Result<RunStatus, p1_workflow::WorkflowError>> {
        Box::pin(async { Err(p1_workflow::WorkflowError::UnknownRun) })
    }

    fn wait<'a>(
        &'a self,
        _id: &'a RunId,
        _cancel: CancellationToken,
    ) -> BoxFuture<'a, Result<RunStatus, p1_workflow::WorkflowError>> {
        Box::pin(async { Err(p1_workflow::WorkflowError::UnknownRun) })
    }

    fn cancel<'a>(
        &'a self,
        _id: &'a RunId,
    ) -> BoxFuture<'a, Result<(), p1_workflow::WorkflowError>> {
        Box::pin(async { Err(p1_workflow::WorkflowError::UnknownRun) })
    }

    fn list<'a>(&'a self) -> BoxFuture<'a, Vec<(RunId, RunStatus)>> {
        Box::pin(async { Vec::new() })
    }
}

/// Runs of an earlier process: the new service does not know them, but their journals
/// are still on disk for `resume_from`. Said once, on stderr, on a resumed session.
pub(crate) fn lost_runs(run_root: &Path) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(run_root) else {
        return Vec::new();
    };
    let mut runs: Vec<(u64, String)> = entries
        .filter_map(|entry| {
            let name = entry.ok()?.file_name().into_string().ok()?;
            let number = name.strip_prefix("wf")?.parse().ok()?;
            Some((number, name))
        })
        .collect();
    runs.sort();
    runs.into_iter().map(|(_, name)| name).collect()
}

/// Runs whose end has not reached the parent yet: the host keeps waiting for them.
pub(crate) fn running_workflows(deps: &HostDeps) -> usize {
    deps.workflow_observer
        .as_ref()
        .map_or(0, |observer| observer.running())
}

// ------------------------------------------------------------------ the `p1 workflow run` entry

/// `p1 workflow run` (ADR-0053): the composition of a run — catalog, worker service,
/// workflow service, line front end — with NO parent agent. The run's lines go to
/// stderr as they come; its report, rendered as `workflow_result` renders it, goes to
/// stdout; the exit code is the outcome.
#[cfg(feature = "workflows")]
pub(crate) async fn workflow_run(
    deps: &mut HostDeps,
    options: &Options,
    workflow: &cli::WorkflowRunOptions,
) -> Result<i32, RunError> {
    use p1_workflow::WorkflowService as _;
    let script = std::fs::read_to_string(&workflow.file).map_err(|error| {
        RunError::usage(format!(
            "cannot read the workflow script {}: {error}",
            workflow.file.display()
        ))
    })?;
    let args = workflow_args(workflow).map_err(RunError::usage)?;
    let workspace = resolve_workspace(options)?;
    let cancel = CancellationToken::new();
    let front_end: Arc<dyn FrontEnd> = Arc::new(LineFrontEnd::new(deps, options, cancel.clone()));

    // The same child composition as an interactive run, including the sibling
    // reservation. A standalone workflow can be invoked repeatedly with one session.
    let (completion_hub, catalog_slot, child_builder, service, _child_counter) = compose_children(
        deps,
        &workspace,
        front_end.clone(),
        options,
        workflow.max_workers,
    )?;
    let run_root = match &workflow.out {
        Some(out) => out.clone(),
        None => crate::workflow::run_root(deps, options.session.as_deref()),
    };
    let workflows = crate::workflow::compose(deps, child_builder, service.clone(), run_root)
        .map_err(RunError::usage)?;
    let catalog = Arc::new(build_catalog(
        deps,
        options.sandbox,
        &options.sandbox_write,
        &options.sandbox_read,
        &options.env_pass,
        &completion_hub,
    )?);
    let _ = catalog_slot.set(catalog);

    // The run's base commit (ADR-0073): what its steps' new worktrees branch from.
    let base = crate::worktree::run_base_async(workspace.clone()).await;
    let request = p1_workflow::StartRequest {
        script,
        args,
        resume_from: workflow.resume_from.clone().map(p1_workflow::RunId),
        role_models: workflow.roles.iter().cloned().collect(),
        workspace: Some(workspace),
        base,
    };
    let code = match workflows.service.start(request).await {
        Err(error) => {
            write_stderr(deps, &format!("{error}\n"));
            EXIT_FAILURE
        }
        Ok(id) => {
            let second = Arc::new(tokio::sync::Notify::new());
            spawn_interrupt(deps.interrupt.clone(), cancel.clone(), second);
            let mut status = workflows.service.wait(&id, cancel.clone()).await;
            if matches!(status, Ok(p1_workflow::RunStatus::Running(_))) {
                // Ctrl-C: cancel the run and wait for its `Ended`, which the engine
                // journals once the in-flight steps have been cancelled.
                let _ = workflows.service.cancel(&id).await;
                status = workflows.service.wait(&id, CancellationToken::new()).await;
            }
            // The end line is the observer's; let it out before the report.
            workflows.observer.settled(&id).await;
            match status {
                Ok(p1_workflow::RunStatus::Ended(report)) => {
                    write_stdout(deps, &(workflow_report(&workflows, &id).await + "\n"));
                    match report.outcome {
                        p1_workflow::RunOutcome::Completed => EXIT_OK,
                        p1_workflow::RunOutcome::CompletedWithIssues => EXIT_USAGE,
                        p1_workflow::RunOutcome::Failed => EXIT_FAILURE,
                        p1_workflow::RunOutcome::Cancelled => EXIT_CANCELLED,
                    }
                }
                Ok(p1_workflow::RunStatus::Running(_)) => EXIT_CANCELLED,
                Err(error) => {
                    write_stderr(deps, &format!("{error}\n"));
                    EXIT_FAILURE
                }
            }
        }
    };

    workflows.service.shutdown().await;
    service.shutdown().await;
    front_end.finish();
    Ok(code)
}

/// `--args FILE` (a JSON object) with every `--arg k=v` laid over it, in order. A value
/// that parses as JSON is that JSON (`n=3`, `items=[…]`); anything else is a string.
#[cfg(feature = "workflows")]
fn workflow_args(workflow: &cli::WorkflowRunOptions) -> Result<serde_json::Value, String> {
    let mut args = match &workflow.args_file {
        None => serde_json::Map::new(),
        Some(path) => {
            let text = std::fs::read_to_string(path)
                .map_err(|error| format!("cannot read --args {}: {error}", path.display()))?;
            match serde_json::from_str::<serde_json::Value>(&text) {
                Ok(serde_json::Value::Object(map)) => map,
                Ok(_) => return Err(format!("--args {} is not a JSON object", path.display())),
                Err(error) => {
                    return Err(format!("--args {} is not JSON: {error}", path.display()));
                }
            }
        }
    };
    for (key, value) in &workflow.args {
        let value = serde_json::from_str(value)
            .unwrap_or_else(|_| serde_json::Value::String(value.clone()));
        args.insert(key.clone(), value);
    }
    Ok(serde_json::Value::Object(args))
}

/// The report exactly as the `workflow_result` tool renders it for a model.
#[cfg(feature = "workflows")]
async fn workflow_report(
    workflows: &crate::workflow::Workflows,
    id: &p1_workflow::RunId,
) -> String {
    use p1_contracts::{Tool, ToolCall, ToolContext, ToolInput};
    let tool = p1_tool_workflow::WorkflowResultTool::new(workflows.service.clone());
    let call = ToolCall {
        call_id: "workflow-run".to_string(),
        name: "workflow_result".to_string(),
        input: ToolInput::Json(serde_json::json!({ "id": id.0 }).to_string()),
    };
    let context = ToolContext {
        cancel: CancellationToken::new(),
    };
    tool.execute(&call, context).await.content
}

/// The observer projects every run event into the structured `FrontEnd` calls the TUI's
/// tree is built from (ADR-0075), beside the unchanged lines.
#[cfg(test)]
mod tests {
    use super::*;
    use p1_contracts::{AuthorizationPolicy, EventSink};
    use p1_workflow::{CallId, Counts, ModelTry, RunOutcome};

    /// Records the workflow calls it hears, as plain strings, and renders nothing.
    #[derive(Default)]
    struct Recording {
        calls: Mutex<Vec<String>>,
    }

    impl Recording {
        fn push(&self, call: String) {
            self.calls.lock().unwrap().push(call);
        }
    }

    impl FrontEnd for Recording {
        fn event_sink(&self) -> Arc<dyn EventSink> {
            Arc::new(p1_testkit::RecordingEvents::new())
        }

        fn child_event_sink(&self, _: &str, _: &str, _: &str) -> Arc<dyn EventSink> {
            Arc::new(p1_testkit::RecordingEvents::new())
        }

        fn child_started(&self, _worker_id: &str) {}

        fn worker_ended(&self, _: &str, _: &str, _: &p1_workers::WorkerReport) {}

        fn workflow_line(&self, line: &str) {
            self.push(format!("line {line}"));
        }

        fn workflow_run_started(&self, run: &WorkflowRunStarted) {
            self.push(format!("run_started {run:?}"));
        }

        fn workflow_phase(&self, run: &str, name: &str) {
            self.push(format!("phase {run} {name}"));
        }

        fn workflow_log(&self, run: &str, text: &str) {
            self.push(format!("log {run} {text}"));
        }

        fn workflow_jobs_queued(&self, run: &str, count: usize) {
            self.push(format!("jobs_queued {run} {count}"));
        }

        fn workflow_step_started(&self, step: &WorkflowStepStarted) {
            self.push(format!("step_started {step:?}"));
        }

        fn workflow_step_ended(&self, step: &WorkflowStepEnded) {
            self.push(format!("step_ended {step:?}"));
        }

        fn workflow_thunk_failed(&self, run: &str, error: &str) {
            self.push(format!("thunk_failed {run} {error}"));
        }

        fn workflow_run_ended(&self, run: &WorkflowRunEnded) {
            self.push(format!("run_ended {run:?}"));
        }

        fn authorization(&self) -> Arc<dyn AuthorizationPolicy> {
            Arc::new(p1_testkit::ScriptedAuthorization::permit_all())
        }

        fn parent_assembled(&self, _: &str, _: &str, _: Option<crate::activity::Completion>) {}

        fn run<'a>(
            &'a self,
            _deps: &'a HostDeps,
            _agent: &'a mut p1_core::Agent,
            _cancel: &'a CancellationToken,
            _workers: Option<Arc<dyn crate::frontend::WorkerService>>,
            _stall: Option<Arc<crate::run::StallGuard>>,
        ) -> BoxFuture<'a, i32> {
            Box::pin(async { 0 })
        }

        fn finish(&self) {}
    }

    fn observer(front_end: Arc<Recording>) -> HostWorkflowObserver {
        HostWorkflowObserver {
            front_end,
            inbox: Mutex::new(None),
            needs: Arc::default(),
            in_flight: Mutex::new(HashSet::new()),
            settled: tokio::sync::Notify::new(),
        }
    }

    fn request() -> StepRequest {
        StepRequest {
            run: RunId("wf2".into()),
            call: CallId("c1".into()),
            label: Some("review:bugs".into()),
            phase: Some("Review".into()),
            role: "reviewer".into(),
            model: ResolvedModel {
                reference: "claude/claude-opus-5-5:high".into(),
                environment: "claude".into(),
                profile: "claude-opus-5-5".into(),
                effort: Some("high".into()),
                wire_model: "claude-opus-5-5".into(),
            },
            tools: vec!["read".into()],
            prompt: "Review the diff.\nList every bug.".into(),
            schema: None,
            workspace: None,
            attempt: 1,
            ordinal: 3,
            worktree: None,
            base: None,
        }
    }

    #[test]
    fn every_observer_event_is_projected_into_the_structured_calls() {
        let front_end = Arc::new(Recording::default());
        let observer = observer(front_end.clone());
        let run = RunId("wf2".into());

        observer.run_started(&run, Some(&RunId("wf1".into())));
        observer.phase(&run, "Review");
        observer.log(&run, "reviewing");
        observer.jobs_queued(&run, 5);
        observer.step_started(
            &run,
            &request(),
            &WorkerRef {
                id: "w3".into(),
                description: "claude/claude-opus-5-5".into(),
            },
        );
        let line = StepLine {
            call: CallId("c1".into()),
            ordinal: 3,
            label: Some("review:bugs".into()),
            role: "reviewer".into(),
            model: "claude/claude-opus-5-5:high".into(),
            worker: Some("w3 (claude/claude-opus-5-5)".into()),
            status: StepStatus::Failed,
            schema: "failed".into(),
            evidence: None,
            attempts: 2,
            replayed: false,
            error: Some("invalid_output: missing field".into()),
            models: vec![ModelTry {
                model: "claude/claude-opus-5-5:high".into(),
                moved_on: None,
            }],
        };
        observer.step_ended(&run, &line);
        observer.thunk_failed(&run, "a thunk failed");
        observer.run_ended(
            &run,
            &RunReport {
                id: run.clone(),
                outcome: RunOutcome::CompletedWithIssues,
                value: serde_json::Value::Null,
                counts: Counts {
                    steps: 1,
                    failed: 1,
                    ..Counts::default()
                },
                steps: vec![line.clone()],
                error: None,
                run_dir: PathBuf::from("/runs/wf2"),
            },
        );

        let calls: Vec<String> = front_end
            .calls
            .lock()
            .unwrap()
            .iter()
            .filter(|call| !call.starts_with("line "))
            .cloned()
            .collect();
        let expected = vec![
            format!(
                "run_started {:?}",
                WorkflowRunStarted {
                    id: "wf2".into(),
                    resumed_from: Some("wf1".into()),
                }
            ),
            "phase wf2 Review".to_string(),
            "log wf2 reviewing".to_string(),
            "jobs_queued wf2 5".to_string(),
            format!(
                "step_started {:?}",
                WorkflowStepStarted {
                    run: "wf2".into(),
                    ordinal: 3,
                    call: "c1".into(),
                    label: Some("review:bugs".into()),
                    phase: Some("Review".into()),
                    role: "reviewer".into(),
                    model: "claude/claude-opus-5-5:high".into(),
                    worker_id: Some("w3".into()),
                    attempt: 1,
                    prompt: "Review the diff.\nList every bug.".into(),
                }
            ),
            format!(
                "step_ended {:?}",
                WorkflowStepEnded {
                    run: "wf2".into(),
                    ordinal: 3,
                    call: "c1".into(),
                    label: Some("review:bugs".into()),
                    model: "claude/claude-opus-5-5:high".into(),
                    status: "failed".into(),
                    attempts: 2,
                    replayed: false,
                    error: Some("invalid_output: missing field".into()),
                    worker_id: Some("w3".into()),
                }
            ),
            "thunk_failed wf2 a thunk failed".to_string(),
            format!(
                "run_ended {:?}",
                WorkflowRunEnded {
                    id: "wf2".into(),
                    outcome: "completed_with_issues".into(),
                    error: None,
                }
            ),
        ];
        assert_eq!(calls, expected);

        // The ledger lines are still there, worded as before.
        let lines: Vec<String> = front_end
            .calls
            .lock()
            .unwrap()
            .iter()
            .filter_map(|call| call.strip_prefix("line ").map(str::to_string))
            .collect();
        assert_eq!(lines[0], "workflow wf2 phase: Review");
        assert_eq!(lines[1], "workflow wf2: reviewing");
        assert_eq!(lines.len(), 4, "{lines:?}");
    }
}
