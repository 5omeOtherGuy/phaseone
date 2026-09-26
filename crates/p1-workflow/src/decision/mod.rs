//! A workflow's decisions (S0-R1.1, the `workflow-decision` world): what a step does next and
//! what an attempt's end means. Pure and synchronous: no I/O, no clock, no lock, no callback
//! into the substrate. The substrate (`engine.rs`) owns every piece of state, passes a
//! [`Snapshot`] in, checks the [`Transition`] that comes back and applies it.
//!
//! [`plan_step`] and [`accept_step`] are the decisions themselves; [`NativeDecisions`] serves
//! them through the [`Decisions`] seam, and [`plan_step_json`] / [`accept_step_json`] are the
//! same calls over JSON text, so a decision component is a thin shell over this code.

mod contract;

pub use contract::*;

use serde_json::Value;

use crate::api::{
    CallId, ModelTry, MovedOn, ResolvedModel, SchemaCheck, StepEnd, StepEnvelope, StepStatus,
    WorkerRef,
};

/// The seam between the substrate and its decisions. The substrate holds one
/// `Arc<dyn Decisions>`, chosen by its constructor.
///
/// The contract every implementation keeps:
///
/// - A call is short and self-contained: it returns before the substrate dispatches, waits,
///   journals or returns into the script, and it never calls back into the substrate, the
///   runner, the observer or the script.
/// - The substrate calls it from several OS threads at once: `parallel` thunks and `pipeline`
///   items each reach `agent()` on their own thread. An implementation must be safe under
///   concurrent calls; the native one keeps no state at all.
/// - A call is never entered from inside another call on the same thread. A component
///   implementation therefore needs one Store per call or a pool of instances, never one
///   Store shared across threads.
pub trait Decisions: Send + Sync {
    /// The next move of a step, given the run's `snapshot` and the call that asks.
    fn plan_step(&self, snapshot: &Snapshot, request: &PlanRequest) -> Result<Transition, String>;

    /// What the `outcome` of the step's latest attempt means, given the run's `snapshot`.
    fn accept_step(
        &self,
        snapshot: &Snapshot,
        outcome: &AttemptOutcome,
    ) -> Result<Transition, String>;
}

/// The decisions compiled into this crate: [`plan_step`] and [`accept_step`], stateless.
#[derive(Debug, Clone, Copy, Default)]
pub struct NativeDecisions;

impl Decisions for NativeDecisions {
    fn plan_step(&self, snapshot: &Snapshot, request: &PlanRequest) -> Result<Transition, String> {
        plan_step(snapshot, request)
    }

    fn accept_step(
        &self,
        snapshot: &Snapshot,
        outcome: &AttemptOutcome,
    ) -> Result<Transition, String> {
        accept_step(snapshot, outcome)
    }
}

/// [`plan_step`] over JSON text: the shape of the world's `plan-step` export.
pub fn plan_step_json(snapshot: &str, request: &str) -> Result<String, String> {
    let snapshot: Snapshot =
        serde_json::from_str(snapshot).map_err(|error| format!("snapshot: {error}"))?;
    let request: PlanRequest =
        serde_json::from_str(request).map_err(|error| format!("request: {error}"))?;
    let transition = plan_step(&snapshot, &request)?;
    serde_json::to_string(&transition).map_err(|error| format!("transition: {error}"))
}

/// [`accept_step`] over JSON text: the shape of the world's `accept-step` export.
pub fn accept_step_json(snapshot: &str, outcome: &str) -> Result<String, String> {
    let snapshot: Snapshot =
        serde_json::from_str(snapshot).map_err(|error| format!("snapshot: {error}"))?;
    let outcome: AttemptOutcome =
        serde_json::from_str(outcome).map_err(|error| format!("outcome: {error}"))?;
    let transition = accept_step(&snapshot, &outcome)?;
    serde_json::to_string(&transition).map_err(|error| format!("transition: {error}"))
}

