//! S7.10.3 (issue #368): a release missing one of the packages the host composes itself must
//! fail startup naming that package, never answer from the native implementation (D083b
//! item 4).
//!
//! The official release ships one `manifest.json` beside its packages
//! (`scripts/release-manifest.py`), and a debug `p1` finds it as
//! `<exe dir>/../share/p1/modules/manifest.json` (`crates/p1-host/src/catalog/modules.rs`).
//! Each case copies the built module set into a scratch tree as `<scratch>/bin/p1` plus
//! `<scratch>/share/p1/`, removes exactly one host-entry package — its manifest entry and its
//! package directory — and runs that scratch binary headless, so the crafted release is the
//! only module set the run can find. The class's own environment and route are copied into the
//! same share tree, the route pointed at a loopback endpoint that refuses every connection and
//! given `[credential] kind = "none"` (the shape `crates/p1-host/tests/none_credential.rs`
//! proves), so no live network is possible and no credential is read. `HOME`,
//! `XDG_CONFIG_HOME`, `XDG_DATA_HOME` and `P1_CONFIG_DIR` all live inside the scratch tree.
//!
//! Every case's owner slice has landed on main (S1.8.1, S3.8, S4.9, S5.11, S6.11), so the
//! host composes each of these packages itself and a release missing one fails naming it
//! instead of answering natively — the silent native answer D083b item 4 forbids. A case whose
//! package the host does not compose yet is red until its owner slice lands; each case's
//! `owner` names that slice, and every failure message repeats it. A case matches the package
//! name in stderr plus a non-zero exit, never the owners' exact wording, and asserts that no
//! request reached the endpoint before that failure.
//!
//! The control case runs the same way over the complete release: it must get past assembly to
//! the refusing endpoint, and it must fail there with the provider's transport error. That is
//! what makes a negative case's failure its own reason rather than the harness's.
//!
//! The scratch binary is the built debug `p1`, resolved with the same rules
//! `installed_release.rs` uses (`$P1_BIN`, else this profile's fresh binary, else a build under
//! the build-directory lock); a missing binary fails the case naming the command to run.

use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock, mpsc};
use std::thread;
use std::time::{Duration, Instant, SystemTime};

use p1_contracts::serde_json::{self, Value};
use p1_module_runtime::{Digest, ReleaseManifest};
use p1_module_tests::p1_binary;
use tempfile::TempDir;

/// How long one scratch run may take before the case kills it. A run here is bounded by its own
/// failing transport, so only a hang reaches this.
const RUN_LIMIT: Duration = Duration::from_secs(180);

/// How often the bound above asks whether the run finished.
const RUN_POLL: Duration = Duration::from_millis(10);

// --- the classes --------------------------------------------------------------

/// How a class of host entry is first needed: the environment a headless run selects, the route
/// file that environment uses, and the flags the run needs for that class.
#[derive(Debug, Clone, Copy)]
struct Run {
    /// The `--env` the run selects.
    environment: &'static str,
    /// The route file the run's environment names, which the case points at the refusing
    /// endpoint.
    route: &'static str,
    /// Flags the class needs before the run reaches it (`--ask` for the ask policy).
    flags: &'static [&'static str],
}

/// One class of official-release host entry (the lead's findings on main, after the tag).
#[derive(Debug, Clone, Copy)]
struct Class {
    /// The `<class>` token this case's name carries.
    name: &'static str,
    /// The package's manifest name, as the release manifest spells it.
    package: &'static str,
    /// How a run first needs it.
    run: Run,
    /// The slice that makes the package a host entry, and so turns this case green.
    owner: &'static str,
}

/// A clean turn of the default environment: how the tool, policy, context and family classes
/// are first needed.
const CLAUDE_RUN: Run = Run {
    environment: "claude",
    route: "anthropic-subscription",
    flags: &[],
};

/// The same turn under the restrictive policy, which is when the ask policy is needed.
const ASK_RUN: Run = Run {
    environment: "claude",
    route: "anthropic-subscription",
    flags: &["--ask"],
};

