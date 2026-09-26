//! The standard tool entries of the catalog. Each tool's registration stays its own
//! block, so the stream that turns one tool into a module replaces only that block.
//!
//! Since S3.8 the `shell` and `finish` entries are official-release HOST ENTRIES
//! (ADR-0083, D083b): each loads its package (`p1/shell`, `p1/finish`) BY NAME from the
//! installed release manifest through the loader ([`super::modules::load_release_module`])
//! and links it exactly the allocation and the services its manifest grants — `process`
//! (the native [`p1_tool_shell::ProcessService`] the host assembles from the workspace, the
//! environment snapshot, the `--env-pass` names and the sandbox) plus `clock` for `shell`,
//! and the completion hub's `completion` for `finish`. No native fallback: a missing
//! release or package fails assembly naming the module. What depends on the host and not on
//! the component — the sandbox paragraph and variant, the face, and `finish`'s
//! policy/contract declaration — the host presents here (ADR-0083 §1 and §2).

use std::path::PathBuf;
use std::sync::{Arc, OnceLock};

use p1_assembly::{Catalog, ToolServices, ToolSpec, load_modules_lock};
use p1_contracts::tool::{ResultDescription, ToolFace};
use p1_contracts::{
    BoxFuture, CallDescription, Effect, Tool, ToolCall, ToolContext, ToolDeclaration, ToolIdentity,
    ToolOutcome, ToolResultItem,
};
use p1_module_runtime::{ExecutionLimits, LoadedModule, Services, wasm_tool};

use crate::HostDeps;
use crate::activity::{AgentRole, CompletionHub, finish_component};
use crate::cli::SandboxMode;

use super::capabilities::NativeDeclaration;
use super::modules::{ModuleServices, load_release_module};

/// The catalog key of the `read` tool, native or the `p1/read` package a lock selects.
const READ: &str = "read";

/// The manifest names of the two packages the host ships as release host entries.
const SHELL_PACKAGE: &str = "p1/shell";
const FINISH_PACKAGE: &str = "p1/finish";

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

/// Whether the `modules.lock` files next to `environment_dirs` name `key`. A lock that
/// cannot be read selects nothing here; the locked-module registration reports its error.
fn lock_selects(environment_dirs: &[PathBuf], key: &str) -> bool {
    load_modules_lock(environment_dirs)
        .is_ok_and(|lock| lock.iter().any(|(module, _)| module == key))
}

/// The semantic capabilities a still-native registration declares, each on the identity
/// implementation its constructor builds (`env!("CARGO_PKG_NAME")` of the tool crate). An
/// entry belongs to its tool's registration block and goes with it when that tool becomes
/// a package, whose manifest grants then decide. S3.8 removed the `shell` and `finish`
/// entries with their registrations — their capabilities now come from the selected
/// package's verified manifest through [`super::capabilities::package_capabilities`] — so
/// the array is empty until a future native registration declares here again.
pub(crate) const NATIVE_CAPABILITIES: [NativeDeclaration; 0] = [];

/// One tool package a release host entry loads once per catalog and assembles per agent.
type ReleaseFilter = OnceLock<Result<Arc<LoadedModule>, String>>;

