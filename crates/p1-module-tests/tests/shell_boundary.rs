//! The shell COMPONENT's execution boundary, proved against a real bubblewrap (S3.4, issue
//! #255).
//!
//! The component (`p1/shell`, `modules/p1-module-shell/`) decides only what the model asked
//! for: it parses the input, renders the result and sends `process.command` — a script and a
//! time limit (`modules/wit/process.wit`). Everything that touches a real process is the
//! host's: the sandbox, the environment policy, the working directory, the process group and
//! the kill. This suite binds the native, sandboxed `ProcessService` of `crates/p1-tool-shell`
//! (S3.1/S3.2) to the component's `process` import through `ProcessCapability` and drives every
//! case through the loaded component's `execute`, so what it proves is the boundary ACROSS the
//! WebAssembly boundary: whatever the component asks, the command runs inside the sandbox the
//! host assembled, with the host's environment policy, and dies with the call.
//!
//! The component is loaded through `Loader` from the release `scripts/build-modules.sh
//! --package p1-module-shell` wrote, so its identity is loader-built; the harness's
//! fixture-bound `Release` is not used because this suite's component is the shipped shell
//! package, not the fixture (see [`shell_loader`]).
//!
//! ADR-0077: a GitHub-hosted runner may forbid unprivileged user namespaces, so the bwrap
//! probe prints `SKIP: bwrap unusable here` and returns. A stream box must prove the boundary
//! instead: `P1_REQUIRE_BWRAP=1` turns that skip into a failure with the same message. Every
//! case starts at the [`require_bwrap!`] probe; on the box bubblewrap works, so the suite runs
//! whole and the logs carry no `SKIP`.
//!
//! No case touches real user data: every home, workspace and secret is a tempdir, no test
//! mutates the process environment (it reads only `PATH` to resolve `bash`/`bwrap`, `HOME` as a
//! path that is never written, and `P1_REQUIRE_BWRAP`), and a case that needs a process to be
//! gone synchronizes on an observed file or an observable pid instead of sleeping.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use nix::errno::Errno;
use nix::sys::signal::kill;
use nix::unistd::Pid;
use p1_contracts::serde_json::{self, Value, json};
use p1_contracts::tool::ResultDetail;
use p1_contracts::{
    CancellationToken, Tool, ToolCall, ToolContext, ToolInput, ToolOutcome, ToolResultItem,
    ToolStatus,
};
use p1_module_runtime::{
    ExecutionLimits, LoadedModule, Loader, ProcessCommand, ReleaseManifest, Services, wasm_tool,
};
use p1_module_tests::within_deadline;
use p1_redact::MaskCounter;
use p1_tool_shell::{
    DEFAULT_HOME_VISIBLE, ProcessCapability, ProcessService, Sandbox, ShellTool, bwrap_args,
};
use p1_workspace::Workspace;
use tempfile::TempDir;

/// The built package of the shell component and the manifest name it is loaded under.
const PACKAGE: &str = "p1-module-shell";
const MODULE_NAME: &str = "p1/shell";

/// The system half of `PATH`: what `bash`, `bwrap` and the coreutils need.
const SYSTEM_PATH: &str = "/usr/bin:/bin";

/// How often a bounded poll re-checks its condition, and how long each may wait.
const POLL: Duration = Duration::from_millis(10);
/// How long a case waits for the sandboxed child to publish itself.
const START_LIMIT: Duration = Duration::from_secs(30);
/// How long a case waits for a killed descendant to disappear from the host.
const GONE_LIMIT: Duration = Duration::from_secs(10);

// ----------------------------------------------------------------------------- the bwrap probe

/// Whether a throwaway real `bwrap` can run here at all: exactly the probe `scripts/gate.sh`
/// and the native boundary tests run.
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

/// The one probe every case of this suite starts at. Without a usable bubblewrap a CI runner
/// skips the case loudly (ADR-0077); `P1_REQUIRE_BWRAP=1` makes a stream box fail it instead,
/// so a boundary that silently stopped being proved cannot pass there.
macro_rules! require_bwrap {
    () => {
        if !bwrap_usable() {
            if std::env::var_os("P1_REQUIRE_BWRAP").is_some_and(|value| value == "1") {
                panic!("SKIP: bwrap unusable here");
            }
            eprintln!("SKIP: bwrap unusable here");
            return;
        }
    };
}

// --------------------------------------------------------------------------- the component

/// Where `scripts/build-modules.sh` publishes the built packages.
fn built_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../modules/target/p1-modules")
}

/// The loader over the built shell release, built once for the whole test binary.
fn shell_loader() -> &'static Loader {
    static LOADER: OnceLock<Loader> = OnceLock::new();
    LOADER.get_or_init(build_loader)
}

/// The compiled shell component, loaded once for the whole test binary: the loader's digest
/// check is what makes sharing it safe, and a Wasmtime compile of the component is by far the
/// dominant cost of a case.
fn shell_module() -> &'static LoadedModule {
    static MODULE: OnceLock<LoadedModule> = OnceLock::new();
    MODULE.get_or_init(|| {
        shell_loader()
            .load(MODULE_NAME)
            .expect("the built p1/shell component loads against the runtime's tool world")
    })
}

/// A loader over the built shell package. Its entry is the package manifest the build wrote,
/// laid out as a release ships it, so the loader verifies the digest and the world and returns
/// a loader-built identity (`p1/shell`, variant `claude`). The harness's `Release` is bound to
/// the fixture artifact, which this test target does not build, so it is not used here.
fn build_loader() -> Loader {
    let manifest_path = built_dir()
        .join(PACKAGE)
        .join(format!("{PACKAGE}.manifest.json"));
    let text = std::fs::read_to_string(&manifest_path).unwrap_or_else(|error| {
        panic!(
            "the built shell component {} is missing ({error}): run \
             scripts/build-modules.sh --package p1-module-shell first",
            manifest_path.display()
        )
    });
    let package: Value = serde_json::from_str(&text).expect("the package manifest is JSON");
    let entry = json!({
        "name": package["name"],
        "digest": package["digest"],
        "path": format!("{PACKAGE}/{PACKAGE}.wasm"),
        "kind": package["kind"],
        "world": package["world"],
        "protocol": package["protocol"],
        "capabilities": package["capabilities"],
        "variant": package["variant"],
    });
    let release = json!({ "format": "p1-release-manifest/1", "components": [entry] });
    Loader::new(
        ReleaseManifest::parse(&release.to_string()).expect("release manifest"),
        built_dir(),
    )
    .expect("loader")
}

