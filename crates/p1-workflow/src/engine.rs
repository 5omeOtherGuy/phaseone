//! The rhai bridge (ADR-0053 item 3): a raw, limited engine; the workflow functions;
//! `agent()` with caps, schema repair and replay; bounded thunk threads.
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
    CallId, Counts, JournalRecord, ModelTry, MovedOn, ResolvedModel, RunId, RunOutcome,
    RunProgress, RunReport, SchemaCheck, StepEnd, StepEnvelope, StepLine, StepRequest, StepRunner,
    StepStatus, WorkerRef, WorkflowError, WorkflowObserver,
};
use crate::caps::CapCounter;
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
    pub(crate) observer: Arc<dyn WorkflowObserver>,
    pub(crate) roles: BTreeMap<String, Role>,
    pub(crate) caps: CapCounter,
    pub(crate) max_steps: u32,
    pub(crate) workspace: Option<PathBuf>,
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
    fn step(
        &self,
        prompt: &str,
        opts: &StepOptions,
        call: &CallId,
    ) -> Result<StepEnvelope, Box<EvalAltResult>> {
        lock(&self.record).steps_started += 1;
        let role = self.roles.get(&opts.role);
        let line = LineContext {
            role: opts.role.clone(),
            model: role.map(Role::head).unwrap_or_default(),
        };
        let refused = |error: String| StepEnvelope {
            step: call.clone(),
            label: opts.label.clone(),
            status: StepStatus::Failed,
            value: Value::Null,
            schema: SchemaCheck::NotRequested,
            evidence: None,
            attempts: 0,
            worker: None,
            needs: None,
            error: Some(error),
            models: Vec::new(),
        };

        let number = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
        if number > self.max_steps {
            let envelope = refused(format!("max_steps: {} reached", self.max_steps));
            return Ok(self.conclude(call, &line, envelope, false, &StepCost::default()));
        }

        let replayed = lock(&self.replay).take(call);
        if let Some((envelope, from)) = replayed {
            self.write(&JournalRecord::Replayed {
                call: call.clone(),
                from,
            });
            return Ok(self.conclude(call, &line, envelope, true, &StepCost::default()));
        }

        let Some(role) = role else {
            let envelope = refused(format!("unknown_role: {}", opts.role));
            return Ok(self.conclude(call, &line, envelope, false, &StepCost::default()));
        };
        let tools = opts.tools.clone().unwrap_or_else(|| role.tools.clone());
        let phase = opts
            .phase
            .clone()
            .or_else(|| lock(&self.record).phase.clone());

        // The chain, head first (ADR-0054 item 2). A link is left only for a route
        // failure or a cap; every other end stops the step (ADR-0054 item 3).
        let mut cost = StepCost::default();
        let mut walked: Vec<ModelTry> = Vec::new();
        let mut last_error = String::new();
        let mut last_worker: Option<String> = None;
        let mut route_failed = false;
        let mut envelope: Option<StepEnvelope> = None;
        for (index, model) in role.chain.iter().enumerate() {
            if let Some(previous) = walked.last() {
                // The hop is journalled BEFORE the next model is dispatched (ADR-0054
                // item 4), so a journal always shows why a link was left.
                self.write(&JournalRecord::Fallback {
                    call: call.clone(),
                    from: previous.model.clone(),
                    to: model.reference.clone(),
                    error: last_error.clone(),
                });
                cost.fell_back += 1;
            }
            let request = StepRequest {
                run: self.id.clone(),
                call: call.clone(),
                label: opts.label.clone(),
                phase: phase.clone(),
                role: opts.role.clone(),
                model: model.clone(),
                tools: tools.clone(),
                prompt: prompt.to_string(),
                schema: opts.schema.clone(),
                workspace: opts.workspace.clone().or_else(|| self.workspace.clone()),
                attempt: 1,
            };
            match self.link(&request, &opts.json, &mut cost) {
                Link::Ended(end) => {
                    walked.push(ModelTry {
                        model: request.model.reference.clone(),
                        moved_on: None,
                    });
                    envelope = Some(end);
                    break;
                }
                Link::MovedOn(moved) => {
                    route_failed |= moved.reason == MovedOn::RouteFailed;
                    last_error = moved.error;
                    last_worker = moved.worker;
                    // A reason is recorded only when the step really moves on: the LAST
                    // link of an exhausted chain stays bare, as the head of a role with
                    // no fallback does.
                    walked.push(ModelTry {
                        model: moved.model,
                        moved_on: (index + 1 < role.chain.len()).then_some(moved.reason),
                    });
                }
                // Cancelled: the run is over, whatever the chain still held.
                Link::Cancelled(end) => {
                    walked.push(ModelTry {
                        model: request.model.reference.clone(),
                        moved_on: None,
                    });
                    envelope = Some(end);
                    break;
                }
            }
        }
        let mut envelope = envelope.unwrap_or_else(|| StepEnvelope {
            // Every link was skipped: nothing ended the step, so the chain is what
            // failed (ADR-0054 item 4). The step names the worker of its LAST link.
            worker: last_worker,
            error: Some(if route_failed {
                format!("route: {last_error}")
            } else {
                last_error.clone()
            }),
            attempts: cost.attempts,
            ..refused(String::new())
        });
        envelope.models = walked;
        if envelope.status == StepStatus::Cancelled {
            self.conclude(call, &line, envelope, false, &cost);
            return Err(cancelled_error());
        }
        Ok(self.conclude(call, &line, envelope, false, &cost))
    }

    /// ONE link of a step's chain: the cap check, the dispatch and the turn, up to the
    /// schema repair (which never leaves the worker that produced the invalid result).
    fn link(&self, request: &StepRequest, opts: &Value, cost: &mut StepCost) -> Link {
        let blank = |attempts: u32| StepEnvelope {
            step: request.call.clone(),
            label: request.label.clone(),
            status: StepStatus::Failed,
            value: Value::Null,
            schema: SchemaCheck::NotRequested,
            evidence: None,
            attempts,
            worker: None,
            needs: None,
            error: None,
            models: Vec::new(),
        };
        match self.spend(request, opts, 1, cost) {
            // A cap is the one refusal a step walks past (ADR-0054 item 4).
            Some(Refused::Capped(error)) => {
                return Link::MovedOn(Moved {
                    reason: MovedOn::Capped,
                    model: request.model.reference.clone(),
                    error,
                    worker: None,
                });
            }
            // No `Dispatch` line: the attempt is unrecorded, so nothing runs.
            Some(Refused::Journal(error)) => {
                return Link::Ended(StepEnvelope {
                    error: Some(error),
                    ..blank(cost.attempts)
                });
            }
            None => {}
        }
        let outcome = match self.cancellable(self.runner.run(request, self.token.clone())) {
            None => {
                cost.attempts += 1;
                return Link::Cancelled(StepEnvelope {
                    status: StepStatus::Cancelled,
                    ..blank(cost.attempts)
                });
            }
            Some(Err(reason)) => {
                // The runner could not start the worker at all: the host's reason, never
                // a hop (ADR-0054 item 3 — a route failure is a route's own report).
                cost.attempts += 1;
                return Link::Ended(StepEnvelope {
                    error: Some(reason),
                    ..blank(cost.attempts)
                });
            }
            Some(Ok(outcome)) => outcome,
        };
        cost.attempts += 1;
        self.observer
            .step_started(&self.id, request, &outcome.worker);
        let worker = outcome.worker;
        let worker_line = format!("{} ({})", worker.id, worker.description);
        let base = StepEnvelope {
            worker: Some(worker_line.clone()),
            ..blank(cost.attempts)
        };

        match outcome.end {
            StepEnd::RouteFailed { model, error } => Link::MovedOn(Moved {
                reason: MovedOn::RouteFailed,
                // The host names the model it could not run; the engine's own answer
                // stands when a runner names none.
                model: if model.is_empty() {
                    request.model.reference.clone()
                } else {
                    model
                },
                error,
                worker: Some(worker_line),
            }),
            StepEnd::Done {
                evidence,
                result,
                schema: SchemaCheck::Failed(errors),
                ..
            } => {
                let rejected = StepEnvelope {
                    value: result.unwrap_or(Value::Null),
                    schema: SchemaCheck::Failed(errors.clone()),
                    evidence: Some(evidence),
                    error: None,
                    ..base.clone()
                };
                match self.repair(request, opts, &worker, &errors, rejected, cost) {
                    Some(envelope) => Link::Ended(envelope),
                    None => Link::Cancelled(StepEnvelope {
                        status: StepStatus::Cancelled,
                        attempts: cost.attempts,
                        ..base
                    }),
                }
            }
            end => Link::Ended(envelope_from_end(end, cost.attempts, base)),
        }
    }

    /// Cap check and `Dispatch` line for one attempt; `Some` when the attempt could not
    /// be dispatched, which also charges the step's capped count (ADR-0054 item 4).
    fn spend(
        &self,
        request: &StepRequest,
        opts: &Value,
        attempt: u32,
        cost: &mut StepCost,
    ) -> Option<Refused> {
        let wire_model = &request.model.wire_model;
        if let Err((used, limit)) = self.caps.try_spend(wire_model) {
            self.write(&JournalRecord::Capped {
                call: request.call.clone(),
                wire_model: wire_model.clone(),
                used,
                limit,
            });
            cost.capped += 1;
            let repair = if attempt > 1 { " (repair)" } else { "" };
            return Some(Refused::Capped(format!(
                "quota_exceeded: {wire_model} used={used} limit={limit}{repair}"
            )));
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
            return Some(Refused::Journal(format!("journal: {error}")));
        }
        None
    }

    /// The one schema repair turn (item 5): another turn in the SAME worker that produced
    /// the invalid result, never a new model (ADR-0054 item 3). `rejected` is the envelope
    /// if the repair cannot run. `None` when the run is cancelled meanwhile.
    fn repair(
        &self,
        request: &StepRequest,
        opts: &Value,
        worker: &WorkerRef,
        errors: &[String],
        rejected: StepEnvelope,
        cost: &mut StepCost,
    ) -> Option<StepEnvelope> {
        if let Some(refused) = self.spend(request, opts, 2, cost) {
            // A capped repair and an unwritable journal both keep the invalid value.
            return Some(StepEnvelope {
                error: Some(refused.error()),
                ..rejected
            });
        }
        cost.attempts += 1;
        let end = self.cancellable(self.runner.repair(
            worker,
            repair_message(errors),
            self.token.clone(),
        ))?;
        Some(match end {
            Err(reason) => StepEnvelope {
                attempts: cost.attempts,
                error: Some(reason),
                ..rejected
            },
            Ok(StepEnd::Done {
                evidence,
                result,
                schema: SchemaCheck::Failed(errors),
                ..
            }) => StepEnvelope {
                status: StepStatus::Failed,
                value: result.unwrap_or(rejected.value),
                error: Some(format!("invalid_output: {}", errors.join("; "))),
                schema: SchemaCheck::Failed(errors),
                evidence: Some(evidence),
                attempts: cost.attempts,
                ..rejected
            },
            Ok(end) => envelope_from_end(
                end,
                cost.attempts,
                StepEnvelope {
                    value: Value::Null,
                    schema: SchemaCheck::NotRequested,
                    evidence: None,
                    ..rejected
                },
            ),
        })
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
}

