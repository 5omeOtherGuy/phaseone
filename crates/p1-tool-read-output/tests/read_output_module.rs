//! #511: the `read_output` tool pages the host's output store (ADR-0109) through the native
//! adapter AND the `p1/read-output` component, built by `scripts/build-modules.sh`, loaded by
//! name through the production loader and linked to the same store view. It is the first guest
//! caller of `tool-outputs`, so every case here runs through the real component and requires
//! its outcome — status, content and both descriptions — to equal the native tool's byte for
//! byte.
//!
//! The store is filled the way the host fills it: a real command run through the `process`
//! capability that tees into the call's outputs (`ProcessCapability::storing`).

use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use p1_contracts::serde_json::{self, Value, json};
use p1_contracts::{
    CancellationToken, Tool, ToolCall, ToolContext, ToolInput, ToolOutcome, ToolResultItem,
    ToolStatus,
};
use p1_module_runtime::capabilities::Services;
use p1_module_runtime::process::{ProcessCapability, ProcessService};
use p1_module_runtime::{
    CallOutputs, ExecutionLimits, Loader, OutputCaps, OutputStore, ProcessCommand, ProcessEvent,
    ReleaseManifest, ToolOutputsService, wasm_tool,
};
use p1_redact::MaskCounter;
use p1_tool_read_output::{ReadOutputTool, page_text};

/// The package directory and the manifest name of the component.
const PACKAGE: &str = "p1-module-read-output";
const NAME: &str = "p1/read-output";

/// A hang guard only: nothing asserts on how long a case takes.
const DEADLOCK_LIMIT: Duration = Duration::from_secs(300);

async fn within_deadline<F: Future>(case: &str, body: F) -> F::Output {
    match tokio::time::timeout(DEADLOCK_LIMIT, body).await {
        Ok(output) => output,
        Err(_) => panic!("deadlock: {case} did not finish within {DEADLOCK_LIMIT:?}"),
    }
}

/// A release directory holding only the component, laid out as p1's release archive ships it.
struct Release {
    dir: tempfile::TempDir,
}

impl Release {
    fn manifest_file(&self) -> PathBuf {
        self.dir.path().join("manifest.json")
    }

    fn loader(&self) -> Loader {
        let manifest = ReleaseManifest::read(&self.manifest_file()).expect("release manifest");
        Loader::new(manifest, self.dir.path()).expect("loader")
    }
}

/// The component as `scripts/build-modules.sh` published it, in a release of its own.
fn release() -> Release {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../modules/target/p1-modules")
        .join(PACKAGE);
    let missing = |path: &Path, error: std::io::Error| -> ! {
        panic!(
            "the {PACKAGE} artifact {} is missing ({error}): run scripts/build-modules.sh first",
            path.display()
        )
    };
    let wasm_path = dir.join(format!("{PACKAGE}.wasm"));
    let manifest_path = dir.join(format!("{PACKAGE}.manifest.json"));
    let wasm = std::fs::read(&wasm_path).unwrap_or_else(|error| missing(&wasm_path, error));
    let manifest: Value = serde_json::from_str(
        &std::fs::read_to_string(&manifest_path)
            .unwrap_or_else(|error| missing(&manifest_path, error)),
    )
    .expect("the package manifest is JSON");
    assert_eq!(
        manifest["capabilities"],
        json!(["tool-outputs"]),
        "read_output is granted the store and nothing else"
    );
    let entry = json!({
        "name": manifest["name"],
        "digest": manifest["digest"],
        "path": format!("packages/{PACKAGE}/{PACKAGE}.wasm"),
        "kind": manifest["kind"],
        "world": manifest["world"],
        "protocol": manifest["protocol"],
        "capabilities": manifest["capabilities"],
        "variant": manifest["variant"],
    });
    let release = Release {
        dir: tempfile::tempdir().expect("release dir"),
    };
    let component = release
        .dir
        .path()
        .join(entry["path"].as_str().expect("entry path"));
    std::fs::create_dir_all(component.parent().expect("package dir")).expect("package dir");
    std::fs::write(&component, &wasm).expect("component file");
    let listing = json!({
        "format": "p1-release-manifest/1",
        "components": [entry],
    });
    std::fs::write(release.manifest_file(), listing.to_string()).expect("manifest");
    release
}