/// The shell component as the host sees it: loaded by name from the built release, its
/// `process` capability linked to `service`, and wrapped by the redacting decorator `wasm_tool`
/// returns (ADR-0083 §4). Must be built inside a Tokio runtime, which runs the executor.
fn shell_tool(service: ProcessService, counter: &Arc<MaskCounter>) -> Arc<dyn Tool> {
    let process = Arc::new(ProcessCapability::new(Arc::new(service)));
    wasm_tool(
        shell_module(),
        Services {
            process: Some(process as Arc<dyn p1_module_runtime::ProcessService>),
            // The shell world imports only `process`, so every other capability stays unset;
            // `..Default::default()` keeps this compiling when `Services` grows a field.
            ..Services::default()
        },
        ExecutionLimits::default(),
        counter,
    )
    .expect("the shell component is a tool")
}

// ------------------------------------------------------------------------------- the fixture

/// The secret the fake home holds. Never a real credential, never a key shape.
const HOME_SECRET: &str = "boundary-home-secret";

/// A fake home in a tempdir (never the real one) with the workspace `H/ws` inside it, a secret
/// file and a secret directory: the shape `crates/p1-tool-shell/tests/sandbox.rs` uses.
struct FakeHome {
    home: TempDir,
    workspace: PathBuf,
}

impl FakeHome {
    fn new() -> Self {
        let home = tempfile::tempdir().expect("temp dir");
        let workspace = home.path().join("ws");
        std::fs::create_dir_all(home.path().join(".secret")).unwrap();
        std::fs::create_dir_all(home.path().join(".ssh")).unwrap();
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::write(home.path().join(".secret/token"), HOME_SECRET).unwrap();
        std::fs::write(home.path().join(".ssh/id_ed25519"), HOME_SECRET).unwrap();
        Self { home, workspace }
    }

    /// The home as the sandbox is told about it: the canonical path both the containment check
    /// and the mounts use.
    fn home(&self) -> PathBuf {
        self.home.path().canonicalize().expect("canonical home")
    }

    fn path(&self) -> &Path {
        self.home.path()
    }
}

fn process_path() -> OsString {
    std::env::var_os("PATH").unwrap_or_else(|| OsString::from(SYSTEM_PATH))
}

/// The environment snapshot the host assembles a service from: `PATH` so the shell can start,
/// the fake `HOME` the sandbox hides, and a locale.
fn snapshot(home: &Path) -> Vec<(OsString, OsString)> {
    vec![
        (OsString::from("PATH"), process_path()),
        (OsString::from("HOME"), home.as_os_str().to_owned()),
        (OsString::from("LC_ALL"), OsString::from("C")),
    ]
}

/// The native service the host assembles and binds to the component: the workspace as root,
/// the snapshot, the pass-list and, in every case of this suite, the real bubblewrap sandbox
/// around the fake home.
fn sandboxed_service(
    fixture: &FakeHome,
    snapshot: Vec<(OsString, OsString)>,
    env_pass: Vec<String>,
    writable: Vec<PathBuf>,
) -> ProcessService {
    let mut sandbox = Sandbox::for_home(fixture.home());
    sandbox.writable = writable;
    ProcessService::new(&fixture.workspace)
        .with_env_snapshot(snapshot)
        .with_env_pass(env_pass)
        .sandboxed(sandbox)
        .expect("the bwrap probe must succeed once bwrap_usable() is true")
}

/// A call to the shell component: the JSON input the guest parses.
fn shell_call(command: &str, raw: bool) -> ToolCall {
    ToolCall {
        call_id: "shell-boundary".to_owned(),
        name: "shell".to_owned(),
        input: ToolInput::Json(json!({ "command": command, "raw": raw }).to_string()),
    }
}

/// As [`shell_call`], with the explicit `timeout_seconds` the guest turns into
/// `process.command.timeout-ms`.
fn shell_call_with_timeout(command: &str, timeout_seconds: i64) -> ToolCall {
    ToolCall {
        call_id: "shell-boundary".to_owned(),
        name: "shell".to_owned(),
        input: ToolInput::Json(
            json!({ "command": command, "timeout_seconds": timeout_seconds }).to_string(),
        ),
    }
}

/// Run one call through the loaded component with a fresh token.
async fn execute(tool: &dyn Tool, call: &ToolCall) -> ToolOutcome {
    tool.execute(
        call,
        ToolContext {
            cancel: CancellationToken::new(),
        },
    )
    .await
}

/// The `[exit code: N]` footer the guest appends; `0` is success.
fn exited_zero(outcome: &ToolOutcome) -> bool {
    outcome.content.contains("[exit code: 0]")
}

/// The value of `NAME=` in `env`-style output, or `None` when the name is absent.
fn value<'a>(content: &'a str, name: &str) -> Option<&'a str> {
    content
        .lines()
        .find_map(|line| line.strip_prefix(name)?.strip_prefix('='))
}

// -------------------------------------------------- case 1: descendants die with the call

/// The marker each case's sandboxed `sleep` carries. The HOST recognises the descendant by it:
/// inside the sandbox's pid namespace the pid a command writes is the namespace pid, never the
/// host's, so `kill(pid, 0)` needs the host pid resolved from the command line.
const CANCEL_MARKER: &str = "300.0731";
const TIMEOUT_MARKER: &str = "300.0732";
const ABANDON_MARKER: &str = "300.0733";

