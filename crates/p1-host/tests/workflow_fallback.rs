//! ADR-0054 end to end, through the host: a role's route fails at the provider, the
//! host's step runner reports it as `RouteFailed`, and the engine walks the role's chain
//! to the next model. Driven by `p1 workflow run` over scripted providers, so the
//! mapping from a provider failure to a hop is proved where it lives: in the host.
#![cfg(feature = "workflows")]

mod common;
mod workflow_common;

use std::time::Duration;

use common::run_args;
use p1_contracts::{Item, Outcome, ProviderError, ProviderErrorKind, StreamEvent, ToolStatus};
use p1_testkit::{Step, json_call, text_response, tool_call_response};
use serde_json::json;
use workflow_common::{Fakes, Scratch, done, read_json, step_lines};

/// The shipped role layout, with the worker on `fake/main` and ONE fallback,
/// `fake/other`: a user table names the role, so this is the whole chain.
const ROLES_WITH_FALLBACK: &str = r#"
[workflows.roles.worker]
model = "fake/main"
tools = ["read", "grep"]
fallback = ["fake/other"]
[workflows.roles.reviewer]
model = "fake/main"
tools = ["read"]
[workflows.roles.verifier]
model = "fake/main"
tools = ["read"]
[workflows.roles.judge]
model = "fake/main"
tools = ["read"]
"#;

const ONE_STEP: &str = r#"agent("the task", #{ label: "one" })"#;

/// The route's failure as the provider reports it (ADR-0046's exhausted account, the
/// case the ADR names first): `stream` fails before any turn exists.
fn exhausted() -> Step {
    Step::SetupError(ProviderError::new(
        ProviderErrorKind::InsufficientBalance,
        "the account has no balance",
    ))
}

fn broken_answer() -> Step {
    Step::Events(vec![
        StreamEvent::TextDelta {
            block: 0,
            text: "partial answer that must not be replayed".into(),
        },
        StreamEvent::Finished(Outcome::Failed(ProviderError::new(
            ProviderErrorKind::Transport,
            "stream timed out",
        ))),
    ])
}

#[tokio::test]
async fn a_mid_answer_failure_continues_the_same_worker_before_fallback() {
    let mut main = vec![broken_answer()];
    main.extend(done("recovered answer"));
    let (fakes, scratch, code, stderr) = run(ROLES_WITH_FALLBACK, ONE_STEP, main).await;
    assert_eq!(code, 0, "{stderr}");
    let result = result_of(&scratch);
    assert_eq!(result["value"]["value"], "recovered answer");
    assert_eq!(result["value"]["attempts"], 1);
    assert_eq!(result["counts"]["fell_back"], 0);
    assert_eq!(fakes.builds(), 1);
    assert!(fakes.other.requests().is_empty());
    let requests = fakes.main.requests();
    assert_eq!(requests.len(), 2);
    let history = format!("{:?}", requests[1].history);
    assert!(history.contains(p1_host::run::PROVIDER_RETRY_MESSAGE));
    assert!(!history.contains("partial answer that must not be replayed"));
}

#[tokio::test]
async fn exhausted_stream_retries_fall_back_to_the_next_model() {
    let (fakes, scratch, code, stderr, waits) =
        run_with_budget(ROLES_WITH_FALLBACK, ONE_STEP, vec![broken_answer(); 4], 3).await;
    assert_eq!(code, 0, "{stderr}");
    assert_eq!(fakes.main.requests().len(), 4);
    assert_eq!(result_of(&scratch)["counts"]["fell_back"], 1);
    assert_eq!(waits, [5, 30, 120].map(Duration::from_secs));
}

#[tokio::test]
async fn zero_stream_retries_falls_back_without_waiting() {
    let (fakes, scratch, code, stderr, waits) =
        run_with_budget(ROLES_WITH_FALLBACK, ONE_STEP, vec![broken_answer()], 0).await;
    assert_eq!(code, 0, "{stderr}");
    assert_eq!(fakes.main.requests().len(), 1);
    assert_eq!(result_of(&scratch)["counts"]["fell_back"], 1);
    assert!(waits.is_empty());
}

