//! Fallback chains for route failures (ADR-0054): a step moves to the next model of its
//! role only when the route failed, every hop is journalled and charged, a capped link is
//! skipped visibly, and a replayed step keeps the chain it recorded.

mod support;

use p1_workflow::{
    JournalRecord, RunOutcome, RunReport, SchemaCheck, StartRequest, StepEnd, StepLine,
    WorkflowSettings,
};
use serde_json::{Value, json};
use support::{Harness, done_with, kinds, request};

/// A worker on a three-link chain (ADR-0054 item 2): `alpha/head → beta/second →
/// gamma/third`. The resolver maps every profile to its own wire model, so a test reads
/// which link ran from the request.
fn chained() -> WorkflowSettings {
    let mut settings = support::settings();
    let worker = settings.roles.get_mut("worker").unwrap();
    worker.model = "alpha/head".to_string();
    worker.fallback = vec!["beta/second".to_string(), "gamma/third".to_string()];
    settings
}

fn route_failed(model: &str, error: &str) -> StepEnd {
    StepEnd::RouteFailed {
        model: model.to_string(),
        error: error.to_string(),
    }
}

fn tried(model: &str, moved_on: Option<&str>) -> Value {
    json!({ "model": model, "moved_on": moved_on })
}

async fn run(harness: &Harness, start: StartRequest) -> RunReport {
    let id = harness.start_request(start).await;
    harness.wait(&id).await
}

fn step_lines(harness: &Harness) -> Vec<StepLine> {
    harness.recorder.steps.lock().unwrap().clone()
}

const WORK: &str = r#"agent("work", #{ label: "w" })"#;

// (a)
#[tokio::test(flavor = "multi_thread")]
async fn a_route_failure_on_the_head_hands_the_step_to_the_next_link() {
    let harness = Harness::with(chained());
    harness.runner.queue(
        "work",
        route_failed(
            "alpha/head",
            "InsufficientBalance: the account has no balance",
        ),
    );
    // Nothing queued for the second link: the runner's default is a `done`.
    let report = run(&harness, request(WORK)).await;

    assert_eq!(report.value["status"], "done");
    assert_eq!(report.value["value"], "did: work");
    assert_eq!(report.value["attempts"], 2, "two starts, no repair");
    assert_eq!(
        report.value["models"],
        json!([
            tried("alpha/head", Some("route_failed")),
            tried("beta/second", None),
        ])
    );
    assert_eq!(report.value["worker"], "w2|work (beta/second)");
    assert_eq!(report.counts.done, 1);
    assert_eq!(report.counts.failed, 0);
    assert_eq!(report.counts.fell_back, 1);
    assert_eq!(report.outcome, RunOutcome::Completed);

    let requests = harness.runner.requests();
    assert_eq!(requests.len(), 2, "one worker per link");
    assert_eq!(requests[0].model.wire_model, "head");
    assert_eq!(requests[1].model.wire_model, "second");

    let records = harness.journal(&report.id);
    assert_eq!(
        kinds(&records),
        [
            "started", "dispatch", "fallback", "dispatch", "result", "ended"
        ]
    );
    assert!(matches!(
        &records[2],
        JournalRecord::Fallback { from, to, error, .. }
            if from == "alpha/head"
                && to == "beta/second"
                && error == "InsufficientBalance: the account has no balance"
    ));

    let lines = step_lines(&harness);
    assert_eq!(lines.len(), 1);
    assert_eq!(lines[0].model, "alpha/head", "the role's own model");
    assert_eq!(
        lines[0].model_chain(),
        "alpha/head route failed → beta/second"
    );
    assert_eq!(lines[0].status, p1_workflow::StepStatus::Done);
}

// (b)
#[tokio::test(flavor = "multi_thread")]
async fn a_capped_link_is_skipped_and_counted() {
    let mut settings = chained();
    settings.caps.insert("head".to_string(), 0);
    let harness = Harness::with(settings);
    let report = run(&harness, request(WORK)).await;

    assert_eq!(report.value["status"], "done");
    assert_eq!(
        report.value["models"],
        json!([
            tried("alpha/head", Some("capped")),
            tried("beta/second", None),
        ])
    );
    assert_eq!(report.value["attempts"], 1, "a skipped link ran nothing");
    assert_eq!(report.counts.capped, 1);
    assert_eq!(report.counts.fell_back, 1);
    assert_eq!(
        report.outcome,
        RunOutcome::CompletedWithIssues,
        "the skipped link stays visible in the run outcome"
    );
    assert_eq!(harness.runner.requests().len(), 1);

    let records = harness.journal(&report.id);
    assert_eq!(
        kinds(&records),
        [
            "started", "capped", "fallback", "dispatch", "result", "ended"
        ]
    );
    assert!(matches!(
        &records[2],
        JournalRecord::Fallback { error, .. } if error == "quota_exceeded: head used=0 limit=0"
    ));
    assert_eq!(
        step_lines(&harness)[0].model_chain(),
        "alpha/head capped → beta/second"
    );
}