/// The next move of a step (ADR-0053, ADR-0054 item 2). On the step's first plan, in this
/// order: the `max_steps` refusal, the replay prefix, the unknown role, the worktree that
/// has no base, then the head of the chain. After the step moved on from a link: the next
/// link, or the end of an exhausted chain.
pub fn plan_step(snapshot: &Snapshot, request: &PlanRequest) -> Result<Transition, String> {
    check_version("snapshot", snapshot.version)?;
    check_version("request", request.version)?;
    let step = &snapshot.step;
    let blank = Blank {
        call: &request.call,
        label: &request.label,
    };
    let end = |envelope: StepEnvelope, latch_replay: bool| {
        Ok(Transition::new(Action::End {
            envelope,
            latch_replay,
        }))
    };

    if let Some(link) = step.link {
        if !step.moved_on {
            return Err(format!(
                "plan_step: the step is still on link {link}: nothing to plan"
            ));
        }
        let role = snapshot
            .role
            .as_ref()
            .ok_or("plan_step: a dispatched step has no role")?;
        let next = link + 1;
        return match chain_link(&role.chain, next) {
            Some(model) => Ok(Transition::new(Action::Dispatch {
                link: next,
                model: model.reference.clone(),
                tools: tools(request, role),
                latch_replay: false,
            })),
            None => end(exhausted(&blank, step), false),
        };
    }

    // Taken before anything can refuse the step, and never consulting the replay prefix:
    // a call past the cap is refused without latching replay off.
    if step.ordinal > snapshot.max_steps {
        let error = format!("max_steps: {} reached", snapshot.max_steps);
        return end(blank.refused(error), false);
    }
    if !snapshot.replay.latched_off
        && let Some(entry) = snapshot
            .replay
            .open
            .iter()
            .find(|entry| entry.call == request.call)
    {
        return Ok(Transition::new(Action::Replay { entry: entry.index }));
    }
    // From here on the replay prefix did not answer this call: every move latches it off.
    let Some(role) = &snapshot.role else {
        return end(
            blank.refused(format!("unknown_role: {}", request.role)),
            true,
        );
    };
    let Some(head) = role.chain.first() else {
        // A role with no model at all: nothing ended the step, so the chain failed.
        return end(exhausted(&blank, step), true);
    };
    if let Some(slug) = &request.worktree
        && !snapshot.has_base
    {
        let error = format!(
            "worktree: {slug}: the run has no base commit (its workspace is not a git repository)"
        );
        return end(blank.refused(error), true);
    }
    Ok(Transition::new(Action::Dispatch {
        link: 0,
        model: head.reference.clone(),
        tools: tools(request, role),
        latch_replay: true,
    }))
}

