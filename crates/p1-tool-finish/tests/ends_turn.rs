//! ADR-0120: the native `finish` tool answers `ends_turn` from its own outcome cell —
//! `true` exactly for an accepted call, `false` for a rejected one. No host, no files.

use std::sync::Arc;

use p1_contracts::{
    CancellationToken, Tool, ToolCall, ToolContext, ToolInput, ToolOutcome, ToolStatus,
};
use p1_tool_finish::{FinishOutcome, FinishTool, SessionActivity, ShellRun};

/// A session with no recorded activity: an accepted `done` with `["none"]` needs none.
struct NoActivity;

impl SessionActivity for NoActivity {
    fn last_file_change(&self) -> Option<u64> {
        None
    }

    fn shell_runs(&self) -> Vec<ShellRun> {
        Vec::new()
    }
}

fn call(raw: &str) -> ToolCall {
    ToolCall {
        call_id: "c1".into(),
        name: "finish".into(),
        input: ToolInput::Json(raw.into()),
    }
}

async fn execute(tool: &FinishTool, raw: &str) -> ToolOutcome {
    tool.execute(
        &call(raw),
        ToolContext {
            cancel: CancellationToken::new(),
        },
    )
    .await
}

#[tokio::test]
async fn an_accepted_done_ends_the_turn() {
    let tool = FinishTool::new(Arc::new(NoActivity), FinishOutcome::default());
    let outcome = execute(
        &tool,
        r#"{"status":"done","summary":"answered","verification":["none"]}"#,
    )
    .await;
    assert_eq!(outcome.status, ToolStatus::Ok);
    assert!(tool.ends_turn(&outcome), "an accepted call ends the turn");
}

#[tokio::test]
async fn an_accepted_blocked_ends_the_turn() {
    let tool = FinishTool::new(Arc::new(NoActivity), FinishOutcome::default());
    let outcome = execute(
        &tool,
        r#"{"status":"blocked","summary":"cannot write","needs":"edit tool"}"#,
    )
    .await;
    assert_eq!(outcome.status, ToolStatus::Ok);
    assert!(
        tool.ends_turn(&outcome),
        "an accepted blocked is still an accepted call"
    );
}

#[tokio::test]
async fn a_rejected_call_keeps_the_turn_going() {
    // No `verification`: the call is rejected and stores nothing in the outcome cell.
    let tool = FinishTool::new(Arc::new(NoActivity), FinishOutcome::default());
    let outcome = execute(&tool, r#"{"status":"done","summary":"s"}"#).await;
    assert_eq!(outcome.status, ToolStatus::Error);
    assert!(
        !tool.ends_turn(&outcome),
        "a rejected call must not end the turn"
    );
}

#[tokio::test]
async fn an_accepted_call_then_a_rejected_one_in_a_later_turn_does_not_end_it() {
    // The bug ADR-0120 point 4 names: `outcome.get().is_some()` stays true once an
    // earlier call was accepted, so a LATER rejected call would wrongly end the turn.
    // `ends_turn` must answer for the call just executed, not the cell's last accept.
    let tool = FinishTool::new(Arc::new(NoActivity), FinishOutcome::default());
    let accepted = execute(
        &tool,
        r#"{"status":"done","summary":"answered","verification":["none"]}"#,
    )
    .await;
    assert!(tool.ends_turn(&accepted), "the accepted call ends its turn");

    // The second turn's rejected call (no `verification`) must not inherit that.
    let rejected = execute(&tool, r#"{"status":"done","summary":"s"}"#).await;
    assert_eq!(rejected.status, ToolStatus::Error);
    assert!(
        !tool.ends_turn(&rejected),
        "a later rejected call must not end the turn"
    );
}
