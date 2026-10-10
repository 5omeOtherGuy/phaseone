//! The four S2 tool components serving real calls (issue #375, step 4).
//!
//! Every case loads one built package of `p1/edit`, `p1/write`, `p1/patch` and `p1/search`
//! through the production loader over the artifacts `scripts/build-modules.sh --all`
//! published, and links it as the host's catalog links its row
//! (`p1_host::catalog::capability_services_for`): the agent's read side and search walk, the
//! mutation mode the row grants, and a read record shared by the read side and the
//! mutation, fresh for every call (ADR-0092).
//!
//! A parity case then runs the SAME call through the component and through the native tool
//! the catalog registers while no lock selects the key, each over its own workspace seeded
//! with the same files and each wrapped in the masking decorator the host hands the model.
//! The two must be indistinguishable through `p1_contracts::Tool`: the same status, the same
//! content, the same call and result descriptions, the same masking, and the same tree left
//! behind. A mutating call changes its workspace, so the two sides cannot share one; a
//! read-only case would not need two, and none here assumes it. A path whose *description*
//! the component decides lexically from the call text alone — an absolute path inside the
//! root, a relative path through an escaping symlink — is compared by outcome and tree only;
//! that divergence is the components' accepted restricted-path contract
//! (`docs/design/modules/protocol.md`, slice U-desc).
//!
//! Besides parity, the cases pin the invariants: read-before-mutate through the components,
//! the write gate serializing component calls against each other, the read identity refusing
//! a target another agent changed between the component's read and its gated write, atomic
//! per-file replacement, symlink confinement, and a search that grants no permission to
//! change. The last case assembles each key through the host's own catalog entry point, so a
//! `modules.lock` entry named `edit`, `write`, `apply_patch` or `grep` really serves its calls
//! as the component.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use p1_assembly::{
    Catalog, EnvironmentFile, ModulesLock, ProviderSpec, Substitutions, ToolServices, ToolSpec,
    assemble,
};
use p1_contracts::serde_json::{self, Value, json};
use p1_contracts::{
    BoxFuture, CancellationToken, DeclarationKind, ModelOptions, Provider, Tool, ToolCall,
    ToolContext, ToolInput, ToolOutcome, ToolResultItem, ToolStatus,
};
use p1_host::catalog::modules::{ModuleServices, load_locked_modules, register_modules};
use p1_module_runtime::capabilities::{HeldMutation, MutationService};
use p1_module_runtime::{ExecutionLimits, Loader, ReleaseManifest, Services, wasm_tool};
use p1_module_tests::{lock_text, within_deadline};
use p1_redact::{MaskCounter, redacted};
use p1_testkit::ScriptedProvider;
use p1_workspace::{Observation, ObservedFiles, Workspace};

// ---------------------------------------------------------------------------- the tool rows

/// One of the four S2 file tools: the package `scripts/build-modules.sh` builds, the module
/// name (the component's verified manifest name, what the loader loads and what the host's
/// services hook keys the row's grants on), the catalog key a `modules.lock` entry names, and
/// the native tool of the same call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Row {
    Edit,
    Write,
    Patch,
    Search,
}

impl Row {
    /// Every S2 row, in catalog-key order.
    const ALL: [Self; 4] = [Self::Edit, Self::Write, Self::Patch, Self::Search];

    fn package(self) -> &'static str {
        match self {
            Self::Edit => "p1-module-edit",
            Self::Write => "p1-module-write",
            Self::Patch => "p1-module-patch",
            Self::Search => "p1-module-search",
        }
    }

    fn module(self) -> &'static str {
        match self {
            Self::Edit => "p1/edit",
            Self::Write => "p1/write",
            Self::Patch => "p1/patch",
            Self::Search => "p1/search",
        }
    }

    /// The tool's model-facing name, which is also the `modules.lock` key the catalog selects
    /// the package by.
    fn key(self) -> &'static str {
        match self {
            Self::Edit => "edit",
            Self::Write => "write",
            Self::Patch => "apply_patch",
            Self::Search => "grep",
        }
    }

    /// The native tool of the same call over one agent.
    fn native(self, workspace: &Workspace, observed: &ObservedFiles) -> Arc<dyn Tool> {
        match self {
            Self::Edit => Arc::new(p1_tool_edit::EditTool::new(
                workspace.clone(),
                observed.clone(),
            )),
            Self::Write => Arc::new(p1_tool_write::WriteTool::new(
                workspace.clone(),
                observed.clone(),
            )),
            Self::Patch => Arc::new(p1_tool_patch::PatchTool::new(
                workspace.clone(),
                observed.clone(),
            )),
            Self::Search => Arc::new(p1_tool_search::GrepTool::new(workspace.clone())),
        }
    }

    /// The services the host links this row's component with, as `catalog/tools.rs` builds
    /// them for the module (the row's mutation mode, the call's read record).
    fn services(
        self,
        workspace: &Workspace,
        observed: &ObservedFiles,
        home: Option<PathBuf>,
    ) -> Services {
        p1_host::catalog::capability_services_for(
            self.module(),
            workspace.clone(),
            observed.clone(),
            home,
        )
    }
}

// ------------------------------------------------------------------------- the built release

/// A release directory holding one component package, laid out as p1's release archive ships
/// it, with the release entry that names it: a release of its own, so a case needs no other
/// package built.
struct Release {
    dir: tempfile::TempDir,
    entry: Value,
}

impl Release {
    /// The package as `scripts/build-modules.sh` published it under
    /// `modules/target/p1-modules`.
    fn of(package: &str) -> Self {
        let built = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../modules/target/p1-modules")
            .join(package);
        let missing = |path: &Path, error: std::io::Error| -> ! {
            panic!(
                "the {package} artifact {} is missing ({error}): run \
                 scripts/build-modules.sh --all first",
                path.display()
            )
        };
        let wasm_path = built.join(format!("{package}.wasm"));
        let manifest_path = built.join(format!("{package}.manifest.json"));
        let wasm = fs::read(&wasm_path).unwrap_or_else(|error| missing(&wasm_path, error));
        let manifest: Value = serde_json::from_str(
            &fs::read_to_string(&manifest_path)
                .unwrap_or_else(|error| missing(&manifest_path, error)),
        )
        .expect("the package manifest is JSON");
        let path = format!("packages/{package}/{package}.wasm");
        let release = Self {
            dir: tempfile::tempdir().expect("release dir"),
            entry: json!({
                "name": manifest["name"],
                "digest": manifest["digest"],
                "path": path,
                "kind": manifest["kind"],
                "world": manifest["world"],
                "protocol": manifest["protocol"],
                "capabilities": manifest["capabilities"],
                "variant": manifest["variant"],
            }),
        };
        let component = release.dir.path().join(&path);
        fs::create_dir_all(component.parent().expect("package dir")).expect("package dir");
        fs::write(&component, &wasm).expect("component file");
        let listing = json!({
            "format": "p1-release-manifest/1",
            "components": [release.entry.clone()],
        });
        fs::write(release.manifest_file(), listing.to_string()).expect("release manifest");
        release
    }

