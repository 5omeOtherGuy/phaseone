//! #511 (ADR-0109 item 8): the shipped `p1/shell` component names the output the host stored
//! when its result shows less than the command printed, and the shipped `p1/read-output`
//! component pages that output back, byte for byte.
//!
//! Both components are loaded by name from the packages `scripts/build-modules.sh` built and
//! linked as the host links them (`crates/p1-host/src/catalog/tools.rs` and `modules.rs`): the
//! shell over a process capability that tees each call's command into the run's store, with the
//! same call's store view as its `tool-outputs`, and `read_output` over a view of the same store.
//! No sandbox is involved: storing is the process capability's, sandboxed or not.

use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use p1_contracts::serde_json::{self, Value, json};
use p1_contracts::{CancellationToken, Tool, ToolCall, ToolContext, ToolInput, ToolOutcome};
use p1_module_runtime::process::{ProcessCapability, ProcessService};
use p1_module_runtime::{
    CallOutputs, ExecutionLimits, Loader, OutputCaps, OutputStore, ReleaseManifest, Services,
    wasm_tool,
};
use p1_module_tests::within_deadline;
use p1_redact::MaskCounter;

const SHELL: (&str, &str) = ("p1-module-shell", "p1/shell");
const READ_OUTPUT: (&str, &str) = ("p1-module-read-output", "p1/read-output");

/// The tool result envelope the shell keeps every result inside.
const ENVELOPE: usize = 50_000;

fn built_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../modules/target/p1-modules")
}

/// One loader over the two built packages, laid out as a release ships them.
fn loader() -> &'static Loader {
    static LOADER: OnceLock<Loader> = OnceLock::new();
    LOADER.get_or_init(|| {
        let entries: Vec<Value> = [SHELL, READ_OUTPUT]
            .into_iter()
            .map(|(package, _)| {
                let path = built_dir()
                    .join(package)
                    .join(format!("{package}.manifest.json"));
                let text = std::fs::read_to_string(&path).unwrap_or_else(|error| {
                    panic!(
                        "{} is missing ({error}): run scripts/build-modules.sh first",
                        path.display()
                    )
                });
                let manifest: Value = serde_json::from_str(&text).expect("package manifest");
                json!({
                    "name": manifest["name"],
                    "digest": manifest["digest"],
                    "path": format!("{package}/{package}.wasm"),
                    "kind": manifest["kind"],
                    "world": manifest["world"],
                    "protocol": manifest["protocol"],
                    "capabilities": manifest["capabilities"],
                    "variant": manifest["variant"],
                })
            })
            .collect();
        let release = json!({ "format": "p1-release-manifest/1", "components": entries });
        Loader::new(
            ReleaseManifest::parse(&release.to_string()).expect("release manifest"),
            built_dir(),
        )
        .expect("loader")
    })
}

/// The run: one store, the shell and `read_output` over it, and a workspace.
struct Run {
    shell: Arc<dyn Tool>,
    read_output: Arc<dyn Tool>,
    workspace: tempfile::TempDir,
    _store: Arc<OutputStore>,
}

impl Run {
    fn new(store: OutputStore) -> Self {
        let workspace = tempfile::tempdir().expect("workspace");
        let store = Arc::new(store);
        let process = Arc::new(
            ProcessService::new(workspace.path())
                // HOME is the workspace: `bash -lc` is a login shell, and a real home's
                // profile would print into every result.
                .with_env_snapshot(vec![
                    ("PATH".into(), "/usr/bin:/bin".into()),
                    ("HOME".into(), workspace.path().into()),
                ]),
        );
        let counter = Arc::new(MaskCounter::new());
        // As the host's shell entry: each call's commands are teed into the store and the same
        // call's `tool-outputs.produced` names them.
        let shell_store = store.clone();
        let linked = Services::call_scoped(move || {
            let outputs = CallOutputs::new(shell_store.clone(), Default::default());
            Services {
                process: Some(Arc::new(
                    ProcessCapability::new(process.clone()).storing(outputs.clone()),
                )),
                tool_outputs: Some(Arc::new(outputs)),
                ..Services::default()
            }
        });
        let shell_module = loader().load(SHELL.1).expect("p1/shell loads");
        let shell = wasm_tool(&shell_module, linked, ExecutionLimits::default(), &counter)
            .expect("the shell links");
        let read_module = loader().load(READ_OUTPUT.1).expect("p1/read-output loads");
        let read_output = wasm_tool(
            &read_module,
            Services {
                tool_outputs: Some(Arc::new(CallOutputs::new(
                    store.clone(),
                    Default::default(),
                ))),
                ..Services::default()
            },
            ExecutionLimits::default(),
            &counter,
        )
        .expect("read_output links");
        Self {
            shell,
            read_output,
            workspace,
            _store: store,
        }
    }

