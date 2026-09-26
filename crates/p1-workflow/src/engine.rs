//! The rhai bridge (ADR-0053 item 3): a raw, limited engine; the workflow functions;
//! `agent()` with caps, schema repair and replay; bounded thunk threads.
//!
//! This is the substrate: it owns the engine, its threads, the run's state, the caps, the
//! journal, worktree holds, the runner and the observer. What a step does next is asked of
//! the run's [`Decisions`] (`crate::decision`), which see a snapshot and answer a transition.
//!
//! rhai has no async VM, so the script runs on its own OS thread and `agent()` blocks that
//! thread on the caller's tokio handle while the step runs. Concurrency comes from OS
//! threads for `parallel` thunks and `pipeline` items, bounded per run.

use std::collections::{BTreeMap, VecDeque};
use std::future::Future;
use std::panic::AssertUnwindSafe;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, Weak};
use std::thread::JoinHandle;

use p1_contracts::CancellationToken;
use rhai::packages::{
    ArithmeticPackage, BasicArrayPackage, BasicIteratorPackage, BasicMapPackage,
    BasicStringPackage, LanguageCorePackage, LogicPackage, MoreStringPackage, Package,
};
use rhai::serde::{from_dynamic, to_dynamic};
use rhai::{AST, Array, Dynamic, Engine, EvalAltResult, FnPtr, Map, Position, Scope};
use serde_json::Value;
use tokio::runtime::Handle;
use tokio::sync::watch;

use crate::api::{
    CallId, Counts, JournalRecord, MovedOn, ResolvedModel, RunId, RunOutcome, RunProgress,
    RunReport, SchemaCheck, StepEnvelope, StepLine, StepRequest, StepRunner, StepStatus, WorkerRef,
    WorkflowError, WorkflowObserver, WorktreeHold,
};
use crate::caps::CapCounter;
use crate::check::{self, Ask};
use crate::decision::{
    Action, Attempt, AttemptOutcome, CONTRACT_VERSION, Decisions, PlanRequest, RepairTurn,
    RoleView, Snapshot, StepCost, StepProgress,
};
use crate::error::{parse_error, runtime_message};
use crate::journal::{JournalWriter, Replay, call_id, canonical_json};

type RhaiResult = Result<Dynamic, Box<EvalAltResult>>;

/// How many `log` lines `RunProgress` keeps.
const LOG_LINES: usize = 20;

/// A role as preflight resolved it.
pub(crate) struct Role {
    /// The chain, head first (ADR-0054 item 2): the role's model, then its fallbacks.
    pub(crate) chain: Vec<ResolvedModel>,
    pub(crate) tools: Vec<String>,
}

impl Role {
    /// The reference the role names first — what a step line shows whatever it walked.
    pub(crate) fn head(&self) -> String {
        self.chain
            .first()
            .map(|model| model.reference.clone())
            .unwrap_or_default()
    }
}

/// Tool names a step may never be granted: `finish` is always added by the host, and a
/// step is one level deep (ADR-0050), so no worker or workflow tools.
pub(crate) fn forbidden_tool(name: &str) -> bool {
    name.is_empty()
        || name == "finish"
        || name == "delegate"
        || name.starts_with("worker")
        || name.starts_with("workflow")
}

/// What one run shares between the script thread, its thunk threads and the service.
/// It holds no engine, so the service can keep it for its lifetime while the engine and
/// AST are dropped when the script ends.
pub(crate) struct RunState {
    pub(crate) id: RunId,
    pub(crate) run_dir: PathBuf,
    pub(crate) resumed_from: Option<RunId>,
    pub(crate) runner: Arc<dyn StepRunner>,
    /// What each step does next and what its attempts' ends mean; the rest is this state's.
    pub(crate) decisions: Arc<dyn Decisions>,
    pub(crate) observer: Arc<dyn WorkflowObserver>,
    pub(crate) roles: BTreeMap<String, Role>,
    pub(crate) caps: CapCounter,
    pub(crate) max_steps: u32,
    pub(crate) workspace: Option<PathBuf>,
    /// The run's base commit (ADR-0073): what a step's new worktree branches from.
    pub(crate) base: Option<String>,
    pub(crate) token: CancellationToken,
    /// The caller's runtime: `agent()` blocks a script thread on it; the crate owns none.
    pub(crate) handle: Handle,
    pub(crate) journal: JournalWriter,
    pub(crate) replay: Mutex<Replay>,
    /// Thread slots left; taken without ever waiting (see `fan_out`).
    pub(crate) free_threads: AtomicUsize,
    pub(crate) calls: AtomicU32,
    pub(crate) record: Mutex<Record>,
    /// `None` while running; the report once ended. Stored BEFORE `run_ended` fires.
    pub(crate) ended: watch::Sender<Option<RunReport>>,
}

#[derive(Default)]
pub(crate) struct Record {
    phase: Option<String>,
    steps_started: u32,
    steps_ended: u32,
    log: VecDeque<String>,
    counts: Counts,
    steps: Vec<StepLine>,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    // A panicking thunk must not take the run's bookkeeping down with it.
    mutex.lock().unwrap_or_else(|poison| poison.into_inner())
}

fn script_error(message: impl Into<String>) -> Box<EvalAltResult> {
    Box::new(EvalAltResult::ErrorRuntime(
        message.into().into(),
        Position::NONE,
    ))
}