#[tokio::test]
async fn a_panicked_retry_is_not_another_route_failure() {
    // The scripted provider panics on the next request after exhausting its script.
    let (fakes, scratch, code, stderr, waits) =
        run_with_budget(ROLES_WITH_FALLBACK, ONE_STEP, vec![broken_answer()], 3).await;
    assert_eq!(code, 2, "{stderr}");
    let result = result_of(&scratch);
    assert!(
        result["value"]["error"]
            .as_str()
            .unwrap()
            .contains("abnormally")
    );
    assert_eq!(result["counts"]["fell_back"], 0);
    assert!(fakes.other.requests().is_empty());
    assert_eq!(waits, [Duration::from_secs(5)]);
}

#[tokio::test]
async fn committed_tool_work_resets_the_retry_budget_and_is_not_replayed() {
    let mut main = vec![
        broken_answer(),
        tool_call_response(vec![json_call(
            "read1",
            "read",
            r#"{"file_path":"note.txt"}"#,
        )]),
        broken_answer(),
    ];
    main.extend(done("recovered after reading"));
    let (fakes, scratch, code, stderr, waits) =
        run_with_budget(ROLES_WITH_FALLBACK, ONE_STEP, main, 1).await;
    assert_eq!(code, 0, "{stderr}");
    assert_eq!(
        result_of(&scratch)["value"]["value"],
        "recovered after reading"
    );
    assert_eq!(result_of(&scratch)["counts"]["fell_back"], 0);
    assert_eq!(waits, [5, 5].map(Duration::from_secs));
    let requests = fakes.main.requests();
    assert_eq!(requests.len(), 4);
    let results: Vec<_> = requests[3]
        .history
        .iter()
        .filter_map(|item| match item {
            Item::ToolResult(result) => Some(result),
            _ => None,
        })
        .collect();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].status, ToolStatus::Ok);
    assert!(results[0].content.contains("committed tool output"));
}

#[tokio::test]
async fn a_final_answer_without_finish_is_nudged_to_success() {
    let mut main = vec![text_response("the complete answer")];
    main.extend(done("the complete answer"));
    let (fakes, scratch, code, stderr) = run(ROLES_WITH_FALLBACK, ONE_STEP, main).await;
    assert_eq!(code, 0, "{stderr}");
    let result = result_of(&scratch);
    assert_eq!(result["value"]["value"], "the complete answer");
    assert_eq!(result["value"]["attempts"], 2);
    assert_eq!(result["counts"]["fell_back"], 0);
    assert_eq!(fakes.builds(), 1);
}

#[tokio::test]
async fn cancellation_during_stream_retry_wait_sends_no_more_requests() {
    let scratch = Scratch::with_settings(ROLES_WITH_FALLBACK);
    let fakes = Fakes::new(Vec::new(), vec![broken_answer()], done("must not run"));
    let script = scratch.script("cancel.rhai", ONE_STEP);
    let out = scratch.root.path().join("runs");
    let mut harness = scratch.harness();
    harness.deps.catalog_hook = Some(fakes.hook());
    let waiting = std::sync::Arc::new(tokio::sync::Notify::new());
    let entered = waiting.clone();
    harness.deps.wait = std::sync::Arc::new(move |_| {
        entered.notify_one();
        Box::pin(std::future::pending())
    });
    let interrupt = harness.interrupt.clone();
    let cancel = async move {
        waiting.notified().await;
        interrupt.fire();
    };
    let run = async {
        run_args(
            &mut harness,
            &[
                "workflow",
                "run",
                script.to_str().unwrap(),
                "--out",
                out.to_str().unwrap(),
                "--workspace",
                scratch.workspace.path().to_str().unwrap(),
            ],
        )
        .await
    };
    let (code, ()) =
        tokio::time::timeout(Duration::from_secs(60), async { tokio::join!(run, cancel) })
            .await
            .expect("cancelled workflow hung");
    assert_eq!(code, p1_host::run::EXIT_CANCELLED);
    assert_eq!(fakes.main.requests().len(), 1);
    assert!(fakes.other.requests().is_empty());
    assert_eq!(result_of(&scratch)["outcome"], "cancelled");
}