/// The turn whose route's adapter is the OpenAI chat provider.
const OPENAI_CHAT_RUN: Run = Run {
    environment: "glm",
    route: "glm-subscription",
    flags: &[],
};

/// The turn whose route's adapter is the Anthropic provider.
const ANTHROPIC_RUN: Run = Run {
    environment: "claude",
    route: "anthropic-subscription",
    flags: &[],
};

/// The turn whose route's adapter is the OpenAI responses provider.
const OPENAI_RUN: Run = Run {
    environment: "gpt",
    route: "openai-codex-subscription",
    flags: &[],
};

/// One class, in the shape the cases build.
fn class(name: &'static str, package: &'static str, run: Run, owner: &'static str) -> Class {
    Class {
        name,
        package,
        run,
        owner,
    }
}

#[test]
fn a_release_missing_read_fails_startup_naming_it() {
    missing_host_entry(&class("read", "p1/read", CLAUDE_RUN, "S1.8.1"));
}

#[test]
fn a_release_missing_edit_fails_startup_naming_it() {
    missing_host_entry(&class("edit", "p1/edit", CLAUDE_RUN, "S7.10-R1 (#390)"));
}

#[test]
fn a_release_missing_write_fails_startup_naming_it() {
    missing_host_entry(&class("write", "p1/write", CLAUDE_RUN, "S7.10-R1 (#390)"));
}

#[test]
fn a_release_missing_search_fails_startup_naming_it() {
    missing_host_entry(&class("search", "p1/search", CLAUDE_RUN, "S7.10-R1 (#390)"));
}

#[test]
fn a_release_missing_patch_fails_startup_naming_it() {
    missing_host_entry(&class("patch", "p1/patch", OPENAI_RUN, "S7.10-R1 (#390)"));
}

#[test]
fn a_release_missing_shell_fails_startup_naming_it() {
    missing_host_entry(&class("shell", "p1/shell", CLAUDE_RUN, "S3.8 (#330)"));
}

#[test]
fn a_release_missing_finish_fails_startup_naming_it() {
    missing_host_entry(&class("finish", "p1/finish", CLAUDE_RUN, "S3.8 (#330)"));
}

#[test]
fn a_release_missing_provider_openai_chat_fails_startup_naming_it() {
    missing_host_entry(&class(
        "provider-openai-chat",
        "p1/provider-openai-chat",
        OPENAI_CHAT_RUN,
        "S4.9 (#353)",
    ));
}

#[test]
fn a_release_missing_provider_anthropic_fails_startup_naming_it() {
    missing_host_entry(&class(
        "provider-anthropic",
        "p1/provider-anthropic",
        ANTHROPIC_RUN,
        "S4.9 (#353)",
    ));
}

#[test]
fn a_release_missing_provider_openai_fails_startup_naming_it() {
    missing_host_entry(&class(
        "provider-openai",
        "p1/provider-openai",
        OPENAI_RUN,
        "S4.9 (#353)",
    ));
}

#[test]
fn a_release_missing_policy_ask_fails_startup_naming_it() {
    missing_host_entry(&class(
        "policy-ask",
        "p1/policy/ask",
        ASK_RUN,
        "S5.11 (#360)",
    ));
}

#[test]
fn a_release_missing_policy_full_access_fails_startup_naming_it() {
    missing_host_entry(&class(
        "policy-full-access",
        "p1/policy/full-access",
        CLAUDE_RUN,
        "S5.11 (#360)",
    ));
}

#[test]
fn a_release_missing_context_summarizing_fails_startup_naming_it() {
    missing_host_entry(&class(
        "context-summarizing",
        "p1/context/summarizing",
        CLAUDE_RUN,
        "S5.11 (#360)",
    ));
}

#[test]
fn a_release_missing_worker_start_fails_startup_naming_it() {
    missing_host_entry(&class(
        "worker-start",
        "p1/worker-start",
        CLAUDE_RUN,
        "S6.11",
    ));
}

