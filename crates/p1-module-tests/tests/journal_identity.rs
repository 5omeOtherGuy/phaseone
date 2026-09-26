//! Journal identity (S1.9, ADR-0080): the host writes the assembly identity line before the
//! first `Environment` record and again when the assembly changes, and on resume it compares
//! the journal's last identity with the identity loaded now and reports every changed
//! artifact — a module whose digest, package or version changed, or one added or removed.
//!
//! The package is the real fixture, verified and compiled by the runtime loader, registered
//! through the host's catalog entry point and built by `WasmTool`; the identities come from
//! `p1_host::run::assembly_identity` and the line lands through `p1_host::run::naming_sink`,
//! the same functions the run path calls. Every journal is a `tempfile` (the version-2 fixture
//! the box's pinned release binary is pointed at lives in Cargo's own test tmp directory), and
//! no case reaches the network.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use p1_assembly::{
    Catalog, EnvironmentFile, ModulesLock, ProviderSpec, Substitutions, ToolServices, ToolSpec,
    assemble,
};
use p1_contracts::serde_json::{self, Value, json};
use p1_contracts::{JournalRecord, ModelOptions, Provider, RecordBody, Tool};
use p1_host::catalog::modules::{ModuleServices, load_locked_modules, register_modules};
use p1_host::run::{
    AssemblyEntry, AssemblyIdentity, AssemblyLines, AssemblyStore, JOURNAL_VERSION, arm_assembly,
    assembly_identity, changed_artifacts, host_identity,
};
use p1_host::session;
use p1_module_runtime::{Digest, Services};
use p1_module_tests::{FIXTURE_NAME, FakeProcesses, Release, fake_processes, lock_text};
use p1_testkit::{FakeTool, ScriptedProvider};

const PROVIDER: &str = "scripted";
/// The module name the lock gives the fixture package.
const MODULE: &str = "fixture";
/// The second module name the lock resolves to the variant package.
const EXTRA: &str = "extra";
/// The compiled-in stand-in tool every environment below also names.
const NATIVE: &str = "read";
/// A second compiled-in stand-in tool: how a module JOINS an assembly without another package.
const GREP: &str = "grep";
/// The manifest name the variant component is added under.
const EXTRA_NAME: &str = "p1/extra";

fn tool_spec(module: &str) -> ToolSpec {
    ToolSpec {
        module: module.into(),
        name: None,
        description: None,
        variant: None,
    }
}

/// One environment naming `modules` in order, under the environment name `name`.
fn environment(name: &str, modules: &[&str]) -> EnvironmentFile {
    EnvironmentFile {
        name: name.into(),
        family: "test".into(),
        provider: PROVIDER.into(),
        model: "test-model".into(),
        profile: None,
        options: ModelOptions::default(),
        tools: modules.iter().map(|module| tool_spec(module)).collect(),
        prompt_template: "tools: {{tool_names}}".into(),
        context: None,
        summarize_prompt: None,
    }
}

fn substitutions() -> Substitutions {
    Substitutions {
        workspace: "/work".into(),
        date: "2026-01-01".into(),
        os: "linux".into(),
    }
}

/// The process service the fixture's verified `process` grant needs: a component whose import
/// is unlinked is never instantiated, so the assembly would fail for the wrong reason.
struct Processes {
    services: ModuleServices,
    _fakes: FakeProcesses,
}

fn processes() -> Processes {
    let (process, fakes) = fake_processes();
    Processes {
        services: Arc::new(move |_: &str, _: &ToolServices| Services {
            process: Some(process.clone()),
            ..Services::default()
        }),
        _fakes: fakes,
    }
}

/// A catalog with the scripted provider, the compiled-in stand-in tools and every package
/// `lock` resolves in `release` — the host's module entry point over the runtime's loader.
fn catalog(release: &Release, lock: &ModulesLock, processes: &ModuleServices) -> Catalog {
    let mut catalog = Catalog::new();
    let provider = ScriptedProvider::new(Vec::new());
    catalog.provider(
        PROVIDER,
        Box::new(move |_spec: &ProviderSpec| Ok(Arc::new(provider.clone()) as Arc<dyn Provider>)),
    );
    for (key, implementation) in [(NATIVE, "p1-tool-read"), (GREP, "p1-tool-grep")] {
        catalog.tool(
            key,
            Box::new(move |_spec: &ToolSpec, _services: &ToolServices| {
                Ok(
                    Arc::new(FakeTool::new(key).with_identity(implementation, "default"))
                        as Arc<dyn Tool>,
                )
            }),
        );
    }
    let packages = load_locked_modules(lock, &release.manifest_file()).expect("the fixture loads");
    register_modules(&mut catalog, packages, processes.clone()).expect("registration");
    catalog
}