/// Not catchable by a script's `try`, so a cancelled run cannot carry on.
fn cancelled_error() -> Box<EvalAltResult> {
    Box::new(EvalAltResult::ErrorTerminated(
        "cancelled".into(),
        Position::NONE,
    ))
}

fn dynamic_to_json(value: &Dynamic) -> Result<Value, String> {
    if value.is_unit() {
        return Ok(Value::Null);
    }
    from_dynamic::<Value>(value).map_err(|error| error.to_string())
}

impl RunState {
    pub(crate) fn progress(&self) -> RunProgress {
        let record = lock(&self.record);
        RunProgress {
            phase: record.phase.clone(),
            steps_started: record.steps_started,
            steps_ended: record.steps_ended,
            replayed: record.counts.replayed,
            log: record.log.iter().cloned().collect(),
        }
    }

    fn write(&self, record: &JournalRecord) {
        if let Err(error) = self.journal.append(record) {
            self.log_line(&format!("journal write failed: {error}"));
        }
    }

    fn log_line(&self, text: &str) {
        {
            let mut record = lock(&self.record);
            if record.log.len() == LOG_LINES {
                record.log.pop_front();
            }
            record.log.push_back(text.to_string());
        }
        self.observer.log(&self.id, text);
    }

    fn set_phase(&self, name: &str) {
        lock(&self.record).phase = Some(name.to_string());
        self.write(&JournalRecord::Phase {
            name: name.to_string(),
        });
        self.observer.phase(&self.id, name);
    }

    /// Drives `future` on this (non-runtime) thread; `None` when the run is cancelled
    /// first, in which case the future — and the step it runs — is dropped.
    fn cancellable<T>(&self, future: impl Future<Output = T>) -> Option<T> {
        let token = self.token.clone();
        self.handle.block_on(async move {
            tokio::select! {
                biased;
                _ = token.cancelled() => None,
                value = future => Some(value),
            }
        })
    }

    /// One `agent()` call, in the order ADR-0053 fixes, walking the role's fallback chain
    /// on a route failure (ADR-0054). `Err` only for a cancelled run.
    ///
    /// The step's moves are its decisions' ([`crate::decision`]); this is the substrate that
    /// asks them, checks each answer ([`check::transition`]) and applies it. Every decision
    /// call returns before anything is dispatched, journalled or handed back to the script,
    /// and no lock is held across it.
    fn step(
        &self,
        prompt: &str,
        opts: &StepOptions,
        call: &CallId,
    ) -> Result<StepEnvelope, Box<EvalAltResult>> {
        lock(&self.record).steps_started += 1;
        // The step's ordinal in its run: what tells two calls with the same id apart
        // (ADR-0075), so it is taken before anything can end the step.
        let number = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
        let role = self.roles.get(&opts.role);
        let line = LineContext {
            role: opts.role.clone(),
            model: role.map(Role::head).unwrap_or_default(),
            ordinal: number,
        };
        let plan = PlanRequest {
            version: CONTRACT_VERSION,
            call: call.clone(),
            label: opts.label.clone(),
            role: opts.role.clone(),
            worktree: opts.worktree.clone(),
            tools: opts.tools.clone(),
        };
        let role_view = role.map(|role| RoleView {
            chain: role.chain.clone(),
            tools: role.tools.clone(),
        });
        let mut progress = StepProgress {
            ordinal: number,
            ..StepProgress::default()
        };
        let mut work = StepWork::default();
        let mut ask = Ask::Plan;
        loop {
            // The replay lock is released before the decision is called.
            let replay = lock(&self.replay).view();
            let snapshot = Snapshot {
                version: CONTRACT_VERSION,
                run: self.id.clone(),
                max_steps: self.max_steps,
                has_base: self.base.is_some(),
                role: role_view.clone(),
                replay,
                step: progress.clone(),
            };
            let answer = match &ask {
                Ask::Plan => self.decisions.plan_step(&snapshot, &plan),
                Ask::Accept(attempt) => self.decisions.accept_step(
                    &snapshot,
                    &AttemptOutcome {
                        version: CONTRACT_VERSION,
                        call: call.clone(),
                        label: opts.label.clone(),
                        attempt: attempt.clone(),
                    },
                ),
            };
            let checked = answer
                .map_err(|error| format!("the decision failed: {error}"))
                .and_then(|transition| {
                    check::transition(&snapshot, call, &ask, transition)
                        .map_err(|error| format!("transition refused: {error}"))
                });
            let action = match checked {
                Ok(action) => action,
                Err(error) => {
                    return self.refuse_step(call, &line, opts, &progress, &mut work, error);
                }
            };
            ask = match action {
                Action::Replay { entry } => {
                    let taken = lock(&self.replay).take_entry(entry, call);
                    let Some((envelope, from)) = taken else {
                        // Another call took the entry, or missed the prefix, since the
                        // snapshot: ask again with the prefix as it stands now.
                        continue;
                    };
                    self.write(&JournalRecord::Replayed {
                        call: call.clone(),
                        from,
                    });
                    return Ok(self.conclude(call, &line, envelope, true, &StepCost::default()));
                }
                Action::Dispatch {
                    link,
                    tools,
                    latch_replay,
                    ..
                } => {
                    if latch_replay {
                        lock(&self.replay).latch_off();
                    }
                    // The check found the link in this role's chain.
                    let model = role.and_then(|role| {
                        Some((role, role.chain.get(usize::try_from(link).ok()?)?))
                    });
                    let Some((role, model)) = model else {
                        let error = format!("transition refused: no link {link} to dispatch");
                        return self.refuse_step(call, &line, opts, &progress, &mut work, error);
                    };
                    let target = Target {
                        prompt,
                        opts,
                        call,
                        role,
                        tools,
                    };
                    Ask::Accept(self.dispatch(&target, link, model, &mut progress, &mut work))
                }
                Action::Repair { message, rejected } => {
                    let (Some(request), Some(worker)) = (work.request.clone(), work.worker.clone())
                    else {
                        let error = "transition refused: no worker to repair".to_string();
                        return self.refuse_step(call, &line, opts, &progress, &mut work, error);
                    };
                    progress.repair = Some(RepairTurn {
                        worker: worker.clone(),
                        rejected,
                    });
                    Ask::Accept(self.repair(&request, &opts.json, &worker, message, &mut progress))
                }
                Action::MoveOn {
                    reason,
                    tried,
                    error,
                    worker,
                } => {
                    progress.route_failed |= reason == MovedOn::RouteFailed;
                    progress.walked.push(tried);
                    progress.last_error = error;
                    progress.last_worker = worker;
                    progress.moved_on = true;
                    Ask::Plan
                }
                Action::End {
                    envelope,
                    latch_replay,
                } => {
                    if latch_replay {
                        lock(&self.replay).latch_off();
                    }
                    return self.finish(call, &line, envelope, &progress.cost, work.hold.take());
                }
                Action::Cancelled { envelope } => {
                    return self.finish(call, &line, envelope, &progress.cost, work.hold.take());
                }
            };
        }
    }

