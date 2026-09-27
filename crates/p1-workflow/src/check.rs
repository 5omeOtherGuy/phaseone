//! The substrate's check of every transition a decision answers, before anything is applied.
//! A refused transition fails its step with the reason; it never panics and never reaches
//! the journal, the runner or the counters.

use crate::api::{CallId, MovedOn, StepEnd, StepEnvelope, StepStatus};
use crate::decision::{Action, Attempt, Snapshot, Transition, check_version};

/// What the substrate asked the decision.
pub(crate) enum Ask {
    /// `plan-step`: the step's next move.
    Plan,
    /// `accept-step`: what this attempt's end means.
    Accept(Attempt),
}

/// The transition's action when the substrate may apply it to the step `snapshot` shows
/// for `call`, asked as `ask`; otherwise why not.
pub(crate) fn transition(
    snapshot: &Snapshot,
    call: &CallId,
    ask: &Ask,
    transition: Transition,
) -> Result<Action, String> {
    check_version("transition", transition.version)?;
    let step = &snapshot.step;
    let action = transition.action;
    match (&action, ask) {
        (Action::Replay { entry }, Ask::Plan) => {
            let held = step.link.is_none()
                && !snapshot.replay.latched_off
                && snapshot
                    .replay
                    .open
                    .iter()
                    .any(|open| open.index == *entry && open.call == *call);
            if !held {
                return Err(format!(
                    "it replays entry {entry}, which the journal does not hold for call {}",
                    call.0
                ));
            }
        }
        (Action::Dispatch { link, model, .. }, Ask::Plan) => {
            let named = snapshot
                .role
                .as_ref()
                .and_then(|role| role.chain.get(usize::try_from(*link).ok()?))
                .is_some_and(|resolved| resolved.reference == *model);
            if !named {
                return Err(format!(
                    "it names {model} (link {link}), which is not in the role's chain"
                ));
            }
            let next = match step.link {
                None => *link == 0,
                Some(current) => step.moved_on && current.checked_add(1) == Some(*link),
            };
            if !next {
                return Err(format!(
                    "it dispatches link {link} without a route failure or a cap on the link before"
                ));
            }
        }
        (Action::MoveOn { reason, .. }, Ask::Accept(attempt)) => {
            // A link is left only for a route failure or a cap, and only on its first
            // attempt: a capped repair ends the step (ADR-0054 item 3).
            let earned = step.repair.is_none()
                && matches!(
                    (reason, attempt),
                    (
                        MovedOn::RouteFailed,
                        Attempt::Ended {
                            end: StepEnd::RouteFailed { .. },
                            ..
                        },
                    ) | (MovedOn::Capped, Attempt::Capped { .. })
                );
            if !earned {
                return Err(
                    "it moves on to the next link without a route failure or a cap".to_string(),
                );
            }
        }
        (Action::Repair { rejected, .. }, Ask::Accept(attempt)) => {
            if step.repair.is_some() || matches!(attempt, Attempt::RepairEnded { .. }) {
                return Err("it asks for a second repair turn".to_string());
            }
            if !matches!(attempt, Attempt::Ended { .. }) {
                return Err("it asks for a repair of a turn that did not run".to_string());
            }
            envelope(snapshot, call, rejected)?;
        }
        (
            Action::End {
                envelope: ended, ..
            },
            _,
        ) => {
            if ended.status == StepStatus::Cancelled {
                return Err("it ends the step cancelled without a cancellation".to_string());
            }
            envelope(snapshot, call, ended)?;
        }
        (Action::Cancelled { envelope: ended }, _) => {
            if ended.status != StepStatus::Cancelled {
                return Err("it cancels the step with an envelope that is not cancelled".into());
            }
            envelope(snapshot, call, ended)?;
        }
        (Action::Replay { .. } | Action::Dispatch { .. }, Ask::Accept(_)) => {
            return Err("accept_step answered with a plan".to_string());
        }
        (Action::MoveOn { .. } | Action::Repair { .. }, Ask::Plan) => {
            return Err("plan_step answered for an attempt that did not run".to_string());
        }
    }
    Ok(action)
}

