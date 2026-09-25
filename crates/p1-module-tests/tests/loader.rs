//! The verified package loader (S1.4, BRIEF §3/§4): a digest mismatch, an ABI the host
//! does not implement, two packages claiming one identity and a package from a source
//! other than p1's release are each refused, each with its own explicit error. Every
//! fixture is derived in a scratch release from the fixture package that
//! `scripts/build-modules.sh` builds; a missing build fails with that script's name.

use std::mem::discriminant;

use p1_host::catalog::modules::{DigestRecord, LoadError, Release, VerifiedModule};
use p1_module_tests::{FIXTURE_PACKAGE, ScratchRelease, fixture, lock_entry};
use serde_json::Value;

/// Open the scratch release and load the lock resolution for `manifest` as `fixture`.
fn load_with(
    release: &ScratchRelease,
    locked: &p1_assembly::LockedModule,
) -> Result<VerifiedModule, LoadError> {
    let engine = p1_module_runtime::engine().expect("engine");
    Release::open(&release.modules_dir)?.load(&engine, "fixture", locked)
}

fn load(release: &ScratchRelease, manifest: &Value) -> Result<VerifiedModule, LoadError> {
    load_with(release, &lock_entry(manifest))
}

/// A release holding the fixture under an edited manifest, named by the release
/// manifest as it is on disk (so only the edit is wrong).
fn release_with_manifest(edit: impl FnOnce(&mut Value)) -> (ScratchRelease, Value) {
    let fixture = fixture();
    let mut manifest = fixture.manifest.clone();
    edit(&mut manifest);
    let release = ScratchRelease::new();
    release.add_package(FIXTURE_PACKAGE, &fixture.wasm, &manifest);
    release.write_manifest(&[FIXTURE_PACKAGE]);
    (release, manifest)
}

// ------------------------------------------------------------------ control

#[test]
fn the_official_fixture_loads_with_a_loader_built_identity() {
    let fixture = fixture();
    let release = ScratchRelease::with_fixture();
    let module = load(&release, &fixture.manifest).expect("the unmodified fixture loads");
    assert_eq!(module.module, "fixture");
    assert_eq!(module.identity.implementation, "p1/fixture");
    assert_eq!(module.identity.variant, "default");
    assert_eq!(
        module.digest,
        format!("sha256:{}", p1_usage::sha256_hex(&fixture.wasm))
    );
    assert_eq!(
        Some(module.digest.as_str()),
        fixture.manifest["digest"].as_str()
    );
}

// ------------------------------------------------------------------ digest

#[test]
fn a_flipped_byte_is_refused_as_a_digest_mismatch() {
    let fixture = fixture();
    let release = ScratchRelease::with_fixture();
    // The corruption fixture: one byte of the component flipped after the release
    // manifest recorded it.
    let wasm = release.package_file(FIXTURE_PACKAGE, "wasm");
    release.flip_byte(&wasm);

    let error = load(&release, &fixture.manifest).expect_err("a corrupted component is refused");
    match &error {
        LoadError::DigestMismatch {
            package,
            path,
            expected,
            actual,
            ..
        } => {
            assert_eq!(package, "p1/fixture");
            assert_eq!(path, &wasm);
            assert_ne!(expected, actual);
            assert_eq!(
                Some(expected.as_str()),
                fixture.manifest["digest"].as_str(),
                "the recorded digest is the original's"
            );
        }
        other => panic!("expected DigestMismatch, got {other:?}"),
    }
    assert!(error.to_string().contains("digest"), "{error}");
}

#[test]
fn a_lock_digest_that_is_not_the_packages_is_refused() {
    let fixture = fixture();
    let release = ScratchRelease::with_fixture();
    let mut locked = lock_entry(&fixture.manifest);
    locked.digest = format!("sha256:{}", "0".repeat(64));
    let error = load_with(&release, &locked).expect_err("the lock pins other bytes");
    assert!(
        matches!(
            &error,
            LoadError::DigestMismatch {
                record: DigestRecord::ModulesLock,
                ..
            }
        ),
        "{error:?}"
    );
}

