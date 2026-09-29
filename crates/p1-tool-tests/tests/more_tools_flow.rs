//! Lead acceptance: grep, shell and apply_patch with real-shaped awkward inputs
//! (docs/design/tools.md).

use std::fs;
use std::future::Future;
use std::os::unix::fs::symlink;
use std::path::Path;
use std::task::{Context, Poll, Waker};
use std::time::{Duration, Instant};

use p1_contracts::{
    CancellationToken, Tool, ToolCall, ToolContext, ToolInput, ToolOutcome, ToolStatus,
};
use p1_tool_patch::PatchTool;
use p1_tool_search::GrepTool;
use p1_tool_shell::ShellTool;
use p1_workspace::{ObservedFiles, Workspace};

fn ws(root: &Path) -> Workspace {
    Workspace::new(root).unwrap()
}

async fn run(tool: &dyn Tool, input: ToolInput, cancel: CancellationToken) -> ToolOutcome {
    let call = ToolCall {
        call_id: "c".into(),
        name: tool.declaration().name.clone(),
        input,
    };
    tool.execute(&call, ToolContext { cancel }).await
}

async fn patch(root: &Path, text: &str) -> ToolOutcome {
    let tool = PatchTool::new(ws(root), ObservedFiles::new());
    run(
        &tool,
        ToolInput::Text(text.into()),
        CancellationToken::new(),
    )
    .await
}

const MAIN_RS: &str = "\
use std::io;

fn helper(x: i32) -> i32 {
    x + 1
}

fn main() {
    let value = helper(1);
    println!(\"{value}\");
}

fn helper_two(x: i32) -> i32 {
    x + 1
}
";

// A patch the way GPT models actually write them: @@ headers naming the function,
// the same context (`x + 1`) occurring twice, two hunks in one file, plus an added file.
#[tokio::test]
async fn a_model_shaped_multi_hunk_patch_applies_at_the_right_places() {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir(dir.path().join("src")).unwrap();
    fs::write(dir.path().join("src/main.rs"), MAIN_RS).unwrap();

    let out = patch(
        dir.path(),
        "*** Begin Patch
*** Update File: src/main.rs
@@ fn main() {
-    let value = helper(1);
+    let value = helper_two(41);
     println!(\"{value}\");
@@ fn helper_two(x: i32) -> i32 {
-    x + 1
+    x + 2
 }
*** Add File: src/notes.md
+# Notes
+second helper now adds two
*** End Patch
",
    )
    .await;

    assert_eq!(out.status, ToolStatus::Ok, "{}", out.content);
    assert_eq!(out.content, "M src/main.rs\nA src/notes.md");
    let after = fs::read_to_string(dir.path().join("src/main.rs")).unwrap();
    assert_eq!(
        after,
        MAIN_RS.replace("helper(1);", "helper_two(41);").replacen(
            "fn helper_two(x: i32) -> i32 {\n    x + 1",
            "fn helper_two(x: i32) -> i32 {\n    x + 2",
            1
        )
    );
    assert!(
        after.contains("fn helper(x: i32) -> i32 {\n    x + 1\n}"),
        "first helper must be untouched"
    );
    assert_eq!(
        fs::read_to_string(dir.path().join("src/notes.md")).unwrap(),
        "# Notes\nsecond helper now adds two\n"
    );
}

// Codex semantics (what GPT models are trained on): an update hunk consisting only of
// added lines, with no context and no `*** End of File`, APPENDS to the file.
#[tokio::test]
async fn an_addition_only_hunk_appends_to_the_end_like_codex() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("list.txt"), "one\ntwo\n").unwrap();
    let out = patch(
        dir.path(),
        "*** Begin Patch\n*** Update File: list.txt\n@@\n+three\n*** End Patch\n",
    )
    .await;
    assert_eq!(out.status, ToolStatus::Ok, "{}", out.content);
    assert_eq!(
        fs::read_to_string(dir.path().join("list.txt")).unwrap(),
        "one\ntwo\nthree\n"
    );
}

#[tokio::test]
async fn a_patch_that_fails_halfway_writes_nothing_at_all() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("a.txt"), "alpha\n").unwrap();
    fs::write(dir.path().join("b.txt"), "beta\n").unwrap();
    let out = patch(
        dir.path(),
        "*** Begin Patch
*** Update File: a.txt
-alpha
+ALPHA
*** Add File: new.txt
+created
*** Delete File: b.txt
*** Update File: a.txt
-this line does not exist
+x
*** End Patch
",
    )
    .await;
    assert_eq!(out.status, ToolStatus::Error, "{}", out.content);
    assert_eq!(
        fs::read_to_string(dir.path().join("a.txt")).unwrap(),
        "alpha\n"
    );
    assert_eq!(
        fs::read_to_string(dir.path().join("b.txt")).unwrap(),
        "beta\n"
    );
    assert!(!dir.path().join("new.txt").exists());
}

