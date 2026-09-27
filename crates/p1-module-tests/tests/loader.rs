//! The verified package loader as the host drives it (S1.4): `modules.lock` selects a
//! package, the host checks the lock against p1's release manifest and the runtime loader
//! verifies and compiles it. A digest mismatch, an ABI the host does not implement, two
//! packages claiming one identity and a package from any source but p1's release are each
//! refused, each with its own explicit error. Every fixture is derived in a scratch release
//! from the fixture component `scripts/build-modules.sh` builds; a missing build fails with
//! that script's name.

use std::path::PathBuf;

use p1_assembly::{ModulesLock, ModulesLockError};
use p1_contracts::serde_json::{Value, json};
use p1_host::catalog::modules::{ModulePackage, ModulesError, load_locked_modules};
use p1_module_runtime::{Digest, LoadError, ManifestError};
use p1_module_tests::{FIXTURE_NAME, Release, lock_text};

/// The module name every case selects.
const MODULE: &str = "fixture";

/// Where the cases' lock claims to come from; only error messages read it.
fn lock_path(release: &Release) -> PathBuf {
    release.root().join("modules.lock")
}

/// Loads what a lock selecting `entry` as [`MODULE`] resolves, from `release`.
fn load(release: &Release, entry: &Value) -> Result<Vec<ModulePackage>, ModulesError> {
    let lock = ModulesLock::parse(&lock_path(release), &lock_text(MODULE, entry))
        .expect("the lock parses");
    load_locked_modules(&lock, &release.manifest_file())
}

/// A release holding the fixture's bytes under its entry edited by `edit`, and that entry.
fn release_with_entry(edit: impl FnOnce(&mut Value)) -> (Release, Value) {
    let mut release = Release::empty();
    let mut entry = release.fixture_entry(FIXTURE_NAME);
    edit(&mut entry);
    let bytes = release.fixture().wasm.clone();
    release.add(entry.clone(), &bytes);
    (release, entry)
}

/// The corruption fixture: the release manifest pins the fixture's digest, the file on disk
/// has one byte flipped.
fn corrupted_release() -> (Release, Value) {
    let mut release = Release::empty();
    let entry = release.fixture_entry(FIXTURE_NAME);
    let mut bytes = release.fixture().wasm.clone();
    let at = bytes.len() / 2;
    bytes[at] ^= 0xff;
    release.add(entry.clone(), &bytes);
    (release, entry)
}

/// The collision fixture: the fixture's bytes listed twice, as `first` and `second`, each
/// in a file of its own so only the identity collides.
fn colliding_release(first: &str, second: &str) -> (Release, Value) {
    let mut release = Release::empty();
    let bytes = release.fixture().wasm.clone();
    let entry = release.fixture_entry(first);
    release.add(entry.clone(), &bytes);
    let mut copy = release.fixture_entry(second);
    copy["path"] = json!("packages/copy/copy.wasm");
    release.add(copy, &bytes);
    (release, entry)
}

/// A release whose packages directory holds the fixture while its manifest names nothing:
/// bytes that came from somewhere other than p1's release.
fn unlisted_release() -> (Release, Value) {
    let release = Release::empty();
    let entry = release.fixture_entry(FIXTURE_NAME);
    let path = release.root().join(entry["path"].as_str().expect("path"));
    std::fs::create_dir_all(path.parent().expect("parent")).expect("package dir");
    std::fs::write(&path, &release.fixture().wasm).expect("component file");
    (release, entry)
}

fn refusal(result: Result<Vec<ModulePackage>, ModulesError>) -> ModulesError {
    match result {
        Ok(_) => panic!("the selection was loaded, a refusal was expected"),
        Err(error) => error,
    }
}

/// The runtime loader's refusal inside a host refusal.
fn load_refusal(error: &ModulesError) -> &LoadError {
    match error {
        ModulesError::Load { source, .. } => source,
        other => panic!("expected the loader's refusal, got {other:?}"),
    }
}

/// The release manifest's refusal inside a host refusal.
fn release_refusal(error: &ModulesError) -> &ManifestError {
    match error {
        ModulesError::Release { source, .. } => source,
        other => panic!("expected the release manifest's refusal, got {other:?}"),
    }
}

// ------------------------------------------------------------------ control

#[test]
fn the_official_fixture_loads_with_a_loader_built_identity() {
    let release = Release::with_fixture();
    let entry = release.fixture_entry(FIXTURE_NAME);
    let packages = load(&release, &entry).expect("the unmodified fixture loads");
    let [package] = packages.as_slice() else {
        panic!("one package per lock entry");
    };
    assert_eq!(package.module, MODULE);
    assert_eq!(package.lock, lock_path(&release));
    assert_eq!(package.loaded.identity().implementation, FIXTURE_NAME);
    assert_eq!(package.loaded.identity().variant, "default");
    assert_eq!(package.loaded.digest(), Digest::of(&release.fixture().wasm));
    assert_eq!(
        Some(package.loaded.digest().to_string().as_str()),
        release.fixture().manifest["digest"].as_str()
    );
}