// ------------------------------------------------------------------ ABI

#[test]
fn a_world_the_host_does_not_implement_is_refused() {
    let (release, manifest) = release_with_manifest(|manifest| {
        manifest["world"] = Value::from("p1:module/tool@2.0.0");
    });
    let error = load(&release, &manifest).expect_err("world 2.0.0 is refused");
    match &error {
        LoadError::UnsupportedAbi { package, world, .. } => {
            assert_eq!(package, "p1/fixture");
            assert_eq!(world, "p1:module/tool@2.0.0");
        }
        other => panic!("expected UnsupportedAbi, got {other:?}"),
    }
}

#[test]
fn a_protocol_the_host_does_not_implement_is_refused() {
    for protocol in ["2.0", "0.9", "1.99"] {
        let (release, manifest) = release_with_manifest(|manifest| {
            manifest["protocol"] = Value::from(protocol);
        });
        let error = load(&release, &manifest).expect_err("another protocol is refused");
        assert!(
            matches!(&error, LoadError::UnsupportedAbi { protocol: p, .. } if p == protocol),
            "{protocol}: {error:?}"
        );
    }
}

#[test]
fn a_lock_naming_another_world_than_the_package_is_refused() {
    let fixture = fixture();
    let release = ScratchRelease::with_fixture();
    let mut locked = lock_entry(&fixture.manifest);
    locked.world = "p1:module/provider@1.0.0".to_string();
    let error = load_with(&release, &locked).expect_err("the lock disagrees");
    assert!(
        matches!(&error, LoadError::LockMismatch { field: "world", .. }),
        "{error:?}"
    );
}

// ------------------------------------------------------------------ duplicate identity

#[test]
fn two_packages_claiming_one_name_are_refused() {
    let fixture = fixture();
    // The collision fixture: the fixture package duplicated under a second directory,
    // both named by the release manifest.
    let release = ScratchRelease::new();
    release.add_package(FIXTURE_PACKAGE, &fixture.wasm, &fixture.manifest);
    release.add_package("p1-module-fixture-copy", &fixture.wasm, &fixture.manifest);
    release.write_manifest(&[FIXTURE_PACKAGE, "p1-module-fixture-copy"]);

    let error = Release::open(&release.modules_dir).expect_err("the release is ambiguous");
    match &error {
        LoadError::DuplicateIdentity {
            identity,
            first,
            second,
        } => {
            assert_eq!(identity, "p1/fixture");
            assert_ne!(first, second);
        }
        other => panic!("expected DuplicateIdentity, got {other:?}"),
    }
}

#[test]
fn two_names_claiming_one_digest_are_refused() {
    let fixture = fixture();
    let mut renamed = fixture.manifest.clone();
    renamed["name"] = Value::from("p1/fixture-two");
    let release = ScratchRelease::new();
    release.add_package(FIXTURE_PACKAGE, &fixture.wasm, &fixture.manifest);
    release.add_package("p1-module-fixture-two", &fixture.wasm, &renamed);
    release.write_manifest(&[FIXTURE_PACKAGE, "p1-module-fixture-two"]);

    let error = Release::open(&release.modules_dir).expect_err("one digest, two names");
    assert!(
        matches!(&error, LoadError::DuplicateIdentity { identity, .. }
            if Some(identity.as_str()) == fixture.manifest["digest"].as_str()),
        "{error:?}"
    );
}

// ------------------------------------------------------------------ official source