    /// Dispatches the first attempt on `link` of the role's chain: the step's worktree on its
    /// first link, the `Fallback` hop before any later one, then the cap check, the
    /// `Dispatch` line and the turn. Returns how it ended, for `accept_step`.
    fn dispatch(
        &self,
        target: &Target<'_>,
        link: u32,
        model: &ResolvedModel,
        progress: &mut StepProgress,
        work: &mut StepWork,
    ) -> Attempt {
        if progress.link.is_none() {
            let opts = target.opts;
            let phase = opts
                .phase
                .clone()
                .or_else(|| lock(&self.record).phase.clone());
            work.phase = phase;
            // The step's own worktree (ADR-0073), held until the step ends: across every
            // link of its chain and its repair turn. Prepared before anything is dispatched.
            if let (Some(_), Some(head)) = (&opts.worktree, target.role.chain.first()) {
                let probe = self.request_for(target, head, self.workspace.clone(), work, progress);
                match self.cancellable(self.runner.worktree(&probe)) {
                    None => return Attempt::WorktreeCancelled,
                    Some(Err(error)) => return Attempt::WorktreeRefused { error },
                    Some(Ok(hold)) => work.hold = Some(hold),
                }
            }
            work.workspace = match &work.hold {
                Some(hold) => Some(hold.info().path.clone()),
                None => opts.workspace.clone().or_else(|| self.workspace.clone()),
            };
        }
        if let Some(previous) = progress.walked.last() {
            // The hop is journalled BEFORE the next model is dispatched (ADR-0054 item 4),
            // so a journal always shows why a link was left.
            self.write(&JournalRecord::Fallback {
                call: target.call.clone(),
                from: previous.model.clone(),
                to: model.reference.clone(),
                error: progress.last_error.clone(),
            });
            progress.cost.fell_back += 1;
        }
        progress.link = Some(link);
        progress.moved_on = false;
        let request = self.request_for(target, model, work.workspace.clone(), work, progress);
        work.request = Some(request.clone());
        work.worker = None;
        if let Some(refused) = self.spend(&request, &target.opts.json, 1, &mut progress.cost) {
            return refused;
        }
        let outcome = self.cancellable(self.runner.run(&request, self.token.clone()));
        progress.cost.attempts += 1;
        match outcome {
            None => Attempt::Cancelled,
            Some(Err(reason)) => Attempt::RunnerRefused { reason },
            Some(Ok(outcome)) => {
                self.observer
                    .step_started(&self.id, &request, &outcome.worker);
                work.worker = Some(outcome.worker.clone());
                Attempt::Ended {
                    worker: outcome.worker,
                    end: outcome.end,
                }
            }
        }
    }

    fn request_for(
        &self,
        target: &Target<'_>,
        model: &ResolvedModel,
        workspace: Option<PathBuf>,
        work: &StepWork,
        progress: &StepProgress,
    ) -> StepRequest {
        StepRequest {
            run: self.id.clone(),
            call: target.call.clone(),
            label: target.opts.label.clone(),
            phase: work.phase.clone(),
            role: target.opts.role.clone(),
            model: model.clone(),
            tools: target.tools.clone(),
            prompt: target.prompt.to_string(),
            schema: target.opts.schema.clone(),
            workspace,
            attempt: 1,
            ordinal: progress.ordinal,
            worktree: target.opts.worktree.clone(),
            base: self.base.clone(),
        }
    }

