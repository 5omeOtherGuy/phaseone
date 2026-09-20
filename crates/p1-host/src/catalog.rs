//! The compile-time catalog: the ONE place in the harness that names concrete
//! provider and tool crates. An environment file can only select keys registered
//! here, so configuration can never load a module that was not compiled in.

use std::path::PathBuf;
use std::sync::Arc;

use p1_assembly::{Catalog, ProviderSpec, ToolServices, ToolSpec};
use p1_contracts::{Provider, Tool};

use crate::HostDeps;
use crate::activity::CompletionHub;
use crate::cli::SandboxMode;

/// Test-only hook run after the built-in catalog is populated. A test registers
/// its fake provider factory here, replacing a real provider key.
pub type CatalogHook = Box<dyn Fn(&mut Catalog) + Send + Sync>;

/// Apply a `ToolSpec`'s optional face override to a `p1-workspace`-based tool.
/// No override at all keeps the constructor's default face and identity.
macro_rules! apply_face {
    ($tool:expr, $spec:expr) => {{
        let tool = $tool;
        if $spec.name.is_none() && $spec.description.is_none() && $spec.variant.is_none() {
            Arc::new(tool) as Arc<dyn Tool>
        } else {
            let name = $spec
                .name
                .clone()
                .unwrap_or_else(|| tool.declaration().name.clone());
            let description = $spec
                .description
                .clone()
                .unwrap_or_else(|| tool.declaration().description.clone());
            let variant = $spec
                .variant
                .clone()
                .unwrap_or_else(|| tool.identity().variant.clone());
            Arc::new(tool.with_face(p1_workspace::ToolFace::new(name, description), &variant))
                as Arc<dyn Tool>
        }
    }};
}

/// Same override rules as [`apply_face`], for the delegation tools' own
/// `ToolFace` type.
#[cfg(feature = "delegation")]
macro_rules! apply_delegate_face {
    ($tool:expr, $spec:expr) => {{
        let tool = $tool;
        if $spec.name.is_none() && $spec.description.is_none() && $spec.variant.is_none() {
            Arc::new(tool) as Arc<dyn Tool>
        } else {
            let name = $spec
                .name
                .clone()
                .unwrap_or_else(|| tool.declaration().name.clone());
            let description = $spec
                .description
                .clone()
                .unwrap_or_else(|| tool.declaration().description.clone());
            let variant = $spec
                .variant
                .clone()
                .unwrap_or_else(|| tool.identity().variant.clone());
            Arc::new(tool.with_face(p1_tool_delegate::ToolFace::new(name, description), &variant))
                as Arc<dyn Tool>
        }
    }};
}

/// Same override rules as [`apply_face`], for the `finish` tool's own `ToolFace`
/// type.
macro_rules! apply_finish_face {
    ($tool:expr, $spec:expr) => {{
        let tool = $tool;
        if $spec.name.is_none() && $spec.description.is_none() && $spec.variant.is_none() {
            Arc::new(tool) as Arc<dyn Tool>
        } else {
            let name = $spec
                .name
                .clone()
                .unwrap_or_else(|| tool.declaration().name.clone());
            let description = $spec
                .description
                .clone()
                .unwrap_or_else(|| tool.declaration().description.clone());
            let variant = $spec
                .variant
                .clone()
                .unwrap_or_else(|| tool.identity().variant.clone());
            Arc::new(tool.with_face(p1_tool_finish::ToolFace::new(name, description), &variant))
                as Arc<dyn Tool>
        }
    }};
}

/// Build the catalog from the injected dependencies.
///
/// Provider keys: `anthropic-subscription`, `openai-codex-subscription`.
/// Tool keys: `read`, `edit`, `write`, `grep`, `shell`, `apply_patch`, and — with
/// the `delegation` feature and a worker service present — the four `worker_*`
/// tools.
///
/// Provider construction reads no credential file; the credential sources are
/// resolved lazily on the first `access`. Tools are constructed per agent with
/// that agent's fresh [`ToolServices`].
pub fn build_catalog(
    deps: &HostDeps,
    sandbox: SandboxMode,
    sandbox_write: &[PathBuf],
    env_pass: &[String],
    completion: &Arc<CompletionHub>,
) -> Catalog {
    #[cfg(feature = "delegation")]
    return build_catalog_with_workers(
        deps,
        deps.worker_service.clone(),
        sandbox,
        sandbox_write,
        env_pass,
        completion,
    );
    #[cfg(not(feature = "delegation"))]
    build_catalog_inner(deps, sandbox, sandbox_write, env_pass, completion)
}

