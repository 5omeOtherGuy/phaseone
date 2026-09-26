//! The standard tool entries of the catalog. Each tool's registration stays its own
//! block, so the stream that turns one tool into a module replaces only that block.

use std::path::PathBuf;
use std::sync::Arc;

use p1_assembly::{Catalog, ToolServices, ToolSpec};
use p1_contracts::Tool;

use crate::HostDeps;
use crate::activity::CompletionHub;
use crate::cli::SandboxMode;

use super::capabilities::{Capabilities, NativeDeclaration, SemanticCapability};
use super::modules::ModuleServices;

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
    // S1.8.1 (D083b 2): `read` is the release's host entry `p1/read`, registered by
    // `catalog::modules::register_host_entries` and loaded from the release manifest; the native
    // registration that stood here is gone. A `modules.lock` entry named `read` still selects a
    // package through the locked-module registration, which wins over the host entry.
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
