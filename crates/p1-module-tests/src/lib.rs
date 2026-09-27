//! The harness of the module execution-model tests: the built fixture component, a release
//! manifest written for it into a temporary directory, a fake `process` service driven by
//! explicit synchronization that records what it sees, manually driven epochs, and the
//! deadlock guard every case runs under. The loader and assembly cases (S1.4) add the
//! `modules.lock` text selecting a release entry ([`lock_text`]) and read the written
//! manifest back through the host ([`Release::manifest_file`]).
//!
//! The fixture is built by `scripts/build-modules.sh` (the gate runs it before the tests).
//! When it is missing the harness fails with that instruction; it never skips a case.

use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc as std_mpsc;
use std::thread;
use std::time::Duration;

use p1_contracts::serde_json::{self, Value, json};
use p1_contracts::{BoxFuture, CancellationToken, ToolCall, ToolInput};
use p1_module_runtime::{
    ExitStatus, Loader, ManualEpochs, ProcessCommand, ProcessEvent, ProcessService,
    ReleaseManifest, RunningProcess,
};
use tokio::sync::{mpsc, oneshot};

/// The fixture's package directory name and manifest name.
pub const FIXTURE_PACKAGE: &str = "p1-module-fixture";
/// The fixture's manifest name.
pub const FIXTURE_NAME: &str = "p1/fixture";

/// How long one case may take before it counts as a deadlock. Far above what a case needs
/// (a component compile in a debug build included), so only a hang reaches it.
pub const DEADLOCK_LIMIT: Duration = Duration::from_secs(180);

/// Where `scripts/build-modules.sh` publishes the fixture package.
pub fn fixture_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../modules/target/p1-modules")
        .join(FIXTURE_PACKAGE)
}

/// The built fixture: its component bytes and its package manifest.
pub struct Fixture {
    /// The `.wasm` component.
    pub wasm: Vec<u8>,
    /// The `.manifest.json` the build wrote.
    pub manifest: Value,
}

/// Reads the built fixture, or fails the case with how to build it.
pub fn fixture() -> Fixture {
    let dir = fixture_dir();
    let wasm_path = dir.join(format!("{FIXTURE_PACKAGE}.wasm"));
    let manifest_path = dir.join(format!("{FIXTURE_PACKAGE}.manifest.json"));
    let missing = |path: &Path, error: std::io::Error| -> ! {
        panic!(
            "the fixture artifact {} is missing ({error}): run scripts/build-modules.sh first",
            path.display()
        )
    };
    let wasm = std::fs::read(&wasm_path).unwrap_or_else(|error| missing(&wasm_path, error));
    let manifest_text = std::fs::read_to_string(&manifest_path)
        .unwrap_or_else(|error| missing(&manifest_path, error));
    let manifest = serde_json::from_str(&manifest_text)
        .unwrap_or_else(|error| panic!("{}: not JSON: {error}", manifest_path.display()));
    Fixture { wasm, manifest }
}

/// A release directory: component files plus the `manifest.json` that lists them, laid out
/// as p1's release archive ships them.
pub struct Release {
    dir: tempfile::TempDir,
    components: Vec<Value>,
    fixture: Fixture,
}

impl Release {
    /// A release with no component yet.
    pub fn empty() -> Self {
        Self {
            dir: tempfile::tempdir().expect("temp dir"),
            components: Vec::new(),
            fixture: fixture(),
        }
    }

    /// A release holding the fixture as the build published it.
    pub fn with_fixture() -> Self {
        let mut release = Self::empty();
        let entry = release.fixture_entry(FIXTURE_NAME);
        let bytes = release.fixture.wasm.clone();
        release.add(entry, &bytes);
        release
    }

    /// The built fixture this release copies from.
    pub fn fixture(&self) -> &Fixture {
        &self.fixture
    }

    /// The release entry of the fixture under manifest name `name`: the package manifest's
    /// frozen fields, its digest and a path of its own inside the release.
    pub fn fixture_entry(&self, name: &str) -> Value {
        let package = &self.fixture.manifest;
        let file = name.replace('/', "-");
        json!({
            "name": name,
            "digest": package["digest"],
            "path": format!("packages/{file}/{file}.wasm"),
            "kind": package["kind"],
            "world": package["world"],
            "protocol": package["protocol"],
            "capabilities": package["capabilities"],
            "variant": package["variant"],
        })
    }

