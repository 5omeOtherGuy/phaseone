//! #511 definition of done item 1 and requirement 5: every shipped environment that lists
//! `shell` lists `read_output` too, and `p1 env show` resolves it to the release's
//! `p1/read-output` component, run through the host exactly as the command line runs it.
//!
//! No terminal, network, credential or home is touched: output goes to buffers, the transport
//! is scripted and empty, the host sees an empty environment snapshot and no home, and `env
//! show` builds no provider.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use p1_contracts::serde_json::{self, Value};
use p1_host::{HostDeps, InterruptSource, ReaderLines, SharedWriter};
use p1_provider_http::testing::ScriptedTransport;

fn shipped_environments() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../environments")
}

/// A buffer the host writes into.
#[derive(Clone, Default)]
struct Buffer(Arc<Mutex<Vec<u8>>>);

impl Write for Buffer {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Buffer {
    fn writer(&self) -> SharedWriter {
        Arc::new(Mutex::new(Box::new(self.clone())))
    }

    fn text(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().unwrap()).into_owned()
    }
}

struct NoInterrupt;

impl InterruptSource for NoInterrupt {
    fn recv<'a>(&'a self) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + 'a>> {
        Box::pin(std::future::pending())
    }
}

/// `p1 env show NAME` over the shipped environments: its exit code, stdout and stderr.
async fn env_show(name: &str) -> (i32, String, String) {
    let (stdout, stderr) = (Buffer::default(), Buffer::default());
    let mut deps = HostDeps::new(
        stdout.writer(),
        stderr.writer(),
        Arc::new(ReaderLines::from_reader(tokio::io::empty())),
        Arc::new(ScriptedTransport::new(Vec::new())),
        "2026-01-02".to_string(),
        Arc::new(NoInterrupt),
        vec![shipped_environments()],
        false,
    );
    deps.home = None;
    deps.shell_env = Some(Vec::new());
    let args: Vec<String> = ["env", "show", name].map(str::to_owned).to_vec();
    let options = p1_host::cli::parse(&args).expect("env show parses");
    let code = p1_host::run::run(&mut deps, options).await;
    (code, stdout.text(), stderr.text())
}

/// The shipped environment names whose file lists the `shell` module.
fn environments_with_shell() -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(shipped_environments())
        .expect("the shipped environments")
        .filter_map(|entry| {
            let entry = entry.ok()?;
            let text = std::fs::read_to_string(entry.path().join("environment.toml")).ok()?;
            modules(&text)
                .contains(&"shell".to_owned())
                .then(|| entry.file_name().to_string_lossy().into_owned())
        })
        .collect();
    names.sort();
    names
}

/// The `module` keys of an environment file's `[[tools]]` tables, read line by line: the files
/// spell each as `module = "<key>"` and this suite needs nothing else from them.
fn modules(text: &str) -> Vec<String> {
    text.lines()
        .filter_map(|line| {
            let value = line.trim().strip_prefix("module")?.trim_start();
            let value = value.strip_prefix('=')?.trim();
            Some(value.trim_matches('"').to_owned())
        })
        .collect()
}

#[test]
fn every_environment_that_lists_shell_lists_read_output() {
    let names = environments_with_shell();
    assert!(names.len() >= 10, "{names:?}");
    for name in names {
        let text =
            std::fs::read_to_string(shipped_environments().join(&name).join("environment.toml"))
                .unwrap();
        assert!(
            modules(&text).contains(&"read_output".to_owned()),
            "`{name}` lists shell but not read_output"
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn env_show_resolves_read_output_to_its_component() {
    for name in environments_with_shell() {
        let (code, stdout, stderr) = env_show(&name).await;
        assert_eq!(code, 0, "env show {name}: {stderr}");
        let json = &stdout[stdout.find('{').expect("the resolved environment")..];
        let resolved: Value = serde_json::from_str(json).expect("JSON");
        let tools = resolved["tools"].as_array().expect("tools");
        let tool = tools
            .iter()
            .find(|tool| tool["module"] == "read_output")
            .unwrap_or_else(|| panic!("`{name}` assembles read_output: {stdout}"));
        assert_eq!(
            tool["identity"]["implementation"], "p1/read-output",
            "`{name}`: {tool}"
        );
    }
}
