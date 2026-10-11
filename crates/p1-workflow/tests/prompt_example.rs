//! The example script every main-agent prompt carries (the `workflows` prompt fragment,
//! "# Workflows") must run on the real engine: a model copies it, so an idiom the engine
//! rejects would fail every first workflow. The script is carried here as the fragment held
//! it on 2026-10-11 and is independent of the prompt files now, so a moved or edited prompt
//! cannot break this target (#843); a change to the fragment's example is a change here.

mod support;

use p1_workflow::{RunOutcome, SchemaCheck, WorkflowSettings};
use serde_json::json;
use support::{Harness, done_with};

const EXAMPLE_SCRIPT: &str = r#"let task = if has(args, "task") { args.task } else { "the user's task" };
let dims = ["correctness", "tests", "docs"];
let schema = #{ type: "object", properties: #{ findings: #{
    type: "array", items: #{ type: "object",
    properties: #{ title: #{ type: "string" }, file: #{ type: "string" } },
    required: ["title", "file"], additionalProperties: false }
} }, required: ["findings"], additionalProperties: false };
phase("review");
let reviews = parallel(dims.map(|d| || agent(
    "Review " + d + " for: " + task,
    #{ role: "reviewer", label: "review:" + d, schema: schema }
)));
let findings = [];
for r in reviews {
    if r.status == "done" { findings += r.value.findings; }
}
phase("verify");
let verdicts = pipeline(findings, |f| agent(
    "Refute or confirm: " + json(f),
    #{ role: "verifier", label: "verify:" + f.title }
));
#{ confirmed: verdicts.filter(|v| v.status == "done"), reviews: reviews.len() }
"#;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_prompts_example_script_runs_on_the_engine() {
    // The shipped roles: the example names `reviewer` and `verifier`, as a user's run would.
    let harness = Harness::with(WorkflowSettings::shipped());
    for dimension in ["correctness", "tests", "docs"] {
        harness.runner.queue(
            &format!("Review {dimension} for: the user's task"),
            done_with(
                json!({ "findings": [{ "title": format!("{dimension} finding"), "file": "src/lib.rs" }] }),
                SchemaCheck::Passed,
            ),
        );
    }
    let report = harness.run(EXAMPLE_SCRIPT).await;
    assert_eq!(report.outcome, RunOutcome::Completed, "{report:?}");
    assert_eq!(report.counts.steps, 6, "three reviews, three verdicts");
    assert_eq!(report.value["reviews"], json!(3));
    assert_eq!(
        report.value["confirmed"]
            .as_array()
            .map(Vec::len)
            .unwrap_or(0),
        3
    );
    let prompts = harness.runner.prompts();
    assert!(
        prompts
            .iter()
            .any(|prompt| prompt
                .starts_with("Refute or confirm: {\"file\":\"src/lib.rs\",\"title\":")),
        "json() renders the finding canonically inside the verifier prompt: {prompts:?}"
    );
    let mut verifier_labels: Vec<String> = harness
        .runner
        .requests()
        .into_iter()
        .filter(|request| request.role == "verifier")
        .filter_map(|request| request.label)
        .collect();
    // `pipeline` items run concurrently, so their dispatch order is not fixed.
    verifier_labels.sort();
    assert_eq!(
        verifier_labels,
        [
            "verify:correctness finding",
            "verify:docs finding",
            "verify:tests finding"
        ]
    );
}
