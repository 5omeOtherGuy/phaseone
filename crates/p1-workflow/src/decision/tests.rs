//! Unit tests of the decisions: `plan_step` and `accept_step` per decision kind, the
//! version rule and the JSON entry points.

use serde_json::json;

use super::*;
use crate::api::RunId;

fn model(reference: &str) -> ResolvedModel {
    let (environment, profile) = reference.split_once('/').unwrap();
    ResolvedModel {
        reference: reference.to_string(),
        environment: environment.to_string(),
        profile: profile.to_string(),
        effort: None,
        wire_model: profile.to_string(),
    }
}

/// A run whose `worker` role has the chain `env/head → env/next`.
fn snapshot() -> Snapshot {
    Snapshot {
        version: CONTRACT_VERSION,
        run: RunId("wf1".into()),
        max_steps: 10,
        has_base: true,
        role: Some(RoleView {
            chain: vec![model("env/head"), model("env/next")],
            tools: vec!["read".into()],
        }),
        replay: ReplayView {
            latched_off: true,
            open: Vec::new(),
        },
        step: StepProgress {
            ordinal: 1,
            ..StepProgress::default()
        },
    }
}

fn request() -> PlanRequest {
    PlanRequest {
        version: CONTRACT_VERSION,
        call: CallId("c1".into()),
        label: Some("l".into()),
        role: "worker".into(),
        worktree: None,
        tools: None,
    }
}

fn outcome(attempt: Attempt) -> AttemptOutcome {
    AttemptOutcome {
        version: CONTRACT_VERSION,
        call: CallId("c1".into()),
        label: Some("l".into()),
        attempt,
    }
}

/// The snapshot of a step on `link` that spent `attempts`.
fn on_link(link: u32, attempts: u32) -> Snapshot {
    let mut snapshot = snapshot();
    snapshot.step.link = Some(link);
    snapshot.step.cost.attempts = attempts;
    snapshot
}

fn worker() -> WorkerRef {
    WorkerRef {
        id: "w1".into(),
        description: "env/head".into(),
    }
}

fn done(summary: &str) -> StepEnd {
    StepEnd::Done {
        summary: summary.into(),
        evidence: "commands passed: t".into(),
        result: None,
        schema: SchemaCheck::NotRequested,
    }
}

fn blank_failed(attempts: u32) -> StepEnvelope {
    Blank {
        call: &CallId("c1".into()),
        label: &Some("l".into()),
    }
    .failed(attempts)
}

fn action(result: Result<Transition, String>) -> Action {
    let transition = result.expect("a transition");
    assert_eq!(transition.version, CONTRACT_VERSION);
    transition.action
}

fn ended(action: Action) -> (StepEnvelope, bool) {
    match action {
        Action::End {
            envelope,
            latch_replay,
        } => (envelope, latch_replay),
        other => panic!("not an end: {other:?}"),
    }
}

fn bare(model: &str) -> ModelTry {
    ModelTry {
        model: model.into(),
        moved_on: None,
    }
}

// ------------------------------------------------------------------ plan_step

#[test]
fn a_first_plan_dispatches_the_head_with_the_role_grant_and_latches_replay() {
    assert_eq!(
        action(plan_step(&snapshot(), &request())),
        Action::Dispatch {
            link: 0,
            model: "env/head".into(),
            tools: vec!["read".into()],
            latch_replay: true,
        }
    );
    let own = PlanRequest {
        tools: Some(vec!["grep".into()]),
        ..request()
    };
    match action(plan_step(&snapshot(), &own)) {
        Action::Dispatch { tools, .. } => assert_eq!(tools, ["grep"], "the call's grant wins"),
        other => panic!("{other:?}"),
    }
}

#[test]
fn a_call_past_max_steps_is_refused_without_consulting_replay() {
    let mut snapshot = snapshot();
    snapshot.step.ordinal = 11;
    snapshot.replay = ReplayView {
        latched_off: false,
        open: vec![ReplayEntry {
            index: 0,
            call: CallId("c1".into()),
        }],
    };
    let (envelope, latch) = ended(action(plan_step(&snapshot, &request())));
    assert_eq!(envelope.error.as_deref(), Some("max_steps: 10 reached"));
    assert_eq!(envelope.status, StepStatus::Failed);
    assert_eq!(envelope.attempts, 0);
    assert!(envelope.models.is_empty());
    assert!(!latch, "the prefix is not consulted, so not latched off");
}

