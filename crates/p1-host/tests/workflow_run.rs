//! ADR-0053 end to end: a parent calls `workflow_start` with a two-step script, and
//! the REAL `InProcessWorkers` + `InProcessWorkflows` run it on scripted providers
//! registered through the catalog hook. Asserted on provider requests, the host's
//! stderr lines, the parent's history and the run directory — never on prose.
#![cfg(feature = "workflows")]

mod common;
mod workflow_common;

use std::time::Duration;

use common::run_args;
use workflow_common::{
    Fakes, Scratch, done, history_text, parent_that_runs, read_json, results_of, step_lines,
    tool_names,
};

const TWO_STEPS: &str = r#"
let a = agent("first task", #{ label: "one" });
let b = agent("second task", #{ label: "two" });
[a.value, b.value]
"#;

#[tokio::test]
async fn a_parent_runs_a_two_step_workflow_and_hears_of_it_once() {
    tokio::time::timeout(Duration::from_secs(60), async {
        parent_run_body().await;
    })
    .await
    .expect("parent workflow run hung");
}

async fn parent_run_body() {
    let scratch = Scratch::new();
    let fakes = Fakes::new(
        parent_that_runs(TWO_STEPS, ""),
        [done("first summary"), done("second summary")].concat(),
        Vec::new(),
    );
    let mut harness = scratch.harness();
    harness.deps.catalog_hook = Some(fakes.hook());

    let code = run_args(
        &mut harness,
        &[
            "--yes",
            "--env",
            "parent",
            "--session",
            scratch.session().to_str().unwrap(),
            "--workspace",
            scratch.workspace.path().to_str().unwrap(),
            "go",
        ],
    )
    .await;
    let stderr = harness.stderr.text();
    assert_eq!(code, 0, "stderr: {stderr}");

    // The step workers: exactly the role's tools plus `finish`, the script's prompts.
    let requests = fakes.main.requests();
    assert_eq!(requests.len(), 4, "two steps, two requests each");
    for request in &requests {
        let mut names = tool_names(request);
        names.sort();
        assert_eq!(names, ["finish", "grep", "read"]);
    }
    assert!(history_text(&requests[0]).contains("first task"));
    assert!(history_text(&requests[2]).contains("second task"));
    assert!(
        !history_text(&requests[2]).contains("first task"),
        "a fresh worker per step"
    );

    // One host line per step, and the end line.
    let lines = step_lines(&stderr, "wf1");
    assert_eq!(lines.len(), 2, "stderr: {stderr}");
    assert!(
        lines[0].contains("workflow wf1 one (worker → fake/main; w1) done"),
        "{lines:?}"
    );
    assert!(
        lines[1].contains("workflow wf1 two (worker → fake/main; w2) done"),
        "{lines:?}"
    );
    assert!(
        lines[0].contains("not verified; parent verification required"),
        "{lines:?}"
    );
    assert!(
        stderr.contains("· workflow wf1 completed"),
        "stderr: {stderr}"
    );
    // The step workers' own end lines are the step lines; no `worker_ended` note.
    assert!(!stderr.contains("worker w1 "), "stderr: {stderr}");

    // The parent: every main agent has the four workflow tools; ONE notification.
    let parent = fakes.parent.requests();
    for tool in [
        "workflow_start",
        "workflow_status",
        "workflow_result",
        "workflow_cancel",
    ] {
        assert!(
            tool_names(&parent[0]).iter().any(|name| name == tool),
            "{tool}"
        );
    }
    let last = parent.last().unwrap();
    assert_eq!(
        history_text(last)
            .matches("Workflow wf1 ended (completed)")
            .count(),
        1,
        "{}",
        history_text(last)
    );
    assert!(
        !history_text(last).contains("Worker w1"),
        "no step completion reached the parent"
    );
    let rendered = results_of(last, "workflow_result");
    assert_eq!(rendered.len(), 1);
    assert!(
        rendered[0].starts_with("Workflow wf1: completed — 2 steps"),
        "{}",
        rendered[0]
    );
    assert!(rendered[0].contains("\"first summary\""), "{}", rendered[0]);
    assert!(
        rendered[0].contains("\"second summary\""),
        "{}",
        rendered[0]
    );

    // The run directory, next to the session's worker journals.
    let run_dir = scratch.run_dir("wf1");
    assert!(run_dir.join("journal.jsonl").is_file());
    let result = read_json(&run_dir.join("result.json"));
    assert_eq!(
        result["value"],
        serde_json::json!(["first summary", "second summary"])
    );
}

fn git(dir: &std::path::Path, args: &[&str]) -> String {
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

fn commit(repo: &std::path::Path, name: &str) -> String {
    std::fs::write(repo.join(name), name).unwrap();
    git(repo, &["add", name]);
    git(
        repo,
        &[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "commit",
            "-q",
            "-m",
            name,
        ],
    );
    git(repo, &["rev-parse", "HEAD"])
}

const TWO_TREES: &str = r#"
let a = agent("first task", #{ label: "one", worktree: "e2e-a" });
let b = agent("second task", #{ label: "two", worktree: "e2e-b" });
[a.worktree.path, a.worktree.branch, a.worktree.head, b.worktree.path, b.worktree.branch, b.worktree.head]
"#;

// ADR-0072: two steps, each in its own worktree next to the parent's checkout — one
// made from the run's base, one attached to its existing branch.
#[tokio::test]
async fn two_steps_run_in_two_worktrees() {
    tokio::time::timeout(Duration::from_secs(60), two_trees_body())
        .await
        .expect("worktree workflow run hung");
}

async fn two_trees_body() {
    let scratch = Scratch::new();
    let checkout = tempfile::tempdir().unwrap();
    let repo = checkout.path().join("repo");
    std::fs::create_dir(&repo).unwrap();
    git(&repo, &["init", "-q", "-b", "main"]);
    let older = commit(&repo, "older");
    git(&repo, &["branch", "task/e2e-b"]);
    let base = commit(&repo, "newer");

    let fakes = Fakes::new(
        parent_that_runs(TWO_TREES, ""),
        [done("first summary"), done("second summary")].concat(),
        Vec::new(),
    );
    let mut harness = scratch.harness();
    harness.deps.catalog_hook = Some(fakes.hook());
    let code = run_args(
        &mut harness,
        &[
            "--yes",
            "--env",
            "parent",
            "--session",
            scratch.session().to_str().unwrap(),
            "--workspace",
            repo.to_str().unwrap(),
            "go",
        ],
    )
    .await;
    let stderr = harness.stderr.text();
    assert_eq!(code, 0, "stderr: {stderr}");

    let result = read_json(&scratch.run_dir("wf1").join("result.json"));
    assert_eq!(result["outcome"], "completed", "{result}");
    let value = result["value"].as_array().unwrap().clone();
    let tree_a = std::path::PathBuf::from(value[0].as_str().unwrap());
    let tree_b = std::path::PathBuf::from(value[3].as_str().unwrap());
    let real = |path: &std::path::Path| path.canonicalize().unwrap();
    assert_eq!(real(&tree_a), real(&checkout.path().join("repo-e2e-a")));
    assert_eq!(real(&tree_b), real(&checkout.path().join("repo-e2e-b")));
    assert_eq!(value[1], "task/e2e-a");
    assert_eq!(value[4], "task/e2e-b");
    assert_eq!(value[2], base.as_str(), "a new tree branches from the base");
    assert_eq!(
        value[5],
        older.as_str(),
        "an existing branch keeps its commit"
    );
    assert_eq!(
        git(&tree_a, &["rev-parse", "--abbrev-ref", "HEAD"]),
        "task/e2e-a"
    );
    assert_eq!(
        git(&tree_b, &["rev-parse", "--abbrev-ref", "HEAD"]),
        "task/e2e-b"
    );
    assert_eq!(
        git(&repo, &["rev-parse", "HEAD"]),
        base,
        "the checkout untouched"
    );
}
