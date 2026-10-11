//! #77 (ADR-0173): the shipped `p1/shell` component and long builds.
//!
//! One build per session: a `cargo` or `scripts/gate.sh` call while one of the session's
//! background jobs still runs a build is refused at once and names that job; a command that is
//! not a build still runs, and once the job ended the next build runs. And a foreground build
//! waits on its own `timeout_seconds`, not on the module's default 300 s call deadline, when
//! the host gives the shell the call deadline it assembles it with.
//!
//! The component is loaded by name from the package `scripts/build-modules.sh` built and linked
//! as the host links it (`crates/p1-host/src/catalog/tools.rs`): a process capability that hands
//! a timed-out command over to the session's job registry, and that registry as its
//! `process-jobs`. No sandbox, no network, tempdirs only; the fake `cargo` ends when the test
//! writes its release fifo and the module clock moves only when the test advances it.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use p1_contracts::serde_json::{self, Value, json};
use p1_contracts::{
    CancellationToken, Tool, ToolCall, ToolContext, ToolInput, ToolOutcome, ToolStatus,
};
use p1_module_runtime::jobs::{JobFinished, JobObserver, JobRegistry};
use p1_module_runtime::loader::EPOCH_TICK;
use p1_module_runtime::process::{ProcessCapability, ProcessService};
use p1_module_runtime::{
    CallOutputs, ExecutionLimits, ExitStatus, Loader, ManualEpochs, OutputStore, ReleaseManifest,
    Services, wasm_tool,
};
use p1_redact::MaskCounter;
use tempfile::TempDir;

const SHELL: (&str, &str) = ("p1-module-shell", "p1/shell");

fn built_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../modules/target/p1-modules")
}

/// The release of the one built shell package, laid out as a release ships it.
fn release() -> ReleaseManifest {
    let (package, _) = SHELL;
    let path = built_dir()
        .join(package)
        .join(format!("{package}.manifest.json"));
    let text = std::fs::read_to_string(&path).unwrap_or_else(|error| {
        panic!(
            "{} is missing ({error}): run scripts/build-modules.sh first",
            path.display()
        )
    });
    let manifest: Value = serde_json::from_str(&text).expect("package manifest");
    let release = json!({
        "format": "p1-release-manifest/1",
        "components": [{
            "name": manifest["name"],
            "digest": manifest["digest"],
            "path": format!("{package}/{package}.wasm"),
            "kind": manifest["kind"],
            "world": manifest["world"],
            "protocol": manifest["protocol"],
            "capabilities": manifest["capabilities"],
            "variant": manifest["variant"],
        }],
    });
    ReleaseManifest::parse(&release.to_string()).expect("release manifest")
}

/// Sends every job end to the test, so it waits on the end itself.
struct Ends(tokio::sync::mpsc::UnboundedSender<JobFinished>);

impl JobObserver for Ends {
    fn started(&self) -> Option<u64> {
        None
    }
    fn ended(&self, _: &str, _: Option<u64>, _: &ExitStatus) {}
    fn finished(&self, job: JobFinished) {
        let _ = self.0.send(job);
    }
}

/// One session: a workspace with a fake `cargo` and a `gate.sh`, its job registry, and the
/// shell component linked over both.
struct Session {
    workspace: TempDir,
    jobs: Arc<JobRegistry>,
    ended: tokio::sync::mpsc::UnboundedReceiver<JobFinished>,
    shell: Arc<dyn Tool>,
}