/// The command the descendant cases run: a `setsid` child in its own session and process group,
/// ignoring SIGTERM, publishing its pid in the workspace and then sleeping under `marker`. The
/// sandboxed shell `wait`s for it, so the sandbox is alive until the host ends it. The child
/// leaves the process group, so the group kill cannot reach it; it dies because the sandbox
/// runs in its own PID namespace (`--unshare-pid`, `bwrap_args`), whose init is the sandbox's
/// shell: when the sandbox ends, the kernel ends what is left of the namespace.
fn survivor_command(marker: &str) -> String {
    format!("setsid bash -c 'trap \"\" TERM; echo $$ > child.pid; exec sleep {marker}' & wait")
}

/// The HOST pid of the sandboxed process whose command line carries `sleep <marker>`.
fn host_pid(marker: &str) -> Option<i32> {
    let needle = format!("sleep\0{marker}");
    std::fs::read_dir("/proc")
        .ok()?
        .flatten()
        .find_map(|entry| {
            let pid = entry.file_name().to_str()?.parse::<i32>().ok()?;
            let bytes = std::fs::read(entry.path().join("cmdline")).ok()?;
            String::from_utf8_lossy(&bytes)
                .contains(needle.as_str())
                .then_some(pid)
        })
}

/// Wait (bounded) for the child's pid file: the explicit synchronization that says the
/// descendant really runs before a case ends the call.
async fn wait_for_file(path: &Path) {
    let deadline = Instant::now() + START_LIMIT;
    while !path.exists() {
        assert!(
            Instant::now() < deadline,
            "the sandboxed child never published {}",
            path.display()
        );
        tokio::time::sleep(POLL).await;
    }
}

/// Wait (bounded) for the host view of the descendant, and return its pid.
async fn wait_for_host_pid(marker: &str) -> i32 {
    let deadline = Instant::now() + START_LIMIT;
    loop {
        if let Some(pid) = host_pid(marker) {
            return pid;
        }
        assert!(
            Instant::now() < deadline,
            "the sandboxed `sleep {marker}` never appeared in /proc"
        );
        tokio::time::sleep(POLL).await;
    }
}

/// The proof that a descendant is gone: `kill(pid, 0)` answers ESRCH. Bounded, because the
/// kernel ends what is left of the pid namespace asynchronously once its init has exited.
async fn wait_gone(pid: i32) {
    let deadline = Instant::now() + GONE_LIMIT;
    while kill(Pid::from_raw(pid), None) != Err(Errno::ESRCH) {
        assert!(
            Instant::now() < deadline,
            "the descendant {pid} outlived the call that started it"
        );
        tokio::time::sleep(POLL).await;
    }
}

/// (a) A cancelled call: once the component's `execute` returns with the `cancelled` status,
/// the SIGTERM-ignoring `setsid` descendant is gone (`kill(pid, 0)` → ESRCH). The cancel fires
/// on an observed condition — the pid file, then the host view — never on a timer.
#[tokio::test]
async fn a_cancelled_call_takes_its_descendants_with_it() {
    require_bwrap!();
    within_deadline("a_cancelled_call_takes_its_descendants_with_it", async {
        let fixture = FakeHome::new();
        let tool = shell_tool(
            sandboxed_service(&fixture, snapshot(&fixture.home()), Vec::new(), Vec::new()),
            &Arc::new(MaskCounter::new()),
        );
        let cancel = CancellationToken::new();
        let control = cancel.clone();
        let call = shell_call(&survivor_command(CANCEL_MARKER), false);
        let root = fixture.workspace.clone();

        let stopper = async move {
            wait_for_file(&root.join("child.pid")).await;
            let pid = wait_for_host_pid(CANCEL_MARKER).await;
            control.cancel();
            pid
        };
        let (outcome, pid) = tokio::join!(
            tool.execute(
                &call,
                ToolContext {
                    cancel: cancel.clone(),
                },
            ),
            stopper
        );

        assert_eq!(outcome.status, ToolStatus::Cancelled, "{outcome:?}");
        assert!(outcome.content.contains("[cancelled]"), "{outcome:?}");
        wait_gone(pid).await;
    })
    .await;
}

/// (b) The same descendant under the input's own time limit: the guest turns
/// `timeout_seconds` into `process.command.timeout-ms`, and when the call returns the
/// descendant is gone.
#[tokio::test]
async fn a_timed_out_call_takes_its_descendants_with_it() {
    require_bwrap!();
    within_deadline("a_timed_out_call_takes_its_descendants_with_it", async {
        let fixture = FakeHome::new();
        let tool = shell_tool(
            sandboxed_service(&fixture, snapshot(&fixture.home()), Vec::new(), Vec::new()),
            &Arc::new(MaskCounter::new()),
        );
        let call = shell_call_with_timeout(&survivor_command(TIMEOUT_MARKER), 3);
        let root = fixture.workspace.clone();

        let watcher = async move {
            wait_for_file(&root.join("child.pid")).await;
            wait_for_host_pid(TIMEOUT_MARKER).await
        };
        let (outcome, pid) = tokio::join!(execute(&*tool, &call), watcher);

        assert_eq!(outcome.status, ToolStatus::Error, "{outcome:?}");
        assert!(
            outcome.content.contains("[timed out after 3 s]"),
            "{outcome:?}"
        );
        wait_gone(pid).await;
    })
    .await;
}

/// (c) An abandoned call: dropping the call future while the component is blocked in
/// `process.running.next` makes the host drop the resource, which ends the process group
/// (`docs/design/modules/wit.md`, streaming resources). The descendant is gone afterwards.
#[tokio::test]
async fn an_abandoned_call_takes_its_descendants_with_it() {
    require_bwrap!();
    within_deadline("an_abandoned_call_takes_its_descendants_with_it", async {
        let fixture = FakeHome::new();
        let tool = shell_tool(
            sandboxed_service(&fixture, snapshot(&fixture.home()), Vec::new(), Vec::new()),
            &Arc::new(MaskCounter::new()),
        );
        let call = shell_call(&survivor_command(ABANDON_MARKER), false);
        let task = tokio::spawn(async move {
            tool.execute(
                &call,
                ToolContext {
                    cancel: CancellationToken::new(),
                },
            )
            .await
        });

        wait_for_file(&fixture.workspace.join("child.pid")).await;
        let pid = wait_for_host_pid(ABANDON_MARKER).await;
        task.abort();
        let joined = task.await;
        assert!(
            joined.as_ref().is_err_and(|error| error.is_cancelled()),
            "the call must have been abandoned while it waited"
        );
        wait_gone(pid).await;
    })
    .await;
}

