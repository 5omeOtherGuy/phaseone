//! S7.7: a real staged release, installed and run offline from its binary plus the modules
//! it ships.
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
//! The host does not load modules from `<prefix>/share/p1/modules` yet
//! (`crates/p1-host/src/catalog/modules.rs`), so the headless run whose tool call is served
//! by a shipped component is slice S7.7.2 at `wasm-loader-v1` and is not here.
//!
//! The case needs a real `bwrap`; like `crates/p1-host/tests/sandbox.rs` it prints
//! `SKIP: bwrap unusable here` and continues with the assertions that need no sandbox when
//! the probe fails. It never skips silently: a missing binary, a missing module build or a
//! missing toolchain fails the case with the command to run.

use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::Arc;
use std::time::SystemTime;

use p1_contracts::serde_json::{self, Value};
use p1_contracts::{CancellationToken, ToolContext, ToolStatus};
use p1_module_runtime::{
    Digest, ExecutionLimits, LoadError, Loader, ReleaseManifest, Services, wasm_tool,
};
use p1_module_tests::{FIXTURE_NAME, call, fake_processes};
use p1_redact::MaskCounter;
use tempfile::TempDir;

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
    let candidate = profile_dir().join("p1");
    let sources = root.join("crates/p1-host");
    if fresh(&candidate, &sources) {
        return candidate;
    }
    // `CARGO` is the toolchain cargo set for this test process, so the nested build uses the
    // same one that is running the tests rather than whatever PATH happens to hold.
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| OsString::from("cargo"));
    if let Err(reason) = run_locked(root, &cargo, &build_p1_args()) {
        assert!(
            fresh(&candidate, &sources),
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

/// Whether `candidate` exists and is at least as new as every file under `sources`.
fn fresh(candidate: &Path, sources: &Path) -> bool {
    let Ok(built) = fs::metadata(candidate).and_then(|meta| meta.modified()) else {
        return false;
    };
    match newest_mtime(sources, None) {
        Some(source) => source <= built,
        None => true,
    }
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

/// Whether every published package is there and newer than the newest module source.
fn module_outputs_current(root: &Path, published: &Path) -> bool {
    let newest_source = newest_mtime(&root.join("modules"), Some("target"));
    let Ok(entries) = fs::read_dir(published) else {
        return false;
    };
    let mut packages = 0;
    for entry in entries.flatten() {
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
    let done = Command::new(bash())
        .arg(layout.root.join("scripts/install.sh"))
        .arg("--from-release")
        .arg(tag)
        .arg("--prefix")
        .arg(&layout.prefix)
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

/// Runs `exe args` in a network-less namespace with the repository root and the real HOME
/// replaced by tmpfs and a temporary cwd, so only the installed share is reachable.
fn sandboxed(
    layout: &Layout,
    root: &Path,
    real_home: Option<&Path>,
    exe: &Path,
    args: &[&str],
    env: &[(OsString, OsString)],
) -> Output {
    let mut command = Command::new("bwrap");
    command.args([
        "--unshare-net",
        "--ro-bind",
        "/",
        "/",
        "--dev",
        "/dev",
        "--proc",
        "/proc",
    ]);
    for hidden in [Some(root), real_home].into_iter().flatten() {
        if hidden != Path::new("/") {
            command.arg("--tmpfs").arg(hidden);
        }
    }
    // The temporary trees stay visible and writable; everything else is the read-only root.
    command
        .arg("--bind")
        .arg(&layout.scratch)
        .arg(&layout.scratch);
    for (key, value) in env {
        command.arg("--setenv").arg(key).arg(value);
    }
    command.arg("--unsetenv").arg("P1_ENVIRONMENTS_DIR");
    command.arg("--chdir").arg(&layout.cwd);
    command.arg("--").arg(exe).args(args);
    command
        .stdin(Stdio::null())
        .output()
        .expect("run the installed p1 under bwrap")
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
    // The interfaces the runtime does not link yet: each worker and workflow member imports the
    // one its own native service owns, and the runtime links an interface only with its service.
    const UNLINKED: [&str; 4] = [
        "workers-start",
        "workers-observe",
        "workers-control",
        "workflows",
    ];

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
    panic!(
        "the p1 binary names commit {short}, which this checkout cannot resolve; build it here \
         with `cargo build --locked -p p1-host --bin p1`"
    );
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