/// What an attempt's end means (ADR-0053 item 5, ADR-0054 items 3 and 4, ADR-0072): the
/// step ends, gets its one repair turn, moves on to the next link, or is cancelled.
pub fn accept_step(snapshot: &Snapshot, outcome: &AttemptOutcome) -> Result<Transition, String> {
    check_version("snapshot", snapshot.version)?;
    check_version("outcome", outcome.version)?;
    let step = &snapshot.step;
    let blank = Blank {
        call: &outcome.call,
        label: &outcome.label,
    };
    let attempts = step.cost.attempts;

    // Before a link: only the worktree can have ended the step.
    let Some(link) = step.link else {
        return match &outcome.attempt {
            Attempt::WorktreeRefused { error } => Ok(Transition::new(Action::End {
                envelope: blank.refused(error.clone()),
                latch_replay: false,
            })),
            Attempt::WorktreeCancelled => Ok(Transition::new(Action::Cancelled {
                envelope: StepEnvelope {
                    status: StepStatus::Cancelled,
                    error: None,
                    ..blank.refused(String::new())
                },
            })),
            other => Err(format!(
                "accept_step: {} before any link was dispatched",
                attempt_name(other)
            )),
        };
    };
    let role = snapshot
        .role
        .as_ref()
        .ok_or("accept_step: a dispatched step has no role")?;
    let model = chain_link(&role.chain, link)
        .ok_or_else(|| format!("accept_step: link {link} is not in the role's chain"))?;
    // A link the step ends on is named bare; a link it leaves names why, except the LAST
    // link of an exhausted chain, which stays bare as the head of a role with no fallback
    // does (ADR-0054 item 4).
    let is_last = link as usize + 1 >= role.chain.len();
    let models = |mut envelope: StepEnvelope| {
        let mut walked = step.walked.clone();
        walked.push(ModelTry {
            model: model.reference.clone(),
            moved_on: None,
        });
        envelope.models = walked;
        envelope
    };
    let end = |envelope: StepEnvelope| {
        Ok(Transition::new(Action::End {
            envelope: models(envelope),
            latch_replay: false,
        }))
    };
    let cancelled = |envelope: StepEnvelope| {
        Ok(Transition::new(Action::Cancelled {
            envelope: models(envelope),
        }))
    };
    let repair = step.repair.as_ref();

    match &outcome.attempt {
        Attempt::WorktreeRefused { .. } | Attempt::WorktreeCancelled => Err(format!(
            "accept_step: {} after link {link} was dispatched",
            attempt_name(&outcome.attempt)
        )),
        Attempt::Capped {
            wire_model,
            used,
            limit,
        } => {
            let suffix = if repair.is_some() { " (repair)" } else { "" };
            let error = format!("quota_exceeded: {wire_model} used={used} limit={limit}{suffix}");
            match repair {
                // A capped repair keeps the rejected value.
                Some(repair) => end(StepEnvelope {
                    error: Some(error),
                    ..repair.rejected.clone()
                }),
                // A cap is the one refusal a step walks past (ADR-0054 item 4).
                None => Ok(Transition::new(Action::MoveOn {
                    reason: MovedOn::Capped,
                    tried: ModelTry {
                        model: model.reference.clone(),
                        moved_on: (!is_last).then_some(MovedOn::Capped),
                    },
                    error,
                    worker: None,
                })),
            }
        }
        // An unwritable journal: the attempt is unrecorded, so nothing ran.
        Attempt::JournalFailed { error } => match repair {
            Some(repair) => end(StepEnvelope {
                error: Some(error.clone()),
                ..repair.rejected.clone()
            }),
            None => end(StepEnvelope {
                error: Some(error.clone()),
                ..blank.failed(attempts)
            }),
        },
        // The runner could not start the worker at all: the host's reason, never a hop
        // (ADR-0054 item 3: a route failure is a route's own report).
        Attempt::RunnerRefused { reason } => match repair {
            Some(repair) => end(StepEnvelope {
                attempts,
                error: Some(reason.clone()),
                ..repair.rejected.clone()
            }),
            None => end(StepEnvelope {
                error: Some(reason.clone()),
                ..blank.failed(attempts)
            }),
        },
        // Cancelled: the run is over, whatever the chain still held.
        Attempt::Cancelled => match repair {
            Some(repair) => cancelled(StepEnvelope {
                status: StepStatus::Cancelled,
                worker: Some(worker_line(&repair.worker)),
                ..blank.failed(attempts)
            }),
            None => cancelled(StepEnvelope {
                status: StepStatus::Cancelled,
                ..blank.failed(attempts)
            }),
        },
        Attempt::Ended { worker, end: ended } => {
            if repair.is_some() {
                return Err("accept_step: a first turn ended during the repair turn".into());
            }
            let line = worker_line(worker);
            let base = StepEnvelope {
                worker: Some(line.clone()),
                ..blank.failed(attempts)
            };
            // A failed contract and a turn that ended without `finish` both get the ONE
            // repair turn of the same worker (item 5, ADR-0072); every other end is the step's.
            match ended {
                StepEnd::RouteFailed {
                    model: reported,
                    error,
                } => Ok(Transition::new(Action::MoveOn {
                    reason: MovedOn::RouteFailed,
                    tried: ModelTry {
                        // The host names the model it could not run; the chain's own answer
                        // stands when a runner names none.
                        model: if reported.is_empty() {
                            model.reference.clone()
                        } else {
                            reported.clone()
                        },
                        moved_on: (!is_last).then_some(MovedOn::RouteFailed),
                    },
                    error: error.clone(),
                    worker: Some(line),
                })),
                StepEnd::Done {
                    evidence,
                    result,
                    schema: SchemaCheck::Failed(errors),
                    ..
                } => Ok(Transition::new(Action::Repair {
                    message: repair_message(errors),
                    rejected: StepEnvelope {
                        value: result.clone().unwrap_or(Value::Null),
                        schema: SchemaCheck::Failed(errors.clone()),
                        evidence: Some(evidence.clone()),
                        error: None,
                        ..base
                    },
                })),
                StepEnd::EndedWithoutFinish { .. } => Ok(Transition::new(Action::Repair {
                    message: FINISH_NUDGE.to_string(),
                    rejected: envelope_from_end(ended.clone(), attempts, base),
                })),
                other => {
                    let envelope = envelope_from_end(other.clone(), attempts, base);
                    if envelope.status == StepStatus::Cancelled {
                        cancelled(envelope)
                    } else {
                        end(envelope)
                    }
                }
            }
        }
        Attempt::RepairEnded { end: ended } => {
            let rejected = &repair
                .ok_or("accept_step: a repair turn ended but none was asked for")?
                .rejected;
            let envelope = match ended {
                StepEnd::Done {
                    evidence,
                    result,
                    schema: SchemaCheck::Failed(errors),
                    ..
                } => StepEnvelope {
                    status: StepStatus::Failed,
                    value: result.clone().unwrap_or_else(|| rejected.value.clone()),
                    error: Some(format!("invalid_output: {}", errors.join("; "))),
                    schema: SchemaCheck::Failed(errors.clone()),
                    evidence: Some(evidence.clone()),
                    attempts,
                    ..rejected.clone()
                },
                other => envelope_from_end(
                    other.clone(),
                    attempts,
                    StepEnvelope {
                        value: Value::Null,
                        schema: SchemaCheck::NotRequested,
                        evidence: None,
                        ..rejected.clone()
                    },
                ),
            };
            if envelope.status == StepStatus::Cancelled {
                cancelled(envelope)
            } else {
                end(envelope)
            }
        }
    }
}