impl Session {
    fn new(loader: &Loader, limits: ExecutionLimits) -> Self {
        let workspace = tempfile::tempdir().expect("workspace");
        let root = workspace.path();
        // The fake `cargo` announces its start on one fifo and ends on a line from the other.
        let cargo = root.join("cargo");
        std::fs::write(
            &cargo,
            "#!/bin/sh\necho \"cargo $*\" > started\nread line < release\necho \"fake cargo $1 done\"\n",
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&cargo, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::write(root.join("gate.sh"), "echo gate ran\n").unwrap();
        for fifo in ["started", "release"] {
            let path = root.join(fifo);
            let made = std::process::Command::new("mkfifo")
                .arg(&path)
                .status()
                .unwrap();
            assert!(made.success(), "mkfifo {}", path.display());
        }

        let store = Arc::new(OutputStore::in_directory(
            root.join(".outputs"),
            Default::default(),
        ));
        let process = Arc::new(ProcessService::new(root).with_env_snapshot(vec![
            ("PATH".into(), "/usr/bin:/bin".into()),
            ("HOME".into(), root.into()),
        ]));
        let counter = Arc::new(MaskCounter::new());
        let jobs = Arc::new(JobRegistry::new(
            process.clone(),
            store.clone(),
            counter.secrets().clone(),
        ));
        let (ends, ended) = tokio::sync::mpsc::unbounded_channel();
        jobs.observe(Arc::new(Ends(ends)));
        let linked = {
            let jobs = jobs.clone();
            Services::call_scoped(move || {
                let outputs = CallOutputs::new(store.clone(), Default::default());
                Services {
                    process: Some(Arc::new(
                        ProcessCapability::new(process.clone())
                            .storing(outputs.clone())
                            .adopting(jobs.clone()),
                    )),
                    tool_outputs: Some(Arc::new(outputs)),
                    process_jobs: Some(jobs.clone()),
                    ..Services::default()
                }
            })
        };
        let module = loader.load(SHELL.1).expect("p1/shell loads");
        let shell = wasm_tool(&module, linked, limits, &counter).expect("the shell links");
        Self {
            workspace,
            jobs,
            ended,
            shell,
        }
    }

    async fn execute(&self, call_id: &str, input: Value) -> ToolOutcome {
        let call = ToolCall {
            call_id: call_id.into(),
            name: "shell".into(),
            input: ToolInput::Json(input.to_string()),
        };
        self.shell
            .execute(
                &call,
                ToolContext {
                    cancel: CancellationToken::new(),
                },
            )
            .await
    }

    /// Starts `input` on its own task, as the core runs a tool call.
    fn start(&self, call_id: &str, input: Value) -> tokio::task::JoinHandle<ToolOutcome> {
        let shell = self.shell.clone();
        let call = ToolCall {
            call_id: call_id.into(),
            name: "shell".into(),
            input: ToolInput::Json(input.to_string()),
        };
        tokio::spawn(async move {
            shell
                .execute(
                    &call,
                    ToolContext {
                        cancel: CancellationToken::new(),
                    },
                )
                .await
        })
    }

    /// Waits until the fake `cargo` runs: opening the fifo for reading blocks until then.
    async fn cargo_started(&self) -> String {
        let announce = self.workspace.path().join("started");
        tokio::task::spawn_blocking(move || std::fs::read_to_string(announce))
            .await
            .unwrap()
            .unwrap()
    }

    /// Lets the fake `cargo` end.
    async fn release_cargo(&self) {
        let release = self.workspace.path().join("release");
        tokio::task::spawn_blocking(move || std::fs::write(release, "go\n"))
            .await
            .unwrap()
            .unwrap();
    }
}

fn loader() -> Loader {
    Loader::new(release(), built_dir()).expect("loader")
}

#[tokio::test(flavor = "multi_thread")]
async fn a_build_while_a_background_build_runs_is_refused_naming_the_job() {
    let mut session = Session::new(&loader(), ExecutionLimits::default());

    let started = session
        .execute(
            "c1",
            json!({ "command": "./cargo build --workspace", "background": true }),
        )
        .await;
    assert_eq!(started.status, ToolStatus::Ok, "{}", started.content);
    assert!(
        started.content.starts_with("Background job j1 started."),
        "{}",
        started.content
    );
    assert_eq!(session.cargo_started().await, "cargo build --workspace\n");

    for (call_id, input) in [
        ("c2", json!({ "command": "./cargo test 2>&1 | tail -40" })),
        (
            "c3",
            json!({ "command": "bash ./gate.sh", "background": true }),
        ),
    ] {
        let refused = session.execute(call_id, input).await;
        assert_eq!(refused.status, ToolStatus::Error, "{}", refused.content);
        assert_eq!(
            refused.content,
            "Refused: a build is already running in this session: background job j1 \
             (`./cargo build --workspace`) is still running; wait for its completion \
             notification, or check or cancel it with shell_job. Only one `cargo` or \
             `scripts/gate.sh` run at a time: a second one would only wait on the same build lock."
        );
    }
    let beside = session
        .execute("c4", json!({ "command": "echo beside" }))
        .await;
    assert_eq!(beside.status, ToolStatus::Ok);
    assert_eq!(beside.content, "beside\n[exit code: 0]");

    session.release_cargo().await;
    let end = session.ended.recv().await.expect("the job's end");
    assert_eq!(end.id, "j1");
    assert_eq!(end.end.status, ExitStatus::Code(0));

    let after = session
        .execute("c5", json!({ "command": "bash ./gate.sh" }))
        .await;
    assert_eq!(after.status, ToolStatus::Ok, "{}", after.content);
    assert_eq!(after.content, "gate ran\n[exit code: 0]");
    session.jobs.shutdown().await;
}

/// The module clock past 300 s, in ticks.
fn past_the_default_deadline() -> u64 {
    let ticks = (Duration::from_secs(301).as_nanos() / EPOCH_TICK.as_nanos()) as u64;
    assert!(Duration::from_secs(301) > ExecutionLimits::default().deadline);
    ticks
}

fn manual_loader() -> (Loader, ManualEpochs) {
    Loader::with_manual_epochs(release(), built_dir()).expect("loader")
}

/// The control: under the module's default 300 s call deadline a foreground build (its own
/// limit 1 800 s) is cut off once the module clock passes 300 s, and never handed over.
#[tokio::test(flavor = "multi_thread")]
async fn the_default_call_deadline_would_cut_a_foreground_build_short() {
    let (loader, epochs) = manual_loader();
    let session = Session::new(&loader, ExecutionLimits::default());
    let call = session.start("c1", json!({ "command": "./cargo build" }));
    assert_eq!(session.cargo_started().await, "cargo build\n");

    epochs.advance(past_the_default_deadline());
    let outcome = call.await.unwrap();

    assert_eq!(outcome.status, ToolStatus::Error, "{}", outcome.content);
    assert!(
        outcome.content.contains("exceeded its time limit"),
        "{}",
        outcome.content
    );
    session.jobs.shutdown().await;
}

/// With the call deadline the host gives the shell (`shell_limits` in
/// `crates/p1-host/src/catalog/tools.rs`: the longest `timeout_seconds` plus a minute), a
/// foreground build still runs once the module clock passes 300 s, and ends with its own exit.
#[tokio::test(flavor = "multi_thread")]
async fn a_foreground_build_outlives_the_default_call_deadline() {
    let (loader, epochs) = manual_loader();
    let limits = ExecutionLimits {
        deadline: Duration::from_secs(p1_tool_shell::LONGEST_TIMEOUT_SECONDS + 60),
        ..ExecutionLimits::default()
    };
    let session = Session::new(&loader, limits);
    let call = session.start("c1", json!({ "command": "./cargo build" }));
    assert_eq!(session.cargo_started().await, "cargo build\n");

    epochs.advance(past_the_default_deadline());
    session.release_cargo().await;
    let outcome = call.await.unwrap();

    assert_eq!(outcome.status, ToolStatus::Ok, "{}", outcome.content);
    assert!(
        outcome.content.contains("fake cargo build done"),
        "{}",
        outcome.content
    );
    session.jobs.shutdown().await;
}
