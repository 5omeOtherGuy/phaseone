//! The harness of the module execution-model tests: the built fixture component, a release
//! manifest written for it into a temporary directory, a fake `process` service driven by
//! explicit synchronization that records what it sees, manually driven epochs, and the
//! deadlock guard every case runs under. The loader and assembly cases (S1.4) add the
//! `modules.lock` text selecting a release entry ([`lock_text`]) and read the written
//! manifest back through the host ([`Release::manifest_file`]).
//!
//! The fixture is built by `scripts/build-modules.sh` (the gate runs it before the tests).
//! When it is missing the harness fails with that instruction; it never skips a case.

use std::collections::HashSet;
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

// ---------------------------------------------------------------- install identity

/// Whether the `p1` binary at `candidate` names the revision this checkout is at.
///
/// An mtime is not build identity: a binary carried over from another checkout, or from an
/// earlier commit in a shared target directory, can be newer than every source here, so an
/// mtime-only freshness check would run the suite against the wrong code. The binary's
/// `--version` names its commit; it must be a prefix of this checkout's, taken from Git HEAD
/// when the tree has a Git database, else from the development module manifest built from the
/// same checkout. A tree that can name neither keeps the mtime-only rule (see the callers).
pub fn binary_names_checkout(candidate: &Path, root: &Path) -> bool {
    names_identity(
        binary_short_sha(candidate).as_deref(),
        checkout_commit(root).as_deref(),
    )
}

/// Whether a binary that names `short` — or no commit at all — matches this checkout.
///
/// A binary that names no commit has unavailable identity: `p1-host/build.rs` writes
/// `unknown` when it cannot run `git`, so a legitimate source export prints
/// `p1 0.0.1 (unknown unknown)`. Treat that as the mtime-only case rather than failing a
/// usable artefact; the callers still attempt the nested rebuild when the mtime rule rejects
/// it.
fn names_identity(short: Option<&str>, commit: Option<&str>) -> bool {
    short.is_none_or(|short| revision_matches(short, commit))
}

/// Whether the binary's short commit is a prefix of the checkout's full commit. `None` is a
/// tree that cannot name its own revision.
fn revision_matches(short: &str, commit: Option<&str>) -> bool {
    commit.is_none_or(|commit| commit.starts_with(short))
}

/// This checkout's full commit: Git HEAD when the tree has a Git database, else the commit the
/// development module build recorded for the same tree.
fn checkout_commit(root: &Path) -> Option<String> {
    git_head(root).or_else(|| manifest_commit(root))
}

/// This worktree's full HEAD commit, when Git can name it here.
fn git_head(root: &Path) -> Option<String> {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["rev-parse", "HEAD"])
        .stdin(std::process::Stdio::null())
        .output()
        .ok()?;
    let full = String::from_utf8_lossy(&out.stdout).trim().to_owned();
    (out.status.success() && is_full_commit(&full)).then_some(full)
}

/// The commit `scripts/build-modules.sh` recorded in the development manifest it writes into
/// the checkout, for a build box that exported the tree without its Git object database.
fn manifest_commit(root: &Path) -> Option<String> {
    let manifest = std::fs::read(root.join("modules/target/p1-modules/manifest.json")).ok()?;
    let manifest: serde_json::Value = serde_json::from_slice(&manifest).ok()?;
    let full = manifest.get("commit")?.as_str()?;
    is_full_commit(full).then(|| full.to_owned())
}

fn is_full_commit(value: &str) -> bool {
    value.len() == 40
        && value
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
}

/// The short commit token of `p1 --version`, `deadbeef0000` in
/// `p1 0.0.1 (deadbeef0000 2026-09-24)`, or `None` when the build named no commit.
fn binary_short_sha(p1: &Path) -> Option<String> {
    let out = std::process::Command::new(p1)
        .arg("--version")
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap_or_else(|error| panic!("cannot run {} --version: {error}", p1.display()));
    assert!(
        out.status.success(),
        "{} --version exited {}",
        p1.display(),
        out.status
    );
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    named_commit(&text)
}

/// The first all-hexadecimal token after an opening `(`, or `None` when the version text
/// names no commit (`(unknown unknown)` from a build without Git metadata).
fn named_commit(version: &str) -> Option<String> {
    version.split_whitespace().find_map(|token| {
        let rest = token.strip_prefix('(')?;
        let sha: String = rest.chars().take_while(char::is_ascii_hexdigit).collect();
        (!sha.is_empty()).then_some(sha)
    })
}

