//! The `shell` sandbox's must-pass examples (a)–(i) from `docs/design/tools.md`,
//! run against the real `bwrap`. A test that needs `bwrap` prints
//! `SKIP: bwrap unusable here` and returns when the probe fails: CI runners may
//! forbid unprivileged user namespaces. On this machine bwrap works, so every
//! test below runs.
//!
//! The fake home `H` lives in a temp dir (never the real home), and no test
//! reads a real credential file.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use p1_contracts::{
    CancellationToken, Tool, ToolCall, ToolContext, ToolInput, ToolOutcome, ToolStatus,
};
use p1_tool_shell::{DEFAULT_HOME_VISIBLE, Sandbox, SandboxError, ShellTool, bwrap_args};
use p1_workspace::{ToolFace, Workspace};

/// The marker used for the `setsid` child in (g): a unique `sleep` argument, so
/// its command line can be recognised in `/proc` on the HOST (the pid numbers
/// inside the sandbox's pid namespace differ from the host's).
const SLEEP_MARKER: &str = "300.0731";

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

/// `(a)–(i)` need a working bwrap; without one the test is skipped loudly.
macro_rules! require_bwrap {
    () => {
        if !bwrap_usable() {
            eprintln!("SKIP: bwrap unusable here");
            return;
        }
    };
}

fn call(command: &str) -> ToolCall {
    ToolCall {
        call_id: "sandbox-call".into(),
        name: "shell".into(),
        input: ToolInput::Json(serde_json::json!({ "command": command }).to_string()),
    }
}

async fn execute(tool: &ShellTool, command: &str) -> ToolOutcome {
    let call = call(command);
    let context = ToolContext {
        cancel: CancellationToken::new(),
    };
    tool.execute(&call, context).await
}

/// The `[exit code: N]` footer says the command ran; `0` is success.
fn exited_zero(outcome: &ToolOutcome) -> bool {
    outcome.content.contains("[exit code: 0]")
}

/// A shell tool with `home` hidden, `workspace` writable, and any extra
/// `writable` paths bound back in.
fn sandboxed(home: &Path, workspace: &Path, writable: Vec<PathBuf>) -> ShellTool {
    let mut sandbox = Sandbox::for_home(home);
    sandbox.writable = writable;
    ShellTool::new(Workspace::new(workspace).unwrap())
        .sandboxed(sandbox)
        .expect("the bwrap probe must succeed once bwrap_usable() is true")
}

/// A fake home with `.secret/token`, `.cargo/bin/` and the workspace `H/ws`.
struct FakeHome {
    home: tempfile::TempDir,
    workspace: PathBuf,
}

impl FakeHome {
    fn new() -> Self {
        let home = tempfile::tempdir().unwrap();
        let workspace = home.path().join("ws");
        std::fs::create_dir_all(home.path().join(".secret")).unwrap();
        std::fs::create_dir_all(home.path().join(".cargo/bin")).unwrap();
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::write(home.path().join(".secret/token"), "sandbox-secret-token").unwrap();
        Self { home, workspace }
    }

    fn path(&self) -> &Path {
        self.home.path()
    }

    /// The workspace as `Workspace`, canonicalised.
    fn tool(&self, writable: Vec<PathBuf>) -> ShellTool {
        sandboxed(self.path(), &self.workspace, writable)
    }
}

// ------------------------------------------------------------------------ (a)

#[tokio::test]
async fn a_writing_inside_the_workspace_works_and_is_visible_on_the_host() {
    require_bwrap!();
    let fixture = FakeHome::new();
    let tool = fixture.tool(Vec::new());

    let outcome = execute(&tool, "echo x > a.txt").await;

    assert_eq!(outcome.status, ToolStatus::Ok, "{outcome:?}");
    assert!(exited_zero(&outcome), "{outcome:?}");
    assert_eq!(
        std::fs::read_to_string(fixture.workspace.join("a.txt"))
            .unwrap()
            .trim(),
        "x"
    );
}

// ------------------------------------------------------------------------ (b)