#[test]
fn replay_answers_a_held_call_and_misses_otherwise() {
    let mut snapshot = snapshot();
    snapshot.replay = ReplayView {
        latched_off: false,
        open: vec![
            ReplayEntry {
                index: 0,
                call: CallId("other".into()),
            },
            ReplayEntry {
                index: 3,
                call: CallId("c1".into()),
            },
        ],
    };
    assert_eq!(
        action(plan_step(&snapshot, &request())),
        Action::Replay { entry: 3 }
    );
    snapshot.replay.latched_off = true;
    assert!(
        matches!(
            action(plan_step(&snapshot, &request())),
            Action::Dispatch {
                latch_replay: true,
                ..
            }
        ),
        "a latched prefix answers nothing"
    );
}

#[test]
fn an_unknown_role_is_refused_after_replay() {
    let mut snapshot = snapshot();
    snapshot.role = None;
    let request = PlanRequest {
        role: "nobody".into(),
        ..request()
    };
    let (envelope, latch) = ended(action(plan_step(&snapshot, &request)));
    assert_eq!(envelope.error.as_deref(), Some("unknown_role: nobody"));
    assert_eq!(envelope.step, CallId("c1".into()));
    assert_eq!(envelope.label.as_deref(), Some("l"));
    assert!(latch);
}

#[test]
fn a_worktree_without_a_base_is_refused() {
    let mut snapshot = snapshot();
    snapshot.has_base = false;
    let request = PlanRequest {
        worktree: Some("fix-a".into()),
        ..request()
    };
    let (envelope, _) = ended(action(plan_step(&snapshot, &request)));
    assert_eq!(
        envelope.error.as_deref(),
        Some("worktree: fix-a: the run has no base commit (its workspace is not a git repository)")
    );
}

#[test]
fn after_moving_on_the_next_link_is_planned_then_the_chain_is_exhausted() {
    let mut snapshot = on_link(0, 1);
    snapshot.step.moved_on = true;
    assert_eq!(
        action(plan_step(&snapshot, &request())),
        Action::Dispatch {
            link: 1,
            model: "env/next".into(),
            tools: vec!["read".into()],
            latch_replay: false,
        }
    );

    let mut snapshot = on_link(1, 2);
    snapshot.step.moved_on = true;
    snapshot.step.route_failed = true;
    snapshot.step.last_error = "no balance".into();
    snapshot.step.last_worker = Some("w2 (env/next)".into());
    snapshot.step.walked = vec![
        ModelTry {
            model: "env/head".into(),
            moved_on: Some(MovedOn::RouteFailed),
        },
        bare("env/next"),
    ];
    let (envelope, _) = ended(action(plan_step(&snapshot, &request())));
    assert_eq!(envelope.error.as_deref(), Some("route: no balance"));
    assert_eq!(envelope.worker.as_deref(), Some("w2 (env/next)"));
    assert_eq!(envelope.attempts, 2);
    assert_eq!(envelope.models, snapshot.step.walked);

    snapshot.step.route_failed = false;
    let (envelope, _) = ended(action(plan_step(&snapshot, &request())));
    assert_eq!(
        envelope.error.as_deref(),
        Some("no balance"),
        "a chain only capped is not a route failure"
    );
}

#[test]
fn a_role_with_no_model_is_an_exhausted_chain() {
    let mut snapshot = snapshot();
    snapshot.role = Some(RoleView {
        chain: Vec::new(),
        tools: vec!["read".into()],
    });
    let (envelope, latch) = ended(action(plan_step(&snapshot, &request())));
    assert_eq!(envelope.error.as_deref(), Some(""));
    assert!(latch);
}

#[test]
fn planning_a_step_still_on_its_link_is_an_error() {
    let error = plan_step(&on_link(0, 1), &request()).unwrap_err();
    assert!(error.contains("still on link 0"), "{error}");
}

// ------------------------------------------------------------------ accept_step

#[test]
fn a_done_turn_ends_the_step_on_its_link() {
    let (envelope, _) = ended(action(accept_step(
        &on_link(0, 1),
        &outcome(Attempt::Ended {
            worker: worker(),
            end: done("ok"),
        }),
    )));
    assert_eq!(envelope.status, StepStatus::Done);
    assert_eq!(envelope.value, json!("ok"));
    assert_eq!(envelope.worker.as_deref(), Some("w1 (env/head)"));
    assert_eq!(envelope.attempts, 1);
    assert_eq!(envelope.models, [bare("env/head")]);
}