    /// Adds `entry` to the manifest and writes `bytes` at its path.
    pub fn add(&mut self, entry: Value, bytes: &[u8]) {
        let path = self
            .dir
            .path()
            .join(entry["path"].as_str().expect("entry path"));
        std::fs::create_dir_all(path.parent().expect("parent")).expect("package dir");
        std::fs::write(&path, bytes).expect("component file");
        self.components.push(entry);
    }

    /// Writes `manifest.json` and returns a loader over it, read back from the file as the
    /// host reads a release.
    pub fn loader(&self) -> Loader {
        Loader::new(self.manifest(), self.dir.path()).expect("loader")
    }

    /// As [`Release::loader`], with epochs that only the returned [`ManualEpochs`] advance,
    /// so a case drives deadlines explicitly.
    pub fn loader_with_manual_epochs(&self) -> (Loader, ManualEpochs) {
        Loader::with_manual_epochs(self.manifest(), self.dir.path()).expect("loader")
    }

    fn manifest(&self) -> ReleaseManifest {
        let path = self.manifest_file();
        ReleaseManifest::read(&path).expect("release manifest")
    }

    /// Writes `manifest.json` and returns its path, for a caller that reads the release
    /// itself (the host's catalog entry point). Nothing checks the entries here, so a case
    /// can write a release the loader must refuse.
    pub fn manifest_file(&self) -> PathBuf {
        let path = self.dir.path().join("manifest.json");
        let manifest = json!({
            "format": "p1-release-manifest/1",
            "components": self.components,
        });
        std::fs::write(&path, manifest.to_string()).expect("manifest");
        path
    }

    /// The directory the release is laid out in.
    pub fn root(&self) -> &Path {
        self.dir.path()
    }
}

/// The `modules.lock` text resolving module name `module` to release entry `entry`, pinning
/// its package, digest, world and protocol as the release states them.
pub fn lock_text(module: &str, entry: &Value) -> String {
    let field = |key: &str| entry[key].as_str().expect("entry field").to_owned();
    format!(
        "format = \"p1-modules-lock/1\"\n\n[modules.{module}]\npackage = \"{}\"\n\
         version = \"0.0.1\"\ndigest = \"{}\"\nworld = \"{}\"\nprotocol = \"{}\"\n",
        field("name"),
        field("digest"),
        field("world"),
        field("protocol"),
    )
}

/// A freeform call to the fixture with `raw` as its input.
pub fn call(raw: &str) -> ToolCall {
    ToolCall {
        call_id: "c1".to_owned(),
        name: "fixture".to_owned(),
        input: ToolInput::Text(raw.to_owned()),
    }
}

/// A process the fixture spawned through the fake service, as the test drives it.
pub struct SpawnedProcess {
    /// The command the module asked for.
    pub command: ProcessCommand,
    /// Events the module's `next` receives, in order; dropping it ends the stream.
    pub events: mpsc::UnboundedSender<ProcessEvent>,
    /// Fires when the runtime drops the process handle (the kill point of a real service).
    pub dropped: oneshot::Receiver<()>,
}

/// What the fake process service saw and did, in the order it happened.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProcessRecord {
    /// A command was started.
    Spawned(String),
    /// `spawn` found no settlement and waits for the test: the module is blocked in the
    /// import. Only the gated fake ([`gated_processes`]) records this.
    SpawnWaiting,
    /// The command's native effect: a `touch <name>` command created this file in
    /// [`FakeProcesses::markers`] before it started waiting.
    Effect(PathBuf),
    /// `next` found no event and waits for the test: the module is blocked on a host wait.
    NextWaiting,
    /// `next` returned this output chunk.
    Output(Vec<u8>),
    /// `next` returned this exit.
    Exited(ExitStatus),
    /// The process group was killed, by `kill` or by dropping a handle that still ran.
    Killed,
    /// The runtime dropped the process handle.
    Dropped,
}

/// The exit a killed fake process reports, as a real one killed with SIGKILL does.
pub const KILLED_EXIT: ExitStatus = ExitStatus::Signal(9);

/// The test's side of the fake process service.
pub struct FakeProcesses {
    spawned: mpsc::UnboundedReceiver<SpawnedProcess>,
    records: mpsc::UnboundedReceiver<ProcessRecord>,
    seen: Vec<ProcessRecord>,
    markers: tempfile::TempDir,
}

impl FakeProcesses {
    /// The next process a module spawned, once it has.
    pub async fn next_spawn(&mut self) -> SpawnedProcess {
        self.spawned
            .recv()
            .await
            .expect("the fake process service is gone")
    }

