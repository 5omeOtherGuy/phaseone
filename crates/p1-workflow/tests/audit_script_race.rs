//! Why `scripts/audits/modularity.rhai` builds its thunks with `Fn("name").curry(..)`:
//! `parallel` thunks and `pipeline` stages run on other OS threads (rhai is built with
//! `sync`), and a closure held in a variable is borrowed mutably for the whole of a
//! `x.call(..)`, so a sibling thunk borrowing the same closure is a data race the engine
//! reports as `Data race detected`. main hit exactly that in the audit script's Verify
//! stage (`Data race detected in closure call (line 70)`), green locally only because the
//! scripted runner answers in microseconds while a real model call holds the borrow for
//! seconds: rhai's lock retries for ~50ms before it gives up.
//!
//! The tests here park a step (the plain-Rhai stand-in for a slow model call) to make the
//! race deterministic, check that the racy idiom really is reported, and that the curried
//! form — the audit script's own shape — runs clean.

mod support;

use std::sync::Arc;

use p1_workflow::{RunId, RunOutcome, RunReport, SchemaCheck, WorkflowSettings};
use serde_json::{Value, json};
use support::{Harness, Hold, done_with};

fn audit_script() -> String {
    std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../scripts/audits/modularity.rhai"
    ))
    .expect("the audit script exists")
}

fn finding(id: &str, file: &str, line: u64, severity: &str) -> Value {
    json!({
        "id": id, "claim": format!("claim {id}"), "rule": "seams.md §1",
        "evidence": [{"file": file, "line": line, "quote": "let x = 1;"}],
        "repro": "rg -n x crates", "severity": severity, "fix": "remove it"
    })
}

/// A refuter prompt exactly as the audit script's `fn refute_step` builds it (`json()` is
/// canonical: sorted keys, no spaces).
fn refute_prompt(args: &Value, finding: &Value, lens: &str) -> String {
    let shown = json!({
        "id": finding["id"], "claim": finding["claim"], "rule": finding["rule"],
        "evidence": finding["evidence"], "repro": finding["repro"],
        "severity": finding["severity"], "fix": finding["fix"]
    });
    format!(
        "{}\n{}\n\n## Finding\n```json\n{}\n```\n\n## Facts\n{}",
        args["refute_preamble"].as_str().unwrap(),
        args["refuter"][lens].as_str().unwrap(),
        shown,
        args["facts"].as_str().unwrap()
    )
}

fn verdict(verdict: &str) -> p1_workflow::StepEnd {
    done_with(
        json!({"verdict": verdict, "reason": "checked", "repro_output": "…"}),
        SchemaCheck::Passed,
    )
}

/// Waits for the first of two parked steps to be reached, then for the sibling thunk to
/// have settled its borrow — it reached the other parked step, or it failed (the engine
/// reports that to the observer) — lets go of both, and returns the finished report. A
/// racy script never reaches the other step, so this must not wait for both.
async fn release_and_wait(
    harness: &Harness,
    id: &RunId,
    first: &Arc<Hold>,
    second: &Arc<Hold>,
) -> RunReport {
    let other = tokio::select! {
        _ = first.reached.notified() => second,
        _ = second.reached.notified() => first,
    };
    tokio::select! {
        _ = other.reached.notified() => {}
        _ = harness.recorder.thunk_failed.notified() => {}
    }
    first.release.notify_one();
    second.release.notify_one();
    harness.wait(id).await
}

/// The bug: two thunks method-call one captured closure. The run must end `Failed` with a
/// data-race error — the script is wrong, and the engine says so instead of corrupting the
/// closure.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn method_calling_a_captured_closure_in_parallel_races() {
    let script = r#"
let refute = |lens| agent("vote " + lens);
let votes = parallel([|| refute.call("rule"), || refute.call("code")]);
votes.len()
"#;
    let harness = Harness::with(WorkflowSettings::shipped());
    let rule = harness.runner.hold("vote rule");
    let code = harness.runner.hold("vote code");
    let id = harness.start(script).await;
    let report = release_and_wait(&harness, &id, &rule, &code).await;
    assert_eq!(
        report.outcome,
        RunOutcome::Failed,
        "a thunk calling a closure its sibling holds must fail: {report:?}"
    );
    let error = report.error.expect("a failed run carries its error");
    assert!(
        error.contains("Data race"),
        "the error must name the race, not something else: {error}"
    );
}

/// The fix: the same two votes as a script-`fn` thunk with its inputs curried in as values.
/// Twenty runs in a row must be `Completed`, which the racy form above is not.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn curried_thunks_run_twenty_times_in_a_row() {
    let script = r#"
let a = args;
fn vote(preamble, lens, facts) {
    agent(preamble + " " + lens + " " + facts);
}
let votes = parallel([
    Fn("vote").curry(a.preamble, "rule", a.facts),
    Fn("vote").curry(a.preamble, "code", a.facts),
]);
votes.len()
"#;
    for run in 0..20 {
        let harness = Harness::with(WorkflowSettings::shipped());
        let mut request = support::request(script);
        request.args = json!({ "preamble": "vote", "facts": "# Facts" });
        let id = harness.start_request(request).await;
        let report = harness.wait(&id).await;
        assert_eq!(
            report.outcome,
            RunOutcome::Completed,
            "run {run}: {report:?}"
        );
        assert_eq!(report.counts.steps, 2, "two votes, run {run}");
    }
}

/// The audit script itself, under the condition that broke CI: a refuter parks in `agent`
/// (a model call takes seconds) while its sibling vote runs. With the closure form this is
/// `Data race detected in closure call (line 70)`; with `Fn("refute_step").curry(..)` both
/// votes run and the finding is confirmed.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_parked_refuter_does_not_race_the_audit_script() {
    let args = json!({
        "sha": "abc123",
        "facts": "# Facts\n- p1-core → p1-contracts",
        "units": [
            {"label": "core", "lens": "core-purity", "files": ["crates/p1-core/src/lib.rs"],
             "brief": "find in core"}
        ],
        "findings_schema": {"type": "object"},
        "verdict_schema": {"type": "object"},
        "refuter": {"rule": "RULE lens", "code": "CODE lens", "code-second": "CODE second"},
        "refute_preamble": "# Refute"
    });
    let high = finding("f1", "crates/p1-core/src/lib.rs", 10, "high");
    let rule = refute_prompt(&args, &high, "rule");
    let code = refute_prompt(&args, &high, "code");

    let harness = Harness::with(WorkflowSettings::shipped());
    let held_rule = harness.runner.hold(&rule);
    let held_code = harness.runner.hold(&code);
    harness.runner.queue(
        "find in core",
        done_with(
            json!({"unit": "core", "read": [], "findings": [high]}),
            SchemaCheck::Passed,
        ),
    );
    harness.runner.queue(&rule, verdict("upheld"));
    harness.runner.queue(&code, verdict("upheld"));

    let mut request = support::request(&audit_script());
    request.args = args;
    let id = harness.start_request(request).await;
    let report = release_and_wait(&harness, &id, &held_rule, &held_code).await;

    assert_eq!(report.outcome, RunOutcome::Completed, "{report:?}");
    assert_eq!(report.counts.steps, 1 + 2, "one find, two refuter votes");
    assert_eq!(report.value["confirmed"][0]["finding"]["id"], json!("f1"));
}