#[test]
fn a_blocked_turn_ends_the_step_with_its_needs() {
    let (envelope, _) = ended(action(accept_step(
        &on_link(0, 1),
        &outcome(Attempt::Ended {
            worker: worker(),
            end: StepEnd::Blocked {
                summary: "s".into(),
                needs: "a key".into(),
            },
        }),
    )));
    assert_eq!(envelope.status, StepStatus::Blocked);
    assert_eq!(envelope.needs.as_deref(), Some("a key"));
}

#[test]
fn a_route_failure_moves_on_naming_the_reported_model() {
    let moved = action(accept_step(
        &on_link(0, 1),
        &outcome(Attempt::Ended {
            worker: worker(),
            end: StepEnd::RouteFailed {
                model: "env/reported".into(),
                error: "down".into(),
            },
        }),
    ));
    assert_eq!(
        moved,
        Action::MoveOn {
            reason: MovedOn::RouteFailed,
            tried: ModelTry {
                model: "env/reported".into(),
                moved_on: Some(MovedOn::RouteFailed),
            },
            error: "down".into(),
            worker: Some("w1 (env/head)".into()),
        }
    );
    // On the last link the entry stays bare; a runner that names no model gets the chain's.
    let moved = action(accept_step(
        &on_link(1, 2),
        &outcome(Attempt::Ended {
            worker: worker(),
            end: StepEnd::RouteFailed {
                model: String::new(),
                error: "down".into(),
            },
        }),
    ));
    match moved {
        Action::MoveOn { tried, .. } => assert_eq!(tried, bare("env/next")),
        other => panic!("{other:?}"),
    }
}

#[test]
fn a_cap_moves_on_and_a_capped_repair_keeps_the_rejected_value() {
    let capped = Attempt::Capped {
        wire_model: "head".into(),
        used: 3,
        limit: 3,
    };
    assert_eq!(
        action(accept_step(&on_link(0, 0), &outcome(capped.clone()))),
        Action::MoveOn {
            reason: MovedOn::Capped,
            tried: ModelTry {
                model: "env/head".into(),
                moved_on: Some(MovedOn::Capped),
            },
            error: "quota_exceeded: head used=3 limit=3".into(),
            worker: None,
        }
    );

    let mut snapshot = on_link(0, 1);
    snapshot.step.repair = Some(RepairTurn {
        worker: worker(),
        rejected: StepEnvelope {
            value: json!("partial"),
            worker: Some("w1 (env/head)".into()),
            ..blank_failed(1)
        },
    });
    let (envelope, _) = ended(action(accept_step(&snapshot, &outcome(capped))));
    assert_eq!(
        envelope.error.as_deref(),
        Some("quota_exceeded: head used=3 limit=3 (repair)")
    );
    assert_eq!(envelope.value, json!("partial"));
    assert_eq!(envelope.models, [bare("env/head")]);
}

#[test]
fn a_failed_contract_and_a_missing_finish_get_the_one_repair() {
    let repair = action(accept_step(
        &on_link(0, 1),
        &outcome(Attempt::Ended {
            worker: worker(),
            end: StepEnd::Done {
                summary: "s".into(),
                evidence: "e".into(),
                result: Some(json!({"n": "x"})),
                schema: SchemaCheck::Failed(vec!["n: not a number".into()]),
            },
        }),
    ));
    match repair {
        Action::Repair { message, rejected } => {
            assert!(message.contains("- n: not a number\n"), "{message}");
            assert_eq!(rejected.value, json!({"n": "x"}));
            assert_eq!(rejected.attempts, 1);
            assert_eq!(rejected.error, None);
        }
        other => panic!("{other:?}"),
    }

    let nudge = action(accept_step(
        &on_link(0, 1),
        &outcome(Attempt::Ended {
            worker: worker(),
            end: StepEnd::EndedWithoutFinish {
                text: "my answer".into(),
            },
        }),
    ));
    match nudge {
        Action::Repair { message, rejected } => {
            assert_eq!(message, FINISH_NUDGE);
            assert_eq!(rejected.value, json!("my answer"));
            assert_eq!(rejected.error.as_deref(), Some("ended without finish"));
        }
        other => panic!("{other:?}"),
    }
}