/// The identity the host would write for an environment naming `modules`, assembled over
/// `release` and `lock`: the same function, over the same registration, the run path uses.
fn identity_for(
    release: &Release,
    lock: &ModulesLock,
    processes: &Processes,
    environment_name: &str,
    modules: &[&str],
) -> AssemblyIdentity {
    let catalog = catalog(release, lock, &processes.services);
    let workspace = tempfile::tempdir().expect("workspace");
    let assembled = assemble(
        &catalog,
        &environment(environment_name, modules),
        workspace.path(),
        &substitutions(),
    )
    .expect("assembles");
    assembly_identity(&assembled, PROVIDER, false, lock)
}

/// The lock resolving `module` to the fixture entry of `release`.
fn fixture_lock(release: &Release, module: &str) -> ModulesLock {
    lock_for(module, &release.fixture_entry(FIXTURE_NAME), release)
}

fn lock_for(module: &str, entry: &Value, release: &Release) -> ModulesLock {
    ModulesLock::parse(
        &release.root().join("modules.lock"),
        &lock_text(module, entry),
    )
    .expect("fixture lock")
}

/// The fixture with one appended, opaque custom section whose last byte is flipped: another
/// artifact with another digest that is still the same module code. An engine reads no custom
/// section it does not know, so the component validates, compiles and instantiates unchanged.
fn variant_bytes(wasm: &[u8]) -> Vec<u8> {
    const NAME: &[u8] = b"p1-journal-identity-variant";
    let mut payload = vec![NAME.len() as u8];
    payload.extend_from_slice(NAME);
    payload.extend_from_slice(&[0xAB; 8]);
    let mut bytes = wasm.to_vec();
    bytes.push(0x00); // a top-level custom section
    bytes.push(payload.len() as u8); // one-byte LEB length
    bytes.extend_from_slice(&payload);
    let last = bytes.len() - 1;
    bytes[last] ^= 0xFF;
    bytes
}

/// The fixture's manifest entry under `name`, with the digest of `bytes`: what a release holds
/// after a package's bytes were swapped and the manifest rebuilt for them.
fn entry_for(release: &Release, name: &str, bytes: &[u8]) -> Value {
    let mut entry = release.fixture_entry(name);
    entry["digest"] = json!(Digest::of(bytes).to_string());
    entry
}

/// A second entry appended to one lock file: `lock_text` writes the format line once, so only
/// the module table of the next entry is appended.
fn next_lock_entry(module: &str, entry: &Value) -> String {
    let text = lock_text(module, entry);
    text.split_once("\n\n")
        .expect("a lock text has a format line, a blank line, then the entry")
        .1
        .to_string()
}

/// A release holding the fixture as `p1/fixture` and the variant bytes as `p1/extra`: two
/// components with two digests, so one lock can select either as its own module.
fn two_package_release() -> (Release, Vec<u8>) {
    let mut release = Release::with_fixture();
    let variant = variant_bytes(&release.fixture().wasm);
    let extra = entry_for(&release, EXTRA_NAME, &variant);
    release.add(extra, &variant);
    (release, variant)
}

/// A release holding ONLY the variant bytes under the fixture's own manifest name (the same
/// package, another artifact) and the lock that selects them.
fn release_of_swapped_bytes(variant: &[u8]) -> (Release, ModulesLock) {
    let mut release = Release::empty();
    let entry = entry_for(&release, FIXTURE_NAME, variant);
    release.add(entry.clone(), variant);
    let lock = lock_for(MODULE, &entry, &release);
    (release, lock)
}

fn user(seq: u64, text: &str) -> JournalRecord {
    JournalRecord {
        seq,
        body: RecordBody::UserInput { text: text.into() },
    }
}

fn record_line(record: &JournalRecord) -> String {
    let mut line = serde_json::to_string(record).unwrap();
    line.push('\n');
    line
}

fn text_of(path: &Path) -> String {
    std::fs::read_to_string(path).expect("the journal reads")
}

