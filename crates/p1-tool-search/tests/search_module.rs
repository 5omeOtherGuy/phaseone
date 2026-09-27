//! U-search.3: the `p1/search` component, built by `scripts/build-modules.sh --package
//! p1-module-search`, loaded by name through the production loader over the built artifact
//! and run over this crate's host service (`search_services`: `workspace` with `list-files`
//! and `search` over the native serial walk, `read` without observation). Every search runs
//! the native `GrepTool` and the component over the same scratch workspace and requires the
//! same outcome, byte for byte, and a search leaves the files it read never observed.

use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use p1_contracts::serde_json::{self, Value, json};
use p1_contracts::{CancellationToken, Tool, ToolCall, ToolContext, ToolInput, ToolOutcome};
use p1_contracts::{ToolResultItem, ToolStatus};
use p1_module_runtime::{ExecutionLimits, Loader, ReleaseManifest, wasm_tool};
use p1_redact::MaskCounter;
use p1_tool_search::{GrepTool, search_services};
use p1_workspace::{Change, MutationPolicy, Observation, ObservedFiles, Workspace};

/// The package directory and the manifest name of the component.
const PACKAGE: &str = "p1-module-search";
const NAME: &str = "p1/search";

/// A hang guard only: every case finishes in well under a second, and a deadlock must fail
/// the case instead of the whole test run. Nothing asserts on how long a case takes.
const DEADLOCK_LIMIT: Duration = Duration::from_secs(120);

async fn within_deadline<F: Future>(case: &str, body: F) -> F::Output {
    match tokio::time::timeout(DEADLOCK_LIMIT, body).await {
        Ok(output) => output,
        Err(_) => panic!("deadlock: {case} did not finish within {DEADLOCK_LIMIT:?}"),
    }
}

/// A release directory holding only the component, laid out as p1's release archive ships
/// it, so these cases need no other package built.
struct SearchRelease {
    dir: tempfile::TempDir,
}

impl SearchRelease {
    fn manifest_file(&self) -> PathBuf {
        self.dir.path().join("manifest.json")
    }

    fn loader(&self) -> Loader {
        let manifest = ReleaseManifest::read(&self.manifest_file()).expect("release manifest");
        Loader::new(manifest, self.dir.path()).expect("loader")
    }
}

