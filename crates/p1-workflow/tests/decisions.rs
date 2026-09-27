//! The substrate's side of the decision seam (S6.3): every transition a decision answers is
//! checked before it is applied, and a refused one — or a decision that fails — fails its
//! step with the reason. Nothing is dispatched or charged for it, nothing panics, and the
//! script carries on with its next call.

mod support;

use std::sync::Arc;

use p1_workflow::decision::{Action, Attempt, AttemptOutcome, PlanRequest, Snapshot, Transition};
use p1_workflow::{
    Decisions, InProcessWorkflows, JournalRecord, NativeDecisions, RunOutcome, RunReport, StepEnd,
    StepStatus,
};
use serde_json::json;
use support::{Recorder, Scratch, ScriptedRunner, TableResolver, journal, kinds, request};

type Tamper = dyn Fn(&Snapshot, Option<&AttemptOutcome>, Transition) -> Result<Transition, String>
    + Send
    + Sync;

/// The native decisions with their answers passed through `tamper`.
struct Tampered(Box<Tamper>);

impl Decisions for Tampered {
    fn plan_step(&self, snapshot: &Snapshot, request: &PlanRequest) -> Result<Transition, String> {
        (self.0)(
            snapshot,
            None,
            NativeDecisions.plan_step(snapshot, request)?,
        )
    }

    fn accept_step(
        &self,
        snapshot: &Snapshot,
        outcome: &AttemptOutcome,
    ) -> Result<Transition, String> {
        (self.0)(
            snapshot,
            Some(outcome),
            NativeDecisions.accept_step(snapshot, outcome)?,
        )
    }
}

struct Run {
    report: RunReport,
    records: Vec<JournalRecord>,
    runner: Arc<ScriptedRunner>,
}

/// Runs `script` against decisions tampered with by `tamper`.
async fn run_with(
    script: &str,
    runner: Arc<ScriptedRunner>,
    tamper: impl Fn(&Snapshot, Option<&AttemptOutcome>, Transition) -> Result<Transition, String>
    + Send
    + Sync
    + 'static,
) -> Run {
    let root = Scratch::new();
    let service = InProcessWorkflows::with_decisions(
        runner.clone(),
        Arc::new(TableResolver),
        Arc::new(Recorder::default()),
        support::settings(),
        root.path().to_path_buf(),
        Arc::new(Tampered(Box::new(tamper))),
    );
    let id = p1_workflow::WorkflowService::start(&*service, request(script))
        .await
        .expect("start");
    let status =
        p1_workflow::WorkflowService::wait(&*service, &id, p1_contracts::CancellationToken::new())
            .await
            .expect("wait");
    let p1_workflow::RunStatus::Ended(report) = status else {
        panic!("still running: {status:?}");
    };
    Run {
        records: journal(&root.path().join(&id.0)),
        report,
        runner,
    }
}

fn step_error(report: &RunReport, index: usize) -> String {
    report.steps[index].error.clone().unwrap_or_default()
}