    /// Cap check and `Dispatch` line for one attempt; `Some` when the attempt could not
    /// be dispatched, which also charges the step's capped count (ADR-0054 item 4). The cap
    /// is the substrate's: a decision may ask to dispatch, a capped model is refused here.
    fn spend(
        &self,
        request: &StepRequest,
        opts: &Value,
        attempt: u32,
        cost: &mut StepCost,
    ) -> Option<Attempt> {
        let wire_model = &request.model.wire_model;
        if let Err((used, limit)) = self.caps.try_spend(wire_model) {
            self.write(&JournalRecord::Capped {
                call: request.call.clone(),
                wire_model: wire_model.clone(),
                used,
                limit,
            });
            cost.capped += 1;
            return Some(Attempt::Capped {
                wire_model: wire_model.clone(),
                used,
                limit,
            });
        }
        // Written before the runner is called: a resumed run charges every dispatch, so
        // an attempt that crashed mid-step still counts against the cap.
        let dispatch = JournalRecord::Dispatch {
            call: request.call.clone(),
            label: request.label.clone(),
            role: request.role.clone(),
            model: request.model.reference.clone(),
            wire_model: wire_model.clone(),
            attempt,
            prompt: request.prompt.clone(),
            opts: opts.clone(),
        };
        if let Err(error) = self.journal.append(&dispatch) {
            return Some(Attempt::JournalFailed {
                error: format!("journal: {error}"),
            });
        }
        None
    }

    /// The one repair turn (item 5): another turn in the SAME worker that produced the
    /// invalid result — or ended its turn without `finish` (ADR-0072) — never a new model
    /// (ADR-0054 item 3). Cap check and `Dispatch` line first, as for any attempt.
    fn repair(
        &self,
        request: &StepRequest,
        opts: &Value,
        worker: &WorkerRef,
        message: String,
        progress: &mut StepProgress,
    ) -> Attempt {
        if let Some(refused) = self.spend(request, opts, 2, &mut progress.cost) {
            return refused;
        }
        let end = self.cancellable(self.runner.repair(worker, message, self.token.clone()));
        progress.cost.attempts += 1;
        match end {
            None => Attempt::Cancelled,
            Some(Err(reason)) => Attempt::RunnerRefused { reason },
            Some(Ok(end)) => Attempt::RepairEnded { end },
        }
    }

    /// The step's end: its worktree settled and released, then [`Self::conclude`]. A
    /// cancelled envelope ends the script too.
    fn finish(
        &self,
        call: &CallId,
        line: &LineContext,
        mut envelope: StepEnvelope,
        cost: &StepCost,
        hold: Option<Box<dyn WorktreeHold>>,
    ) -> Result<StepEnvelope, Box<EvalAltResult>> {
        if let Some(hold) = hold {
            // The head as the step left it; the hold is released right after, before the
            // step concludes, so the script's next step finds the worktree free.
            let settled = self.handle.block_on(hold.settle());
            envelope.worktree = Some(match settled {
                Ok(info) => info,
                Err(error) => {
                    self.log_line(&format!("worktree: {error}"));
                    hold.info().clone()
                }
            });
            drop(hold);
        }
        if envelope.status == StepStatus::Cancelled {
            self.conclude(call, line, envelope, false, cost);
            return Err(cancelled_error());
        }
        Ok(self.conclude(call, line, envelope, false, cost))
    }

    /// A decision that failed, or a transition the check refused: the step fails with the
    /// reason, and the replay prefix is latched off as for any call it did not answer.
    fn refuse_step(
        &self,
        call: &CallId,
        line: &LineContext,
        opts: &StepOptions,
        progress: &StepProgress,
        work: &mut StepWork,
        error: String,
    ) -> Result<StepEnvelope, Box<EvalAltResult>> {
        lock(&self.replay).latch_off();
        let envelope = StepEnvelope {
            step: call.clone(),
            label: opts.label.clone(),
            status: StepStatus::Failed,
            value: Value::Null,
            schema: SchemaCheck::NotRequested,
            evidence: None,
            attempts: progress.cost.attempts,
            worker: None,
            needs: None,
            error: Some(format!("workflow decision: {error}")),
            models: progress.walked.clone(),
            worktree: None,
        };
        self.finish(call, line, envelope, &progress.cost, work.hold.take())
    }

    /// Everything after a step: `Result` line, counts, the step line, the observer.
    fn conclude(
        &self,
        call: &CallId,
        line: &LineContext,
        envelope: StepEnvelope,
        replayed: bool,
        cost: &StepCost,
    ) -> StepEnvelope {
        self.write(&JournalRecord::Result {
            call: call.clone(),
            envelope: envelope.clone(),
        });
        let step_line = StepLine {
            call: call.clone(),
            ordinal: line.ordinal,
            label: envelope.label.clone(),
            role: line.role.clone(),
            model: line.model.clone(),
            worker: envelope.worker.clone(),
            status: envelope.status,
            schema: match envelope.schema {
                SchemaCheck::NotRequested => "not_requested",
                SchemaCheck::Passed => "passed",
                SchemaCheck::Failed(_) => "failed",
            }
            .to_string(),
            evidence: envelope.evidence.clone(),
            attempts: envelope.attempts,
            replayed,
            error: envelope.error.clone(),
            models: envelope.models.clone(),
        };
        {
            let mut record = lock(&self.record);
            record.steps_ended += 1;
            let counts = &mut record.counts;
            counts.steps += 1;
            counts.replayed += u32::from(replayed);
            match envelope.status {
                StepStatus::Done => counts.done += 1,
                StepStatus::Blocked => counts.blocked += 1,
                StepStatus::Failed => counts.failed += 1,
                StepStatus::Cancelled => counts.cancelled += 1,
            }
            let error = envelope.error.as_deref().unwrap_or("");
            if envelope.status == StepStatus::Done
                && envelope
                    .evidence
                    .as_deref()
                    .is_some_and(|evidence| evidence.starts_with("not verified"))
            {
                counts.not_verified += 1;
            }
            // A cap counts every link it refused (ADR-0054 item 4), however the step
            // ended: a skipped link is cost the run must be able to see.
            counts.capped += cost.capped;
            counts.fell_back += cost.fell_back;
            counts.invalid_output += u32::from(error.starts_with("invalid_output:"));
            record.steps.push(step_line.clone());
        }
        self.observer.step_ended(&self.id, &step_line);
        envelope
    }

