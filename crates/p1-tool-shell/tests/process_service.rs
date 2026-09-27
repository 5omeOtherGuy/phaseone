//! The process service on its own, without the tool on top: what S3.2 hands a
//! WebAssembly guest as its only process capability.
//!
//! A [`ProcessRequest`] holds a command text and a timeout and nothing else, so a
//! caller cannot pick the program, the environment, the working directory or
//! the sandbox (the compile-level half is the `compile_fail` example on
//! `ProcessRequest`). These tests prove the runtime half: whatever the command,
//! an assembled sandbox and the environment policy apply, and a cancelled run
//! leaves no process of its group behind. The sandboxed cases need a real
//! `bwrap` and print `SKIP: bwrap unusable here` when the probe fails; on this
//! machine bwrap works, so they run.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::Duration;

use nix::errno::Errno;
use nix::sys::signal::kill;
use nix::unistd::Pid;
use p1_contracts::CancellationToken;
use p1_tool_shell::{ProcessEnd, ProcessOutcome, ProcessRequest, ProcessService, Sandbox};

const SECRET: &str = "process-service-secret";

/// Whether a throwaway real `bwrap` can run here at all.
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

/// The sandboxed cases need a working bwrap; without one they are skipped loudly.
macro_rules! require_bwrap {
    () => {
        if !bwrap_usable() {
            eprintln!("SKIP: bwrap unusable here");
            return;
        }
    };
}

fn process_path() -> OsString {
    std::env::var_os("PATH").unwrap_or_else(|| OsString::from("/usr/bin:/bin"))
}

async fn run(service: &ProcessService, command: &str) -> ProcessOutcome {
    let request = ProcessRequest {
        command,
        timeout: Duration::from_secs(60),
    };
    service.run(request, &CancellationToken::new()).await
}

fn text(outcome: &ProcessOutcome) -> String {
    String::from_utf8_lossy(&outcome.output).into_owned()
}

/// A fake home holding `.secret/token`, with the workspace `H/ws` inside it.
struct FakeHome {
    home: tempfile::TempDir,
    workspace: PathBuf,
}

impl FakeHome {
    fn new() -> Self {
        let home = tempfile::tempdir().unwrap();
        let workspace = home.path().canonicalize().unwrap().join("ws");
        std::fs::create_dir_all(home.path().join(".secret")).unwrap();
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::write(home.path().join(".secret/token"), SECRET).unwrap();
        Self { home, workspace }
    }

    fn home(&self) -> PathBuf {
        self.home.path().canonicalize().unwrap()
    }

    fn service(&self) -> ProcessService {
        ProcessService::new(&self.workspace).with_env_snapshot(vec![
            (OsString::from("PATH"), process_path()),
            (OsString::from("HOME"), self.home().into_os_string()),
        ])
    }
}

/// Commands that try, in different ways, to reach the hidden home. Each one
/// reveals the secret when run WITHOUT the sandbox (the control), so its failure
/// inside the sandbox is the sandbox's doing and not a broken command.
fn escape_attempts(home: &Path) -> Vec<String> {
    let token = home.join(".secret/token");
    let token = token.display();
    let home = home.display();
    vec![
        format!("cat {token}"),
        "cat \"$HOME/.secret/token\"".to_string(),
        format!("cd {home} && cat .secret/token"),
        format!("env -i /bin/sh -c 'cat {token}'"),
        format!("bash --norc --noprofile -c 'cat {token}'"),
        format!("find {home} -name token -exec cat {{}} \\;"),
    ]
}

#[tokio::test]
async fn a_sandboxed_service_hides_the_home_whatever_the_command() {
    require_bwrap!();
    let fixture = FakeHome::new();
    let open = fixture.service();
    let sandboxed = fixture
        .service()
        .sandboxed(Sandbox::for_home(fixture.home()))
        .expect("the bwrap probe must succeed once bwrap_usable() is true");
    assert!(sandboxed.is_sandboxed());

    for command in escape_attempts(&fixture.home()) {
        let control = run(&open, &command).await;
        assert!(
            text(&control).contains(SECRET),
            "control {command:?} must reach the secret unsandboxed: {control:?}"
        );

        let outcome = run(&sandboxed, &command).await;
        assert!(
            matches!(outcome.end, ProcessEnd::Exited(_)),
            "{command:?}: {outcome:?}"
        );
        assert!(
            !text(&outcome).contains(SECRET),
            "{command:?} uncovered the hidden home: {outcome:?}"
        );
    }
}