#[test]
fn a_release_missing_workflow_start_fails_startup_naming_it() {
    missing_host_entry(&class(
        "workflow-start",
        "p1/workflow-start",
        CLAUDE_RUN,
        "S6.11",
    ));
}

/// The control: the complete release, the same turn, nothing removed. It must get past assembly
/// to the refusing endpoint and fail there with the provider's transport error.
#[test]
fn a_complete_release_reaches_the_provider_and_fails_there() {
    let mut endpoint = RefusingEndpoint::start();
    let release = Release::complete();
    release.refuse_provider(CLAUDE_RUN.route, endpoint.port());
    let output = headless_turn(&release, CLAUDE_RUN);
    let requests = endpoint.requests();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();

    assert!(
        !output.status.success(),
        "the complete release must still fail at the refusing provider, and it exited {}: {stderr}",
        output.status
    );
    assert!(
        requests > 0,
        "the complete release must get past assembly to the provider, and nothing reached the \
         endpoint: {stderr}"
    );
    assert!(
        stderr.contains("Transport"),
        "the complete release must fail with the provider's transport error, not a missing host \
         entry; stderr was: {stderr}"
    );
}

/// One negative case: the scratch release is missing `class.package`, so the run must fail
/// naming that package, with a non-zero exit and before any request reaches a provider.
fn missing_host_entry(class: &Class) {
    let mut endpoint = RefusingEndpoint::start();
    let release = Release::without(class.package);
    release.refuse_provider(class.run.route, endpoint.port());
    let output = headless_turn(&release, class.run);
    let requests = endpoint.requests();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();

    assert!(
        !output.status.success(),
        "the {} class: a release without {} must fail the run, and it exited {}: {stderr}",
        class.name,
        class.package,
        output.status
    );
    assert!(
        stderr.contains(class.package),
        "the {} class: a release without {} must fail startup naming it instead of answering \
         natively (the slice that composes it is {}); stderr was: {stderr}",
        class.name,
        class.package,
        class.owner
    );
    assert_eq!(
        requests, 0,
        "the {} class: a release without {} must fail before any provider call, and {} request(s) \
         reached the endpoint",
        class.name, class.package, requests
    );
}

/// Runs one headless turn of the scratch release's own `p1`: the class's environment and route,
/// one prompt, and a session file inside the scratch tree. Every path p1 resolves from the
/// environment is inside that tree, and the provider's endpoint refuses.
fn headless_turn(release: &Release, run: Run) -> Output {
    let mut command = Command::new(release.bin());
    command
        .arg("--env")
        .arg(run.environment)
        .arg("--session")
        .arg(release.session())
        // `--provider-retries 0` bounds the host's turn-level loop, which otherwise waits out a
        // transient provider failure and continues the turn instead of ending on it
        // (`crates/p1-host/src/run.rs`). It does not bound the provider transport, whose retry
        // budget is fixed: a case still fails on its own assertion, never on the transport's
        // timing.
        .arg("--provider-retries")
        .arg("0")
        .args(run.flags)
        .arg("hello")
        .current_dir(release.cwd())
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", release.home())
        .env("P1_CONFIG_DIR", release.config())
        .env("XDG_CONFIG_HOME", release.config())
        .env("XDG_DATA_HOME", release.data())
        .env("TMPDIR", release.scratch())
        .env("LC_ALL", "C");
    run_bounded(&mut command, RUN_LIMIT, run.environment)
}

// --- the refusing endpoint ----------------------------------------------------

/// A loopback endpoint that accepts every connection and closes it at once: a provider that
/// reaches it fails at the transport, so a run with no live service is still a run that cannot
/// get an answer. It counts what reached it, which is how a case proves the run failed before
/// any provider call (no connection) or after one (the control case).
///
/// The adapters refuse a route endpoint that is not HTTPS, so a case points the route at
/// `https://127.0.0.1:<port>` rather than at a named port.
struct RefusingEndpoint {
    port: u16,
    count: Arc<AtomicUsize>,
    acceptor: Option<thread::JoinHandle<()>>,
}

