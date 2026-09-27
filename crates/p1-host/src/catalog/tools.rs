//! The standard tool entries of the catalog. Each tool's registration stays its own
//! block, so the stream that turns one tool into a module replaces only that block.
//!
//! Since S3.8 the `shell` and `finish` entries are official-release HOST ENTRIES
//! (ADR-0083, D083b, D-XO-49): `HOST_ENTRIES` lists the two keys beside `read`'s, and
//! [`register_composed_host_entries`] — the shared host-entry step — loads each package
//! (`p1/shell`, `p1/finish`) BY NAME from the installed release manifest, verifies it
//! against that same manifest and hands it to the registration this file owns, which
//! links it exactly the allocation and the services its manifest grants — `process` (the
//! native [`p1_tool_shell::ProcessService`] the host assembles from the workspace, the
//! environment snapshot, the `--env-pass` names and the sandbox) plus `clock` for
//! `shell`, and the completion hub's `completion` for `finish`. No native fallback: a
//! missing release or package fails the catalog build naming the module. What depends on
//! the host and not on the component — the sandbox paragraph and variant, the face, and
//! `finish`'s policy/contract declaration — the host presents here (ADR-0083 §1 and §2).

use std::path::PathBuf;
use std::sync::Arc;

use p1_assembly::{Catalog, ToolServices, ToolSpec};
use p1_contracts::Tool;
use p1_contracts::tool::{ResultDescription, ToolFace};
use p1_contracts::{
    BoxFuture, CallDescription, Effect, ToolCall, ToolContext, ToolDeclaration, ToolIdentity,
    ToolOutcome, ToolResultItem,
};
use p1_module_runtime::{ExecutionLimits, LoadedModule, Services, wasm_tool};

use crate::HostDeps;
use crate::activity::{AgentRole, CompletionHub, finish_component};
use crate::cli::SandboxMode;

use super::capabilities::NativeDeclaration;
use super::modules::{HostEntryRegistration, ModuleServices, register_composed_host_entries};

/// The catalog keys of the two host entries this file composes its own tool for (S3.8):
/// `HOST_ENTRIES` names the packages the release must ship for them.
const SHELL: &str = "shell";
const FINISH: &str = "finish";

/// The capability services a module tool is linked with, from the assembling agent's own:
/// the read side of its workspace and its observations (`p1-tool-read`'s service over
/// `p1-workspace`, S1.8). The credential files under the host's home are refused there
/// exactly as the native `read` refuses them (issue #142), so selecting the `p1/read`
/// package never widens what an agent can read. Each service is linked only where a
/// package's manifest grants it.
pub(super) fn module_services(deps: &HostDeps) -> ModuleServices {
    let home = deps.home.clone();
    Arc::new(move |_module: &str, services: &ToolServices| {
        p1_tool_read::capability_services(
            services.workspace.clone(),
            services.observed.clone(),
            home.clone(),
        )
    })
}

/// The semantic capabilities a still-native registration declares, each on the identity
/// implementation its constructor builds (`env!("CARGO_PKG_NAME")` of the tool crate). An
/// entry belongs to its tool's registration block and goes with it when that tool becomes
/// a package, whose manifest grants then decide. S3.8 removed the `shell` and `finish`
/// entries with their registrations — their capabilities now come from the selected
/// package's verified manifest through [`super::capabilities::package_capabilities`] — so
/// the array is empty until a future native registration declares here again.
pub(crate) const NATIVE_CAPABILITIES: [NativeDeclaration; 0] = [];

/// What the `shell` entry's factory assembles its process service from, fixed for one catalog
/// build: the sandbox selection and the paths and names it is built with (ADR-0035, ADR-0083 §1).
struct ShellSetup {
    /// Whether the command runs inside the bubblewrap boundary.
    sandbox: SandboxMode,
    /// The paths the sandbox binds writable, from `--sandbox-write`.
    writable: Vec<PathBuf>,
    /// The paths the sandbox binds readable, from `--sandbox-read`.
    readable: Vec<PathBuf>,
    /// The home the sandbox hides, from `HOME`.
    home: Option<PathBuf>,
    /// The runtime directory the sandbox replaces, from `XDG_RUNTIME_DIR`.
    runtime_dir: Option<PathBuf>,
    /// The environment snapshot commands are rebuilt from, from the host's deps.
    shell_env: Option<Vec<(std::ffi::OsString, std::ffi::OsString)>>,
    /// The names `--env-pass` lets through.
    env_pass: Vec<String>,
}