#[tokio::test]
async fn patches_cannot_leave_the_workspace() {
    let outside = tempfile::tempdir().unwrap();
    fs::write(outside.path().join("victim.txt"), "safe\n").unwrap();
    let dir = tempfile::tempdir().unwrap();
    symlink(outside.path(), dir.path().join("out")).unwrap();
    let abs = outside.path().join("victim.txt");

    for body in [
        "*** Add File: ../escape.txt\n+x\n".to_string(),
        "*** Add File: out/escape.txt\n+x\n".to_string(),
        "*** Delete File: out/victim.txt\n".to_string(),
        "*** Update File: out/victim.txt\n-safe\n+pwned\n".to_string(),
        format!("*** Update File: {}\n-safe\n+pwned\n", abs.display()),
        "*** Update File: a.txt\n*** Move to: out/moved.txt\n-alpha\n+beta\n".to_string(),
    ] {
        fs::write(dir.path().join("a.txt"), "alpha\n").unwrap();
        let out = patch(
            dir.path(),
            &format!("*** Begin Patch\n{body}*** End Patch\n"),
        )
        .await;
        assert_eq!(out.status, ToolStatus::Error, "{body} -> {}", out.content);
    }
    assert_eq!(fs::read_to_string(&abs).unwrap(), "safe\n");
    assert_eq!(fs::read_dir(outside.path()).unwrap().count(), 1);
}

#[tokio::test]
async fn a_heredoc_wrapped_crlf_patch_without_final_newline_still_applies() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("w.txt"), "a\r\nb\r\n").unwrap();
    let out = patch(
        dir.path(),
        "<<'EOF'\r\n*** Begin Patch\r\n*** Update File: w.txt\r\n a\r\n-b\r\n+B\r\n*** End Patch\r\nEOF",
    )
    .await;
    assert_eq!(out.status, ToolStatus::Ok, "{}", out.content);
    assert_eq!(fs::read(dir.path().join("w.txt")).unwrap(), b"a\r\nB\r\n");
}

/// The complete, newline-terminated PID records in a readiness file. `lines()` also counts an
/// unterminated final line, so a partially written PID would otherwise look like a ready child;
/// only a record the writer finished with a newline, and that parses to a positive PID, names
/// one.
fn complete_child_pids(text: &str) -> Vec<u32> {
    text.split_inclusive('\n')
        .filter_map(|record| record.strip_suffix('\n'))
        .filter_map(|record| record.trim().parse::<u32>().ok())
        .filter(|pid| *pid > 0)
        .collect()
}

async fn await_child_pids(pids: &std::path::Path) {
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            if fs::read_to_string(pids)
                .map(|s| complete_child_pids(&s).len() >= 3)
                .unwrap_or(false)
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("shell children did not report readiness before cancellation");
}

#[test]
fn child_readiness_needs_three_complete_positive_pid_records() {
    // A partial third write is not a child, though `lines()` would count it.
    assert_eq!(complete_child_pids("100\n200\n30"), vec![100, 200]);
    assert_eq!(complete_child_pids("100\n200\n300\n"), vec![100, 200, 300]);
    // A finished but unusable record names no child either.
    assert_eq!(
        complete_child_pids("100\n200\n\n300\n"),
        vec![100, 200, 300]
    );
    assert_eq!(complete_child_pids("100\n0\n-2\n300\n"), vec![100, 300]);
}

#[tokio::test]
async fn cancellation_waits_until_all_child_pids_exist() {
    let dir = tempfile::tempdir().unwrap();
    let pids = dir.path().join("pids");
    // Two complete records and a torn third: `lines()` counts the torn one, so an early
    // readiness check reads three children here.
    fs::write(&pids, "1\n2\n3").unwrap();
    let mut waiter = Box::pin(await_child_pids(&pids));
    // Polling the waiter once proves it read that file and stayed blocked. A spawned task was
    // never polled before a synchronous `is_finished()`, so a waiter that returned early still
    // passed that check.
    let mut context = Context::from_waker(Waker::noop());
    assert!(
        matches!(waiter.as_mut().poll(&mut context), Poll::Pending),
        "cancelled before the third PID was complete"
    );
    fs::write(&pids, "1\n2\n3\n").unwrap();
    waiter.await;
}

