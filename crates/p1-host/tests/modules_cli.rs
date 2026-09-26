//! `p1 modules` (ADR-0079, S1.6): `list`, `inspect` and `verify` over a scratch share
//! directory built from the fixture package `scripts/build-modules.sh` publishes, laid out as
//! ADR-0079 ships a release: `modules/manifest.json` beside `modules/packages/…`.
//!
//! Every case runs the real binary, so the parsing, the dispatch and the exit codes are
//! exercised end to end, and every one of them runs with a scratch home and a scratch config
//! directory: no case can read a real login or a real setting.

use std::collections::BTreeMap;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

/// The fixture package `scripts/build-modules.sh` builds.
const FIXTURE_PACKAGE: &str = "p1-module-fixture";
/// Its manifest name.
const FIXTURE_NAME: &str = "p1/fixture";
/// The catalog key an environment names it by: its name without the reserved namespace.
const FIXTURE_KEY: &str = "fixture";
/// The value the no-configuration case hides in every credential and settings file: no
/// command may ever print it.
const POISON: &str = "poisoned-value-that-must-never-be-printed";

fn shipped(relative: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join(relative)
}

/// One artifact of the built fixture, or a failure that says how to build it.
fn fixture_file(extension: &str) -> PathBuf {
    let path = shipped("modules/target/p1-modules")
        .join(FIXTURE_PACKAGE)
        .join(format!("{FIXTURE_PACKAGE}.{extension}"));
    assert!(
        path.is_file(),
        "the fixture artifact {} is missing: run scripts/build-modules.sh first",
        path.display()
    );
    path
}

/// The fixture package's own manifest, as the build wrote it.
fn fixture_manifest() -> serde_json::Value {
    let text = std::fs::read_to_string(fixture_file("manifest.json")).unwrap();
    serde_json::from_str(&text).unwrap()
}

/// The digest the release manifest pins for the fixture.
fn fixture_digest() -> String {
    fixture_manifest()["digest"].as_str().unwrap().to_string()
}

/// A scratch release: a share directory whose `modules/` set holds the fixture package as the
/// build published it, a scratch home and a scratch config directory.
struct Scratch {
    share: tempfile::TempDir,
    home: tempfile::TempDir,
    config: tempfile::TempDir,
}

impl Scratch {
    fn new() -> Self {
        let scratch = Self {
            share: tempfile::tempdir().unwrap(),
            home: tempfile::tempdir().unwrap(),
            config: tempfile::tempdir().unwrap(),
        };
        std::fs::create_dir_all(scratch.package_dir()).unwrap();
        for extension in ["wasm", "imports"] {
            std::fs::copy(
                fixture_file(extension),
                scratch
                    .package_dir()
                    .join(format!("{FIXTURE_PACKAGE}.{extension}")),
            )
            .unwrap();
        }
        scratch.write_components(&[scratch.fixture_entry(FIXTURE_NAME)]);
        scratch
    }

    /// The share directory, as ADR-0079 ships it.
    fn share(&self) -> &Path {
        self.share.path()
    }

    /// The module set inside the share directory.
    fn modules(&self) -> PathBuf {
        self.share().join("modules")
    }

    /// The fixture package's directory inside the module set.
    fn package_dir(&self) -> PathBuf {
        self.modules().join("packages").join(FIXTURE_PACKAGE)
    }

    /// The installed component.
    fn component(&self) -> PathBuf {
        self.package_dir().join(format!("{FIXTURE_PACKAGE}.wasm"))
    }

    fn home(&self) -> &Path {
        self.home.path()
    }

    fn config(&self) -> &Path {
        self.config.path()
    }

    /// One manifest entry for the fixture under the manifest name `name`. Its bytes and its
    /// digest are the fixture package's, so a case changes only the field it is about.
    fn fixture_entry(&self, name: &str) -> serde_json::Value {
        let package = fixture_manifest();
        serde_json::json!({
            "name": name,
            "digest": package["digest"],
            "path": format!("packages/{FIXTURE_PACKAGE}/{FIXTURE_PACKAGE}.wasm"),
            "kind": package["kind"],
            "world": package["world"],
            "protocol": package["protocol"],
            "capabilities": package["capabilities"],
            "variant": package["variant"],
        })
    }

