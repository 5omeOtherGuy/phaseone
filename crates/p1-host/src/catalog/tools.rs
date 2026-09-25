//! The standard tool entries of the catalog. Each tool's registration stays its own
//! block, so the stream that turns one tool into a module replaces only that block.

use std::path::PathBuf;
use std::sync::Arc;

use p1_assembly::{Catalog, ToolServices, ToolSpec};
use p1_contracts::Tool;

use crate::HostDeps;
use crate::activity::CompletionHub;
use crate::cli::SandboxMode;

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
    catalog.tool(
        "read",
        Box::new(move |spec: &ToolSpec, services: &ToolServices| {
            // Issue #142: `read` refuses the credential files under the agent's home
            // (the injected one in tests), whatever access it was granted.
            let tool =
                p1_tool_read::ReadTool::new(services.workspace.clone(), services.observed.clone())
                    .with_home(read_home.clone());
            Ok(apply_face!(tool, spec))
        }),
    );
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
