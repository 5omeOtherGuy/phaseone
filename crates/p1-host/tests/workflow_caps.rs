//! ADR-0053 item 4: caps count the WIRE model the host's resolver returns. A cap of 1
//! on the fake route's `wire-main` lets one step run; the second is `failed` with
//! `quota_exceeded` and no worker is built for it.
#![cfg(feature = "workflows")]

mod common;
mod workflow_common;

use common::run_args;
use workflow_common::{Fakes, ROLES, Scratch, done, read_json};

#[tokio::test]
async fn a_capped_wire_model_refuses_the_second_step_before_any_worker() {
    let scratch =
        Scratch::with_settings(&format!("{ROLES}\n[workflows.caps]\n\"wire-main\" = 1\n"));
    let fakes = Fakes::new(Vec::new(), [done("one"), done("two")].concat(), Vec::new());
    let script = scratch.script(
        "caps.rhai",
        r#"[agent("first", #{ label: "a" }), agent("second", #{ label: "b" })]"#,
    );
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
        ],
    )
    .await;
    let stderr = harness.stderr.text();
    assert_eq!(code, 2, "completed with issues: {stderr}");
    assert_eq!(fakes.builds(), 1, "no worker for the capped step");
    assert_eq!(fakes.main.requests().len(), 2, "only the first step ran");

    let result = read_json(&out.join("wf1/result.json"));
    let second = &result["value"][1];
    assert_eq!(second["status"], "failed", "{result}");
    assert_eq!(
        second["error"], "quota_exceeded: wire-main used=1 limit=1",
        "{result}"
    );
    assert_eq!(result["counts"]["capped"], 1, "{result}");
    assert!(
        stderr.contains("(worker → fake/main) failed — quota_exceeded: wire-main used=1 limit=1"),
        "{stderr}"
    );
}