/// Loads `package` once, declares its capabilities, and hands the same compiled module to
/// every later assembly; the loader's refusal (a missing release, a missing package or a
/// bad manifest) is reported once and then repeated, so no assembly ever silently falls
/// back to something native.
fn release_module(slot: &ReleaseFilter, package: &str) -> Result<Arc<LoadedModule>, String> {
    match slot.get_or_init(|| {
        let module = load_release_module(package)?;
        super::capabilities::declare_package(&module);
        Ok(Arc::new(module))
    }) {
        Ok(module) => Ok(module.clone()),
        Err(error) => Err(error.clone()),
    }
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
) {
    let read_home = deps.home.clone();
    // S1.8: a `modules.lock` entry named `read` selects a package for this key, which the
    // locked-module registration then registers (with `module_services` above). The native
    // tool stays the default: it is registered whenever no lock selects the key.
    if !lock_selects(&deps.environment_dirs, READ) {
        catalog.tool(
            READ,
            Box::new(move |spec: &ToolSpec, services: &ToolServices| {
                // Issue #142: `read` refuses the credential files under the agent's home
                // (the injected one in tests), whatever access it was granted.
                let tool = p1_tool_read::ReadTool::new(
                    services.workspace.clone(),
                    services.observed.clone(),
                )
                .with_home(read_home.clone());
                Ok(apply_face!(tool, spec))
            }),
        );
    }
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
    let choice = sandbox;
    let writable = sandbox_write.to_vec();
    let readable = sandbox_read.to_vec();
    let home = deps.home.clone();
    let runtime_dir = deps.runtime_dir.clone();
    let shell_env = deps.shell_env.clone();
    let env_pass = env_pass.to_vec();
    let shell = Arc::new(ReleaseFilter::new());
    catalog.tool(
        "shell",
        Box::new(move |spec: &ToolSpec, services: &ToolServices| {
            let module = release_module(&shell, SHELL_PACKAGE)?;
            // The execution boundary is the native process service, assembled here and
            // fixed for this agent: a request carries only the command text and its time
            // limit (ADR-0083 §1). The sandbox is applied before the face, so a face
            // override keeps the sandbox paragraph and the `+sandbox` variant.
            let mut process = p1_tool_shell::ProcessService::new(services.workspace.root());
            if let Some(snapshot) = &shell_env {
                process = process.with_env_snapshot(snapshot.clone());
            }
            let process = process.with_env_pass(env_pass.clone());
            let (process, sandboxed) = match choice {
                SandboxMode::Off => (process, false),
                SandboxMode::Workspace => {
                    let Some(home) = home.clone() else {
                        return Err(
                            "--sandbox workspace needs HOME to know which home to hide: set HOME, \
                             or pass --sandbox off"
                                .to_string(),
                        );
                    };
                    let mut sandbox = p1_tool_shell::Sandbox::for_home(home);
                    sandbox.readable = readable.clone();
                    sandbox.writable = writable.clone();
                    sandbox.runtime_dir = runtime_dir.clone();
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
            let component = wasm_tool(&module, linked, ExecutionLimits::default(), &services.mask)
                .map_err(|error| error.to_string())?;
            Ok(apply_face!(FacedTool::new(component, sandboxed), spec))
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
    // S3.8: the `finish` entry is the `p1/finish` component over the host's completion
    // hub (ADR-0083 §2). The manifest grants `completion`, and the hub's service is the
    // only thing that can back it: a package granted a capability whose service is absent
    // fails assembly (`MissingService`) rather than running unlinked. The assembly-time
    // declaration is the MAIN agent's (the hub always gives a main agent
    // `recorded-commands`); a worker's policy and its output contract are applied by the
    // hub at that worker's assembly boundary (`catalog/children.rs`, rule 7).
    let finish = Arc::new(ReleaseFilter::new());
    let hub = completion.clone();
    catalog.tool(
        "finish",
        Box::new(move |spec: &ToolSpec, services: &ToolServices| {
            let module = release_module(&finish, FINISH_PACKAGE)?;
            hub.register_finish(&module);
            let completion = hub.issue();
            let grant = hub.grant(completion, &[], AgentRole::Main, None);
            let tool = finish_component(&module, &grant, None, &services.mask)
                .map_err(|error| error.to_string())?;
            Ok(apply_face!(FacedTool::new(tool, false), spec))
        }),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    const LOCKED_READ: &str = "format = \"p1-modules-lock/1\"\n\n[modules.read]\n\
        package = \"p1/read\"\nversion = \"0.0.1\"\ndigest = \"sha256:\
        0000000000000000000000000000000000000000000000000000000000000000\"\n\
        world = \"p1:module/tool@1.0.0\"\nprotocol = \"1.0\"\n";

    #[test]
    fn the_native_read_stays_unless_a_lock_names_the_read_key() {
        let root = tempfile::tempdir().unwrap();
        let environments = root.path().join("environments");
        std::fs::create_dir_all(&environments).unwrap();
        let dirs = [environments];
        assert!(!lock_selects(&dirs, READ), "no lock selects nothing");

        std::fs::write(
            root.path().join("modules.lock"),
            "format = \"p1-modules-lock/1\"\n\n[modules]\n",
        )
        .unwrap();
        assert!(!lock_selects(&dirs, READ), "the shipped empty lock");

        std::fs::write(root.path().join("modules.lock"), LOCKED_READ).unwrap();
        assert!(lock_selects(&dirs, READ));
        assert!(!lock_selects(&dirs, "grep"));
    }

    /// One catalog compiles a shipped package at most once, and the assembly it serves is
    /// therefore fixed for that catalog's whole life: no package can be replaced under a
    /// turn in progress (ADR-0083 rule 7). A replacement is read by the NEXT generation,
    /// whose catalog is built again from the release and installed between complete turns
    /// (ADR-0078 §4, the machinery S5.7's reload suite covers) — never by the running one.
    #[test]
    fn a_catalog_loads_its_package_once_and_the_next_catalog_reads_the_release_again() {
        let first = Arc::new(ReleaseFilter::new());
        let one = release_module(&first, SHELL_PACKAGE).expect("the shell package loads");
        let again = release_module(&first, SHELL_PACKAGE).expect("the cached shell package");
        assert!(
            Arc::ptr_eq(&one, &again),
            "the catalog reuses the package it already compiled"
        );
        assert_eq!(one.identity().implementation, SHELL_PACKAGE);
        let second = Arc::new(ReleaseFilter::new());
        let replacement =
            release_module(&second, SHELL_PACKAGE).expect("a new catalog loads it again");
        assert!(
            !Arc::ptr_eq(&one, &replacement),
            "a new generation gets its own compilation of the release"
        );
    }
}