impl RefusingEndpoint {
    /// Binds an ephemeral loopback port and starts accepting.
    fn start() -> Self {
        Self::spawn(None)
    }

    /// The same endpoint with a test seam: the acceptor signals every accepted connection on
    /// the returned receiver and blocks until the test sends on the returned sender before it
    /// decides whether the connection was the fence. A test can therefore hold a connection
    /// between `accept` and that decision, fence while it is held, and prove the held connection
    /// is still counted — the ordering issue #417 is about, without a timing race.
    fn held() -> (Self, mpsc::Receiver<()>, mpsc::Sender<()>) {
        let (accepted_tx, accepted_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        (
            Self::spawn(Some((accepted_tx, release_rx))),
            accepted_rx,
            release_tx,
        )
    }

    fn spawn(gate: Option<(mpsc::Sender<()>, mpsc::Receiver<()>)>) -> Self {
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind a loopback endpoint");
        let port = listener
            .local_addr()
            .expect("the endpoint's address")
            .port();
        let count = Arc::new(AtomicUsize::new(0));
        let acceptor = {
            let listener = listener;
            let count = Arc::clone(&count);
            thread::spawn(move || {
                loop {
                    match listener.accept() {
                        Ok((mut stream, _)) => {
                            if let Some((accepted, release)) = &gate {
                                let _ = accepted.send(());
                                let _ = release.recv();
                            }
                            // A stop flag can discard queued provider connections; only the
                            // explicitly marked fence ends the accept loop.
                            let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
                            let mut first = [0];
                            if stream.read(&mut first).ok() == Some(1) && first[0] == b'F' {
                                return;
                            }
                            count.fetch_add(1, Ordering::SeqCst);
                            let _ = stream.shutdown(Shutdown::Both);
                        }
                        Err(_) => return,
                    }
                }
            })
        };
        Self {
            port,
            count,
            acceptor: Some(acceptor),
        }
    }

    fn port(&self) -> u16 {
        self.port
    }

    fn fence(&self) {
        let mut stream =
            TcpStream::connect(("127.0.0.1", self.port)).expect("connect the endpoint fence");
        stream.write_all(b"F").expect("send the endpoint fence");
    }

    /// How many connections reached the endpoint, once the run has ended: the acceptor is woken
    /// by the fence connection this queues, and a blocking `accept` returns connections in
    /// order, so a woken acceptor has counted every connection the run made.
    fn requests(&mut self) -> usize {
        if let Some(acceptor) = self.acceptor.take() {
            self.fence();
            acceptor.join().expect("endpoint acceptor");
        }
        self.count.load(Ordering::SeqCst)
    }

    /// Join the acceptor without fencing again, for a case whose fence the acceptor already
    /// consumed through a seam.
    fn finish(mut self) -> usize {
        if let Some(acceptor) = self.acceptor.take() {
            acceptor.join().expect("endpoint acceptor");
        }
        self.count.load(Ordering::SeqCst)
    }
}

impl Drop for RefusingEndpoint {
    fn drop(&mut self) {
        if let Some(acceptor) = self.acceptor.take() {
            self.fence();
            let _ = acceptor.join();
        }
    }
}

#[test]
fn refusing_endpoint_drains_connections_before_the_fence() {
    let mut endpoint = RefusingEndpoint::start();
    let first = TcpStream::connect(("127.0.0.1", endpoint.port())).unwrap();
    let second = TcpStream::connect(("127.0.0.1", endpoint.port())).unwrap();
    drop((first, second));
    assert_eq!(endpoint.requests(), 2);
}

#[test]
fn a_connection_held_before_the_fence_is_still_counted() {
    let (endpoint, accepted, release) = RefusingEndpoint::held();
    let queued = TcpStream::connect(("127.0.0.1", endpoint.port())).unwrap();
    // The acceptor has taken the queued connection and holds it before the fence decision.
    accepted
        .recv_timeout(Duration::from_secs(5))
        .expect("the queued connection was accepted");
    drop(queued);
    // Fence while the acceptor is held: a stop flag set here would discard the queued
    // connection, which is exactly the #417 false green.
    endpoint.fence();
    release.send(()).expect("release the held connection");
    // The fence connection reaches the same seam; release it to end the accept loop.
    accepted
        .recv_timeout(Duration::from_secs(5))
        .expect("the fence connection was accepted");
    release.send(()).expect("release the fence connection");
    assert_eq!(endpoint.finish(), 1);
}

// --- the scratch release ------------------------------------------------------

/// One scratch installed release: `bin/p1` and `share/p1/` in a temporary directory, holding
/// the built module set with at most one package removed, the shipped environments, routes and
/// profiles, and the run's home, config and cwd.
struct Release {
    dir: TempDir,
}

impl Release {
    /// The built release with every package: what the control case runs against.
    fn complete() -> Self {
        Self::build(None)
    }