// (c)
#[tokio::test(flavor = "multi_thread")]
async fn a_whole_chain_failing_ends_failed_on_the_route() {
    let harness = Harness::with(chained());
    for (model, error) in [
        ("alpha/head", "the route is unreachable"),
        ("beta/second", "the route refuses the model"),
        ("gamma/third", "InsufficientBalance: empty"),
    ] {
        harness.runner.queue("work", route_failed(model, error));
    }
    let report = run(&harness, request(WORK)).await;

    assert_eq!(report.value["status"], "failed");
    assert_eq!(
        report.value["error"], "route: InsufficientBalance: empty",
        "the LAST link's error names the route"
    );
    assert_eq!(report.value["value"], Value::Null);
    assert_eq!(report.value["attempts"], 3);
    assert_eq!(report.value["worker"], "w3|work (gamma/third)");
    assert_eq!(
        report.value["models"],
        json!([
            tried("alpha/head", Some("route_failed")),
            tried("beta/second", Some("route_failed")),
            tried("gamma/third", None),
        ])
    );
    assert_eq!(report.counts.fell_back, 2);
    assert_eq!(report.counts.failed, 1);
    assert_eq!(report.outcome, RunOutcome::CompletedWithIssues);
    assert_eq!(harness.runner.requests().len(), 3);

    assert_eq!(
        kinds(&harness.journal(&report.id)),
        [
            "started", "dispatch", "fallback", "dispatch", "fallback", "dispatch", "result",
            "ended"
        ]
    );
    assert_eq!(
        step_lines(&harness)[0].model_chain(),
        "alpha/head route failed → beta/second route failed → gamma/third"
    );
}

// (b), whole chain capped
#[tokio::test(flavor = "multi_thread")]
async fn a_whole_chain_capped_ends_failed_with_the_last_cap() {
    let mut settings = chained();
    settings.caps.insert("head".to_string(), 0);
    settings.caps.insert("second".to_string(), 0);
    settings.caps.insert("third".to_string(), 0);
    let harness = Harness::with(settings);
    let report = run(&harness, request(WORK)).await;

    assert_eq!(report.value["status"], "failed");
    assert_eq!(
        report.value["error"],
        "quota_exceeded: third used=0 limit=0"
    );
    assert_eq!(report.value["attempts"], 0, "no link ran");
    assert_eq!(report.counts.capped, 3, "every refused link counts");
    assert_eq!(report.counts.fell_back, 2);
    assert!(harness.runner.requests().is_empty());
    assert_eq!(
        report.value["models"],
        json!([
            tried("alpha/head", Some("capped")),
            tried("beta/second", Some("capped")),
            tried("gamma/third", None),
        ])
    );
}

