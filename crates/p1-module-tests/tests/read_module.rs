//! S1.8: the `read` tool as the `p1/read` component (ADR-0071), built by
//! `scripts/build-modules.sh --package p1-module-read`, loaded by name through the runtime
//! loader and run by the real host loader over the native `workspace` and `snapshot`
//! services of `p1-tool-read`. Every case runs the native `ReadTool` and the component over
//! the same scratch workspace and requires the same outcome, byte for byte: the component's
//! output is identical to the native tool's.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use p1_assembly::{
    Catalog, EnvironmentFile, ModulesLock, ProviderSpec, Substitutions, ToolServices, ToolSpec,
    assemble,
};
use p1_contracts::serde_json::{self, Value, json};
use p1_contracts::{
    CancellationToken, ModelOptions, Provider, Tool, ToolCall, ToolContext, ToolInput, ToolOutcome,
    ToolResultItem, ToolStatus,
};
use p1_host::catalog::modules::{ModuleServices, load_locked_modules, register_modules};
use p1_module_runtime::{ExecutionLimits, Loader, ReleaseManifest, wasm_tool};
use p1_module_tests::{lock_text, within_deadline};
use p1_redact::MaskCounter;
use p1_testkit::ScriptedProvider;
use p1_tool_read::{ReadTool, capability_services};
use p1_workspace::{Observation, ObservedFiles, Workspace};

/// The package directory and the manifest name of the component.
const PACKAGE: &str = "p1-module-read";
const NAME: &str = "p1/read";

/// A release directory holding only the component, laid out as p1's release archive ships
/// it: its own, so these cases need no other package built.
struct ReadRelease {
    dir: tempfile::TempDir,
}

impl ReadRelease {
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
}

/// The component as `scripts/build-modules.sh` published it, in a release of its own, and
/// its release entry.
fn read_release() -> (ReadRelease, Value) {
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
    let release = ReadRelease {
        dir: tempfile::tempdir().expect("release dir"),
    };
    let component = release
        .root()
        .join(entry["path"].as_str().expect("entry path"));
    std::fs::create_dir_all(component.parent().expect("package dir")).expect("package dir");
    std::fs::write(&component, &wasm).expect("component file");
    let listing = json!({
        "format": "p1-release-manifest/1",
        "components": [entry.clone()],
    });
    std::fs::write(release.manifest_file(), listing.to_string()).expect("manifest");
    (release, entry)
}

/// The native tool and the component over one workspace, each with its own observations.
struct Pair {
    native: ReadTool,
    native_observed: ObservedFiles,
    module: Arc<dyn Tool>,
    module_observed: ObservedFiles,
    _release: ReadRelease,
}

/// Both tools over `root`, refusing the credential files under `home`, as the host builds
/// them: the native `read` with `with_home`, the component with the host's services.
fn both_tools(root: &Path, home: Option<&Path>) -> Pair {
    let workspace = Workspace::new(root).expect("workspace");
    let home = home.map(Path::to_path_buf);
    let native_observed = ObservedFiles::new();
    let native = ReadTool::new(workspace.clone(), native_observed.clone()).with_home(home.clone());
    let module_observed = ObservedFiles::new();
    let (release, _) = read_release();
    let loaded = release.loader().load(NAME).expect("p1/read loads by name");
    assert_eq!(loaded.identity().implementation, NAME);
    assert_eq!(loaded.identity().variant, "claude");
    let module = wasm_tool(
        &loaded,
        capability_services(workspace, module_observed.clone(), home),
        ExecutionLimits::default(),
        &Arc::new(MaskCounter::new()),
    )
    .expect("the component is a tool");
    Pair {
        native,
        native_observed,
        module,
        module_observed,
        _release: release,
    }
}

fn call(arguments: &str) -> ToolCall {
    ToolCall {
        call_id: "c1".into(),
        name: "read".into(),
        input: ToolInput::Json(arguments.to_owned()),
    }
}

