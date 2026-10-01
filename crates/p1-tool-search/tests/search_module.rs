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

use p1_contracts::BoxFuture;
use p1_contracts::serde_json::{self, Value, json};
use p1_contracts::{CancellationToken, Tool, ToolCall, ToolContext, ToolInput, ToolOutcome};
use p1_contracts::{ToolResultItem, ToolStatus};
use p1_module_runtime::capabilities::{FsError, Services, WorkspaceEntry, WorkspaceService};
use p1_module_runtime::file_services::{ReadCapability, mutation_service_over, tool_services};
use p1_module_runtime::{ExecutionLimits, Loader, ReleaseManifest, wasm_tool};
use p1_redact::MaskCounter;
use p1_tool_search::{GrepTool, search_services};
use p1_workspace::ReadRecord;
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
    module_release(PACKAGE)
}

fn module_release(package: &str) -> SearchRelease {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../modules/target/p1-modules")
        .join(package);
    let missing = |path: &Path, error: std::io::Error| -> ! {
        panic!(
            "the {package} artifact {} is missing ({error}): run scripts/build-modules.sh first",
            path.display()
        )
    };
    let wasm_path = dir.join(format!("{package}.wasm"));
    let manifest_path = dir.join(format!("{package}.manifest.json"));
    let wasm = std::fs::read(&wasm_path).unwrap_or_else(|error| missing(&wasm_path, error));
    let manifest: Value = serde_json::from_str(
        &std::fs::read_to_string(&manifest_path)
            .unwrap_or_else(|error| missing(&manifest_path, error)),
    )
    .expect("the package manifest is JSON");
    let entry = json!({
        "name": manifest["name"],
        "digest": manifest["digest"],
        "path": format!("packages/{package}/{package}.wasm"),
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
        search_services(
            workspace.clone(),
            workspace.credential_home().map(Path::to_path_buf),
        ),
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

/// #509 items 1 and 2: a literal pattern, and pages cut by the byte bound (with and without
/// leading context), give the component and the native tool the same outcome.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn literal_and_byte_cut_pages_match_the_native_tool() {
    within_deadline("literal and paging", async {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join("call.txt"), "call a.b(x)\naxb(y)\na.b\n").unwrap();
        let long = "x".repeat(12_482);
        std::fs::write(
            root.join("context.txt"),
            format!("{long}\n{long}\n{long}\n{long}\nbeta one\nbeta two\n"),
        )
        .unwrap();
        for file in 0..3 {
            let text: String = (0..20)
                .map(|line| format!("beta{}\n", "y".repeat(900 + 37 * ((file * 20 + line) % 11))))
                .collect();
            std::fs::write(root.join(format!("wide{file}.txt")), text).unwrap();
        }
        let workspace = Workspace::new(root).unwrap();
        let pair = both_tools(&workspace);

        let literal = pair
            .same(r#"{"pattern": "a.b(", "literal": true}"#)
            .await;
        assert_eq!(literal.content, "call.txt\n1:call a.b(x)");
        pair.same(r#"{"pattern": "a.b(", "literal": true, "mode": "count"}"#)
            .await;
        assert_eq!(
            pair.same(r#"{"pattern": "a.b("}"#).await.status,
            ToolStatus::Error
        );

        for (arguments, head_limit) in [
            (r#""glob": "context.txt", "context": 4"#, 1),
            (r#""glob": "wide*.txt""#, 7),
            (r#""glob": "wide*.txt""#, 60),
        ] {
            let mut offset = 0;
            for _ in 0..=60 {
                let page = pair
                    .same(&format!(
                        r#"{{"pattern": "beta", {arguments}, "offset": {offset}, "head_limit": {head_limit}}}"#
                    ))
                    .await;
                assert_eq!(page.status, ToolStatus::Ok);
                let Some(next) = page
                    .content
                    .rsplit_once("continue with offset=")
                    .map(|(_, next)| next.trim_end_matches(']').parse::<usize>().unwrap())
                else {
                    break;
                };
                assert!(next > offset, "{arguments}: page {offset} does not advance");
                offset = next;
            }
        }
    })
    .await;
}

/// Seven `hit` lines in four matching files over three directories, and one file without a
/// match: enough entries for `count`, `offset`, `head_limit` and `max_per_file` to cut.
fn paged_tree(root: &Path) {
    for dir in ["a", "b", "c"] {
        std::fs::create_dir_all(root.join(dir)).unwrap();
    }
    std::fs::write(root.join("a/one.txt"), "hit 1\nhit 2\nmiss\nhit 3\n").unwrap();
    std::fs::write(root.join("a/two.txt"), "hit\n").unwrap();
    std::fs::write(root.join("b/three.txt"), "hit\nhit\n").unwrap();
    std::fs::write(root.join("b/four.txt"), "miss\n").unwrap();
    std::fs::write(root.join("c/five.txt"), "hit\n").unwrap();
}

/// #519 M1: `mode: count`, whole and paged, past its end, and with no match.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn count_mode_matches_the_native_tool() {
    within_deadline("count", async {
        let dir = tempfile::tempdir().unwrap();
        paged_tree(dir.path());
        let pair = both_tools(&Workspace::new(dir.path()).unwrap());

        let whole = pair.same(r#"{"pattern": "hit", "mode": "count"}"#).await;
        assert_eq!(whole.status, ToolStatus::Ok);
        assert_eq!(
            whole.content,
            "a/one.txt:3\na/two.txt:1\nb/three.txt:2\nc/five.txt:1\n[total: 7 matches in 4 files]"
        );
        assert_eq!(
            pair.same(r#"{"pattern": "hit", "mode": "count", "offset": 1, "head_limit": 2}"#)
                .await
                .content,
            "a/two.txt:1\nb/three.txt:2\n[total: 7 matches in 4 files]\n\
             [showing files 2-3 of 4; continue with offset=3]"
        );
        assert_eq!(
            pair.same(r#"{"pattern": "hit", "mode": "count", "offset": 4}"#)
                .await
                .content,
            "[showing no files: offset 4 is past the last of 4]\n[total: 7 matches in 4 files]"
        );
        assert_eq!(
            pair.same(r#"{"pattern": "absent", "mode": "count"}"#)
                .await
                .content,
            "No matches."
        );
    })
    .await;
}

/// #519 M1: `offset` alone skips entries in every mode: match lines, paths and count lines.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn offset_matches_the_native_tool() {
    within_deadline("offset", async {
        let dir = tempfile::tempdir().unwrap();
        paged_tree(dir.path());
        let pair = both_tools(&Workspace::new(dir.path()).unwrap());

        let content = pair.same(r#"{"pattern": "hit", "offset": 2}"#).await;
        assert_eq!(content.status, ToolStatus::Ok);
        assert_eq!(
            content.content,
            "a/one.txt\n4:hit 3\n\na/two.txt\n1:hit\n\nb/three.txt\n1:hit\n2:hit\n\nc/five.txt\n1:hit"
        );
        assert_eq!(
            pair.same(r#"{"pattern": "hit", "mode": "files", "offset": 3}"#)
                .await
                .content,
            "c/five.txt"
        );
        assert_eq!(
            pair.same(r#"{"pattern": "", "mode": "files", "glob": "*.txt", "offset": 1}"#)
                .await
                .content,
            "a/two.txt\nb/four.txt\nb/three.txt\nc/five.txt"
        );
        assert_eq!(
            pair.same(r#"{"pattern": "hit", "offset": 7}"#)
                .await
                .content,
            "[showing no matches: offset 7 is past the last of 7]"
        );
        // `offset: 0` names no page: the result renders as an unpaged one.
        assert_eq!(
            pair.same(r#"{"pattern": "hit", "mode": "files", "offset": 0}"#)
                .await
                .content,
            "a/one.txt\na/two.txt\nb/three.txt\nc/five.txt"
        );
    })
    .await;
}

/// #519 M1: `head_limit` cuts a page and names the next `offset`; a cut files page ends with
/// the footer and then the directory summary.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn head_limit_matches_the_native_tool() {
    within_deadline("head_limit", async {
        let dir = tempfile::tempdir().unwrap();
        paged_tree(dir.path());
        let pair = both_tools(&Workspace::new(dir.path()).unwrap());

        let content = pair.same(r#"{"pattern": "hit", "head_limit": 2}"#).await;
        assert_eq!(content.status, ToolStatus::Ok);
        assert!(
            content
                .content
                .starts_with("a/one.txt\n1:hit 1\n2:hit 2\n[showing matches 1-2"),
            "{}",
            content.content
        );
        assert!(content.content.ends_with("continue with offset=2]"));
        assert_eq!(
            pair.same(r#"{"pattern": "hit", "mode": "files", "head_limit": 2}"#)
                .await
                .content,
            "a/one.txt\na/two.txt\n[showing files 1-2 of 4; continue with offset=2]\n\
             [4 matching files, 2 shown, 2 omitted; omitted by directory: b/ (1), c/ (1)]"
        );
        assert_eq!(
            pair.same(
                r#"{"pattern": "", "mode": "files", "glob": "*.txt", "offset": 1, "head_limit": 2}"#
            )
            .await
            .content,
            "a/two.txt\nb/four.txt\n[showing files 2-3 of 5; continue with offset=3]\n\
             [5 matching files, 2 shown, 3 omitted; omitted by directory: a/ (1), b/ (1), c/ (1)]"
        );
        // The last page leaves nothing after it: no footer, no summary.
        assert_eq!(
            pair.same(r#"{"pattern": "hit", "mode": "files", "offset": 2, "head_limit": 5}"#)
                .await
                .content,
            "b/three.txt\nc/five.txt"
        );
        // Following the named offsets pages through every match line once.
        let mut offset = 0;
        let mut shown = Vec::new();
        loop {
            let page = pair
                .same(&format!(
                    r#"{{"pattern": "hit", "offset": {offset}, "head_limit": 3}}"#
                ))
                .await;
            shown.extend(
                page.content
                    .lines()
                    .filter(|line| line.contains(":hit"))
                    .map(str::to_owned),
            );
            let Some((_, next)) = page.content.rsplit_once("continue with offset=") else {
                break;
            };
            offset = next.trim_end_matches(']').parse().unwrap();
        }
        assert_eq!(shown.len(), 7, "{shown:?}");
    })
    .await;
}

/// #519 M1: `max_per_file` shows each file's first N matches with their context and counts
/// the rest, alone and with a page.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn max_per_file_matches_the_native_tool() {
    within_deadline("max_per_file", async {
        let dir = tempfile::tempdir().unwrap();
        paged_tree(dir.path());
        let pair = both_tools(&Workspace::new(dir.path()).unwrap());

        let capped = pair.same(r#"{"pattern": "hit", "max_per_file": 1}"#).await;
        assert_eq!(capped.status, ToolStatus::Ok);
        assert_eq!(
            capped.content,
            "a/one.txt\n1:hit 1\n… 2 more matches in this file\n\na/two.txt\n1:hit\n\n\
             b/three.txt\n1:hit\n… 1 more match in this file\n\nc/five.txt\n1:hit"
        );
        assert_eq!(
            pair.same(r#"{"pattern": "hit", "max_per_file": 2, "context": 1, "glob": "a/*"}"#)
                .await
                .content,
            "a/one.txt\n1:hit 1\n2:hit 2\n3-miss\n… 1 more match in this file\n\na/two.txt\n1:hit"
        );
        pair.same(r#"{"pattern": "hit", "max_per_file": 1, "offset": 1, "head_limit": 2}"#)
            .await;
    })
    .await;
}

/// Real call-scoped read service, with one deterministic ungated write just after
/// the first imported read returns bytes; mutation must reject that old snapshot.
struct ReplacedAfterRead {
    read: Arc<ReadCapability>,
    file: PathBuf,
    retarget_to: Option<PathBuf>,
    fired: std::sync::atomic::AtomicBool,
}

impl WorkspaceService for ReplacedAfterRead {
    fn stat(&self, path: String) -> BoxFuture<'_, Result<WorkspaceEntry, FsError>> {
        WorkspaceService::stat(self.read.as_ref(), path)
    }

    fn read(
        &self,
        path: String,
        offset: u64,
        length: u64,
    ) -> BoxFuture<'_, Result<Vec<u8>, FsError>> {
        Box::pin(async move {
            let bytes = WorkspaceService::read(self.read.as_ref(), path, offset, length).await?;
            if offset == 0 && !self.fired.swap(true, std::sync::atomic::Ordering::SeqCst) {
                if let Some(target) = &self.retarget_to {
                    #[cfg(unix)]
                    {
                        std::fs::remove_file(&self.file)
                            .map_err(|error| FsError::Io(error.to_string()))?;
                        std::os::unix::fs::symlink(target, &self.file)
                            .map_err(|error| FsError::Io(error.to_string()))?;
                    }
                    #[cfg(not(unix))]
                    {
                        let _ = target;
                        return Err(FsError::Io("symlink retarget unsupported".into()));
                    }
                } else {
                    std::fs::write(&self.file, b"external-marker\n")
                        .map_err(|error| FsError::Io(error.to_string()))?;
                }
            }
            Ok(bytes)
        })
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn assembled_mutating_components_recheck_the_file_they_read() {
    within_deadline("real read record", async {
        for (package, input, policy) in [
            (
                "p1-module-write",
                ToolInput::Json(json!({"file_path":"victim", "content":"new"}).to_string()),
                MutationPolicy::Observed,
            ),
            (
                "p1-module-edit",
                ToolInput::Json(
                    json!({"file_path":"victim", "old_string":"before", "new_string":"after"})
                        .to_string(),
                ),
                MutationPolicy::Observed,
            ),
            (
                "p1-module-patch",
                ToolInput::Text(
                    "*** Begin Patch\n*** Update File: victim\n-before\n+after\n*** End Patch"
                        .into(),
                ),
                MutationPolicy::PatchAuthorized,
            ),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let file = dir.path().join("victim");
            std::fs::write(&file, b"before\n").unwrap();
            let workspace = Workspace::new(dir.path()).unwrap();
            let observed = ObservedFiles::new();
            workspace.read("victim", &observed).unwrap();
            let reads = ReadRecord::new();
            let read = Arc::new(ReadCapability::with_reads(
                workspace.clone(),
                observed.clone(),
                reads.clone(),
                None,
            ));
            let injected = Arc::new(ReplacedAfterRead {
                read: read.clone(),
                file: file.clone(),
                retarget_to: None,
                fired: std::sync::atomic::AtomicBool::new(false),
            });
            let services = Services {
                workspace: Some(injected.clone()),
                snapshot: Some(read),
                workspace_mutation: Some(mutation_service_over(workspace, observed, reads, policy)),
                ..Services::default()
            };
            let release = module_release(package);
            let loaded = release
                .loader()
                .load(&format!(
                    "p1/{}",
                    package.strip_prefix("p1-module-").unwrap()
                ))
                .unwrap();
            let module = wasm_tool(
                &loaded,
                services,
                ExecutionLimits::default(),
                &Arc::new(MaskCounter::new()),
            )
            .unwrap();
            let call = ToolCall {
                call_id: "stale".into(),
                name: module.declaration().name.clone(),
                input,
            };
            let result = execute(module.as_ref(), &call).await;
            assert!(
                injected.fired.load(std::sync::atomic::Ordering::SeqCst),
                "{package} did not read"
            );
            assert_eq!(result.status, ToolStatus::Error, "{package}: {result:?}");
            assert_eq!(std::fs::read(&file).unwrap(), b"external-marker\n");
        }
    })
    .await;
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn assembled_write_and_edit_refuse_retargeted_source_after_read() {
    use std::os::unix::fs::symlink;
    within_deadline("retargeted assembled mutation", async {
        for (package, input) in [
            (
                "p1-module-write",
                ToolInput::Json(json!({"file_path":"link", "content":"new"}).to_string()),
            ),
            (
                "p1-module-edit",
                ToolInput::Json(
                    json!({"file_path":"link", "old_string":"same", "new_string":"new"})
                        .to_string(),
                ),
            ),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let (a, b, link) = (
                dir.path().join("a"),
                dir.path().join("b"),
                dir.path().join("link"),
            );
            std::fs::write(&a, b"same\n").unwrap();
            std::fs::write(&b, b"same\n").unwrap();
            symlink("a", &link).unwrap();
            let workspace = Workspace::new(dir.path()).unwrap();
            let observed = ObservedFiles::new();
            workspace.read("a", &observed).unwrap();
            workspace.read("b", &observed).unwrap();
            let reads = ReadRecord::new();
            let read = Arc::new(ReadCapability::with_reads(
                workspace.clone(),
                observed.clone(),
                reads.clone(),
                None,
            ));
            let injected = Arc::new(ReplacedAfterRead {
                read: read.clone(),
                file: link,
                retarget_to: Some(PathBuf::from("b")),
                fired: std::sync::atomic::AtomicBool::new(false),
            });
            let services = Services {
                workspace: Some(injected.clone()),
                snapshot: Some(read),
                workspace_mutation: Some(mutation_service_over(
                    workspace,
                    observed,
                    reads,
                    MutationPolicy::Observed,
                )),
                ..Services::default()
            };
            let release = module_release(package);
            let loaded = release
                .loader()
                .load(&format!(
                    "p1/{}",
                    package.strip_prefix("p1-module-").unwrap()
                ))
                .unwrap();
            let module = wasm_tool(
                &loaded,
                services,
                ExecutionLimits::default(),
                &Arc::new(MaskCounter::new()),
            )
            .unwrap();
            let call = ToolCall {
                call_id: "retarget".into(),
                name: module.declaration().name.clone(),
                input,
            };
            let result = execute(module.as_ref(), &call).await;
            assert!(injected.fired.load(std::sync::atomic::Ordering::SeqCst));
            assert_eq!(result.status, ToolStatus::Error, "{package}: {result:?}");
            assert_eq!(std::fs::read(&a).unwrap(), b"same\n");
            assert_eq!(std::fs::read(&b).unwrap(), b"same\n");
        }
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn assembled_mutating_components_use_real_credential_and_observation_services() {
    within_deadline("mutating components", async {
        for (package, input, policy) in [
            ("p1-module-write", ToolInput::Json(json!({"file_path":"alias", "content":"changed"}).to_string()), MutationPolicy::Observed),
            ("p1-module-edit", ToolInput::Json(json!({"file_path":"alias", "old_string":"private-marker", "new_string":"changed"}).to_string()), MutationPolicy::Observed),
            ("p1-module-patch", ToolInput::Text("*** Begin Patch\n*** Delete File: alias\n*** End Patch".into()), MutationPolicy::PatchAuthorized),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let credential = dir.path().join(".codex/auth.json");
            std::fs::create_dir_all(credential.parent().unwrap()).unwrap();
            std::fs::write(&credential, b"private-marker\n").unwrap();
            std::fs::hard_link(&credential, dir.path().join("alias")).unwrap();
            let workspace = Workspace::new(dir.path()).unwrap();
            let observed = ObservedFiles::new();
            workspace.read("alias", &observed).unwrap();
            let release = module_release(package);
            let loaded = release.loader().load(&format!("p1/{}", package.strip_prefix("p1-module-").unwrap())).unwrap();
            let module = wasm_tool(&loaded, tool_services(workspace, observed, Some(dir.path().into()), Some(policy)),
                ExecutionLimits::default(), &Arc::new(MaskCounter::new())).unwrap();
            let call = ToolCall { call_id: "protected".into(), name: module.declaration().name.clone(), input };
            let result = execute(module.as_ref(), &call).await;
            assert_eq!(result.status, ToolStatus::Error, "{package}: {result:?}");
            assert_eq!(std::fs::read(&credential).unwrap(), b"private-marker\n");
            assert_eq!(std::fs::read(dir.path().join("alias")).unwrap(), b"private-marker\n");
        }
    }).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native_and_component_refuse_home_credentials_and_hardlink_aliases() {
    within_deadline("credentials", async {
        let dir = tempfile::tempdir().unwrap();
        for (path, contents) in [
            (".codex/auth.json", "private-marker\n"),
            (".config/keys/fixture.key", "private-marker\n"),
            ("public.txt", "public-marker\n"),
        ] {
            let target = dir.path().join(path);
            std::fs::create_dir_all(target.parent().unwrap()).unwrap();
            std::fs::write(target, contents).unwrap();
        }
        std::fs::hard_link(
            dir.path().join(".codex/auth.json"),
            dir.path().join("alias.txt"),
        )
        .unwrap();
        let workspace = Workspace::new(dir.path())
            .unwrap()
            .with_credential_home(Some(dir.path().to_path_buf()));
        let pair = both_tools(&workspace);
        let content = pair.same(r#"{"pattern":"private-marker"}"#).await;
        assert!(!content.content.contains("private-marker"));
        let files = pair.same(r#"{"pattern":"","mode":"files"}"#).await;
        assert!(!files.content.contains("alias.txt"));
        assert!(!files.content.contains("auth.json"));
        assert!(!files.content.contains("fixture.key"));
        assert_eq!(
            pair.same(r#"{"pattern":"public-marker"}"#).await.status,
            ToolStatus::Ok
        );
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