#[tokio::test]
async fn shell_kills_the_whole_process_tree_on_cancel_and_on_timeout() {
    let dir = tempfile::tempdir().unwrap();
    let tool = ShellTool::new(ws(dir.path()));

    // Cancel: grandchildren record their pids, then everything sleeps.
    let cancel = CancellationToken::new();
    let trigger = cancel.clone();
    let pids = dir.path().join("pids");
    let command = format!(
        r#"{{"command":"(sleep 600 & echo $! >> {p}; sleep 600 & echo $! >> {p}; wait) & echo $! >> {p}; wait"}}"#,
        p = pids.display()
    );
    let waiter = {
        let pids = pids.clone();
        tokio::spawn(async move {
            await_child_pids(&pids).await;
            trigger.cancel();
        })
    };
    let started = Instant::now();
    let out = run(&tool, ToolInput::Json(command), cancel).await;
    waiter.await.unwrap();
    assert_eq!(out.status, ToolStatus::Cancelled, "{}", out.content);
    assert!(started.elapsed() < Duration::from_secs(20));
    // Signalled grandchildren whose parent is gone are reaped by init a moment later:
    // "dead" means gone OR a zombie, observed within a short grace period.
    for pid in fs::read_to_string(&pids).unwrap().lines() {
        let mut dead = false;
        for _ in 0..80 {
            let state = fs::read_to_string(format!("/proc/{pid}/stat"))
                .ok()
                .and_then(|stat| {
                    stat.rsplit(')')
                        .next()
                        .map(|rest| rest.trim().chars().next())
                })
                .flatten();
            if matches!(state, None | Some('Z') | Some('X')) {
                dead = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        assert!(dead, "process {pid} survived cancellation");
    }

    // Timeout.
    let started = Instant::now();
    let out = run(
        &tool,
        ToolInput::Json(r#"{"command":"echo before; sleep 600","timeout_seconds":1}"#.into()),
        CancellationToken::new(),
    )
    .await;
    assert_eq!(out.status, ToolStatus::Error);
    assert_eq!(out.content, "before\n[timed out after 1 s]");
    assert!(started.elapsed() < Duration::from_secs(15));
}

#[tokio::test]
async fn shell_survives_binary_and_enormous_output() {
    let dir = tempfile::tempdir().unwrap();
    let tool = ShellTool::new(ws(dir.path()));
    let out = run(
        &tool,
        ToolInput::Json(
            r#"{"command":"head -c 3000000 /dev/urandom; echo; echo END; exit 7"}"#.into(),
        ),
        CancellationToken::new(),
    )
    .await;
    assert_eq!(out.status, ToolStatus::Ok);
    assert!(out.content.len() < 70_000, "{} bytes", out.content.len());
    assert!(
        out.content.ends_with("END\n[exit code: 7]"),
        "tail: {:?}",
        &out.content[out.content.len() - 40..]
    );
    assert!(out.content.contains("bytes omitted"));
}

#[tokio::test]
async fn grep_stays_inside_the_workspace_and_respects_ignores() {
    let outside = tempfile::tempdir().unwrap();
    fs::write(outside.path().join("o.txt"), "needle outside\n").unwrap();
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir(dir.path().join(".git")).unwrap();
    fs::write(dir.path().join(".gitignore"), "build/\n").unwrap();
    fs::create_dir(dir.path().join("build")).unwrap();
    fs::write(dir.path().join("build/gen.txt"), "needle generated\n").unwrap();
    fs::write(dir.path().join("src.txt"), "a needle here\nnothing\n").unwrap();
    symlink(outside.path(), dir.path().join("linked")).unwrap();
    let tool = GrepTool::new(ws(dir.path()));

    let out = run(
        &tool,
        ToolInput::Json(r#"{"pattern":"needle"}"#.into()),
        CancellationToken::new(),
    )
    .await;
    assert_eq!(out.status, ToolStatus::Ok);
    assert_eq!(out.content, "src.txt\n1:a needle here");

    for json in [
        r#"{"pattern":"needle","path":"linked"}"#,
        r#"{"pattern":"needle","path":".."}"#,
    ] {
        let out = run(
            &tool,
            ToolInput::Json(json.into()),
            CancellationToken::new(),
        )
        .await;
        assert_eq!(out.status, ToolStatus::Error, "{json} -> {}", out.content);
        assert!(!out.content.contains("outside"));
    }
}