#[test]
fn binary_identity_requires_the_checkout_commit() {
    let head = "2c7a6fbd88bab6777b338de6ce6cad36f27fc430";
    assert!(revision_matches("2c7a6fbd88ba", Some(head)));
    assert!(!revision_matches("deadbeef0000", Some(head)));
    assert!(revision_matches("deadbeef0000", None));
}

#[test]
fn a_binary_without_a_commit_token_has_unavailable_identity() {
    let head = "2c7a6fbd88bab6777b338de6ce6cad36f27fc430";
    // `p1-host/build.rs` names `unknown` in a source export without Git metadata.
    assert_eq!(named_commit("p1 0.0.1 (unknown unknown)"), None);
    assert_eq!(named_commit("p1 0.0.1"), None);
    assert_eq!(
        named_commit("p1 0.0.1 (deadbeef0000 2026-09-24)").as_deref(),
        Some("deadbeef0000")
    );
    // Unavailable identity falls back to the mtime rule instead of panicking.
    assert!(names_identity(None, Some(head)));
    assert!(!names_identity(Some("deadbeef0000"), Some(head)));
}

#[test]
fn a_module_manifest_names_only_a_full_lowercase_commit() {
    let dir = tempfile::tempdir().expect("a scratch directory");
    let modules = dir.path().join("modules/target/p1-modules");
    std::fs::create_dir_all(&modules).expect("the manifest directory");
    let full = "2c7a6fbd88bab6777b338de6ce6cad36f27fc430";
    std::fs::write(
        modules.join("manifest.json"),
        json!({ "commit": full }).to_string(),
    )
    .expect("the manifest");
    assert_eq!(manifest_commit(dir.path()).as_deref(), Some(full));
    std::fs::write(
        modules.join("manifest.json"),
        json!({ "commit": full.to_ascii_uppercase() }).to_string(),
    )
    .expect("the manifest");
    assert_eq!(manifest_commit(dir.path()), None);
}

// ---------------------------------------------------------------- binary inputs

/// The files and directories the `p1` binary is built from: the workspace manifests, the
/// runtime data trees, and each crate in `p1-host`'s normal dependency closure — its
/// `Cargo.toml`, `build.rs` and `src/`.
///
/// Scanning every crate, or a whole crate directory, treats a test or a test-only crate as an
/// input to the executable: editing one leaves it newer than the binary, while `cargo build -p
/// p1-host` does not relink the binary, so a caller that insists on freshness rejects a usable
/// artefact. Test and bench sources are not part of the binary and are skipped for the same
/// reason. A tree that names no `p1-host` manifest is read conservatively, as every crate
/// directory.
pub fn p1_binary_inputs(root: &Path) -> Vec<PathBuf> {
    let mut inputs: Vec<PathBuf> = ["Cargo.toml", "Cargo.lock", "build.rs", "routes", "profiles"]
        .iter()
        .map(|part| root.join(part))
        .collect();
    for dir in p1_binary_crates(root) {
        inputs.push(dir.join("Cargo.toml"));
        inputs.push(dir.join("build.rs"));
        inputs.push(dir.join("src"));
    }
    inputs
}

/// The crate directories `p1-host` links: its path-dependency closure, or every crate directory
/// when its manifest cannot be read.
fn p1_binary_crates(root: &Path) -> Vec<PathBuf> {
    let host = root.join("crates/p1-host");
    if host.join("Cargo.toml").is_file() {
        path_dependency_closure(&host)
    } else {
        crate_directories(&root.join("crates"))
    }
}

/// Every directory reachable from `start` through `[dependencies]` and `[build-dependencies]`
/// path entries, `start` included. A `[dev-dependencies]` entry is not followed: a test-only
/// crate is not linked into a normal build.
fn path_dependency_closure(start: &Path) -> Vec<PathBuf> {
    let mut crates = Vec::new();
    let mut seen = HashSet::new();
    let mut pending = vec![start.to_path_buf()];
    while let Some(dir) = pending.pop() {
        let key = std::fs::canonicalize(&dir).unwrap_or_else(|_| dir.clone());
        if !seen.insert(key.clone()) {
            continue;
        }
        crates.push(key);
        let Ok(manifest) = std::fs::read_to_string(dir.join("Cargo.toml")) else {
            continue;
        };
        for relative in dependency_paths(&manifest) {
            pending.push(dir.join(relative));
        }
    }
    crates
}