    /// Writes `result.json` and `Ended`, stores the report, THEN tells the observer — so
    /// whoever the observer wakes finds the run ended.
    pub(crate) fn end(&self, value: Value, error: Option<(RunOutcome, String)>) {
        let (counts, steps) = {
            let record = lock(&self.record);
            (record.counts, record.steps.clone())
        };
        let (outcome, error) = match error {
            Some((outcome, message)) => (outcome, Some(message)),
            None if counts.failed + counts.blocked + counts.cancelled + counts.capped == 0 => {
                (RunOutcome::Completed, None)
            }
            None => (RunOutcome::CompletedWithIssues, None),
        };
        let report = RunReport {
            id: self.id.clone(),
            outcome,
            value,
            counts,
            steps,
            error: error.clone(),
            run_dir: self.run_dir.clone(),
        };
        let written = serde_json::to_vec_pretty(&report)
            .map_err(std::io::Error::other)
            .and_then(|bytes| std::fs::write(self.run_dir.join("result.json"), bytes));
        if let Err(error) = written {
            self.log_line(&format!("result.json write failed: {error}"));
        }
        self.write(&JournalRecord::Ended {
            outcome,
            counts,
            error,
        });
        self.ended.send_replace(Some(report.clone()));
        self.observer.run_ended(&self.id, &report);
    }
}

/// The part of a step line that comes from the call, not the envelope.
struct LineContext {
    role: String,
    model: String,
    ordinal: u32,
}

/// What the substrate holds for one step that no snapshot carries: the worktree hold, the
/// step's workspace and phase, the request of the link it is on and that link's worker.
#[derive(Default)]
struct StepWork {
    hold: Option<Box<dyn WorktreeHold>>,
    workspace: Option<PathBuf>,
    phase: Option<String>,
    request: Option<StepRequest>,
    worker: Option<WorkerRef>,
}

/// The call a step answers and the grant its decision chose, for building its requests.
struct Target<'a> {
    prompt: &'a str,
    opts: &'a StepOptions,
    call: &'a CallId,
    role: &'a Role,
    tools: Vec<String>,
}

/// `agent()`'s options. Every key is known: a typo is an error, never a silently
/// different run.
struct StepOptions {
    /// As given, for the call id and the `Dispatch` line.
    json: Value,
    role: String,
    label: Option<String>,
    phase: Option<String>,
    schema: Option<Value>,
    tools: Option<Vec<String>>,
    workspace: Option<PathBuf>,
    /// The step's own git worktree, by slug (ADR-0073).
    worktree: Option<String>,
}

impl StepOptions {
    fn parse(opts: &Dynamic) -> Result<Self, Box<EvalAltResult>> {
        let json = if opts.is_unit() {
            Value::Object(serde_json::Map::new())
        } else if opts.is_map() {
            dynamic_to_json(opts).map_err(|error| script_error(format!("agent: opts: {error}")))?
        } else {
            return Err(script_error(format!(
                "agent: opts must be a map, not {}",
                opts.type_name()
            )));
        };
        let mut parsed = Self {
            json: Value::Null,
            role: "worker".to_string(),
            label: None,
            phase: None,
            schema: None,
            tools: None,
            workspace: None,
            worktree: None,
        };
        let text = |key: &str, value: &Value| {
            value
                .as_str()
                .map(str::to_string)
                .ok_or_else(|| script_error(format!("agent: option \"{key}\" must be a string")))
        };
        for (key, value) in json.as_object().into_iter().flatten() {
            match key.as_str() {
                "role" => parsed.role = text(key, value)?,
                "label" => parsed.label = Some(text(key, value)?),
                "phase" => parsed.phase = Some(text(key, value)?),
                "workspace" => parsed.workspace = Some(PathBuf::from(text(key, value)?)),
                "worktree" => parsed.worktree = Some(parse_slug(&text(key, value)?)?),
                "schema" if value.is_object() => parsed.schema = Some(value.clone()),
                "schema" => return Err(script_error("agent: option \"schema\" must be a map")),
                "tools" => parsed.tools = Some(parse_tools(value)?),
                other => return Err(script_error(format!("agent: unknown option \"{other}\""))),
            }
        }
        if parsed.worktree.is_some() && parsed.workspace.is_some() {
            return Err(script_error(
                "agent: options \"worktree\" and \"workspace\" cannot be given together",
            ));
        }
        parsed.json = json;
        Ok(parsed)
    }
}

/// The longest worktree slug a script may name (ADR-0073).
const MAX_SLUG: usize = 64;

/// A worktree slug: lowercase ASCII letters, digits and `-`, starting and ending with a
/// letter or digit, at most [`MAX_SLUG`] characters — it becomes a directory name and
/// the branch `task/<slug>`, so nothing else is let through.
fn parse_slug(slug: &str) -> Result<String, Box<EvalAltResult>> {
    let edge = |byte: u8| byte.is_ascii_lowercase() || byte.is_ascii_digit();
    let bytes = slug.as_bytes();
    let valid = !bytes.is_empty()
        && bytes.len() <= MAX_SLUG
        && bytes.iter().all(|&byte| edge(byte) || byte == b'-')
        && edge(bytes[0])
        && edge(bytes[bytes.len() - 1]);
    if !valid {
        return Err(script_error(format!(
            "agent: option \"worktree\" must be a slug of lowercase letters, digits and '-', starting and ending with a letter or digit, at most {MAX_SLUG} characters, not \"{slug}\""
        )));
    }
    Ok(slug.to_string())
}