    /// Writes the release manifest that lists `components`.
    fn write_components(&self, components: &[serde_json::Value]) {
        let manifest = serde_json::json!({
            "format": "p1-release-manifest/1",
            "components": components,
        });
        std::fs::write(self.modules().join("manifest.json"), manifest.to_string()).unwrap();
    }

    /// Writes one environment naming the fixture, as the shipped environments name a tool.
    fn environment(&self, name: &str) {
        let dir = self.config().join("environments").join(name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("environment.toml"),
            format!(
                "family = \"{name}\"\nprovider = \"fake\"\nmodel = \"fake-1\"\n\n\
                 [[tools]]\nmodule = \"{FIXTURE_KEY}\"\n"
            ),
        )
        .unwrap();
        std::fs::write(dir.join("prompt.md"), "prompt").unwrap();
    }

    /// Turns one byte of the installed component into another, past its 8-byte header: the
    /// bytes then hash to something the manifest does not pin.
    fn corrupt_component(&self) {
        let path = self.component();
        let mut bytes = std::fs::read(&path).unwrap();
        bytes[8] ^= 0xff;
        std::fs::write(&path, bytes).unwrap();
    }

    /// Drops the imported-interface record, as a release archive does: it ships the component
    /// alone (ADR-0079).
    fn drop_imports(&self) {
        std::fs::remove_file(
            self.package_dir()
                .join(format!("{FIXTURE_PACKAGE}.imports")),
        )
        .unwrap();
    }

    /// Runs the real binary on `args` with this scratch home, config directory and no ambient
    /// credential variable, and returns what it printed.
    fn run(&self, args: &[&str]) -> Output {
        self.run_with(args, None)
    }

    /// As [`Scratch::run`], with `XDG_CONFIG_HOME` set to `xdg` when it is given.
    fn run_with(&self, args: &[&str], xdg: Option<&Path>) -> Output {
        let mut command = Command::new(env!("CARGO_BIN_EXE_p1"));
        command
            .env("HOME", self.home())
            .env("P1_CONFIG_DIR", self.config());
        match xdg {
            Some(xdg) => command.env("XDG_CONFIG_HOME", xdg),
            None => command.env_remove("XDG_CONFIG_HOME"),
        };
        for name in [
            "XDG_DATA_HOME",
            "P1_ENVIRONMENTS_DIR",
            "PI_CODING_AGENT_DIR",
            "CLAUDE_CONFIG_DIR",
            "CODEX_HOME",
            "OPENCODE_API_KEY",
            "OPENCODE_GO_1_API_KEY",
            "OPENCODE_GO_2_API_KEY",
            "OPENCODE_GO_3_API_KEY",
            "OPENCODE_ZEN_1_API_KEY",
            "OPENCODE_ZEN_2_API_KEY",
            "OPENCODE_ZEN_3_API_KEY",
            "OPENCODE_ZEN_API_KEY",
            "CLINE_PASS_1_API_KEY",
            "CLINE_PASS_2_API_KEY",
            "ZAI_API_KEY",
            "KIMI_API_KEY",
        ] {
            command.env_remove(name);
        }
        command.args(args).output().unwrap()
    }

    /// The share directory as an argument.
    fn root(&self) -> String {
        self.share().to_str().unwrap().to_string()
    }
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).to_string()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).to_string()
}

fn code(output: &Output) -> i32 {
    output
        .status
        .code()
        .unwrap_or_else(|| panic!("p1 was killed: {}", stderr(output)))
}

/// The `inspect` report as key → value, so a case asserts a field without counting the
/// columns' alignment.
fn inspect_fields(report: &str) -> BTreeMap<String, String> {
    report
        .lines()
        .map(|line| {
            let (key, value) = line
                .split_once(' ')
                .unwrap_or_else(|| panic!("{line:?} is not a `key  value` line"));
            (key.to_string(), value.trim().to_string())
        })
        .collect()
}

/// The one line of `list` for the fixture.
fn fixture_line(report: &str) -> String {
    report
        .lines()
        .find(|line| line.starts_with(FIXTURE_NAME))
        .unwrap_or_else(|| panic!("no line for {FIXTURE_NAME} in:\n{report}"))
        .to_string()
}