// ------------------------------------------------------------- case 2: the hidden home

/// Every way a command can try to reach the hidden home, each one a command that DOES print the
/// secret unsandboxed (the control below), so its silence inside the sandbox is the sandbox's
/// doing and not a broken command.
fn escape_attempts(fixture: &FakeHome) -> Vec<String> {
    let home = fixture.home();
    let home = home.display();
    vec![
        format!("cat '{home}/.secret/token'"),
        "cat \"$HOME/.secret/token\"".to_string(),
        format!("cd '{home}' && cat .secret/token"),
        format!("env -i /bin/sh -c 'cat {home}/.secret/token'"),
        format!("bash --norc --noprofile -c 'cat {home}/.secret/token'"),
        format!("cat '{home}/.ssh/id_ed25519'"),
        format!("find '{home}' -name token -exec cat {{}} \\;"),
    ]
}

/// The fake home, secret directory included, is invisible to every command the component runs:
/// the same commands reach the secret through an unsandboxed service (the control).
#[tokio::test]
async fn the_hidden_home_and_its_secret_directory_are_invisible() {
    require_bwrap!();
    within_deadline(
        "the_hidden_home_and_its_secret_directory_are_invisible",
        async {
            let fixture = FakeHome::new();
            let tool = shell_tool(
                sandboxed_service(&fixture, snapshot(&fixture.home()), Vec::new(), Vec::new()),
                &Arc::new(MaskCounter::new()),
            );
            let control = shell_tool(
                ProcessService::new(&fixture.workspace)
                    .with_env_snapshot(snapshot(&fixture.home())),
                &Arc::new(MaskCounter::new()),
            );

            for command in escape_attempts(&fixture) {
                let open = execute(&*control, &shell_call(&command, false)).await;
                assert!(
                    open.content.contains(HOME_SECRET),
                    "control {command:?} must reach the secret unsandboxed: {open:?}"
                );

                let outcome = execute(&*tool, &shell_call(&command, false)).await;
                assert_eq!(outcome.status, ToolStatus::Ok, "{command:?}: {outcome:?}");
                assert!(
                    !outcome.content.contains(HOME_SECRET),
                    "{command:?} uncovered the hidden home: {outcome:?}"
                );
            }
        },
    )
    .await;
}

/// A write outside the workspace fails and creates nothing: the workspace's parent is the
/// hidden home, which the sandbox remounts read-only, and `$HOME` is the same directory.
#[tokio::test]
async fn a_write_outside_the_workspace_fails_and_creates_nothing() {
    require_bwrap!();
    within_deadline(
        "a_write_outside_the_workspace_fails_and_creates_nothing",
        async {
            let fixture = FakeHome::new();
            let tool = shell_tool(
                sandboxed_service(&fixture, snapshot(&fixture.home()), Vec::new(), Vec::new()),
                &Arc::new(MaskCounter::new()),
            );

            for command in [
                "echo x > ../outside.txt".to_string(),
                "echo x > \"$HOME/outside.txt\"".to_string(),
            ] {
                let outcome = execute(&*tool, &shell_call(&command, false)).await;
                assert!(
                    !exited_zero(&outcome),
                    "{command:?} must fail outside the workspace: {outcome:?}"
                );
            }
            assert!(!fixture.path().join("outside.txt").exists());
            assert!(!fixture.home().join("outside.txt").exists());
        },
    )
    .await;
}

// --------------------------------------------------------- case 3: the bind order and masks