fn parse_tools(value: &Value) -> Result<Vec<String>, Box<EvalAltResult>> {
    let shape = || script_error("agent: option \"tools\" must be a non-empty array of strings");
    let items = value
        .as_array()
        .filter(|items| !items.is_empty())
        .ok_or_else(shape)?;
    items
        .iter()
        .map(|item| {
            let name = item.as_str().ok_or_else(shape)?;
            if forbidden_tool(name) {
                return Err(script_error(format!(
                    "agent: a step may not be granted \"{name}\""
                )));
            }
            Ok(name.to_string())
        })
        .collect()
}

// ------------------------------------------------------------------------ the engine

/// Everything a script evaluation needs from any thread. Registered functions hold it
/// weakly, so the engine (which owns those functions) does not keep itself alive.
pub(crate) struct Script {
    run: Arc<RunState>,
    engine: OnceLock<Arc<Engine>>,
    ast: OnceLock<Arc<AST>>,
}

/// What ONE step envelope may carry, in each of the three sums rhai checks: strings,
/// array items and map entries (§2 "Engine limits") — the per-envelope part of a run's
/// data budget.
const ENVELOPE_STRINGS: usize = 64 * 1024;
const ENVELOPE_ARRAY_ITEMS: usize = 4096;
const ENVELOPE_MAP_ENTRIES: usize = 4096;

/// The most envelopes' worth of data one run is budgeted for, whatever its `max_steps` is.
///
/// A budget that grew with the whole step cap (`max_steps` defaults to 200, a settings
/// table may set thousands) is not a machine-safe ceiling: rhai gives every value its own
/// three sums, so one run can hold this much per VALUE, and at most [`MAX_RUN_THREADS`]
/// thunks may be in flight at once (`max_threads` is clamped to it). The ceiling is therefore
/// this cap times that concurrency, not the step count.
///
/// The arithmetic, worst case: 64 thunks × 4 MiB of strings = 256 MiB; a thunk that also
/// fills an array and a map holds 64 × 262,144 entries — about 6 MiB of array items and
/// 13 MiB of map entries at roughly 24 and 50 bytes each, so a script holding a full-size
/// value of each kind in every thunk peaks near 1.5 GiB, about a third of what an uncapped
/// 200-step budget allowed (the review of PR #174 measured 4.5–5.5 GiB there). Filling
/// those containers is not cheap either: the map path re-checks the WHOLE map on every
/// insert, and that walk is not operation-counted, so filling one to the cap is 262,144²/2
/// entry walks — tens of minutes of one thread's CPU.
///
/// So a fan-out whose envelopes sum past one capped value — more than 64 full-size (64 KiB)
/// envelopes, or about 136 at the 30 KB the issue's steps returned — must be split into
/// several `parallel()` calls, which `max_steps` 200 still allows.
const DATA_BUDGET_ENVELOPES: u32 = 64;

/// The most thunk threads one run may use, whatever `max_threads` says: the data budget above
/// is per value, so the run's aggregate ceiling holds only while concurrency is bounded too
/// (review of PR #174). A thunk that finds no free thread runs inline on its caller's thread.
pub(crate) const MAX_RUN_THREADS: usize = 64;

/// The data-size limits for a run that may make `max_steps` `agent()` calls: the smaller of
/// that cap and [`DATA_BUDGET_ENVELOPES`] envelopes' worth, in each of rhai's three sums.
///
/// rhai checks its data limits against the SUM over the whole value a call returns
/// (`eval/data_check.rs` `calc_array_sizes`/`calc_map_sizes`), and a native function's
/// result is checked like any other, so rhai offers no way to exempt a host-provided value
/// from the accounting: `parallel()`'s array of a dozen done envelopes, a map a script
/// builds from several envelopes, and one verbose envelope are all ONE budget (issue #121).
/// The envelopes are the host's data — one per `agent()` call the run's `max_steps` caps —
/// so many envelopes' worth, and not one, is the bound a completed fan-out needs. A
/// script's OWN strings, arrays and maps stay bounded by the same per-value numbers, which
/// `max_variables` and `max_operations` keep it from multiplying without limit.
fn data_limits(max_steps: u32) -> (usize, usize, usize) {
    // A run that may make no call still evaluates its script: one envelope's worth, never
    // zero (a zero limit refuses every non-empty string).
    let envelopes = usize::try_from(max_steps.min(DATA_BUDGET_ENVELOPES))
        .unwrap_or(usize::MAX)
        .max(1);
    (
        ENVELOPE_STRINGS.saturating_mul(envelopes),
        ENVELOPE_ARRAY_ITEMS.saturating_mul(envelopes),
        ENVELOPE_MAP_ENTRIES.saturating_mul(envelopes),
    )
}