#[test]
fn lists_every_installed_package_with_its_digest_and_selection() {
    let scratch = Scratch::new();
    let done = scratch.run(&["modules", "list", "--root", &scratch.root()]);
    assert_eq!(code(&done), 0, "{}", stderr(&done));

    let report = stdout(&done);
    assert_eq!(report.lines().count(), 1, "one line per package:\n{report}");
    let line = fixture_line(&report);
    assert!(line.contains("tool"), "{line}");
    assert!(line.contains("1.0"), "the protocol it speaks: {line}");
    assert!(
        line.contains(&fixture_digest()),
        "the digest it was installed with: {line}"
    );
    assert!(
        line.contains("unselected"),
        "no shipped environment names the fixture: {line}"
    );
}

#[test]
fn list_names_the_environment_that_selects_a_module() {
    let scratch = Scratch::new();
    scratch.environment("fixture-env");

    let done = scratch.run(&["modules", "list", "--root", &scratch.root()]);
    assert_eq!(code(&done), 0, "{}", stderr(&done));
    let line = fixture_line(&stdout(&done));
    assert!(line.contains("selected by fixture-env"), "{line}");
    assert!(!line.contains("unselected"), "{line}");
}

#[test]
fn inspect_reports_the_manifest_fields_the_imports_and_the_grants() {
    let scratch = Scratch::new();
    let done = scratch.run(&[
        "modules",
        "inspect",
        FIXTURE_NAME,
        "--root",
        &scratch.root(),
    ]);
    assert_eq!(code(&done), 0, "{}", stderr(&done));
    let report = stdout(&done);
    let fields = inspect_fields(&report);

    assert_eq!(fields["name"], FIXTURE_NAME);
    assert_eq!(fields["kind"], "tool");
    assert_eq!(fields["world"], "p1:module/tool@1.0.0");
    assert_eq!(fields["protocol"], "1.0");
    assert_eq!(fields["variant"], "default");
    assert_eq!(fields["digest"], fixture_digest());
    assert_eq!(
        fields["size"],
        format!(
            "{} bytes",
            std::fs::metadata(scratch.component()).unwrap().len()
        )
    );
    assert_eq!(fields["capabilities"], "control, clock, process");
    assert_eq!(fields["grants"], "control, clock, process");
    assert_eq!(fields["identity"], "p1/fixture@default");
    assert_eq!(
        fields["imports"],
        "p1:module/clock@1.0.0, p1:module/control@1.0.0, p1:module/process@1.0.0, \
         p1:module/types@1.0.0"
    );

    // The module set itself is a valid `--root` too, not only the share directory above it.
    let done = scratch.run(&[
        "modules",
        "inspect",
        FIXTURE_NAME,
        "--root",
        scratch.modules().to_str().unwrap(),
    ]);
    assert_eq!(code(&done), 0, "{}", stderr(&done));
    assert_eq!(stdout(&done), report);

    // A set that ships the component alone (a release archive) has no import record to read,
    // and says so instead of inventing a list.
    let released = Scratch::new();
    released.drop_imports();
    let done = released.run(&[
        "modules",
        "inspect",
        FIXTURE_NAME,
        "--root",
        &released.root(),
    ]);
    assert_eq!(code(&done), 0, "{}", stderr(&done));
    assert_eq!(
        inspect_fields(&stdout(&done))["imports"],
        format!(
            "not recorded (the module set has no packages/{FIXTURE_PACKAGE}/{FIXTURE_PACKAGE}.imports)"
        )
    );
}

/// An unknown name is an explicit error, never an empty report.
#[test]
fn inspect_refuses_a_name_the_release_does_not_ship() {
    let scratch = Scratch::new();
    let done = scratch.run(&["modules", "inspect", "p1/absent", "--root", &scratch.root()]);
    assert_eq!(code(&done), 1, "{}", stderr(&done));
    assert!(stdout(&done).is_empty(), "{}", stdout(&done));
    let errors = stderr(&done);
    assert!(errors.contains("p1/absent"), "{errors}");
    assert!(errors.contains("not in the module set"), "{errors}");
}