// (b), a cap cannot be passed by a chain
#[tokio::test(flavor = "multi_thread")]
async fn a_chain_cannot_pass_a_capped_model_past_its_cap() {
    let mut settings = chained();
    // Every step's head is skipped, and the second link may be used ONCE per run.
    settings.caps.insert("head".to_string(), 0);
    settings.caps.insert("second".to_string(), 1);
    let harness = Harness::with(settings);
    let report = run(
        &harness,
        request(r#"[agent("one", #{ label: "a" }), agent("two", #{ label: "b" })]"#),
    )
    .await;

    assert_eq!(report.value[0]["status"], "done");
    assert_eq!(report.value[1]["status"], "done");
    // One step ran on the second link and one on the third: the second call could not
    // take the capped model past its cap.
    let ran: Vec<String> = harness
        .runner
        .requests()
        .into_iter()
        .map(|request| request.model.wire_model)
        .collect();
    assert_eq!(ran, ["second", "third"]);
    assert_eq!(
        report.counts.capped, 3,
        "each step's head, then the second call's skipped link"
    );
    assert_eq!(report.counts.fell_back, 3);
    assert_eq!(
        step_lines(&harness)[1].model_chain(),
        "alpha/head capped → beta/second capped → gamma/third"
    );
    assert_eq!(
        harness
            .journal(&report.id)
            .iter()
            .filter(|record| matches!(record, JournalRecord::Fallback { .. }))
            .count(),
        3
    );
}

// (d)
#[tokio::test(flavor = "multi_thread")]
async fn a_step_that_ran_and_failed_does_not_fall_back() {
    let harness = Harness::with(chained());
    harness
        .runner
        .queue("work", StepEnd::Failed("the answer was wrong".into()));
    harness.runner.queue(
        "blocked",
        StepEnd::Blocked {
            summary: "stuck".into(),
            needs: "a token".into(),
        },
    );
    let report = run(
        &harness,
        request(r#"[agent("work", #{ label: "w" }), agent("blocked")]"#),
    )
    .await;

    assert_eq!(report.value[0]["status"], "failed");
    assert_eq!(report.value[0]["error"], "the answer was wrong");
    assert_eq!(report.value[1]["status"], "blocked");
    assert_eq!(report.counts.fell_back, 0);
    assert_eq!(
        harness.runner.requests().len(),
        2,
        "one worker each: a wrong answer is not a route failure"
    );
    for line in step_lines(&harness) {
        assert_eq!(line.models.len(), 1);
        assert_eq!(line.model_chain(), "alpha/head");
    }
    assert!(
        !harness
            .journal(&report.id)
            .iter()
            .any(|record| matches!(record, JournalRecord::Fallback { .. }))
    );
}

// (e)
#[tokio::test(flavor = "multi_thread")]
async fn a_schema_repair_stays_on_the_worker_that_produced_it() {
    let harness = Harness::with(chained());
    harness.runner.queue(
        "work",
        done_with(
            json!({"n": "x"}),
            SchemaCheck::Failed(vec!["/n: expected integer".into()]),
        ),
    );
    harness
        .runner
        .queue_repair("work", done_with(json!({"n": 1}), SchemaCheck::Passed));
    let report = run(&harness, request(WORK)).await;

    assert_eq!(report.value["status"], "done");
    assert_eq!(report.value["value"], json!({"n": 1}));
    assert_eq!(report.value["attempts"], 2);
    assert_eq!(report.value["models"], json!([tried("alpha/head", None)]));
    assert_eq!(report.counts.fell_back, 0);
    assert_eq!(
        harness.runner.requests().len(),
        1,
        "a repair is never a new request"
    );
    let repairs = harness.runner.repair_messages();
    assert_eq!(repairs.len(), 1);
    assert_eq!(repairs[0].0.id, "w1|work");
}

// (e), a repair turn whose route failed
#[tokio::test(flavor = "multi_thread")]
async fn a_repair_turn_that_loses_its_route_does_not_hop() {
    let harness = Harness::with(chained());
    harness.runner.queue(
        "work",
        done_with(
            json!({"n": "x"}),
            SchemaCheck::Failed(vec!["/n: expected integer".into()]),
        ),
    );
    harness.runner.queue_repair_result(
        "work",
        Ok(route_failed("alpha/head", "InsufficientBalance: empty")),
    );
    let report = run(&harness, request(WORK)).await;

    assert_eq!(report.value["status"], "failed");
    assert_eq!(report.value["error"], "route: InsufficientBalance: empty");
    assert_eq!(report.value["attempts"], 2);
    assert_eq!(report.value["models"], json!([tried("alpha/head", None)]));
    assert_eq!(report.counts.fell_back, 0);
    assert_eq!(harness.runner.requests().len(), 1);
}

// (f)
#[tokio::test(flavor = "multi_thread")]
async fn resume_replays_the_recorded_chain_and_reruns_from_the_head() {
    let harness = Harness::with(chained());
    harness.runner.queue(
        "work one",
        route_failed("alpha/head", "the route is unreachable"),
    );
    let first = harness.run(r#"agent("work one", #{ label: "w" })"#).await;
    assert_eq!(first.counts.fell_back, 1);
    assert_eq!(harness.runner.requests().len(), 2);

    // The same call replays: no worker, and the CHAIN it recorded, not the head alone.
    let replay = StartRequest {
        resume_from: Some(first.id.clone()),
        ..request(r#"agent("work one", #{ label: "w" })"#)
    };
    let replayed = run(&harness, replay).await;
    assert_eq!(harness.runner.requests().len(), 2, "a replay runs nothing");
    assert_eq!(replayed.counts.replayed, 1);
    assert_eq!(replayed.counts.fell_back, 0, "a replay spends nothing");
    assert_eq!(
        replayed.steps[0].models,
        first.steps[0].models.clone(),
        "the recorded chain is kept"
    );
    assert_eq!(
        replayed.steps[0].model_chain(),
        "alpha/head route failed → beta/second"
    );
    assert_eq!(replayed.value["worker"], "w2|work one (beta/second)");

    // A changed call re-runs and starts the chain at its HEAD again.
    harness.runner.queue(
        "work two",
        route_failed("alpha/head", "the route is unreachable"),
    );
    let rerun = StartRequest {
        resume_from: Some(first.id.clone()),
        ..request(r#"agent("work two", #{ label: "w" })"#)
    };
    let second = run(&harness, rerun).await;
    let requests = harness.runner.requests();
    assert_eq!(requests.len(), 4);
    assert_eq!(requests[2].model.reference, "alpha/head");
    assert_eq!(requests[3].model.reference, "beta/second");
    assert_eq!(second.counts.fell_back, 1);
}
