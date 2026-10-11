//! ADR-0053 item 5: a step with `schema` gets `finish` with a `result` parameter; an
//! invalid result is repaired ONCE by the same worker, then the step fails
//! `invalid_output`. Driven through `p1 workflow run` on scripted providers. The step
//! environment lists the benched `finish` (ADR-0187); the `bare` cases show a step
//! without it.
#![cfg(feature = "workflows")]

mod common;
mod workflow_common;

use std::time::Duration;

use common::run_args;
use p1_testkit::text_response;
use p1_testkit::{json_call, tool_call_response};
use workflow_common::{
    Fakes, ROLES, Scratch, done_with, history_text, read_json, step_lines, write,
};

const SCRIPT: &str = r#"
let schema = #{ type: "object", required: ["n"], properties: #{ n: #{ type: "integer" } } };
agent("count them", #{ schema: schema })
"#;

async fn run(scratch: &Scratch, fakes: &Fakes) -> (i32, serde_json::Value) {
    run_script(scratch, fakes, SCRIPT).await
}

async fn run_script(scratch: &Scratch, fakes: &Fakes, text: &str) -> (i32, serde_json::Value) {
    let script = scratch.script("schema.rhai", text);
    let out = scratch.root.path().join("runs");
    let mut harness = scratch.harness();
    harness.deps.catalog_hook = Some(fakes.hook());
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
            "--yes",
        ],
    )
    .await;
    let result = read_json(&out.join("wf1/result.json"));
    (code, result)
}

/// A scratch tree whose roles all run on a `bare` step environment: the fake route and
/// profile without `finish`, like every shipped environment (ADR-0187).
fn bare_scratch() -> Scratch {
    let scratch = Scratch::with_settings(&ROLES.replace("fake/main", "bare/main"));
    let bare = scratch.root.path().join("environments/bare");
    write(
        &bare.join("environment.toml"),
        "route   = \"route-fake\"\nprofile = \"main\"\n",
    );
    write(&bare.join("prompt.md"), "STEP {{tool_names}}\n");
    scratch
}