/// The call's own grant, or the role's.
fn tools(request: &PlanRequest, role: &RoleView) -> Vec<String> {
    request.tools.clone().unwrap_or_else(|| role.tools.clone())
}

fn chain_link(chain: &[ResolvedModel], link: u32) -> Option<&ResolvedModel> {
    chain.get(usize::try_from(link).ok()?)
}

fn attempt_name(attempt: &Attempt) -> &'static str {
    match attempt {
        Attempt::WorktreeRefused { .. } => "worktree_refused",
        Attempt::WorktreeCancelled => "worktree_cancelled",
        Attempt::Capped { .. } => "capped",
        Attempt::JournalFailed { .. } => "journal_failed",
        Attempt::RunnerRefused { .. } => "runner_refused",
        Attempt::Cancelled => "cancelled",
        Attempt::Ended { .. } => "ended",
        Attempt::RepairEnded { .. } => "repair_ended",
    }
}

/// `<id> (<route/model>)`: how an envelope and a step line name a worker.
fn worker_line(worker: &WorkerRef) -> String {
    format!("{} ({})", worker.id, worker.description)
}

/// The call an envelope answers, for the envelopes built from nothing.
struct Blank<'a> {
    call: &'a CallId,
    label: &'a Option<String>,
}

impl Blank<'_> {
    /// A `failed` envelope with nothing in it yet.
    fn failed(&self, attempts: u32) -> StepEnvelope {
        StepEnvelope {
            step: self.call.clone(),
            label: self.label.clone(),
            status: StepStatus::Failed,
            value: Value::Null,
            schema: SchemaCheck::NotRequested,
            evidence: None,
            attempts,
            worker: None,
            needs: None,
            error: None,
            models: Vec::new(),
            worktree: None,
        }
    }

    /// A step refused before anything was dispatched.
    fn refused(&self, error: String) -> StepEnvelope {
        StepEnvelope {
            error: Some(error),
            ..self.failed(0)
        }
    }
}

/// Every link was skipped: nothing ended the step, so the chain is what failed (ADR-0054
/// item 4). The step names the worker of its LAST link.
fn exhausted(blank: &Blank<'_>, step: &StepProgress) -> StepEnvelope {
    StepEnvelope {
        worker: step.last_worker.clone(),
        error: Some(if step.route_failed {
            format!("route: {}", step.last_error)
        } else {
            step.last_error.clone()
        }),
        attempts: step.cost.attempts,
        models: step.walked.clone(),
        ..blank.refused(String::new())
    }
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

/// The repair turn's message to a worker whose turn ended without `finish` (ADR-0072).
const FINISH_NUDGE: &str = "You ended your turn without calling finish. Call finish now: status \"done\" with your result (and the evidence), or \"blocked\" with what you need.";

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

#[cfg(test)]
mod tests;
