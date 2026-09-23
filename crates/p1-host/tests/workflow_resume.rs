//! ADR-0053 item 3: a run resumed from an earlier run's journal replays every step it
//! can — the same script builds ZERO new workers and reports `replayed == steps`.
//! Both runs go through `p1 workflow run` into one run root; `workflow_start`'s
//! `resume_from` reaches the same `StartRequest` field.
#![cfg(feature = "workflows")]

mod common;
mod workflow_common;

use common::run_args;
use workflow_common::{Fakes, Scratch, done, read_json};

const SCRIPT: &str = r#"
let a = agent("first", #{ label: "a" });
let b = agent("second from " + a.value, #{ label: "b" });
[a.value, b.value]
"#;

#[tokio::test]
async fn resuming_the_same_script_replays_every_step() {
    let scratch = Scratch::new();
    let script = scratch.script("resume.rhai", SCRIPT);
    let out = scratch.root.path().join("runs");
    let fakes = Fakes::new(Vec::new(), [done("one"), done("two")].concat(), Vec::new());
    let run = |extra: &'static [&'static str]| {
        let mut harness = scratch.harness();
        harness.deps.catalog_hook = Some(fakes.hook());
        let mut args = vec![
            "workflow".to_string(),
            "run".to_string(),
            script.to_str().unwrap().to_string(),
            "--out".to_string(),
            out.to_str().unwrap().to_string(),
            "--workspace".to_string(),
            scratch.workspace.path().to_str().unwrap().to_string(),
        ];
        args.extend(extra.iter().map(|arg| arg.to_string()));
        async move {
            let args: Vec<&str> = args.iter().map(String::as_str).collect();
            let code = run_args(&mut harness, &args).await;
            (code, harness.stderr.text())
        }
    };

    let (code, stderr) = run(&[]).await;
    assert_eq!(code, 0, "{stderr}");
    assert_eq!(fakes.builds(), 2);
    let first = read_json(&out.join("wf1/result.json"));

    let (code, stderr) = run(&["--resume-from", "wf1"]).await;
    assert_eq!(code, 0, "{stderr}");
    assert_eq!(fakes.builds(), 2, "no new worker on resume");
    assert_eq!(fakes.main.requests().len(), 4, "no new request on resume");

    let resumed = read_json(&out.join("wf2/result.json"));
    assert_eq!(resumed["counts"]["steps"], 2, "{resumed}");
    assert_eq!(resumed["counts"]["replayed"], 2, "{resumed}");
    assert_eq!(resumed["value"], first["value"]);
    assert!(
        stderr.contains("a (worker → fake/main; w1) done — replayed"),
        "{stderr}"
    );
}