fn repairing(attempts: u32) -> Snapshot {
    let mut snapshot = on_link(0, attempts);
    snapshot.step.repair = Some(RepairTurn {
        worker: worker(),
        rejected: StepEnvelope {
            value: json!({"n": "x"}),
            schema: SchemaCheck::Failed(vec!["n".into()]),
            evidence: Some("e".into()),
            worker: Some("w1 (env/head)".into()),
            ..blank_failed(1)
        },
    });
    snapshot
}

#[test]
fn the_repair_turn_ends_the_step_either_way() {
    let (envelope, _) = ended(action(accept_step(
        &repairing(2),
        &outcome(Attempt::RepairEnded {
            end: StepEnd::Done {
                summary: "s".into(),
                evidence: "e2".into(),
                result: None,
                schema: SchemaCheck::Failed(vec!["a".into(), "b".into()]),
            },
        }),
    )));
    assert_eq!(envelope.error.as_deref(), Some("invalid_output: a; b"));
    assert_eq!(
        envelope.value,
        json!({"n": "x"}),
        "the rejected value stands"
    );
    assert_eq!(envelope.attempts, 2);

    let (envelope, _) = ended(action(accept_step(
        &repairing(2),
        &outcome(Attempt::RepairEnded { end: done("fixed") }),
    )));
    assert_eq!(envelope.status, StepStatus::Done);
    assert_eq!(envelope.value, json!("fixed"));
    assert_eq!(envelope.worker.as_deref(), Some("w1 (env/head)"));

    let (envelope, _) = ended(action(accept_step(
        &repairing(2),
        &outcome(Attempt::RepairEnded {
            end: StepEnd::EndedWithoutFinish {
                text: "again".into(),
            },
        }),
    )));
    assert_eq!(
        envelope.error.as_deref(),
        Some("ended without finish"),
        "no second nudge"
    );
}

#[test]
fn a_runner_refusal_and_an_unwritable_journal_end_the_step() {
    let (envelope, _) = ended(action(accept_step(
        &on_link(0, 1),
        &outcome(Attempt::RunnerRefused {
            reason: "unknown environment".into(),
        }),
    )));
    assert_eq!(envelope.error.as_deref(), Some("unknown environment"));
    assert_eq!(envelope.worker, None);

    let (envelope, _) = ended(action(accept_step(
        &repairing(2),
        &outcome(Attempt::RunnerRefused {
            reason: "gone".into(),
        }),
    )));
    assert_eq!(envelope.error.as_deref(), Some("gone"));
    assert_eq!(envelope.attempts, 2);
    assert_eq!(envelope.value, json!({"n": "x"}));

    let (envelope, _) = ended(action(accept_step(
        &on_link(0, 0),
        &outcome(Attempt::JournalFailed {
            error: "journal: disk full".into(),
        }),
    )));
    assert_eq!(envelope.error.as_deref(), Some("journal: disk full"));
    assert_eq!(envelope.attempts, 0);
}

