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
    StepEnd, StepLine, StepOutcome, StepRequest, StepRunner, WorkerRef, WorkflowObserver,
    WorkflowService,
};

use crate::HostDeps;
use crate::frontend::FrontEnd;
use crate::run::{ChildBuilder, TurnEndCell};

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

impl WorkflowObserver for HostWorkflowObserver {
    fn run_started(&self, id: &RunId, _resumed_from: Option<&RunId>) {
        self.in_flight.lock().unwrap().insert(id.0.clone());
    }

    fn phase(&self, id: &RunId, name: &str) {
        self.front_end
            .workflow_line(&format!("workflow {} phase: {name}", id.0));
    }

    fn log(&self, id: &RunId, text: &str) {
        self.front_end
            .workflow_line(&format!("workflow {}: {text}", id.0));
    }

    fn step_ended(&self, id: &RunId, line: &StepLine) {
        let worker = line
            .worker
            .as_deref()
            .and_then(|worker| worker.split_whitespace().next());
        let needs = worker.and_then(|worker| self.needs.lock().unwrap().get(worker).cloned());
        self.front_end
            .workflow_line(&crate::render::workflow_step_note(
                &id.0,
                line,
                needs.as_deref(),
            ));
    }

    fn run_ended(&self, id: &RunId, report: &RunReport) {
        self.front_end
            .workflow_line(&crate::render::workflow_run_note(report));
        let outcome = serde_json::to_value(report.outcome)
            .ok()
            .and_then(|value| value.as_str().map(str::to_string))
            .unwrap_or_default();
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
    let runner = Arc::new(HostStepRunner {
        builder,
        service: workers,
        workers: Mutex::new(HashMap::new()),
        needs,
    });
    let service = InProcessWorkflows::new(runner, resolver, observer.clone(), settings, run_root);
    deps.workflow_service = Some(service.clone() as Arc<dyn WorkflowService>);
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