/// The component as `scripts/build-modules.sh` published it, in a release of its own.
fn search_release() -> SearchRelease {
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
    let release = SearchRelease {
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

/// The native tool and the component over one workspace.
struct Pair {
    native: GrepTool,
    module: Arc<dyn Tool>,
    _release: SearchRelease,
}

fn both_tools(workspace: &Workspace) -> Pair {
    let release = search_release();
    let loaded = release
        .loader()
        .load(NAME)
        .expect("p1/search loads by name");
    assert_eq!(loaded.identity().implementation, NAME);
    let module = wasm_tool(
        &loaded,
        search_services(workspace.clone()),
        ExecutionLimits::default(),
        &Arc::new(MaskCounter::new()),
    )
    .expect("the component is a tool");
    Pair {
        native: GrepTool::new(workspace.clone()),
        module,
        _release: release,
    }
}

fn call(arguments: &str) -> ToolCall {
    ToolCall {
        call_id: "c1".into(),
        name: "grep".into(),
        input: ToolInput::Json(arguments.to_owned()),
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
    async fn same(&self, arguments: &str) -> ToolOutcome {
        let call = call(arguments);
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
}

/// An ignored `target/`, a hidden directory, a binary file and source files matching `beta`.
fn tree(root: &Path) {
    for dir in ["src", "target", ".hidden", "docs"] {
        std::fs::create_dir_all(root.join(dir)).unwrap();
    }
    std::fs::write(root.join(".gitignore"), "target/\n").unwrap();
    std::fs::write(root.join("src/a.rs"), "fn alpha() {}\nfn beta() {}\n").unwrap();
    std::fs::write(root.join("src/b.rs"), "fn beta() {}\nfn gamma() {}\n").unwrap();
    std::fs::write(root.join("target/x.rs"), "fn beta() {}\n").unwrap();
    std::fs::write(root.join(".hidden/c.rs"), "fn beta() {}\n").unwrap();
    std::fs::write(root.join("docs/notes.md"), "beta notes\n").unwrap();
    std::fs::write(root.join("docs/blob.md"), b"beta\0binary\n").unwrap();
}

// `WasmTool` starts its executor on the current Tokio runtime and the host service's walk
// runs on the blocking pool: every case runs inside a multi-threaded runtime.

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn content_and_files_searches_match_the_native_tool_byte_for_byte() {
    within_deadline("searches", async {
        let dir = tempfile::tempdir().unwrap();
        tree(dir.path());
        let workspace = Workspace::new(dir.path()).unwrap();
        let pair = both_tools(&workspace);
        assert_eq!(pair.module.declaration(), pair.native.declaration());

        // Content: `workspace.search` with context lines.
        let content = pair.same(r#"{"pattern": "beta", "context": 1}"#).await;
        assert_eq!(content.status, ToolStatus::Ok);
        assert_eq!(
            content.content,
            "docs/notes.md\n1:beta notes\n\nsrc/a.rs\n1-fn alpha() {}\n2:fn beta() {}\n\n\
             src/b.rs\n1:fn beta() {}\n2-fn gamma() {}"
        );

        // Files, with a pattern: `workspace.search`, one path per matching file.
        let files = pair
            .same(r#"{"pattern": "beta", "mode": "files", "glob": "*.rs"}"#)
            .await;
        assert_eq!(files.status, ToolStatus::Ok);
        assert_eq!(files.content, "src/a.rs\nsrc/b.rs");

        // Files, without a pattern: `workspace.list-files`, then `read` sniffs out the
        // binary file.
        let listed = pair
            .same(r#"{"pattern": "", "mode": "files", "glob": "*.md"}"#)
            .await;
        assert_eq!(listed.status, ToolStatus::Ok);
        assert_eq!(listed.content, "docs/notes.md");

        // The host's failures reach the model as the native tool words them.
        for failing in [
            r#"{"pattern": "("}"#,
            r#"{"pattern": "beta", "path": "nope"}"#,
            r#"{"pattern": "beta", "path": "../"}"#,
        ] {
            assert_eq!(pair.same(failing).await.status, ToolStatus::Error);
        }
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_search_leaves_the_files_it_read_never_observed() {
    within_deadline("observation", async {
        let dir = tempfile::tempdir().unwrap();
        tree(dir.path());
        let workspace = Workspace::new(dir.path()).unwrap();
        // The agent's own registry, as its edit tool would hold it.
        let observed = ObservedFiles::new();
        let pair = both_tools(&workspace);

        pair.same(r#"{"pattern": "beta", "path": "src/a.rs"}"#)
            .await;
        pair.same(r#"{"pattern": "", "mode": "files", "glob": "*.md"}"#)
            .await;

        let searched = dir.path().join("src/a.rs");
        let listed = dir.path().join("docs/notes.md");
        assert_eq!(
            observed.check_unchanged(&searched, b"fn alpha() {}\nfn beta() {}\n"),
            Observation::NeverObserved
        );
        assert_eq!(
            observed.check_unchanged(&listed, b"beta notes\n"),
            Observation::NeverObserved
        );
        // So the search gave no permission to change what it read.
        assert!(
            workspace
                .commit(
                    &[Change::write("src/a.rs", "changed")],
                    &observed,
                    MutationPolicy::Observed
                )
                .is_err()
        );
        assert_eq!(
            std::fs::read_to_string(&searched).unwrap(),
            "fn alpha() {}\nfn beta() {}\n"
        );
    })
    .await;
}
