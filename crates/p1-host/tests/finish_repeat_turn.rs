//! ADR-0120 point 2, interactive: an interactive run never clears the
//! accepted-completion cell between turns (`run_interactive`; the TUI discards the
//! completion), so the host's `FinishTurnEnd` must detect acceptance PER CALL, never
//! by the cell's values. Two turns, each ending with the IDENTICAL accepted finish,
//! must each cost exactly one request. On c2c70a0 the second turn compared equal and
//! sent another request.
//!
//! Unlike the headless tests, nothing calls `FinishOutcome::clear`: the two turns run
//! on one agent in one interactive session, driven by two scripted lines.

mod common;

use std::path::Path;

use common::{Harness, provider_hook, run_args, write_environment};
use p1_testkit::{ScriptedProvider, Step, json_call, text_response, tool_call_response};
use tempfile::tempdir;

/// A `done` with `["none"]`: accepted, because the session changed nothing and the
/// environment has no tool that runs a command.
const DONE_NONE: &str = r#"{"status":"done","summary":"answered","verification":["none"]}"#;

fn finish_environment(root: &Path) {
    write_environment(
        root,
        "finish-env",
        "fake",
        "fake-model",
        &["finish"],
        "test",
    );
}

fn finish(id: &str) -> Step {
    tool_call_response(vec![json_call(id, "finish", DONE_NONE)])
}

#[tokio::test]
async fn two_identical_accepted_finishes_each_end_their_turn() {
    let workspace = tempdir().unwrap();
    let environments = tempdir().unwrap();
    finish_environment(environments.path());

    // Two interactive turns, each ending with the SAME accepted finish. The third step
    // is a spare the value-equality bug would consume on turn 2, so the count below
    // fails (3) instead of erroring on an exhausted script.
    let provider = ScriptedProvider::new(vec![finish("f1"), finish("f2"), text_response("spare")]);
    let handle = provider.clone();
    let mut harness = Harness::new(
        vec![environments.path().to_path_buf()],
        &["one", "two", "/exit"],
    );
    harness.deps.catalog_hook = Some(provider_hook(vec![("fake", provider)]));

    // No positional prompt: the line front end runs interactively, so the cell is
    // never cleared between the two turns.
    let code = run_args(
        &mut harness,
        &[
            "--env",
            "finish-env",
            "--workspace",
            workspace.path().to_str().unwrap(),
        ],
    )
    .await;

    assert_eq!(code, 0, "stderr: {}", harness.stderr.text());
    assert_eq!(
        handle.requests().len(),
        2,
        "one request per turn, even though the cell still holds turn 1's identical \
         accepted value: {}",
        harness.stderr.text()
    );
}