#[test]
fn an_empty_lock_loads_nothing() {
    let release = Release::with_fixture();
    let packages = load_locked_modules(&ModulesLock::default(), &release.manifest_file())
        .expect("an empty lock loads");
    assert!(packages.is_empty());
}

// ------------------------------------------------------------------ digest

#[test]
fn a_flipped_byte_is_refused_as_a_digest_mismatch() {
    let (release, entry) = corrupted_release();
    let error = refusal(load(&release, &entry));
    assert!(
        matches!(&error, ModulesError::Load { module, .. } if module == MODULE),
        "{error:?}"
    );
    match load_refusal(&error) {
        LoadError::DigestMismatch {
            name,
            expected,
            actual,
        } => {
            assert_eq!(name, FIXTURE_NAME);
            assert_ne!(expected, actual);
            assert_eq!(
                Some(expected.to_string().as_str()),
                release.fixture().manifest["digest"].as_str(),
                "the pinned digest is the built fixture's"
            );
        }
        other => panic!("expected a DigestMismatch, got {other:?}"),
    }
    assert!(error.to_string().contains("digest"), "{error}");
}

#[test]
fn a_lock_pinning_other_bytes_than_the_release_is_refused() {
    let release = Release::with_fixture();
    let mut entry = release.fixture_entry(FIXTURE_NAME);
    entry["digest"] = json!(Digest::of(b"other bytes").to_string());
    let error = refusal(load(&release, &entry));
    assert!(
        matches!(
            &error,
            ModulesError::LockMismatch {
                field: "digest",
                ..
            }
        ),
        "{error:?}"
    );
}

// ------------------------------------------------------------------ ABI

#[test]
fn a_world_the_host_does_not_implement_is_refused() {
    let (release, entry) = release_with_entry(|entry| {
        entry["world"] = json!("p1:module/tool@2.0.0");
    });
    let error = refusal(load(&release, &entry));
    match load_refusal(&error) {
        LoadError::WorldMismatch {
            name,
            world,
            expected,
            ..
        } => {
            assert_eq!(name, FIXTURE_NAME);
            assert_eq!(world, "p1:module/tool@2.0.0");
            assert_eq!(expected, "p1:module/tool@1.0.0");
        }
        other => panic!("expected a WorldMismatch, got {other:?}"),
    }
}

#[test]
fn a_protocol_the_host_does_not_implement_is_refused() {
    for protocol in ["2.0", "0.9"] {
        let (release, entry) = release_with_entry(|entry| {
            entry["protocol"] = json!(protocol);
        });
        let error = refusal(load(&release, &entry));
        assert!(
            matches!(load_refusal(&error),
                LoadError::ProtocolMismatch { protocol: written, .. } if written == protocol),
            "{protocol}: {error:?}"
        );
    }
}

#[test]
fn a_lock_naming_another_abi_than_the_release_is_refused() {
    let release = Release::with_fixture();
    for (field, value) in [("world", "p1:module/provider@1.0.0"), ("protocol", "1.1")] {
        let mut entry = release.fixture_entry(FIXTURE_NAME);
        entry[field] = json!(value);
        let error = refusal(load(&release, &entry));
        assert!(
            matches!(&error, ModulesError::LockMismatch { field: f, locked, .. }
                if *f == field && locked == value),
            "{field}: {error:?}"
        );
    }
}

// ------------------------------------------------------------------ duplicate identity

#[test]
fn two_packages_claiming_one_name_are_refused() {
    let (release, entry) = colliding_release(FIXTURE_NAME, FIXTURE_NAME);
    let error = refusal(load(&release, &entry));
    assert!(
        matches!(&error, ModulesError::Release { path, .. } if path == &release.manifest_file()),
        "{error:?}"
    );
    match release_refusal(&error) {
        ManifestError::DuplicateIdentity {
            identity,
            first,
            second,
        } => {
            assert_eq!(identity, FIXTURE_NAME);
            assert_ne!(first, second);
        }
        other => panic!("expected a DuplicateIdentity, got {other:?}"),
    }
}

#[test]
fn two_names_claiming_one_digest_are_refused() {
    let (release, entry) = colliding_release(FIXTURE_NAME, "p1/fixture-two");
    let error = refusal(load(&release, &entry));
    assert!(
        matches!(release_refusal(&error), ManifestError::DuplicateIdentity { identity, .. }
            if Some(identity.as_str()) == release.fixture().manifest["digest"].as_str()),
        "{error:?}"
    );
}

