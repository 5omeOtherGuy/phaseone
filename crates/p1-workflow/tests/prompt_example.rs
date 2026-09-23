//! The example script every main-agent prompt carries (`environments/*/prompt.md`,
//! "# Workflows") must run on the real engine: a model copies it, so an idiom the
//! engine rejects would fail every first workflow. The five prompts carry the same
//! section (the assembly tests check that); this reads the claude one.

mod support;

use p1_workflow::{RunOutcome, SchemaCheck, WorkflowSettings};
use serde_json::json;
use support::{Harness, done_with};

fn example_script() -> String {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../environments/claude/prompt.md"
    );
    let prompt = std::fs::read_to_string(path).expect("the claude prompt exists");
    let start = prompt
        .find("```rhai\n")
        .expect("the prompt carries a rhai example")
        + "```rhai\n".len();
    let end = prompt[start..].find("```").expect("the example is closed") + start;
    prompt[start..end].to_string()
}

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
    let report = harness.run(&example_script()).await;
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
