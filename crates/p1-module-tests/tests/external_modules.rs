//! External modules (ADR-0209, #887): a package of any publisher, installed into the user's
//! set with `p1 modules install`, is selected by its lock entry, verified by digest against
//! the set that holds it, refused beyond its class's allocation, and dispatches like p1's
//! own. Every set and lock is scratch data; the package is the built fixture
//! (`scripts/build-modules.sh`) published under another namespace with
//! `scripts/pack-module.py`.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use p1_assembly::{ModulesLock, load_modules_lock};
use p1_contracts::serde_json::{self, Value, json};
use p1_contracts::{CancellationToken, ToolContext, ToolStatus};
use p1_host::catalog::modules::{
    ModulePackage, ModulesError, installed_sets, load_locked_modules_in,
};
use p1_module_runtime::{Digest, ExecutionLimits, LoadError, Services, wasm_tool};
use p1_module_tests::{
    FIXTURE_PACKAGE, Release, call, fake_processes, p1_binary, pack, publish_as,
};
use p1_redact::MaskCounter;

/// The publisher's name for the fixture.
const PACKAGE: &str = "acme/fixture";
/// The module key the package's name declares.
const KEY: &str = "fixture";

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// The fixture's build outputs republished into `dir` as [`PACKAGE`], its manifest edited by
/// `edit` and its component by `wasm`.
fn publish(dir: &Path, edit: impl FnOnce(&mut Value), wasm: impl FnOnce(&mut Vec<u8>)) -> PathBuf {
    publish_as(FIXTURE_PACKAGE, PACKAGE, dir, edit, wasm)
}

/// `p1 modules install <archive>` with `config` as the user's catalog directory.
fn install(config: &Path, archive: &Path) -> std::process::Output {
    Command::new(p1_binary(&repo_root()))
        .args(["modules", "install"])
        .arg(archive)
        .env("P1_CONFIG_DIR", config)
        .env("HOME", config)
        .output()
        .expect("p1")
}

/// A scratch user directory with the fixture installed as [`PACKAGE`] by the real binary.
fn installed() -> tempfile::TempDir {
    let scratch = tempfile::tempdir().unwrap();
    let build = publish(scratch.path(), |_| {}, |_| {});
    let archive = scratch.path().join("fixture.p1pkg");
    pack(&build, &archive);
    let config = scratch.path().join("config");
    let output = install(&config, &archive);
    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains(&format!("installed {PACKAGE} sha256:")),
        "{stdout}"
    );
    assert!(
        stdout.contains(&format!("pinned module `{KEY}`")),
        "{stdout}"
    );
    scratch
}

/// The user's environment directory below a scratch from [`installed`].
fn user_dirs(scratch: &Path) -> Vec<PathBuf> {
    vec![scratch.join("config/environments")]
}

/// What the user's lock resolves, from the user's set and then `release`.
fn load(dirs: &[PathBuf], release: &Release) -> Result<Vec<ModulePackage>, ModulesError> {
    let lock = load_modules_lock(dirs).expect("the user lock reads");
    let manifest = release.manifest_file();
    load_locked_modules_in(&lock, &installed_sets(dirs, Some(&manifest)), &manifest)
}

/// The refusal a load that must fail returned.
fn refused(result: Result<Vec<ModulePackage>, ModulesError>) -> ModulesError {
    match result {
        Ok(_) => panic!("the selection loaded, a refusal was expected"),
        Err(error) => error,
    }
}

