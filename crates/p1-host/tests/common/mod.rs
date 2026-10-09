//! Shared fakes for the host's end-to-end tests. Nothing here touches the
//! network or a real credential file.

#![allow(dead_code)]

use std::collections::VecDeque;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::{Arc, Mutex, OnceLock};

use p1_assembly::Catalog;
use p1_contracts::Provider;
use p1_host::HostDeps;
use p1_host::catalog::ProviderComponents;
use p1_host::{InterruptSource, LineSource, SharedWriter};
use p1_module_tests::Release;
use p1_provider_http::testing::ScriptedTransport;
use p1_testkit::ScriptedProvider;

/// The provider packages `scripts/build-modules.sh` publishes: the manifest name the release
/// calls each one and the package directory the build writes it under.
pub const PROVIDER_PACKAGES: [(&str, &str); 3] = [
    ("p1/provider-anthropic", "p1-module-provider-anthropic"),
    ("p1/provider-openai", "p1-module-provider-openai"),
    ("p1/provider-openai-chat", "p1-module-provider-openai-chat"),
];

/// One built provider package: the component bytes the build published and the release entry
/// its package manifest describes.
pub struct ProviderPackage {
    /// The manifest name of the component.
    pub name: &'static str,
    /// The `.wasm` component.
    pub bytes: Vec<u8>,
    /// The package's release-manifest entry, as the build recorded it.
    pub entry: serde_json::Value,
}

/// The built provider packages. A test binary that composes a shipped route needs them, so a
/// missing artifact fails the case with how to build it instead of skipping.
pub fn built_provider_packages() -> &'static [ProviderPackage; 3] {
    static BUILT: OnceLock<[ProviderPackage; 3]> = OnceLock::new();
    BUILT.get_or_init(|| {
        PROVIDER_PACKAGES.map(|(name, package)| {
            let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../modules/target/p1-modules")
                .join(package);
            let read = |file: String| {
                let path = dir.join(file);
                std::fs::read(&path).unwrap_or_else(|error| {
                    panic!(
                        "the provider artifact {} is missing ({error}): run scripts/build-modules.sh --all first",
                        path.display()
                    )
                })
            };
            let manifest: serde_json::Value =
                serde_json::from_slice(&read(format!("{package}.manifest.json")))
                    .expect("a package manifest");
            assert_eq!(manifest["name"], name, "{package}");
            let file = name.replace('/', "-");
            ProviderPackage {
                name,
                bytes: read(format!("{package}.wasm")),
                entry: serde_json::json!({
                    "name": name,
                    "digest": manifest["digest"],
                    "path": format!("packages/{file}/{file}.wasm"),
                    "kind": manifest["kind"],
                    "world": manifest["world"],
                    "protocol": manifest["protocol"],
                    "capabilities": manifest["capabilities"],
                    "variant": manifest["variant"],
                }),
            }
        })
    })
}

/// A release laid out in a temp directory that holds the built provider components, with
/// `edit` applied to the entry of every one of them: the way S4.7's `provider_conformance.rs`
/// loads the same packages, so a test can hold a refusal by tampering with an entry.
pub fn provider_release(edit: &dyn Fn(&mut serde_json::Value)) -> Release {
    let mut release = Release::empty();
    for package in built_provider_packages() {
        let mut entry = package.entry.clone();
        edit(&mut entry);
        release.add(entry, &package.bytes);
    }
    release
}

/// The provider components the build published, read once per test binary through a release
/// manifest in a temp directory. Production discovery is the ONE path (`official_release_manifest`,
/// ADR-0079, plus S3.8.0's debug fallback once it is on main); a test binary installs no module
/// set, so it lays the built packages out as a release and reads it through
/// `ProviderComponents::read`, exactly as S4.7's conformance suite does. No second path.
pub fn provider_components() -> &'static ProviderComponents {
    /// The release directory must outlive the loader that reads the packages out of it.
    struct Built {
        _release: Release,
        components: ProviderComponents,
    }
    static COMPONENTS: OnceLock<Built> = OnceLock::new();
    &COMPONENTS
        .get_or_init(|| {
            let release = provider_release(&|_| {});
            let components = ProviderComponents::read(&release.manifest_file())
                .expect("the built provider release");
            Built {
                _release: release,
                components,
            }
        })
        .components
}

/// A writer that appends to an in-memory buffer the test can read.
pub struct Capture {
    buffer: Arc<Mutex<Vec<u8>>>,
}

impl Capture {
    pub fn new() -> (SharedWriter, Self) {
        let buffer = Arc::new(Mutex::new(Vec::new()));
        let writer: SharedWriter = Arc::new(Mutex::new(Box::new(CaptureWriter(buffer.clone()))));
        (writer, Self { buffer })
    }

    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.buffer.lock().unwrap()).to_string()
    }
}