#[test]
fn a_package_the_release_manifest_does_not_name_is_refused_naming_its_source() {
    let fixture = fixture();
    // The package files are in the release tree, but p1's release manifest does not
    // name them: they came from somewhere else.
    let release = ScratchRelease::new();
    release.add_package(FIXTURE_PACKAGE, &fixture.wasm, &fixture.manifest);
    release.write_manifest(&[]);

    let error = load(&release, &fixture.manifest).expect_err("an unlisted package is refused");
    let source = release.package_file(FIXTURE_PACKAGE, "manifest.json");
    match &error {
        LoadError::NotOfficial {
            package, origin, ..
        } => {
            assert_eq!(package, "p1/fixture");
            assert_eq!(origin, &source.display().to_string());
        }
        other => panic!("expected NotOfficial, got {other:?}"),
    }
    assert!(
        error.to_string().contains(&source.display().to_string()),
        "the error names the source: {error}"
    );
}

#[test]
fn a_package_outside_the_p1_namespace_is_refused_naming_its_source() {
    let fixture = fixture();
    let release = ScratchRelease::with_fixture();
    let mut locked = lock_entry(&fixture.manifest);
    locked.package = "acme/fixture".to_string();
    locked.source = "/home/user/.config/p1/modules.lock".into();
    let error = load_with(&release, &locked).expect_err("a foreign namespace is refused");
    assert!(
        matches!(&error, LoadError::NotOfficial { package, origin, .. }
            if package == "acme/fixture" && origin == "/home/user/.config/p1/modules.lock"),
        "{error:?}"
    );
}

// ------------------------------------------------------------------ grants

#[test]
fn a_component_importing_more_than_its_manifest_grants_is_refused() {
    // The fixture imports `process`; a manifest that does not grant it cannot load it.
    let (release, manifest) = release_with_manifest(|manifest| {
        manifest["capabilities"] = serde_json::json!(["control", "clock"]);
    });
    let error = load(&release, &manifest).expect_err("an ungranted import is refused");
    assert!(
        matches!(&error, LoadError::ImportNotGranted { import, .. }
            if import.starts_with("p1:module/process@")),
        "{error:?}"
    );
}

#[test]
fn a_capability_outside_the_class_allocation_is_refused() {
    let (release, manifest) = release_with_manifest(|manifest| {
        manifest["capabilities"] = serde_json::json!(["control", "clock", "process", "http"]);
    });
    let error = load(&release, &manifest).expect_err("`http` is not a tool capability");
    assert!(
        matches!(&error, LoadError::CapabilityNotAllocated { capability, .. } if capability == "http"),
        "{error:?}"
    );
}

// ------------------------------------------------------------------ distinct errors

#[test]
fn each_refusal_has_its_own_error() {
    let fixture = fixture();

    let corrupted = ScratchRelease::with_fixture();
    corrupted.flip_byte(&corrupted.package_file(FIXTURE_PACKAGE, "wasm"));
    let digest = load(&corrupted, &fixture.manifest).unwrap_err();

    let (abi_release, abi_manifest) = release_with_manifest(|manifest| {
        manifest["world"] = Value::from("p1:module/tool@2.0.0");
    });
    let abi = load(&abi_release, &abi_manifest).unwrap_err();

    let colliding = ScratchRelease::new();
    colliding.add_package(FIXTURE_PACKAGE, &fixture.wasm, &fixture.manifest);
    colliding.add_package("p1-module-fixture-copy", &fixture.wasm, &fixture.manifest);
    colliding.write_manifest(&[FIXTURE_PACKAGE, "p1-module-fixture-copy"]);
    let duplicate = Release::open(&colliding.modules_dir).unwrap_err();

    let unlisted = ScratchRelease::new();
    unlisted.add_package(FIXTURE_PACKAGE, &fixture.wasm, &fixture.manifest);
    unlisted.write_manifest(&[]);
    let foreign = load(&unlisted, &fixture.manifest).unwrap_err();

    let errors = [digest, abi, duplicate, foreign];
    for (i, a) in errors.iter().enumerate() {
        for b in &errors[i + 1..] {
            assert_ne!(discriminant(a), discriminant(b), "{a:?} vs {b:?}");
            assert_ne!(a.to_string(), b.to_string());
        }
    }
}
