//! The streaming form of the process service and the `process` capability built on it
//! (`ProcessCapability`, the runtime's `ProcessService` and `RunningProcess` traits),
//! over real processes.
//!
//! Every property is shown on the native stream and, where the traits add something,
//! through the capability. The sandbox test needs a working `bwrap` and prints
//! `SKIP: bwrap unusable here` when the probe fails; on this machine bwrap works, so
//! every test below runs. "Group gone" is proved with the pids a command publishes and
//! `kill(pid, 0)` answering ESRCH the moment the call returned; a command is only
//! killed once it published them.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use nix::errno::Errno;
use nix::sys::signal::kill;
use nix::unistd::Pid;
use p1_contracts::CancellationToken;
use p1_module_runtime::{
    ExitStatus, ProcessCommand, ProcessEvent, ProcessService as _, RunningProcess,
};
use p1_tool_shell::{
    ProcessCapability, ProcessEnd, ProcessFailure, ProcessRequest, ProcessService, Sandbox,
    StreamEvent,
};

fn process_path() -> OsString {
    std::env::var_os("PATH").unwrap_or_else(|| OsString::from("/usr/bin:/bin"))
}

/// `HOME` is the (empty) workspace, so `bash -l` finds no profile of the real home and
/// prints nothing of its own: the output is the command's alone.
fn service(root: &Path) -> ProcessService {
    ProcessService::new(root).with_env_snapshot(vec![
        (OsString::from("PATH"), process_path()),
        (OsString::from("HOME"), root.as_os_str().to_owned()),
    ])
}

fn request(command: &str) -> ProcessRequest<'_> {
    ProcessRequest {
        command,
        timeout: Duration::from_secs(3_600),
    }
}

fn command(script: &str, timeout_ms: u64) -> ProcessCommand {
    ProcessCommand {
        script: script.to_owned(),
        timeout_ms,
    }
}

/// Drain a native stream: the concatenated output, the one exit, and a check that
/// `None` follows it and stays.
async fn drain(stream: &mut p1_tool_shell::ProcessStream) -> (Vec<u8>, ProcessEnd) {
    let mut output = Vec::new();
    loop {
        match stream.next().await {
            Some(StreamEvent::Output(bytes)) => output.extend_from_slice(&bytes),
            Some(StreamEvent::Exited(end)) => {
                assert_eq!(stream.next().await, None, "None follows the exit");
                assert_eq!(stream.next().await, None, "and stays");
                return (output, end);
            }
            None => panic!("the stream ended before its exit"),
        }
    }
}

/// Drain a capability handle the same way.
async fn drain_running(running: &mut Box<dyn RunningProcess>) -> (Vec<u8>, ExitStatus) {
    let mut output = Vec::new();
    loop {
        match running.next().await {
            Some(ProcessEvent::Output(bytes)) => output.extend_from_slice(&bytes),
            Some(ProcessEvent::Exited(status)) => {
                assert_eq!(running.next().await, None, "None follows the exit");
                return (output, status);
            }
            None => panic!("the stream ended before its exit"),
        }
    }
}

fn pid_in(root: &Path, name: &str) -> Option<i32> {
    std::fs::read_to_string(root.join(name))
        .ok()?
        .trim()
        .parse()
        .ok()
}

