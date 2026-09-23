//! Journal replay on `resume_from` (ADR-0053 item 6): content matching, the prefix rule,
//! caps rebuilt from every old dispatch, and non-`done` steps running again.

mod support;

use p1_workflow::{JournalRecord, RunId, RunOutcome, RunReport, StartRequest, StepEnd};
use serde_json::{Value, json};
use support::{Harness, kinds, request};

/// The spike's five-step chain: only step 3 depends on `args.seed`.
const CHAIN: &str = r#"
    phase("replay");
    let seed = if has(args, "seed") { args.seed } else { "A" };
    let r1 = agent("step 1 (constant)", #{ label: "1" });
    let r2 = agent("step 2 (constant)", #{ label: "2" });
    let r3 = agent("step 3 for " + seed, #{ label: "3" });
    let r4 = agent("step 4 (constant)", #{ label: "4" });
    let r5 = agent("step 5 (constant)", #{ label: "5" });
    [r1.value, r2.value, r3.value, r4.value, r5.value]
"#;

fn chain(args: Value, resume_from: Option<&str>) -> StartRequest {
    StartRequest {
        args,
        resume_from: resume_from.map(|id| RunId(id.to_string())),
        ..request(CHAIN)
    }
}

async fn run(harness: &Harness, start: StartRequest) -> RunReport {
    let id = harness.start_request(start).await;
    harness.wait(&id).await
}

// (7)
#[tokio::test(flavor = "multi_thread")]
async fn resuming_an_unchanged_run_replays_every_step() {
    let harness = Harness::new();
    let first = run(&harness, chain(json!({}), None)).await;
    assert_eq!(first.outcome, RunOutcome::Completed);
    assert_eq!(harness.runner.requests().len(), 5);

    let second = run(&harness, chain(json!({}), Some("wf1"))).await;
    assert_eq!(
        harness.runner.requests().len(),
        5,
        "no runner call on replay"
    );
    assert_eq!(second.value, first.value);
    assert_eq!(second.outcome, RunOutcome::Completed);
    assert_eq!(second.counts.replayed, 5);
    assert_eq!(second.counts.steps, 5);
    assert!(second.steps.iter().all(|line| line.replayed));
    let records = harness.journal(&second.id);
    let kinds = kinds(&records);
    assert_eq!(kinds.iter().filter(|kind| **kind == "replayed").count(), 5);
    assert_eq!(kinds.iter().filter(|kind| **kind == "result").count(), 5);
    assert_eq!(kinds.iter().filter(|kind| **kind == "dispatch").count(), 0);
    assert!(matches!(
        &records[0],
        JournalRecord::Started { resumed_from: Some(from), .. } if from.0 == "wf1"
    ));
    assert!(records.iter().any(|record| matches!(
        record,
        JournalRecord::Replayed { from, .. } if from.0 == "wf1"
    )));
    let observed = harness.recorder.steps.lock().unwrap().clone();
    assert_eq!(observed.iter().filter(|line| line.replayed).count(), 5);
}

// (8)
#[tokio::test(flavor = "multi_thread")]
async fn an_edited_middle_call_reruns_it_and_everything_after() {
    let harness = Harness::new();
    run(&harness, chain(json!({"seed": "A"}), None)).await;
    let before = harness.runner.requests().len();

    let second = run(&harness, chain(json!({"seed": "B"}), Some("wf1"))).await;
    let rerun: Vec<String> = harness.runner.prompts()[before..].to_vec();
    assert_eq!(
        rerun,
        ["step 3 for B", "step 4 (constant)", "step 5 (constant)"],
        "steps 4 and 5 are unchanged but come after the change"
    );
    assert_eq!(second.counts.replayed, 2);
    assert_eq!(second.value[2], "did: step 3 for B");
}

// (9)
#[tokio::test(flavor = "multi_thread")]
async fn caps_are_rebuilt_from_every_old_dispatch_on_resume() {
    let harness = Harness::new();
    let first = harness
        .run(r#"[agent("j1", #{ role: "judge" }), agent("j2", #{ role: "second_judge" }), agent("j3", #{ role: "judge" })]"#)
        .await;
    assert_eq!(first.outcome, RunOutcome::Completed);

    let resume = StartRequest {
        resume_from: Some(first.id.clone()),
        ..request(r#"agent("a new question", #{ role: "judge" })"#)
    };
    let second = run(&harness, resume).await;
    assert_eq!(second.value["status"], "failed");
    assert_eq!(
        second.value["error"],
        "quota_exceeded: claude-fable-5 used=3 limit=3"
    );
    assert_eq!(harness.runner.requests().len(), 3);
}

// (10)
#[tokio::test(flavor = "multi_thread")]
async fn a_failed_step_is_not_replayed() {
    let harness = Harness::new();
    harness
        .runner
        .queue("flaky", StepEnd::Failed("the provider went away".into()));
    let script = r#"[agent("steady").status, agent("flaky").status]"#;
    let first = harness.run(script).await;
    assert_eq!(first.value, json!(["done", "failed"]));
    assert_eq!(first.outcome, RunOutcome::CompletedWithIssues);

    let resume = StartRequest {
        resume_from: Some(first.id.clone()),
        ..request(script)
    };
    let second = run(&harness, resume).await;
    assert_eq!(second.value, json!(["done", "done"]));
    assert_eq!(second.counts.replayed, 1);
    assert_eq!(harness.runner.prompts(), ["steady", "flaky", "flaky"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn replay_matches_parallel_calls_by_content_and_equal_calls_in_order() {
    let harness = Harness::new();
    harness.runner.queue("same", support::done("first answer"));
    harness.runner.queue("same", support::done("second answer"));
    let script = r#"
        let pair = parallel([|| agent("left"), || agent("right")]);
        [agent("same").value, agent("same").value, pair[0].value, pair[1].value]
    "#;
    let first = harness.run(script).await;
    let resume = StartRequest {
        resume_from: Some(first.id.clone()),
        ..request(script)
    };
    let second = run(&harness, resume).await;
    assert_eq!(second.value, first.value);
    assert_eq!(second.value[0], "first answer");
    assert_eq!(second.value[1], "second answer");
    assert_eq!(second.counts.replayed, 4);
}