/// ADR-0187: a step whose worker has no `finish` is `done` when its turn completes, its
/// last message the value, unverified, with no repair turn.
#[tokio::test]
async fn a_step_without_finish_is_done_by_its_turn_end_without_a_repair() {
    tokio::time::timeout(Duration::from_secs(60), async {
        let scratch = bare_scratch();
        let fakes = Fakes::new(Vec::new(), vec![text_response("the answer")], Vec::new());
        let (code, result) = run_script(&scratch, &fakes, r#"agent("answer it")"#).await;
        assert_eq!(code, 0, "{result}");

        let requests = fakes.main.requests();
        assert_eq!(requests.len(), 1, "no repair turn after the answer");
        let tools: Vec<&str> = requests[0]
            .tools
            .iter()
            .map(|tool| tool.name.as_str())
            .collect();
        assert_eq!(
            tools,
            ["read", "grep"],
            "exactly the role's grant, no `finish`"
        );

        let envelope = &result["value"];
        assert_eq!(envelope["status"], "done", "{envelope}");
        assert_eq!(envelope["value"], "the answer");
        assert_eq!(
            envelope["evidence"],
            "not verified; parent verification required"
        );
        assert_eq!(envelope["attempts"], 1);
        assert!(envelope["error"].is_null(), "{envelope}");
    })
    .await
    .expect("finish-less step workflow hung");
}

/// ADR-0187: a structured result is returned through `finish`, so a `schema` step whose
/// role's environment does not list it fails before any worker runs.
#[tokio::test]
async fn a_schema_step_needs_an_environment_that_lists_finish() {
    tokio::time::timeout(Duration::from_secs(60), async {
        let scratch = bare_scratch();
        let fakes = Fakes::new(Vec::new(), Vec::new(), Vec::new());
        let (code, result) = run(&scratch, &fakes).await;
        assert_eq!(code, 2, "completed with issues: {result}");
        assert!(fakes.main.requests().is_empty(), "no worker ran");

        let envelope = &result["value"];
        assert_eq!(envelope["status"], "failed", "{envelope}");
        let error = envelope["error"].as_str().unwrap();
        assert!(
            error.contains("`finish`") && error.contains("`bare`"),
            "{envelope}"
        );
    })
    .await
    .expect("schema workflow on a bare environment hung");
}

#[tokio::test]
async fn an_invalid_result_is_repaired_by_the_same_worker() {
    tokio::time::timeout(Duration::from_secs(60), async {
        repaired_body().await;
    })
    .await
    .expect("schema repair workflow hung");
}

async fn repaired_body() {
    let scratch = Scratch::new();
    let fakes = Fakes::new(
        Vec::new(),
        [
            done_with("first", r#"{"n":"many"}"#),
            done_with("second", r#"{"n":3}"#),
        ]
        .concat(),
        Vec::new(),
    );
    let (code, result) = run(&scratch, &fakes).await;
    assert_eq!(code, 0, "{result}");

    let requests = fakes.main.requests();
    assert_eq!(fakes.builds(), 1, "one worker for both attempts");
    // ADR-0120: an accepted `finish` ends the worker's turn, so the invalid first
    // result costs one request and the repair turn costs one more.
    assert_eq!(
        requests.len(),
        2,
        "a turn, then the repair turn of the same worker"
    );
    let finish = requests[0]
        .tools
        .iter()
        .find(|tool| tool.name == "finish")
        .expect("a step worker has finish");
    let p1_contracts::DeclarationKind::Function { input_schema } = &finish.kind else {
        panic!("finish is a function tool");
    };
    assert!(
        input_schema["properties"]["result"].is_object(),
        "{input_schema}"
    );
    assert!(
        !history_text(&requests[0]).contains("did not match the required schema"),
        "no repair before the turn ended"
    );
    assert!(
        history_text(&requests[1]).contains("did not match the required schema"),
        "{}",
        history_text(&requests[1])
    );

    let envelope = &result["value"];
    assert_eq!(envelope["status"], "done", "{envelope}");
    assert_eq!(envelope["schema"], "passed");
    assert_eq!(envelope["attempts"], 2);
    assert_eq!(envelope["value"], serde_json::json!({"n": 3}));
}

#[tokio::test]
async fn an_invalid_result_twice_fails_the_step_as_invalid_output() {
    tokio::time::timeout(Duration::from_secs(60), async {
        invalid_output_body().await;
    })
    .await
    .expect("invalid-output workflow hung");
}

async fn invalid_output_body() {
    let scratch = Scratch::new();
    let fakes = Fakes::new(
        Vec::new(),
        [
            done_with("first", r#"{"n":"many"}"#),
            done_with("second", r#"{"n":"still many"}"#),
        ]
        .concat(),
        Vec::new(),
    );
    let (code, result) = run(&scratch, &fakes).await;
    assert_eq!(code, 2, "completed with issues: {result}");
    assert_eq!(fakes.builds(), 1);

    let envelope = &result["value"];
    assert_eq!(envelope["status"], "failed", "{envelope}");
    assert_eq!(envelope["attempts"], 2);
    assert!(
        envelope["error"]
            .as_str()
            .unwrap()
            .starts_with("invalid_output: "),
        "{envelope}"
    );
    assert_eq!(result["counts"]["invalid_output"], 1, "{result}");
}

/// A repair turn that ends `blocked` stores `needs`, and the observer must still see
/// it when it renders the step line: the repair's cleanup guard must not drop a need
/// that a successful repair left for `step_ended`.
#[tokio::test]
async fn a_repair_that_ends_blocked_keeps_its_needs() {
    tokio::time::timeout(Duration::from_secs(60), async {
        blocked_repair_body().await;
    })
    .await
    .expect("blocked repair workflow hung");
}

async fn blocked_repair_body() {
    let scratch = Scratch::new();
    let fakes = Fakes::new(
        Vec::new(),
        [
            done_with("first", r#"{"n":"many"}"#),
            vec![tool_call_response(vec![json_call(
                "f1",
                "finish",
                r#"{"status":"blocked","summary":"cannot count","needs":"a countable list"}"#,
            )])],
        ]
        .concat(),
        Vec::new(),
    );
    let script = scratch.script("blocked.rhai", SCRIPT);
    let out = scratch.root.path().join("runs");
    let mut harness = scratch.harness();
    harness.deps.catalog_hook = Some(fakes.hook());
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
            "--yes",
        ],
    )
    .await;
    let stderr = harness.stderr.text();
    assert_eq!(code, 2, "completed with issues: {stderr}");
    let lines = step_lines(&stderr, "wf1");
    assert!(
        lines
            .iter()
            .any(|line| line.contains("blocked: needs a countable list")),
        "the blocked step must still name its need: {lines:?}\n{stderr}"
    );
}