fn lines_of(path: &Path) -> Vec<String> {
    text_of(path).lines().map(str::to_string).collect()
}

/// Where the version-2 fixture of [`a_version_2_journal_names_the_assembly_before_the_first_record`]
/// lives: Cargo's own test tmp directory, so the box can point the PINNED RELEASE BINARY at a
/// file this test wrote (`cargo test --locked -p p1-module-tests --test journal_identity`).
fn evidence_path(name: &str) -> PathBuf {
    let base = std::env::var_os("CARGO_TARGET_TMPDIR")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    let dir = base.join("journal_identity");
    std::fs::create_dir_all(&dir).expect("evidence dir");
    dir.join(name)
}

/// The released binaries' header check, verbatim in effect: `p1_journal` must be exactly 1.
/// Slice S1.9 runs the real pinned release binary against the version-2 file of
/// [`a_version_2_journal_names_the_assembly_before_the_first_record`] on the box as evidence.
fn released_binary_accepts(header: &str) -> bool {
    let value: Value = serde_json::from_str(header).unwrap();
    value.get("p1_journal").and_then(Value::as_u64) == Some(1)
}

/// Create a version-2 journal, arm the assembly line for `identity` against `entries` and
/// commit one record through the host's own sink: the line lands immediately before it. The
/// store is dropped here, so the caller can resume the file.
async fn open_and_name(
    path: &Path,
    identity: &AssemblyIdentity,
    entries: &[AssemblyEntry],
) -> Vec<String> {
    let lines = Arc::new(AssemblyLines::new(
        AssemblyStore::File(session::create(path).expect("a new session")),
        JOURNAL_VERSION,
    ));
    let changes = arm_assembly(&lines, entries, identity);
    lines
        .sink()
        .commit(&user(0, "hello"))
        .await
        .expect("commit");
    drop(lines);
    changes
}

/// The entry of the module the environment selected by `key`, as the journal would carry it.
fn module_of(identity: &AssemblyIdentity, key: &str) -> Value {
    let value = serde_json::to_value(identity).expect("serializes");
    value["modules"]
        .as_array()
        .expect("modules")
        .iter()
        .find(|module| module["package"] == json!(key))
        .unwrap_or_else(|| panic!("no module `{key}` in {value}"))
        .clone()
}