/// The registration of the `shell` host entry (S3.8): the loaded `p1/shell` component
/// linked with the host-assembled, sandboxed process service and presented under the
/// sandbox paragraph and the `+sandbox` variant, then the environment's face (ADR-0083
/// §1). The package is the one the shared step loaded from the release
/// ([`register_composed_host_entries`]), so a catalog compiles it once for its whole life
/// and the next catalog reads the release again.
fn shell_entry(
    deps: &HostDeps,
    sandbox: SandboxMode,
    sandbox_write: &[PathBuf],
    sandbox_read: &[PathBuf],
    env_pass: &[String],
) -> HostEntryRegistration {
    let setup = Arc::new(ShellSetup {
        sandbox,
        writable: sandbox_write.to_vec(),
        readable: sandbox_read.to_vec(),
        home: deps.home.clone(),
        runtime_dir: deps.runtime_dir.clone(),
        shell_env: deps.shell_env.clone(),
        env_pass: env_pass.to_vec(),
    });
    Box::new(move |catalog: &mut Catalog, module: Arc<LoadedModule>| {
        // The shell's semantic capability comes from the package's verified manifest grant
        // (`process`), never from a native declaration, which left with its registration.
        super::capabilities::declare_package(&module);
        let loaded = module;
        let setup = setup.clone();
        catalog.tool(
            SHELL,
            Box::new(move |spec: &ToolSpec, services: &ToolServices| {
                // The execution boundary is the native process service, assembled here and
                // fixed for this agent: a request carries only the command text and its time
                // limit (ADR-0083 §1). The sandbox is applied before the face, so a face
                // override keeps the sandbox paragraph and the `+sandbox` variant.
                let mut process = p1_tool_shell::ProcessService::new(services.workspace.root());
                if let Some(snapshot) = &setup.shell_env {
                    process = process.with_env_snapshot(snapshot.clone());
                }
                let process = process.with_env_pass(setup.env_pass.clone());
                let (process, sandboxed) = match setup.sandbox {
                    SandboxMode::Off => (process, false),
                    SandboxMode::Workspace => {
                        let Some(home) = setup.home.clone() else {
                            return Err(
                                "--sandbox workspace needs HOME to know which home to hide: set \
                                 HOME, or pass --sandbox off"
                                    .to_string(),
                            );
                        };
                        let mut sandbox = p1_tool_shell::Sandbox::for_home(home);
                        sandbox.readable = setup.readable.clone();
                        sandbox.writable = setup.writable.clone();
                        sandbox.runtime_dir = setup.runtime_dir.clone();
                        (
                            process
                                .sandboxed(sandbox)
                                .map_err(|error| error.to_string())?,
                            true,
                        )
                    }
                };
                let linked = Services {
                    process: Some(Arc::new(p1_tool_shell::ProcessCapability::new(Arc::new(
                        process,
                    )))),
                    ..Services::default()
                };
                let component =
                    wasm_tool(&loaded, linked, ExecutionLimits::default(), &services.mask)
                        .map_err(|error| error.to_string())?;
                Ok(apply_face!(FacedTool::new(component, sandboxed), spec))
            }),
        );
        Ok(())
    })
}