/// As [`build_catalog`], with the worker tools bound to `service` instead of
/// `deps.worker_service` (used by `p1 env show`, which starts no workers).
#[cfg(feature = "delegation")]
pub fn build_catalog_with_workers(
    deps: &HostDeps,
    service: Option<Arc<dyn p1_workers::WorkerService>>,
    sandbox: SandboxMode,
    sandbox_write: &[PathBuf],
    env_pass: &[String],
    completion: &Arc<CompletionHub>,
) -> Catalog {
    let mut catalog = Catalog::new();
    register_providers(&mut catalog, deps);
    register_standard_tools(
        &mut catalog,
        deps,
        sandbox,
        sandbox_write,
        env_pass,
        completion,
    );
    register_delegation_tools(&mut catalog, service);
    if let Some(hook) = &deps.catalog_hook {
        hook(&mut catalog);
    }
    catalog
}

#[cfg(not(feature = "delegation"))]
fn build_catalog_inner(
    deps: &HostDeps,
    sandbox: SandboxMode,
    sandbox_write: &[PathBuf],
    env_pass: &[String],
    completion: &Arc<CompletionHub>,
) -> Catalog {
    let mut catalog = Catalog::new();

    register_providers(&mut catalog, deps);
    register_standard_tools(
        &mut catalog,
        deps,
        sandbox,
        sandbox_write,
        env_pass,
        completion,
    );

    if let Some(hook) = &deps.catalog_hook {
        hook(&mut catalog);
    }
    catalog
}

fn register_providers(catalog: &mut Catalog, deps: &HostDeps) {
    let transport = deps.transport.clone();
    catalog.provider(
        "anthropic-subscription",
        Box::new(move |spec: &ProviderSpec| {
            let credentials = p1_provider_anthropic::ClaudeCodeCredentials::from_default_location()
                .map_err(|error| error.to_string())?;
            let provider = p1_provider_anthropic::AnthropicProvider::new(
                &spec.model,
                transport.clone(),
                Arc::new(credentials),
            );
            Ok(Arc::new(provider) as Arc<dyn Provider>)
        }),
    );

    let transport = deps.transport.clone();
    catalog.provider(
        "openai-codex-subscription",
        Box::new(move |spec: &ProviderSpec| {
            let credentials = p1_provider_openai::CodexCliCredentials::from_default_location()
                .map_err(|error| error.to_string())?;
            let provider = p1_provider_openai::OpenAiCodexProvider::new(
                &spec.model,
                transport.clone(),
                Arc::new(credentials),
            );
            Ok(Arc::new(provider) as Arc<dyn Provider>)
        }),
    );
}

fn register_standard_tools(
    catalog: &mut Catalog,
    deps: &HostDeps,
    sandbox: SandboxMode,
    sandbox_write: &[PathBuf],
    env_pass: &[String],
    completion: &Arc<CompletionHub>,
) {
    catalog.tool(
        "read",
        Box::new(|spec: &ToolSpec, services: &ToolServices| {
            Ok(apply_face!(
                p1_tool_read::ReadTool::new(services.workspace.clone(), services.observed.clone()),
                spec
            ))
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
            let tool = apply_finish_face!(
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

#[cfg(feature = "delegation")]
fn register_delegation_tools(
    catalog: &mut Catalog,
    service: Option<Arc<dyn p1_workers::WorkerService>>,
) {
    // Without a service the keys are not registered at all, so an environment naming
    // one gets the ordinary `UnknownToolModule`.
    let Some(service) = service else {
        return;
    };

    let service_for = service.clone();
    catalog.tool(
        "worker_start",
        Box::new(move |spec: &ToolSpec, _services: &ToolServices| {
            Ok(apply_delegate_face!(
                p1_tool_delegate::WorkerStartTool::new(service_for.clone()),
                spec
            ))
        }),
    );

    let service_for = service.clone();
    catalog.tool(
        "worker_result",
        Box::new(move |spec: &ToolSpec, _services: &ToolServices| {
            Ok(apply_delegate_face!(
                p1_tool_delegate::WorkerResultTool::new(service_for.clone()),
                spec
            ))
        }),
    );

    let service_for = service.clone();
    catalog.tool(
        "worker_continue",
        Box::new(move |spec: &ToolSpec, _services: &ToolServices| {
            Ok(apply_delegate_face!(
                p1_tool_delegate::WorkerContinueTool::new(service_for.clone()),
                spec
            ))
        }),
    );

    catalog.tool(
        "worker_cancel",
        Box::new(move |spec: &ToolSpec, _services: &ToolServices| {
            Ok(apply_delegate_face!(
                p1_tool_delegate::WorkerCancelTool::new(service.clone()),
                spec
            ))
        }),
    );
}
