//! S7.7 and S7.7.2: a real staged release, installed and run offline from its binary plus the
//! modules it ships.
//!
//! `scripts/stage-release.sh` stages the p1 binary and the built module packages into the
//! four release assets; `scripts/install.sh` installs them from a `file://` release base into
//! a temporary prefix; and the installed binary is then run with the repository root and the
//! real HOME covered by a tmpfs, so neither the network nor the checkout's `environments/`
//! (the debug build's source-tree fallback) nor the build outputs can be reached: the
//! installed `<prefix>/share/p1` is the only data the run finds. The installed module set is
//! also loaded through `p1-module-runtime`'s release manifest and loader and the shipped
//! fixture is executed once, which is what proves the shipped manifest and components are
//! what the runtime accepts.
//!
//! The second case (S7.7.2) runs a headless turn whose tool call is served by a shipped
//! component. The run's own `$P1_CONFIG_DIR` carries the overrides D083b allows: an
//! environment that selects the `read` key, a `modules.lock` pinning that key to the
//! installed manifest's `p1/read` entry, and a route for an `openai-chat` provider that is a
//! loopback listener in this test process, so no test provider ships and nothing but the
//! installed share is read. A second install with one corrupted package byte proves the run
//! refuses assembly at the loader rather than serving a tool call.
//!
//! The cases need a real `bwrap`; like `crates/p1-host/tests/sandbox.rs` they print
//! `SKIP: bwrap unusable here` and continue with the assertions that need no sandbox when
//! the probe fails. They never skip silently: a missing binary, a missing module build or a
//! missing toolchain fails the case with the command to run.

use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, SystemTime};

use p1_contracts::serde_json::{self, Value, json};
use p1_contracts::{CancellationToken, ToolContext, ToolStatus};
use p1_module_runtime::{
    Digest, ExecutionLimits, LoadError, Loader, ReleaseManifest, Services, wasm_tool,
};
use p1_module_tests::{
    FIXTURE_NAME, binary_names_checkout, call, fake_processes, p1_binary_inputs,
};
use p1_redact::MaskCounter;
use tempfile::TempDir;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// The four release assets, in the order the workflow names them.
const ASSETS: [&str; 4] = [
    "p1-linux-x86_64",
    "p1-linux-x86_64.sha256",
    "p1-share.tar.gz",
    "p1-share.tar.gz.sha256",
];

/// A shipped environment: the installed run must resolve it from its own share directory.
const ENVIRONMENT: &str = "claude";

/// The tools `scripts/install.sh` runs for a release install. A farm of symlinks to exactly
/// these is the PATH the install sees, so `gh` is genuinely absent and the curl fallback —
/// here over `file://` — is the path taken.
const FARM_TOOLS: &[&str] = &[
    "mktemp",
    "sha256sum",
    "cut",
    "awk",
    "tar",
    "gzip",
    "cp",
    "mv",
    "mkdir",
    "rm",
    "chmod",
    "basename",
    "dirname",
    "env",
    "cat",
    "python3",
    "curl",
];

/// The user environment the module case writes into the run's `$P1_CONFIG_DIR` (D083b): a
/// user-selected extra module beside the shipped set, never a shipped lock entry.
const MODULE_ENV: &str = "installed-read";
/// The route override's id: a new route, so no shipped route file changes.
const LOOPBACK_ROUTE: &str = "s7-installed-loopback";
/// The shipped profile the route override binds.
const LOOPBACK_PROFILE: &str = "deepseek-v4.1-flash";
/// The wire model the route override names.
const LOOPBACK_MODEL: &str = "s7-installed-release";
/// The module key the lock selects the shipped `read` package under.
const READ_KEY: &str = "read";
/// The installed package the key resolves to.
const READ_PACKAGE: &str = "p1/read";
/// The file the scripted provider's tool call reads, in the run's cwd.
const MARKER_FILE: &str = "marker.txt";
/// The headless prompt: it lets the scripted answer be the whole model behaviour.
const PROMPT: &str = "Read marker.txt and report the marker line it holds.";

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_staged_release_installs_and_runs_offline() {
    let root = repo_root();
    let p1 = p1_binary(&root);
    let short = version_short_sha(&p1);
    let commit = resolve_commit(&root, &short);
    let modules = module_outputs(&root);

    let scratch = scratch_dir(&root);
    let layout = Layout::new(&root, scratch.path());
    let tag = format!("main-{short}");

    // --- stage ---------------------------------------------------------------
    let out = layout.scratch.join("dist");
    stage_release(&root, &p1, &out, &commit, &tag, &modules);
    for asset in ASSETS {
        assert!(
            out.join(asset).is_file(),
            "{} is missing",
            out.join(asset).display()
        );
    }

    // A release base that serves the four assets, exactly as the published release does. The
    // URL is file://, so no network is reached; the sha256 files are the installer's.
    let published = layout.base.join("download").join(&tag);
    fs::create_dir_all(&published).expect("release base");
    for asset in ASSETS {
        fs::copy(out.join(asset), published.join(asset)).expect("publish the asset");
    }

    // --- install -------------------------------------------------------------
    install_release(&layout, &tag);

    let bin = layout.prefix.join("bin/p1");
    let modules_root = layout.prefix.join("share/p1/modules");
    for dir in ["environments", "routes", "profiles", "modules"] {
        assert!(
            layout.prefix.join("share/p1").join(dir).is_dir(),
            "the installed share has no {dir}/"
        );
    }

    // --- the installed set is what the manifest says --------------------------
    let manifest_text =
        fs::read_to_string(modules_root.join("manifest.json")).expect("installed manifest");
    let manifest: Value = serde_json::from_str(&manifest_text).expect("installed manifest JSON");
    assert_eq!(manifest["format"], "p1-release-manifest/1");
    assert_eq!(manifest["commit"].as_str(), Some(commit.as_str()));
    assert_eq!(manifest["tag"].as_str(), Some(tag.as_str()));
    assert_eq!(
        manifest["environment_locks"].as_array().map(Vec::len),
        Some(0)
    );

    let native = manifest["native"]["sha256"]
        .as_str()
        .expect("native.sha256");
    assert_eq!(
        sha256_of(&bin),
        format!("sha256:{native}"),
        "the installed binary is not the one the manifest pins"
    );
    verify_installed_components(&modules_root, &manifest);
    assert!(
        manifest["components"]
            .as_array()
            .expect("components")
            .iter()
            .any(|entry| entry["name"] == FIXTURE_NAME),
        "{FIXTURE_NAME} is not in the installed components"
    );

    // --- the runtime accepts what was installed -------------------------------
    load_every_installed_component(&modules_root).await;

    // --- the installed binary, offline, with the checkout out of reach --------
    let real_home = std::env::var_os("HOME").map(PathBuf::from);
    if !bwrap_usable() {
        eprintln!("SKIP: bwrap unusable here");
    } else {
        let env = run_env(&layout);
        let version = sandboxed(
            &layout,
            &root,
            real_home.as_deref(),
            &bin,
            &["--version"],
            &env,
            false,
        );
        assert!(
            version.status.success(),
            "installed p1 --version exited {}: {}",
            version.status,
            String::from_utf8_lossy(&version.stderr)
        );
        let text = String::from_utf8_lossy(&version.stdout).into_owned();
        assert!(
            text.contains(&short),
            "p1 --version must name the release commit {short}: {text}"
        );

        let shown = sandboxed(
            &layout,
            &root,
            real_home.as_deref(),
            &bin,
            &["env", "show", ENVIRONMENT],
            &env,
            false,
        );
        assert!(
            shown.status.success(),
            "installed p1 env show {ENVIRONMENT} exited {}: {}",
            shown.status,
            String::from_utf8_lossy(&shown.stderr)
        );
        let shown_text = String::from_utf8_lossy(&shown.stdout).into_owned();
        assert!(
            shown_text.contains(ENVIRONMENT),
            "env show {ENVIRONMENT} must resolve from the installed share: {shown_text}"
        );
    }
}