async fn wait_for_pid(root: &Path, name: &str) -> i32 {
    loop {
        if let Some(pid) = pid_in(root, name) {
            return pid;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

fn assert_gone(pid: i32, what: &str) {
    assert_eq!(
        kill(Pid::from_raw(pid), None),
        Err(Errno::ESRCH),
        "{what} {pid} is still there"
    );
}

/// A leader that publishes its pid and a descendant that ignores SIGTERM, so only the
/// escalation to SIGKILL ends the group.
const STUBBORN_GROUP: &str = "echo $$ > leader; \
     bash -c 'trap \"\" TERM; echo $$ > survivor; exec sleep 300' & \
     echo started; wait";

// ------------------------------------------------------------------------ (a)

/// The bound and the omission marker are applied natively, before any byte reaches a
/// caller: a flooding command's events concatenate to exactly what `run` returns.
#[tokio::test]
async fn a_streamed_run_is_byte_identical_to_run_even_when_flooding() {
    let dir = tempfile::tempdir().unwrap();
    let service = service(dir.path());
    for script in [
        "echo hi",
        "printf 'no newline'",
        "seq 1 2000000",
        "yes | head -c 5000000",
        "seq 1 500; echo err >&2; exit 3",
    ] {
        let run = service
            .run(request(script), &CancellationToken::new())
            .await;
        let mut stream = service
            .spawn(request(script), CancellationToken::new())
            .await
            .unwrap();
        let (output, end) = drain(&mut stream).await;
        assert_eq!(end, run.end, "{script}");
        assert_eq!(output, run.output, "{script}");

        let capability = ProcessCapability::new(Arc::new(self::service(dir.path())));
        let mut running = capability
            .spawn(command(script, 3_600_000), CancellationToken::new())
            .await
            .unwrap();
        let (output, _) = drain_running(&mut running).await;
        assert_eq!(output, run.output, "{script} through the capability");
    }
    let flood = service
        .run(request("seq 1 2000000"), &CancellationToken::new())
        .await;
    let text = String::from_utf8_lossy(&flood.output);
    assert!(text.contains("bytes omitted"), "{text}");
    assert!(flood.output.len() < 60_000, "{}", flood.output.len());
}

/// The head is handed on as it is printed, not held until the exit.
#[tokio::test]
async fn the_head_streams_before_the_command_ends() {
    let dir = tempfile::tempdir().unwrap();
    let service = service(dir.path());
    let mut stream = service
        .spawn(
            request("echo before; while [ ! -e go ]; do sleep 0.01; done; echo after"),
            CancellationToken::new(),
        )
        .await
        .unwrap();

    let mut seen = Vec::new();
    // By length, so unexpected output fails the assertion below instead of waiting on
    // a command that waits for the test.
    while seen.len() < b"before\n".len() {
        match stream.next().await {
            Some(StreamEvent::Output(bytes)) => seen.extend_from_slice(&bytes),
            other => panic!("expected output while the command waits, got {other:?}"),
        }
    }
    assert_eq!(seen, b"before\n");
    std::fs::write(dir.path().join("go"), b"").unwrap();
    let (rest, end) = drain(&mut stream).await;
    assert_eq!(rest, b"after\n");
    assert_eq!(end, ProcessEnd::Exited(0));
}

// ------------------------------------------------------------------------ (b)

/// Output, exactly one exit, then `None`; and a `next` dropped while it waits (the
/// runtime drops it on cancel) loses nothing. The command cannot print before the
/// test creates `go`, so the dropped `next` was certainly waiting.
#[tokio::test]
async fn next_is_cancellation_safe_and_ends_with_one_exit() {
    let dir = tempfile::tempdir().unwrap();
    let capability = ProcessCapability::new(Arc::new(service(dir.path())));
    let mut running = capability
        .spawn(
            command(
                "echo one; while [ ! -e go ]; do sleep 0.01; done; echo two; exit 4",
                3_600_000,
            ),
            CancellationToken::new(),
        )
        .await
        .unwrap();

    let mut output = Vec::new();
    while output.len() < b"one\n".len() {
        match running.next().await {
            Some(ProcessEvent::Output(bytes)) => output.extend_from_slice(&bytes),
            other => panic!("expected output, got {other:?}"),
        }
    }
    assert_eq!(output, b"one\n");
    for _ in 0..3 {
        // Dropped unfinished: nothing can be printed until `go` exists.
        let dropped = tokio::time::timeout(Duration::from_millis(50), running.next()).await;
        assert!(dropped.is_err(), "{dropped:?}");
    }
    std::fs::write(dir.path().join("go"), b"").unwrap();
    let (rest, status) = drain_running(&mut running).await;
    output.extend_from_slice(&rest);

    assert_eq!(output, b"one\ntwo\n");
    assert_eq!(status, ExitStatus::Code(4));
}

// ------------------------------------------------------------------------ (c)

/// `kill` runs the cancelled run's SIGTERM, grace, SIGKILL and reap and returns only
/// once the group is gone; `next` then yields the rest and `Exited(Cancelled)`.
#[tokio::test]
async fn kill_ends_the_whole_group_before_it_returns() {
    let dir = tempfile::tempdir().unwrap();
    let service = service(dir.path());
    let mut stream = service
        .spawn(request(STUBBORN_GROUP), CancellationToken::new())
        .await
        .unwrap();
    let leader = wait_for_pid(dir.path(), "leader").await;
    let survivor = wait_for_pid(dir.path(), "survivor").await;

    stream.kill().await;

    assert_gone(survivor, "the TERM-ignoring survivor");
    assert_gone(leader, "the leader");
    let (_, end) = drain(&mut stream).await;
    assert_eq!(end, ProcessEnd::Cancelled);
}

/// Through the capability, with more output than the head holds: `kill` returns once
/// the group is gone, and `next` then yields what remains — the omission marker and
/// the tail the service kept — and `Exited(Cancelled)`.
#[tokio::test]
async fn kill_through_the_capability_ends_the_group_and_leaves_the_tail() {
    let dir = tempfile::tempdir().unwrap();
    let capability = ProcessCapability::new(Arc::new(service(dir.path())));
    let script = format!("seq 1 100000; {STUBBORN_GROUP}");
    let mut running = capability
        .spawn(command(&script, 3_600_000), CancellationToken::new())
        .await
        .unwrap();
    // Keep reading while waiting for the pids, or the flood would fill the pipe and
    // block the command before it gets there; a `next` dropped here loses nothing.
    let mut output = Vec::new();
    let published = async {
        (
            wait_for_pid(dir.path(), "leader").await,
            wait_for_pid(dir.path(), "survivor").await,
        )
    };
    let mut published = std::pin::pin!(published);
    let (leader, survivor) = loop {
        tokio::select! {
            pids = &mut published => break pids,
            event = running.next() => match event {
                Some(ProcessEvent::Output(bytes)) => output.extend_from_slice(&bytes),
                other => panic!("expected output while the command runs, got {other:?}"),
            },
        }
    };

    running.kill().await;

    assert_gone(survivor, "the TERM-ignoring survivor");
    assert_gone(leader, "the leader");
    let head = output.len();
    let (rest, status) = drain_running(&mut running).await;
    assert_eq!(status, ExitStatus::Cancelled);
    output.extend_from_slice(&rest);
    let text = String::from_utf8_lossy(&output);
    assert!(text.starts_with("1\n2\n3\n"), "the head streamed first");
    let rest = String::from_utf8_lossy(&output[head..]);
    assert!(
        rest.contains("bytes omitted"),
        "the marker comes after the kill"
    );
    assert!(rest.contains("\n100000\n"), "and the tail the service kept");
}

/// The call's token, when it fires, ends the command the same way (the runtime also
/// calls `kill`, which then finds the group already gone).
#[tokio::test]
async fn the_call_token_cancels_the_stream() {
    let dir = tempfile::tempdir().unwrap();
    let capability = ProcessCapability::new(Arc::new(service(dir.path())));
    let cancel = CancellationToken::new();
    let mut running = capability
        .spawn(command(STUBBORN_GROUP, 3_600_000), cancel.clone())
        .await
        .unwrap();
    let leader = wait_for_pid(dir.path(), "leader").await;
    let survivor = wait_for_pid(dir.path(), "survivor").await;

    cancel.cancel();
    let (_, status) = drain_running(&mut running).await;

    assert_eq!(status, ExitStatus::Cancelled);
    assert_gone(survivor, "the TERM-ignoring survivor");
    assert_gone(leader, "the leader");
    running.kill().await;
}

/// Dropping the handle while the command runs ends the group. A destructor cannot
/// wait, so the termination runs on the runtime; the test waits for the group to be
/// gone (the bound is a failure deadline, not the synchronisation).
#[tokio::test]
async fn dropping_the_handle_kills_the_group() {
    let dir = tempfile::tempdir().unwrap();
    let capability = ProcessCapability::new(Arc::new(service(dir.path())));
    let running = capability
        .spawn(command(STUBBORN_GROUP, 3_600_000), CancellationToken::new())
        .await
        .unwrap();
    let leader = wait_for_pid(dir.path(), "leader").await;
    let survivor = wait_for_pid(dir.path(), "survivor").await;

    drop(running);

    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    for (pid, what) in [(survivor, "the survivor"), (leader, "the leader")] {
        while kill(Pid::from_raw(pid), None) != Err(Errno::ESRCH) {
            assert!(
                tokio::time::Instant::now() < deadline,
                "{what} {pid} outlived its dropped handle"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }
}

/// A timeout is `Exited(TimedOut)`, reported only once the group is gone.
#[tokio::test]
async fn a_timeout_is_exited_timed_out_after_the_group_is_gone() {
    let dir = tempfile::tempdir().unwrap();
    let capability = ProcessCapability::new(Arc::new(service(dir.path())));
    let mut running = capability
        .spawn(command(STUBBORN_GROUP, 2_000), CancellationToken::new())
        .await
        .unwrap();

    let (_, status) = drain_running(&mut running).await;

    assert_eq!(status, ExitStatus::TimedOut);
    // A start-up slower than the limit leaves no pid to check: then the group was
    // killed before its members existed, which is also correct.
    for (name, what) in [("survivor", "the survivor"), ("leader", "the leader")] {
        if let Some(pid) = pid_in(dir.path(), name) {
            assert_gone(pid, what);
        }
    }
}

// ------------------------------------------------------------------------ (d)

/// `spawn`'s `Err` is `ProcessFailure`'s text, the one the native tool shows the model.
#[tokio::test]
async fn a_failed_start_is_the_process_failure_text() {
    let dir = tempfile::tempdir().unwrap();
    // No `bash` on this PATH: the start itself fails.
    let broken = || {
        ProcessService::new(dir.path()).with_env_snapshot(vec![(
            OsString::from("PATH"),
            OsString::from("/nonexistent"),
        )])
    };
    let run = broken()
        .run(request("true"), &CancellationToken::new())
        .await;
    let ProcessEnd::Failed(failure) = run.end else {
        panic!("the start must fail: {run:?}");
    };
    assert!(matches!(
        failure,
        ProcessFailure::Start {
            program: "bash",
            ..
        }
    ));

    let native = match broken()
        .spawn(request("true"), CancellationToken::new())
        .await
    {
        Ok(_) => panic!("the start must fail"),
        Err(failure) => failure,
    };
    assert_eq!(native, failure);

    let capability = ProcessCapability::new(Arc::new(broken()));
    let refused = match capability
        .spawn(command("true", 1_000), CancellationToken::new())
        .await
    {
        Ok(_) => panic!("the start must fail"),
        Err(reason) => reason,
    };
    assert_eq!(refused, failure.to_string());
    assert!(refused.starts_with("failed to start bash: "), "{refused}");
}

/// A call already cancelled starts nothing; the runtime turns the refusal into
/// `exited(cancelled)` on the resource.
#[tokio::test]
async fn a_cancelled_call_starts_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let capability = ProcessCapability::new(Arc::new(service(dir.path())));
    let cancel = CancellationToken::new();
    cancel.cancel();

    let spawned = capability
        .spawn(command("touch started", 60_000), cancel)
        .await;

    assert!(spawned.is_err());
    assert!(!dir.path().join("started").exists());
}

// ------------------------------------------------------------------------ (e)

fn bwrap_usable() -> bool {
    std::process::Command::new("bwrap")
        .args([
            "--ro-bind",
            "/",
            "/",
            "--dev",
            "/dev",
            "--proc",
            "/proc",
            "true",
        ])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

/// A guest's command runs inside the sandbox the host assembled: the request carries
/// only a script and a time limit, so nothing lets it reach the hidden home.
#[tokio::test]
async fn a_sandboxed_capability_spawns_inside_the_sandbox() {
    if !bwrap_usable() {
        eprintln!("SKIP: bwrap unusable here");
        return;
    }
    let home = tempfile::tempdir().unwrap();
    let home_path: PathBuf = home.path().canonicalize().unwrap();
    let workspace = home_path.join("ws");
    std::fs::create_dir_all(home_path.join(".secret")).unwrap();
    std::fs::create_dir_all(&workspace).unwrap();
    std::fs::write(home_path.join(".secret/token"), "stream-secret").unwrap();
    let script = format!("cat {}", home_path.join(".secret/token").display());

    let open = ProcessCapability::new(Arc::new(service(&workspace)));
    let mut control = open
        .spawn(command(&script, 60_000), CancellationToken::new())
        .await
        .unwrap();
    let (output, _) = drain_running(&mut control).await;
    assert!(
        String::from_utf8_lossy(&output).contains("stream-secret"),
        "the control must reach the secret unsandboxed"
    );

    let sandboxed = service(&workspace)
        .sandboxed(Sandbox::for_home(&home_path))
        .expect("the bwrap probe must succeed once bwrap_usable() is true");
    let capability = ProcessCapability::new(Arc::new(sandboxed));
    let mut running = capability
        .spawn(command(&script, 60_000), CancellationToken::new())
        .await
        .unwrap();
    let (output, status) = drain_running(&mut running).await;

    assert!(
        matches!(status, ExitStatus::Code(code) if code != 0),
        "{status:?}"
    );
    assert!(
        !String::from_utf8_lossy(&output).contains("stream-secret"),
        "the sandboxed command uncovered the hidden home"
    );
}
