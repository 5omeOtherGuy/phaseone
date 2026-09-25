//! Lead checks for review finding R1: the tool returns only once its whole process
//! group is gone, whether the descendants cooperate with SIGTERM or ignore it, and
//! whether the run ends by cancellation or by timeout.

use std::path::Path;
use std::time::Duration;

use p1_contracts::{CancellationToken, Tool, ToolCall, ToolContext, ToolInput, ToolStatus};
use p1_tool_shell::ShellTool;
use p1_workspace::Workspace;

fn call(command: &str, timeout_seconds: Option<u64>) -> ToolCall {
    let mut input = serde_json::json!({ "command": command });
    if let Some(seconds) = timeout_seconds {
        input["timeout_seconds"] = seconds.into();
    }
    ToolCall {
        call_id: "c".into(),
        name: "shell".into(),
        input: ToolInput::Json(input.to_string()),
    }
}

fn pid_in(root: &Path, name: &str) -> Option<i32> {
    std::fs::read_to_string(root.join(name))
        .ok()?
        .trim()
        .parse()
        .ok()
}

/// Running or sleeping — anything but gone, zombie or dead.
fn alive(pid: i32) -> bool {
    let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
        return false;
    };
    let state = stat.rsplit_once(") ").map(|(_, rest)| rest).unwrap_or("");
    !(state.starts_with('Z') || state.starts_with('X'))
}

async fn wait_for_pid(root: &Path, name: &str) -> i32 {
    loop {
        if let Some(pid) = pid_in(root, name) {
            return pid;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

#[tokio::test]
async fn cooperative_descendants_end_without_waiting_out_the_grace_period() {
    let dir = tempfile::tempdir().unwrap();
    let tool = ShellTool::new(Workspace::new(dir.path()).unwrap());
    let cancel = CancellationToken::new();
    // The leader records how its descendant ended: on the group's SIGTERM its trap waits
    // for the descendant and writes that exit status, then the leader exits too. A SIGKILL
    // anywhere in the group (the grace run out) leaves no status or a 137.
    let call = call(
        "trap 'wait \"$child\"; echo $? > status; exit 143' TERM; \
         sleep 300 & child=$!; echo $child > pid; wait",
        None,
    );
    let run = tool.execute(
        &call,
        ToolContext {
            cancel: cancel.clone(),
        },
    );
    let stopper = async {
        let pid = wait_for_pid(dir.path(), "pid").await;
        cancel.cancel();
        pid
    };
    let (outcome, pid) = tokio::join!(run, stopper);

    assert_eq!(outcome.status, ToolStatus::Cancelled);
    assert!(!alive(pid), "descendant {pid} survived");
    // SIGTERM is enough here; the group must not be left for the SIGKILL after the 2 s
    // grace period. 143 = 128 + SIGTERM: the descendant died of the TERM, and the leader
    // lived to report it, so nothing in the group was killed.
    let status = std::fs::read_to_string(dir.path().join("status"))
        .unwrap_or_else(|error| panic!("the leader did not survive to report: {error}"));
    assert_eq!(
        status.trim(),
        "143",
        "the descendant must end by SIGTERM (143), not SIGKILL (137)"
    );
}

#[tokio::test]
async fn a_timeout_kills_a_term_ignoring_descendant_too() {
    let dir = tempfile::tempdir().unwrap();
    let tool = ShellTool::new(Workspace::new(dir.path()).unwrap());
    // The outer shell dies on SIGTERM at once; the inner one ignores it.
    let call = call(
        "bash -c 'trap \"\" TERM; echo $$ > survivor; exec sleep 300' & wait",
        Some(2),
    );
    let outcome = tool
        .execute(
            &call,
            ToolContext {
                cancel: CancellationToken::new(),
            },
        )
        .await;

    assert_eq!(outcome.status, ToolStatus::Error);
    assert!(
        outcome.content.contains("[timed out after 2 s]"),
        "{outcome:?}"
    );
    // A start-up slower than the timeout leaves no survivor to check — then the
    // whole group was killed before the inner shell existed, which is also correct.
    if let Some(pid) = pid_in(dir.path(), "survivor") {
        let still_alive = alive(pid);
        if still_alive {
            let _ = nix::sys::signal::kill(
                nix::unistd::Pid::from_raw(pid),
                nix::sys::signal::Signal::SIGKILL,
            );
        }
        assert!(!still_alive, "descendant {pid} survived the timeout");
    }
}