#[test]
fn verify_accepts_a_complete_staged_set() {
    let scratch = Scratch::new();
    let done = scratch.run(&["modules", "verify", "--root", &scratch.root()]);
    assert_eq!(code(&done), 0, "{}", stderr(&done));
    let report = stdout(&done);
    assert!(
        report.contains(&format!("{FIXTURE_NAME} ok {}", fixture_digest())),
        "{report}"
    );
    assert!(
        report.contains("modules verify: 1 ok, 0 failed"),
        "{report}"
    );
}

#[test]
fn verify_fails_a_corrupted_component() {
    let scratch = Scratch::new();
    scratch.corrupt_component();
    let done = scratch.run(&["modules", "verify", "--root", &scratch.root()]);
    assert_eq!(code(&done), 1, "{}", stderr(&done));
    let report = stdout(&done);
    assert!(
        report.contains(&format!("{FIXTURE_NAME} FAILED")),
        "{report}"
    );
    assert!(report.contains("the bytes hash to"), "{report}");
    assert!(report.contains(&fixture_digest()), "{report}");
    assert!(
        report.contains("modules verify: 0 ok, 1 failed"),
        "{report}"
    );
}

#[test]
fn verify_fails_a_missing_component() {
    let scratch = Scratch::new();
    std::fs::remove_file(scratch.component()).unwrap();
    let done = scratch.run(&["modules", "verify", "--root", &scratch.root()]);
    assert_eq!(code(&done), 1, "{}", stderr(&done));
    let report = stdout(&done);
    assert!(
        report.contains(&format!("{FIXTURE_NAME} FAILED")),
        "{report}"
    );
    assert!(report.contains("cannot read"), "{report}");
}

/// The loader's manifest-only checks (freeze item 6) are the checks `verify` makes without
/// compiling, so every field the loader would refuse fails a verification.
#[test]
fn verify_refuses_the_manifest_fields_the_loader_would_refuse() {
    let cases = [
        (
            "kind",
            serde_json::json!("plugin"),
            "kind plugin is not a module class this runtime speaks",
        ),
        (
            "world",
            serde_json::json!("p1:module/provider@1.0.0"),
            "world p1:module/provider@1.0.0 is not p1:module/tool@1.0.0",
        ),
        (
            "protocol",
            serde_json::json!("2.0"),
            "protocol 2.0 is refused, this runtime speaks protocol major 1",
        ),
        (
            "capabilities",
            serde_json::json!(["http"]),
            "capability http cannot be linked by this runtime",
        ),
    ];
    for (field, value, expected) in cases {
        let scratch = Scratch::new();
        let mut entry = scratch.fixture_entry(FIXTURE_NAME);
        entry[field] = value;
        scratch.write_components(&[entry]);

        let done = scratch.run(&["modules", "verify", "--root", &scratch.root()]);
        assert_eq!(code(&done), 1, "{field}: {}", stderr(&done));
        assert!(
            stdout(&done).contains(expected),
            "{field}: {expected:?} missing from:\n{}",
            stdout(&done)
        );
    }
}

/// The fixture entry with `notices` granted beside its own capabilities: an interface
/// `modules/capabilities.toml` allocates to the tool class but this runtime has no native
/// service to link yet. Shipped packages grant such interfaces before their streams link them
/// (`workers-*`/`workflows` did until S6.7), so the installer's verify must be able to report
/// this without refusing the release (S1.6.1).
fn unlinkable_entry(scratch: &Scratch) -> serde_json::Value {
    let mut entry = scratch.fixture_entry(FIXTURE_NAME);
    entry["capabilities"] = serde_json::json!(["control", "clock", "process", "notices"]);
    entry
}