#[tokio::test]
async fn b_a_write_outside_the_workspace_fails_and_creates_nothing() {
    require_bwrap!();
    let fixture = FakeHome::new();
    let tool = fixture.tool(Vec::new());

    let outcome = execute(&tool, "echo x > ../outside.txt").await;

    assert_eq!(
        outcome.status,
        ToolStatus::Ok,
        "non-zero exit is not an error"
    );
    assert!(!exited_zero(&outcome), "{outcome:?}");
    assert!(
        outcome.content.contains("Read-only file system"),
        "the hidden home must fail loudly: {outcome:?}"
    );
    assert!(!fixture.path().join("outside.txt").exists());
}

// ------------------------------------------------------------------------ (c)

#[tokio::test]
async fn c_the_hidden_home_and_its_secrets_are_invisible() {
    require_bwrap!();
    let fixture = FakeHome::new();
    let tool = fixture.tool(Vec::new());

    // HOME is set inside the command because the sandbox hides a fake home that
    // is not the process's real HOME; `~` must resolve to it.
    let command = format!(
        "HOME='{}'; cat \"$HOME/.secret/token\"",
        fixture.path().display()
    );
    let outcome = execute(&tool, &command).await;

    assert!(!exited_zero(&outcome), "{outcome:?}");
    assert!(
        !outcome.content.contains("sandbox-secret-token"),
        "the hidden token leaked: {outcome:?}"
    );
}

// ------------------------------------------------------------------------ (d)

#[tokio::test]
async fn d_visible_home_entries_are_read_only() {
    require_bwrap!();
    let fixture = FakeHome::new();
    let tool = fixture.tool(Vec::new());

    let listing = execute(&tool, &format!("ls '{}'/.cargo", fixture.path().display())).await;
    assert!(exited_zero(&listing), "{listing:?}");
    assert!(listing.content.contains("bin"), "{listing:?}");

    let touch = execute(
        &tool,
        &format!("touch '{}'/.cargo/z", fixture.path().display()),
    )
    .await;
    assert!(!exited_zero(&touch), "{touch:?}");
    assert!(!fixture.path().join(".cargo/z").exists());
}

// ------------------------------------------------------------------------ (e)

#[tokio::test]
async fn e_tmp_is_private_to_the_sandbox() {
    require_bwrap!();
    let fixture = FakeHome::new();
    let tool = fixture.tool(Vec::new());
    // The spec's name; make sure a stale one cannot make the assertion vacuous.
    let _ = std::fs::remove_file("/tmp/t");
    assert!(!Path::new("/tmp/t").exists());

    let outcome = execute(&tool, "echo t > /tmp/t").await;

    assert!(exited_zero(&outcome), "{outcome:?}");
    assert!(
        !Path::new("/tmp/t").exists(),
        "the write must land in the private /tmp, not the host's"
    );
}

// ------------------------------------------------------------------------ (f)

#[tokio::test]
async fn f_a_configured_writable_path_stays_writable() {
    require_bwrap!();
    let fixture = FakeHome::new();
    let extra = tempfile::tempdir().unwrap();
    let tool = fixture.tool(vec![extra.path().to_path_buf()]);

    let outcome = execute(
        &tool,
        &format!("echo w > '{}'/w.txt", extra.path().display()),
    )
    .await;

    assert!(exited_zero(&outcome), "{outcome:?}");
    assert_eq!(
        std::fs::read_to_string(extra.path().join("w.txt"))
            .unwrap()
            .trim(),
        "w"
    );
}

// ------------------------------------------------------------------------ (g)

/// `true` while a host process runs `sleep <SLEEP_MARKER>`, i.e. the sandboxed
/// `setsid` child. Scanning `/proc` shows the namespace's processes from the
/// host, where their pids differ from the sandbox's.
fn sleep_visible() -> bool {
    let needle = format!("sleep\0{SLEEP_MARKER}");
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return false;
    };
    entries.flatten().any(|entry| {
        let Ok(bytes) = std::fs::read(entry.path().join("cmdline")) else {
            return false;
        };
        String::from_utf8_lossy(&bytes).contains(&needle)
    })
}