#[tokio::test]
async fn a_sandboxed_service_runs_in_the_assembled_workspace_only() {
    require_bwrap!();
    let fixture = FakeHome::new();
    let sandboxed = fixture
        .service()
        .sandboxed(Sandbox::for_home(fixture.home()))
        .expect("the bwrap probe must succeed once bwrap_usable() is true");

    // Whatever directory the command moves to, it started in the workspace, and
    // only the workspace (not the home around it) is writable.
    let outcome = run(
        &sandboxed,
        "pwd; touch inside && echo wrote-inside; touch \"$HOME/outside\" 2>/dev/null || echo home-read-only",
    )
    .await;

    assert_eq!(outcome.end, ProcessEnd::Exited(0), "{outcome:?}");
    assert_eq!(
        text(&outcome),
        format!(
            "{}\nwrote-inside\nhome-read-only\n",
            fixture.workspace.display()
        )
    );
    assert!(fixture.workspace.join("inside").exists());
    assert!(!fixture.home().join("outside").exists());
}

/// The value of `NAME=` in `env` output, or `None` when the name is absent.
fn value_of(output: &str, name: &str) -> Option<String> {
    let prefix = format!("{name}=");
    output
        .lines()
        .find_map(|line| line.strip_prefix(&prefix).map(str::to_string))
}

fn env_service(root: &Path) -> ProcessService {
    ProcessService::new(root)
        .with_env_snapshot(vec![
            (OsString::from("PATH"), process_path()),
            (OsString::from("LC_ALL"), OsString::from("C")),
            (OsString::from("CANARY_TOKEN"), OsString::from("secret-1")),
            (OsString::from("SSH_AUTH_SOCK"), OsString::from("/x")),
            (OsString::from("MY_TOOL_HOME"), OsString::from("/opt/t")),
        ])
        .with_env_pass(vec!["MY_TOOL_HOME".to_string()])
}

fn assert_env_policy(outcome: &ProcessOutcome) {
    assert_eq!(outcome.end, ProcessEnd::Exited(0), "{outcome:?}");
    let output = text(outcome);
    assert_eq!(value_of(&output, "CANARY_TOKEN"), None);
    assert_eq!(value_of(&output, "SSH_AUTH_SOCK"), None);
    assert_eq!(value_of(&output, "LC_ALL").as_deref(), Some("C"));
    assert_eq!(value_of(&output, "MY_TOOL_HOME").as_deref(), Some("/opt/t"));
}

#[tokio::test]
async fn the_environment_policy_applies_at_the_service() {
    let dir = tempfile::tempdir().unwrap();
    let service = env_service(dir.path());

    assert_env_policy(&run(&service, "env").await);
}

#[tokio::test]
async fn the_environment_policy_applies_inside_the_sandbox() {
    require_bwrap!();
    let fixture = FakeHome::new();
    let service = env_service(&fixture.workspace)
        .sandboxed(Sandbox::for_home(fixture.home()))
        .expect("the bwrap probe must succeed once bwrap_usable() is true");

    assert_env_policy(&run(&service, "env").await);
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

/// The survivor ignores SIGTERM, so only the SIGKILL escalation can end it. The
/// cancel fires once both pids are published (an observed condition), and the
/// check is made the moment `run` returns: the service must not return while any
/// process of the group still exists.
#[tokio::test]
async fn a_cancelled_run_returns_only_after_the_whole_group_is_gone() {
    let dir = tempfile::tempdir().unwrap();
    let service = ProcessService::new(dir.path())
        .with_env_snapshot(vec![(OsString::from("PATH"), process_path())]);
    let cancel = CancellationToken::new();
    let request = ProcessRequest {
        command: "echo $$ > leader; \
                  bash -c 'trap \"\" TERM; echo $$ > survivor; exec sleep 300' & wait",
        timeout: Duration::from_secs(3_600),
    };

    let run = service.run(request, &cancel);
    let stopper = async {
        let leader = wait_for_pid(dir.path(), "leader").await;
        let survivor = wait_for_pid(dir.path(), "survivor").await;
        cancel.cancel();
        (leader, survivor)
    };
    let (outcome, (leader, survivor)) = tokio::join!(run, stopper);

    assert_eq!(outcome.end, ProcessEnd::Cancelled, "{outcome:?}");
    assert_eq!(
        kill(Pid::from_raw(survivor), None),
        Err(Errno::ESRCH),
        "the TERM-ignoring survivor {survivor} outlived the run"
    );
    assert_eq!(
        kill(Pid::from_raw(leader), None),
        Err(Errno::ESRCH),
        "the leader {leader} outlived the run"
    );
}
