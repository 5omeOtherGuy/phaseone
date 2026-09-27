//! The standard tool entries of the catalog. Each tool's registration stays its own
//! block, so the stream that turns one tool into a module replaces only that block.

use std::path::PathBuf;
use std::sync::Arc;

use p1_assembly::{Catalog, ToolServices, ToolSpec, load_modules_lock};
use p1_contracts::Tool;
use p1_module_runtime::Services;
use p1_workspace::{MutationPolicy, ObservedFiles, Workspace};

use crate::HostDeps;
use crate::activity::CompletionHub;
use crate::cli::SandboxMode;

use super::capabilities::{Capabilities, NativeDeclaration, SemanticCapability};
use super::modules::ModuleServices;

/// The catalog key of the `read` tool, native or the `p1/read` package a lock selects.
const READ: &str = "read";
/// The catalog key of the `edit` tool, native or the `p1/edit` package a lock selects.
const EDIT: &str = "edit";
/// The catalog key of the `write` tool, native or the `p1/write` package a lock selects.
const WRITE: &str = "write";
/// The catalog key of the `apply_patch` tool, native or the `p1/patch` package a lock
/// selects.
const PATCH: &str = "apply_patch";
/// The catalog key of the `grep` tool, native or the `p1/search` package a lock selects.
const SEARCH: &str = "grep";

/// The capability services a module tool is linked with, from the assembling agent's own:
/// the read side of its workspace and its observations (`p1-tool-read`'s service over
/// `p1-workspace`, S1.8), the search tool's walk beside it, and — for the mutating rows —
/// the owned mutation with the mode the row grants (S2). The credential files under the
/// host's home are refused there exactly as the native `read` refuses them (issue #142), so
/// selecting a package never widens what an agent can read. Each service is linked only
/// where a package's manifest grants it.
///
/// The hook keys on the module identity the loader verified (`p1/edit`, `p1/patch`, …), so
/// a lock entry can serve another key of the same package without moving a grant.
pub(super) fn module_services(deps: &HostDeps) -> ModuleServices {
    let home = deps.home.clone();
    Arc::new(move |module: &str, services: &ToolServices| {
        capability_services_for(
            module,
            services.workspace.clone(),
            services.observed.clone(),
            home.clone(),
        )
    })
}

/// The mutation mode the catalog row of the module `module` (its verified manifest name,
/// e.g. `p1/edit`) assembles: observed for the edit and write components, patch-authorized
/// for patch, and none for a tool that never mutates
/// (`docs/design/modules/workspace-mutation.md`, the per-tool table; ADR-0088 point 4).
pub fn mutation_mode(module: &str) -> Option<MutationPolicy> {
    match module {
        "p1/edit" | "p1/write" => Some(MutationPolicy::Observed),
        "p1/patch" => Some(MutationPolicy::PatchAuthorized),
        _ => None,
    }
}

/// The capability services the locked tool module `module` is linked with, from the
/// assembling agent's own workspace and observations: the catalog row's grants. The search
/// component is linked the search capability alone (its manifest grants the walk and no
/// snapshot); every other tool gets the read side, with the mutation its row grants. Public
/// so the module acceptance suite links a component exactly as the host does.
pub fn capability_services_for(
    module: &str,
    workspace: Workspace,
    observed: ObservedFiles,
    home: Option<PathBuf>,
) -> Services {
    if module == "p1/search" {
        return p1_tool_search::search_services(workspace);
    }
    p1_tool_read::tool_services(workspace, observed, home, mutation_mode(module))
}

/// Whether the `modules.lock` files next to `environment_dirs` name `key`. A lock that
/// cannot be read selects nothing here; the locked-module registration reports its error.
fn lock_selects(environment_dirs: &[PathBuf], key: &str) -> bool {
    load_modules_lock(environment_dirs)
        .is_ok_and(|lock| lock.iter().any(|(module, _)| module == key))
}

