//! `p1 workflow run` (ADR-0053 item 7): a run with no parent agent. Exit code by
//! outcome, the `workflow_result` rendering on stdout, `result.json` in `--out`, role
//! overrides reaching the step's model, and `--arg`/`--args` reaching `args`.
#![cfg(feature = "workflows")]

mod common;
mod workflow_common;

use std::sync::Arc;
use std::time::Duration;

use common::{ChannelInterrupt, run_args};
use p1_contracts::{
    BoxFuture, CancellationToken, Provider, ProviderError, ProviderRequest, ProviderStream,
    RouteDescription,
};
use p1_testkit::{ScriptedProvider, text_response};
use workflow_common::{Fakes, Scratch, done, history_text, read_json, step_lines};

const TWO_STEPS: &str = r#"
let a = agent("first for " + args.name + " n=" + args.n, #{ label: "one" });
let b = agent("second with " + args.items.len() + " items from " + args.source, #{ label: "two" });
[a.value, b.value]
"#;

fn base_args(scratch: &Scratch, script: &str, out: &str) -> Vec<String> {
    vec![
        "workflow".into(),
        "run".into(),
        script.into(),
        "--out".into(),
        out.into(),
        "--workspace".into(),
        scratch.workspace.path().to_str().unwrap().into(),
    ]
}

#[tokio::test]
async fn a_two_step_script_runs_without_a_parent() {
    tokio::time::timeout(Duration::from_secs(60), async {
        two_step_body().await;
    })
    .await
    .expect("two-step workflow run hung");
}

async fn two_step_body() {
    let scratch = Scratch::new();
    let script = scratch.script("two.rhai", TWO_STEPS);
    let args_file = scratch.workspace.path().join("args.json");
    std::fs::write(
        &args_file,
        r#"{"name":"from-file","source":"file","items":[1]}"#,
    )
    .unwrap();
    let out = scratch.root.path().join("runs");
    let fakes = Fakes::new(
        Vec::new(),
        [done("first done"), done("second done")].concat(),
        Vec::new(),
    );
    let mut harness = scratch.harness();
    harness.deps.catalog_hook = Some(fakes.hook());

    let mut args = base_args(&scratch, script.to_str().unwrap(), out.to_str().unwrap());
    args.extend(
        [
            "--args",
            args_file.to_str().unwrap(),
            "--arg",
            "name=cli",
            "--arg",
            "n=3",
            "--arg",
            "items=[1,2,3]",
            "--role",
            "reviewer=fake/other",
            "--yes",
        ]
        .map(String::from),
    );
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let code = run_args(&mut harness, &args).await;
    let (stdout, stderr) = (harness.stdout.text(), harness.stderr.text());
    assert_eq!(code, 0, "stderr: {stderr}");

    // `--arg` lies over `--args`; a JSON value is passed as JSON.
    let main = fakes.main.requests();
    assert!(
        history_text(&main[0]).contains("first for cli n=3"),
        "{}",
        history_text(&main[0])
    );

    // The role override is preflighted but only `worker` runs here: every step on `main`.
    assert_eq!(fakes.other.requests().len(), 0);
    assert_eq!(step_lines(&stderr, "wf1").len(), 2, "{stderr}");

    // The workers' `[wN]` event lines come first, as for any worker; the report last.
    let report = &stdout[stdout.find("Workflow wf1: ").expect("the report")..];
    assert!(
        report.starts_with("Workflow wf1: completed — 2 steps"),
        "{stdout}"
    );
    assert!(report.contains("run dir: "), "{stdout}");
    let result = read_json(&out.join("wf1/result.json"));
    assert_eq!(result["outcome"], "completed");
}

#[tokio::test]
async fn a_role_override_reaches_the_step_and_items_come_from_the_args_file() {
    tokio::time::timeout(Duration::from_secs(60), async {
        role_override_body().await;
    })
    .await
    .expect("role-override workflow run hung");
}

async fn role_override_body() {
    let scratch = Scratch::new();
    let script = scratch.script("two.rhai", TWO_STEPS);
    let args_file = scratch.workspace.path().join("args.json");
    std::fs::write(
        &args_file,
        r#"{"name":"file","n":1,"source":"the file","items":["a","b"]}"#,
    )
    .unwrap();
    let out = scratch.root.path().join("runs");
    let fakes = Fakes::new(
        Vec::new(),
        Vec::new(),
        [done("first done"), done("second done")].concat(),
    );
    let mut harness = scratch.harness();
    harness.deps.catalog_hook = Some(fakes.hook());

    let mut args = base_args(&scratch, script.to_str().unwrap(), out.to_str().unwrap());
    args.extend(
        [
            "--args",
            args_file.to_str().unwrap(),
            "--role",
            "worker=fake/other",
        ]
        .map(String::from),
    );
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let code = run_args(&mut harness, &args).await;
    let stderr = harness.stderr.text();
    assert_eq!(code, 0, "stderr: {stderr}");

    assert_eq!(fakes.main.requests().len(), 0, "no step ran on the default");
    let other = fakes.other.requests();
    assert_eq!(other.len(), 4);
    assert!(
        history_text(&other[2]).contains("second with 2 items from the file"),
        "{}",
        history_text(&other[2])
    );
    let lines = step_lines(&stderr, "wf1");
    assert!(
        lines[0].contains("(worker → fake/other; w1) done"),
        "{lines:?}"
    );
}

#[tokio::test]
async fn a_failing_script_exits_1_and_issues_exit_2() {
    tokio::time::timeout(Duration::from_secs(60), async {
        failing_script_body().await;
    })
    .await
    .expect("failing workflow run hung");
}

async fn failing_script_body() {
    let scratch = Scratch::new();
    let out = scratch.root.path().join("runs");

    let failing = scratch.script("fail.rhai", r#"throw "boom""#);
    let fakes = Fakes::new(Vec::new(), Vec::new(), Vec::new());
    let mut harness = scratch.harness();
    harness.deps.catalog_hook = Some(fakes.hook());
    let args = base_args(&scratch, failing.to_str().unwrap(), out.to_str().unwrap());
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let code = run_args(&mut harness, &args).await;
    assert_eq!(code, 1, "failed: {}", harness.stderr.text());
    assert!(harness.stdout.text().starts_with("Workflow wf1: failed"));

    // A step that ends without `finish` is an issue, not a failure of the run.
    let issues = scratch.script("issues.rhai", r#"agent("talk only").status"#);
    let fakes = Fakes::new(Vec::new(), vec![text_response("no finish")], Vec::new());
    let mut harness = scratch.harness();
    harness.deps.catalog_hook = Some(fakes.hook());
    let args = base_args(&scratch, issues.to_str().unwrap(), out.to_str().unwrap());
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let code = run_args(&mut harness, &args).await;
    assert_eq!(code, 2, "completed with issues: {}", harness.stderr.text());
    let stdout = harness.stdout.text();
    assert!(
        stdout.contains("\nWorkflow wf2: completed with issues — 1 steps"),
        "{stdout}"
    );
}

/// Fires the host's Ctrl-C once the step worker is inside its provider call, then
/// answers only when cancelled.
struct Interrupting {
    inner: ScriptedProvider,
    interrupt: Arc<ChannelInterrupt>,
}

impl Provider for Interrupting {
    fn describe(&self) -> RouteDescription {
        self.inner.describe()
    }

    fn validate(&self, request: &ProviderRequest) -> Result<(), ProviderError> {
        self.inner.validate(request)
    }

    fn stream<'a>(
        &'a self,
        request: ProviderRequest,
        cancel: CancellationToken,
    ) -> BoxFuture<'a, Result<ProviderStream, ProviderError>> {
        Box::pin(async move {
            self.interrupt.fire();
            cancel.cancelled().await;
            self.inner.stream(request, cancel).await
        })
    }
}

#[tokio::test]
async fn ctrl_c_cancels_the_run_and_exits_130() {
    tokio::time::timeout(Duration::from_secs(60), async {
        ctrl_c_body().await;
    })
    .await
    .expect("cancelled workflow run hung");
}

async fn ctrl_c_body() {
    let scratch = Scratch::new();
    let script = scratch.script("stuck.rhai", r#"agent("never ends")"#);
    let out = scratch.root.path().join("runs");
    let mut harness = scratch.harness();
    let stuck: Arc<dyn Provider> = Arc::new(Interrupting {
        inner: ScriptedProvider::new(vec![text_response("too late")]),
        interrupt: harness.interrupt.clone(),
    });
    harness.deps.catalog_hook = Some(common::provider_hook_arc(vec![("route-fake", stuck)]));

    let args = base_args(&scratch, script.to_str().unwrap(), out.to_str().unwrap());
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let code = run_args(&mut harness, &args).await;
    assert_eq!(code, 130, "stderr: {}", harness.stderr.text());
    let result = read_json(&out.join("wf1/result.json"));
    assert_eq!(result["outcome"], "cancelled", "{result}");
}

#[test]
fn the_command_line_is_checked_before_anything_runs() {
    let parse = |args: &[&str]| {
        let args: Vec<String> = args.iter().map(|arg| arg.to_string()).collect();
        p1_host::cli::parse(&args)
    };
    assert!(parse(&["workflow", "run", "x.rhai"]).is_ok());
    assert!(parse(&["workflow", "x.rhai"]).is_err(), "`run` is required");
    assert!(parse(&["workflow", "run"]).is_err(), "a script is required");
    assert!(parse(&["workflow", "run", "x.rhai", "--arg", "novalue"]).is_err());
    assert!(parse(&["workflow", "run", "x.rhai", "--role", "worker"]).is_err());
    assert!(parse(&["workflow", "run", "x.rhai", "--max-workers", "0"]).is_err());
}
