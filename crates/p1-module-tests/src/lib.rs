//! The harness of the module execution-model tests: the built fixture component, a release
//! manifest written for it into a temporary directory, a fake `process` service driven by
//! explicit synchronization, and the deadlock guard every case runs under.
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
    Loader, ProcessCommand, ProcessEvent, ProcessService, ReleaseManifest, RunningProcess,
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
        let path = self.dir.path().join("manifest.json");
        let manifest = json!({
            "format": "p1-release-manifest/1",
            "components": self.components,
        });
        std::fs::write(&path, manifest.to_string()).expect("manifest");
        let manifest = ReleaseManifest::read(&path).expect("release manifest");
        Loader::new(manifest, self.dir.path()).expect("loader")
    }
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

/// The test's side of the fake process service.
pub struct FakeProcesses {
    spawned: mpsc::UnboundedReceiver<SpawnedProcess>,
}

impl FakeProcesses {
    /// The next process a module spawned, once it has.
    pub async fn next_spawn(&mut self) -> SpawnedProcess {
        self.spawned
            .recv()
            .await
            .expect("the fake process service is gone")
    }
}

struct FakeProcessService {
    spawned: mpsc::UnboundedSender<SpawnedProcess>,
}

struct FakeRunning {
    events: mpsc::UnboundedReceiver<ProcessEvent>,
    dropped: Option<oneshot::Sender<()>>,
}

/// A `process` service whose every event the test sends explicitly: `next` stays pending
/// until the test sends one, so a case proves the module really waits on an async import.
pub fn fake_processes() -> (Arc<dyn ProcessService>, FakeProcesses) {
    let (spawned, receiver) = mpsc::unbounded_channel();
    (
        Arc::new(FakeProcessService { spawned }),
        FakeProcesses { spawned: receiver },
    )
}

impl ProcessService for FakeProcessService {
    fn spawn(
        &self,
        command: ProcessCommand,
        _cancel: CancellationToken,
    ) -> BoxFuture<'_, Result<Box<dyn RunningProcess>, String>> {
        Box::pin(async move {
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
                dropped: Some(dropped),
            }) as Box<dyn RunningProcess>)
        })
    }
}

impl RunningProcess for FakeRunning {
    fn next(&mut self) -> BoxFuture<'_, Option<ProcessEvent>> {
        Box::pin(async move { self.events.recv().await })
    }
}

impl Drop for FakeRunning {
    fn drop(&mut self) {
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
