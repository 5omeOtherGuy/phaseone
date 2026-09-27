//! Lead acceptance for issue #1 (review 2026-09-20, departure D-A): agents that share
//! a working directory — a parent and its workers — never lose each other's updates
//! through the file tools. Each agent has its OWN observation registry; what they
//! share is the write gate, as `p1-assembly` wires it.

use std::fs;
use std::future::Future;
use std::path::Path;
use std::sync::Arc;
use std::task::{Context, Poll, Waker};

use p1_contracts::serde_json;
use p1_contracts::{
    CancellationToken, Tool, ToolCall, ToolContext, ToolInput, ToolOutcome, ToolStatus,
};
use p1_tool_edit::EditTool;
use p1_tool_patch::PatchTool;
use p1_tool_read::ReadTool;
use p1_tool_write::WriteTool;
use p1_workspace::{
    MutationError, MutationPolicy, Observation, ObservedFiles, Workspace, WriteGate,
};

/// One agent's file tools: its own observations, the shared gate.
struct AgentTools {
    read: ReadTool,
    edit: EditTool,
    write: WriteTool,
    patch: PatchTool,
}

fn agent(root: &Path, gate: &WriteGate) -> Arc<AgentTools> {
    let workspace = Workspace::new(root).unwrap().with_write_gate(gate.clone());
    let observed = ObservedFiles::new();
    Arc::new(AgentTools {
        read: ReadTool::new(workspace.clone(), observed.clone()),
        edit: EditTool::new(workspace.clone(), observed.clone()),
        write: WriteTool::new(workspace.clone(), observed.clone()),
        patch: PatchTool::new(workspace, observed),
    })
}

async fn run(tool: &dyn Tool, input: ToolInput) -> ToolOutcome {
    let call = ToolCall {
        call_id: "c".into(),
        name: tool.declaration().name.clone(),
        input,
    };
    tool.execute(
        &call,
        ToolContext {
            cancel: CancellationToken::new(),
        },
    )
    .await
}

async fn json(tool: &dyn Tool, value: serde_json::Value) -> ToolOutcome {
    run(tool, ToolInput::Json(value.to_string())).await
}

#[tokio::test]
async fn a_write_landing_before_a_queued_edit_is_seen_not_overwritten() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("notes.txt");
    fs::write(&file, "one\n").unwrap();
    let gate = WriteGate::new();
    let worker = agent(dir.path(), &gate);
    let read = json(&worker.read, serde_json::json!({"file_path": "notes.txt"})).await;
    assert_eq!(read.status, ToolStatus::Ok, "{read:?}");

    // Another agent is in the middle of a mutation…
    let other_agents_mutation = gate.begin_mutation();
    let mut edit = Box::pin(json(
        &worker.edit,
        serde_json::json!({"file_path": "notes.txt", "old_string": "one", "new_string": "ONE"}),
    ));
    // …the worker's edit is started and has to queue behind it…
    let mut context = Context::from_waker(Waker::noop());
    assert!(matches!(edit.as_mut().poll(&mut context), Poll::Pending));
    // …and that mutation changes the very file.
    fs::write(&file, "one\ntwo (from the other agent)\n").unwrap();
    drop(other_agents_mutation);

    let outcome = edit.await;
    assert_eq!(outcome.status, ToolStatus::Error, "{outcome:?}");
    assert_eq!(
        outcome.content,
        "notes.txt changed on disk since you last read it; read it again."
    );
    assert_eq!(
        fs::read_to_string(&file).unwrap(),
        "one\ntwo (from the other agent)\n"
    );
}

/// Add `line` above the `# end` marker with `edit`, re-reading whenever another
/// agent got there first — what a model does on the stale-file error.
async fn add_line_with_edit(tools: &AgentTools, line: &str) -> usize {
    let mut rereads = 0;
    loop {
        let read = json(&tools.read, serde_json::json!({"file_path": "log.txt"})).await;
        assert_eq!(read.status, ToolStatus::Ok, "{read:?}");
        let outcome = json(
            &tools.edit,
            serde_json::json!({
                "file_path": "log.txt", "old_string": "# end", "new_string": format!("{line}\n# end"),
            }),
        )
        .await;
        match outcome.status {
            ToolStatus::Ok => return rereads,
            _ => {
                assert!(
                    outcome.content.contains("changed on disk"),
                    "unexpected failure: {outcome:?}"
                );
                rereads += 1;
            }
        }
    }
}

