//! #525 (ADR-0109 item 5): ordinary fast output is stored whole. The #511 measurement case 10,
//! a shell loop of 20,000 one-line `echo`s with the error at line 9,000, through the shipped
//! `p1/shell` component over a session store: the result names a `complete` stored output and
//! the shipped `p1/read-output` component pages the error line back.
//!
//! The components are loaded and linked as `shell_stored_output.rs` links them (the host's
//! `crates/p1-host/src/catalog/tools.rs` and `modules.rs`); this file holds only what the case
//! needs.

use std::path::{Path, PathBuf};
use std::sync::Arc;

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

fn built_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../modules/target/p1-modules")
}

/// One loader over the two built packages, laid out as a release ships them.
fn loader() -> Loader {
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
}

async fn execute(tool: &dyn Tool, name: &str, input: Value) -> ToolOutcome {
    let call = ToolCall {
        call_id: "c1".into(),
        name: name.into(),
        input: ToolInput::Json(input.to_string()),
    };
    tool.execute(
        &call,
        ToolContext {
            cancel: CancellationToken::new(),
        },
    )
    .await
}

#[tokio::test(flavor = "multi_thread")]
async fn twenty_thousand_echos_are_stored_complete_and_read_output_finds_the_error() {
    within_deadline(
        "twenty_thousand_echos_are_stored_complete_and_read_output_finds_the_error",
        async {
            let workspace = tempfile::tempdir().unwrap();
            let session = tempfile::tempdir().unwrap();
            let store = Arc::new(OutputStore::in_directory(
                session.path().join("session.jsonl.outputs"),
                OutputCaps::DEFAULT,
            ));
            let process = Arc::new(
                ProcessService::new(workspace.path())
                    // HOME is the workspace: `bash -lc` is a login shell, and a real home's
                    // profile would print into the result.
                    .with_env_snapshot(vec![
                        ("PATH".into(), "/usr/bin:/bin".into()),
                        ("HOME".into(), workspace.path().into()),
                    ]),
            );
            let counter = Arc::new(MaskCounter::new());
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
            let loader = loader();
            let shell = wasm_tool(
                &loader.load(SHELL.1).expect("p1/shell loads"),
                linked,
                ExecutionLimits::default(),
                &counter,
            )
            .expect("the shell links");
            let read_output = wasm_tool(
                &loader.load(READ_OUTPUT.1).expect("p1/read-output loads"),
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

            // Each `echo` is its own write, so the store receives 20,000 small chunks.
            let command = "for i in $(seq 1 20000); do \
                 if [ $i -eq 9000 ]; then echo \"error: step $i failed\"; \
                 else echo \"step $i ok\"; fi; done";
            let printed: String = (1..=20_000)
                .map(|n| match n {
                    9_000 => format!("error: step {n} failed\n"),
                    _ => format!("step {n} ok\n"),
                })
                .collect();
            let outcome = execute(&*shell, "shell", json!({ "command": command })).await;
            assert!(
                outcome.content.ends_with("\n[exit code: 0]"),
                "{}",
                outcome.content
            );
            assert!(
                !outcome.content.contains("error: step 9000 failed"),
                "the result cut the error line"
            );
            let notice = outcome
                .content
                .lines()
                .rev()
                .nth(1)
                .expect("a line above the footer");
            let handle = notice
                .strip_prefix("[stored output: handle_id ")
                .and_then(|rest| rest.split_once(','))
                .map(|(handle, _)| handle)
                .unwrap_or_else(|| panic!("no handle in {notice:?}"));
            assert_eq!(
                notice,
                format!(
                    "[stored output: handle_id {handle}, {} bytes, complete; page it with read_output]",
                    printed.len()
                )
            );

            // Page to the end, as a model with no offset hint would.
            let mut offset = 0_u64;
            let mut recovered = String::new();
            loop {
                let input = json!({ "handle_id": handle, "offset": offset });
                let page = execute(&*read_output, "read_output", input).await;
                assert_eq!(page.status, p1_contracts::ToolStatus::Ok, "{}", page.content);
                let (text, footer) = page
                    .content
                    .rsplit_once('\n')
                    .unwrap_or(("", page.content.as_str()));
                recovered.push_str(text);
                offset += text.len() as u64;
                if footer.ends_with("; end]") {
                    break;
                }
            }
            assert!(recovered.contains("\nerror: step 9000 failed\n"));
            assert_eq!(recovered, printed);
        },
    )
    .await;
}