// ------------------------------------------------------------------ official source

#[test]
fn a_package_the_release_manifest_does_not_name_is_refused_naming_its_source() {
    let (release, entry) = unlisted_release();
    let error = refusal(load(&release, &entry));
    assert!(
        matches!(&error, ModulesError::Load { lock, .. } if lock == &lock_path(&release)),
        "{error:?}"
    );
    assert!(
        matches!(load_refusal(&error), LoadError::NotInManifest { name } if name == FIXTURE_NAME),
        "{error:?}"
    );
    let message = error.to_string();
    assert!(
        message.contains(&lock_path(&release).display().to_string()),
        "the error names the lock that selected it: {message}"
    );
}

#[test]
fn a_lock_cannot_select_a_foreign_package_or_a_path() {
    let release = Release::with_fixture();
    let entry = release.fixture_entry(FIXTURE_NAME);
    let foreign = lock_text(MODULE, &entry).replace("\"p1/fixture\"", "\"acme/fixture\"");
    let with_path = format!(
        "{}path = \"/tmp/fixture.wasm\"\n",
        lock_text(MODULE, &entry)
    );
    for text in [foreign, with_path] {
        let error = ModulesLock::parse(&lock_path(&release), &text).expect_err("refused");
        assert!(
            matches!(
                &error,
                ModulesLockError::InvalidEntry { .. } | ModulesLockError::Parse { .. }
            ),
            "{error:?}"
        );
        assert!(
            error
                .to_string()
                .contains(&lock_path(&release).display().to_string()),
            "the error names the lock: {error}"
        );
    }
}

// ------------------------------------------------------------------ grants

#[test]
fn a_component_importing_more_than_its_manifest_grants_is_refused() {
    // The fixture imports `process`; a release entry that does not grant it cannot load it.
    let (release, entry) = release_with_entry(|entry| {
        entry["capabilities"] = json!(["control", "clock"]);
    });
    let error = refusal(load(&release, &entry));
    assert!(
        matches!(load_refusal(&error), LoadError::UndeclaredImport { import, .. }
            if import.starts_with("p1:module/process@")),
        "{error:?}"
    );
}

#[test]
fn a_grant_outside_the_class_allocation_is_refused() {
    // `process` is a tool capability; the provider class is not allocated it.
    let (release, entry) = release_with_entry(|entry| {
        entry["kind"] = json!("provider");
        entry["world"] = json!("p1:module/provider@1.0.0");
        entry["capabilities"] = json!(["process"]);
    });
    let error = refusal(load(&release, &entry));
    assert!(
        matches!(&error, ModulesError::CapabilityNotAllocated { capability, kind, .. }
            if capability == "process" && kind == "provider"),
        "{error:?}"
    );
}

// ------------------------------------------------------------------ distinct errors

/// The variant name at the head of a `Debug` rendering.
fn variant(debug: String) -> String {
    debug
        .split([' ', '{', '('])
        .next()
        .unwrap_or_default()
        .to_owned()
}

/// The refusal's name, down to the runtime's own variant.
fn refusal_name(error: &ModulesError) -> String {
    match error {
        ModulesError::Load { source, .. } => format!("Load/{}", variant(format!("{source:?}"))),
        ModulesError::Release { source, .. } => {
            format!("Release/{}", variant(format!("{source:?}")))
        }
        other => variant(format!("{other:?}")),
    }
}

#[test]
fn each_refusal_has_its_own_error() {
    let (corrupted, entry) = corrupted_release();
    let digest = refusal(load(&corrupted, &entry));

    let (abi_release, abi_entry) = release_with_entry(|entry| {
        entry["world"] = json!("p1:module/tool@2.0.0");
    });
    let abi = refusal(load(&abi_release, &abi_entry));

    let (colliding, colliding_entry) = colliding_release(FIXTURE_NAME, FIXTURE_NAME);
    let duplicate = refusal(load(&colliding, &colliding_entry));

    let (unlisted, unlisted_entry) = unlisted_release();
    let foreign = refusal(load(&unlisted, &unlisted_entry));

    let errors = [digest, abi, duplicate, foreign];
    let names: Vec<String> = errors.iter().map(refusal_name).collect();
    assert_eq!(
        names,
        [
            "Load/DigestMismatch",
            "Load/WorldMismatch",
            "Release/DuplicateIdentity",
            "Load/NotInManifest",
        ]
    );
    for (i, a) in errors.iter().enumerate() {
        for b in &errors[i + 1..] {
            assert_ne!(a.to_string(), b.to_string());
        }
    }
}