/// One run's store with one stored output, and both tools over it.
struct Pair {
    native: ReadOutputTool,
    module: Arc<dyn Tool>,
    handle: String,
    /// What the command printed: what the store must give back, byte for byte.
    printed: Vec<u8>,
    _release: Release,
    _workspace: tempfile::TempDir,
    _store: Arc<OutputStore>,
}

/// Stores what `cat` of a file holding `printed` prints, through the host's process capability,
/// and builds both tools over the store.
async fn stored(printed: &[u8], caps: OutputCaps) -> Pair {
    let workspace = tempfile::tempdir().expect("workspace");
    std::fs::write(workspace.path().join("printed"), printed).expect("fixture");
    let store = Arc::new(OutputStore::temporary(caps));
    let outputs = CallOutputs::new(store.clone(), Default::default());
    let process = ProcessCapability::new(Arc::new(
        ProcessService::new(workspace.path())
            // HOME is the workspace: `bash -lc` is a login shell, and a real home's profile
            // would print into the stored output.
            .with_env_snapshot(vec![
                ("PATH".into(), "/usr/bin:/bin".into()),
                ("HOME".into(), workspace.path().into()),
            ]),
    ))
    .storing(outputs.clone());
    let mut running = p1_module_runtime::ProcessService::spawn(
        &process,
        ProcessCommand {
            script: "cat printed".into(),
            timeout_ms: 60_000,
        },
        CancellationToken::new(),
    )
    .await
    .expect("the command starts");
    loop {
        match running.next().await {
            Some(ProcessEvent::Exited(_)) | None => break,
            Some(ProcessEvent::Output(_)) => {}
        }
    }
    drop(running);
    let produced = tokio::task::spawn_blocking({
        let outputs = outputs.clone();
        move || outputs.produced()
    })
    .await
    .expect("produced");
    assert_eq!(produced.len(), 1, "{produced:?}");
    let handle = produced[0].handle.clone();

    let release = release();
    let loaded = release.loader().load(NAME).expect("p1/read-output loads");
    assert_eq!(loaded.identity().implementation, NAME);
    // The host links the component to a store view that produced nothing itself: it reads.
    let view: Arc<dyn ToolOutputsService> =
        Arc::new(CallOutputs::new(store.clone(), Default::default()));
    let module = wasm_tool(
        &loaded,
        Services {
            tool_outputs: Some(view.clone()),
            ..Services::default()
        },
        ExecutionLimits::default(),
        &Arc::new(MaskCounter::new()),
    )
    .expect("the component is a tool");
    Pair {
        native: ReadOutputTool::new(view),
        module,
        handle,
        printed: printed.to_vec(),
        _release: release,
        _workspace: workspace,
        _store: store,
    }
}

fn call(arguments: &Value) -> ToolCall {
    ToolCall {
        call_id: "c1".into(),
        name: "read_output".into(),
        input: ToolInput::Json(arguments.to_string()),
    }
}

async fn execute(tool: &dyn Tool, call: &ToolCall) -> ToolOutcome {
    tool.execute(
        call,
        ToolContext {
            cancel: CancellationToken::new(),
        },
    )
    .await
}

impl Pair {
    /// Runs `arguments` through both tools and requires the same outcome and the same
    /// descriptions of the call and its result.
    async fn same(&self, arguments: Value) -> ToolOutcome {
        let call = call(&arguments);
        let native = execute(&self.native, &call).await;
        let module = execute(self.module.as_ref(), &call).await;
        assert_eq!(module.status, native.status, "status of {arguments}");
        assert_eq!(
            module.content.as_bytes(),
            native.content.as_bytes(),
            "content of {arguments}"
        );
        assert_eq!(
            self.module.describe(&call),
            self.native.describe(&call),
            "describe {arguments}"
        );
        let result = ToolResultItem {
            call_id: call.call_id.clone(),
            name: call.name.clone(),
            status: native.status,
            content: native.content.clone(),
        };
        assert_eq!(
            self.module.describe_result(&call, &result),
            self.native.describe_result(&call, &result),
            "describe_result of {arguments}"
        );
        native
    }