/// An envelope answers this call and carries the attempts the substrate counted.
fn envelope(snapshot: &Snapshot, call: &CallId, envelope: &StepEnvelope) -> Result<(), String> {
    if envelope.step != *call {
        return Err(format!(
            "its envelope answers call {}, not {}",
            envelope.step.0, call.0
        ));
    }
    let attempts = snapshot.step.cost.attempts;
    if envelope.attempts != attempts {
        return Err(format!(
            "it changes a counter: attempts {} where the step spent {attempts}",
            envelope.attempts
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use serde_json::Value;

    use super::*;
    use crate::api::{ModelTry, ResolvedModel, RunId, SchemaCheck, WorkerRef};
    use crate::decision::{
        CONTRACT_VERSION, RepairTurn, ReplayEntry, ReplayView, RoleView, StepProgress,
    };

    fn model(reference: &str) -> ResolvedModel {
        ResolvedModel {
            reference: reference.to_string(),
            environment: "env".into(),
            profile: reference.to_string(),
            effort: None,
            wire_model: reference.to_string(),
        }
    }

    fn call() -> CallId {
        CallId("c1".into())
    }

    /// A step of role `head → next`, with one replayable entry for another call.
    fn snapshot() -> Snapshot {
        Snapshot {
            version: CONTRACT_VERSION,
            run: RunId("wf1".into()),
            max_steps: 10,
            has_base: false,
            role: Some(RoleView {
                chain: vec![model("env/head"), model("env/next")],
                tools: vec!["read".into()],
            }),
            replay: ReplayView {
                latched_off: false,
                open: vec![
                    ReplayEntry {
                        index: 0,
                        call: CallId("other".into()),
                    },
                    ReplayEntry {
                        index: 1,
                        call: call(),
                    },
                ],
            },
            step: StepProgress {
                ordinal: 1,
                ..StepProgress::default()
            },
        }
    }

    fn on_link(link: u32, attempts: u32) -> Snapshot {
        let mut snapshot = snapshot();
        snapshot.step.link = Some(link);
        snapshot.step.cost.attempts = attempts;
        snapshot
    }

    fn envelope(attempts: u32, status: StepStatus) -> StepEnvelope {
        StepEnvelope {
            step: call(),
            label: None,
            status,
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

    fn worker() -> WorkerRef {
        WorkerRef {
            id: "w1".into(),
            description: "env/head".into(),
        }
    }

    fn ended(end: StepEnd) -> Ask {
        Ask::Accept(Attempt::Ended {
            worker: worker(),
            end,
        })
    }

    fn dispatch(link: u32, model: &str) -> Action {
        Action::Dispatch {
            link,
            model: model.into(),
            tools: vec!["read".into()],
            latch_replay: true,
        }
    }

    fn move_on(reason: MovedOn) -> Action {
        Action::MoveOn {
            reason,
            tried: ModelTry {
                model: "env/head".into(),
                moved_on: Some(reason),
            },
            error: "down".into(),
            worker: None,
        }
    }

    fn repair(attempts: u32) -> Action {
        Action::Repair {
            message: "again".into(),
            rejected: envelope(attempts, StepStatus::Failed),
        }
    }

    fn check(snapshot: &Snapshot, ask: &Ask, action: Action) -> Result<Action, String> {
        transition(snapshot, &call(), ask, Transition::new(action))
    }

    fn refused(snapshot: &Snapshot, ask: &Ask, action: Action) -> String {
        check(snapshot, ask, action).expect_err("the transition must be refused")
    }

    fn route_failed() -> StepEnd {
        StepEnd::RouteFailed {
            model: String::new(),
            error: "down".into(),
        }
    }

    #[test]
    fn the_transitions_of_a_well_formed_step_are_applied() {
        let snapshot = snapshot();
        assert!(check(&snapshot, &Ask::Plan, dispatch(0, "env/head")).is_ok());
        assert!(check(&snapshot, &Ask::Plan, Action::Replay { entry: 1 }).is_ok());
        let failed = StepEnd::Failed("no".into());
        let end = Action::End {
            envelope: envelope(1, StepStatus::Failed),
            latch_replay: false,
        };
        assert!(check(&on_link(0, 1), &ended(failed), end).is_ok());
        assert!(
            check(
                &on_link(0, 1),
                &ended(route_failed()),
                move_on(MovedOn::RouteFailed)
            )
            .is_ok()
        );
        let capped = Ask::Accept(Attempt::Capped {
            wire_model: "head".into(),
            used: 1,
            limit: 1,
        });
        assert!(check(&on_link(0, 0), &capped, move_on(MovedOn::Capped)).is_ok());
        let mut moved = on_link(0, 1);
        moved.step.moved_on = true;
        assert!(check(&moved, &Ask::Plan, dispatch(1, "env/next")).is_ok());
        let nudge = ended(StepEnd::EndedWithoutFinish { text: "t".into() });
        assert!(check(&on_link(0, 1), &nudge, repair(1)).is_ok());
        let cancelled = Action::Cancelled {
            envelope: envelope(1, StepStatus::Cancelled),
        };
        assert!(check(&on_link(0, 1), &Ask::Accept(Attempt::Cancelled), cancelled).is_ok());
    }

    #[test]
    fn a_model_outside_the_roles_chain_is_refused() {
        let error = refused(&snapshot(), &Ask::Plan, dispatch(0, "env/elsewhere"));
        assert!(error.contains("not in the role's chain"), "{error}");
        let error = refused(&snapshot(), &Ask::Plan, dispatch(2, "env/next"));
        assert!(error.contains("not in the role's chain"), "{error}");
        let mut unknown = snapshot();
        unknown.role = None;
        let error = refused(&unknown, &Ask::Plan, dispatch(0, "env/head"));
        assert!(error.contains("not in the role's chain"), "{error}");
    }

    #[test]
    fn a_skipped_link_is_refused() {
        // The first dispatch is the head's.
        let error = refused(&snapshot(), &Ask::Plan, dispatch(1, "env/next"));
        assert!(
            error.contains("without a route failure or a cap"),
            "{error}"
        );
        // The next link only after the step moved on from this one.
        let error = refused(&on_link(0, 1), &Ask::Plan, dispatch(1, "env/next"));
        assert!(
            error.contains("without a route failure or a cap"),
            "{error}"
        );
        // Moving on from a turn that failed, or naming the wrong reason.
        let failed = ended(StepEnd::Failed("wrong answer".into()));
        let error = refused(&on_link(0, 1), &failed, move_on(MovedOn::RouteFailed));
        assert!(
            error.contains("without a route failure or a cap"),
            "{error}"
        );
        let error = refused(
            &on_link(0, 1),
            &ended(route_failed()),
            move_on(MovedOn::Capped),
        );
        assert!(
            error.contains("without a route failure or a cap"),
            "{error}"
        );
        // A capped repair ends the step; it never walks the chain.
        let mut repairing = on_link(0, 1);
        repairing.step.repair = Some(RepairTurn {
            worker: worker(),
            rejected: envelope(1, StepStatus::Failed),
        });
        let capped = Ask::Accept(Attempt::Capped {
            wire_model: "head".into(),
            used: 1,
            limit: 1,
        });
        let error = refused(&repairing, &capped, move_on(MovedOn::Capped));
        assert!(
            error.contains("without a route failure or a cap"),
            "{error}"
        );
    }

    #[test]
    fn a_second_repair_is_refused() {
        let mut repairing = on_link(0, 2);
        repairing.step.repair = Some(RepairTurn {
            worker: worker(),
            rejected: envelope(1, StepStatus::Failed),
        });
        let again = Ask::Accept(Attempt::RepairEnded {
            end: StepEnd::EndedWithoutFinish { text: "t".into() },
        });
        let error = refused(&repairing, &again, repair(2));
        assert!(error.contains("second repair"), "{error}");
        let error = refused(&on_link(0, 1), &Ask::Accept(Attempt::Cancelled), repair(1));
        assert!(error.contains("did not run"), "{error}");
    }

    #[test]
    fn a_replay_the_journal_does_not_hold_is_refused() {
        let snapshot = snapshot();
        for entry in [0, 7] {
            let error = refused(&snapshot, &Ask::Plan, Action::Replay { entry });
            assert!(error.contains("does not hold"), "{error}");
        }
        let mut latched = snapshot.clone();
        latched.replay.latched_off = true;
        let error = refused(&latched, &Ask::Plan, Action::Replay { entry: 1 });
        assert!(error.contains("does not hold"), "{error}");
        let error = refused(&on_link(0, 1), &Ask::Plan, Action::Replay { entry: 1 });
        assert!(error.contains("does not hold"), "{error}");
    }

    #[test]
    fn a_changed_counter_is_refused() {
        let end = |attempts| Action::End {
            envelope: envelope(attempts, StepStatus::Done),
            latch_replay: false,
        };
        let done = ended(StepEnd::Failed("x".into()));
        let error = refused(&on_link(0, 1), &done, end(0));
        assert!(error.contains("changes a counter"), "{error}");
        let error = refused(&on_link(0, 1), &done, end(5));
        assert!(
            error.contains("attempts 5 where the step spent 1"),
            "{error}"
        );
        let nudge = ended(StepEnd::EndedWithoutFinish { text: "t".into() });
        let error = refused(&on_link(0, 1), &nudge, repair(2));
        assert!(error.contains("changes a counter"), "{error}");
    }

    #[test]
    fn a_transition_of_another_version_or_shape_is_refused() {
        let newer = Transition {
            version: CONTRACT_VERSION + 1,
            action: dispatch(0, "env/head"),
        };
        let error = transition(&snapshot(), &call(), &Ask::Plan, newer).unwrap_err();
        assert_eq!(
            error,
            "unknown transition version 2 (this build speaks version 1)"
        );
        let mut other_call = envelope(0, StepStatus::Failed);
        other_call.step = CallId("c2".into());
        let end = Action::End {
            envelope: other_call,
            latch_replay: true,
        };
        let error = refused(&snapshot(), &Ask::Plan, end);
        assert!(error.contains("answers call c2"), "{error}");
        let cancelled_end = Action::End {
            envelope: envelope(0, StepStatus::Cancelled),
            latch_replay: true,
        };
        assert!(check(&snapshot(), &Ask::Plan, cancelled_end).is_err());
        let not_cancelled = Action::Cancelled {
            envelope: envelope(0, StepStatus::Failed),
        };
        assert!(check(&snapshot(), &Ask::Plan, not_cancelled).is_err());
        let error = refused(
            &on_link(0, 1),
            &ended(route_failed()),
            dispatch(1, "env/next"),
        );
        assert!(error.contains("answered with a plan"), "{error}");
        let error = refused(&snapshot(), &Ask::Plan, move_on(MovedOn::Capped));
        assert!(error.contains("did not run"), "{error}");
    }
}