/// The raw engine with only the reviewed packages and the limits of ADR-0053. `max_steps`
/// is the run's own step cap, which sizes the data limits ([`data_limits`]). No module
/// resolver is set, so `import` finds nothing; `eval` is a disabled keyword (a parse error).
pub(crate) fn sandboxed_engine(max_steps: u32) -> Engine {
    let mut engine = Engine::new_raw();
    LanguageCorePackage::new().register_into_engine(&mut engine);
    ArithmeticPackage::new().register_into_engine(&mut engine);
    LogicPackage::new().register_into_engine(&mut engine);
    BasicStringPackage::new().register_into_engine(&mut engine);
    MoreStringPackage::new().register_into_engine(&mut engine);
    BasicArrayPackage::new().register_into_engine(&mut engine);
    BasicMapPackage::new().register_into_engine(&mut engine);
    BasicIteratorPackage::new().register_into_engine(&mut engine);

    engine.set_max_operations(5_000_000);
    engine.set_max_call_levels(24);
    engine.set_max_expr_depths(32, 32);
    let (strings, arrays, maps) = data_limits(max_steps);
    engine.set_max_string_size(strings);
    engine.set_max_array_size(arrays);
    engine.set_max_map_size(maps);
    engine.set_max_variables(256);
    // Closures count as functions: this also bounds the thunks of one script.
    engine.set_max_functions(256);
    engine.set_max_modules(1);
    engine.disable_symbol("eval");

    // `LanguageCorePackage` ships `sleep`, which would block a thread for as long as the
    // script asks. Functions registered on the engine are found before package ones.
    engine.register_fn("sleep", |_seconds: i64| -> RhaiResult {
        Err(script_error("sleep is not available to scripts"))
    });
    engine.register_fn("sleep", |_seconds: f64| -> RhaiResult {
        Err(script_error("sleep is not available to scripts"))
    });
    engine
}

pub(crate) fn compile(engine: &Engine, script: &str) -> Result<AST, WorkflowError> {
    engine.compile(script).map_err(|error| parse_error(&error))
}

/// Registers the workflow functions on the compiled script's engine.
pub(crate) fn prepare(mut engine: Engine, ast: AST, run: Arc<RunState>) -> Arc<Script> {
    let script = Arc::new(Script {
        run: run.clone(),
        engine: OnceLock::new(),
        ast: OnceLock::new(),
    });

    let state = run.clone();
    engine.register_fn("log", move |value: Dynamic| {
        state.log_line(&value.to_string())
    });
    let state = run.clone();
    engine.register_fn("phase", move |name: &str| state.set_phase(name));
    let state = run.clone();
    engine.on_print(move |text| state.log_line(text));
    let state = run.clone();
    engine.on_debug(move |text, _source, _position| state.log_line(text));

    let state = run.clone();
    engine.register_fn("agent", move |prompt: &str| -> RhaiResult {
        agent(&state, prompt, &Dynamic::UNIT)
    });
    let state = run.clone();
    engine.register_fn("agent", move |prompt: &str, opts: Dynamic| -> RhaiResult {
        agent(&state, prompt, &opts)
    });

    let weak = Arc::downgrade(&script);
    engine.register_fn("parallel", move |thunks: Array| -> RhaiResult {
        let script = upgrade(&weak)?;
        let thunks = thunks
            .iter()
            .map(|thunk| cast_fn(thunk, "parallel: every element"))
            .collect::<Result<Vec<_>, _>>()?;
        let jobs = thunks
            .into_iter()
            .map(|thunk| -> Job {
                Box::new(move |script: &Script| script.call(&thunk, Vec::new()))
            })
            .collect();
        script.fan_out(jobs).map(Dynamic::from)
    });

    // rhai has no variadic native functions: one registration per arity.
    macro_rules! register_pipeline {
        ($($stage:ident),+) => {{
            let weak = Arc::downgrade(&script);
            engine.register_fn("pipeline", move |items: Array, $($stage: Dynamic),+| -> RhaiResult {
                let script = upgrade(&weak)?;
                let stages = [$($stage),+]
                    .iter()
                    .map(|stage| cast_fn(stage, "pipeline: every stage"))
                    .collect::<Result<Vec<_>, _>>()?;
                let stages = Arc::new(stages);
                let jobs = items
                    .into_iter()
                    .map(|item| -> Job {
                        let stages = stages.clone();
                        Box::new(move |script: &Script| {
                            let mut current = item;
                            for stage in stages.iter() {
                                current = script.call(stage, vec![current])?;
                            }
                            Ok(current)
                        })
                    })
                    .collect();
                script.fan_out(jobs).map(Dynamic::from)
            });
        }};
    }
    register_pipeline!(s1);
    register_pipeline!(s1, s2);
    register_pipeline!(s1, s2, s3);
    register_pipeline!(s1, s2, s3, s4);
    register_pipeline!(s1, s2, s3, s4, s5);
    register_pipeline!(s1, s2, s3, s4, s5, s6);

    // The idioms need these: rhai has no `null`-safe key test on maps that reads well,
    // and prompts want a stable rendering of structured values.
    engine.register_fn("has", |map: Map, key: &str| map.contains_key(key));
    engine.register_fn(
        "json",
        |value: Dynamic| -> Result<String, Box<EvalAltResult>> {
            dynamic_to_json(&value)
                .map(|value| canonical_json(&value))
                .map_err(|error| script_error(format!("json: {error}")))
        },
    );

    // A spinning script dies here within a few operations of the cancel; a blocked
    // `agent()` is dropped by the `select!` in `cancellable`.
    let token = run.token.clone();
    engine.on_progress(move |_operations| token.is_cancelled().then(|| Dynamic::from("cancelled")));

    let _ = script.engine.set(Arc::new(engine));
    let _ = script.ast.set(Arc::new(ast));
    script
}