struct CaptureWriter(Arc<Mutex<Vec<u8>>>);

impl Write for CaptureWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// A line source that replays a fixed list of lines and then EOF.
pub struct ScriptedLines {
    queue: Mutex<VecDeque<Option<String>>>,
}

impl ScriptedLines {
    pub fn new(lines: &[&str]) -> Arc<Self> {
        let mut queue: VecDeque<Option<String>> =
            lines.iter().map(|line| Some(line.to_string())).collect();
        queue.push_back(None);
        Arc::new(Self {
            queue: Mutex::new(queue),
        })
    }
}

impl LineSource for ScriptedLines {
    fn next_line<'a>(
        &'a self,
    ) -> Pin<Box<dyn std::future::Future<Output = Option<String>> + Send + 'a>> {
        let next = self.queue.lock().unwrap().pop_front().flatten();
        Box::pin(async move { next })
    }
}

/// A Ctrl-C source the test fires explicitly.
pub struct ChannelInterrupt {
    tx: tokio::sync::mpsc::UnboundedSender<()>,
    rx: tokio::sync::Mutex<tokio::sync::mpsc::UnboundedReceiver<()>>,
}

impl ChannelInterrupt {
    pub fn new() -> Arc<Self> {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        Arc::new(Self {
            tx,
            rx: tokio::sync::Mutex::new(rx),
        })
    }

    pub fn fire(&self) {
        let _ = self.tx.send(());
    }
}

impl InterruptSource for ChannelInterrupt {
    fn recv<'a>(&'a self) -> Pin<Box<dyn std::future::Future<Output = ()> + Send + 'a>> {
        Box::pin(async move {
            let mut rx = self.rx.lock().await;
            let _ = rx.recv().await;
        })
    }
}