/// Why ONE attempt was not dispatched.
enum Refused {
    /// The cap refused it: the step may walk past this link (ADR-0054 item 4).
    Capped(String),
    /// The `Dispatch` line could not be written: nothing ran and nothing is charged.
    Journal(String),
}

impl Refused {
    fn error(self) -> String {
        match self {
            Refused::Capped(error) | Refused::Journal(error) => error,
        }
    }
}

/// What ONE step's chain spent (ADR-0054 item 4): dispatches (starts plus repairs), the
/// links a cap refused, and the hops between links.
#[derive(Default)]
struct StepCost {
    attempts: u32,
    capped: u32,
    fell_back: u32,
}

/// One link of a step's chain the step moves on from.
struct Moved {
    reason: MovedOn,
    /// The model the link ran (or was refused for), as the runner reported it.
    model: String,
    /// Why the step moves on: the route error, or the cap refusal.
    error: String,
    /// The worker the link ran, as a step line names it; `None` for a capped link,
    /// where nothing was built.
    worker: Option<String>,
}

/// What one link of a step's chain decided (ADR-0054 item 4).
enum Link {
    /// The step ended on this link.
    Ended(StepEnvelope),
    /// The step moves to the next link.
    MovedOn(Moved),
    /// The run was cancelled while this link ran.
    Cancelled(StepEnvelope),
}