/// The same with `apply_patch`: its context lines are its staleness check.
async fn add_line_with_patch(tools: &AgentTools, line: &str) {
    let patch = format!(
        "*** Begin Patch\n*** Update File: log.txt\n@@\n-# end\n+{line}\n+# end\n*** End Patch\n"
    );
    let outcome = run(&tools.patch, ToolInput::Text(patch)).await;
    assert_eq!(outcome.status, ToolStatus::Ok, "{outcome:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_agents_never_lose_an_update() {
    const ROUNDS: usize = 150;
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("log.txt"), "# start\n# end\n").unwrap();
    let gate = WriteGate::new();
    let parent = agent(dir.path(), &gate);
    let first_worker = agent(dir.path(), &gate);
    let second_worker = agent(dir.path(), &gate);

    let a = tokio::spawn(async move {
        for round in 0..ROUNDS {
            add_line_with_edit(&parent, &format!("parent {round}")).await;
        }
    });
    let b = tokio::spawn(async move {
        for round in 0..ROUNDS {
            add_line_with_edit(&first_worker, &format!("worker-1 {round}")).await;
        }
    });
    let c = tokio::spawn(async move {
        for round in 0..ROUNDS {
            add_line_with_patch(&second_worker, &format!("worker-2 {round}")).await;
        }
    });
    for writer in [a, b, c] {
        writer.await.unwrap();
    }

    let text = fs::read_to_string(dir.path().join("log.txt")).unwrap();
    for writer in ["parent", "worker-1", "worker-2"] {
        for round in 0..ROUNDS {
            let line = format!("{writer} {round}");
            assert_eq!(
                text.lines().filter(|found| *found == line).count(),
                1,
                "`{line}` was lost or duplicated"
            );
        }
    }
    assert_eq!(text.lines().count(), 3 * ROUNDS + 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_agents_creating_the_same_file_do_not_overwrite_each_other() {
    for _ in 0..50 {
        let dir = tempfile::tempdir().unwrap();
        let gate = WriteGate::new();
        let first = agent(dir.path(), &gate);
        let second = agent(dir.path(), &gate);

        let a = tokio::spawn(async move {
            json(
                &first.write,
                serde_json::json!({"file_path": "new.txt", "content": "from the first\n"}),
            )
            .await
        });
        let b = tokio::spawn(async move {
            json(
                &second.write,
                serde_json::json!({"file_path": "new.txt", "content": "from the second\n"}),
            )
            .await
        });
        let (a, b) = (a.await.unwrap(), b.await.unwrap());

        let winners = [&a, &b]
            .iter()
            .filter(|outcome| outcome.status == ToolStatus::Ok)
            .count();
        assert_eq!(winners, 1, "{a:?} / {b:?}");
        let loser = if a.status == ToolStatus::Ok { &b } else { &a };
        assert_eq!(loser.content, "You must read new.txt before changing it.");
        let text = fs::read_to_string(dir.path().join("new.txt")).unwrap();
        let expected = if a.status == ToolStatus::Ok {
            "from the first\n"
        } else {
            "from the second\n"
        };
        assert_eq!(text, expected);
    }
}

/// A module's `workspace-mutation.begin` goes through the owned, async acquisition of the
/// same gate the native tools wait on, and a parent and its worker still keep separate
/// observations: what one writes is its own observation, never the other's.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_parent_and_a_worker_share_the_gate_through_the_owned_api_and_keep_separate_observations()
{
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("notes.txt");
    fs::write(&file, "one\n").unwrap();
    let gate = WriteGate::new();
    let parent = Workspace::new(dir.path())
        .unwrap()
        .with_write_gate(gate.clone());
    let worker = Workspace::new(dir.path())
        .unwrap()
        .with_write_gate(gate.clone());
    let (parent_observed, worker_observed) = (ObservedFiles::new(), ObservedFiles::new());
    let worker_edit = EditTool::new(worker.clone(), worker_observed.clone());
    parent.read("notes.txt", &parent_observed).unwrap();
    worker.read("notes.txt", &worker_observed).unwrap();

    // The parent's module holds the gate…
    let parent_mutation = parent
        .begin_owned(&parent_observed, MutationPolicy::Observed)
        .await;
    // …so the worker's module queues behind it, and so does the worker's native edit.
    let mut worker_begin = Box::pin(worker.begin_owned(&worker_observed, MutationPolicy::Observed));
    let mut context = Context::from_waker(Waker::noop());
    assert!(matches!(
        worker_begin.as_mut().poll(&mut context),
        Poll::Pending
    ));
    let mut edit = Box::pin(json(
        &worker_edit,
        serde_json::json!({"file_path": "notes.txt", "old_string": "one", "new_string": "ONE"}),
    ));
    assert!(matches!(edit.as_mut().poll(&mut context), Poll::Pending));

    parent_mutation
        .write("notes.txt", "one\ntwo (from the parent)\n")
        .unwrap();
    let written = b"one\ntwo (from the parent)\n";
    assert_eq!(
        parent_observed.check_unchanged(&file, written),
        Observation::Unchanged
    );
    assert_eq!(
        worker_observed.check_unchanged(&file, written),
        Observation::ChangedSinceObserved
    );
    drop(parent_mutation);

    let outcome = edit.await;
    assert_eq!(outcome.status, ToolStatus::Error, "{outcome:?}");
    assert_eq!(
        outcome.content,
        "notes.txt changed on disk since you last read it; read it again."
    );
    let worker_mutation = worker_begin.await;
    assert_eq!(
        worker_mutation.write("notes.txt", "lost update\n"),
        Err(MutationError::Io(
            "notes.txt changed on disk since you last read it; read it again.".into()
        ))
    );
    worker_mutation
        .create("from-worker.txt", "the worker's\n")
        .unwrap();
    drop(worker_mutation);

    // The worker's file is the worker's observation; the parent must read it first.
    let parent_mutation = parent
        .begin_owned(&parent_observed, MutationPolicy::Observed)
        .await;
    assert_eq!(
        parent_mutation.write("from-worker.txt", "overwritten\n"),
        Err(MutationError::Io(
            "You must read from-worker.txt before changing it.".into()
        ))
    );
    drop(parent_mutation);
    assert_eq!(
        fs::read_to_string(&file).unwrap(),
        "one\ntwo (from the parent)\n"
    );
    assert_eq!(
        fs::read_to_string(dir.path().join("from-worker.txt")).unwrap(),
        "the worker's\n"
    );
}