#[test]
fn a_cancellation_cancels_the_step() {
    for (snapshot, worker) in [
        (on_link(0, 1), None),
        (repairing(2), Some("w1 (env/head)".to_string())),
    ] {
        match action(accept_step(&snapshot, &outcome(Attempt::Cancelled))) {
            Action::Cancelled { envelope } => {
                assert_eq!(envelope.status, StepStatus::Cancelled);
                assert_eq!(envelope.worker, worker);
                assert_eq!(envelope.attempts, snapshot.step.cost.attempts);
                assert_eq!(envelope.models, [bare("env/head")]);
            }
            other => panic!("{other:?}"),
        }
    }
    // A turn that itself reports a cancellation is the same end.
    match action(accept_step(
        &on_link(0, 1),
        &outcome(Attempt::Ended {
            worker: worker(),
            end: StepEnd::Cancelled,
        }),
    )) {
        Action::Cancelled { envelope } => {
            assert_eq!(envelope.worker.as_deref(), Some("w1 (env/head)"))
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn a_worktree_that_cannot_be_prepared_ends_the_step_before_any_link() {
    let (envelope, latch) = ended(action(accept_step(
        &snapshot(),
        &outcome(Attempt::WorktreeRefused {
            error: "worktree_busy: fix-a".into(),
        }),
    )));
    assert_eq!(envelope.error.as_deref(), Some("worktree_busy: fix-a"));
    assert!(envelope.models.is_empty());
    assert!(!latch);
    match action(accept_step(
        &snapshot(),
        &outcome(Attempt::WorktreeCancelled),
    )) {
        Action::Cancelled { envelope } => {
            assert_eq!(envelope.status, StepStatus::Cancelled);
            assert_eq!(envelope.error, None);
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn an_outcome_that_does_not_fit_the_step_is_an_error() {
    assert!(accept_step(&snapshot(), &outcome(Attempt::Cancelled)).is_err());
    let worktree = outcome(Attempt::WorktreeRefused { error: "x".into() });
    assert!(accept_step(&on_link(0, 1), &worktree).is_err());
    let repair = outcome(Attempt::RepairEnded { end: done("x") });
    assert!(accept_step(&on_link(0, 1), &repair).is_err());
    let first = outcome(Attempt::Ended {
        worker: worker(),
        end: done("x"),
    });
    assert!(accept_step(&repairing(2), &first).is_err());
}

// ------------------------------------------------------------------ versions and JSON

#[test]
fn an_unknown_version_is_refused() {
    let newer = Snapshot {
        version: 2,
        ..snapshot()
    };
    assert_eq!(
        plan_step(&newer, &request()).unwrap_err(),
        "unknown snapshot version 2 (this build speaks version 1)"
    );
    let request = PlanRequest {
        version: 0,
        ..request()
    };
    let error = plan_step(&snapshot(), &request).unwrap_err();
    assert!(error.contains("request version 0"), "{error}");
    let outcome = AttemptOutcome {
        version: 9,
        ..outcome(Attempt::Cancelled)
    };
    let error = accept_step(&on_link(0, 1), &outcome).unwrap_err();
    assert!(error.contains("outcome version 9"), "{error}");
}

#[test]
fn the_json_entry_points_are_the_same_calls() {
    let snapshot_text = serde_json::to_string(&snapshot()).unwrap();
    let request_text = serde_json::to_string(&request()).unwrap();
    let answer = plan_step_json(&snapshot_text, &request_text).unwrap();
    let transition: Transition = serde_json::from_str(&answer).unwrap();
    assert_eq!(transition, plan_step(&snapshot(), &request()).unwrap());
    assert!(answer.contains(r#""kind":"dispatch""#), "{answer}");

    let outcome_text = serde_json::to_string(&outcome(Attempt::Ended {
        worker: worker(),
        end: done("ok"),
    }))
    .unwrap();
    let snapshot_text = serde_json::to_string(&on_link(0, 1)).unwrap();
    let answer = accept_step_json(&snapshot_text, &outcome_text).unwrap();
    let transition: Transition = serde_json::from_str(&answer).unwrap();
    assert!(matches!(transition.action, Action::End { .. }));

    let mut unknown = serde_json::to_value(snapshot()).unwrap();
    unknown["version"] = json!(7);
    let error = plan_step_json(&unknown.to_string(), &request_text).unwrap_err();
    assert!(error.contains("snapshot version 7"), "{error}");
    let error = plan_step_json("{", &request_text).unwrap_err();
    assert!(error.starts_with("snapshot: "), "{error}");
    let error = accept_step_json(&snapshot_text, "[]").unwrap_err();
    assert!(error.starts_with("outcome: "), "{error}");
}

#[test]
fn the_contract_round_trips_as_json() {
    let outcome = outcome(Attempt::Ended {
        worker: worker(),
        end: StepEnd::Failed("boom".into()),
    });
    let text = serde_json::to_string(&outcome).unwrap();
    assert!(text.contains(r#""kind":"ended""#), "{text}");
    assert!(text.contains(r#""failed":"boom""#), "{text}");
    assert_eq!(
        serde_json::from_str::<AttemptOutcome>(&text).unwrap(),
        outcome
    );
    let snapshot = repairing(2);
    let text = serde_json::to_string(&snapshot).unwrap();
    assert_eq!(serde_json::from_str::<Snapshot>(&text).unwrap(), snapshot);
}

#[test]
fn the_native_decisions_are_the_pure_functions() {
    let native: &dyn Decisions = &NativeDecisions;
    assert_eq!(
        native.plan_step(&snapshot(), &request()),
        plan_step(&snapshot(), &request())
    );
    let outcome = outcome(Attempt::Cancelled);
    assert_eq!(
        native.accept_step(&on_link(0, 1), &outcome),
        accept_step(&on_link(0, 1), &outcome)
    );
}