const TWO_CALLS: &str = r#"
let a = agent("first");
let b = agent("second");
[a.status, a.error, b.status, b.value]
"#;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_native_decisions_through_the_seam_run_as_before() {
    let run = run_with(TWO_CALLS, ScriptedRunner::new(), |_, _, transition| {
        Ok(transition)
    })
    .await;
    assert_eq!(run.report.outcome, RunOutcome::Completed);
    assert_eq!(
        run.report.value,
        json!(["done", null, "done", "did: second"])
    );
    assert_eq!(
        kinds(&run.records),
        [
            "started", "dispatch", "result", "dispatch", "result", "ended"
        ]
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_model_outside_the_chain_fails_the_step_before_dispatch() {
    let run = run_with(
        TWO_CALLS,
        ScriptedRunner::new(),
        |snapshot, _, mut transition| {
            if let Action::Dispatch { model, .. } = &mut transition.action
                && snapshot.step.ordinal == 1
            {
                *model = "claude/somewhere-else".into();
            }
            Ok(transition)
        },
    )
    .await;
    assert_eq!(run.report.outcome, RunOutcome::CompletedWithIssues);
    assert_eq!(
        run.report.value[1],
        json!(
            "workflow decision: transition refused: it names claude/somewhere-else (link 0), which is not in the role's chain"
        )
    );
    assert_eq!(
        run.report.value[3],
        json!("did: second"),
        "the script carries on"
    );
    assert_eq!(
        run.runner.prompts(),
        ["second"],
        "the refused step ran nothing"
    );
    assert_eq!(
        kinds(&run.records),
        ["started", "result", "dispatch", "result", "ended"],
        "no Dispatch line for a refused transition"
    );
    assert_eq!(run.report.counts.failed, 1);
    assert_eq!(run.report.steps[0].attempts, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_decision_that_fails_fails_its_step() {
    let run = run_with(
        TWO_CALLS,
        ScriptedRunner::new(),
        |snapshot, _, transition| {
            if snapshot.step.ordinal == 1 {
                return Err("the decision component trapped".into());
            }
            Ok(transition)
        },
    )
    .await;
    assert_eq!(
        step_error(&run.report, 0),
        "workflow decision: the decision failed: the decision component trapped"
    );
    assert_eq!(run.report.steps[1].status, StepStatus::Done);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_second_repair_is_refused_and_nothing_more_runs() {
    let runner = ScriptedRunner::new();
    let never = || StepEnd::EndedWithoutFinish {
        text: "no finish".into(),
    };
    runner.queue("first", never());
    runner.queue_repair("first", never());
    runner.queue_repair("first", never());
    let run = run_with(TWO_CALLS, runner, |snapshot, outcome, transition| {
        if let (
            Some(repair),
            Some(AttemptOutcome {
                attempt: Attempt::RepairEnded { .. },
                ..
            }),
        ) = (&snapshot.step.repair, outcome)
        {
            return Ok(Transition::new(Action::Repair {
                message: "once more".into(),
                rejected: p1_workflow::StepEnvelope {
                    attempts: snapshot.step.cost.attempts,
                    ..repair.rejected.clone()
                },
            }));
        }
        Ok(transition)
    })
    .await;
    assert_eq!(
        step_error(&run.report, 0),
        "workflow decision: transition refused: it asks for a second repair turn"
    );
    assert_eq!(run.report.steps[0].attempts, 2);
    assert_eq!(
        run.runner.repair_messages().len(),
        1,
        "one repair turn only"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_changed_counter_is_refused() {
    let run = run_with(
        TWO_CALLS,
        ScriptedRunner::new(),
        |snapshot, _, mut transition| {
            if let Action::End { envelope, .. } = &mut transition.action
                && snapshot.step.ordinal == 1
            {
                envelope.attempts = 0;
            }
            Ok(transition)
        },
    )
    .await;
    assert_eq!(
        step_error(&run.report, 0),
        "workflow decision: transition refused: it changes a counter: attempts 0 where the step spent 1"
    );
    assert_eq!(
        run.report.steps[0].attempts, 1,
        "the substrate's count stands"
    );
    assert_eq!(run.report.counts.steps, 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_replay_the_journal_does_not_hold_is_refused() {
    let run = run_with(
        TWO_CALLS,
        ScriptedRunner::new(),
        |snapshot, outcome, transition| {
            if outcome.is_none() && snapshot.step.ordinal == 1 {
                return Ok(Transition::new(Action::Replay { entry: 0 }));
            }
            Ok(transition)
        },
    )
    .await;
    assert!(
        step_error(&run.report, 0).starts_with(
            "workflow decision: transition refused: it replays entry 0, which the journal does not hold"
        ),
        "{:?}",
        run.report.steps[0]
    );
    assert_eq!(run.report.counts.replayed, 0);
    assert!(
        !kinds(&run.records).contains(&"replayed"),
        "{:?}",
        run.records
    );
}