    /// Waits until the service records `expected`, and returns every record so far.
    pub async fn wait_for(&mut self, expected: &ProcessRecord) -> &[ProcessRecord] {
        let already = self.seen.iter().position(|record| record == expected);
        if already.is_none() {
            loop {
                let record = self
                    .records
                    .recv()
                    .await
                    .expect("the fake process service is gone");
                let found = &record == expected;
                self.seen.push(record);
                if found {
                    break;
                }
            }
        }
        &self.seen
    }

    /// Every record so far, without waiting.
    pub fn records(&mut self) -> &[ProcessRecord] {
        while let Ok(record) = self.records.try_recv() {
            self.seen.push(record);
        }
        &self.seen
    }

    /// The directory the fake commands' native effects write into.
    pub fn markers(&self) -> &Path {
        self.markers.path()
    }
}

struct FakeProcessService {
    spawned: mpsc::UnboundedSender<SpawnedProcess>,
    records: mpsc::UnboundedSender<ProcessRecord>,
    markers: PathBuf,
    /// Present in the gated fake: `spawn` waits for the test's [`Settlement`] before it
    /// settles the start, so a case can act on a call that is blocked inside the import.
    gate: Option<tokio::sync::Mutex<mpsc::UnboundedReceiver<Settlement>>>,
}

struct FakeRunning {
    events: mpsc::UnboundedReceiver<ProcessEvent>,
    records: mpsc::UnboundedSender<ProcessRecord>,
    dropped: Option<oneshot::Sender<()>>,
    killed: bool,
    exited: bool,
}

/// A `process` service whose every event the test sends explicitly: `next` stays pending
/// until the test sends one, so a case proves the module really waits on an async import.
/// It records what happens ([`ProcessRecord`]) and performs one native effect: a command
/// `touch <name>` creates `<name>` in [`FakeProcesses::markers`] when it starts.
pub fn fake_processes() -> (Arc<dyn ProcessService>, FakeProcesses) {
    let (process, records, processes) = fake_parts();
    (
        Arc::new(FakeProcessService {
            spawned: process,
            records,
            markers: processes.markers().to_owned(),
            gate: None,
        }),
        processes,
    )
}

/// As [`fake_processes`], but its `spawn` waits for the test to settle the start
/// ([`SpawnGate`]): a case can cancel a call while the module is blocked in `process.spawn`.
pub fn gated_processes() -> (Arc<dyn ProcessService>, FakeProcesses, SpawnGate) {
    let (process, records, processes) = fake_parts();
    let (settle, settlement) = mpsc::unbounded_channel();
    (
        Arc::new(FakeProcessService {
            spawned: process,
            records,
            markers: processes.markers().to_owned(),
            gate: Some(tokio::sync::Mutex::new(settlement)),
        }),
        processes,
        SpawnGate { settle },
    )
}

/// The sender halves and the test's side both fakes are built from.
fn fake_parts() -> (
    mpsc::UnboundedSender<SpawnedProcess>,
    mpsc::UnboundedSender<ProcessRecord>,
    FakeProcesses,
) {
    let (spawned, spawned_receiver) = mpsc::unbounded_channel();
    let (records, records_receiver) = mpsc::unbounded_channel();
    let markers = tempfile::tempdir().expect("marker dir");
    (
        spawned,
        records,
        FakeProcesses {
            spawned: spawned_receiver,
            records: records_receiver,
            seen: Vec::new(),
            markers,
        },
    )
}

/// How the test settles a [`gated_processes`] `spawn` that waits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Settlement {
    /// Start the command, as a service does whose start the cancellation did not stop.
    Start,
    /// Refuse it, as a service may that noticed the call's cancellation first.
    Refuse,
}

/// The test's side of [`gated_processes`]: what the blocked `spawn` settles as.
pub struct SpawnGate {
    settle: mpsc::UnboundedSender<Settlement>,
}

impl SpawnGate {
    /// Lets the blocked `spawn` start its command.
    pub fn start(&self) {
        self.settle
            .send(Settlement::Start)
            .expect("the fake process service is gone");
    }

    /// Lets the blocked `spawn` answer `err` without starting anything.
    pub fn refuse(&self) {
        self.settle
            .send(Settlement::Refuse)
            .expect("the fake process service is gone");
    }
}