/// S7.7.2: a headless turn whose tool call is served by a shipped component, offline, with
/// the checkout out of reach.
///
/// Every override lives in the run's own `$P1_CONFIG_DIR` (D083b): an environment that names
/// the `read` module key, a `modules.lock` pinning that key to the installed manifest's
/// `p1/read` entry (package, version, digest, world and protocol read from
/// `<prefix>/share/p1/modules/manifest.json`, never from the checkout), and a route for an
/// `openai-chat` provider that is a loopback listener in this test process. The provider
/// answers one tool call and then a final text; the tool reads a marker file written into the
/// run's cwd, so the journal's tool result carries the component's own answer.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_installed_release_serves_a_tool_call_from_its_shipped_read_component() {
    let root = repo_root();
    let p1 = p1_binary(&root);
    let short = version_short_sha(&p1);
    let commit = resolve_commit(&root, &short);
    let modules = module_outputs(&root);

    let scratch = scratch_dir(&root);
    let layout = Layout::new(&root, scratch.path());
    let tag = format!("main-{short}");
    stage_and_install(&root, &layout, &p1, &commit, &tag, &modules);

    // The installed release's own entry for the package the lock selects.
    let modules_root = layout.prefix.join("share/p1/modules");
    let manifest_text =
        fs::read_to_string(modules_root.join("manifest.json")).expect("installed manifest");
    let manifest: Value = serde_json::from_str(&manifest_text).expect("installed manifest JSON");
    let entry = manifest["components"]
        .as_array()
        .expect("components")
        .iter()
        .find(|entry| entry["name"] == READ_PACKAGE)
        .unwrap_or_else(|| panic!("the installed release ships no {READ_PACKAGE}"))
        .clone();
    let field = |key: &str| {
        entry[key]
            .as_str()
            .unwrap_or_else(|| panic!("the installed {READ_PACKAGE} entry has no {key}"))
            .to_owned()
    };
    let package = field("name");
    let digest = field("digest");
    let world = field("world");
    let protocol = field("protocol");
    // The release manifest carries no version of its own, so the lock records the version the
    // installed binary names — still never anything from the checkout.
    let version = binary_version(&p1);

    // The environment, its prompt and the shipped profile the route binds, all in the run's
    // config directory.
    let environment_dir = layout.config.join("environments").join(MODULE_ENV);
    fs::create_dir_all(&environment_dir).expect("environment directory");
    fs::write(
        environment_dir.join("environment.toml"),
        format!(
            "route   = \"{LOOPBACK_ROUTE}\"\nprofile = \"{LOOPBACK_PROFILE}\"\n\n\
             [[tools]]\nmodule = \"{READ_KEY}\"\n"
        ),
    )
    .expect("environment file");
    fs::write(
        environment_dir.join("prompt.md"),
        "You are the installed-release test agent.\n\nTools: {{tool_names}}\n",
    )
    .expect("prompt file");
    let profiles = layout.config.join("profiles");
    fs::create_dir_all(&profiles).expect("profiles directory");
    fs::copy(
        layout
            .prefix
            .join("share/p1/profiles")
            .join(format!("{LOOPBACK_PROFILE}.toml")),
        profiles.join(format!("{LOOPBACK_PROFILE}.toml")),
    )
    .expect("the shipped profile copies out of the installed share");

    // The lock: the key the environment names, pinned to the installed manifest's entry. A
    // user lock may only select what the release ships, and this is exactly what it ships.
    fs::write(
        layout.config.join("modules.lock"),
        format!(
            "format = \"p1-modules-lock/1\"\n\n[modules.{READ_KEY}]\npackage = \"{package}\"\n\
             version = \"{version}\"\ndigest = \"{digest}\"\nworld = \"{world}\"\n\
             protocol = \"{protocol}\"\n"
        ),
    )
    .expect("modules.lock");

    let marker = format!("s7-7-2-marker-{short}");
    fs::write(layout.cwd.join(MARKER_FILE), format!("{marker}\n")).expect("the marker file");

    let real_home = std::env::var_os("HOME").map(PathBuf::from);
    if !bwrap_usable() {
        eprintln!("SKIP: bwrap unusable here");
        // What needs no sandbox: the installed bytes are the package the lock selects, and
        // the runtime accepts them.
        load_installed_read(&modules_root);
        return;
    }

    // The offline provider: a listener in THIS test process on 127.0.0.1 only, which is the
    // test's own socket and not live network. The route names its port, so it binds first.
    let served = Arc::new(AtomicUsize::new(0));
    let (port, server) = loopback(
        vec![
            sse_tool_call(MARKER_FILE),
            sse_final_text("the marker is reported"),
        ],
        served.clone(),
    )
    .await;
    let routes = layout.config.join("routes");
    fs::create_dir_all(&routes).expect("routes directory");
    fs::write(
        routes.join(format!("{LOOPBACK_ROUTE}.toml")),
        format!(
            "id           = \"{LOOPBACK_ROUTE}\"\norigin_route = \"openai-chat/{LOOPBACK_ROUTE}\"\n\
             adapter      = \"openai-chat\"\n\
             endpoint     = \"http://127.0.0.1:{port}/v1/chat/completions\"\n\n\
             [credential]\nkind = \"none\"\n\n\
             [adapter_settings]\ndialect = \"thinking-with-reasoning-alias\"\n\n\
             [models.\"{LOOPBACK_PROFILE}\"]\nwire_model = \"{LOOPBACK_MODEL}\"\n"
        ),
    )
    .expect("the route override");

    let bin = layout.prefix.join("bin/p1");
    let session = layout.scratch.join("module-run.jsonl");
    let session_arg = session.to_string_lossy().into_owned();
    let env = run_env(&layout);
    let done = sandboxed(
        &layout,
        &root,
        real_home.as_deref(),
        &bin,
        &["--env", MODULE_ENV, "--session", &session_arg, PROMPT],
        &env,
        true,
    );

    assert!(
        done.status.success(),
        "the installed module run exited {}: {}",
        done.status,
        String::from_utf8_lossy(&done.stderr)
    );
    assert_eq!(
        served.load(Ordering::SeqCst),
        2,
        "the scripted provider answers one tool call and one final text"
    );

    // The cover is real, not a promise: nothing under the checkout is reachable from inside
    // the sandbox the run just used.
    the_checkout_is_covered(&layout, &root, real_home.as_deref(), true);

    let records = journal_records(&session);
    let started = records
        .iter()
        .find(|record| record["record"] == "tool_started")
        .unwrap_or_else(|| panic!("the journal records no tool call"));
    assert_eq!(
        started["identity"]["implementation"], READ_PACKAGE,
        "the call must be served by {READ_PACKAGE}, not the native read"
    );
    let finished = records
        .iter()
        .find(|record| record["record"] == "tool_finished")
        .unwrap_or_else(|| panic!("the journal records no tool result"));
    assert_eq!(finished["result"]["status"], "ok", "{finished}");
    let content = finished["result"]["content"]
        .as_str()
        .expect("the tool result carries text");
    assert!(
        content.contains(&marker),
        "the component's answer must carry the marker {marker}: {content}"
    );

    // ADR-0080 (S1.9): the journal's assembly identity names the package the installed
    // manifest pins, with the digest the loader verified.
    let assembly = records
        .iter()
        .find_map(|record| record.get("assembly"))
        .unwrap_or_else(|| panic!("the journal names no assembly"));
    let module = assembly["modules"]
        .as_array()
        .expect("modules")
        .iter()
        .find(|module| module["package"] == READ_KEY)
        .unwrap_or_else(|| panic!("the assembly names no `{READ_KEY}` module"));
    assert_eq!(module["name"], READ_PACKAGE, "{module}");
    assert_eq!(module["kind"], "tool", "{module}");
    assert_eq!(
        module["digest"],
        digest.strip_prefix("sha256:").expect("a sha256 digest"),
        "{module}"
    );
    assert_eq!(module["abi"], format!("{world}+{protocol}"), "{module}");

    // The negative case: a second install whose shipped package file lost one byte. The
    // loader verifies the bytes against the manifest the lock pins, so the run refuses
    // assembly there and never reaches the provider.
    let second = layout.scratch.join("second-prefix");
    install_release_into(&layout, &tag, &second);
    let component = second.join("share/p1/modules").join(field("path"));
    let mut bytes = fs::read(&component).expect("the second install's package file");
    let last = bytes.len() - 1;
    bytes[last] ^= 0xFF;
    fs::write(&component, &bytes).expect("the corrupted package file");

    let served_before = served.load(Ordering::SeqCst);
    let broken_session = layout.scratch.join("broken-run.jsonl");
    let broken_arg = broken_session.to_string_lossy().into_owned();
    let refused = sandboxed(
        &layout,
        &root,
        real_home.as_deref(),
        &second.join("bin/p1"),
        &["--env", MODULE_ENV, "--session", &broken_arg, PROMPT],
        &env,
        true,
    );
    assert!(
        !refused.status.success(),
        "the corrupted install must refuse the run: {}",
        String::from_utf8_lossy(&refused.stdout)
    );
    let stderr = String::from_utf8_lossy(&refused.stderr).into_owned();
    for part in [READ_PACKAGE, "failed verification", "digest"] {
        assert!(
            stderr.contains(part),
            "the refusal must name {part}: {stderr}"
        );
    }
    assert_eq!(
        served.load(Ordering::SeqCst),
        served_before,
        "a refused assembly serves no tool call"
    );
    if broken_session.is_file() {
        assert!(
            journal_records(&broken_session).iter().all(|record| {
                record["record"] != "tool_started" && record["record"] != "tool_finished"
            }),
            "a refused assembly records no tool call"
        );
    }

    server.abort();
}