/// The semantic capabilities the still-native registrations below declare, each on the
/// identity implementation its constructor builds (`env!("CARGO_PKG_NAME")` of the tool
/// crate). An entry belongs to its tool's registration block and goes with it when
/// that tool becomes a package, whose manifest grants then decide.
pub(crate) const NATIVE_CAPABILITIES: [NativeDeclaration; 2] = [
    // `shell` (S3): every run's outcome carries the command and its exit code.
    NativeDeclaration {
        implementation: "p1-tool-shell",
        capabilities: Capabilities::of(&[SemanticCapability::RecordsCommandEvidence]),
    },
    // `finish` (S3): an accepted call ends the turn with the completion report.
    NativeDeclaration {
        implementation: "p1-tool-finish",
        capabilities: Capabilities::of(&[SemanticCapability::ReportsCompletion]),
    },
];

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
    // S2: as `read` for S1.8, a `modules.lock` entry named `edit`, `write`, `apply_patch`
    // or `grep` selects the package of that key (`p1/edit`, `p1/write`, `p1/patch`,
    // `p1/search`), which the locked-module registration then registers. The native tool
    // stays the fallback: it is registered whenever no lock selects the key (D083b keeps
    // the four fallbacks until the cutover audit changes).
    if !lock_selects(&deps.environment_dirs, EDIT) {
        catalog.tool(
            EDIT,
            Box::new(|spec: &ToolSpec, services: &ToolServices| {
                Ok(apply_face!(
                    p1_tool_edit::EditTool::new(
                        services.workspace.clone(),
                        services.observed.clone()
                    ),
                    spec
                ))
            }),
        );
    }
    if !lock_selects(&deps.environment_dirs, WRITE) {
        catalog.tool(
            WRITE,
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
    }
    if !lock_selects(&deps.environment_dirs, SEARCH) {
        catalog.tool(
            SEARCH,
            Box::new(|spec: &ToolSpec, services: &ToolServices| {
                Ok(apply_face!(
                    p1_tool_search::GrepTool::new(services.workspace.clone()),
                    spec
                ))
            }),
        );
    }
    let choice = sandbox;
    let writable = sandbox_write.to_vec();
    let readable = sandbox_read.to_vec();
    let home = deps.home.clone();
    let runtime_dir = deps.runtime_dir.clone();
    let shell_env = deps.shell_env.clone();
    let env_pass = env_pass.to_vec();
    catalog.tool(
        "shell",
        Box::new(move |spec: &ToolSpec, services: &ToolServices| {
            let mut tool = p1_tool_shell::ShellTool::new(services.workspace.clone());
            if let Some(snapshot) = &shell_env {
                tool = tool.with_env_snapshot(snapshot.clone());
            }
            let tool = tool.with_env_pass(env_pass.clone());
            // The sandbox is applied BEFORE `apply_face!`, so a face override keeps
            // the sandbox paragraph and the `+sandbox` variant.
            let tool = match choice {
                SandboxMode::Off => tool,
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
                    tool.sandboxed(sandbox).map_err(|error| error.to_string())?
                }
            };
            Ok(apply_face!(tool, spec))
        }),
    );
    if !lock_selects(&deps.environment_dirs, PATCH) {
        catalog.tool(
            PATCH,
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
    }
    // `finish` owns no files or processes: it reads the session through the log
    // the host feeds from the event stream. Each assembly gets its own pair.
    let hub = completion.clone();
    catalog.tool(
        "finish",
        Box::new(move |spec: &ToolSpec, _services: &ToolServices| {
            let completion = hub.issue();
            let tool = apply_face!(
                p1_tool_finish::FinishTool::new(completion.log.clone(), completion.outcome.clone()),
                spec
            );
            completion
                .log
                .set_finish_name(tool.declaration().name.clone());
            Ok(tool)
        }),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A lock naming one module, as an installation writes it.
    fn lock_naming(module: &str, package: &str) -> String {
        format!(
            "format = \"p1-modules-lock/1\"\n\n[modules.{module}]\npackage = \"{package}\"\n\
             version = \"0.0.1\"\ndigest = \"sha256:\
             0000000000000000000000000000000000000000000000000000000000000000\"\n\
             world = \"p1:module/tool@1.0.0\"\nprotocol = \"1.0\"\n"
        )
    }

    #[test]
    fn the_native_file_tools_stay_unless_a_lock_names_their_key() {
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
        for key in [READ, EDIT, WRITE, PATCH, SEARCH] {
            assert!(!lock_selects(&dirs, key), "the shipped empty lock: {key}");
        }

        // S1.8's read and S2's four keys, each its own lock entry.
        for (key, package) in [
            (READ, "p1/read"),
            (EDIT, "p1/edit"),
            (WRITE, "p1/write"),
            (PATCH, "p1/patch"),
            (SEARCH, "p1/search"),
        ] {
            std::fs::write(root.path().join("modules.lock"), lock_naming(key, package)).unwrap();
            assert!(lock_selects(&dirs, key), "{key} is selected");
            for other in [READ, EDIT, WRITE, PATCH, SEARCH] {
                if other != key {
                    assert!(
                        !lock_selects(&dirs, other),
                        "{other} is not selected by {key}"
                    );
                }
            }
        }

        // A lock entry of another tool (`shell` stays native in S2) selects none of the five.
        std::fs::write(
            root.path().join("modules.lock"),
            lock_naming("shell", "p1/shell"),
        )
        .unwrap();
        for key in [READ, EDIT, WRITE, PATCH, SEARCH] {
            assert!(!lock_selects(&dirs, key), "{key} stays native");
        }
    }

    #[test]
    fn the_mutation_mode_is_the_row_of_the_component() {
        assert_eq!(mutation_mode("p1/edit"), Some(MutationPolicy::Observed));
        assert_eq!(mutation_mode("p1/write"), Some(MutationPolicy::Observed));
        assert_eq!(
            mutation_mode("p1/patch"),
            Some(MutationPolicy::PatchAuthorized)
        );
        // The read component links no mutation, and neither does any other module: a
        // component is granted only what its own row assembles.
        for module in ["p1/read", "p1/search", "p1/worker-start", "p1/summary"] {
            assert_eq!(mutation_mode(module), None, "{module}");
        }
    }
}