/// The default `verify` still fails an unlinkable grant exactly as it always did, and
/// `--integrity-only` reports it as `UNLINKED` and passes the entry (S1.6.1).
#[test]
fn verify_reports_an_unlinkable_grant_and_integrity_only_passes_it() {
    let scratch = Scratch::new();
    scratch.write_components(&[unlinkable_entry(&scratch)]);

    let strict = scratch.run(&["modules", "verify", "--root", &scratch.root()]);
    assert_eq!(code(&strict), 1, "{}", stderr(&strict));
    let report = stdout(&strict);
    assert!(
        report.contains(&format!(
            "{FIXTURE_NAME} FAILED capability notices cannot be linked by this runtime"
        )),
        "the default message is unchanged:\n{report}"
    );
    assert!(
        report.contains("modules verify: 0 ok, 1 failed"),
        "{report}"
    );

    // `--integrity-only`: the digest and the manifest fields are what it is about, and the
    // one grant this runtime cannot link is named rather than failed.
    let relaxed = scratch.run(&[
        "modules",
        "verify",
        "--integrity-only",
        "--root",
        &scratch.root(),
    ]);
    assert_eq!(code(&relaxed), 0, "{}", stderr(&relaxed));
    let report = stdout(&relaxed);
    assert!(
        report.contains(&format!("UNLINKED notices ({FIXTURE_NAME})")),
        "{report}"
    );
    assert!(
        report.contains(&format!("{FIXTURE_NAME} ok {}", fixture_digest())),
        "{report}"
    );
    assert!(
        report.contains("modules verify: 1 ok, 0 failed"),
        "{report}"
    );
    assert!(
        !report.contains("FAILED") && !report.contains("cannot be linked"),
        "an unlinkable grant is reported, never failed, in this mode:\n{report}"
    );

    // The flag combines with `--root` at either level and in either order: the module set
    // itself is as good a root as the share directory above it.
    let reordered = scratch.run(&[
        "modules",
        "verify",
        "--root",
        scratch.modules().to_str().unwrap(),
        "--integrity-only",
    ]);
    assert_eq!(code(&reordered), 0, "{}", stderr(&reordered));
    assert_eq!(stdout(&reordered), report);
}

/// `--integrity-only` passes linkability alone: the bytes and every other metadata problem
/// fail it exactly as they fail the default mode.
#[test]
fn verify_integrity_only_still_fails_a_corrupted_component() {
    let scratch = Scratch::new();
    scratch.write_components(&[unlinkable_entry(&scratch)]);
    scratch.corrupt_component();

    let done = scratch.run(&[
        "modules",
        "verify",
        "--integrity-only",
        "--root",
        &scratch.root(),
    ]);
    assert_eq!(code(&done), 1, "{}", stderr(&done));
    let report = stdout(&done);
    assert!(
        report.contains(&format!("{FIXTURE_NAME} FAILED the bytes hash to")),
        "{report}"
    );
    assert!(report.contains(&fixture_digest()), "{report}");
    assert!(
        report.contains("modules verify: 0 ok, 1 failed"),
        "{report}"
    );
}

/// A manifest field the loader would refuse still fails `--integrity-only`: the mode passes
/// only the linkability of a grant.
#[test]
fn verify_integrity_only_still_fails_the_manifest_fields_the_loader_would_refuse() {
    let cases = [
        (
            "kind",
            serde_json::json!("plugin"),
            "kind plugin is not a module class this runtime speaks",
        ),
        (
            "world",
            serde_json::json!("p1:module/provider@1.0.0"),
            "world p1:module/provider@1.0.0 is not p1:module/tool@1.0.0",
        ),
        (
            "protocol",
            serde_json::json!("2.0"),
            "protocol 2.0 is refused, this runtime speaks protocol major 1",
        ),
    ];
    for (field, value, expected) in cases {
        let scratch = Scratch::new();
        let mut entry = scratch.fixture_entry(FIXTURE_NAME);
        entry[field] = value;
        scratch.write_components(&[entry]);

        let done = scratch.run(&[
            "modules",
            "verify",
            "--integrity-only",
            "--root",
            &scratch.root(),
        ]);
        assert_eq!(code(&done), 1, "{field}: {}", stderr(&done));
        let report = stdout(&done);
        assert!(
            report.contains(expected),
            "{field}: {expected:?} missing from:\n{report}"
        );
        assert!(
            report.contains("modules verify: 0 ok, 1 failed"),
            "{field}: {report}"
        );
    }
}