    async fn shell(&self, command: &str, raw: bool) -> ToolOutcome {
        let input = json!({ "command": command, "raw": raw }).to_string();
        execute(&*self.shell, "shell", input).await
    }

    /// Pages the output under `handle` to its end and returns the text.
    async fn page_all(&self, handle: &str) -> String {
        let mut offset = 0_u64;
        let mut text = String::new();
        loop {
            let input = json!({ "handle_id": handle, "offset": offset }).to_string();
            let outcome = execute(&*self.read_output, "read_output", input).await;
            assert_eq!(
                outcome.status,
                p1_contracts::ToolStatus::Ok,
                "{}",
                outcome.content
            );
            let (page, footer) = outcome
                .content
                .rsplit_once('\n')
                .unwrap_or(("", outcome.content.as_str()));
            text.push_str(page);
            offset += page.len() as u64;
            if footer.ends_with("; end]") {
                return text;
            }
        }
    }

    /// A program on the workspace `PATH` that prints `output` and exits `code`.
    fn program(&self, name: &str, output: &str, code: i32) {
        let bin = self.workspace.path().join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let data = bin.join(format!("{name}.out"));
        std::fs::write(&data, output).unwrap();
        let path = bin.join(name);
        std::fs::write(
            &path,
            format!("#!/bin/sh\ncat '{}'\nexit {code}\n", data.display()),
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
}

async fn execute(tool: &dyn Tool, name: &str, input: String) -> ToolOutcome {
    let call = ToolCall {
        call_id: "c1".into(),
        name: name.into(),
        input: ToolInput::Json(input),
    };
    tool.execute(
        &call,
        ToolContext {
            cancel: CancellationToken::new(),
        },
    )
    .await
}

/// The stored-output line of a result: the line just above its end footer.
fn notice(content: &str) -> &str {
    let mut lines = content.lines().rev();
    lines.next();
    lines.next().expect("a line above the footer")
}

/// The handle the stored-output line names.
fn handle(notice: &str) -> &str {
    notice
        .strip_prefix("[stored output: handle_id ")
        .and_then(|rest| rest.split_once(','))
        .map(|(handle, _)| handle)
        .unwrap_or_else(|| panic!("no handle in {notice:?}"))
}

/// A command whose output the host and the byte bound cut: filtered or raw, the result stays
/// inside the envelope with its exit footer last, names the stored output on the line above,
/// and `read_output` pages back exactly what the command printed.
#[tokio::test(flavor = "multi_thread")]
async fn a_cut_result_names_the_handle_and_read_output_recovers_every_byte() {
    within_deadline(
        "a_cut_result_names_the_handle_and_read_output_recovers_every_byte",
        async {
            let run = Run::new(OutputStore::temporary(OutputCaps::DEFAULT));
            let printed: String = (0..20_000)
                .map(|n| format!("line {n}: é€😀 diagnostic detail\n"))
                .collect();
            std::fs::write(run.workspace.path().join("log"), &printed).unwrap();
            let mut handles = Vec::new();
            for raw in [false, true] {
                let outcome = run.shell("cat log; exit 3", raw).await;
                assert!(outcome.content.len() <= ENVELOPE, "{}", outcome.content.len());
                assert!(outcome.content.ends_with("\n[exit code: 3]"), "raw={raw}");
                let line = notice(&outcome.content);
                assert_eq!(
                    line,
                    format!(
                        "[stored output: handle_id {}, {} bytes, complete; page it with read_output]",
                        handle(line),
                        printed.len()
                    ),
                    "raw={raw}"
                );
                assert!(
                    !outcome.content.contains("line 10000:"),
                    "the middle was cut from the result"
                );
                let recovered = run.page_all(handle(line)).await;
                assert_eq!(recovered.as_bytes(), printed.as_bytes(), "raw={raw}");
                handles.push(handle(line).to_owned());
            }
            assert_ne!(handles[0], handles[1], "each run is its own stored output");
        },
    )
    .await;
}

/// #527 review: the host's head/tail cut is line-based, so 990 lines, `FAIL`, 990 lines loses
/// only `FAIL` while the omission marker adds more bytes than it dropped. The result must still
/// name the stored output, raw or not, and `read_output` must give `FAIL` back.
#[tokio::test(flavor = "multi_thread")]
async fn a_line_cut_smaller_than_its_marker_names_the_handle() {
    within_deadline("a_line_cut_smaller_than_its_marker_names_the_handle", async {
        let run = Run::new(OutputStore::temporary(OutputCaps::DEFAULT));
        let lines = "x\n".repeat(990);
        let printed = format!("{lines}FAIL\n{lines}");
        std::fs::write(run.workspace.path().join("log"), &printed).unwrap();
        for raw in [false, true] {
            let outcome = run.shell("cat log; exit 1", raw).await;
            assert!(outcome.content.ends_with("\n[exit code: 1]"), "raw={raw}");
            assert!(!outcome.content.contains("FAIL"), "the host cut FAIL, raw={raw}");
            assert!(
                outcome.content.contains("bytes omitted; diagnostics may be missing"),
                "raw={raw}"
            );
            let line = notice(&outcome.content);
            assert_eq!(
                line,
                format!(
                    "[stored output: handle_id {}, {} bytes, complete; page it with read_output]",
                    handle(line),
                    printed.len()
                ),
                "raw={raw}"
            );
            assert_eq!(run.page_all(handle(line)).await, printed, "raw={raw}");
        }
    })
    .await;
}

/// A summarised result names the stored output too; the same command shown whole (`raw`) has
/// nothing to recover and carries no line, nor does any small output.
#[tokio::test(flavor = "multi_thread")]
async fn a_filtered_result_names_the_handle_and_a_whole_one_does_not() {
    within_deadline(
        "a_filtered_result_names_the_handle_and_a_whole_one_does_not",
        async {
            let run = Run::new(OutputStore::temporary(OutputCaps::DEFAULT));
            let log = "   Compiling a v0.1.0 (/ws)\n".repeat(40)
                + "    Finished `dev` profile [unoptimized + debuginfo] target(s) in 1.00s\n";
            run.program("cargo", &log, 0);
            let command = format!(
                "PATH={}/bin:/usr/bin:/bin cargo build",
                run.workspace.path().display()
            );
            let filtered = run.shell(&command, false).await;
            assert!(
                filtered
                    .content
                    .contains("[output filtered; pass raw:true for the full log]"),
                "{}",
                filtered.content
            );
            let line = notice(&filtered.content);
            assert!(
                line.ends_with("bytes, complete; page it with read_output]"),
                "{line}"
            );
            assert_eq!(run.page_all(handle(line)).await, log);

            let raw = run.shell(&command, true).await;
            assert!(!raw.content.contains("[stored output:"), "{}", raw.content);
            let small = run.shell("echo hi", false).await;
            assert_eq!(small.content, "hi\n[exit code: 0]");
        },
    )
    .await;
}

/// The store stopped at its cap: the line says so, and what it holds is the exact prefix.
#[tokio::test(flavor = "multi_thread")]
async fn a_capped_store_is_named_with_its_state() {
    within_deadline("a_capped_store_is_named_with_its_state", async {
        let caps = OutputCaps {
            per_output: 8_192,
            per_session: OutputCaps::DEFAULT.per_session,
        };
        let run = Run::new(OutputStore::temporary(caps));
        let printed = "0123 4567 89\n".repeat(16_000);
        std::fs::write(run.workspace.path().join("log"), &printed).unwrap();
        for raw in [false, true] {
            let outcome = run.shell("cat log", raw).await;
            assert!(outcome.content.len() <= ENVELOPE);
            assert!(outcome.content.ends_with("\n[exit code: 0]"));
            let line = notice(&outcome.content);
            assert!(
                line.ends_with(
                    "bytes, stopped at the store's cap, later output not stored; page it with read_output]"
                ),
                "{line}"
            );
            let recovered = run.page_all(handle(line)).await;
            assert!(!recovered.is_empty() && recovered.len() <= 8_192);
            assert!(printed.starts_with(&recovered), "an exact prefix");
        }
    })
    .await;
}

/// The store could not write: the result says the output was not stored and shows no handle,
/// filtered or raw.
#[tokio::test(flavor = "multi_thread")]
async fn a_failed_store_shows_no_handle() {
    within_deadline("a_failed_store_shows_no_handle", async {
        let blocker = tempfile::NamedTempFile::new().unwrap();
        // The session root is a regular file, so the run directory can never be created.
        let run = Run::new(OutputStore::in_directory(
            blocker.path(),
            OutputCaps::DEFAULT,
        ));
        let printed = "x".repeat(200_000);
        std::fs::write(run.workspace.path().join("log"), &printed).unwrap();
        for raw in [false, true] {
            let outcome = run.shell("cat log", raw).await;
            assert!(
                outcome
                    .content
                    .ends_with("\n[full output not stored; recovery unavailable]\n[exit code: 0]"),
                "raw={raw}: {}",
                &outcome.content[outcome.content.len().saturating_sub(200)..]
            );
            assert!(!outcome.content.contains("handle_id"));
        }
    })
    .await;
}