fn kinds_of(identity: &AssemblyIdentity) -> Vec<String> {
    serde_json::to_value(identity).expect("serializes")["modules"]
        .as_array()
        .expect("modules")
        .iter()
        .map(|module| module["kind"].as_str().expect("kind").to_string())
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_version_2_journal_names_the_assembly_before_the_first_record() {
    let release = Release::with_fixture();
    let processes = processes();
    let lock = fixture_lock(&release, MODULE);
    let identity = identity_for(
        &release,
        &lock,
        &processes,
        "modules-test",
        &[NATIVE, MODULE],
    );

    let path = evidence_path("version2.jsonl");
    let _ = std::fs::remove_file(&path);
    // The file names no assembly yet, so the host names the one running now and has nothing to
    // compare: a fresh journal reports nothing. The line lands with the first record the run
    // commits, so it is line 2.
    let changes = open_and_name(&path, &identity, &[]).await;
    assert!(changes.is_empty(), "{changes:?}");

    // The identity line is line 2: before the first record, which the first turn commits
    // together with the `Environment` record it wrote first.
    let lines = lines_of(&path);
    assert_eq!(lines[0], r#"{"p1_journal":2}"#);
    let recorded: Value = serde_json::from_str(&lines[1]).expect("the assembly line is JSON");
    assert_eq!(
        recorded,
        json!({ "assembly": serde_json::to_value(&identity).unwrap() }),
        "the line carries the identity the host built"
    );
    assert!(
        recorded["assembly"].get("seq").is_none(),
        "an assembly line has no seq"
    );
    assert_eq!(lines.len(), 3, "header, assembly line, one record");
    assert!(lines[2].contains("\"user_input\""), "{}", lines[2]);

    // The identity names the loader-VERIFIED digest in the bare hex spelling, and the ABI the
    // lock's world and protocol spell.
    let verified = release
        .loader()
        .load(FIXTURE_NAME)
        .expect("the fixture loads");
    let bare = verified.digest().to_string();
    let bare = bare.strip_prefix("sha256:").expect("the manifest form");
    let package = module_of(&identity, MODULE);
    assert_eq!(package["kind"], "tool");
    assert_eq!(package["name"], FIXTURE_NAME);
    assert_eq!(package["digest"], json!(bare));
    let manifest = &release.fixture().manifest;
    assert_eq!(
        package["abi"],
        json!(format!(
            "{}+{}",
            manifest["world"].as_str().unwrap(),
            manifest["protocol"].as_str().unwrap()
        ))
    );
    // A native module has no bytes: its digest is null and the host's commit identifies it.
    let native = module_of(&identity, NATIVE);
    assert_eq!(native["name"], "p1-tool-read");
    assert!(native["digest"].is_null(), "{native}");
    assert!(native["abi"].is_null(), "{native}");
    assert_eq!(module_of(&identity, PROVIDER)["kind"], "provider");
    let policy = module_of(&identity, p1_host::policy::FULL_ACCESS_POLICY);
    assert_eq!(policy["kind"], "authorization_policy");
    assert!(policy["digest"].is_null(), "{policy}");
    // The environment declares no `[context]`: the core sends the history unchanged and no
    // context-policy module is assembled at all.
    let kinds = kinds_of(&identity);
    assert!(!kinds.contains(&"context_policy".to_string()), "{kinds:?}");

    // A resume reads the line back, at the seq of the record that follows it.
    let (_store, resumed) = session::resume(&path).expect("resumes");
    assert_eq!(resumed.version, 2);
    assert_eq!(resumed.records, vec![user(0, "hello")]);
    assert_eq!(resumed.assemblies.len(), 1);
    assert_eq!(resumed.assemblies[0].from_seq, 0);
    assert_eq!(resumed.assemblies[0].identity, identity);
    assert_eq!(resumed.assemblies[0].identity.host, host_identity());

    // The header check of the released binaries, applied to the file this case just wrote.
    // Running the pinned release binary against it is the box evidence of this slice.
    assert!(!released_binary_accepts(&lines[0]));
    assert!(released_binary_accepts(r#"{"p1_journal":1}"#));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unchanged_assembly_resumes_with_no_report_and_no_new_line() {
    let release = Release::with_fixture();
    let processes = processes();
    let lock = fixture_lock(&release, MODULE);
    let identity = identity_for(
        &release,
        &lock,
        &processes,
        "modules-test",
        &[NATIVE, MODULE],
    );

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("unchanged.jsonl");
    let changes = open_and_name(&path, &identity, &[]).await;
    assert!(changes.is_empty(), "{changes:?}");
    let before = std::fs::read(&path).unwrap();

    let (store, resumed) = session::resume(&path).expect("resumes");
    // Rebuilt from a second assembly of the same package: the journal's own claim, so nothing
    // is owed and there is nothing to report.
    let rebuilt = identity_for(
        &release,
        &lock,
        &processes,
        "modules-test",
        &[NATIVE, MODULE],
    );
    assert_eq!(rebuilt, identity);
    let lines = Arc::new(AssemblyLines::new(
        AssemblyStore::File(store.clone()),
        resumed.version,
    ));
    let changes = arm_assembly(&lines, &resumed.assemblies, &rebuilt);
    assert!(changes.is_empty(), "{changes:?}");
    // A record still goes through the sink: nothing owed is no line, not a broken sink.
    let sink = lines.sink();
    sink.commit(&user(1, "again")).await.expect("commit");
    drop(sink);
    drop(lines);
    drop(store);
    let after = lines_of(&path);
    assert_eq!(after.len(), 4, "{after:?}");
    assert!(after[3].contains("\"user_input\""), "{}", after[3]);
    assert_eq!(
        after
            .iter()
            .filter(|line| line.contains("\"assembly\""))
            .count(),
        1,
        "an unchanged resume appends no line: {after:?}"
    );
    assert!(
        text_of(&path).as_bytes().starts_with(&before),
        "appends only, never rewrites"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn swapping_a_packages_bytes_is_reported_by_the_module_it_changes() {
    let first_release = Release::with_fixture();
    let built = first_release.fixture().wasm.clone();
    let variant = variant_bytes(&built);
    let (second_release, second_lock) = release_of_swapped_bytes(&variant);
    let processes = processes();
    let modules = [NATIVE, MODULE];
    let first = identity_for(
        &first_release,
        &fixture_lock(&first_release, MODULE),
        &processes,
        "modules-test",
        &modules,
    );
    // The same manifest name, the same lock key, another artifact: the release was rebuilt
    // over the swapped bytes, and the lock pins the digest the loader verified for them.
    let verified = second_release
        .loader()
        .load(FIXTURE_NAME)
        .expect("the re-manifested package loads");
    assert_eq!(verified.digest(), Digest::of(&variant));
    assert_ne!(verified.digest(), Digest::of(&built));
    let swapped = identity_for(
        &second_release,
        &second_lock,
        &processes,
        "modules-test",
        &modules,
    );
    assert_eq!(
        module_of(&swapped, MODULE)["digest"],
        json!(
            Digest::of(&variant)
                .to_string()
                .strip_prefix("sha256:")
                .unwrap()
        )
    );

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("swapped.jsonl");
    let changes = open_and_name(&path, &first, &[]).await;
    assert!(changes.is_empty(), "{changes:?}");
    let before = text_of(&path);

    let (store, resumed) = session::resume(&path).expect("resumes");
    let lines = Arc::new(AssemblyLines::new(
        AssemblyStore::File(store.clone()),
        resumed.version,
    ));
    let changes = arm_assembly(&lines, &resumed.assemblies, &swapped);
    assert_eq!(changes.len(), 1, "{changes:?}");
    let report = &changes[0];
    assert!(report.contains(MODULE), "{report}");
    assert!(report.contains(FIXTURE_NAME), "{report}");
    assert!(!report.contains("added"), "{report}");
    assert!(!report.contains("removed"), "{report}");
    assert!(report.contains("digest"), "{report}");
    for digest in [Digest::of(&built), Digest::of(&variant)] {
        let hex = digest.to_string();
        assert!(
            report.contains(hex.strip_prefix("sha256:").unwrap()),
            "{report} does not name {hex}"
        );
    }
    // The changed artifact is reported, not blocked: the resumed session writes on, and its
    // first commit lands the new line before the record that follows it.
    let sink = lines.sink();
    sink.commit(&user(1, "again")).await.expect("commit");
    drop(sink);
    drop(lines);
    drop(store);

    // The new line is appended after the records the FIRST assembly executed and applies from
    // there on: a resume never rewrites what the journal already says.
    let text = text_of(&path);
    assert!(text.starts_with(&before), "appends only, never rewrites");
    let (store, resumed) = session::resume(&path).expect("resumes");
    assert_eq!(resumed.version, 2);
    assert_eq!(resumed.assemblies.len(), 2);
    assert_eq!(resumed.assemblies[0].identity, first);
    assert_eq!(resumed.assemblies[1].identity, swapped);
    assert_eq!(resumed.assemblies[1].from_seq, 1);
    assert_eq!(resumed.records, vec![user(0, "hello"), user(1, "again")]);
    drop(store);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_added_module_and_a_removed_module_are_both_reported() {
    let (release, variant) = two_package_release();
    let processes = processes();
    // One lock naming both packages: `fixture` is the built fixture, `extra` the variant.
    let mut entries = lock_text(MODULE, &release.fixture_entry(FIXTURE_NAME));
    entries.push_str(&next_lock_entry(
        EXTRA,
        &entry_for(&release, EXTRA_NAME, &variant),
    ));
    let both = ModulesLock::parse(&release.root().join("modules.lock"), &entries).expect("a lock");
    let verified = release
        .loader()
        .load(EXTRA_NAME)
        .expect("the variant loads");
    assert_eq!(verified.digest(), Digest::of(&variant));

    let first = identity_for(
        &release,
        &both,
        &processes,
        "modules-test",
        &[NATIVE, MODULE],
    );
    // A module JOINING the assembly: a compiled-in tool the first assembly did not name.
    let added = identity_for(
        &release,
        &both,
        &processes,
        "modules-test",
        &[NATIVE, MODULE, GREP],
    );
    let added_lines = changed_artifacts(&first, &added);
    assert_eq!(added_lines.len(), 1, "{added_lines:?}");
    assert!(added_lines[0].contains("added"), "{added_lines:?}");
    assert!(added_lines[0].contains(GREP), "{added_lines:?}");
    assert!(added_lines[0].contains("p1-tool-grep"), "{added_lines:?}");

    // A module LEAVING it: the package the first assembly named is not assembled at all.
    let removed = identity_for(&release, &both, &processes, "modules-test", &[NATIVE]);
    let removed_lines = changed_artifacts(&first, &removed);
    assert_eq!(removed_lines.len(), 1, "{removed_lines:?}");
    assert!(removed_lines[0].contains("removed"), "{removed_lines:?}");
    assert!(removed_lines[0].contains(MODULE), "{removed_lines:?}");
    assert!(removed_lines[0].contains(FIXTURE_NAME), "{removed_lines:?}");

    // And one package replaced by another: the report names both, as removed and added.
    let other = identity_for(
        &release,
        &both,
        &processes,
        "modules-test",
        &[NATIVE, EXTRA],
    );
    let replaced = changed_artifacts(&first, &other);
    assert_eq!(replaced.len(), 2, "{replaced:?}");
    assert!(
        replaced
            .iter()
            .any(|line| line.contains("removed") && line.contains(FIXTURE_NAME)),
        "{replaced:?}"
    );
    assert!(
        replaced
            .iter()
            .any(|line| line.contains("added") && line.contains(EXTRA_NAME)),
        "{replaced:?}"
    );

    // Through the store: the same report, the line landing before the next record, the session
    // still resumable.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("added.jsonl");
    let changes = open_and_name(&path, &first, &[]).await;
    assert!(changes.is_empty(), "{changes:?}");
    let (store, resumed) = session::resume(&path).expect("resumes");
    let lines = Arc::new(AssemblyLines::new(
        AssemblyStore::File(store.clone()),
        resumed.version,
    ));
    let changes = arm_assembly(&lines, &resumed.assemblies, &added);
    assert_eq!(changes, added_lines);
    let sink = lines.sink();
    sink.commit(&user(1, "again")).await.expect("commit");
    drop(sink);
    drop(lines);
    drop(store);
    let (_store, reloaded) = session::resume(&path).expect("resumes");
    assert_eq!(reloaded.assemblies.len(), 2);
    assert_eq!(reloaded.assemblies[1].identity, added);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_changed_environment_is_reported_by_name() {
    let release = Release::with_fixture();
    let processes = processes();
    let lock = fixture_lock(&release, MODULE);
    let modules = [NATIVE, MODULE];
    let first = identity_for(&release, &lock, &processes, "modules-test", &modules);
    let moved = identity_for(&release, &lock, &processes, "moved", &modules);

    let lines = changed_artifacts(&first, &moved);
    assert_eq!(lines.len(), 1, "{lines:?}");
    assert!(lines[0].contains("environment"), "{lines:?}");
    assert!(lines[0].contains("modules-test"), "{lines:?}");
    assert!(lines[0].contains("moved"), "{lines:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_version_1_journal_resumes_with_no_assembly_line_and_no_report() {
    let release = Release::with_fixture();
    let processes = processes();
    let lock = fixture_lock(&release, MODULE);
    let identity = identity_for(
        &release,
        &lock,
        &processes,
        "modules-test",
        &[NATIVE, MODULE],
    );

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("old.jsonl");
    let written = format!(
        "{{\"p1_journal\":1}}\n{}{}",
        record_line(&user(0, "a")),
        record_line(&user(1, "b"))
    );
    std::fs::write(&path, &written).unwrap();

    let (store, resumed) = session::resume(&path).expect("resumes");
    assert_eq!(resumed.version, 1);
    assert_eq!(resumed.records, vec![user(0, "a"), user(1, "b")]);
    assert!(resumed.assemblies.is_empty(), "version 1 carries no line");
    // Writing the line would make the file unreadable as version 1, whose header is never
    // rewritten: the resume owes nothing, and it has no journal claim to report against.
    let lines = Arc::new(AssemblyLines::new(
        AssemblyStore::File(store.clone()),
        resumed.version,
    ));
    let changes = arm_assembly(&lines, &resumed.assemblies, &identity);
    assert!(changes.is_empty(), "{changes:?}");
    // A record still goes through the sink: a version-1 file never gets a line.
    let sink = lines.sink();
    sink.commit(&user(2, "c")).await.expect("commit");
    drop(sink);
    drop(lines);
    drop(store);
    let text = text_of(&path);
    assert!(
        text.as_bytes().starts_with(written.as_bytes()),
        "appends only, never rewrites"
    );
    assert_eq!(text.matches("assembly").count(), 0, "{text}");
    assert!(released_binary_accepts(text.lines().next().unwrap()));
}