    /// Pages the whole output with `limit` through both tools, checks every footer and returns
    /// the concatenated page texts.
    async fn page_all(&self, limit: u32) -> Vec<u8> {
        let mut offset = 0_u64;
        let mut text = Vec::new();
        loop {
            let outcome = self
                .same(json!({"handle_id": self.handle, "offset": offset, "limit": limit}))
                .await;
            assert_eq!(outcome.status, ToolStatus::Ok, "{}", outcome.content);
            let page = page_text(&outcome.content);
            let footer = outcome.content.lines().last().expect("a footer");
            text.extend_from_slice(page.as_bytes());
            let next = offset + page.len() as u64;
            assert!(
                page.len() <= limit as usize,
                "a page holds at most `limit` bytes"
            );
            assert!(
                footer.starts_with(&format!(
                    "[read_output: bytes {offset}-{next} of {} stored; capture complete; ",
                    self.printed.len()
                )),
                "{footer}"
            );
            if footer.ends_with("; end]") {
                assert_eq!(next, self.printed.len() as u64);
                return text;
            }
            assert!(
                footer.ends_with(&format!("; next_offset {next}]")),
                "{footer}"
            );
            assert!(next > offset, "a page always makes progress");
            offset = next;
        }
    }
}

/// A single 3 MB line (3,150,000 bytes, no line break) pages in 50,000-byte pages, and the pages put together are the stored
/// text byte for byte.
#[tokio::test(flavor = "multi_thread")]
async fn a_single_three_megabyte_line_pages_back_byte_for_byte() {
    within_deadline(
        "a_single_three_megabyte_line_pages_back_byte_for_byte",
        async {
            // Words, not one unbroken run: the store masks a credential-shaped run it would
            // have to cut (ADR-0109 item 2), so a 3 MB `[A-Za-z0-9]` run is stored masked.
            let line: Vec<u8> = b"lorem ipsum dolor sit amet, ".repeat(112_500);
            let pair = stored(&line, OutputCaps::DEFAULT).await;
            assert_eq!(pair.page_all(50_000).await, pair.printed);
            // The default limit is the maximum one.
            let first = pair.same(json!({"handle_id": pair.handle})).await;
            assert_eq!(page_text(&first.content).len(), 50_000);
            assert!(
                first.content.ends_with("; next_offset 50000]"),
                "{}",
                first.content
            );
        },
    )
    .await;
}

/// Characters of two, three and four bytes fall on every page boundary: each limit below cuts
/// the text at every position inside some character, and no page splits one.
#[tokio::test(flavor = "multi_thread")]
async fn multibyte_characters_at_page_boundaries_are_never_split() {
    within_deadline(
        "multibyte_characters_at_page_boundaries_are_never_split",
        async {
            let text = "é€😀a\n".repeat(40);
            let pair = stored(text.as_bytes(), OutputCaps::DEFAULT).await;
            for limit in [4, 5, 6, 7, 9, 11, 13, 64] {
                let paged = pair.page_all(limit).await;
                assert_eq!(paged, pair.printed, "limit {limit}");
            }
        },
    )
    .await;
}