#[tokio::test]
async fn g_cancellation_kills_a_setsid_child_the_standalone_process() {
    require_bwrap!();
    let fixture = FakeHome::new();
    let tool = fixture.tool(Vec::new());
    let cancel = CancellationToken::new();
    let task_cancel = cancel.clone();
    let command = format!("setsid sleep {SLEEP_MARKER} & wait");
    let task = tokio::spawn(async move {
        let call = call(&command);
        tool.execute(
            &call,
            ToolContext {
                cancel: task_cancel,
            },
        )
        .await
    });

    // Cancel only once the sandboxed sleep is really running, so there is a
    // process the tool has to kill.
    let deadline = Instant::now() + Duration::from_secs(20);
    while !sleep_visible() {
        assert!(
            Instant::now() < deadline,
            "the sandboxed sleep never started"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    cancel.cancel();
    let outcome = task.await.unwrap();

    assert_eq!(outcome.status, ToolStatus::Cancelled, "{outcome:?}");
    assert!(!sleep_visible(), "the setsid child survived the tool");
}

// ------------------------------------------------------------------------ (h)

#[test]
fn h_a_workspace_that_contains_the_home_is_refused() {
    // Workspace == home.
    let home = tempfile::tempdir().unwrap();
    let error = match ShellTool::new(Workspace::new(home.path()).unwrap())
        .sandboxed(Sandbox::for_home(home.path()))
    {
        Err(error) => error,
        Ok(_) => panic!("a workspace equal to the home must be refused"),
    };
    assert!(
        matches!(error, SandboxError::WorkspaceContainsHome { .. }),
        "{error:?}"
    );

    // Workspace is an ancestor of home.
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("deep/home");
    std::fs::create_dir_all(&home).unwrap();
    let error = match ShellTool::new(Workspace::new(root.path()).unwrap())
        .sandboxed(Sandbox::for_home(&home))
    {
        Err(error) => error,
        Ok(_) => panic!("a workspace containing the home must be refused"),
    };
    assert!(
        matches!(error, SandboxError::WorkspaceContainsHome { .. }),
        "{error:?}"
    );
}

// ------------------------------------------------------------------------ (i)

/// The argument vector exactly as the mount plan lists it: `/tmp` before the
/// home, the home before the workspace, `--remount-ro` after every bind.
fn expected_args(home: &Path, workspace: &Path, tmp: &Path, writable: &[&Path]) -> Vec<String> {
    let mut args: Vec<String> = vec![
        "--ro-bind".into(),
        "/".into(),
        "/".into(),
        "--dev".into(),
        "/dev".into(),
        "--proc".into(),
        "/proc".into(),
        "--bind".into(),
        tmp.display().to_string(),
        "/tmp".into(),
        "--setenv".into(),
        "TMPDIR".into(),
        "/tmp".into(),
        "--tmpfs".into(),
        home.display().to_string(),
    ];
    for entry in DEFAULT_HOME_VISIBLE {
        let path = home.join(entry);
        if path.exists() {
            args.extend([
                "--ro-bind".into(),
                path.display().to_string(),
                path.display().to_string(),
            ]);
        }
    }
    for name in ["credentials.toml", "credentials"] {
        let path = home.join(".cargo").join(name);
        if path.exists() {
            args.extend([
                "--ro-bind".into(),
                "/dev/null".into(),
                path.display().to_string(),
            ]);
        }
    }
    for path in writable {
        if path.exists() {
            args.extend([
                "--bind".into(),
                path.display().to_string(),
                path.display().to_string(),
            ]);
        }
    }
    args.extend([
        "--bind".into(),
        workspace.display().to_string(),
        workspace.display().to_string(),
        "--remount-ro".into(),
        home.display().to_string(),
        "--unshare-pid".into(),
        "--die-with-parent".into(),
        "--chdir".into(),
        workspace.display().to_string(),
    ]);
    args
}

/// A workspace under `/tmp` (the common case for `tempfile`-based callers).
#[test]
fn i_bwrap_args_order_for_a_workspace_under_tmp() {
    let home = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(home.path().join(".cargo")).unwrap();
    std::fs::write(home.path().join(".cargo/credentials.toml"), "token").unwrap();
    let workspace = home.path().join("ws");
    std::fs::create_dir_all(&workspace).unwrap();
    let extra = tempfile::tempdir().unwrap();
    let private_tmp = tempfile::tempdir().unwrap();
    let missing = home.path().join("does-not-exist");

    let mut sandbox = Sandbox::for_home(home.path());
    sandbox.writable = vec![extra.path().to_path_buf(), missing.clone()];
    let args = bwrap_args(&sandbox, &workspace, private_tmp.path());
    let expected = expected_args(
        home.path(),
        &workspace,
        private_tmp.path(),
        &[extra.path(), &missing],
    );

    assert_eq!(os_args(&args), expected);
}

/// A workspace under the home: the workspace bind must come after the home's
/// tmpfs and before `--remount-ro`.
#[test]
fn i_bwrap_args_order_for_a_workspace_under_the_home() {
    let home = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(home.path().join(".gitconfig")).unwrap();
    let workspace = home.path().join("nested/ws");
    std::fs::create_dir_all(&workspace).unwrap();
    let private_tmp = tempfile::tempdir().unwrap();

    let sandbox = Sandbox::for_home(home.path());
    let args = bwrap_args(&sandbox, &workspace, private_tmp.path());
    let expected = expected_args(home.path(), &workspace, private_tmp.path(), &[]);

    assert_eq!(os_args(&args), expected);
}

/// Requirement 4: `with_face` after `sandboxed` and `sandboxed` after
/// `with_face` both keep the sandbox, its paragraph and the `+sandbox` variant,
/// exactly once.
#[tokio::test]
async fn the_sandbox_survives_a_face_change_in_either_order() {
    require_bwrap!();
    let home = tempfile::tempdir().unwrap();
    let workspace = home.path().join("ws");
    std::fs::create_dir_all(&workspace).unwrap();

    let sandboxed_then_faced = ShellTool::new(Workspace::new(&workspace).unwrap())
        .sandboxed(Sandbox::for_home(home.path()))
        .unwrap()
        .with_face(ToolFace::new("Run", "custom"), "gpt");
    let faced_then_sandboxed = ShellTool::new(Workspace::new(&workspace).unwrap())
        .with_face(ToolFace::new("Run", "custom"), "gpt")
        .sandboxed(Sandbox::for_home(home.path()))
        .unwrap();

    let paragraph = "Commands run in a sandbox: only the workspace and /tmp are writable";
    for tool in [&sandboxed_then_faced, &faced_then_sandboxed] {
        assert_eq!(tool.declaration().name, "Run");
        assert!(
            tool.declaration().description.starts_with("custom"),
            "{}",
            tool.declaration().description
        );
        assert_eq!(
            tool.declaration().description.matches(paragraph).count(),
            1,
            "the sandbox paragraph must appear exactly once: {}",
            tool.declaration().description
        );
        assert_eq!(tool.identity().variant, "gpt+sandbox");
    }
    // The sandbox really is on: a command still runs through bwrap.
    let tool = &sandboxed_then_faced;
    let outcome = execute(tool, "echo sandboxed").await;
    assert!(exited_zero(&outcome), "{outcome:?}");
}

/// `bwrap_args` is pure: `OsString` here, plain `String` above, for comparison.
fn os_args(args: &[std::ffi::OsString]) -> Vec<String> {
    args.iter()
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect()
}

#[test]
fn the_default_visible_entries_are_exactly_the_spec_list() {
    let sandbox = Sandbox::for_home("/home/fake");
    let listed: Vec<String> = sandbox
        .home_visible
        .iter()
        .map(|entry| entry.display().to_string())
        .collect();
    assert_eq!(
        listed,
        [
            ".cargo",
            ".rustup",
            ".local/bin",
            ".local/lib",
            ".nvm",
            ".gitconfig",
            ".config/git"
        ]
    );
    assert_eq!(
        DEFAULT_HOME_VISIBLE,
        &[
            ".cargo",
            ".rustup",
            ".local/bin",
            ".local/lib",
            ".nvm",
            ".gitconfig",
            ".config/git"
        ]
    );
}