fn upgrade(weak: &Weak<Script>) -> Result<Arc<Script>, Box<EvalAltResult>> {
    weak.upgrade()
        .ok_or_else(|| script_error("the workflow run has ended"))
}

fn cast_fn(value: &Dynamic, what: &str) -> Result<FnPtr, Box<EvalAltResult>> {
    value.clone().try_cast::<FnPtr>().ok_or_else(|| {
        script_error(format!(
            "{what} must be a function (`|| agent(..)` or `|x| ..`), not {}",
            value.type_name()
        ))
    })
}

fn agent(run: &RunState, prompt: &str, opts: &Dynamic) -> RhaiResult {
    let opts = StepOptions::parse(opts)?;
    let call = call_id(opts.label.as_deref().unwrap_or(""), prompt, &opts.json);
    let envelope = run.step(prompt, &opts, &call)?;
    let json = serde_json::to_value(&envelope)
        .map_err(|error| script_error(format!("agent: envelope: {error}")))?;
    to_dynamic(json)
}

type Job = Box<dyn FnOnce(&Script) -> RhaiResult + Send>;

enum Pending {
    Thread(JoinHandle<RhaiResult>),
    Inline(RhaiResult),
}

/// A taken thread slot, returned when the thread's work ends — also on a panic, and when
/// the spawn itself fails (the closure owning it is dropped).
struct ThreadSlot(Arc<RunState>);

impl Drop for ThreadSlot {
    fn drop(&mut self) {
        self.0.free_threads.fetch_add(1, Ordering::SeqCst);
    }
}

impl Script {
    fn call(&self, function: &FnPtr, args: Vec<Dynamic>) -> RhaiResult {
        let (Some(engine), Some(ast)) = (self.engine.get(), self.ast.get()) else {
            return Err(script_error("the workflow engine is not ready"));
        };
        function.call::<Dynamic>(engine, ast, args)
    }

    fn take_slot(&self) -> Option<ThreadSlot> {
        self.run
            .free_threads
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |free| {
                free.checked_sub(1)
            })
            .ok()
            .map(|_| ThreadSlot(self.run.clone()))
    }

    /// Runs `jobs` on thread slots where one is free and INLINE on this thread, in order,
    /// where none is. Nothing ever waits for a slot, so nesting cannot deadlock at any
    /// bound. Results keep input order; every thread is joined before the first error (in
    /// input order) is reported.
    fn fan_out(self: &Arc<Self>, jobs: Vec<Job>) -> Result<Array, Box<EvalAltResult>> {
        self.run.observer.jobs_queued(&self.run.id, jobs.len());
        let mut pending = Vec::with_capacity(jobs.len());
        for job in jobs {
            let job: Job = Box::new(move |script: &Script| {
                let result = job(script);
                if let Err(error) = &result {
                    script
                        .run
                        .observer
                        .thunk_failed(&script.run.id, &error.to_string());
                }
                result
            });
            match self.take_slot() {
                Some(slot) => {
                    let script = self.clone();
                    let spawned = std::thread::Builder::new()
                        .name("p1-wf-thunk".to_string())
                        .spawn(move || {
                            let _slot = slot;
                            job(&script)
                        });
                    pending.push(match spawned {
                        Ok(handle) => Pending::Thread(handle),
                        Err(error) => Pending::Inline(Err(script_error(format!(
                            "cannot start a thunk thread: {error}"
                        )))),
                    });
                }
                None => pending.push(Pending::Inline(job(self))),
            }
        }
        let mut values = Array::with_capacity(pending.len());
        let mut first_error = None;
        for item in pending {
            let result = match item {
                Pending::Thread(handle) => handle
                    .join()
                    .unwrap_or_else(|_| Err(script_error("a thunk panicked"))),
                Pending::Inline(result) => result,
            };
            match result {
                Ok(value) => values.push(value),
                Err(error) => {
                    first_error.get_or_insert(error);
                }
            }
        }
        match first_error {
            Some(error) => Err(error),
            None => Ok(values),
        }
    }
}

/// The script thread's body: evaluate, then end the run whatever happened.
pub(crate) fn execute(script: Arc<Script>, args: Dynamic) {
    let run = script.run.clone();
    let result = std::panic::catch_unwind(AssertUnwindSafe(|| {
        let (Some(engine), Some(ast)) = (script.engine.get(), script.ast.get()) else {
            return Err(script_error("the workflow engine is not ready"));
        };
        let mut scope = Scope::new();
        scope.push_constant("args", args);
        engine.eval_ast_with_scope::<Dynamic>(&mut scope, ast)
    }));
    // Every thunk thread was joined inside `parallel`/`pipeline`; the engine goes now.
    drop(script);
    if run.token.is_cancelled() {
        run.end(
            Value::Null,
            Some((RunOutcome::Cancelled, "cancelled".to_string())),
        );
        return;
    }
    match result {
        Ok(Ok(value)) => match dynamic_to_json(&value) {
            Ok(value) => run.end(value, None),
            Err(error) => run.end(
                Value::Null,
                Some((
                    RunOutcome::Failed,
                    format!("the script's return value is not JSON: {error}"),
                )),
            ),
        },
        Ok(Err(error)) => run.end(
            Value::Null,
            Some((RunOutcome::Failed, runtime_message(error))),
        ),
        Err(_) => run.end(
            Value::Null,
            Some((RunOutcome::Failed, "the script engine panicked".to_string())),
        ),
    }
}