/// `--integrity-only` narrows a verification only: on `list` or `inspect` it changes nothing,
/// so the parser refuses it rather than taking a flag the caller meant for something else.
#[test]
fn integrity_only_is_refused_on_list_and_inspect() {
    let scratch = Scratch::new();
    for args in [
        vec![
            "modules",
            "list",
            "--integrity-only",
            "--root",
            &scratch.root(),
        ],
        vec![
            "modules",
            "inspect",
            FIXTURE_NAME,
            "--integrity-only",
            "--root",
            &scratch.root(),
        ],
    ] {
        let done = scratch.run(&args);
        assert_eq!(code(&done), 2, "{}", stderr(&done));
        assert!(stdout(&done).is_empty(), "{}", stdout(&done));
        let errors = stderr(&done);
        assert!(
            errors.contains("--integrity-only is only for `p1 modules verify`"),
            "{errors}"
        );
    }
}

/// The class names and the interface names the frozen boundary table `modules/capabilities.toml`
/// lists: the one file a new class or interface is added to, so driving both commands over them
/// is what catches `verify`'s copies of the loader's class and capability lists drifting apart
/// from the loader's own.
fn published_boundary() -> (Vec<String>, Vec<String>) {
    let text = std::fs::read_to_string(shipped("modules/capabilities.toml")).unwrap();
    let table: toml::Value = toml::from_str(&text).unwrap();
    let mut classes = Vec::new();
    let mut interfaces = Vec::new();
    for (key, value) in table.as_table().unwrap() {
        if key == "type-only" {
            // `types` and `worker-types` grant nothing; a manifest naming either as a grant is
            // refused by both checks, which is the agreement this guard asserts.
            for name in value.as_array().unwrap() {
                interfaces.push(name.as_str().unwrap().to_string());
            }
            continue;
        }
        classes.push(key.clone());
        for name in value.get("imports").unwrap().as_array().unwrap() {
            interfaces.push(name.as_str().unwrap().to_string());
        }
    }
    interfaces.sort();
    interfaces.dedup();
    (classes, interfaces)
}

/// ADR-0079 has an installer run `verify` on a staged set before that set replaces the installed
/// one, so `verify`'s manifest-only checks must be the loader's own: a set `verify` refuses must
/// be one `Loader::load` refuses too, and a set it accepts must load. `inspect` goes through the
/// real loader, so driving both commands over one manifest and asserting their verdicts agree is
/// the drift guard for `verify`'s copies of the loader's class and capability lists (finding
/// S1.6-2): a class or a linkable capability added to the runtime without them makes one command
/// accept what the other refuses.
#[test]
fn verify_and_the_loader_agree_on_every_manifest_field() {
    let fixture = fixture_manifest();
    let fixture_kind = fixture["kind"].as_str().unwrap().to_string();
    let fixture_world = fixture["world"].as_str().unwrap().to_string();
    let fixture_capabilities = fixture["capabilities"].clone();
    let (classes, interfaces) = published_boundary();
    assert!(
        classes.len() >= 5,
        "the boundary table's classes: {classes:?}"
    );
    assert!(
        interfaces.len() >= 10,
        "the boundary table's interfaces: {interfaces:?}"
    );

    // The world of `class`, under the same version as the fixture's own world, so the guard
    // keeps exercising the class check if the WIT version moves.
    let world_of = |class: &str| {
        fixture_world.replacen(&format!("/{fixture_kind}@"), &format!("/{class}@"), 1)
    };

    let base = Scratch::new().fixture_entry(FIXTURE_NAME);
    // The exit codes of `verify` and of `inspect` (which loads) over one manifest.
    let verdicts = |entry: &serde_json::Value| -> (i32, i32) {
        let scratch = Scratch::new();
        scratch.write_components(std::slice::from_ref(entry));
        let verified = scratch.run(&["modules", "verify", "--root", &scratch.root()]);
        let inspected = scratch.run(&[
            "modules",
            "inspect",
            FIXTURE_NAME,
            "--root",
            &scratch.root(),
        ]);
        (code(&verified), code(&inspected))
    };
    let agree = |what: String, entry: &serde_json::Value| {
        let (verified, inspected) = verdicts(entry);
        assert_eq!(
            verified == 0,
            inspected == 0,
            "{what}: verify said {verified}, the loader said {inspected}"
        );
    };

    // The fixture's own entry: the set both commands must accept.
    agree("the fixture manifest".to_string(), &base);

    for class in &classes {
        let mut entry = base.clone();
        entry["kind"] = serde_json::json!(class);
        entry["world"] = serde_json::json!(world_of(class));
        agree(format!("class {class}"), &entry);
    }

    // A class neither speaks.
    let mut entry = base.clone();
    entry["kind"] = serde_json::json!("plugin");
    agree("kind plugin".to_string(), &entry);

    // The fixture's class under another class's world.
    if let Some(other) = classes.iter().find(|class| class.as_str() != fixture_kind) {
        let mut entry = base.clone();
        entry["world"] = serde_json::json!(world_of(other));
        agree(
            format!("world of class {other} under kind {fixture_kind}"),
            &entry,
        );
    }

    // The protocol's `major.minor` shape, including the edges `u32::parse` alone accepts.
    for protocol in [
        "1.0", "1.9", "01.0", "1.+5", "1.-0", "1.", ".0", "1.0.0", "2.0", "1", "", "a.0", "+1.0",
    ] {
        let mut entry = base.clone();
        entry["protocol"] = serde_json::json!(protocol);
        agree(format!("protocol {protocol:?}"), &entry);
    }

    // Every published interface, granted beside the fixture's own: a linkable one both accept,
    // anything else both refuse.
    for interface in &interfaces {
        let mut grants = fixture_capabilities.as_array().unwrap().clone();
        grants.push(serde_json::json!(interface));
        let mut entry = base.clone();
        entry["capabilities"] = serde_json::Value::Array(grants);
        agree(format!("capability {interface}"), &entry);
    }
}

