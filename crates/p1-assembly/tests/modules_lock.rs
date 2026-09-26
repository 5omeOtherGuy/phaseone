//! `modules.lock` parsing, validation and the override order (module name → package,
//! version, digest and ABI).

use std::path::{Path, PathBuf};

use p1_assembly::{LockedProtocol, ModulesLock, ModulesLockError, load_modules_lock};

const DIGEST: &str = "sha256:e68bceff22096a061850e37d13b4876c7799e02f32e5ec0c97553cd40fa806ed";
const OTHER_DIGEST: &str =
    "sha256:0000000000000000000000000000000000000000000000000000000000000000";

fn lock_text(module: &str, package: &str, digest: &str) -> String {
    format!(
        "format = \"p1-modules-lock/1\"\n\n[modules.{module}]\npackage = \"{package}\"\n\
         version = \"0.0.1\"\ndigest = \"{digest}\"\nworld = \"p1:module/tool@1.0.0\"\n\
         protocol = \"1.0\"\n"
    )
}

fn parse(text: &str) -> Result<ModulesLock, ModulesLockError> {
    ModulesLock::parse(Path::new("modules.lock"), text)
}

#[test]
fn an_entry_resolves_a_name_to_package_version_digest_and_abi() {
    let lock = parse(&lock_text("fixture", "p1/fixture", DIGEST)).unwrap();
    let entry = lock.resolve("fixture").expect("resolves");
    assert_eq!(entry.package, "p1/fixture");
    assert_eq!(entry.version, "0.0.1");
    assert_eq!(entry.digest, DIGEST);
    assert_eq!(entry.world, "p1:module/tool@1.0.0");
    assert_eq!(entry.protocol, LockedProtocol { major: 1, minor: 0 });
    assert!(lock.resolve("read").is_none());
}

#[test]
fn the_shipped_lock_parses() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../modules.lock");
    let text = std::fs::read_to_string(&path).expect("the repository ships modules.lock");
    ModulesLock::parse(&path, &text).expect("the shipped lock is valid");
}

#[test]
fn another_format_is_refused() {
    let text = lock_text("fixture", "p1/fixture", DIGEST).replace("/1\"", "/2\"");
    assert!(matches!(
        parse(&text),
        Err(ModulesLockError::UnsupportedFormat { found, .. }) if found == "p1-modules-lock/2"
    ));
}

#[test]
fn an_unknown_key_is_refused() {
    let text = format!(
        "{}path = \"/tmp/x.wasm\"\n",
        lock_text("fixture", "p1/fixture", DIGEST)
    );
    assert!(matches!(parse(&text), Err(ModulesLockError::Parse { .. })));
}

#[test]
fn a_package_outside_the_p1_namespace_is_refused() {
    let error = parse(&lock_text("fixture", "acme/fixture", DIGEST)).unwrap_err();
    assert!(
        matches!(&error, ModulesLockError::InvalidEntry { module, .. } if module == "fixture"),
        "{error}"
    );
    assert!(error.to_string().contains("acme/fixture"), "{error}");
}

#[test]
fn malformed_digests_and_protocols_are_refused() {
    for digest in ["e68bceff", "sha256:E68B", "md5:00", ""] {
        let error = parse(&lock_text("fixture", "p1/fixture", digest)).unwrap_err();
        assert!(
            matches!(error, ModulesLockError::InvalidEntry { .. }),
            "{digest}"
        );
    }
    let text = lock_text("fixture", "p1/fixture", DIGEST).replace("\"1.0\"", "\"1\"");
    assert!(matches!(
        parse(&text),
        Err(ModulesLockError::InvalidEntry { .. })
    ));
}

#[test]
fn a_higher_priority_lock_overrides_by_name() {
    let root = tempfile::tempdir().unwrap();
    let user = root.path().join("user");
    let shipped = root.path().join("shipped");
    for dir in [&user, &shipped] {
        std::fs::create_dir_all(dir.join("environments")).unwrap();
    }
    std::fs::write(
        user.join("modules.lock"),
        lock_text("fixture", "p1/fixture", OTHER_DIGEST),
    )
    .unwrap();
    std::fs::write(
        shipped.join("modules.lock"),
        format!(
            "{}\n[modules.other]\npackage = \"p1/other\"\nversion = \"0.0.1\"\n\
             digest = \"{DIGEST}\"\nworld = \"p1:module/tool@1.0.0\"\nprotocol = \"1.0\"\n",
            lock_text("fixture", "p1/fixture", DIGEST)
        ),
    )
    .unwrap();
    let dirs: Vec<PathBuf> = vec![user.join("environments"), shipped.join("environments")];
    let lock = load_modules_lock(&dirs).unwrap();
    assert_eq!(lock.resolve("fixture").unwrap().digest, OTHER_DIGEST);
    assert_eq!(lock.resolve("other").unwrap().package, "p1/other");
    assert_eq!(lock.iter().count(), 2);
}

#[test]
fn no_lock_file_is_the_empty_lock() {
    let root = tempfile::tempdir().unwrap();
    let lock = load_modules_lock(&[root.path().join("environments")]).unwrap();
    assert!(lock.is_empty());
}