/// The temporary trees one staged release, its install and its sandboxed runs use.
struct Layout {
    /// The repository root the scripts are read from.
    root: PathBuf,
    /// The scratch root the sandbox keeps visible and writable.
    scratch: PathBuf,
    /// The `file://` release base the installer downloads from.
    base: PathBuf,
    /// The temporary HOME the installed run gets.
    home: PathBuf,
    /// The temporary config directory (`P1_CONFIG_DIR`) the installed run gets.
    config: PathBuf,
    /// The PATH of symlinks `install.sh` runs, with no `gh` in it.
    farm: PathBuf,
    /// The temporary cwd the installed run starts in.
    cwd: PathBuf,
    /// Where the install goes.
    prefix: PathBuf,
}

impl Layout {
    fn new(root: &Path, scratch: &Path) -> Self {
        Self {
            root: root.to_path_buf(),
            scratch: scratch.to_path_buf(),
            base: mkdir(scratch, "release"),
            home: mkdir(scratch, "home"),
            config: mkdir(scratch, "config"),
            farm: tool_farm(scratch),
            cwd: mkdir(scratch, "cwd"),
            prefix: scratch.join("prefix"),
        }
    }
}

// --- staging, installing and running -----------------------------------------

/// The repository root of this worktree.
fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("the repository root")
}

/// The `target/<profile>` directory the test binary lives in.
fn profile_dir() -> PathBuf {
    let exe = std::env::current_exe().expect("the test binary's path");
    exe.parent()
        .and_then(Path::parent)
        .expect("target/<profile>")
        .to_path_buf()
}

