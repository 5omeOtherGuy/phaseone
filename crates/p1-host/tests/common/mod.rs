//! Shared fakes for the host's end-to-end tests. Nothing here touches the
//! network or a real credential file.

#![allow(dead_code)]

use std::collections::VecDeque;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use p1_assembly::Catalog;
use p1_contracts::Provider;
use p1_host::{HostDeps, InterruptSource, LineSource, SharedWriter};
use p1_provider_http::testing::ScriptedTransport;
use p1_testkit::ScriptedProvider;

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
