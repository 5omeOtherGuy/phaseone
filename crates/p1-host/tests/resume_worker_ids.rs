//! A resumed session never hands out a worker id whose journal already exists
//! (issue #98).
//!
//! Workers of the earlier process are declared gone and their ids are reserved, so a
//! new worker cannot answer to an old name. The ids come from the delegation tool's
//! own results — but a WORKFLOW step is a worker too, and it leaves only its
//! `FILE.w<N>.jsonl` beside the session, with no record naming it. Reserving solely
//! from the journal therefore offered an id whose file was already there, and every
//! new worker (and every workflow child) failed with "journal file already exists"
//! after zero provider turns.
#![cfg(feature = "delegation")]

mod common;
#[cfg(feature = "workflows")]
mod workflow_common;

#[cfg(feature = "workflows")]
use workflow_common::{Fakes, Scratch, done};

use std::path::{Path, PathBuf};

use common::{Harness, provider_hook, run_args, write_environment};
use p1_contracts::Item;
use p1_testkit::{ScriptedProvider, json_call, text_response, tool_call_response};
use tempfile::tempdir;

const START: &str = r#"{"environment":"b","task":"work","tools":["read"]}"#;

#[cfg(feature = "workflows")]
#[tokio::test]
async fn repeated_standalone_workflows_reserve_step_worker_ids() {
    let scratch = Scratch::new();
    let script = scratch.script("one.rhai", r#"agent("one step")"#);
    let session = scratch.session();
    let args = [
        "workflow",
        "run",
        script.to_str().unwrap(),
        "--workspace",
        scratch.workspace.path().to_str().unwrap(),
        "--session",
        session.to_str().unwrap(),
        "--yes",
    ];
    let mut first = scratch.harness();
    first.deps.catalog_hook = Some(Fakes::new(Vec::new(), done("first"), Vec::new()).hook());
    assert_eq!(
        run_args(&mut first, &args).await,
        0,
        "{}",
        first.stderr.text()
    );
    assert!(p1_host::session::worker_path(&session, 1).exists());

    let mut second = scratch.harness();
    second.deps.catalog_hook = Some(Fakes::new(Vec::new(), done("second"), Vec::new()).hook());
    assert_eq!(
        run_args(&mut second, &args).await,
        0,
        "{}",
        second.stderr.text()
    );
    assert!(p1_host::session::worker_path(&session, 2).exists());
    assert!(!second.stderr.text().contains("journal file already exists"));
}

fn start_call(call_id: &str) -> p1_contracts::ToolCall {
    json_call(call_id, "worker_start", START)
}

/// Environment `a` may start workers on environment `b`, which has no tools.
fn declared_environments(root: &Path) {
    write_environment(root, "a", "fake-a", "a", &["worker_start"], "parent");
    write_environment(root, "b", "fake-b", "b", &[], "child");
}

/// The worker journal the host creates for `<id>` next to the session.
fn worker_journal(session: &Path, id: usize) -> PathBuf {
    p1_host::session::worker_path(session, id)
}

/// Run one host invocation with a scripted parent and child and the given
/// environments.
async fn run(
    environments: &Path,
    parent: ScriptedProvider,
    child: ScriptedProvider,
    args: &[&str],
) -> (i32, Harness) {
    let mut harness = Harness::new(vec![environments.to_path_buf()], &[]);
    harness.deps.catalog_hook = Some(provider_hook(vec![("fake-a", parent), ("fake-b", child)]));
    let code = run_args(&mut harness, args).await;
    (code, harness)
}

#[tokio::test]
async fn a_resumed_session_reserves_the_ids_of_worker_journals_a_workflow_left() {
    let workspace = tempdir().unwrap();
    let environments = tempdir().unwrap();
    declared_environments(environments.path());
    let session = workspace.path().join("session.jsonl");
    let session_arg = session.to_str().unwrap();
    let workspace_arg = workspace.path().to_str().unwrap();

    // One delegate-tool worker gives the first process a real journal. Completion
    // notifications and provider scheduling are deliberately not used to serialize
    // more workers: the remaining journals below model workflow children, whose ids
    // must be discovered from the filesystem.
    let (code, harness) = run(
        environments.path(),
        ScriptedProvider::new(vec![
            tool_call_response(vec![start_call("c1")]),
            text_response("done"),
            text_response("spare"),
            text_response("spare"),
        ]),
        ScriptedProvider::new(vec![text_response("child done")]),
        &[
            "--yes",
            "--env",
            "a",
            "--workspace",
            workspace_arg,
            "--session",
            session_arg,
            "go",
        ],
    )
    .await;
    assert_eq!(code, 0, "stderr: {}", harness.stderr.text());
    assert!(
        worker_journal(&session, 1).exists(),
        "w1 must be journalled: {}",
        harness.stderr.text()
    );

    // Four more worker journals, exactly the files a workflow's step workers leave:
    // on disk beside the session, and in no record the delegation tool wrote. w10 is
    // the highest id this session has used.
    let header = "{\"p1_journal\":1}\n";
    for id in 7..=10 {
        std::fs::write(worker_journal(&session, id), header).unwrap();
    }

    // A new process: a new worker service that knows nothing of w1…w10.
    let parent = ScriptedProvider::new(vec![
        tool_call_response(vec![start_call("c7")]),
        text_response("started"),
        text_response("waiting"),
        text_response("spare"),
    ]);
    let (code, harness) = run(
        environments.path(),
        parent.clone(),
        ScriptedProvider::new(vec![text_response("done again")]),
        &[
            "--yes",
            "--env",
            "a",
            "--workspace",
            workspace_arg,
            "--session",
            session_arg,
            "--resume",
            "again",
        ],
    )
    .await;
    assert_eq!(code, 0, "stderr: {}", harness.stderr.text());

    // The delegate-tool message is unchanged: it names only what the journal records.
    assert!(
        harness
            .stderr
            .text()
            .contains("resume: worker(s) w1 belonged to the earlier process and are not restored"),
        "stderr: {}",
        harness.stderr.text()
    );
    assert!(
        !harness
            .stdout
            .text()
            .contains("journal file already exists"),
        "the new worker must not be given an id whose journal is on disk: {}",
        harness.stdout.text()
    );
    assert!(
        workspace.path().join("session.jsonl.w11.jsonl").exists(),
        "the worker past every existing journal gets w11; dir: {:?}",
        std::fs::read_dir(workspace.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect::<Vec<_>>()
    );
    for id in 7..=10 {
        assert_eq!(
            std::fs::read_to_string(worker_journal(&session, id)).unwrap(),
            header,
            "the journal already there is never written to"
        );
    }
    let started = parent.requests()[1]
        .history
        .iter()
        .find_map(|item| match item {
            Item::ToolResult(result) => Some(result.content.clone()),
            _ => None,
        });
    assert!(
        started
            .as_deref()
            .is_some_and(|content| content.starts_with("Started worker w11 ")),
        "the model is told which worker it got: {started:?}"
    );
}

#[tokio::test]
async fn a_session_whose_worker_journals_only_a_workflow_left_still_reserves_their_ids() {
    let workspace = tempdir().unwrap();
    let environments = tempdir().unwrap();
    declared_environments(environments.path());
    let session = workspace.path().join("session.jsonl");
    let session_arg = session.to_str().unwrap();
    let workspace_arg = workspace.path().to_str().unwrap();

    // No worker under this session: the journal names no worker id at all.
    let (code, harness) = run(
        environments.path(),
        ScriptedProvider::new(vec![text_response("one")]),
        ScriptedProvider::new(Vec::new()),
        &[
            "--yes",
            "--env",
            "a",
            "--workspace",
            workspace_arg,
            "--session",
            session_arg,
            "go",
        ],
    )
    .await;
    assert_eq!(code, 0, "stderr: {}", harness.stderr.text());

    // Three step workers of a workflow: their journals are the only trace of them.
    let header = "{\"p1_journal\":1}\n";
    for id in 1..=3 {
        std::fs::write(worker_journal(&session, id), header).unwrap();
    }

    let parent = ScriptedProvider::new(vec![
        tool_call_response(vec![start_call("c1")]),
        text_response("started"),
        text_response("waiting"),
        text_response("spare"),
    ]);
    let (code, harness) = run(
        environments.path(),
        parent.clone(),
        ScriptedProvider::new(vec![text_response("done")]),
        &[
            "--yes",
            "--env",
            "a",
            "--workspace",
            workspace_arg,
            "--session",
            session_arg,
            "--resume",
            "again",
        ],
    )
    .await;
    assert_eq!(code, 0, "stderr: {}", harness.stderr.text());

    assert!(
        !harness
            .stdout
            .text()
            .contains("journal file already exists"),
        "the reservation must not be skipped when the journal names no worker: {}",
        harness.stdout.text()
    );
    assert!(
        workspace.path().join("session.jsonl.w4.jsonl").exists(),
        "the worker past the workflow's journals gets w4"
    );
    // Nothing was journalled as a lost worker, so nothing is announced as one.
    assert!(
        !harness
            .stderr
            .text()
            .contains("belonged to the earlier process"),
        "stderr: {}",
        harness.stderr.text()
    );
    let started = parent.requests()[1]
        .history
        .iter()
        .find_map(|item| match item {
            Item::ToolResult(result) => Some(result.content.clone()),
            _ => None,
        });
    assert!(
        started
            .as_deref()
            .is_some_and(|content| content.starts_with("Started worker w4 ")),
        "{started:?}"
    );
}

#[tokio::test]
async fn a_resumed_session_without_worker_journals_hands_out_w1() {
    let workspace = tempdir().unwrap();
    let environments = tempdir().unwrap();
    declared_environments(environments.path());
    let session = workspace.path().join("session.jsonl");
    let session_arg = session.to_str().unwrap();
    let workspace_arg = workspace.path().to_str().unwrap();

    let (code, harness) = run(
        environments.path(),
        ScriptedProvider::new(vec![text_response("one")]),
        ScriptedProvider::new(Vec::new()),
        &[
            "--yes",
            "--env",
            "a",
            "--workspace",
            workspace_arg,
            "--session",
            session_arg,
            "go",
        ],
    )
    .await;
    assert_eq!(code, 0, "stderr: {}", harness.stderr.text());

    // Names that are NOT worker journals: another session's, a worker with a suffix,
    // a bare `w`, a negative "id". None of them may move the first id.
    for name in [
        "session.jsonl.wX.jsonl",
        "session.jsonl.w.jsonl",
        "session.jsonl.w1x.jsonl",
        "session.jsonl.w-1.jsonl",
        "other.jsonl.w9.jsonl",
    ] {
        std::fs::write(workspace.path().join(name), "not a worker journal\n").unwrap();
    }

    let parent = ScriptedProvider::new(vec![
        tool_call_response(vec![start_call("c1")]),
        text_response("started"),
        text_response("waiting"),
        text_response("spare"),
    ]);
    let (code, harness) = run(
        environments.path(),
        parent.clone(),
        ScriptedProvider::new(vec![text_response("done")]),
        &[
            "--yes",
            "--env",
            "a",
            "--workspace",
            workspace_arg,
            "--session",
            session_arg,
            "--resume",
            "again",
        ],
    )
    .await;
    assert_eq!(code, 0, "stderr: {}", harness.stderr.text());

    assert!(
        workspace.path().join("session.jsonl.w1.jsonl").exists(),
        "no worker journal means the first id is still w1"
    );
    let started = parent.requests()[1]
        .history
        .iter()
        .find_map(|item| match item {
            Item::ToolResult(result) => Some(result.content.clone()),
            _ => None,
        });
    assert!(
        started
            .as_deref()
            .is_some_and(|content| content.starts_with("Started worker w1 ")),
        "{started:?}"
    );
}
