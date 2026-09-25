//! A step's own git worktree (ADR-0073): the `worktree` option, the run's base commit
//! carried into every step request and kept on resume, the hold across the step, and the
//! `worktree` field of the envelope. The git work is the host's; here the scripted
//! runner hands out fake worktrees.

mod support;

use std::path::PathBuf;

use p1_workflow::{JournalRecord, RunId, RunOutcome, StartRequest};
use serde_json::json;
use support::{Harness, request};

fn based(script: &str, base: Option<&str>) -> StartRequest {
    StartRequest {
        base: base.map(str::to_string),
        ..request(script)
    }
}

fn started_base(records: &[JournalRecord]) -> Option<String> {
    match &records[0] {
        JournalRecord::Started { base, .. } => base.clone(),
        other => panic!("first record is not Started: {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn the_worktree_option_takes_a_slug_and_nothing_else() {
    let harness = Harness::new();
    let long = "a".repeat(65);
    for slug in [
        r#""""#,
        r#""Upper""#,
        r#""-lead""#,
        r#""trail-""#,
        r#""a_b""#,
        r#""a/b""#,
        r#""../up""#,
        r#""a b""#,
        r#""ü""#,
        &format!("\"{long}\""),
        "5",
    ] {
        let id = harness
            .start_request(based(
                &format!("agent(\"x\", #{{ worktree: {slug} }})"),
                Some("base1"),
            ))
            .await;
        let report = harness.wait(&id).await;
        assert_eq!(report.outcome, RunOutcome::Failed, "{slug}");
        let error = report.error.unwrap();
        assert!(error.contains(r#"option "worktree""#), "{slug}: {error}");
    }

    let id = harness
        .start_request(based(
            r#"agent("x", #{ worktree: "a-1", workspace: "/elsewhere" })"#,
            Some("base1"),
        ))
        .await;
    let report = harness.wait(&id).await;
    assert_eq!(report.outcome, RunOutcome::Failed);
    let error = report.error.unwrap();
    assert!(error.contains("cannot be given together"), "{error}");
    assert!(harness.runner.requests().is_empty());
    assert!(harness.runner.worktree_requests().is_empty());

    let max = "b".repeat(64);
    for slug in ["212-read-tool", "a", "9", "a-b-c", max.as_str()] {
        let id = harness
            .start_request(based(
                &format!("agent(\"{slug}\", #{{ worktree: \"{slug}\" }}).status"),
                Some("base1"),
            ))
            .await;
        let report = harness.wait(&id).await;
        assert_eq!(report.outcome, RunOutcome::Completed, "{slug}: {report:?}");
        assert_eq!(report.value, json!("done"), "{slug}");
    }
    let asked: Vec<Option<String>> = harness
        .runner
        .worktree_requests()
        .into_iter()
        .map(|request| request.worktree)
        .collect();
    assert_eq!(
        asked,
        ["212-read-tool", "a", "9", "a-b-c", max.as_str()]
            .map(|slug| Some(slug.to_string()))
            .to_vec()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_worktree_step_runs_in_its_held_tree_and_returns_it() {
    let harness = Harness::new();
    let script = r#"
        let r = agent("implement", #{ worktree: "212-read-tool" });
        let again = agent("fix it", #{ worktree: "212-read-tool" });
        let review = agent("review", #{ workspace: r.worktree.path });
        [r.worktree.path, r.worktree.branch, r.worktree.head, again.status, review.status]
    "#;
    let id = harness.start_request(based(script, Some("base1"))).await;
    let report = harness.wait(&id).await;
    assert_eq!(report.outcome, RunOutcome::Completed, "{report:?}");
    assert_eq!(
        report.value,
        json!([
            "/fake-worktrees/212-read-tool",
            "task/212-read-tool",
            "ended-212-read-tool",
            "done",
            "done"
        ]),
        "the second step on the same tree ran: the first released it"
    );

    // The runner was asked with the run's own workspace, and the base.
    let asked = harness.runner.worktree_requests();
    assert_eq!(asked.len(), 2);
    assert_eq!(asked[0].workspace, None);
    assert_eq!(asked[0].base.as_deref(), Some("base1"));

    // Each link ran in the worktree; the later step was pointed at exactly that tree.
    let tree = PathBuf::from("/fake-worktrees/212-read-tool");
    let requests = harness.runner.requests();
    assert_eq!(requests.len(), 3);
    for request in &requests {
        assert_eq!(
            request.workspace.as_ref(),
            Some(&tree),
            "{}",
            request.prompt
        );
        assert_eq!(request.base.as_deref(), Some("base1"));
    }
    assert_eq!(requests[0].worktree.as_deref(), Some("212-read-tool"));
    assert_eq!(requests[2].worktree, None);
    assert!(harness.runner.held_worktrees.lock().unwrap().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_refused_worktree_fails_its_step_before_dispatch() {
    let harness = Harness::new();
    harness
        .runner
        .held_worktrees
        .lock()
        .unwrap()
        .push("busy-one".to_string());
    let script = r#"
        let a = agent("wt", #{ worktree: "busy-one" });
        let b = agent("plain");
        [a.status, a.error, a.attempts, b.status]
    "#;
    let id = harness.start_request(based(script, Some("base1"))).await;
    let report = harness.wait(&id).await;
    assert_eq!(report.outcome, RunOutcome::CompletedWithIssues);
    assert_eq!(
        report.value,
        json!(["failed", "worktree_busy: busy-one", 0, "done"])
    );
    assert_eq!(harness.runner.prompts(), ["plain"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn without_a_base_a_worktree_step_fails_and_the_others_run() {
    let harness = Harness::new();
    let script = r#"
        let a = agent("wt", #{ worktree: "x" });
        let b = agent("plain");
        [a.status, a.error, b.status]
    "#;
    let report = harness.run(script).await;
    assert_eq!(report.outcome, RunOutcome::CompletedWithIssues);
    let value = report.value.as_array().unwrap().clone();
    assert_eq!(value[0], json!("failed"));
    let error = value[1].as_str().unwrap();
    assert!(error.starts_with("worktree: x: "), "{error}");
    assert!(error.contains("no base commit"), "{error}");
    assert_eq!(value[2], json!("done"));
    assert!(harness.runner.worktree_requests().is_empty());
    assert_eq!(harness.runner.prompts(), ["plain"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn the_base_reaches_every_step_request_and_the_journal() {
    let harness = Harness::new();
    let script = r#"
        agent("one");
        parallel([|| agent("two"), || agent("three")]);
    "#;
    let id = harness.start_request(based(script, Some("abc123"))).await;
    let report = harness.wait(&id).await;
    assert_eq!(report.outcome, RunOutcome::Completed);
    let requests = harness.runner.requests();
    assert_eq!(requests.len(), 3);
    for request in &requests {
        assert_eq!(
            request.base.as_deref(),
            Some("abc123"),
            "{}",
            request.prompt
        );
        assert_eq!(request.worktree, None);
    }
    assert_eq!(
        started_base(&harness.journal(&id)).as_deref(),
        Some("abc123")
    );

    // No base: the journal line carries no key, so older readers see what they knew.
    let plain = harness.run(r#"agent("four")"#).await;
    let text = std::fs::read_to_string(plain.run_dir.join("journal.jsonl")).unwrap();
    assert!(!text.lines().next().unwrap().contains("base"), "{text}");
    assert_eq!(harness.runner.requests()[3].base, None);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_resumed_run_keeps_its_recorded_base_and_replays_the_worktree() {
    let harness = Harness::new();
    let first = harness
        .start_request(based(
            r#"let r = agent("one", #{ worktree: "t-1" }); r.worktree.head"#,
            Some("old-base"),
        ))
        .await;
    let first = harness.wait(&first).await;
    assert_eq!(first.value, json!("ended-t-1"));
    assert_eq!(harness.runner.worktree_requests().len(), 1);

    let resumed = StartRequest {
        resume_from: Some(RunId(first.id.0.clone())),
        ..based(
            r#"let r = agent("one", #{ worktree: "t-1" }); agent("two"); r.worktree"#,
            Some("new-base"),
        )
    };
    let id = harness.start_request(resumed).await;
    let second = harness.wait(&id).await;
    assert_eq!(second.outcome, RunOutcome::Completed, "{second:?}");
    assert_eq!(second.counts.replayed, 1);
    assert_eq!(
        second.value,
        json!({
            "path": "/fake-worktrees/t-1",
            "branch": "task/t-1",
            "head": "ended-t-1"
        }),
        "the replayed step returns its recorded envelope"
    );
    assert_eq!(
        harness.runner.worktree_requests().len(),
        1,
        "a replayed step makes no worktree"
    );
    let requests = harness.runner.requests();
    assert_eq!(requests.last().unwrap().prompt, "two");
    assert_eq!(
        requests.last().unwrap().base.as_deref(),
        Some("old-base"),
        "the resumed run's recorded base wins"
    );
    assert_eq!(
        started_base(&harness.journal(&id)).as_deref(),
        Some("old-base")
    );

    // A run recorded without a base takes the fresh one.
    let bare = harness.run(r#"agent("three")"#).await;
    let resumed = StartRequest {
        resume_from: Some(RunId(bare.id.0.clone())),
        ..based(r#"agent("five")"#, Some("fresh"))
    };
    let id = harness.start_request(resumed).await;
    harness.wait(&id).await;
    assert_eq!(
        harness.runner.requests().last().unwrap().base.as_deref(),
        Some("fresh")
    );
}