/// The registration of the `finish` host entry (S3.8): the loaded `p1/finish` component
/// over the host's completion hub, under the hub's gate and the declaration its policy and
/// output contract choose (ADR-0083 §2). The manifest grants `completion`, and the hub's
/// service is the only thing that can back it: a package granted a capability whose service
/// is absent fails assembly (`MissingService`) rather than running unlinked. The
/// assembly-time declaration is the MAIN agent's (the hub always gives a main agent
/// `recorded-commands`); a worker's policy and its output contract are applied by the hub at
/// that worker's assembly boundary (`catalog/children.rs`, rule 7).
fn finish_entry(completion: &Arc<CompletionHub>) -> HostEntryRegistration {
    let hub = completion.clone();
    Box::new(move |catalog: &mut Catalog, module: Arc<LoadedModule>| {
        // The finish's semantic capability comes from the package's verified manifest grant
        // (`completion`); nothing native declares it any more.
        super::capabilities::declare_package(&module);
        let loaded = module;
        let hub = hub.clone();
        catalog.tool(
            FINISH,
            Box::new(move |spec: &ToolSpec, services: &ToolServices| {
                hub.register_finish(&loaded);
                let completion = hub.issue();
                let grant = hub.grant(completion, &[], AgentRole::Main, None);
                let tool = finish_component(&loaded, &grant, None, &services.mask)
                    .map_err(|error| error.to_string())?;
                Ok(apply_face!(FacedTool::new(tool, false), spec))
            }),
        );
        Ok(())
    })
}

/// Presents a module tool under the host's presentation (ADR-0083 §1): the environment's
/// face, and for the shell the sandbox paragraph and the `+sandbox` variant. It follows the
/// native `ShellTool`'s two-step composition exactly — the sandbox first, then the
/// environment's face — so the assembled name, description and variant are byte-identical
/// to what the native entry produced. The identity's IMPLEMENTATION stays the loader's
/// (ADR-0087): the host moves only the variant it presents.
struct FacedTool {
    inner: Arc<dyn Tool>,
    /// The name and description BEFORE the sandbox paragraph, and the variant BEFORE the
    /// suffix, so composing the sandbox and a face in either order never stacks.
    face: ToolFace,
    variant: String,
    sandboxed: bool,
    declaration: ToolDeclaration,
    identity: ToolIdentity,
}

impl FacedTool {
    fn new(inner: Arc<dyn Tool>, sandboxed: bool) -> Self {
        let declaration = inner.declaration().clone();
        Self {
            face: ToolFace::new(declaration.name.clone(), declaration.description.clone()),
            variant: inner.identity().variant.clone(),
            sandboxed,
            declaration,
            identity: inner.identity().clone(),
            inner,
        }
        .composed()
    }

    /// Replace the presentation's name, description and variant; the sandbox paragraph and
    /// suffix survive the way they survive the native `ShellTool`'s `with_face`.
    fn with_face(self, face: ToolFace, variant: &str) -> Self {
        Self {
            face,
            variant: variant.to_string(),
            ..self
        }
        .composed()
    }

    /// Recompute the declaration and identity from the face, the variant and whether the
    /// sandbox is on. Called by every constructor, so the order of the sandbox and a face
    /// never doubles the paragraph or the suffix.
    fn composed(mut self) -> Self {
        let description = if self.sandboxed {
            format!(
                "{}\n{}",
                self.face.description,
                p1_tool_shell::SANDBOX_PARAGRAPH
            )
        } else {
            self.face.description.clone()
        };
        let variant = if self.sandboxed {
            format!("{}{}", self.variant, p1_tool_shell::SANDBOX_VARIANT_SUFFIX)
        } else {
            self.variant.clone()
        };
        self.declaration = ToolDeclaration {
            name: self.face.name.clone(),
            description,
            kind: self.inner.declaration().kind.clone(),
        };
        self.identity = ToolIdentity {
            implementation: self.inner.identity().implementation.clone(),
            variant,
        };
        self
    }
}

impl Tool for FacedTool {
    fn declaration(&self) -> &ToolDeclaration {
        &self.declaration
    }

    fn identity(&self) -> &ToolIdentity {
        &self.identity
    }

    fn effect(&self, call: &ToolCall) -> Effect {
        self.inner.effect(call)
    }

    fn describe(&self, call: &ToolCall) -> CallDescription {
        self.inner.describe(call)
    }