/// One `p1 workflow run` of `script`: the fakes, the scratch tree, the exit code and
/// the host's stderr.
async fn run(settings: &str, script: &str, main: Vec<Step>) -> (Fakes, Scratch, i32, String) {
    let (fakes, scratch, code, stderr, _) = run_with_budget(settings, script, main, 3).await;
    (fakes, scratch, code, stderr)
}

async fn run_with_budget(
    settings: &str,
    script: &str,
    main: Vec<Step>,
    retries: usize,
) -> (Fakes, Scratch, i32, String, Vec<Duration>) {
    let scratch = Scratch::with_settings(settings);
    std::fs::write(
        scratch.workspace.path().join("note.txt"),
        "committed tool output",
    )
    .unwrap();
    let fakes = Fakes::new(Vec::new(), main, done("ran on the fallback"));
    let script = scratch.script("fallback.rhai", script);
    let out = scratch.root.path().join("runs");
    let mut harness = scratch.harness();
    harness.deps.catalog_hook = Some(fakes.hook());
    let waits = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let asked = waits.clone();
    harness.deps.wait = std::sync::Arc::new(move |wait| {
        asked.lock().unwrap().push(wait);
        Box::pin(std::future::ready(()))
    });
    let code = run_args(
        &mut harness,
        &[
            "workflow",
            "run",
            script.to_str().unwrap(),
            "--out",
            out.to_str().unwrap(),
            "--workspace",
            scratch.workspace.path().to_str().unwrap(),
            "--provider-retries",
            &retries.to_string(),
        ],
    )
    .await;
    let stderr = harness.stderr.text();
    let waits = waits.lock().unwrap().clone();
    (fakes, scratch, code, stderr, waits)
}

fn result_of(scratch: &Scratch) -> serde_json::Value {
    read_json(&scratch.root.path().join("runs/wf1/result.json"))
}

fn journal_kinds(scratch: &Scratch) -> Vec<String> {
    std::fs::read_to_string(scratch.root.path().join("runs/wf1/journal.jsonl"))
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
        .map(|record| record["kind"].as_str().unwrap().to_string())
        .collect()
}

/// The chain is settings a host parses: a role that names `fallback` has one, a role the
/// user table did not name (and a role that names a `model` only) has none.
#[test]
fn the_host_parses_a_roles_fallback_chain() {
    let scratch = Scratch::with_settings(ROLES_WITH_FALLBACK);
    let harness = scratch.harness();
    let settings = p1_host::models::workflow_settings(&p1_host::auth::locations(&harness.deps))
        .expect("the table loads");
    assert_eq!(settings.roles["worker"].fallback, ["fake/other"]);
    assert!(settings.roles["judge"].fallback.is_empty());

    let named = Scratch::with_settings(
        "[workflows.roles.worker]\nmodel = \"fake/main\"\ntools = [\"read\"]\n",
    );
    let harness = named.harness();
    let settings = p1_host::models::workflow_settings(&p1_host::auth::locations(&harness.deps))
        .expect("the table loads");
    assert!(
        settings.roles["worker"].fallback.is_empty(),
        "a named role replaces the shipped one wholesale"
    );
}

#[tokio::test]
async fn a_route_failure_on_the_head_runs_the_next_link() {
    tokio::time::timeout(Duration::from_secs(60), async {
        route_failure_body().await;
    })
    .await
    .expect("fallback workflow run hung");
}