/// A registered fake provider factory set.
pub fn provider_hook(
    entries: Vec<(&'static str, ScriptedProvider)>,
) -> p1_host::catalog::CatalogHook {
    provider_hook_arc(
        entries
            .into_iter()
            .map(|(key, provider)| (key, Arc::new(provider) as Arc<dyn Provider>))
            .collect(),
    )
}

/// Like [`provider_hook`] for arbitrary fake providers (e.g. a gated wrapper).
pub fn provider_hook_arc(
    entries: Vec<(&'static str, Arc<dyn Provider>)>,
) -> p1_host::catalog::CatalogHook {
    Box::new(move |catalog: &mut Catalog| {
        for (key, provider) in &entries {
            let provider = provider.clone();
            catalog.provider(key, Box::new(move |_spec| Ok(provider.clone())));
        }
    })
}

/// Run `f` inside a current-thread Tokio runtime.
///
/// A module tool must be built inside a runtime, which runs its executor. Since S1.8.1 the
/// release's `p1/read` host entry answers to the `read` key, so a SYNC test that assembles one
/// of the shipped environments — every one that names `read` does — needs a runtime here; an
/// `#[tokio::test]` already has one and calls nothing of this. A case that also EXECUTES a
/// module tool keeps its own runtime (an `#[tokio::test]`), because a tool built on a runtime
/// that is gone cannot run.
pub fn on_runtime<T>(f: impl FnOnce() -> T) -> T {
    tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("a current-thread runtime")
        .block_on(async { f() })
}

/// The path to the shipped environments directory.
pub fn shipped_environments() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../environments")
}

/// Write `<root>/<name>/environment.toml` and `prompt.md`.
pub fn write_environment(
    root: &Path,
    name: &str,
    provider: &str,
    model: &str,
    tools: &[&str],
    prompt: &str,
) {
    let dir = root.join(name);
    std::fs::create_dir_all(&dir).unwrap();
    let mut toml = format!("family = \"{name}\"\nprovider = \"{provider}\"\nmodel = \"{model}\"\n");
    for tool in tools {
        toml.push_str(&format!("[[tools]]\nmodule = \"{tool}\"\n"));
    }
    std::fs::write(dir.join("environment.toml"), toml).unwrap();
    std::fs::write(dir.join("prompt.md"), prompt).unwrap();
}

/// Everything a test needs to drive the host.
pub struct Harness {
    pub deps: HostDeps,
    pub stdout: Capture,
    pub stderr: Capture,
    pub lines: Arc<ScriptedLines>,
    pub interrupt: Arc<ChannelInterrupt>,
}

impl Harness {
    pub fn new(environment_dirs: Vec<PathBuf>, lines: &[&str]) -> Self {
        let (stdout_writer, stdout) = Capture::new();
        let (stderr_writer, stderr) = Capture::new();
        let lines = ScriptedLines::new(lines);
        let interrupt = ChannelInterrupt::new();
        let mut deps = HostDeps::new(
            stdout_writer,
            stderr_writer,
            lines.clone(),
            Arc::new(ScriptedTransport::new(Vec::new())),
            "2026-01-02".to_string(),
            interrupt.clone(),
            environment_dirs,
            false,
        );
        // No test touches the real home: the credential chain resolves its locations
        // from this field, and a test that needs a home (the sandbox ones) injects one.
        deps.home = None;
        Self {
            deps,
            stdout,
            stderr,
            lines,
            interrupt,
        }
    }
}

/// The host sees NO ambient environment: an injected empty snapshot replaces the
/// process environment, so no credential path can point outside the scratch
/// directories a test wrote. Every `env show` test calls this — `env show` reports
/// which credential source a route would use, and that probe must never reach a real
/// login.
pub fn isolated_environment(harness: &mut Harness) {
    harness.deps.shell_env = Some(Vec::new());
}

/// The resolved environment `env show` printed, after its `credential  …` and
/// `model  …` lines.
pub fn env_show_json(stdout: &str) -> serde_json::Value {
    let json = stdout
        .lines()
        .skip_while(|line| !line.trim_start().starts_with('{'))
        .collect::<Vec<_>>()
        .join("\n");
    serde_json::from_str(json.trim()).expect("env show prints the resolved environment as JSON")
}

/// Parse `args` and run the host. Panics on a usage error so a typo in a test is
/// loud rather than a silent exit code.
pub async fn run_args(harness: &mut Harness, args: &[&str]) -> i32 {
    let args: Vec<String> = args.iter().map(|arg| arg.to_string()).collect();
    let options = p1_host::cli::parse(&args).expect("test args must parse");
    p1_host::run::run(&mut harness.deps, options).await
}

/// ADR-0118 test 3: the request that follows a response with two Shared `read` calls whose
/// second call finished first. The core runs them together and the scripted provider records
/// the follow-up, whose history holds the two results in block order (`first`, `second`).
pub async fn parallel_reads_follow_up() -> p1_contracts::ProviderRequest {
    use p1_contracts::{
        BoxFuture, CancellationToken, Concurrency, DeclarationKind, Effect, ModelOptions, Tool,
        ToolCall, ToolContext, ToolDeclaration, ToolIdentity, ToolOutcome,
    };
    use p1_testkit::{
        PassthroughContext, RecordingEvents, RecordingJournal, ScriptedAuthorization, json_call,
        text_response, tool_call_response,
    };

    /// A Shared read whose call `first` returns only after `second` ran.
    struct SecondFirst(ToolDeclaration, ToolIdentity, tokio::sync::Notify);
    impl Tool for SecondFirst {
        fn declaration(&self) -> &ToolDeclaration {
            &self.0
        }
        fn identity(&self) -> &ToolIdentity {
            &self.1
        }
        fn effect(&self, _call: &ToolCall) -> Effect {
            Effect::ReadOnly
        }
        fn concurrency(&self, _call: &ToolCall) -> Concurrency {
            Concurrency::Shared
        }
        fn execute<'a>(
            &'a self,
            call: &'a ToolCall,
            _context: ToolContext,
        ) -> BoxFuture<'a, ToolOutcome> {
            Box::pin(async move {
                if call.call_id == "first" {
                    self.2.notified().await;
                } else {
                    self.2.notify_one();
                }
                ToolOutcome::ok(format!("{} content", call.call_id))
            })
        }
    }

    let tool: Arc<dyn Tool> = Arc::new(SecondFirst(
        ToolDeclaration {
            name: "read".into(),
            description: "Read a file".into(),
            kind: DeclarationKind::Function {
                input_schema: p1_contracts::serde_json::json!({"type": "object"}),
            },
        },
        ToolIdentity {
            implementation: "second-first".into(),
            variant: "test".into(),
        },
        tokio::sync::Notify::new(),
    ));
    let provider = Arc::new(ScriptedProvider::new(vec![
        tool_call_response(vec![
            json_call("first", "read", r#"{"file_path":"a"}"#),
            json_call("second", "read", r#"{"file_path":"b"}"#),
        ]),
        text_response("done"),
    ]));
    let mut agent = p1_core::Agent::new(p1_core::AgentParts {
        provider: provider.clone(),
        tools: vec![tool],
        system_prompt: "parallel reads".into(),
        options: ModelOptions::default(),
        context: Arc::new(PassthroughContext),
        authorization: Arc::new(ScriptedAuthorization::permit_all()),
        journal: Arc::new(RecordingJournal::new()),
        events: Arc::new(RecordingEvents::new()),
    })
    .expect("the agent builds");
    tokio::time::timeout(
        std::time::Duration::from_secs(30),
        agent.run_turn("read both".into(), CancellationToken::new()),
    )
    .await
    .expect("the turn finishes");
    provider.requests()[1].clone()
}