    fn describe_result(&self, call: &ToolCall, result: &ToolResultItem) -> ResultDescription {
        self.inner.describe_result(call, result)
    }

    fn execute<'a>(
        &'a self,
        call: &'a ToolCall,
        context: ToolContext,
    ) -> BoxFuture<'a, ToolOutcome> {
        self.inner.execute(call, context)
    }
}

pub(super) fn register_standard_tools(
    catalog: &mut Catalog,
    deps: &HostDeps,
    sandbox: SandboxMode,
    sandbox_write: &[PathBuf],
    sandbox_read: &[PathBuf],
    env_pass: &[String],
    completion: &Arc<CompletionHub>,
) -> Result<(), String> {
    catalog.tool(
        "edit",
        Box::new(|spec: &ToolSpec, services: &ToolServices| {
            Ok(apply_face!(
                p1_tool_edit::EditTool::new(services.workspace.clone(), services.observed.clone()),
                spec
            ))
        }),
    );
    catalog.tool(
        "write",
        Box::new(|spec: &ToolSpec, services: &ToolServices| {
            Ok(apply_face!(
                p1_tool_write::WriteTool::new(
                    services.workspace.clone(),
                    services.observed.clone()
                ),
                spec
            ))
        }),
    );
    catalog.tool(
        "grep",
        Box::new(|spec: &ToolSpec, services: &ToolServices| {
            Ok(apply_face!(
                p1_tool_search::GrepTool::new(services.workspace.clone()),
                spec
            ))
        }),
    );
    catalog.tool(
        "apply_patch",
        Box::new(|spec: &ToolSpec, services: &ToolServices| {
            Ok(apply_face!(
                p1_tool_patch::PatchTool::new(
                    services.workspace.clone(),
                    services.observed.clone()
                ),
                spec
            ))
        }),
    );
    // S3.8: the `shell` and `finish` entries are the release's `p1/shell` and `p1/finish` host
    // entries (ADR-0083, D083b, D-XO-49). They are listed in `HOST_ENTRIES` beside `read`'s, and
    // the one shared host-entry step loads each package from the release manifest, verifies it
    // against that same manifest — its class allocation included — and hands it to the
    // registration built here, so `load_release_module` is gone and a key a user lock names is
    // left to the locked-module registration exactly as `read` is.
    register_composed_host_entries(
        catalog,
        deps,
        &[
            (
                SHELL,
                shell_entry(deps, sandbox, sandbox_write, sandbox_read, env_pass),
            ),
            (FINISH, finish_entry(completion)),
        ],
    )
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use p1_module_runtime::{LinkError, ToolError};
    use p1_redact::MaskCounter;

    use super::*;
    use crate::catalog::modules::{HOST_ENTRIES, quiet_deps};

    const LOCKED_SHELL: &str = "format = \"p1-modules-lock/1\"\n\n[modules.shell]\n\
        package = \"p1/shell\"\nversion = \"0.0.1\"\ndigest = \"sha256:\
        0000000000000000000000000000000000000000000000000000000000000000\"\n\
        world = \"p1:module/tool@1.0.0\"\nprotocol = \"1.0\"\n";

    /// The release package `HOST_ENTRIES` names for `key`: the one the shared step loads.
    fn package_of(key: &str) -> &'static str {
        HOST_ENTRIES
            .iter()
            .find(|(entry, _)| *entry == key)
            .unwrap_or_else(|| panic!("`{key}` is one of the host entries"))
            .1
    }

    /// A user lock that names `shell` keeps the release's entry out, exactly as it does for
    /// `read` (S1.8.1): the shared step registers nothing here, and the locked-module
    /// registration is what the key runs.
    #[test]
    fn the_host_entry_stays_out_unless_a_lock_names_the_key() {
        let root = tempfile::tempdir().unwrap();
        let environments = root.path().join("environments");
        std::fs::create_dir_all(&environments).unwrap();
        let dirs = [environments];
        assert!(
            !super::super::modules::lock_selects(&dirs, SHELL),
            "no lock selects nothing"
        );

        std::fs::write(
            root.path().join("modules.lock"),
            "format = \"p1-modules-lock/1\"\n\n[modules]\n",
        )
        .unwrap();
        assert!(
            !super::super::modules::lock_selects(&dirs, SHELL),
            "the shipped empty lock"
        );

        std::fs::write(root.path().join("modules.lock"), LOCKED_SHELL).unwrap();
        assert!(super::super::modules::lock_selects(&dirs, SHELL));
        assert!(!super::super::modules::lock_selects(&dirs, "grep"));
    }

    /// One catalog compiles a shipped package at most once, and the assembly it serves is
    /// therefore fixed for that catalog's whole life: no package can be replaced under a
    /// turn in progress (ADR-0083 rule 7). A replacement is read by the NEXT generation,
    /// whose catalog is built again from the release and installed between complete turns
    /// (ADR-0078 §4, the machinery S5.7's reload suite covers) — never by the running one.
    ///
    /// The shared step loads the package and hands it to the entry's registration, so the
    /// catalog's `shell` entry holds that one compilation; the next catalog's step reads the
    /// release again and hands out its own.
    #[test]
    fn a_catalog_loads_its_package_once_and_the_next_catalog_reads_the_release_again() {
        let root = tempfile::tempdir().unwrap();
        let environments = root.path().join("environments");
        std::fs::create_dir_all(&environments).unwrap();
        let deps = quiet_deps(vec![environments]);
        let loaded = Arc::new(Mutex::new(Vec::new()));
        for _ in 0..2 {
            let recorded = loaded.clone();
            register_composed_host_entries(
                &mut Catalog::new(),
                &deps,
                &[(
                    SHELL,
                    Box::new(move |_catalog: &mut Catalog, module: Arc<LoadedModule>| {
                        recorded.lock().unwrap().push(module);
                        Ok(())
                    }),
                )],
            )
            .expect("the release's p1/shell loads");
        }
        let loaded = loaded.lock().unwrap();
        assert_eq!(
            loaded.len(),
            2,
            "one load per catalog, one hand-over per load"
        );
        let (one, replacement) = (&loaded[0], &loaded[1]);
        assert_eq!(one.identity().implementation, package_of(SHELL));
        assert!(
            !Arc::ptr_eq(one, replacement),
            "a new generation gets its own compilation of the release"
        );
    }

    /// A host entry's package is linked with exactly the services its manifest grants: the
    /// `p1/shell` manifest imports `process`, so linking the loaded package without the host's
    /// process service is a `MissingService`, never a component that runs unlinked. The
    /// completion half of the same rule is
    /// `crates/p1-module-tests/tests/finish_boundary.rs`, and the entry that DOES link the
    /// service is `tests/activation.rs`.
    #[tokio::test]
    async fn a_shell_service_that_is_absent_fails_assembly() {
        let root = tempfile::tempdir().unwrap();
        let environments = root.path().join("environments");
        std::fs::create_dir_all(&environments).unwrap();
        let deps = quiet_deps(vec![environments]);
        let shell = Arc::new(Mutex::new(None));
        let recorded = shell.clone();
        register_composed_host_entries(
            &mut Catalog::new(),
            &deps,
            &[(
                SHELL,
                Box::new(move |_catalog: &mut Catalog, module: Arc<LoadedModule>| {
                    *recorded.lock().unwrap() = Some(module);
                    Ok(())
                }),
            )],
        )
        .expect("the release's p1/shell loads");
        let module = shell
            .lock()
            .unwrap()
            .clone()
            .expect("the shell package was handed over");
        assert_eq!(module.identity().implementation, package_of(SHELL));
        match wasm_tool(
            &module,
            Services::default(),
            ExecutionLimits::default(),
            &Arc::new(MaskCounter::new()),
        ) {
            Err(ToolError::Link {
                source: LinkError::MissingService(capability),
                ..
            }) => assert_eq!(capability, "process"),
            Err(other) => panic!("wrong error: {other}"),
            Ok(_) => panic!("the shell must not link without a process service"),
        }
    }
}
