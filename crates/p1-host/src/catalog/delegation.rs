//! The delegation family of the catalog: the four `worker_*` tool registrations and
//! the step that appends them to every main agent. Kept apart from `run.rs` so the
//! family can later be built as a module of its own without touching the run drivers.

#[cfg(feature = "delegation")]
use std::sync::Arc;

use p1_assembly::EnvironmentFile;
#[cfg(feature = "delegation")]
use p1_assembly::{Catalog, ToolServices, ToolSpec};
#[cfg(feature = "delegation")]
use p1_contracts::Tool;

#[cfg(feature = "delegation")]
use crate::HostDeps;
#[cfg(feature = "workflows")]
use crate::catalog::workflow::WORKFLOW_MODULES;

/// The four worker tools, in the order the host appends them to a main agent.
#[cfg(feature = "delegation")]
pub(crate) const WORKER_MODULES: [&str; 4] = [
    "worker_start",
    "worker_result",
    "worker_continue",
    "worker_cancel",
];

/// Give every MAIN agent the worker tools (ADR-0050 item 1). Appends a default-face
/// [`ToolSpec`] for each worker module the environment does not already list, in
/// `worker_start`, `worker_result`, `worker_continue`, `worker_cancel` order; an
/// environment that lists one keeps its own entry (which carries a face). Called only
/// at the three main-agent assembly sites — never in the child factory, so a worker
/// never gets the worker tools. A no-op when the `delegation` feature is not compiled.
/// With `workflows` the four `workflow_*` tools follow the same way (ADR-0053 item 7).
#[cfg(feature = "delegation")]
pub(crate) fn with_worker_tools(environment: &mut EnvironmentFile) {
    #[cfg(feature = "workflows")]
    let modules = WORKER_MODULES.iter().chain(WORKFLOW_MODULES.iter());
    #[cfg(not(feature = "workflows"))]
    let modules = WORKER_MODULES.iter();
    for &module in modules {
        if environment.tools.iter().any(|tool| tool.module == module) {
            continue;
        }
        environment.tools.push(ToolSpec {
            module: module.to_string(),
            name: None,
            description: None,
            variant: None,
        });
    }
}

#[cfg(not(feature = "delegation"))]
pub(crate) fn with_worker_tools(_environment: &mut EnvironmentFile) {}

#[cfg(feature = "delegation")]
pub(crate) fn register_delegation_tools(
    catalog: &mut Catalog,
    deps: &HostDeps,
    service: Option<Arc<dyn p1_workers::WorkerService>>,
) -> Result<(), String> {
    // Without a service the keys are not registered at all, so an environment naming
    // one gets the ordinary `UnknownToolModule`.
    let Some(service) = service else {
        return Ok(());
    };

    // What a parent may grant is the host's own knowledge, never a compiled list in
    // the tool crate: every tool module this catalog registers, minus `finish` (the
    // factory adds it to every worker), the `worker_*` modules (a worker never
    // delegates) and the `workflow_*` modules (a worker never orchestrates). The environments a worker may run are the host's environment dirs.
    let grantable: Vec<String> = catalog
        .tool_keys()
        .into_iter()
        .filter(|key| {
            key != "finish" && !key.starts_with("worker_") && !key.starts_with("workflow_")
        })
        .collect();
    let environments = crate::models::environment_names(&deps.environment_dirs)?;

    let service_for = service.clone();
    let grantable_for_start = grantable.clone();
    catalog.tool(
        "worker_start",
        Box::new(move |spec: &ToolSpec, _services: &ToolServices| {
            Ok(apply_face!(
                p1_tool_delegate::WorkerStartTool::new(
                    service_for.clone(),
                    grantable_for_start.clone(),
                    environments.clone(),
                ),
                spec
            ))
        }),
    );

    let service_for = service.clone();
    catalog.tool(
        "worker_result",
        Box::new(move |spec: &ToolSpec, _services: &ToolServices| {
            Ok(apply_face!(
                p1_tool_delegate::WorkerResultTool::new(service_for.clone()),
                spec
            ))
        }),
    );

    let service_for = service.clone();
    catalog.tool(
        "worker_continue",
        Box::new(move |spec: &ToolSpec, _services: &ToolServices| {
            Ok(apply_face!(
                p1_tool_delegate::WorkerContinueTool::new(service_for.clone(), grantable.clone()),
                spec
            ))
        }),
    );

    catalog.tool(
        "worker_cancel",
        Box::new(move |spec: &ToolSpec, _services: &ToolServices| {
            Ok(apply_face!(
                p1_tool_delegate::WorkerCancelTool::new(service.clone()),
                spec
            ))
        }),
    );
    Ok(())
}