fn envelope_from_end(end: StepEnd, attempts: u32, base: StepEnvelope) -> StepEnvelope {
    let base = StepEnvelope { attempts, ..base };
    match end {
        StepEnd::Done {
            summary,
            evidence,
            result,
            schema,
        } => StepEnvelope {
            status: StepStatus::Done,
            value: match (&schema, result) {
                (SchemaCheck::Passed, Some(result)) => result,
                _ => Value::String(summary),
            },
            schema,
            evidence: Some(evidence),
            error: None,
            ..base
        },
        StepEnd::Blocked { needs, .. } => StepEnvelope {
            status: StepStatus::Blocked,
            needs: Some(needs),
            error: None,
            ..base
        },
        StepEnd::RouteFailed { error, .. } => StepEnvelope {
            status: StepStatus::Failed,
            value: Value::Null,
            error: Some(format!("route: {error}")),
            ..base
        },
        StepEnd::EndedWithoutFinish { text } => StepEnvelope {
            status: StepStatus::Failed,
            value: Value::String(text),
            error: Some("ended without finish".to_string()),
            ..base
        },
        StepEnd::Failed(message) => StepEnvelope {
            status: StepStatus::Failed,
            error: Some(message),
            ..base
        },
        StepEnd::Cancelled => StepEnvelope {
            status: StepStatus::Cancelled,
            error: None,
            ..base
        },
    }
}

fn repair_message(errors: &[String]) -> String {
    let mut message = String::from("Your result did not match the required schema:\n");
    for error in errors {
        message.push_str("- ");
        message.push_str(error);
        message.push('\n');
    }
    message.push_str(
        "Call finish again with a corrected \"result\" that matches the schema of the \"result\" parameter.",
    );
    message
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
                "schema" if value.is_object() => parsed.schema = Some(value.clone()),
                "schema" => return Err(script_error("agent: option \"schema\" must be a map")),
                "tools" => parsed.tools = Some(parse_tools(value)?),
                other => return Err(script_error(format!("agent: unknown option \"{other}\""))),
            }
        }
        parsed.json = json;
        Ok(parsed)
    }
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
/// three sums, so one run can hold this much per VALUE, and `max_threads` (64) thunks may
/// be in flight at once. The ceiling is therefore this cap times that concurrency, not the
/// step count.
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
        let mut pending = Vec::with_capacity(jobs.len());
        for job in jobs {
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