/// The argument vector exactly as `crates/p1-tool-shell/tests/sandbox.rs` asserts it: `/tmp`
/// before the home, the visible entries and then the read-only `readable` paths right after the
/// home `tmpfs`, the runtime dir after those, the token masks AFTER every writable bind, the
/// workspace last before `--remount-ro`. Copied, not weakened: the order is the contract.
fn expected_args(
    home: &Path,
    workspace: &Path,
    tmp: &Path,
    readable: &[&Path],
    writable: &[&Path],
    runtime_dir: Option<&Path>,
) -> Vec<String> {
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
    for path in readable {
        if path.exists() {
            args.extend([
                "--ro-bind".into(),
                path.display().to_string(),
                path.display().to_string(),
            ]);
        }
    }
    if let Some(runtime_dir) = runtime_dir
        && runtime_dir.exists()
    {
        args.extend(["--tmpfs".into(), runtime_dir.display().to_string()]);
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

/// `bwrap_args` is pure: `OsString` there, plain `String` here, for comparison.
fn os_args(args: &[OsString]) -> Vec<String> {
    args.iter()
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect()
}

/// A workspace under `/tmp` (the common `tempfile` case) with a writable path, a nonexistent
/// writable path, a readable path and a runtime dir.
#[test]
fn bwrap_args_order_for_a_workspace_under_tmp() {
    require_bwrap!();
    let home = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(home.path().join(".cargo")).unwrap();
    std::fs::write(home.path().join(".cargo/credentials.toml"), "token").unwrap();
    let workspace = home.path().join("ws");
    std::fs::create_dir_all(&workspace).unwrap();
    let extra = tempfile::tempdir().unwrap();
    let private_tmp = tempfile::tempdir().unwrap();
    let runtime_dir = tempfile::tempdir().unwrap();
    let missing = home.path().join("does-not-exist");
    let readable = home.path().join("shared");
    std::fs::create_dir_all(&readable).unwrap();
    let missing_readable = home.path().join("no-shared");

    let mut sandbox = Sandbox::for_home(home.path());
    sandbox.writable = vec![extra.path().to_path_buf(), missing.clone()];
    sandbox.readable = vec![readable.clone(), missing_readable.clone()];
    sandbox.runtime_dir = Some(runtime_dir.path().to_path_buf());
    let args = bwrap_args(&sandbox, &workspace, private_tmp.path());
    let expected = expected_args(
        home.path(),
        &workspace,
        private_tmp.path(),
        &[&readable, &missing_readable],
        &[extra.path(), &missing],
        Some(runtime_dir.path()),
    );

    assert_eq!(os_args(&args), expected);
}

/// A workspace under the home: the workspace bind must come after the home's `tmpfs` and
/// before `--remount-ro`.
#[test]
fn bwrap_args_order_for_a_workspace_under_the_home() {
    require_bwrap!();
    let home = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(home.path().join(".gitconfig")).unwrap();
    let workspace = home.path().join("nested/ws");
    std::fs::create_dir_all(&workspace).unwrap();
    let readable = home.path().join("shared");
    std::fs::create_dir_all(&readable).unwrap();
    let private_tmp = tempfile::tempdir().unwrap();

    let mut sandbox = Sandbox::for_home(home.path());
    sandbox.readable = vec![readable.clone()];
    let args = bwrap_args(&sandbox, &workspace, private_tmp.path());
    let expected = expected_args(
        home.path(),
        &workspace,
        private_tmp.path(),
        &[&readable],
        &[],
        None,
    );

    assert_eq!(os_args(&args), expected);
}

/// The token mask, at runtime and through the component: a writable `.cargo` bind must not
/// uncover the `/dev/null` mask the mount order puts after it.
#[tokio::test]
async fn the_cargo_token_mask_survives_a_writable_bind_from_the_component() {
    require_bwrap!();
    within_deadline(
        "the_cargo_token_mask_survives_a_writable_bind_from_the_component",
        async {
            const TOKEN_CANARY: &str = "TOKEN-CANARY";
            let home = tempfile::tempdir().unwrap();
            let cargo = home.path().join(".cargo");
            std::fs::create_dir_all(cargo.join("bin")).unwrap();
            std::fs::write(cargo.join("credentials.toml"), TOKEN_CANARY).unwrap();
            let workspace = home.path().join("ws");
            std::fs::create_dir_all(&workspace).unwrap();
            let canonical = home.path().canonicalize().unwrap();

            let mut sandbox = Sandbox::for_home(&canonical);
            sandbox.writable = vec![canonical.join(".cargo")];
            let service = ProcessService::new(&workspace)
                .with_env_snapshot(snapshot(&canonical))
                .sandboxed(sandbox)
                .expect("the bwrap probe must succeed once bwrap_usable() is true");
            let tool = shell_tool(service, &Arc::new(MaskCounter::new()));
            // The control: without the sandbox the same read reaches the token, so its absence
            // below is the mask's doing and not a broken command.
            let control = shell_tool(
                ProcessService::new(&workspace).with_env_snapshot(snapshot(&canonical)),
                &Arc::new(MaskCounter::new()),
            );
            let read_command = "cat \"$HOME/.cargo/credentials.toml\"";
            let open = execute(&*control, &shell_call(read_command, false)).await;
            assert!(
                open.content.contains(TOKEN_CANARY),
                "the control must read the token: {open:?}"
            );

            let read = execute(&*tool, &shell_call(read_command, false)).await;
            assert!(
                !read.content.contains(TOKEN_CANARY),
                "the token leaked: {read:?}"
            );

            // The rest of the writable directory really is writable.
            let touch = execute(&*tool, &shell_call("touch \"$HOME/.cargo/ok\"", false)).await;
            assert!(exited_zero(&touch), "{touch:?}");
            assert!(cargo.join("ok").exists());
        },
    )
    .await;
}

// ----------------------------------------------------------- case 4: the environment policy

const CANARY_TOKEN: &str = "CANARY_TOKEN";
const CANARY_VALUE: &str = "secret-1";
const SSH_AUTH_SOCK: &str = "SSH_AUTH_SOCK";
const MY_TOOL_HOME: &str = "MY_TOOL_HOME";
const MY_TOOL_HOME_VALUE: &str = "/opt/t";

/// The host's allow-list applies to whatever the component asks: a snapshot secret is absent
/// inside the command, an allow-listed name and an `--env-pass` name are present, and the
/// name stays out without the pass.
#[tokio::test]
async fn the_environment_policy_applies_through_the_component() {
    require_bwrap!();
    within_deadline(
        "the_environment_policy_applies_through_the_component",
        async {
            let fixture = FakeHome::new();
            let mut snap = snapshot(&fixture.home());
            snap.extend([
                (OsString::from(CANARY_TOKEN), OsString::from(CANARY_VALUE)),
                (OsString::from(SSH_AUTH_SOCK), OsString::from("/x")),
                (
                    OsString::from(MY_TOOL_HOME),
                    OsString::from(MY_TOOL_HOME_VALUE),
                ),
            ]);

            let without = shell_tool(
                sandboxed_service(&fixture, snap.clone(), Vec::new(), Vec::new()),
                &Arc::new(MaskCounter::new()),
            );
            let passed = shell_tool(
                sandboxed_service(&fixture, snap, vec![MY_TOOL_HOME.to_string()], Vec::new()),
                &Arc::new(MaskCounter::new()),
            );

            let outcome = execute(&*without, &shell_call("env", false)).await;
            assert_eq!(outcome.status, ToolStatus::Ok, "{outcome:?}");
            assert!(value(&outcome.content, "PATH").is_some(), "{outcome:?}");
            assert_eq!(
                value(&outcome.content, "LC_ALL"),
                Some("C"),
                "the allow-list keeps LC_ALL: {outcome:?}"
            );
            assert_eq!(
                value(&outcome.content, "TMPDIR"),
                Some("/tmp"),
                "bubblewrap still sets TMPDIR after the allow-list: {outcome:?}"
            );
            assert_eq!(
                value(&outcome.content, CANARY_TOKEN),
                None,
                "the snapshot secret leaked: {outcome:?}"
            );
            assert_eq!(
                value(&outcome.content, SSH_AUTH_SOCK),
                None,
                "the agent socket leaked: {outcome:?}"
            );
            assert_eq!(
                value(&outcome.content, MY_TOOL_HOME),
                None,
                "MY_TOOL_HOME needs --env-pass: {outcome:?}"
            );

            let outcome = execute(&*passed, &shell_call("env", false)).await;
            assert_eq!(
                value(&outcome.content, MY_TOOL_HOME),
                Some(MY_TOOL_HOME_VALUE),
                "the passed name must reach the command: {outcome:?}"
            );
            assert_eq!(
                value(&outcome.content, CANARY_TOKEN),
                None,
                "the pass-list must not widen the rest: {outcome:?}"
            );
        },
    )
    .await;
}

// ------------------------------------------------- case 5: the component cannot pick a host

/// The marker this case's own command text carries: the sandbox's init argv contains it, the
/// host's init does not.
const PID_NAMESPACE_MARKER: &str = "p1-shell-boundary-pidns";

/// Whatever the component asks, it cannot leave the sandbox: `process.command` carries only a
/// script and a time limit, and the host's own view (a read-only `/`, a private PID namespace)
/// is what the command sees.
#[tokio::test]
async fn the_component_cannot_ask_the_host_out_of_the_sandbox() {
    require_bwrap!();
    within_deadline(
        "the_component_cannot_ask_the_host_out_of_the_sandbox",
        async {
            // `process.command` has exactly two fields, and this exhaustive destructuring stops
            // compiling if the record ever grows one a component could use to name a program,
            // an environment, a working directory or a sandbox (`modules/wit/process.wit`).
            let ProcessCommand { script, timeout_ms } = ProcessCommand {
                script: String::new(),
                timeout_ms: 0,
            };
            let _ = (script, timeout_ms);

            let fixture = FakeHome::new();
            let tool = shell_tool(
                sandboxed_service(&fixture, snapshot(&fixture.home()), Vec::new(), Vec::new()),
                &Arc::new(MaskCounter::new()),
            );

            // The real home lives on the read-only root: the write fails and creates nothing.
            let outside = std::env::var_os("HOME")
                .map(PathBuf::from)
                .filter(|home| home.is_dir())
                .map(|home| home.join("p1-shell-boundary-probe"))
                // A host with no usable HOME still has a read-only `/` to refuse.
                .unwrap_or_else(|| PathBuf::from("/p1-shell-boundary-probe"));
            let write = execute(
                &*tool,
                &shell_call(&format!("echo x > '{}'", outside.display()), false),
            )
            .await;
            assert!(
                !exited_zero(&write),
                "a write to the real home must fail: {write:?}"
            );
            assert!(
                write.content.contains("Read-only file system"),
                "the failure must be the read-only root: {write:?}"
            );
            assert!(!outside.exists(), "the write must have created nothing");

            // `--unshare-pid`: `/proc` inside shows the sandbox's own namespace, so pid 1 is the
            // sandbox's init — a process running this very command, marker and all — and never
            // the host's.
            let command = format!("tr '\\0' ' ' < /proc/1/cmdline   # {PID_NAMESPACE_MARKER}");
            let view = execute(&*tool, &shell_call(&command, false)).await;
            assert!(
                view.content.contains(PID_NAMESPACE_MARKER),
                "pid 1 inside must be the sandbox's own process: {view:?}"
            );
            let host_init =
                std::fs::read_to_string("/proc/1/comm").expect("the host's pid 1 must be readable");
            let host_init = host_init.trim().to_owned();
            assert!(!host_init.is_empty(), "the host's pid 1 must have a name");
            assert!(
                !view.content.contains(&host_init),
                "the host's init {host_init:?} must not be visible inside: {view:?}"
            );
        },
    )
    .await;
}

// -------------------------------------------------------- case 6: byte-for-byte output parity

/// Last line of a summarised result, before the exit-code footer: the guest's filter marker.
const FILTER_MARKER: &str = "[output filtered; pass raw:true for the full log]";

const CARGO_TEST_PASS: &str =
    include_str!("../../p1-tool-shell/tests/fixtures/filter_corpus/cargo-test-pass.txt");
const CARGO_TEST_MULTI_PASS: &str =
    include_str!("../../p1-tool-shell/tests/fixtures/filter_corpus/cargo-test-multi-pass.txt");
const CARGO_TEST_FAIL: &str =
    include_str!("../../p1-tool-shell/tests/fixtures/filter_corpus/cargo-test-fail.txt");
const CARGO_BUILD_ERROR: &str =
    include_str!("../../p1-tool-shell/tests/fixtures/filter_corpus/cargo-build-error.txt");
const CARGO_BUILD_PASS: &str =
    include_str!("../../p1-tool-shell/tests/fixtures/filter_corpus/cargo-build-pass.txt");
const CARGO_CHECK_WARN: &str =
    include_str!("../../p1-tool-shell/tests/fixtures/filter_corpus/cargo-check-warn.txt");
const GIT_STATUS: &str =
    include_str!("../../p1-tool-shell/tests/fixtures/filter_corpus/git-status.txt");
const GIT_DIFF: &str =
    include_str!("../../p1-tool-shell/tests/fixtures/filter_corpus/git-diff.txt");
const GIT_DIFF_LOCKFILE: &str =
    include_str!("../../p1-tool-shell/tests/fixtures/filter_corpus/git-diff-lockfile.txt");
const GIT_LOG: &str = include_str!("../../p1-tool-shell/tests/fixtures/filter_corpus/git-log.txt");
const GIT_LOG_ONELINE: &str =
    include_str!("../../p1-tool-shell/tests/fixtures/filter_corpus/git-log-oneline.txt");
const NPM_TEST_PASS: &str =
    include_str!("../../p1-tool-shell/tests/fixtures/filter_corpus/npm-test-pass.txt");
const NPM_TEST_FAIL: &str =
    include_str!("../../p1-tool-shell/tests/fixtures/filter_corpus/npm-test-fail.txt");
const VITEST_PASS: &str =
    include_str!("../../p1-tool-shell/tests/fixtures/filter_corpus/vitest-pass.txt");
const VITEST_FAIL: &str =
    include_str!("../../p1-tool-shell/tests/fixtures/filter_corpus/vitest-fail.txt");
const NPM_INSTALL: &str =
    include_str!("../../p1-tool-shell/tests/fixtures/filter_corpus/npm-install.txt");
const SHELLCHECK: &str =
    include_str!("../../p1-tool-shell/tests/fixtures/filter_corpus/shellcheck.txt");

/// One captured corpus case, exactly as `crates/p1-tool-shell/tests/output_filters.rs` drives
/// it: the command the model sends, the fake program it must resolve to, the captured bytes,
/// the exit code, and whether no filter covers the class.
struct Sample {
    class: &'static str,
    command: &'static str,
    program: &'static str,
    raw: &'static str,
    code: i32,
    passthrough: bool,
}

fn samples() -> Vec<Sample> {
    vec![
        Sample {
            class: "cargo test (pass)",
            command: "cargo test",
            program: "cargo",
            raw: CARGO_TEST_PASS,
            code: 0,
            passthrough: false,
        },
        Sample {
            class: "cargo test (pass, workspace)",
            command: "cargo test",
            program: "cargo",
            raw: CARGO_TEST_MULTI_PASS,
            code: 0,
            passthrough: false,
        },
        Sample {
            class: "cargo test (fail)",
            command: "cargo test",
            program: "cargo",
            raw: CARGO_TEST_FAIL,
            code: 101,
            passthrough: false,
        },
        Sample {
            class: "cargo build (compile error)",
            command: "cargo build",
            program: "cargo",
            raw: CARGO_BUILD_ERROR,
            code: 101,
            passthrough: false,
        },
        Sample {
            class: "cargo build (pass)",
            command: "cargo build",
            program: "cargo",
            raw: CARGO_BUILD_PASS,
            code: 0,
            passthrough: false,
        },
        Sample {
            class: "cargo check (warnings)",
            command: "cargo check",
            program: "cargo",
            raw: CARGO_CHECK_WARN,
            code: 0,
            passthrough: false,
        },
        Sample {
            class: "git status",
            command: "git status",
            program: "git",
            raw: GIT_STATUS,
            code: 0,
            passthrough: false,
        },
        Sample {
            class: "git diff (source-heavy)",
            command: "git diff HEAD~2 HEAD~1",
            program: "git",
            raw: GIT_DIFF,
            code: 0,
            passthrough: false,
        },
        Sample {
            class: "git diff (lockfile churn)",
            command: "git diff",
            program: "git",
            raw: GIT_DIFF_LOCKFILE,
            code: 0,
            passthrough: false,
        },
        Sample {
            class: "git log",
            command: "git log -n 12",
            program: "git",
            raw: GIT_LOG,
            code: 0,
            passthrough: false,
        },
        Sample {
            class: "git log --oneline",
            command: "git log --oneline -n 12",
            program: "git",
            raw: GIT_LOG_ONELINE,
            code: 0,
            passthrough: true,
        },
        Sample {
            class: "npm test (pass)",
            command: "npm test -- --verbose",
            program: "npm",
            raw: NPM_TEST_PASS,
            code: 0,
            passthrough: false,
        },
        Sample {
            class: "npm test (fail)",
            command: "npm test",
            program: "npm",
            raw: NPM_TEST_FAIL,
            code: 1,
            passthrough: false,
        },
        Sample {
            class: "vitest (pass)",
            command: "npx vitest run",
            program: "npx",
            raw: VITEST_PASS,
            code: 0,
            passthrough: false,
        },
        Sample {
            class: "vitest (fail)",
            command: "npx vitest run",
            program: "npx",
            raw: VITEST_FAIL,
            code: 1,
            passthrough: false,
        },
        Sample {
            class: "npm install (installer log)",
            command: "npm install gulp@4 request@2 bower@1",
            program: "npm",
            raw: NPM_INSTALL,
            code: 0,
            passthrough: true,
        },
        Sample {
            class: "shellcheck (linter)",
            command: "shellcheck /tmp/fixgen/sh/deploy.sh",
            program: "shellcheck",
            raw: SHELLCHECK,
            code: 1,
            passthrough: true,
        },
    ]
}

/// One parity run: the class name for failure messages, the fake program and the bytes it
/// replays, and the command and `raw` flag the model sends.
struct Case<'a> {
    class: &'a str,
    program: &'a str,
    output: &'a str,
    code: i32,
    command: &'a str,
    raw: bool,
    passthrough: bool,
}

/// The two tools the parity case compares, over one sandbox: the native `ShellTool` and the
/// loaded component. Both run from the same workspace with the same environment snapshot.
struct Parity {
    /// Held so the fake home and the workspace inside it live as long as the tools.
    _home: TempDir,
    bin: PathBuf,
    native: ShellTool,
    component: Arc<dyn Tool>,
}

impl Parity {
    fn new() -> Self {
        let home = tempfile::tempdir().expect("temp dir");
        let workspace = home.path().join("ws");
        let bin = workspace.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let canonical = home.path().canonicalize().unwrap();
        let path = format!("{}:{SYSTEM_PATH}", bin.display());
        // HOME is the hidden fake home: `bash -lc` is a login shell, and a real home would
        // source the machine's profile into the captured output.
        let env = vec![
            (OsString::from("PATH"), OsString::from(&path)),
            (OsString::from("HOME"), canonical.clone().into_os_string()),
            (OsString::from("LC_ALL"), OsString::from("C")),
        ];
        let native = ShellTool::new(Workspace::new(&workspace).unwrap())
            .with_env_snapshot(env.clone())
            .sandboxed(Sandbox::for_home(&canonical))
            .expect("the bwrap probe must succeed once bwrap_usable() is true");
        let service = ProcessService::new(&workspace)
            .with_env_snapshot(env)
            .sandboxed(Sandbox::for_home(&canonical))
            .expect("the bwrap probe must succeed once bwrap_usable() is true");
        let component = shell_tool(service, &Arc::new(MaskCounter::new()));
        Self {
            _home: home,
            bin,
            native,
            component,
        }
    }

    /// Install `program` so it prints `output` byte for byte and exits `code` (the same replay
    /// mechanism `output_filters.rs` uses: the captured bytes are data, never code).
    fn replay(&self, program: &str, output: &str, code: i32) {
        let data = self.bin.join(format!("{program}.out"));
        std::fs::write(&data, output).unwrap();
        let script = format!("#!/bin/sh\ncat '{}'\nexit {code}\n", data.display());
        let path = self.bin.join(program);
        std::fs::write(&path, script).unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    /// The `PATH` the fake programs are found through: the replay directory first, then the
    /// system half (`output_filters.rs` builds the same value).
    fn path(&self) -> String {
        format!("{}:{SYSTEM_PATH}", self.bin.display())
    }

    /// Run one case through both tools and require the component's outcome to equal the native
    /// tool's byte for byte, in status and content.
    async fn compare(&self, case: Case<'_>) {
        let Case {
            class,
            program,
            output,
            code,
            command,
            raw,
            passthrough,
        } = case;
        self.replay(program, output, code);
        // The fake programs come first on `PATH`, through the environment snapshot and the
        // command itself: a login shell may reset `PATH` from the system profile.
        let command = format!("PATH={} {command}", self.path());
        let call = shell_call(&command, raw);
        let native = self
            .native
            .execute(
                &call,
                ToolContext {
                    cancel: CancellationToken::new(),
                },
            )
            .await;
        let component = execute(&*self.component, &call).await;

        assert_eq!(
            component.status, native.status,
            "[{class}] raw={raw}: the status differs"
        );
        assert_eq!(
            component.content, native.content,
            "[{class}] raw={raw}: the content differs"
        );
        if !raw {
            assert_eq!(
                component.content.contains(FILTER_MARKER),
                !passthrough,
                "[{class}] the filter marker must be present exactly when a filter applied: \
                 {component:?}"
            );
        }
    }
}

/// D-XO-8 bound 4: the shell component is the same pure guest behaviour the native tool ships,
/// so over the same corpus fixtures and the same fake binaries the component's outcome content
/// and status equal the native `ShellTool`'s byte for byte, filtered and raw.
#[tokio::test]
async fn the_filter_corpus_is_byte_for_byte_parity_with_the_native_tool() {
    require_bwrap!();
    within_deadline(
        "the_filter_corpus_is_byte_for_byte_parity_with_the_native_tool",
        async {
            let parity = Parity::new();

            for sample in samples() {
                for raw in [false, true] {
                    parity
                        .compare(Case {
                            class: sample.class,
                            program: sample.program,
                            output: sample.raw,
                            code: sample.code,
                            command: sample.command,
                            raw,
                            passthrough: sample.passthrough,
                        })
                        .await;
                }
            }

            // The fail-safe case the corpus suite also drives: garbage under a recognised
            // command passes through, byte for byte, on both tools.
            for (class, program, command) in [
                ("cargo test", "cargo", "cargo test"),
                ("cargo build", "cargo", "cargo build"),
                ("git status", "git", "git status"),
                ("git log", "git", "git log"),
                ("git diff", "git", "git diff"),
                ("npm test", "npm", "npm test"),
            ] {
                parity
                    .compare(Case {
                        class,
                        program,
                        output: "complete garbage output\n",
                        code: 0,
                        command,
                        raw: false,
                        passthrough: true,
                    })
                    .await;
            }
        },
    )
    .await;
}

// ------------------------------------------ case 8: masking over the loaded shell component

/// A credential-shaped value built at runtime: never a literal in the tree (the secret scan
/// refuses one).
fn key() -> String {
    format!("sk-{}", "a".repeat(24))
}

/// The masking layer the host wraps every assembled tool in must cover the shell component too:
/// the outcome content, the result description's summary and tail, and the call's `describe`
/// target when the command text itself carries the value.
#[tokio::test]
async fn the_component_cannot_smuggle_a_credential_shape_past_the_mask() {
    require_bwrap!();
    within_deadline(
        "the_component_cannot_smuggle_a_credential_shape_past_the_mask",
        async {
            let fixture = FakeHome::new();
            let counter = Arc::new(MaskCounter::new());
            let tool = shell_tool(
                sandboxed_service(&fixture, snapshot(&fixture.home()), Vec::new(), Vec::new()),
                &counter,
            );
            let secret = key();
            let mask = "<redacted:sk-:24 chars>";

            // (a) The command builds the value at runtime, so it is the outcome that carries it.
            let build = "printf 'sk-%s' \"$(printf 'a%.0s' $(seq 24))\"";
            let outcome = execute(&*tool, &shell_call(build, false)).await;
            assert_eq!(outcome.status, ToolStatus::Ok, "{outcome:?}");
            assert!(!outcome.content.contains(&secret), "{outcome:?}");
            assert_eq!(outcome.content, format!("{mask}\n[exit code: 0]"));
            assert!(counter.take() > 0);

            // (b) The component's own `describe_result` runs on the content the host stored —
            // the raw capture, key and all — so the decorator is what must mask the summary and
            // the detail's tail.
            let result = ToolResultItem {
                call_id: "shell-boundary".to_owned(),
                name: "shell".to_owned(),
                status: ToolStatus::Ok,
                content: format!("{secret}\n[exit code: 0]"),
            };
            let described = tool.describe_result(&shell_call(build, false), &result);
            assert!(
                !described.summary.contains(&secret),
                "{}",
                described.summary
            );
            let Some(ResultDetail::Command { tail, .. }) = described.detail else {
                panic!("the shell component describes a command detail: {described:?}");
            };
            assert_eq!(tail, vec![mask.to_string()]);
            assert!(counter.take() > 0);

            // (c) A command whose own text carries the value: the call's `describe` target is
            // masked.
            let described = tool.describe(&shell_call(&format!("echo {secret}"), false));
            let target = described
                .target
                .expect("the shell component describes the command's first line");
            assert!(!target.contains(&secret), "{target}");
            assert_eq!(target, format!("echo {mask}"));
            assert!(counter.take() > 0);
        },
    )
    .await;
}