async fn route_failure_body() {
    let (fakes, scratch, code, stderr) =
        run(ROLES_WITH_FALLBACK, ONE_STEP, vec![exhausted()]).await;
    assert_eq!(code, 0, "completed: {stderr}");

    // Two step workers were assembled — one per link — and the fallback link ran its
    // turn (one request: the accepted finish ends it — ADR-0120).
    assert_eq!(fakes.builds(), 2, "one worker per link: {stderr}");
    assert_eq!(fakes.main.requests().len(), 1);
    assert_eq!(fakes.other.requests().len(), 1);

    // The line names the chain the step walked, not just the model that answered.
    let lines = step_lines(&stderr, "wf1");
    assert_eq!(lines.len(), 1, "{stderr}");
    assert!(
        lines[0]
            .contains("workflow wf1 one (worker → fake/main route failed → fake/other; w2) done"),
        "{lines:?}"
    );
    assert!(
        stderr.contains("0 invalid output; 1 fell back"),
        "the run line counts the hop: {stderr}"
    );

    let result = result_of(&scratch);
    assert_eq!(result["counts"]["done"], 1, "{result}");
    assert_eq!(result["counts"]["failed"], 0, "{result}");
    assert_eq!(result["counts"]["fell_back"], 1, "{result}");
    assert_eq!(
        result["value"]["value"], "ran on the fallback",
        "the value is the link that answered: {result}"
    );
    assert_eq!(result["value"]["attempts"], 2, "{result}");
    assert_eq!(
        result["steps"][0]["models"],
        json!([
            {"model": "fake/main", "moved_on": "route_failed"},
            {"model": "fake/other", "moved_on": null},
        ]),
        "{result}"
    );

    // Every model tried was a dispatch of its own, and the hop is between them.
    assert_eq!(
        journal_kinds(&scratch),
        [
            "started", "dispatch", "fallback", "dispatch", "result", "ended"
        ]
    );
}

/// The mapping is for the ROUTE alone: a worker whose turn completed without a `finish`
/// executed and failed, so the chain is not walked — its one nudge (ADR-0072) is another
/// turn of the SAME worker on the same route.
#[tokio::test]
async fn a_step_that_ran_and_failed_does_not_fall_back() {
    tokio::time::timeout(Duration::from_secs(60), async {
        let (fakes, scratch, code, stderr) = run(
            ROLES_WITH_FALLBACK,
            ONE_STEP,
            vec![
                text_response("nothing done"),
                text_response("still nothing"),
            ],
        )
        .await;
        assert_eq!(code, 2, "completed with issues: {stderr}");
        assert_eq!(fakes.builds(), 1, "no second worker: {stderr}");
        assert!(fakes.other.requests().is_empty());
        let nudged = fakes.main.requests();
        assert_eq!(nudged.len(), 2, "one nudge turn: {stderr}");
        assert!(
            format!("{:?}", nudged[1].history)
                .contains("You ended your turn without calling finish."),
            "the nudge is the worker's next message"
        );
        let lines = step_lines(&stderr, "wf1");
        assert!(
            lines[0].contains("(worker → fake/main; w1) failed — attempts 2; ended without finish"),
            "{lines:?}"
        );
        let result = result_of(&scratch);
        assert_eq!(result["counts"]["fell_back"], 0, "{result}");
        assert_eq!(result["counts"]["failed"], 1, "{result}");
        assert_eq!(result["counts"]["capped"], 0, "{result}");
    })
    .await
    .expect("run hung");
}

/// A role with no chain still reports the route's failure as one (ADR-0054 item 4):
/// the step fails `route: <error>` rather than looking like the worker's own work.
#[tokio::test]
async fn a_role_without_a_chain_still_names_the_route_failure() {
    tokio::time::timeout(Duration::from_secs(60), async {
        let (fakes, scratch, code, stderr) =
            run(workflow_common::ROLES, ONE_STEP, vec![exhausted()]).await;
        assert_eq!(code, 2, "completed with issues: {stderr}");
        assert_eq!(fakes.builds(), 1, "there is no link to fall back to");
        let result = result_of(&scratch);
        assert_eq!(
            result["value"]["error"],
            json!("route: InsufficientBalance: the account has no balance"),
            "{result}"
        );
        assert_eq!(
            result["value"]["models"],
            json!([{"model": "fake/main", "moved_on": null}]),
            "{result}"
        );
        assert!(
            stderr.contains("failed — route: InsufficientBalance"),
            "{stderr}"
        );
        assert_eq!(result["counts"]["fell_back"], 0, "{result}");
    })
    .await
    .expect("run hung");
}
