//! `scripts/audits/modularity.rhai` (the modularity audit as a workflow, ADR-0053 live
//! check 5) runs on the engine: one Find step per unit, dedupe by evidence location, two
//! refuters per finding and a third vote on a split high/medium finding, then the buckets.
//! The prompts a refuter gets are rebuilt here byte for byte, because the scripted runner
//! answers by exact prompt.

mod support;

use p1_workflow::{RunOutcome, SchemaCheck, WorkflowSettings};
use serde_json::{Value, json};
use support::{Harness, done_with};

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

/// The refuter prompt exactly as the script builds it (`json()` is canonical: sorted keys,
/// no spaces — `serde_json::Value` renders objects the same way).
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_audit_script_finds_dedupes_verifies_and_buckets() {
    let args = json!({
        "sha": "abc123",
        "facts": "# Facts\n- p1-core → p1-contracts",
        "units": [
            {"label": "core", "lens": "core-purity", "files": ["crates/p1-core/src/lib.rs"], "brief": "find in core"},
            {"label": "host", "lens": "composition-root", "files": ["crates/p1-host/src/run.rs"], "brief": "find in host"}
        ],
        "findings_schema": {"type": "object"},
        "verdict_schema": {"type": "object"},
        "refuter": {"rule": "RULE lens", "code": "CODE lens", "code-second": "CODE second"},
        "refute_preamble": "# Refute"
    });
    let harness = Harness::with(WorkflowSettings::shipped());
    let runner = &harness.runner;
    // core: two findings; host: one that duplicates core's first (same file, line within 3)
    // and one of its own. Three survive.
    let both = finding("f1", "crates/p1-core/src/lib.rs", 10, "high");
    let split = finding("f2", "crates/p1-core/src/lib.rs", 40, "medium");
    let low_split = finding("f3", "crates/p1-host/src/run.rs", 5, "low");
    let duplicate = finding("dup", "crates/p1-core/src/lib.rs", 12, "high");
    runner.queue(
        "find in core",
        done_with(
            json!({"unit": "core", "read": [], "findings": [both, split]}),
            SchemaCheck::Passed,
        ),
    );
    runner.queue(
        "find in host",
        done_with(
            json!({"unit": "host", "read": [], "findings": [duplicate, low_split]}),
            SchemaCheck::Passed,
        ),
    );
    // f1: both refuters uphold → confirmed. f2: split, medium → third vote upholds →
    // confirmed. f3: split, low → no third vote → discarded.
    let tagged = |f: &Value, unit: &str, lens: &str| {
        let mut f = f.clone();
        f["unit"] = json!(unit);
        f["lens"] = json!(lens);
        f
    };
    let f1 = tagged(&both, "core", "core-purity");
    let f2 = tagged(&split, "core", "core-purity");
    let f3 = tagged(&low_split, "host", "composition-root");
    runner.queue(&refute_prompt(&args, &f1, "rule"), verdict("upheld"));
    runner.queue(&refute_prompt(&args, &f1, "code"), verdict("upheld"));
    runner.queue(&refute_prompt(&args, &f2, "rule"), verdict("refuted"));
    runner.queue(&refute_prompt(&args, &f2, "code"), verdict("upheld"));
    runner.queue(&refute_prompt(&args, &f2, "code-second"), verdict("upheld"));
    runner.queue(&refute_prompt(&args, &f3, "rule"), verdict("upheld"));
    runner.queue(&refute_prompt(&args, &f3, "code"), verdict("refuted"));

    let mut request = support::request(&audit_script());
    request.args = args.clone();
    let id = harness.start_request(request).await;
    let report = harness.wait(&id).await;
    assert_eq!(report.outcome, RunOutcome::Completed, "{report:?}");
    assert_eq!(
        report.counts.steps,
        2 + 2 + 3 + 2,
        "finds, then 2+3+2 refuter votes"
    );
    assert_eq!(report.value["commit"], json!("abc123"));
    let ids = |bucket: &str| -> Vec<String> {
        report.value[bucket]
            .as_array()
            .unwrap()
            .iter()
            .map(|entry| entry["finding"]["id"].as_str().unwrap().to_string())
            .collect()
    };
    assert_eq!(ids("confirmed"), ["f1", "f2"]);
    assert_eq!(ids("discarded"), ["f3"]);
    assert_eq!(
        report.value["confirmed"][1]["votes"]
            .as_array()
            .unwrap()
            .len(),
        3,
        "the split medium finding got its third vote"
    );
    let labels: Vec<String> = harness
        .runner
        .requests()
        .into_iter()
        .filter_map(|request| request.label)
        .filter(|label| label.starts_with("verify-"))
        .collect();
    assert!(
        labels.contains(&"verify-core-f2-code-second".to_string()),
        "{labels:?}"
    );
    assert!(
        harness
            .runner
            .requests()
            .iter()
            .all(|request| request.tools == ["read", "grep", "shell"]),
        "audit steps are read-only plus a shell for the repro"
    );
}
