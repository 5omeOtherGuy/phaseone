//! The standard tool entries of the catalog. Each tool's registration stays its own
//! block, so the stream that turns one tool into a module replaces only that block.

use std::path::PathBuf;
use std::sync::Arc;

use p1_assembly::{Catalog, ToolServices, ToolSpec, load_modules_lock};
use p1_contracts::Tool;

use crate::HostDeps;
use crate::activity::CompletionHub;
use crate::cli::SandboxMode;

use super::capabilities::{Capabilities, NativeDeclaration, SemanticCapability};
use super::modules::ModuleServices;

/// The catalog key of the `read` tool, native or the `p1/read` package a lock selects.
const READ: &str = "read";

/// The capability services a module tool is linked with, from the assembling agent's own:
/// the read side of its workspace and its observations (`p1-tool-read`'s service over
/// `p1-workspace`, S1.8). The credential files under the host's home are refused there
/// exactly as the native `read` refuses them (issue #142), so selecting the `p1/read`
/// package never widens what an agent can read. Each service is linked only where a
/// package's manifest grants it.
pub(super) fn module_services(deps: &HostDeps) -> ModuleServices {
    let home = deps.home.clone();
    Arc::new(move |services: &ToolServices| {
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
}