/// A limit smaller than the next character is a precise error, never an empty page with the
/// same cursor; an offset inside a character, past the end or under an unknown handle each have
/// their own text.
#[tokio::test(flavor = "multi_thread")]
async fn every_store_error_is_a_precise_message() {
    within_deadline("every_store_error_is_a_precise_message", async {
        let pair = stored("😀é".as_bytes(), OutputCaps::DEFAULT).await;
        let handle = pair.handle.clone();

        let small = pair
            .same(json!({"handle_id": handle, "offset": 0, "limit": 3}))
            .await;
        assert_eq!(small.status, ToolStatus::Error);
        assert_eq!(
            small.content,
            "read_output: `limit` 3 is smaller than the character at offset 0; ask again with a limit of at least 4."
        );

        let inside = pair.same(json!({"handle_id": handle, "offset": 2})).await;
        assert_eq!(inside.status, ToolStatus::Error);
        assert_eq!(
            inside.content,
            format!(
                "read_output: offset 2 lies inside a multi-byte character of `{handle}`; use 0 or a `next_offset` an earlier page gave."
            )
        );

        let past = pair.same(json!({"handle_id": handle, "offset": 7})).await;
        assert_eq!(past.status, ToolStatus::Error);
        assert_eq!(
            past.content,
            format!(
                "read_output: offset 7 is past the end of `{handle}`, which holds 6 bytes; offsets run from 0 to 6."
            )
        );

        let at_end = pair.same(json!({"handle_id": handle, "offset": 6})).await;
        assert_eq!(at_end.status, ToolStatus::Ok);
        assert_eq!(
            at_end.content,
            "[read_output: bytes 6-6 of 6 stored; capture complete; end]"
        );

        for unknown in ["out-00000000000000000000000000000000", "../printed", "x"] {
            let outcome = pair.same(json!({"handle_id": unknown})).await;
            assert_eq!(outcome.status, ToolStatus::Error);
            assert!(
                outcome
                    .content
                    .starts_with(&format!("read_output: no stored output has the handle `{unknown}`.")),
                "{}",
                outcome.content
            );
        }

        for invalid in [
            json!({"handle_id": ""}),
            json!({"handle_id": handle, "limit": 0}),
            json!({"handle_id": handle, "limit": 50_001}),
            json!({"handle_id": handle, "offset": -1}),
            json!({"handle_id": handle, "unknown": true}),
            json!({}),
        ] {
            let outcome = pair.same(invalid.clone()).await;
            assert_eq!(outcome.status, ToolStatus::Error, "{invalid}");
            assert!(
                outcome.content.starts_with("Invalid input for read_output: "),
                "{invalid}: {}",
                outcome.content
            );
        }
    })
    .await;
}

/// An output the store stopped at its cap pages what it holds and says so in every footer.
#[tokio::test(flavor = "multi_thread")]
async fn a_capped_output_pages_what_it_holds_and_says_so() {
    within_deadline("a_capped_output_pages_what_it_holds_and_says_so", async {
        let text = "0123 4567 89\n".repeat(800);
        let caps = OutputCaps {
            per_output: 4_096,
            per_session: OutputCaps::DEFAULT.per_session,
        };
        let pair = stored(text.as_bytes(), caps).await;
        let outcome = pair.same(json!({"handle_id": pair.handle})).await;
        assert_eq!(outcome.status, ToolStatus::Ok);
        let page = page_text(&outcome.content);
        assert!(text.starts_with(page), "what is stored is exact");
        assert!(page.len() <= 4_096);
        assert!(
            outcome.content.ends_with(&format!(
                "[read_output: bytes 0-{} of {} stored; capture stopped at the store's cap (the command printed more); end]",
                page.len(),
                page.len()
            )),
            "{}",
            outcome.content
        );
    })
    .await;
}

/// Reading an output runs no command: the tool is read-only and holds no host-observed exit, so
/// a page never stands in for a verification run (ADR-0109 item 8).
#[tokio::test(flavor = "multi_thread")]
async fn reading_an_output_is_never_a_command_run() {
    within_deadline("reading_an_output_is_never_a_command_run", async {
        let pair = stored(b"error: test failed\n", OutputCaps::DEFAULT).await;
        let call = call(&json!({"handle_id": pair.handle}));
        for tool in [&pair.native as &dyn Tool, pair.module.as_ref()] {
            assert_eq!(tool.effect(&call), p1_contracts::Effect::ReadOnly);
            let outcome = execute(tool, &call).await;
            assert_eq!(outcome.status, ToolStatus::Ok);
            assert_eq!(tool.take_command_exit_code(&call.call_id), None);
            assert_eq!(tool.command_exit_code(&call.call_id), None);
        }
    })
    .await;
}