/// The p1 binary to ship: `$P1_BIN`, else this profile's freshly built `p1`, else one built
/// now. A nested cargo call runs under the build-directory lock the outer `cargo test` holds,
/// so a lock wait is recognised at once and the already-built binary is taken instead; when
/// there is no usable binary either, the case fails naming the command to run.
fn p1_binary(root: &Path) -> PathBuf {
    if let Some(bin) = std::env::var_os("P1_BIN")
        && !bin.is_empty()
    {
        let path = PathBuf::from(bin);
        assert!(
            path.is_file(),
            "P1_BIN is set but {} is not a file",
            path.display()
        );
        return path;
    }
    // The cases run in parallel; a second nested build would see the first one's lock and
    // give up, so one case builds while the other waits and then takes the fresh binary.
    static BUILD: Mutex<()> = Mutex::new(());
    let _build = BUILD.lock().unwrap_or_else(PoisonError::into_inner);
    let candidate = profile_dir().join("p1");
    if fresh(&candidate, root) && binary_names_checkout(&candidate, root) {
        return candidate;
    }
    // `CARGO` is the toolchain cargo set for this test process, so the nested build uses the
    // same one that is running the tests rather than whatever PATH happens to hold.
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| OsString::from("cargo"));
    if let Err(reason) = run_locked(root, &cargo, &build_p1_args()) {
        assert!(
            fresh(&candidate, root) && binary_names_checkout(&candidate, root),
            "no usable p1 binary at {}: {reason}; run `cargo build --locked -p p1-host --bin p1`",
            candidate.display()
        );
    }
    assert!(
        candidate.is_file(),
        "the p1 build produced no {}",
        candidate.display()
    );
    candidate
}

/// The cargo line build.rs and the release workflow use for the p1 binary.
fn build_p1_args() -> [&'static str; 6] {
    ["build", "--locked", "-p", "p1-host", "--bin", "p1"]
}

/// Reject a binary older than any input the `p1` binary is built from: the workspace manifests
/// and `p1-host`'s dependency closure (`p1_binary_inputs`), not every workspace crate and not
/// the runtime config trees. A test-only or unrelated crate is not linked into the binary, and
/// neither are `routes/` and `profiles/` (staged separately at run time), so editing either
/// must not reject a binary that is newer than every real input.
fn fresh(candidate: &Path, root: &Path) -> bool {
    let Ok(built) = fs::metadata(candidate).and_then(|meta| meta.modified()) else {
        return false;
    };
    p1_binary_inputs(root)
        .iter()
        .filter_map(|path| newest_mtime(path, Some("target")))
        .all(|source| source <= built)
}

fn set_mtime(path: &Path, at: SystemTime) {
    fs::File::options()
        .write(true)
        .open(path)
        .unwrap()
        .set_times(fs::FileTimes::new().set_modified(at))
        .unwrap();
}

#[test]
fn binary_freshness_covers_dependency_crates() {
    let dir = tempfile::tempdir().unwrap();
    let candidate = dir.path().join("p1");
    fs::write(&candidate, b"old binary").unwrap();
    fs::File::options()
        .write(true)
        .open(&candidate)
        .unwrap()
        .set_times(fs::FileTimes::new().set_modified(SystemTime::UNIX_EPOCH))
        .unwrap();
    let dependency = dir.path().join("crates/p1-contracts/src/lib.rs");
    fs::create_dir_all(dependency.parent().unwrap()).unwrap();
    fs::write(&dependency, b"new dependency").unwrap();
    assert!(!fresh(&candidate, dir.path()));
}

#[test]
fn binary_freshness_ignores_crates_outside_the_binarys_dependency_closure() {
    let dir = tempfile::tempdir().unwrap();
    let candidate = dir.path().join("p1");
    fs::write(&candidate, b"binary").unwrap();
    let host_manifest = dir.path().join("crates/p1-host/Cargo.toml");
    fs::create_dir_all(host_manifest.parent().unwrap()).unwrap();
    fs::write(
        &host_manifest,
        "[package]\nname = \"p1-host\"\n\n[dependencies]\n\
         p1-contracts = { path = \"../p1-contracts\" }\n\n[dev-dependencies]\n\
         p1-module-tests = { path = \"../p1-module-tests\" }\n",
    )
    .unwrap();
    let dependency = dir.path().join("crates/p1-contracts/src/lib.rs");
    fs::create_dir_all(dependency.parent().unwrap()).unwrap();
    fs::write(&dependency, b"dependency").unwrap();
    let test_only = dir.path().join("crates/p1-module-tests/src/lib.rs");
    fs::create_dir_all(test_only.parent().unwrap()).unwrap();
    fs::write(&test_only, b"test only").unwrap();

    let built = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
    set_mtime(&candidate, built);
    set_mtime(&host_manifest, built - Duration::from_secs(100));
    set_mtime(&dependency, built - Duration::from_secs(100));
    set_mtime(&test_only, built + Duration::from_secs(100));

    assert!(
        fresh(&candidate, dir.path()),
        "a test-only crate is not an input to the binary"
    );
    set_mtime(&dependency, built + Duration::from_secs(200));
    assert!(
        !fresh(&candidate, dir.path()),
        "a crate the binary links is an input"
    );
}

#[test]
fn binary_freshness_ignores_runtime_config() {
    let dir = tempfile::tempdir().unwrap();
    let candidate = dir.path().join("p1");
    fs::write(&candidate, b"binary").unwrap();
    let built = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
    set_mtime(&candidate, built);
    for relative in ["routes/loopback.toml", "profiles/loopback.toml"] {
        let path = dir.path().join(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, b"runtime config").unwrap();
        set_mtime(&path, built + Duration::from_secs(200));
    }
    assert!(
        fresh(&candidate, dir.path()),
        "routes and profiles are staged at run time, not compiled into the binary"
    );
}

/// The newest modification time below `path`, ignoring a directory named `skip` (the build
/// outputs live inside `modules/target`, which is not a source).
fn newest_mtime(path: &Path, skip: Option<&str>) -> Option<SystemTime> {
    let meta = fs::metadata(path).ok()?;
    if meta.is_file() {
        return meta.modified().ok();
    }
    let mut newest = None;
    let mut pending = vec![path.to_path_buf()];
    while let Some(dir) = pending.pop() {
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            if skip == Some(entry.file_name().to_string_lossy().as_ref()) {
                continue;
            }
            let Ok(meta) = entry.metadata() else {
                continue;
            };
            if meta.is_dir() {
                pending.push(entry.path());
            } else if let Ok(modified) = meta.modified()
                && newest.is_none_or(|newest| modified > newest)
            {
                newest = Some(modified);
            }
        }
    }
    newest
}