/// The immediate directory children of a `crates/` directory.
fn crate_directories(crates: &Path) -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Ok(entries) = std::fs::read_dir(crates) {
        for entry in entries.flatten() {
            if entry.file_type().is_ok_and(|kind| kind.is_dir()) {
                dirs.push(entry.path());
            }
        }
    }
    dirs
}

/// The `path` of every dependency in a manifest's `[dependencies]` and
/// `[build-dependencies]` sections, relative to the manifest's directory. `[dev-dependencies]`
/// is skipped.
fn dependency_paths(manifest: &str) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    let mut in_dependencies = false;
    for line in manifest.lines() {
        let line = line.split('#').next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        if let Some(section) = line
            .strip_prefix('[')
            .and_then(|rest| rest.strip_suffix(']'))
        {
            in_dependencies = is_dependency_section(section);
            continue;
        }
        if in_dependencies && let Some(path) = inline_path(line) {
            paths.push(PathBuf::from(path));
        }
    }
    paths
}

/// Whether a section header holds normal or build dependencies, not dev dependencies:
/// `dependencies`, `build-dependencies`, and their `target.'cfg(...)'.` forms. `[[bin]]`'s
/// `[bin]` and every other section are not dependency tables.
fn is_dependency_section(section: &str) -> bool {
    matches!(
        section.rsplit('.').next().unwrap_or(section),
        "dependencies" | "build-dependencies"
    )
}

/// The `path = "..."` value of an inline dependency table (`name = { path = "../crate" }`),
/// or `None` for a line that is not one.
fn inline_path(line: &str) -> Option<String> {
    let (_, value) = line.split_once('=')?;
    let table = value.trim().strip_prefix('{')?;
    let start = table.find("path")?;
    let after = table[start + "path".len()..].trim_start();
    let after = after.strip_prefix('=')?.trim_start();
    let after = after.strip_prefix('"')?;
    let (path, _) = after.split_once('"')?;
    Some(path.to_owned())
}

#[test]
fn a_dependency_manifest_reads_normal_and_build_paths_but_not_dev_paths() {
    let manifest = "[package]\nname = \"p1-host\"\n\n[dependencies]\n\
                    a = { path = \"../a\", features = [\"x\"] }\nb = { version = \"1\", path = \"../b\" }\n\
                    c = { workspace = true }\n\n[build-dependencies]\nd = { path = \"../d\" }\n\n\
                    [dev-dependencies]\ne = { path = \"../e\" }\n";
    assert_eq!(
        dependency_paths(manifest),
        vec![
            PathBuf::from("../a"),
            PathBuf::from("../b"),
            PathBuf::from("../d")
        ]
    );
}

#[test]
fn a_crate_closure_follows_only_the_paths_it_links() {
    let dir = tempfile::tempdir().expect("a scratch workspace");
    let host = dir.path().join("crates/p1-host");
    std::fs::create_dir_all(&host).expect("host dir");
    std::fs::write(
        host.join("Cargo.toml"),
        "[package]\nname = \"p1-host\"\n\n[dependencies]\n\
         p1-lib = { path = \"../p1-lib\" }\n\n[dev-dependencies]\n\
         p1-tests = { path = \"../p1-tests\" }\n",
    )
    .expect("host manifest");
    let lib = dir.path().join("crates/p1-lib");
    std::fs::create_dir_all(&lib).expect("lib dir");
    std::fs::write(
        lib.join("Cargo.toml"),
        "[package]\nname = \"p1-lib\"\n\n[dependencies]\n\
         p1-base = { path = \"../p1-base\" }\n",
    )
    .expect("lib manifest");
    let base = dir.path().join("crates/p1-base");
    std::fs::create_dir_all(&base).expect("base dir");
    std::fs::write(base.join("Cargo.toml"), "[package]\nname = \"p1-base\"\n")
        .expect("base manifest");
    let tests = dir.path().join("crates/p1-tests");
    std::fs::create_dir_all(&tests).expect("tests dir");
    std::fs::write(tests.join("Cargo.toml"), "[package]\nname = \"p1-tests\"\n")
        .expect("tests manifest");

    let mut closure = p1_binary_crates(dir.path());
    closure.sort();
    let mut expected = vec![
        std::fs::canonicalize(&host).unwrap(),
        std::fs::canonicalize(&lib).unwrap(),
        std::fs::canonicalize(&base).unwrap(),
    ];
    expected.sort();
    assert_eq!(closure, expected);
}