fn file_call(path: &str) -> String {
    json!({ "file_path": path }).to_string()
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
    /// Runs `arguments` through both tools and requires the same outcome, and the same
    /// description of the call and of its result.
    async fn same(&self, arguments: &str) -> ToolOutcome {
        let call = call(arguments);
        let native = execute(&self.native, &call).await;
        let module = execute(self.module.as_ref(), &call).await;
        assert_eq!(module.status, native.status, "status of {arguments}");
        assert_eq!(module.content, native.content, "content of {arguments}");

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

// `WasmTool` starts its executor on the current Tokio runtime, and the native tool's file
// work runs on its blocking pool: every case runs inside a multi-threaded runtime.

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_component_declares_what_the_native_tool_declares() {
    within_deadline("declaration", async {
        let dir = tempfile::tempdir().unwrap();
        let pair = both_tools(dir.path(), None);
        assert_eq!(pair.module.declaration(), pair.native.declaration());
        let probe = call(&file_call("a.txt"));
        assert_eq!(pair.module.effect(&probe), pair.native.effect(&probe));
        for invalid in ["not json", "{}", r#"{"file_path":"a","offset":0}"#] {
            assert_eq!(
                pair.module.describe(&call(invalid)),
                pair.native.describe(&call(invalid))
            );
        }
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn whole_file_windows_and_long_files_render_identically() {
    within_deadline("windows", async {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join("a.txt"), "alpha\nbeta\r\ngamma\n").unwrap();
        std::fs::write(root.join("no-newline.txt"), "one\ntwo").unwrap();
        // More lines than one window shows, and more bytes than one host read carries.
        let many: String = (1..=3_000)
            .map(|n| format!("line {n:05} of a long file\n"))
            .collect();
        std::fs::write(root.join("many.txt"), &many).unwrap();
        // One line longer than the whole output bound.
        std::fs::write(root.join("wide.txt"), "x".repeat(60_000)).unwrap();
        std::fs::write(root.join("empty.txt"), "").unwrap();
        std::fs::write(root.join("nul.dat"), b"alpha\0beta").unwrap();
        std::fs::write(root.join("bad.dat"), [b'a', 0xFF, b'b']).unwrap();
        let pair = both_tools(root, None);

        let whole = pair.same(&file_call("a.txt")).await;
        assert_eq!(whole.status, ToolStatus::Ok);
        assert_eq!(whole.content, "     1\talpha\n     2\tbeta\n     3\tgamma");
        pair.same(&file_call("no-newline.txt")).await;
        pair.same(&file_call("./a.txt")).await;

        let window = pair
            .same(r#"{"file_path": "many.txt", "offset": 2500, "limit": 3}"#)
            .await;
        assert_eq!(
            window.content,
            "  2500\tline 02500 of a long file\n  2501\tline 02501 of a long file\n  \
             2502\tline 02502 of a long file\n[498 more lines; continue with offset=2503]"
        );
        // The byte bound stops the window before the line bound does.
        let long = pair.same(&file_call("many.txt")).await;
        let footer = long.content.lines().last().unwrap_or_default();
        assert!(
            footer.starts_with('[') && footer.contains("more lines; continue with offset="),
            "{footer}"
        );
        assert!(long.content.len() <= 50_100, "{}", long.content.len());
        pair.same(r#"{"file_path": "many.txt", "limit": 1}"#).await;

        let wide = pair.same(&file_call("wide.txt")).await;
        assert!(
            wide.content.contains("[output truncated: showing"),
            "{}",
            wide.content
        );

        for (arguments, expected) in [
            (file_call("empty.txt"), "empty.txt is empty."),
            (file_call("nul.dat"), "nul.dat is a binary file."),
            (file_call("bad.dat"), "bad.dat is not valid UTF-8."),
            (
                r#"{"file_path": "a.txt", "offset": 9}"#.to_owned(),
                "offset 9 is beyond the end of a.txt (3 lines).",
            ),
        ] {
            let outcome = pair.same(&arguments).await;
            assert_eq!(outcome.content, expected, "{arguments}");
        }
        for invalid in [
            "",
            "[]",
            r#"{"file_path": 5}"#,
            r#"{"file_path":"a.txt","limit":0}"#,
        ] {
            let outcome = pair.same(invalid).await;
            assert_eq!(outcome.status, ToolStatus::Error, "{invalid}");
        }
    })
    .await;
}

/// The lead check of `crates/p1-tool-read/tests/lead_multibyte.rs`, over both tools: valid
/// multi-byte text longer than every buffer is accepted wherever the chunk and window
/// boundaries fall, and an invalid byte after a split character is still rejected.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn multibyte_text_across_every_boundary_is_read_identically() {
    within_deadline("multibyte", async {
        let dir = tempfile::tempdir().unwrap();
        let pair = both_tools(dir.path(), None);
        for shift in 0..3 {
            let long_line = format!("{}{}\n", "a".repeat(shift), "€".repeat(100_000));
            std::fs::write(dir.path().join("long.txt"), &long_line).unwrap();
            assert_eq!(
                pair.same(&file_call("long.txt")).await.status,
                ToolStatus::Ok
            );

            let many_lines = format!("{}{}", "a".repeat(shift), "€€€€€€€€€\n".repeat(30_000));
            std::fs::write(dir.path().join("lines.txt"), &many_lines).unwrap();
            assert_eq!(
                pair.same(&file_call("lines.txt")).await.status,
                ToolStatus::Ok
            );

            let mut bad = format!("{}{}", "a".repeat(shift), "€".repeat(100_000)).into_bytes();
            bad.push(0xFF);
            bad.extend_from_slice("€€€\n".as_bytes());
            std::fs::write(dir.path().join("bad.txt"), &bad).unwrap();
            let outcome = pair.same(&file_call("bad.txt")).await;
            assert_eq!(outcome.status, ToolStatus::Error, "shift {shift}");
            assert!(outcome.content.contains("not valid UTF-8"), "{outcome:?}");
        }
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn missing_files_directories_and_paths_outside_are_refused_identically() {
    within_deadline("refusals", async {
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("subdir")).unwrap();
        std::fs::write(outside.path().join("secret.txt"), "secret").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(outside.path(), dir.path().join("link")).unwrap();
        let pair = both_tools(dir.path(), None);

        let missing = pair.same(&file_call("nope.txt")).await;
        assert_eq!(missing.content, "nope.txt does not exist.");
        pair.same(&file_call("subdir/../nope.txt")).await;
        let directory = pair.same(&file_call("subdir")).await;
        assert_eq!(directory.content, "subdir is not a regular file.");

        let escaped = pair.same(&file_call("../secret.txt")).await;
        assert!(escaped.content.contains("escapes workspace"), "{escaped:?}");
        let absolute = outside.path().join("secret.txt");
        let absolute = pair.same(&file_call(absolute.to_str().unwrap())).await;
        assert!(
            absolute.content.contains("escapes workspace"),
            "{absolute:?}"
        );
        #[cfg(unix)]
        {
            let linked = pair.same(&file_call("link/secret.txt")).await;
            assert!(linked.content.contains("escapes workspace"), "{linked:?}");
        }
    })
    .await;
}

/// The credential files `read` refuses, relative to the home it was given (issue #142).
const CREDENTIAL_PATHS: [&str; 7] = [
    ".config/p1/auth.json",
    ".config/keys/tool.key",
    ".config/keys/nested/deeper.key",
    ".codex/auth.json",
    ".claude/.credentials.json",
    ".local/share/opencode/auth.json",
    ".pi/agent/auth.json",
];

fn home_with_credentials() -> tempfile::TempDir {
    let home = tempfile::tempdir().unwrap();
    for relative in CREDENTIAL_PATHS {
        let path = home.path().join(relative);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "{}\n").unwrap();
    }
    std::fs::write(home.path().join("notes.txt"), "alpha\nbeta\n").unwrap();
    home
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_host_passes_the_credential_refusal_into_the_workspace_service() {
    within_deadline("credentials", async {
        // The workspace IS the home, so a relative credential path is inside the
        // confinement and only the refusal can stop it.
        let home = home_with_credentials();
        let pair = both_tools(home.path(), Some(home.path()));
        for relative in CREDENTIAL_PATHS {
            let outcome = pair.same(&file_call(relative)).await;
            assert_eq!(outcome.status, ToolStatus::Error, "{relative}");
            assert!(
                outcome.content.contains("read refuses credential files")
                    && outcome
                        .content
                        .contains("credentials never enter the model's context"),
                "{relative}: {}",
                outcome.content
            );
        }
        assert_eq!(
            pair.module_observed
                .check_unchanged(&home.path().join(".codex/auth.json"), b"{}\n"),
            Observation::NeverObserved
        );
        let ordinary = pair.same(&file_call("notes.txt")).await;
        assert_eq!(ordinary.content, "     1\talpha\n     2\tbeta");

        // A workspace elsewhere: the refusal comes before confinement, for both.
        let elsewhere = tempfile::tempdir().unwrap();
        let pair = both_tools(elsewhere.path(), Some(home.path()));
        let absolute = home.path().join(".codex/auth.json");
        let outcome = pair.same(&file_call(absolute.to_str().unwrap())).await;
        assert!(
            outcome.content.contains("read refuses credential files"),
            "{}",
            outcome.content
        );
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_successful_read_is_observed_and_a_failed_one_is_not() {
    within_deadline("observation", async {
        let dir = tempfile::tempdir().unwrap();
        let contents = "alpha\nbeta\ngamma\n";
        let many: String = (1..=5_000).map(|n| format!("row {n}\n")).collect();
        std::fs::write(dir.path().join("a.txt"), contents).unwrap();
        std::fs::write(dir.path().join("many.txt"), &many).unwrap();
        std::fs::write(dir.path().join("empty.txt"), "").unwrap();
        std::fs::write(dir.path().join("nul.dat"), b"a\0b").unwrap();
        let pair = both_tools(dir.path(), None);
        let observations = |path: &str, bytes: &[u8]| {
            let path = dir.path().join(path);
            (
                pair.native_observed.check_unchanged(&path, bytes),
                pair.module_observed.check_unchanged(&path, bytes),
            )
        };

        // A windowed read observes the WHOLE file, several host reads long.
        pair.same(r#"{"file_path": "a.txt", "limit": 1}"#).await;
        pair.same(r#"{"file_path": "many.txt", "offset": 10, "limit": 1}"#)
            .await;
        pair.same(&file_call("empty.txt")).await;
        pair.same(&file_call("nul.dat")).await;

        let unchanged = (Observation::Unchanged, Observation::Unchanged);
        assert_eq!(observations("a.txt", contents.as_bytes()), unchanged);
        assert_eq!(observations("many.txt", many.as_bytes()), unchanged);
        assert_eq!(observations("empty.txt", b""), unchanged);
        assert_eq!(
            observations("a.txt", b"alpha\n"),
            (
                Observation::ChangedSinceObserved,
                Observation::ChangedSinceObserved
            )
        );
        assert_eq!(
            observations("nul.dat", b"a\0b"),
            (Observation::NeverObserved, Observation::NeverObserved),
            "a refused read observes nothing"
        );
    })
    .await;
}

/// The component loaded by name through the host's catalog entry point: a `modules.lock`
/// entry named `read` selects `p1/read`, and an environment naming `read` assembles it with
/// the services the host builds (`p1_tool_read::capability_services` over the agent's own
/// workspace and observations, with the host's home).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_read_key_assembles_the_component_through_the_catalog() {
    within_deadline("catalog", async {
        let (release, entry) = read_release();
        let lock = ModulesLock::parse(
            &release.root().join("modules.lock"),
            &lock_text("read", &entry),
        )
        .expect("lock");
        let packages = load_locked_modules(&lock, &release.manifest_file()).expect("p1/read loads");
        let home = home_with_credentials();
        let host_home = Some(home.path().to_path_buf());
        let services: ModuleServices = Arc::new(move |_module: &str, services: &ToolServices| {
            capability_services(
                services.workspace.clone(),
                services.observed.clone(),
                host_home.clone(),
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
            name: "read-module".into(),
            family: "test".into(),
            provider: "scripted".into(),
            model: "test-model".into(),
            profile: None,
            options: ModelOptions::default(),
            tools: vec![ToolSpec {
                module: "read".into(),
                name: None,
                description: None,
                variant: None,
            }],
            prompt_template: "tools: {{tool_names}}".into(),
            context: None,
            summarize_prompt: None,
        };
        let substitutions = Substitutions {
            workspace: "/work".into(),
            date: "2026-01-01".into(),
            os: "linux".into(),
        };
        let assembled =
            assemble(&catalog, &environment, home.path(), &substitutions).expect("assembles");
        assert_eq!(assembled.tools.len(), 1);
        let read = &assembled.tools[0];
        assert_eq!(read.declaration().name, "read");
        assert_eq!(assembled.resolved.tools[0].identity.implementation, NAME);

        let native = ReadTool::new(Workspace::new(home.path()).unwrap(), ObservedFiles::new())
            .with_home(Some(home.path().to_path_buf()));
        for arguments in [file_call("notes.txt"), file_call(".codex/auth.json")] {
            let call = call(&arguments);
            let module = execute(read.as_ref(), &call).await;
            let expected = execute(&native, &call).await;
            assert_eq!(
                (module.status, module.content),
                (expected.status, expected.content),
                "{arguments}"
            );
        }
    })
    .await;
}