    fn manifest_file(&self) -> PathBuf {
        self.dir.path().join("manifest.json")
    }

    fn root(&self) -> &Path {
        self.dir.path()
    }

    fn loader(&self) -> Loader {
        let manifest = ReleaseManifest::read(&self.manifest_file()).expect("release manifest");
        Loader::new(manifest, self.dir.path()).expect("loader")
    }

    /// The package loaded by name through the production loader, linked with `services` and
    /// wrapped by the masking decorator `wasm_tool` returns.
    fn tool(&self, row: Row, services: Services, mask: &Arc<MaskCounter>) -> Arc<dyn Tool> {
        let loaded = self
            .loader()
            .load(row.module())
            .unwrap_or_else(|error| panic!("{} loads by name: {error}", row.module()));
        assert_eq!(loaded.identity().implementation, row.module());
        wasm_tool(&loaded, services, ExecutionLimits::default(), mask)
            .unwrap_or_else(|error| panic!("{} is a tool: {error}", row.module()))
    }
}

// ------------------------------------------------------------------------- the parity harness

/// A workspace seeded with `files`, and where the tools of that side run.
struct Side {
    root: tempfile::TempDir,
    workspace: Workspace,
}

impl Side {
    fn of(files: &[(&str, &str)]) -> Self {
        let root = tempfile::tempdir().expect("workspace dir");
        for (path, contents) in files {
            let path = root.path().join(path);
            fs::create_dir_all(path.parent().expect("parent")).expect("parent dir");
            fs::write(path, contents).expect("seed file");
        }
        let workspace = Workspace::new(root.path()).expect("workspace");
        Self { root, workspace }
    }

    fn text(&self, path: &str) -> String {
        fs::read_to_string(self.root.path().join(path)).unwrap()
    }

    fn entries(&self) -> Vec<String> {
        entries(self.root.path())
    }
}

/// The native tool and the component of one row, each over its own identically seeded
/// workspace: a mutating call changes the workspace, so the two sides cannot share one. Each
/// side has its own observations and its own mask counter, so a case can tell them apart, and
/// both tools are wrapped by the masking decorator as the host wraps every assembled tool.
struct Pair {
    native: Arc<dyn Tool>,
    native_observed: ObservedFiles,
    native_mask: Arc<MaskCounter>,
    native_side: Side,
    module: Arc<dyn Tool>,
    module_observed: ObservedFiles,
    module_mask: Arc<MaskCounter>,
    module_side: Side,
    _release: Release,
}

impl Pair {
    /// Both tools of `row` over two workspaces holding the same `files`, the component linked
    /// as the host links its row.
    fn of(row: Row, files: &[(&str, &str)]) -> Self {
        let native_side = Side::of(files);
        let native_observed = ObservedFiles::new();
        let native_mask = Arc::new(MaskCounter::new());
        let native = redacted(
            row.native(&native_side.workspace, &native_observed),
            &native_mask,
        );

        let module_side = Side::of(files);
        let module_observed = ObservedFiles::new();
        let module_mask = Arc::new(MaskCounter::new());
        let release = Release::of(row.package());
        let module = release.tool(
            row,
            row.services(&module_side.workspace, &module_observed, None),
            &module_mask,
        );

        Pair {
            native,
            native_observed,
            native_mask,
            native_side,
            module,
            module_observed,
            module_mask,
            module_side,
            _release: release,
        }
    }

    /// The call the model makes: the tool's own name and declaration form, over `raw`.
    fn call(&self, raw: &str) -> ToolCall {
        let name = self.native.declaration().name.clone();
        let input = match &self.native.declaration().kind {
            DeclarationKind::Function { .. } => ToolInput::Json(raw.to_owned()),
            DeclarationKind::Freeform { .. } => ToolInput::Text(raw.to_owned()),
        };
        ToolCall {
            call_id: "c1".into(),
            name,
            input,
        }
    }

    /// Records the same observation for both agents, so a mutating case starts from "this
    /// agent read the file" on both sides.
    fn record(&self, path: &str, contents: &[u8]) {
        self.native_observed
            .record(&self.native_side.root.path().join(path), contents);
        self.module_observed
            .record(&self.module_side.root.path().join(path), contents);
    }

    /// Runs `raw` through both tools and requires the same outcome, the same description of
    /// the call and of its result, and the same tree left behind: what the model sees, and
    /// what the workspace holds afterwards, cannot differ between them.
    async fn same(&self, raw: &str) -> ToolOutcome {
        let call = self.call(raw);
        let native = execute(self.native.as_ref(), &call).await;
        let module = execute(self.module.as_ref(), &call).await;
        assert_eq!(
            (module.status, &module.content),
            (native.status, &native.content),
            "outcome of {raw}"
        );
        assert_eq!(
            self.module.describe(&call),
            self.native.describe(&call),
            "describe {raw}"
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
            "describe_result of {raw}"
        );
        self.same_tree(raw);
        native
    }

    /// As [`Self::same`], for a call whose *description* the two sides may legitimately
    /// differ on: a path the component decides lexically (from the call text alone) and the
    /// native tool resolves against the real filesystem.
    async fn same_outcome(&self, raw: &str) -> ToolOutcome {
        let call = self.call(raw);
        let native = execute(self.native.as_ref(), &call).await;
        let module = execute(self.module.as_ref(), &call).await;
        assert_eq!(
            (module.status, &module.content),
            (native.status, &native.content),
            "outcome of {raw}"
        );
        self.same_tree(raw);
        native
    }

    fn same_tree(&self, raw: &str) {
        assert_eq!(
            self.module_side.entries(),
            self.native_side.entries(),
            "the trees agree after {raw}"
        );
    }
}