    /// The built release without `package`, which must be one of its components.
    fn without(package: &str) -> Self {
        Self::build(Some(package))
    }

    fn build(removed: Option<&str>) -> Self {
        let root = repo_root();
        let binary = p1();
        let outputs = module_outputs();

        let dir = tempfile::Builder::new()
            .prefix("p1-host-entry-")
            .tempdir()
            .expect("a scratch directory");
        let scratch = dir.path().to_path_buf();
        let share = scratch.join("share/p1");
        let modules = share.join("modules");
        create_dir(&scratch.join("bin"));
        fs::copy(&binary, scratch.join("bin/p1"))
            .unwrap_or_else(|error| panic!("copy {}: {error}", binary.display()));
        copy_tree(&outputs, &modules);
        for name in ["environments", "routes", "profiles", "accounts"] {
            copy_tree(&root.join(name), &share.join(name));
        }

        // Remove the one package: its manifest entry and its package directory. The manifest's
        // other fields (the pin, the toolchain, the empty packages list) are left as built.
        let manifest_path = modules.join("manifest.json");
        let mut manifest: Value =
            serde_json::from_str(&fs::read_to_string(&manifest_path).expect("the built manifest"))
                .expect("the built manifest is JSON");
        let components = manifest["components"]
            .as_array_mut()
            .expect("the manifest's components");
        let built = components.len();
        if let Some(package) = removed {
            let index = components
                .iter()
                .position(|entry| entry["name"].as_str() == Some(package))
                .unwrap_or_else(|| panic!("the built release has no {package}"));
            let entry = components.remove(index);
            let path = entry["path"].as_str().expect("the entry's path");
            let directory = Path::new(path)
                .parent()
                .and_then(Path::file_name)
                .expect("a package directory");
            fs::remove_dir_all(modules.join(directory))
                .unwrap_or_else(|error| panic!("remove {}: {error}", directory.display()));
        }
        fs::write(
            &manifest_path,
            serde_json::to_string(&manifest).expect("the manifest serializes"),
        )
        .expect("write the trimmed manifest");

        // The trim must not be what a case fails on: the manifest still parses, the removed
        // component is gone, the package directories and the entries still agree, and every
        // remaining entry's digest still matches its bytes.
        let manifest = ReleaseManifest::read(&manifest_path).expect("the trimmed manifest parses");
        let expected = built - usize::from(removed.is_some());
        assert_eq!(
            manifest.components().len(),
            expected,
            "the trimmed manifest lists another number of components"
        );
        if let Some(package) = removed {
            assert!(
                manifest.entry(package).is_none(),
                "{package} is still in the trimmed manifest"
            );
        }
        let entries = fs::read_dir(&modules).expect("the module set").count();
        assert_eq!(
            entries - 1,
            manifest.components().len(),
            "the package directories and the manifest components disagree"
        );
        for entry in manifest.components() {
            let path = modules.join(&entry.path);
            let bytes =
                fs::read(&path).unwrap_or_else(|error| panic!("{}: {error}", path.display()));
            assert_eq!(
                Digest::of(&bytes),
                entry.digest,
                "{}: the bytes are not the manifest's digest",
                entry.path
            );
        }

        for name in ["home", "config", "data", "cwd"] {
            create_dir(&scratch.join(name));
        }
        Self { dir }
    }