/// Runs one build that may take cargo's build-directory lock, ending it as soon as cargo says
/// it is waiting for that lock: the outer `cargo test` holds it while this test runs, so a
/// nested build could never finish and the caller falls back to what is already built.
fn run_locked(root: &Path, program: impl AsRef<OsStr>, args: &[&str]) -> Result<(), String> {
    let program = program.as_ref();
    let name = program.to_string_lossy();
    let mut child = Command::new(program)
        .args(args)
        .current_dir(root)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| format!("cannot run {name}: {error}"))?;
    let stderr = child.stderr.take().expect("stderr pipe");
    let mut tail: Vec<String> = Vec::new();
    let mut locked = false;
    for line in BufReader::new(stderr).lines() {
        let line = line.unwrap_or_default();
        if line.contains("Blocking waiting for file lock") {
            locked = true;
            break;
        }
        tail.push(line);
        if tail.len() > 20 {
            tail.remove(0);
        }
    }
    if locked {
        let _ = child.kill();
        let _ = child.wait();
        return Err("the build directory lock is held by the running cargo test".to_owned());
    }
    let status = child
        .wait()
        .map_err(|error| format!("cannot wait for {name}: {error}"))?;
    if !status.success() {
        return Err(format!(
            "{name} {} exited {status}: {}",
            args.join(" "),
            tail.join(" | ")
        ));
    }
    Ok(())
}

/// The built module package outputs, building them when they are not current. The outputs are
/// published into the checkout (never into the cargo target directory), so a missing or stale
/// set is rebuilt once and a nested build that cannot run fails the case by name.
fn module_outputs(root: &Path) -> PathBuf {
    let published = root.join("modules/target/p1-modules");
    if module_outputs_current(root, &published) {
        return published;
    }
    if let Err(reason) = run_locked(root, "bash", &["scripts/build-modules.sh", "--all"]) {
        assert!(
            module_outputs_current(root, &published),
            "the module packages under {} are missing or stale: {reason}; run scripts/build-modules.sh --all",
            published.display()
        );
    }
    published
}

/// Whether every published package is there and newer than the newest module source. Only
/// directories are packages: the build also publishes `manifest.json` (the release manifest
/// the loader reads, S3.8.0) beside them, and that file is not one.
fn module_outputs_current(root: &Path, published: &Path) -> bool {
    let newest_source = newest_mtime(&root.join("modules"), Some("target"));
    let Ok(entries) = fs::read_dir(published) else {
        return false;
    };
    let mut packages = 0;
    for entry in entries.flatten() {
        if !entry.path().is_dir() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        for suffix in [".wasm", ".sha256", ".manifest.json"] {
            let Ok(meta) = entry.path().join(format!("{name}{suffix}")).metadata() else {
                return false;
            };
            if !meta.is_file() {
                return false;
            }
            if let (Some(source), Ok(built)) = (newest_source, meta.modified())
                && built < source
            {
                return false;
            }
        }
        packages += 1;
    }
    packages > 0
}

/// `scripts/stage-release.sh` into `out`, from the p1 binary and the module outputs.
fn stage_release(root: &Path, p1: &Path, out: &Path, commit: &str, tag: &str, modules: &Path) {
    let done = Command::new(bash())
        .arg(root.join("scripts/stage-release.sh"))
        .arg("--native")
        .arg(p1)
        .arg("--out")
        .arg(out)
        .arg("--commit")
        .arg(commit)
        .arg("--tag")
        .arg(tag)
        .arg("--modules")
        .arg(modules)
        .current_dir(root)
        .stdin(Stdio::null())
        .output()
        .expect("run scripts/stage-release.sh");
    assert!(
        done.status.success(),
        "scripts/stage-release.sh exited {}: {}{}",
        done.status,
        String::from_utf8_lossy(&done.stdout),
        String::from_utf8_lossy(&done.stderr)
    );
}

/// `scripts/install.sh --from-release tag` into the layout's prefix, from a `file://` release
/// base, with `HOME`, the config dir and the prefix in temporary directories and a PATH that
/// has no `gh`.
fn install_release(layout: &Layout, tag: &str) {
    install_release_into(layout, tag, &layout.prefix);
}

/// As [`install_release`], into `prefix`: the module case installs the same release twice, so
/// its negative half runs a prefix of its own with one package file corrupted.
fn install_release_into(layout: &Layout, tag: &str, prefix: &Path) {
    let done = Command::new(bash())
        .arg(layout.root.join("scripts/install.sh"))
        .arg("--from-release")
        .arg(tag)
        .arg("--prefix")
        .arg(prefix)
        .current_dir(&layout.root)
        .env_clear()
        .env("PATH", &layout.farm)
        .env("HOME", &layout.home)
        .env("XDG_CONFIG_HOME", &layout.config)
        .env("P1_CONFIG_DIR", &layout.config)
        .env(
            "P1_RELEASE_BASE_URL",
            format!("file://{}", layout.base.display()),
        )
        .env("TMPDIR", &layout.scratch)
        .env("LC_ALL", "C")
        .stdin(Stdio::null())
        .output()
        .expect("run scripts/install.sh");
    assert!(
        done.status.success(),
        "scripts/install.sh exited {}: {}{}",
        done.status,
        String::from_utf8_lossy(&done.stdout),
        String::from_utf8_lossy(&done.stderr)
    );
}

/// Stage the built release, publish its four assets under the layout's `file://` base and
/// install them into `layout.prefix`: the three steps the first case runs inline, reused by
/// the module case.
fn stage_and_install(
    root: &Path,
    layout: &Layout,
    p1: &Path,
    commit: &str,
    tag: &str,
    modules: &Path,
) {
    let out = layout.scratch.join("dist");
    stage_release(root, p1, &out, commit, tag, modules);
    for asset in ASSETS {
        assert!(
            out.join(asset).is_file(),
            "{} is missing",
            out.join(asset).display()
        );
    }
    let published = layout.base.join("download").join(tag);
    fs::create_dir_all(&published).expect("release base");
    for asset in ASSETS {
        fs::copy(out.join(asset), published.join(asset)).expect("publish the asset");
    }
    install_release(layout, tag);
}

/// A directory of symlinks to the tools `install.sh` runs, and no `gh`.
fn tool_farm(scratch: &Path) -> PathBuf {
    let farm = mkdir(scratch, "farm");
    for tool in FARM_TOOLS {
        let link = farm.join(tool);
        if link.exists() {
            continue;
        }
        let found = find_tool(tool).unwrap_or_else(|| panic!("{tool} is not on PATH"));
        std::os::unix::fs::symlink(&found, &link).expect("tool symlink");
    }
    farm
}

/// The first `name` on PATH that is a file.
fn find_tool(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(name))
        .find(|candidate| candidate.is_file())
}

/// Bash by absolute path: the install runs with a PATH of its own, which carries no shell.
fn bash() -> PathBuf {
    find_tool("bash").unwrap_or_else(|| PathBuf::from("/bin/bash"))
}

