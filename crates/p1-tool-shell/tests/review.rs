use nix::{sys::signal::{kill, Signal}, unistd::Pid};
use p1_contracts::{Tool, ToolCall, ToolInput, ToolContext, CancellationToken};
use p1_tool_shell::ShellTool;
use p1_workspace::Workspace;

#[tokio::test]
async fn review_cancel_kills_term_ignoring_descendant() {
    let dir = tempfile::tempdir().unwrap();
    let tool = ShellTool::new(Workspace::new(dir.path()).unwrap());
    let cancel = CancellationToken::new();
    let task_cancel = cancel.clone();
    let task = tokio::spawn(async move {
        let call = ToolCall { call_id: "c".into(), name: "shell".into(), input: ToolInput::Json(serde_json::json!({"command": "bash -c 'trap \"\" TERM; echo $$ > survivor; exec sleep 300' & wait"}).to_string()) };
        tool.execute(&call, ToolContext { cancel: task_cancel }).await
    });
    let pid = tokio::time::timeout(std::time::Duration::from_secs(20), async {
        loop {
            if let Ok(s) = std::fs::read_to_string(dir.path().join("survivor")) {
                if let Ok(pid) = s.trim().parse::<i32>() { break Pid::from_raw(pid); }
            }
            tokio::task::yield_now().await;
        }
    }).await.unwrap();
    cancel.cancel();
    let outcome = task.await.unwrap();
    let stat = std::fs::read_to_string(format!("/proc/{}/stat", pid.as_raw())).unwrap_or_default();
    let alive = !stat.is_empty() && !stat.split_once(") ").unwrap().1.starts_with('Z');
    let _ = kill(pid, Signal::SIGKILL);
    assert!(!alive, "descendant still alive after tool returned {:?}", outcome.status);
}