fn load_refusal(error: &ModulesError) -> &LoadError {
    match error {
        ModulesError::Load { source, .. } => source,
        other => panic!("expected the loader's refusal, got {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_installed_package_of_another_publisher_loads_and_dispatches() {
    let scratch = installed();
    let config = scratch.path().join("config");
    let lock = std::fs::read_to_string(config.join("modules.lock")).unwrap();
    let parsed = ModulesLock::parse(&config.join("modules.lock"), &lock).unwrap();
    let locked = parsed.resolve(KEY).expect("install pinned the key");
    assert_eq!(locked.package, PACKAGE);
    assert_eq!(locked.version, "1.2.3");

    // The release holds nothing of it: the user's set supplies it.
    let release = Release::empty();
    let [package] = load(&user_dirs(scratch.path()), &release)
        .expect("the installed package loads")
        .try_into()
        .unwrap_or_else(|_| panic!("one package"));
    assert_eq!(package.module, KEY);
    assert_eq!(package.loaded.identity().implementation, PACKAGE);
    assert_eq!(package.loaded.digest().to_string(), locked.digest);

    let tool = wasm_tool(
        &package.loaded,
        Services {
            process: Some(fake_processes().0),
            ..Services::default()
        },
        ExecutionLimits::default(),
        &Arc::new(MaskCounter::new()),
    )
    .expect("the tool builds");
    let outcome = tool
        .execute(
            &call("echo:from another publisher"),
            ToolContext {
                cancel: CancellationToken::new(),
            },
        )
        .await;
    assert_eq!(outcome.status, ToolStatus::Ok, "{outcome:?}");
    assert!(
        outcome.content.contains("from another publisher"),
        "{outcome:?}"
    );
}

#[test]
fn one_changed_byte_is_refused_at_install_and_in_the_set() {
    let scratch = installed();
    // An archive whose component is not the bytes its manifest pins installs nothing.
    let build = publish(&scratch.path().join("tampered"), |_| {}, |_| {});
    let wasm = build.join(format!("{FIXTURE_PACKAGE}.wasm"));
    let mut bytes = std::fs::read(&wasm).unwrap();
    let archive = scratch.path().join("tampered.p1pkg");
    pack(&build, &archive);
    let mut packed = std::fs::read(&archive).unwrap();
    let at = packed
        .windows(bytes.len().min(64))
        .position(|window| window == &bytes[..bytes.len().min(64)])
        .expect("the component is in the archive")
        + bytes.len() / 2;
    packed[at] ^= 0xff;
    std::fs::write(&archive, &packed).unwrap();
    let output = install(&scratch.path().join("config"), &archive);
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("nothing installed"),
        "{output:?}"
    );

    // A byte changed in the installed set's copy is the loader's DigestMismatch naming the set.
    let set = scratch.path().join("config/modules");
    let manifest: Value =
        serde_json::from_str(&std::fs::read_to_string(set.join("manifest.json")).unwrap()).unwrap();
    let path = set.join(manifest["components"][0]["path"].as_str().unwrap());
    bytes = std::fs::read(&path).unwrap();
    let middle = bytes.len() / 2;
    bytes[middle] ^= 0xff;
    std::fs::write(&path, &bytes).unwrap();
    let error = refused(load(&user_dirs(scratch.path()), &Release::empty()));
    assert!(
        matches!(load_refusal(&error), LoadError::DigestMismatch { name, set: refused, .. }
            if name == PACKAGE && refused == &set),
        "{error:?}"
    );
}

#[test]
fn a_component_importing_beyond_its_class_allocation_is_refused() {
    // The fixture imports `process`; as a provider, whose class is not allocated it, the
    // loader refuses it whatever the manifest grants.
    let scratch = tempfile::tempdir().unwrap();
    let as_provider = |manifest: &mut Value| {
        manifest["kind"] = json!("provider");
        manifest["world"] = json!("p1:module/provider@1.0.0");
        manifest["capabilities"] = json!(["control", "clock"]);
    };
    let build = publish(scratch.path(), as_provider, |_| {});
    let archive = scratch.path().join("provider.p1pkg");
    pack(&build, &archive);
    let output = install(&scratch.path().join("config"), &archive);
    assert!(!output.status.success(), "{output:?}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("p1:module/process@1.1.0") && stderr.contains("not allocated"),
        "{stderr}"
    );
    assert!(!scratch.path().join("config/modules.lock").exists());

    // A set written by hand is refused the same way when it loads.
    let set = scratch.path().join("pack/modules");
    let mut release_like = Release::empty();
    let mut entry = release_like.fixture_entry(PACKAGE);
    as_provider(&mut entry);
    let bytes = release_like.fixture().wasm.clone();
    release_like.add(entry.clone(), &bytes);
    std::fs::create_dir_all(set.parent().unwrap()).unwrap();
    copy_dir(release_like.root(), &set);
    std::fs::write(
        set.join("manifest.json"),
        std::fs::read(release_like.manifest_file()).unwrap(),
    )
    .unwrap();
    std::fs::write(
        scratch.path().join("pack/modules.lock"),
        p1_module_tests::lock_text(KEY, &entry),
    )
    .unwrap();
    let error = refused(load(
        &[scratch.path().join("pack/environments")],
        &Release::empty(),
    ));
    assert!(
        matches!(load_refusal(&error), LoadError::ImportsExceedAllocation { import, kind, .. }
            if import == "p1:module/process@1.1.0" && *kind == "provider"),
        "{error:?}"
    );
}

#[test]
fn the_lock_digest_chooses_among_sets_and_p1_names_come_from_the_release_only() {
    let scratch = installed();
    let dirs = vec![
        scratch.path().join("config/environments"),
        scratch.path().join("pack/environments"),
    ];
    // A pack's set holds the same name at other bytes (a valid custom section more); a pack
    // lock pinning those is overridden by the user's lock, which pins the user's set.
    let mut pack_release = Release::empty();
    let entry = pack_release.fixture_entry(PACKAGE);
    let other = [pack_release.fixture().wasm.as_slice(), &[0, 2, 1, b'x']].concat();
    let mut other_entry = entry.clone();
    other_entry["digest"] = json!(Digest::of(&other).to_string());
    pack_release.add(other_entry.clone(), &other);
    let pack_set = scratch.path().join("pack/modules");
    copy_dir(pack_release.root(), &pack_set);
    std::fs::write(
        pack_set.join("manifest.json"),
        std::fs::read(pack_release.manifest_file()).unwrap(),
    )
    .unwrap();
    let user = load(&dirs, &Release::empty()).expect("the user's pin loads");
    assert_ne!(user[0].loaded.digest(), Digest::of(&other));

    // Without the user's lock, the pack's lock pins the pack's bytes: the user's set holds the
    // name too, at another digest, and is passed over.
    std::fs::remove_file(scratch.path().join("config/modules.lock")).unwrap();
    std::fs::write(
        scratch.path().join("pack/modules.lock"),
        p1_module_tests::lock_text(KEY, &other_entry),
    )
    .unwrap();
    let from_pack = load(&dirs, &Release::empty()).expect("the pack's pin loads");
    assert_eq!(from_pack[0].loaded.digest(), Digest::of(&other));

    // A lock that pins bytes no set holds names the lock, the key and the set it compared.
    let mut ghost = other_entry.clone();
    ghost["digest"] = json!(Digest::of(b"no such bytes").to_string());
    std::fs::write(
        scratch.path().join("pack/modules.lock"),
        p1_module_tests::lock_text(KEY, &ghost),
    )
    .unwrap();
    let error = refused(load(&dirs, &Release::empty()));
    assert!(
        matches!(&error, ModulesError::LockMismatch { module, field, .. }
            if module == KEY && *field == "digest"),
        "{error:?}"
    );

    // A set other than the release may not list a `p1/` package: refused whole.
    let mut reserved = Release::empty();
    let p1_entry = reserved.fixture_entry("p1/fixture");
    let bytes = reserved.fixture().wasm.clone();
    reserved.add(p1_entry.clone(), &bytes);
    std::fs::write(
        pack_set.join("manifest.json"),
        std::fs::read(reserved.manifest_file()).unwrap(),
    )
    .unwrap();
    copy_dir(reserved.root(), &pack_set);
    std::fs::write(
        scratch.path().join("pack/modules.lock"),
        p1_module_tests::lock_text(KEY, &other_entry),
    )
    .unwrap();
    let error = refused(load(&dirs, &Release::empty()));
    assert!(
        matches!(&error, ModulesError::ReservedNamespace { package, .. } if package == "p1/fixture"),
        "{error:?}"
    );

    // And installing a `p1/` package is refused before anything is written.
    let build = publish(
        &scratch.path().join("official"),
        |manifest| manifest["name"] = json!("p1/fixture"),
        |_| {},
    );
    let archive = scratch.path().join("official.p1pkg");
    pack(&build, &archive);
    let output = install(&scratch.path().join("other-config"), &archive);
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("reserved"),
        "{output:?}"
    );
    assert!(!scratch.path().join("other-config/modules.lock").exists());
}

/// Copies the files of `from` (a scratch release) into `to`, keeping relative paths.
fn copy_dir(from: &Path, to: &Path) {
    for entry in std::fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let target = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            std::fs::create_dir_all(&target).unwrap();
            copy_dir(&entry.path(), &target);
        } else {
            std::fs::create_dir_all(to).unwrap();
            std::fs::copy(entry.path(), &target).unwrap();
        }
    }
}