/// The environment the installed run gets: the layout's temporary HOME, config dir and PATH,
/// and no `P1_ENVIRONMENTS_DIR`, so the installed share is what the host finds.
fn run_env(layout: &Layout) -> Vec<(OsString, OsString)> {
    vec![
        (OsString::from("HOME"), layout.home.as_os_str().to_owned()),
        (
            OsString::from("P1_CONFIG_DIR"),
            layout.config.as_os_str().to_owned(),
        ),
        (OsString::from("PATH"), layout.farm.as_os_str().to_owned()),
        (
            OsString::from("TMPDIR"),
            layout.scratch.as_os_str().to_owned(),
        ),
        (OsString::from("LC_ALL"), OsString::from("C")),
    ]
}

/// Whether a throwaway `bwrap` can run here at all, as `crates/p1-host/tests/sandbox.rs` asks.
fn bwrap_usable() -> bool {
    Command::new("bwrap")
        .args([
            "--ro-bind",
            "/",
            "/",
            "--dev",
            "/dev",
            "--proc",
            "/proc",
            "true",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

/// Runs `exe args` under the sandbox cover with the repository root and the real HOME
/// replaced by tmpfs and a temporary cwd, so only the installed share is reachable; `network`
/// says whether the run keeps the host network (the module case's loopback provider).
fn sandboxed(
    layout: &Layout,
    root: &Path,
    real_home: Option<&Path>,
    exe: &Path,
    args: &[&str],
    env: &[(OsString, OsString)],
    network: bool,
) -> Output {
    let mut command = Command::new("bwrap");
    command.args(sandbox_cover(layout, root, real_home, network));
    for (key, value) in env {
        command.arg("--setenv").arg(key).arg(value);
    }
    command.arg("--unsetenv").arg("P1_ENVIRONMENTS_DIR");
    command.arg("--").arg(exe).args(args);
    command
        .stdin(Stdio::null())
        .output()
        .expect("run the installed p1 under bwrap")
}

/// The cover a sandboxed run gets: the repository root and the real HOME replaced by tmpfs
/// and a temporary cwd, so only the installed share and the scratch tree are reachable. The
/// temporary trees stay visible and writable; everything else is the read-only root.
///
/// `network` keeps the host network for the module case alone, whose provider is a loopback
/// listener in this test process; every other run shares no network at all.
fn sandbox_cover(
    layout: &Layout,
    root: &Path,
    real_home: Option<&Path>,
    network: bool,
) -> Vec<OsString> {
    let mut args: Vec<OsString> = Vec::new();
    if !network {
        args.push(OsString::from("--unshare-net"));
    }
    for arg in ["--ro-bind", "/", "/", "--dev", "/dev", "--proc", "/proc"] {
        args.push(OsString::from(arg));
    }
    // An ancestor's tmpfs hides every mount below it, so the parents come first: a worktree
    // usually sits below the real HOME, and both must end up EMPTY rather than merely
    // unreachable, which is what lets the cover be probed.
    let mut hidden: Vec<&Path> = [Some(root), real_home].into_iter().flatten().collect();
    hidden.sort_by_key(|path| path.components().count());
    hidden.dedup();
    for hidden in hidden {
        if hidden != Path::new("/") {
            args.push(OsString::from("--tmpfs"));
            args.push(hidden.as_os_str().to_owned());
        }
    }
    args.push(OsString::from("--bind"));
    args.push(layout.scratch.as_os_str().to_owned());
    args.push(layout.scratch.as_os_str().to_owned());
    args.push(OsString::from("--chdir"));
    args.push(layout.cwd.as_os_str().to_owned());
    args
}

/// The checkout under the sandbox cover: `ls -A` over the covered root prints nothing, which
/// is what makes "no path under the checkout was read" a checked fact rather than a promise.
fn the_checkout_is_covered(layout: &Layout, root: &Path, real_home: Option<&Path>, network: bool) {
    let out = Command::new("bwrap")
        .args(sandbox_cover(layout, root, real_home, network))
        .arg("--unsetenv")
        .arg("P1_ENVIRONMENTS_DIR")
        .arg("--")
        .arg(find_tool("ls").expect("ls is on PATH"))
        .arg("-A")
        .arg(root)
        .stdin(Stdio::null())
        .output()
        .expect("run the cover probe under bwrap");
    assert!(
        out.status.success(),
        "bwrap ls -A {} exited {}: {}",
        root.display(),
        out.status,
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        out.stdout.is_empty(),
        "the sandbox cover must leave {} empty: {}",
        root.display(),
        String::from_utf8_lossy(&out.stdout)
    );
}

// --- the offline provider -----------------------------------------------------

/// The offline provider of the module case: a loopback listener in THIS test process,
/// answering the `n`th request with `answers[n]` and closing each connection after its answer.
/// `served` counts every connection the listener accepted, so "the run reached it" and "the
/// run never reached it" are both facts. It binds 127.0.0.1 only: this is the test's own
/// socket, never live network (`crates/p1-module-tests/tests/transport_authority.rs` binds
/// the same way).
async fn loopback(
    answers: Vec<String>,
    served: Arc<AtomicUsize>,
) -> (u16, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind the loopback provider");
    let port = listener
        .local_addr()
        .expect("the listener's address")
        .port();
    let server = tokio::spawn(async move {
        let mut answers = answers.into_iter();
        while let Ok((mut socket, _)) = listener.accept().await {
            served.fetch_add(1, Ordering::SeqCst);
            match answers.next() {
                Some(answer) => answer_request(&mut socket, &answer).await,
                // More requests than answers is a case failure, not a hang: close without
                // answering and let the run's own error surface.
                None => {
                    let _ = socket.shutdown().await;
                }
            }
        }
    });
    (port, server)
}

/// Reads one HTTP request and answers it with `body` as an SSE 200, then closes.
async fn answer_request(socket: &mut TcpStream, body: &str) {
    let _ = read_request(socket).await;
    let answer = format!(
        "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\n\
         connection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = socket.write_all(answer.as_bytes()).await;
    let _ = socket.shutdown().await;
}

/// Reads one request head and its declared body, so the peer never sees its request cut short
/// before the answer.
async fn read_request(socket: &mut TcpStream) -> std::io::Result<()> {
    let mut request = Vec::new();
    let mut buffer = [0_u8; 4096];
    let head_end = loop {
        let read = socket.read(&mut buffer).await?;
        if read == 0 {
            return Ok(());
        }
        request.extend_from_slice(&buffer[..read]);
        if let Some(at) = request.windows(4).position(|window| window == b"\r\n\r\n") {
            break at + 4;
        }
    };
    let want = content_length(&request[..head_end]).unwrap_or(0);
    let mut have = request.len() - head_end;
    while have < want {
        let read = socket.read(&mut buffer).await?;
        if read == 0 {
            break;
        }
        have += read;
    }
    Ok(())
}

/// The `Content-Length` of an HTTP/1.1 message head, or `None`.
fn content_length(head: &[u8]) -> Option<usize> {
    String::from_utf8_lossy(head)
        .lines()
        .filter_map(|line| line.split_once(':'))
        .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
        .and_then(|(_, value)| value.trim().parse().ok())
}

/// One `openai-chat` chunk in the shape the shipped adapter's parser accepts.
fn chat_chunk(delta: Value, finish: Value) -> Value {
    json!({
        "id": "chatcmpl-s7-read",
        "model": LOOPBACK_MODEL,
        "choices": [{ "index": 0, "delta": delta, "finish_reason": finish }],
    })
}

/// One SSE frame carrying `data`.
fn sse(value: &Value) -> String {
    format!("data: {value}\n\n")
}

/// The `openai-chat` answer whose one tool call reads `file`: the delta that declares the call
/// and the `tool_calls` finish reason the adapter requires for one.
fn sse_tool_call(file: &str) -> String {
    let call = json!({
        "index": 0,
        "id": "call_s7",
        "type": "function",
        "function": {
            "name": READ_KEY,
            "arguments": json!({ "file_path": file }).to_string(),
        },
    });
    let mut body = sse(&chat_chunk(
        json!({ "role": "assistant", "content": "" }),
        Value::Null,
    ));
    body.push_str(&sse(&chat_chunk(
        json!({ "tool_calls": [call] }),
        Value::Null,
    )));
    body.push_str(&sse(&chat_chunk(json!({}), json!("tool_calls"))));
    body.push_str("data: [DONE]\n\n");
    body
}

/// The `openai-chat` answer of a final text turn.
fn sse_final_text(text: &str) -> String {
    let mut body = sse(&chat_chunk(json!({ "content": text }), Value::Null));
    body.push_str(&sse(&chat_chunk(json!({}), json!("stop"))));
    body.push_str("data: [DONE]\n\n");
    body
}

// --- the installed bytes ------------------------------------------------------

/// Every file under `share/p1/modules/packages/` matches the manifest's digest and size, and
/// the manifest lists every one of them.
fn verify_installed_components(modules_root: &Path, manifest: &Value) {
    let mut listed: BTreeMap<String, (String, u64)> = BTreeMap::new();
    for entry in manifest["packages"].as_array().expect("packages") {
        let path = entry["path"].as_str().expect("packages[].path").to_owned();
        let sha = entry["sha256"]
            .as_str()
            .expect("packages[].sha256")
            .to_owned();
        let size = entry["size"].as_u64().expect("packages[].size");
        assert!(
            listed.insert(path.clone(), (sha, size)).is_none(),
            "{path} twice"
        );
    }

    let mut found: Vec<String> = Vec::new();
    walk(&modules_root.join("packages"), "packages", &mut found);
    found.sort();
    let want: Vec<String> = listed.keys().cloned().collect();
    assert_eq!(
        found, want,
        "the installed files are not the manifest's packages"
    );

    for (path, (sha, size)) in &listed {
        let bytes = fs::read(modules_root.join(path)).expect("installed package file");
        assert_eq!(bytes.len() as u64, *size, "{path}: size");
        assert_eq!(digest_of(&bytes), format!("sha256:{sha}"), "{path}: sha256");
    }

    for entry in manifest["components"].as_array().expect("components") {
        let path = entry["path"].as_str().expect("components[].path");
        assert!(
            listed.contains_key(path),
            "components names an unshipped {path}"
        );
        let digest = entry["digest"].as_str().expect("components[].digest");
        assert_eq!(
            digest,
            format!("sha256:{}", listed[path].0),
            "{path}: digest"
        );
    }
}

/// Loads every installed component through the release manifest and loader and executes the
/// shipped fixture once, exactly as the harness's `Release::loader` does. A component that
/// grants an interface the runtime does not link yet is refused from its manifest entry alone
/// (`docs/design/modules/package.md`, "The loader"; ADR-0085 item 3): that refusal, naming the
/// component's own grant, is the expected outcome for such a component, while every other
/// component must load and every other refusal still ends the case.
async fn load_every_installed_component(modules_root: &Path) {
    let manifest_path = modules_root.join("manifest.json");
    let manifest = ReleaseManifest::read(&manifest_path).expect("read the installed manifest");
    // Every interface an installed component imports is linked: the provider interfaces
    // (http, websocket, credential-control) by S4.7, the worker and workflow interfaces
    // (workers-start, workers-observe, workers-control, workflows) by S6.7 (D057), `workspace`
    // and `snapshot` by S1, and `workspace-mutation` (edit, write, patch) by S2 (U-mut.2). So
    // every installed component must load; the list stays for the next family to arrive.
    const UNLINKED: [&str; 0] = [];

    let entries: Vec<(String, Vec<String>)> = manifest
        .components()
        .iter()
        .map(|entry| (entry.name.clone(), entry.capabilities.clone()))
        .collect();
    assert!(
        !entries.is_empty(),
        "the installed release ships no component"
    );

    let loader = Loader::new(manifest, modules_root).expect("a loader over the installed set");
    for (name, capabilities) in &entries {
        match loader.load(name) {
            Ok(_) => assert!(
                !capabilities
                    .iter()
                    .any(|capability| UNLINKED.contains(&capability.as_str())),
                "{name}: a component of a family this runtime does not link was loaded"
            ),
            Err(LoadError::UnsupportedCapability {
                name: named,
                capability,
            }) => {
                assert_eq!(&named, name, "{name}: the refusal names another component");
                assert!(
                    capabilities.contains(&capability),
                    "{name}: the refusal names {capability}, which its manifest does not grant"
                );
                assert!(
                    UNLINKED.contains(&capability.as_str()),
                    "{name}: the refusal names {capability}"
                );
            }
            Err(error) => panic!("load {name}: {error}"),
        }
    }

    let (process, _processes) = fake_processes();
    let module = loader
        .load(FIXTURE_NAME)
        .expect("load the shipped fixture component");
    let tool = wasm_tool(
        &module,
        Services {
            process: Some(process),
            ..Services::default()
        },
        ExecutionLimits::default(),
        &Arc::new(MaskCounter::new()),
    )
    .expect("the shipped fixture is a tool");
    let outcome = tool
        .execute(
            &call("echo:installed"),
            ToolContext {
                cancel: CancellationToken::new(),
            },
        )
        .await;
    assert_eq!(outcome.status, ToolStatus::Ok, "{}", outcome.content);
    assert_eq!(outcome.content, "installed");
}

// --- small helpers ------------------------------------------------------------

/// The version line's commit token, `deadbeef0000` in `p1 0.0.1 (deadbeef0000 2026-09-24)`.
fn version_short_sha(p1: &Path) -> String {
    let out = Command::new(p1)
        .arg("--version")
        .stdin(Stdio::null())
        .output()
        .unwrap_or_else(|error| panic!("cannot run {} --version: {error}", p1.display()));
    assert!(
        out.status.success(),
        "{} --version exited {}",
        p1.display(),
        out.status
    );
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    for token in text.split_whitespace() {
        if let Some(rest) = token.strip_prefix('(') {
            let sha: String = rest.chars().take_while(char::is_ascii_hexdigit).collect();
            if !sha.is_empty() {
                return sha;
            }
        }
    }
    panic!("{} --version names no commit: {text}", p1.display());
}

/// The release version the installed binary names, `0.0.1` in `p1 0.0.1 (deadbeef0000
/// 2026-09-24)`. A release manifest carries no version of its own, so this is where the lock
/// entry's recorded version comes from — the installed artefact, never the checkout.
fn binary_version(p1: &Path) -> String {
    let out = Command::new(p1)
        .arg("--version")
        .stdin(Stdio::null())
        .output()
        .unwrap_or_else(|error| panic!("cannot run {} --version: {error}", p1.display()));
    assert!(
        out.status.success(),
        "{} --version exited {}",
        p1.display(),
        out.status
    );
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    text.split_whitespace()
        .nth(1)
        .map(str::to_owned)
        .unwrap_or_else(|| panic!("{} --version names no version: {text}", p1.display()))
}

/// The installed set through the runtime's loader: the bytes the lock pins are the bytes the
/// runtime accepts. The assertion the module case keeps when `bwrap` is unusable.
fn load_installed_read(modules_root: &Path) {
    let manifest =
        ReleaseManifest::read(&modules_root.join("manifest.json")).expect("the installed manifest");
    let loader = Loader::new(manifest, modules_root).expect("a loader over the installed set");
    loader
        .load(READ_PACKAGE)
        .unwrap_or_else(|error| panic!("the installed {READ_PACKAGE} must load: {error}"));
}

/// Every line of the session journal at `path`, parsed. The first line is the format header
/// and the assembly identity lines carry no `record`.
fn journal_records(path: &Path) -> Vec<Value> {
    let text = fs::read_to_string(path)
        .unwrap_or_else(|error| panic!("read the run journal {}: {error}", path.display()));
    text.lines()
        .map(|line| {
            serde_json::from_str(line)
                .unwrap_or_else(|error| panic!("a journal line is not JSON: {error}"))
        })
        .collect()
}

/// The full 40-hex commit the p1 binary names, resolved in this checkout.
fn resolve_commit(root: &Path, short: &str) -> String {
    if let Ok(out) = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["rev-parse", "--verify", short])
        .stdin(Stdio::null())
        .output()
        && out.status.success()
    {
        let full = String::from_utf8_lossy(&out.stdout).trim().to_owned();
        if full.len() == 40 && full.starts_with(short) {
            return full;
        }
    }
    // Build boxes can export the source tree without its Git object database. The module
    // generator writes the same checkout's full commit to the built development manifest;
    // accept that provenance only when it matches the binary's version prefix.
    if let Some(full) = built_manifest_commit(root, short) {
        return full;
    }
    panic!(
        "the p1 binary names commit {short}, which this checkout cannot resolve; build it here \
         with `cargo build --locked -p p1-host --bin p1`"
    );
}

fn built_manifest_commit(root: &Path, short: &str) -> Option<String> {
    let manifest = fs::read(root.join("modules/target/p1-modules/manifest.json")).ok()?;
    let manifest: Value = serde_json::from_slice(&manifest).ok()?;
    let full = manifest.get("commit")?.as_str()?;
    (full.len() == 40
        && full
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        && full.starts_with(short))
    .then(|| full.to_owned())
}

#[test]
fn an_exported_build_manifest_resolves_only_its_matching_binary_commit() {
    let root = tempfile::tempdir().unwrap();
    let modules = root.path().join("modules/target/p1-modules");
    fs::create_dir_all(&modules).unwrap();
    let full = "2c7a6fbd88bab6777b338de6ce6cad36f27fc430";
    fs::write(
        modules.join("manifest.json"),
        serde_json::json!({ "commit": full }).to_string(),
    )
    .unwrap();
    assert_eq!(
        built_manifest_commit(root.path(), "2c7a6fbd88ba"),
        Some(full.to_owned())
    );
    assert_eq!(built_manifest_commit(root.path(), "123456789012"), None);
}

/// `sha256:<hex>` of the bytes at `path`, through the runtime's own digest.
fn sha256_of(path: &Path) -> String {
    digest_of(&fs::read(path).unwrap_or_else(|error| panic!("read {}: {error}", path.display())))
}

/// `sha256:<hex>` of `bytes`, through the runtime's own digest.
fn digest_of(bytes: &[u8]) -> String {
    Digest::of(bytes).to_string()
}

/// Every regular file below `dir`, as POSIX-relative paths under `prefix`.
fn walk(dir: &Path, prefix: &str, found: &mut Vec<String>) {
    let entries =
        fs::read_dir(dir).unwrap_or_else(|error| panic!("read {}: {error}", dir.display()));
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let path = format!("{prefix}/{name}");
        let meta = entry.metadata().expect("package entry");
        if meta.is_dir() {
            walk(&entry.path(), &path, found);
        } else {
            found.push(path);
        }
    }
}

/// A temporary directory that the sandbox's tmpfs mounts cannot hide.
fn scratch_dir(root: &Path) -> TempDir {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let temp = std::env::temp_dir();
    let hidden = [Some(root.to_path_buf()), home].into_iter().flatten();
    let base = if hidden.into_iter().any(|hidden| temp.starts_with(&hidden)) {
        PathBuf::from("/tmp")
    } else {
        temp
    };
    tempfile::Builder::new()
        .prefix("p1-installed-release-")
        .tempdir_in(&base)
        .unwrap_or_else(|error| panic!("scratch directory under {}: {error}", base.display()))
}

fn mkdir(parent: &Path, name: &str) -> PathBuf {
    let path = parent.join(name);
    fs::create_dir_all(&path).unwrap_or_else(|error| panic!("create {}: {error}", path.display()));
    path
}