/// Every entry under `dir`, recursively, as names relative to `dir` with a rendered value:
/// a file's contents, `dir` for a directory, and a link's target. A symlink is never
/// descended, so a link pointing outside cannot fake entries.
fn entries(dir: &Path) -> Vec<String> {
    let mut found = Vec::new();
    let mut pending = vec![dir.to_path_buf()];
    while let Some(next) = pending.pop() {
        for entry in fs::read_dir(&next).expect("read_dir") {
            let entry = entry.expect("entry");
            let path = entry.path();
            let name = path
                .strip_prefix(dir)
                .expect("inside")
                .to_string_lossy()
                .into_owned();
            let meta = fs::symlink_metadata(&path).expect("metadata");
            if meta.is_dir() {
                pending.push(path);
                found.push(format!("{name}/"));
            } else if meta.is_symlink() {
                let target = fs::read_link(&path).expect("link target");
                found.push(format!("{name} -> {}", target.to_string_lossy()));
            } else {
                let contents = fs::read_to_string(&path).unwrap_or_else(|_| "<binary>".to_owned());
                found.push(format!("{name} = {contents}"));
            }
        }
    }
    found.sort();
    found
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

/// A credential-shaped value built at runtime: never a literal in the tree (the secret scan
/// refuses one).
fn key() -> String {
    format!("sk-{}", "a".repeat(24))
}

/// A component linked with its row's services, with each call's mutation service wrapped so
/// a case learns when a call asks for the write gate — no sleep, no polling. The row's
/// services are call-scoped (ADR-0092), so the wrapping is too: every call's own mutation
/// service, over that call's read record, is the one that announces.
fn announcing(
    row: Row,
    workspace: &Workspace,
    observed: &ObservedFiles,
    waiting: tokio::sync::mpsc::UnboundedSender<()>,
) -> Arc<dyn Tool> {
    let scope = row
        .services(workspace, observed, None)
        .call_scope
        .expect("the row's services are call-scoped");
    let services = Services::call_scoped(move || {
        let mut call = scope();
        let inner = call
            .workspace_mutation
            .take()
            .expect("a mutating row links a mutation service");
        call.workspace_mutation = Some(Arc::new(Announcing {
            inner,
            waiting: waiting.clone(),
        }));
        call
    });
    let release = Release::of(row.package());
    release.tool(row, services, &Arc::new(MaskCounter::new()))
}

/// The runtime's mutation service, wrapped to say when a call has asked for the gate, so a
/// case can order its next step after the component's read and before its gated write.
struct Announcing {
    inner: Arc<dyn MutationService>,
    waiting: tokio::sync::mpsc::UnboundedSender<()>,
}

impl MutationService for Announcing {
    fn begin(&self) -> BoxFuture<'_, Box<dyn HeldMutation>> {
        let _ = self.waiting.send(());
        self.inner.begin()
    }
}