/// Identity is the digest, so one package's bytes under two names is a duplicate the release
/// must not carry.
#[test]
fn verify_fails_a_duplicate_identity() {
    let scratch = Scratch::new();
    let mut copy = scratch.fixture_entry(FIXTURE_NAME);
    copy["name"] = serde_json::json!("p1/fixture-copy");
    scratch.write_components(&[scratch.fixture_entry(FIXTURE_NAME), copy]);

    let done = scratch.run(&["modules", "verify", "--root", &scratch.root()]);
    assert_eq!(code(&done), 1, "{}", stderr(&done));
    let report = stdout(&done);
    assert!(report.contains("duplicate identity"), "{report}");
    assert!(report.contains("p1/fixture-copy FAILED"), "{report}");
    assert!(
        report.contains("modules verify: 1 ok, 1 failed"),
        "{report}"
    );
}

/// ADR-0079 has an installer run `verify` on a staged set in an install with no session and no
/// configured model, so it must read neither a credential nor a setting: every file that could
/// hold one exists, is unreadable (mode 000) and holds a value no command may print.
#[test]
fn verify_reads_no_credential_and_no_user_configuration() {
    let scratch = Scratch::new();
    let xdg = scratch.home().join("xdg");
    let homes = [
        // HOME's config directory, and the XDG one beside it.
        scratch.home().join(".config").join("p1"),
        xdg.join("p1"),
    ];
    let mut poisoned: Vec<PathBuf> = vec![scratch.config().to_path_buf()];
    poisoned.extend(homes.iter().cloned());
    for directory in &poisoned {
        std::fs::create_dir_all(directory).unwrap();
        for name in ["auth.json", "settings.toml"] {
            let path = directory.join(name);
            std::fs::write(&path, format!("{{\"api_key\": \"{POISON}\"}}\n")).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o000)).unwrap();
        }
    }

    // Once with the home's own config directory, once with XDG_CONFIG_HOME; both unreadable.
    for xdg_config in [None, Some(xdg.as_path())] {
        let done = scratch.run_with(
            &["modules", "verify", "--root", &scratch.root()],
            xdg_config,
        );
        assert_eq!(
            code(&done),
            0,
            "verify must not need a credential or a setting: {}",
            stderr(&done)
        );
        let printed = stdout(&done) + &stderr(&done);
        assert!(printed.contains("1 ok, 0 failed"), "{printed}");
        for forbidden in [POISON, "auth.json", "settings.toml"] {
            assert!(
                !printed.contains(forbidden),
                "{forbidden:?} reached the output:\n{printed}"
            );
        }
        for directory in &poisoned {
            assert!(
                !printed.contains(&directory.display().to_string()),
                "{} reached the output:\n{printed}",
                directory.display()
            );
        }
    }
}