    fn scratch(&self) -> PathBuf {
        self.dir.path().to_path_buf()
    }

    fn bin(&self) -> PathBuf {
        self.scratch().join("bin/p1")
    }

    fn home(&self) -> PathBuf {
        self.scratch().join("home")
    }

    fn config(&self) -> PathBuf {
        self.scratch().join("config")
    }

    fn data(&self) -> PathBuf {
        self.scratch().join("data")
    }

    fn cwd(&self) -> PathBuf {
        self.scratch().join("cwd")
    }

    fn session(&self) -> PathBuf {
        self.scratch().join("session.jsonl")
    }

    /// Rewrites this release's copy of the route file `id` so the run can reach no provider:
    /// the endpoint is the refusing one, and the credential is `none`, which reads no store.
    fn refuse_provider(&self, id: &str, port: u16) {
        let path = self
            .scratch()
            .join("share/p1/routes")
            .join(format!("{id}.toml"));
        let text =
            fs::read_to_string(&path).unwrap_or_else(|error| panic!("{}: {error}", path.display()));
        let endpoint = format!("https://127.0.0.1:{port}");
        fs::write(&path, refusing_route(&text, &endpoint)).expect("write the route override");
    }
}

/// `text` with its `endpoint` pointed at `endpoint` and its whole `[credential]` table replaced
/// by `kind = "none"`: the shipped routes are otherwise unchanged, so the run drives the same
/// loader, adapters and credential code a real route does.
fn refusing_route(text: &str, endpoint: &str) -> String {
    let mut out = String::new();
    let mut in_credential = false;
    let mut replaced_credential = false;
    let mut replaced_account = false;
    let mut replaced_endpoint = false;
    for line in text.lines() {
        if line.trim_start().starts_with('[') {
            in_credential = line.trim() == "[credential]";
            if in_credential {
                out.push_str("[credential]\nkind = \"none\"\n");
                replaced_credential = true;
                continue;
            }
        }
        if in_credential {
            continue;
        }
        match line.split_once('=') {
            Some((key, _)) if key.trim() == "endpoint" => {
                out.push_str(&format!("endpoint = \"{endpoint}\"\n"));
                replaced_endpoint = true;
            }
            // ADR-0139: a converted shipped route names its account instead of carrying a
            // credential; the `none` table below replaces that account.
            Some((key, _)) if key.trim() == "account" && !replaced_credential => {
                replaced_account = true;
            }
            _ => {
                out.push_str(line);
                out.push('\n');
            }
        }
    }
    if !replaced_credential && replaced_account {
        out.push_str("[credential]\nkind = \"none\"\n");
        replaced_credential = true;
    }
    assert!(replaced_endpoint, "the route names no endpoint");
    assert!(
        replaced_credential,
        "the route has no [credential] table or account"
    );
    out
}

// --- the built binary and the built module set --------------------------------

/// The `p1` binary the scratch releases copy, built once per test binary.
static P1: OnceLock<PathBuf> = OnceLock::new();

/// The built module packages the scratch releases copy, once per test binary.
static MODULES: OnceLock<PathBuf> = OnceLock::new();

/// The p1 binary to copy: `$P1_BIN` as given, else the executable a locked `cargo build
/// -p p1-host --bin p1` reports for this checkout (`p1_module_tests::p1_binary`).
fn p1() -> PathBuf {
    P1.get_or_init(|| p1_binary(&repo_root())).clone()
}

/// The built module package outputs, building them when they are not current. They are
/// published into the checkout (never into the cargo target directory), so a missing or stale
/// set is rebuilt once and a nested build that cannot run fails the case by name.
fn module_outputs() -> PathBuf {
    MODULES
        .get_or_init(|| {
            let root = repo_root();
            let published = root.join("modules/target/p1-modules");
            if module_outputs_current(&root, &published) {
                return published;
            }
            if let Err(reason) = run_locked(&root, "bash", &["scripts/build-modules.sh", "--all"]) {
                assert!(
                    module_outputs_current(&root, &published),
                    "the module packages under {} are missing or stale: {reason}; run \
                     scripts/build-modules.sh --all",
                    published.display()
                );
            }
            published
        })
        .clone()
}

/// Whether every published package is there and newer than the newest module source. The
/// manifest is written beside the package directories and names no package of its own, so it is
/// skipped here (`scripts/release-manifest.py` reads its build directory the same way); only a
/// genuinely missing or stale set is rebuilt.
fn module_outputs_current(root: &Path, published: &Path) -> bool {
    if !published.join("manifest.json").is_file() {
        return false;
    }
    let newest_source = newest_mtime(&root.join("modules"), Some("target"));
    let Ok(entries) = fs::read_dir(published) else {
        return false;
    };
    let mut packages = 0;
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name == "manifest.json" {
            continue;
        }
        if !entry.file_type().is_ok_and(|kind| kind.is_dir()) {
            return false;
        }
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

/// The repository root of this worktree.
fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("the repository root")
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
fn run_locked(
    root: &Path,
    program: impl AsRef<std::ffi::OsStr>,
    args: &[&str],
) -> Result<(), String> {
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

// --- small helpers ------------------------------------------------------------

/// Runs `command` with its output captured, killing it once `limit` has passed: a case's run is
/// bounded, so a hung child fails the case rather than the gate's own timeout. The readers keep
/// a full pipe from blocking the child.
fn run_bounded(command: &mut Command, limit: Duration, what: &str) -> Output {
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap_or_else(|error| panic!("cannot start the p1 run for {what}: {error}"));
    let stdout = child.stdout.take().expect("stdout pipe");
    let stderr = child.stderr.take().expect("stderr pipe");
    let out = thread::spawn(move || read_all(stdout));
    let err = thread::spawn(move || read_all(stderr));

    let started = Instant::now();
    let mut timed_out = false;
    let status = loop {
        if let Some(status) = child.try_wait().expect("poll the p1 run") {
            break status;
        }
        if started.elapsed() >= limit {
            timed_out = true;
            let _ = child.kill();
            break child.wait().expect("wait for the killed p1 run");
        }
        thread::sleep(RUN_POLL);
    };
    let output = Output {
        status,
        stdout: out.join().expect("the stdout reader"),
        stderr: err.join().expect("the stderr reader"),
    };
    assert!(
        !timed_out,
        "the p1 run for {what} did not finish within {limit:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

/// Everything `reader` yields.
fn read_all(mut reader: impl Read) -> Vec<u8> {
    let mut bytes = Vec::new();
    let _ = reader.read_to_end(&mut bytes);
    bytes
}

/// Copies `source` to `target` recursively. The build outputs and the shipped data hold only
/// regular files and directories.
fn copy_tree(source: &Path, target: &Path) {
    create_dir(target);
    for entry in
        fs::read_dir(source).unwrap_or_else(|error| panic!("read {}: {error}", source.display()))
    {
        let entry = entry.expect("a directory entry");
        let from = entry.path();
        let to = target.join(entry.file_name());
        if entry.metadata().expect("the entry's metadata").is_dir() {
            copy_tree(&from, &to);
        } else {
            fs::copy(&from, &to).unwrap_or_else(|error| panic!("copy {}: {error}", from.display()));
        }
    }
}

fn create_dir(path: &Path) {
    fs::create_dir_all(path).unwrap_or_else(|error| panic!("create {}: {error}", path.display()));
}