// `WasmTool` starts its executor on the current Tokio runtime and the services' file work
// runs on its blocking pool: every case runs inside a multi-threaded runtime.

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn every_component_declares_and_effects_what_its_native_tool_does() {
    within_deadline("declarations", async {
        for row in Row::ALL {
            let pair = Pair::of(row, &[("a.txt", "alpha\n")]);
            assert_eq!(
                pair.module.declaration(),
                pair.native.declaration(),
                "{row:?} declares what the native tool declares"
            );
            assert_eq!(
                pair.module.declaration().name,
                row.key(),
                "{row:?} is named by its catalog key"
            );
            let probe = pair.call("probe");
            assert_eq!(
                pair.module.effect(&probe),
                pair.native.effect(&probe),
                "{row:?} claims the same effect"
            );
        }
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn edit_serves_the_same_calls_as_the_native_tool() {
    within_deadline("edit parity", async {
        let pair = Pair::of(
            Row::Edit,
            &[
                ("notes.txt", "alpha\nbeta\ngamma\n"),
                ("crlf.txt", "one\r\ntwo\r\n"),
                ("no-final.txt", "one\ntwo"),
                ("twice.txt", "same\nsame\n"),
                ("a.txt", "never read\n"),
            ],
        );
        for (path, contents) in [
            ("notes.txt", "alpha\nbeta\ngamma\n"),
            ("crlf.txt", "one\r\ntwo\r\n"),
            ("no-final.txt", "one\ntwo"),
            ("twice.txt", "same\nsame\n"),
        ] {
            pair.record(path, contents.as_bytes());
        }

        let edited = pair
            .same(r#"{"file_path":"notes.txt","old_string":"beta","new_string":"BETA"}"#)
            .await;
        assert_eq!(edited.status, ToolStatus::Ok, "{}", edited.content);
        assert_eq!(edited.content, "Edited notes.txt (1 replacement).");
        assert_eq!(pair.native_side.text("notes.txt"), "alpha\nBETA\ngamma\n");

        // A second edit of the same file needs no re-read: the host recorded what the edit
        // wrote.
        let again = pair
            .same(r#"{"file_path":"notes.txt","old_string":"alpha","new_string":"ALPHA"}"#)
            .await;
        assert_eq!(again.status, ToolStatus::Ok, "{}", again.content);
        assert_eq!(pair.native_side.text("notes.txt"), "ALPHA\nBETA\ngamma\n");

        // #458 unit 3: an identical old/new is a no-op on both sides, not an input error.
        let unchanged = pair
            .same(r#"{"file_path":"notes.txt","old_string":"ALPHA","new_string":"ALPHA"}"#)
            .await;
        assert_eq!(unchanged.status, ToolStatus::Ok, "{}", unchanged.content);
        assert_eq!(
            unchanged.content,
            "No change: old_string and new_string are identical; notes.txt was not modified."
        );
        assert_eq!(pair.native_side.text("notes.txt"), "ALPHA\nBETA\ngamma\n");

        // Line endings and a missing final newline are preserved, not rewritten.
        pair.same(r#"{"file_path":"crlf.txt","old_string":"one","new_string":"ONE"}"#)
            .await;
        assert_eq!(pair.native_side.text("crlf.txt"), "ONE\r\ntwo\r\n");
        pair.same(r#"{"file_path":"no-final.txt","old_string":"two","new_string":"TWO"}"#)
            .await;
        assert_eq!(pair.native_side.text("no-final.txt"), "one\nTWO");

        // Refusals: an unobserved file, a missing match, an ambiguous match, invalid input.
        let unobserved = pair
            .same(r#"{"file_path":"a.txt","old_string":"x","new_string":"y"}"#)
            .await;
        assert_eq!(
            unobserved.status,
            ToolStatus::Error,
            "{}",
            unobserved.content
        );
        assert_eq!(
            unobserved.content,
            "You must read a.txt before changing it."
        );
        let missing = pair
            .same(r#"{"file_path":"notes.txt","old_string":"absent","new_string":"x"}"#)
            .await;
        assert_eq!(missing.status, ToolStatus::Error, "{}", missing.content);
        let ambiguous = pair
            .same(r#"{"file_path":"twice.txt","old_string":"same","new_string":"other"}"#)
            .await;
        assert_eq!(ambiguous.status, ToolStatus::Error, "{}", ambiguous.content);
        for invalid in [
            "not json",
            "{}",
            r#"{"file_path":"notes.txt","old_string":"a"}"#,
        ] {
            let outcome = pair.same(invalid).await;
            assert_eq!(outcome.status, ToolStatus::Error, "{invalid}");
        }
        assert_eq!(pair.native_side.text("twice.txt"), "same\nsame\n");
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn write_serves_the_same_calls_as_the_native_tool() {
    within_deadline("write parity", async {
        let pair = Pair::of(Row::Write, &[("a.txt", "old\n"), ("b.txt", "b\n")]);
        pair.record("a.txt", b"old\n");

        let created = pair
            .same(r#"{"file_path":"deep/nested/new.txt","content":"hello\n"}"#)
            .await;
        assert_eq!(created.status, ToolStatus::Ok, "{}", created.content);
        assert_eq!(created.content, "Wrote deep/nested/new.txt (6 bytes).");
        assert_eq!(pair.native_side.text("deep/nested/new.txt"), "hello\n");

        let empty = pair.same(r#"{"file_path":"empty.txt","content":""}"#).await;
        assert_eq!(empty.status, ToolStatus::Ok, "{}", empty.content);
        assert_eq!(empty.content, "Wrote empty.txt (0 bytes).");

        let overwritten = pair
            .same(r#"{"file_path":"a.txt","content":"new\n"}"#)
            .await;
        assert_eq!(
            overwritten.status,
            ToolStatus::Ok,
            "{}",
            overwritten.content
        );
        assert_eq!(pair.native_side.text("a.txt"), "new\n");

        // An existing file this agent never read is refused, on both sides.
        let refused = pair
            .same(r#"{"file_path":"b.txt","content":"pwned\n"}"#)
            .await;
        assert_eq!(refused.status, ToolStatus::Error, "{}", refused.content);
        assert_eq!(refused.content, "You must read b.txt before changing it.");
        assert_eq!(pair.native_side.text("b.txt"), "b\n");

        // Invalid input never touches a file.
        for invalid in [
            "not json",
            "{}",
            r#"{"file_path":"c.txt"}"#,
            r#"{"file_path":"c.txt","content":5}"#,
        ] {
            let outcome = pair.same(invalid).await;
            assert_eq!(outcome.status, ToolStatus::Error, "{invalid}");
        }
        assert!(!pair.native_side.root.path().join("c.txt").exists());
        assert!(!pair.module_side.root.path().join("c.txt").exists());
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn patch_serves_the_same_calls_as_the_native_tool() {
    within_deadline("patch parity", async {
        let pair = Pair::of(
            Row::Patch,
            &[
                ("a.txt", "one\ntwo\n"),
                ("gone.txt", "delete me\n"),
                ("from.txt", "move me\n"),
            ],
        );

        // The patch row is exempt from read-before-mutate (ADR-0025): neither side needs an
        // observation of the files it rewrites.
        let added = pair
            .same("*** Begin Patch\n*** Add File: new.txt\n+line one\n+line two\n*** End Patch\n")
            .await;
        assert_eq!(added.status, ToolStatus::Ok, "{}", added.content);
        assert_eq!(added.content, "A new.txt");
        assert_eq!(pair.native_side.text("new.txt"), "line one\nline two\n");

        let updated = pair
            .same("*** Begin Patch\n*** Update File: a.txt\n@@\n-one\n+ONE\n*** End Patch\n")
            .await;
        assert_eq!(updated.status, ToolStatus::Ok, "{}", updated.content);
        assert_eq!(updated.content, "M a.txt");
        assert_eq!(pair.native_side.text("a.txt"), "ONE\ntwo\n");

        let deleted = pair
            .same("*** Begin Patch\n*** Delete File: gone.txt\n*** End Patch\n")
            .await;
        assert_eq!(deleted.status, ToolStatus::Ok, "{}", deleted.content);
        assert_eq!(deleted.content, "D gone.txt");
        assert!(!pair.native_side.root.path().join("gone.txt").exists());

        let moved = pair
            .same(
                "*** Begin Patch\n*** Update File: from.txt\n*** Move to: to.txt\n@@\n\
                 -move me\n+move me\n+moved\n*** End Patch\n",
            )
            .await;
        assert_eq!(moved.status, ToolStatus::Ok, "{}", moved.content);
        assert_eq!(moved.content, "M from.txt -> to.txt");
        assert_eq!(pair.native_side.text("to.txt"), "move me\nmoved\n");
        assert!(!pair.native_side.root.path().join("from.txt").exists());

        // A context mismatch changes nothing, and invalid patches are refused.
        let mismatch = pair
            .same("*** Begin Patch\n*** Update File: a.txt\n@@\n-absent\n+x\n*** End Patch\n")
            .await;
        assert_eq!(mismatch.status, ToolStatus::Error, "{}", mismatch.content);
        assert_eq!(pair.native_side.text("a.txt"), "ONE\ntwo\n");
        for invalid in [
            "not a patch",
            "*** Begin Patch\n*** Update File: a.txt\n@@\n-x\n+y\n",
            "*** Begin Patch\n*** Add File: a.txt\n+x\n*** End Patch\n",
        ] {
            let outcome = pair.same(invalid).await;
            assert_eq!(outcome.status, ToolStatus::Error, "{invalid}");
        }
        assert_eq!(pair.native_side.text("a.txt"), "ONE\ntwo\n");
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn search_serves_the_same_calls_as_the_native_tool() {
    within_deadline("search parity", async {
        let pair = Pair::of(
            Row::Search,
            &[
                ("src/a.rs", "fn alpha() {}\nfn beta() {}\n"),
                ("src/b.rs", "fn beta() {}\n"),
                ("notes.md", "beta in prose\n"),
            ],
        );

        let content = pair.same(r#"{"pattern":"beta"}"#).await;
        assert_eq!(content.status, ToolStatus::Ok, "{}", content.content);
        assert_eq!(
            content.content,
            "notes.md\n1:beta in prose\n\nsrc/a.rs\n2:fn beta() {}\n\nsrc/b.rs\n1:fn beta() {}"
        );

        let files = pair.same(r#"{"pattern":"beta","mode":"files"}"#).await;
        assert_eq!(files.content, "notes.md\nsrc/a.rs\nsrc/b.rs");
        let globbed = pair.same(r#"{"pattern":"beta","glob":"*.rs"}"#).await;
        assert_eq!(
            globbed.content,
            "src/a.rs\n2:fn beta() {}\n\nsrc/b.rs\n1:fn beta() {}"
        );
        let scoped = pair.same(r#"{"pattern":"beta","path":"src/b.rs"}"#).await;
        assert_eq!(scoped.content, "src/b.rs\n1:fn beta() {}");
        let none = pair.same(r#"{"pattern":"absent"}"#).await;
        assert_eq!(none.content, "No matches.");
        let insensitive = pair
            .same(r#"{"pattern":"BETA","case_insensitive":true,"mode":"files"}"#)
            .await;
        assert_eq!(insensitive.content, "notes.md\nsrc/a.rs\nsrc/b.rs");
        let context = pair.same(r#"{"pattern":"alpha","context":1}"#).await;
        assert_eq!(context.content, "src/a.rs\n1:fn alpha() {}\n2-fn beta() {}");
        for invalid in ["not json", "{}", r#"{"pattern":"("}"#] {
            let outcome = pair.same(invalid).await;
            assert_eq!(outcome.status, ToolStatus::Error, "{invalid}");
        }
    })
    .await;
}

/// The masking layer the host wraps every assembled tool in covers each component too: the
/// outcome content and the call's describe (an edit preview's new side), while the file on
/// disk keeps what was written.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_mask_covers_every_component_outcome() {
    within_deadline("masking", async {
        let secret = key();
        let mask = format!("<redacted:sk-:{} chars>", secret.len() - 3);

        let seeded = format!("token {secret}\n");
        let search = Pair::of(Row::Search, &[("notes.txt", seeded.as_str())]);
        let found = search.same(r#"{"pattern":"token"}"#).await;
        assert_eq!(found.status, ToolStatus::Ok, "{}", found.content);
        assert!(!found.content.contains(&secret), "{}", found.content);
        assert_eq!(found.content, format!("notes.txt\n1:token {mask}"));
        assert!(search.module_mask.take() > 0, "the component masked");
        assert!(search.native_mask.take() > 0, "the native tool masked");

        // A write whose *content* carries the value: the outcome has no secret, the call's
        // description (the edit preview's new side) must be masked on both sides.
        let write = Pair::of(Row::Write, &[]);
        let call = write.call(&json!({"file_path": "out.txt", "content": secret}).to_string());
        let outcome = execute(write.module.as_ref(), &call).await;
        assert_eq!(outcome.status, ToolStatus::Ok, "{}", outcome.content);
        assert!(!outcome.content.contains(&secret), "{}", outcome.content);
        let described = write.module.describe(&call);
        let preview = described.edit.expect("write describes an edit preview");
        assert!(!preview.new.contains(&secret), "{}", preview.new);
        assert_eq!(write.module.describe(&call), write.native.describe(&call));
        assert!(write.module_mask.take() > 0);
        assert!(
            write.module_side.text("out.txt").contains(&secret),
            "masking never rewrites the file"
        );
    })
    .await;
}

/// Read-before-mutate holds through both mutating components of the observed rows: the
/// refusal is the host's, word for word the native tool's, and nothing is observed or left
/// behind.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unread_existing_file_is_refused_through_both_mutating_components() {
    within_deadline("read before mutate", async {
        let edit = Pair::of(Row::Edit, &[("a.txt", "one\n")]);
        let refused = edit
            .same(r#"{"file_path":"a.txt","old_string":"one","new_string":"ONE"}"#)
            .await;
        assert_eq!(refused.status, ToolStatus::Error, "{}", refused.content);
        assert_eq!(refused.content, "You must read a.txt before changing it.");

        let write = Pair::of(Row::Write, &[("a.txt", "one\n")]);
        let refused = write
            .same(r#"{"file_path":"a.txt","content":"new\n"}"#)
            .await;
        assert_eq!(refused.status, ToolStatus::Error, "{}", refused.content);
        assert_eq!(refused.content, "You must read a.txt before changing it.");

        // Neither component observed the file it refused.
        for pair in [&edit, &write] {
            assert_eq!(
                pair.module_observed
                    .check_unchanged(&pair.module_side.root.path().join("a.txt"), b"one\n"),
                Observation::NeverObserved
            );
            assert_eq!(pair.module_side.text("a.txt"), "one\n");
            assert_eq!(pair.module_side.entries(), vec!["a.txt = one\n".to_owned()]);
        }
    })
    .await;
}

/// #458 unit 4: a file this agent last changed with `write` (or `edit`) keeps its read state,
/// so a following edit of it is accepted, not refused as "changed on disk". The write and edit
/// components share one agent's workspace and observations, as the host wires them for a
/// single agent; the write's own bytes become that agent's observation.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_agents_own_write_or_edit_keeps_its_read_state() {
    within_deadline("own write read state", async {
        let side = Side::of(&[("seed.txt", "alpha\nbeta\n")]);
        let observed = ObservedFiles::new();
        let mask = Arc::new(MaskCounter::new());
        let write_release = Release::of(Row::Write.package());
        let write = write_release.tool(
            Row::Write,
            Row::Write.services(&side.workspace, &observed, None),
            &mask,
        );
        let edit_release = Release::of(Row::Edit.package());
        let edit = edit_release.tool(
            Row::Edit,
            Row::Edit.services(&side.workspace, &observed, None),
            &mask,
        );
        let call = |name: &str, raw: &str| ToolCall {
            call_id: "c1".into(),
            name: name.into(),
            input: ToolInput::Json(raw.to_owned()),
        };

        // The write component records the bytes it wrote as this agent's observation.
        let written = execute(
            write.as_ref(),
            &call(
                "write",
                r#"{"file_path":"made.txt","content":"one\ntwo\n"}"#,
            ),
        )
        .await;
        assert_eq!(written.status, ToolStatus::Ok, "{}", written.content);

        // The edit of the file the same agent just wrote is accepted, not "changed on disk".
        let edited = execute(
            edit.as_ref(),
            &call(
                "edit",
                r#"{"file_path":"made.txt","old_string":"two","new_string":"TWO"}"#,
            ),
        )
        .await;
        assert_eq!(edited.status, ToolStatus::Ok, "{}", edited.content);
        assert_eq!(side.text("made.txt"), "one\nTWO\n");

        // A second edit of the same file needs no re-read either.
        let again = execute(
            edit.as_ref(),
            &call(
                "edit",
                r#"{"file_path":"made.txt","old_string":"one","new_string":"ONE"}"#,
            ),
        )
        .await;
        assert_eq!(again.status, ToolStatus::Ok, "{}", again.content);
        assert_eq!(side.text("made.txt"), "ONE\nTWO\n");
    })
    .await;
}

/// A file search grants no permission to change: the `grep` component reads through the
/// search service, which records no observation, so the same agent's edit is refused.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_search_through_the_component_grants_no_permission_to_change() {
    within_deadline("search grants nothing", async {
        let root = tempfile::tempdir().unwrap();
        fs::write(root.path().join("a.txt"), "needle\n").unwrap();
        let workspace = Workspace::new(root.path()).unwrap();
        let observed = ObservedFiles::new();
        let release = Release::of(Row::Search.package());
        let grep = release.tool(
            Row::Search,
            Row::Search.services(&workspace, &observed, None),
            &Arc::new(MaskCounter::new()),
        );

        let found = execute(
            grep.as_ref(),
            &ToolCall {
                call_id: "c1".into(),
                name: "grep".into(),
                input: ToolInput::Json(json!({"pattern": "needle"}).to_string()),
            },
        )
        .await;
        assert_eq!(found.status, ToolStatus::Ok, "{}", found.content);
        assert_eq!(found.content, "a.txt\n1:needle");

        let edit = Release::of(Row::Edit.package()).tool(
            Row::Edit,
            Row::Edit.services(&workspace, &observed, None),
            &Arc::new(MaskCounter::new()),
        );
        let refused = execute(
            edit.as_ref(),
            &ToolCall {
                call_id: "c2".into(),
                name: "edit".into(),
                input: ToolInput::Json(
                    json!({"file_path": "a.txt", "old_string": "needle", "new_string": "thread"})
                        .to_string(),
                ),
            },
        )
        .await;
        assert_eq!(refused.status, ToolStatus::Error, "{}", refused.content);
        assert_eq!(refused.content, "You must read a.txt before changing it.");
        assert_eq!(entries(root.path()), vec!["a.txt = needle\n".to_owned()]);
        assert_eq!(
            observed.check_unchanged(&root.path().join("a.txt"), b"needle\n"),
            Observation::NeverObserved
        );
    })
    .await;
}

/// One write gate, two components: two agents that both read the file and then ask for the
/// gate together. Exactly one change lands; the other is refused as stale, never silently
/// overwriting the first.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_write_gate_serializes_two_components_on_one_workspace() {
    within_deadline("gate serialization", async {
        let root = tempfile::tempdir().unwrap();
        fs::write(root.path().join("a.txt"), "one\n").unwrap();
        let workspace = Workspace::new(root.path()).unwrap();
        let (waiting, mut waits) = tokio::sync::mpsc::unbounded_channel();
        let agents: Vec<Arc<dyn Tool>> = ["A\n", "B\n"]
            .into_iter()
            .map(|_| {
                let observed = ObservedFiles::new();
                observed.record(&root.path().join("a.txt"), b"one\n");
                announcing(Row::Write, &workspace, &observed, waiting.clone())
            })
            .collect();

        // Another agent holds the gate, so both calls read the file and then wait on it.
        let other = workspace.begin_mutation();
        let calls: Vec<_> = ["A\n", "B\n"]
            .into_iter()
            .zip(&agents)
            .map(|(content, tool)| {
                let call = ToolCall {
                    call_id: "c1".into(),
                    name: "write".into(),
                    input: ToolInput::Json(
                        json!({"file_path": "a.txt", "content": content}).to_string(),
                    ),
                };
                let tool = tool.clone();
                tokio::spawn(async move { execute(tool.as_ref(), &call).await })
            })
            .collect();
        waits
            .recv()
            .await
            .expect("the first call asks for the gate");
        waits
            .recv()
            .await
            .expect("the second call asks for the gate");
        assert!(
            calls.iter().all(|call| !call.is_finished()),
            "both calls wait while the gate is held"
        );
        drop(other);

        let outcomes: Vec<ToolOutcome> = futures_util::future::join_all(calls)
            .await
            .into_iter()
            .map(|outcome| outcome.expect("the call task"))
            .collect();
        let landed: Vec<&ToolOutcome> = outcomes
            .iter()
            .filter(|outcome| outcome.status == ToolStatus::Ok)
            .collect();
        assert_eq!(landed.len(), 1, "exactly one change lands: {outcomes:?}");
        assert!(
            landed[0].content.starts_with("Wrote a.txt"),
            "{}",
            landed[0].content
        );
        let refused = outcomes
            .iter()
            .find(|outcome| outcome.status != ToolStatus::Ok)
            .expect("one is refused");
        assert_eq!(
            refused.content,
            "a.txt changed on disk since you last read it; read it again."
        );
        let landed = fs::read_to_string(root.path().join("a.txt")).unwrap();
        assert!(["A\n", "B\n"].contains(&landed.as_str()), "{landed:?}");
        assert_eq!(entries(root.path()), vec![format!("a.txt = {landed}")]);
    })
    .await;
}

/// The read identity, under the gate: a patch-authorized change computed from a file another
/// writer replaced between the component's read and its gated write is refused as stale, and
/// the other writer's bytes stay. Patch-authorized has no observation to fall back on, so
/// only the read record can refuse this.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_change_between_the_components_read_and_its_gated_write_is_refused() {
    within_deadline("read identity", async {
        let root = tempfile::tempdir().unwrap();
        fs::write(root.path().join("a.txt"), "one\n").unwrap();
        let workspace = Workspace::new(root.path()).unwrap();
        let observed = ObservedFiles::new();
        let (waiting, mut waits) = tokio::sync::mpsc::unbounded_channel();
        let patch = announcing(Row::Patch, &workspace, &observed, waiting);

        // This agent holds the gate, so the component reads and plans, then waits on it.
        let other = workspace.begin_mutation();
        let call = ToolCall {
            call_id: "c1".into(),
            name: "apply_patch".into(),
            input: ToolInput::Text(
                "*** Begin Patch\n*** Update File: a.txt\n@@\n-one\n+ONE\n*** End Patch\n"
                    .to_owned(),
            ),
        };
        let applied = tokio::spawn(async move { execute(patch.as_ref(), &call).await });
        waits.recv().await.expect("the call asks for the gate");

        // A second writer replaces the file after the component read it.
        fs::write(root.path().join("a.txt"), "changed by another agent\n").unwrap();
        drop(other);

        let outcome = applied.await.expect("the call task");
        assert_eq!(outcome.status, ToolStatus::Error, "{}", outcome.content);
        assert_eq!(
            outcome.content,
            "a.txt changed on disk since you last read it; read it again."
        );
        assert_eq!(
            entries(root.path()),
            vec!["a.txt = changed by another agent\n".to_owned()]
        );
    })
    .await;
}

/// The read record is the call's own (ADR-0092): a later call of the same assembled tool does
/// not inherit an earlier call's read. One patch moves `a.txt` away — it read `a.txt` to do so
/// — and the next patch creates `a.txt` afresh, which a record shared across calls refused as
/// "changed on disk" (the file its digest names is gone).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_later_call_does_not_inherit_an_earlier_calls_read() {
    within_deadline("call-scoped record, sequential", async {
        let root = tempfile::tempdir().unwrap();
        fs::write(root.path().join("a.txt"), "one\n").unwrap();
        let workspace = Workspace::new(root.path()).unwrap();
        let release = Release::of(Row::Patch.package());
        let patch = release.tool(
            Row::Patch,
            Row::Patch.services(&workspace, &ObservedFiles::new(), None),
            &Arc::new(MaskCounter::new()),
        );
        let call = |text: &str| ToolCall {
            call_id: "c1".into(),
            name: "apply_patch".into(),
            input: ToolInput::Text(text.to_owned()),
        };

        let moved = execute(
            patch.as_ref(),
            &call(
                "*** Begin Patch\n*** Update File: a.txt\n*** Move to: b.txt\n@@\n-one\n+one\n\
                 *** End Patch\n",
            ),
        )
        .await;
        assert_eq!(moved.status, ToolStatus::Ok, "{}", moved.content);
        let added = execute(
            patch.as_ref(),
            &call("*** Begin Patch\n*** Add File: a.txt\n+fresh\n*** End Patch\n"),
        )
        .await;
        assert_eq!(added.status, ToolStatus::Ok, "{}", added.content);
        assert_eq!(added.content, "A a.txt");
        assert_eq!(
            entries(root.path()),
            vec!["a.txt = fresh\n".to_owned(), "b.txt = one\n".to_owned()]
        );
    })
    .await;
}

/// Two concurrent calls of one assembled tool never share a read record (ADR-0092): call A
/// reads `a.txt`, another agent changes it, call B reads the new contents, and only then do
/// both reach the gate. A's change was computed from the old contents and is refused whichever
/// call writes first; with one record per assembly B's read overwrote A's digest, so A's
/// stale change could pass the recheck and undo the other agent's line. B's change lands.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_calls_of_one_tool_keep_their_own_read_identity() {
    within_deadline("call-scoped record, concurrent", async {
        let root = tempfile::tempdir().unwrap();
        fs::write(root.path().join("a.txt"), "one\n").unwrap();
        let workspace = Workspace::new(root.path()).unwrap();
        let (waiting, mut waits) = tokio::sync::mpsc::unbounded_channel();
        let patch = announcing(Row::Patch, &workspace, &ObservedFiles::new(), waiting);
        let spawn = |text: &'static str| {
            let patch = patch.clone();
            let call = ToolCall {
                call_id: "c1".into(),
                name: "apply_patch".into(),
                input: ToolInput::Text(text.to_owned()),
            };
            tokio::spawn(async move { execute(patch.as_ref(), &call).await })
        };

        // Another agent holds the gate, so each call reads and plans, then waits on it.
        let other = workspace.begin_mutation();
        let first =
            spawn("*** Begin Patch\n*** Update File: a.txt\n@@\n-one\n+ONE\n*** End Patch\n");
        waits.recv().await.expect("call A asks for the gate");
        fs::write(root.path().join("a.txt"), "one\ntwo\n").unwrap();
        let second =
            spawn("*** Begin Patch\n*** Update File: a.txt\n@@\n-two\n+TWO\n*** End Patch\n");
        waits.recv().await.expect("call B asks for the gate");
        drop(other);

        let first = first.await.expect("call A");
        let second = second.await.expect("call B");
        assert_eq!(first.status, ToolStatus::Error, "{}", first.content);
        assert_eq!(
            first.content,
            "a.txt changed on disk since you last read it; read it again."
        );
        assert_eq!(second.status, ToolStatus::Ok, "{}", second.content);
        assert_eq!(entries(root.path()), vec!["a.txt = one\nTWO\n".to_owned()]);
    })
    .await;
}

/// A replacement is per file and atomic: a reader sees the old file or the new one, never a
/// half-written state, while a component's write runs.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_component_replacement_is_never_observed_partially() {
    within_deadline("atomicity", async {
        // Four megabytes, the oracle's size: big enough that an in-place write is caught
        // mid-write, and above the three megabytes wasmtime's default hostcall budget let a
        // component hand the host (ADR-0092, `p1_module_runtime::executor::HOSTCALL_FUEL`).
        let before = "a".repeat(4 * 1024 * 1024);
        let after = "b".repeat(4 * 1024 * 1024);
        let pair = Pair::of(Row::Write, &[("big.txt", &before)]);
        pair.record("big.txt", before.as_bytes());
        let path = pair.module_side.root.path().join("big.txt");

        let the_new = after.clone();
        let reading = std::thread::spawn(move || {
            let mut partial = 0usize;
            for _ in 0..2_000 {
                let seen = fs::read(&path).expect("read");
                if seen != before.as_bytes() && seen != the_new.as_bytes() {
                    partial += 1;
                }
            }
            partial
        });
        // The call description previews the new contents; a four-megabyte preview is not what
        // this case is about, so it compares the outcome and the tree.
        let written = pair
            .same_outcome(&json!({"file_path": "big.txt", "content": after}).to_string())
            .await;
        let partial = reading.join().expect("the reader thread");
        assert_eq!(written.status, ToolStatus::Ok, "{}", written.content);
        assert_eq!(partial, 0, "a reader must never see a partial replacement");
        assert_eq!(pair.module_side.text("big.txt"), after);
    })
    .await;
}

/// Confinement is the host's and holds for every component: an escaping `..` path, an
/// absolute path outside, and a path through a symlink that leaves the workspace are refused,
/// and nothing appears outside the root.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn every_component_refuses_a_path_outside_the_workspace() {
    within_deadline("confinement", async {
        let outside = tempfile::tempdir().unwrap();
        fs::write(outside.path().join("secret.txt"), "secret\n").unwrap();
        let absolute = outside.path().join("abs.txt");
        let calls = [
            (
                Row::Edit,
                json!({"file_path": "../escape.txt", "old_string": "a", "new_string": "b"})
                    .to_string(),
            ),
            (
                Row::Write,
                json!({"file_path": "link/new.txt", "content": "pwned\n"}).to_string(),
            ),
            (
                Row::Write,
                json!({"file_path": absolute.to_string_lossy(), "content": "pwned\n"}).to_string(),
            ),
            (
                Row::Search,
                json!({"pattern": "secret", "path": "../"}).to_string(),
            ),
            (
                Row::Search,
                json!({"pattern": "secret", "path": "link"}).to_string(),
            ),
        ];
        for (row, raw) in calls {
            let pair = Pair::of(row, &[("inside.txt", "inside\n")]);
            // The same escaping link on both sides, pointing at the one outside directory.
            std::os::unix::fs::symlink(outside.path(), pair.native_side.root.path().join("link"))
                .unwrap();
            std::os::unix::fs::symlink(outside.path(), pair.module_side.root.path().join("link"))
                .unwrap();
            let outcome = pair.same_outcome(&raw).await;
            assert_eq!(outcome.status, ToolStatus::Error, "{row:?} {raw}");
            assert!(
                outcome.content.contains("escapes workspace"),
                "{row:?} {raw}: {}",
                outcome.content
            );
        }

        // The patch row, whose calls are freeform text.
        let pair = Pair::of(Row::Patch, &[("inside.txt", "inside\n")]);
        let outcome = pair
            .same_outcome("*** Begin Patch\n*** Add File: ../escape.txt\n+pwned\n*** End Patch\n")
            .await;
        assert_eq!(outcome.status, ToolStatus::Error, "{}", outcome.content);
        assert!(
            outcome.content.contains("escapes workspace"),
            "{}",
            outcome.content
        );

        assert!(!outside.path().join("escape.txt").exists());
        assert_eq!(
            fs::read_to_string(outside.path().join("secret.txt")).unwrap(),
            "secret\n"
        );
        assert!(!outside.path().join("new.txt").exists());
        assert!(!outside.path().join("abs.txt").exists());
    })
    .await;
}

/// A lock entry named for each key selects its package, and the host's own catalog entry
/// point then serves that key's calls as the component: the same outcome as the native tool
/// in the same state, with the row's mutation mode.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn each_key_assembles_its_component_through_the_catalog() {
    within_deadline("catalog", async {
        // The call each key serves over the seeded workspace.
        let cases = [
            (
                Row::Edit,
                "notes.txt",
                "alpha\nbeta\n",
                r#"{"file_path":"notes.txt","old_string":"beta","new_string":"BETA"}"#,
            ),
            (
                Row::Write,
                "notes.txt",
                "alpha\n",
                r#"{"file_path":"written.txt","content":"fresh\n"}"#,
            ),
            (
                Row::Patch,
                "notes.txt",
                "alpha\n",
                "*** Begin Patch\n*** Add File: patched.txt\n+fresh\n*** End Patch\n",
            ),
            (
                Row::Search,
                "notes.txt",
                "alpha\nbeta\n",
                r#"{"pattern":"beta"}"#,
            ),
        ];

        for (row, seed, contents, raw) in cases {
            let seeded = [(seed, contents)];
            let assembled_side = Side::of(&seeded);
            let native_side = Side::of(&seeded);
            let release = Release::of(row.package());
            let lock = ModulesLock::parse(
                &release.root().join("modules.lock"),
                &lock_text(row.key(), &release.entry),
            )
            .expect("lock");
            let packages =
                load_locked_modules(&lock, &release.manifest_file()).expect("the package loads");
            let services: ModuleServices = Arc::new(|module: &str, services: &ToolServices| {
                p1_host::catalog::capability_services_for(
                    module,
                    services.workspace.clone(),
                    services.observed.clone(),
                    None,
                )
            });
            let mut catalog = Catalog::new();
            let provider = ScriptedProvider::new(Vec::new());
            catalog.provider(
                "scripted",
                Box::new(move |_spec: &ProviderSpec| {
                    Ok(Arc::new(provider.clone()) as Arc<dyn Provider>)
                }),
            );
            register_modules(&mut catalog, packages, services).expect("registration");

            let environment = EnvironmentFile {
                name: format!("{}-module", row.key()),
                family: "test".into(),
                provider: "scripted".into(),
                model: "test-model".into(),
                profile: None,
                profile_text: None,
                options: ModelOptions::default(),
                tools: vec![ToolSpec {
                    module: row.key().into(),
                    name: None,
                    description: None,
                    variant: None,
                }],
                prompt_template: "tools: {{tool_names}}".into(),
                context: None,
                summarize_prompt: None,
                capabilities: Default::default(),
                tool_concurrency: Default::default(),
                instructions: Default::default(),
                skills: Default::default(),
            };
            let substitutions = Substitutions {
                workspace: "/work".into(),
                date: "2026-01-01".into(),
                os: "linux".into(),
                scratch: String::new(),
            };
            let assembled = assemble(
                &catalog,
                &environment,
                assembled_side.root.path(),
                &substitutions,
            )
            .unwrap_or_else(|error| panic!("{} assembles: {error}", row.key()));
            assert_eq!(assembled.tools.len(), 1, "{}", row.key());
            let tool = &assembled.tools[0];
            assert_eq!(tool.declaration().name, row.key());
            assert_eq!(
                assembled.resolved.tools[0].identity.implementation,
                row.module(),
                "the key {} selects {}",
                row.key(),
                row.module()
            );

            let call = ToolCall {
                call_id: "c1".into(),
                name: row.key().into(),
                input: match &tool.declaration().kind {
                    DeclarationKind::Function { .. } => ToolInput::Json(raw.to_owned()),
                    DeclarationKind::Freeform { .. } => ToolInput::Text(raw.to_owned()),
                },
            };
            let module = execute(tool.as_ref(), &call).await;
            // The assembled agent starts with its own, empty observations, so the reference
            // run must start from the same state: a refused call is refused on both sides.
            let native = redacted(
                row.native(&native_side.workspace, &ObservedFiles::new()),
                &Arc::new(MaskCounter::new()),
            );
            let expected = execute(native.as_ref(), &call).await;
            assert_eq!(
                (module.status, &module.content),
                (expected.status, &expected.content),
                "{} through the catalog",
                row.key()
            );
            assert_eq!(
                entries(assembled_side.root.path()),
                entries(native_side.root.path()),
                "the trees agree for {}",
                row.key()
            );
            if row == Row::Edit {
                // No observation of `notes.txt`: the host refuses the change under the gate.
                assert_eq!(module.status, ToolStatus::Error, "{}", module.content);
                assert_eq!(
                    module.content,
                    "You must read notes.txt before changing it."
                );
            } else {
                assert_eq!(module.status, ToolStatus::Ok, "{}", module.content);
            }
        }
    })
    .await;
}