impl ProcessService for FakeProcessService {
    fn spawn(
        &self,
        command: ProcessCommand,
        _cancel: CancellationToken,
    ) -> BoxFuture<'_, Result<Box<dyn RunningProcess>, String>> {
        Box::pin(async move {
            if let Some(gate) = &self.gate {
                let _ = self.records.send(ProcessRecord::SpawnWaiting);
                let settlement = gate.lock().await.recv().await;
                // A gate the test dropped settles as a refusal, so a case cannot hang here.
                if settlement != Some(Settlement::Start) {
                    return Err("the fake service refused to start a cancelled command".to_owned());
                }
            }
            let _ = self
                .records
                .send(ProcessRecord::Spawned(command.script.clone()));
            if let Some(name) = command.script.strip_prefix("touch ") {
                let marker = self.markers.join(name);
                std::fs::write(&marker, b"").map_err(|error| error.to_string())?;
                let _ = self.records.send(ProcessRecord::Effect(marker));
            }
            let (events, receiver) = mpsc::unbounded_channel();
            let (dropped, dropped_receiver) = oneshot::channel();
            self.spawned
                .send(SpawnedProcess {
                    command,
                    events,
                    dropped: dropped_receiver,
                })
                .map_err(|_| "the test stopped listening".to_owned())?;
            Ok(Box::new(FakeRunning {
                events: receiver,
                records: self.records.clone(),
                dropped: Some(dropped),
                killed: false,
                exited: false,
            }) as Box<dyn RunningProcess>)
        })
    }
}

impl FakeRunning {
    fn deliver(&mut self, event: Option<ProcessEvent>) -> Option<ProcessEvent> {
        let record = match &event {
            Some(ProcessEvent::Output(bytes)) => ProcessRecord::Output(bytes.clone()),
            Some(ProcessEvent::Exited(status)) => {
                self.exited = true;
                ProcessRecord::Exited(*status)
            }
            None => return None,
        };
        let _ = self.records.send(record);
        event
    }
}

impl RunningProcess for FakeRunning {
    fn next(&mut self) -> BoxFuture<'_, Option<ProcessEvent>> {
        Box::pin(async move {
            if self.exited {
                return None;
            }
            let event = match self.events.try_recv() {
                Ok(event) => Some(event),
                // A killed process prints nothing more than what was already queued.
                Err(_) if self.killed => Some(ProcessEvent::Exited(KILLED_EXIT)),
                Err(mpsc::error::TryRecvError::Disconnected) => None,
                Err(mpsc::error::TryRecvError::Empty) => {
                    let _ = self.records.send(ProcessRecord::NextWaiting);
                    self.events.recv().await
                }
            };
            self.deliver(event)
        })
    }

    fn kill(&mut self) -> BoxFuture<'_, ()> {
        Box::pin(async move {
            if !self.killed && !self.exited {
                self.killed = true;
                let _ = self.records.send(ProcessRecord::Killed);
            }
        })
    }
}

impl Drop for FakeRunning {
    fn drop(&mut self) {
        // A real service kills the process group when its handle is dropped while it runs.
        if !self.killed && !self.exited {
            let _ = self.records.send(ProcessRecord::Killed);
        }
        let _ = self.records.send(ProcessRecord::Dropped);
        if let Some(dropped) = self.dropped.take() {
            let _ = dropped.send(());
        }
    }
}

/// Runs `body` as case `case`, failing it as a deadlock if it does not finish within
/// [`DEADLOCK_LIMIT`]. Two guards, because a deadlock takes two shapes: a future that never
/// wakes (the Tokio timeout catches it) and a thread blocked outright, which on a
/// current-thread runtime stops the timeout from ever running — a watchdog thread catches
/// that one and aborts the test binary with the case named.
pub async fn within_deadline<F: Future>(case: &str, body: F) -> F::Output {
    let _watchdog = Watchdog::arm(case);
    match tokio::time::timeout(DEADLOCK_LIMIT, body).await {
        Ok(output) => output,
        Err(_) => panic!("deadlock: {case} did not finish within {DEADLOCK_LIMIT:?}"),
    }
}

struct Watchdog {
    done: std_mpsc::Sender<()>,
}

impl Watchdog {
    fn arm(case: &str) -> Self {
        let (done, finished) = std_mpsc::channel::<()>();
        let case = case.to_owned();
        // Longer than the Tokio timeout, so a case whose runtime still runs fails by name
        // through the panic above rather than through the abort.
        let limit = DEADLOCK_LIMIT + Duration::from_secs(30);
        thread::spawn(move || {
            if let Err(std_mpsc::RecvTimeoutError::Timeout) = finished.recv_timeout(limit) {
                eprintln!("deadlock: {case} blocked its thread for {limit:?}");
                std::process::abort();
            }
        });
        Self { done }
    }
}

impl Drop for Watchdog {
    fn drop(&mut self) {
        let _ = self.done.send(());
    }
}
