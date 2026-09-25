//! The delegation keys of the catalog: the four `worker_*` tools and the four
//! `workflow_*` tools, each registered only when its feature is compiled in and its
//! service is present.

// `workflows` implies `delegation`, so one gate covers both registrations' imports.
#[cfg(feature = "delegation")]
use std::sync::Arc;

#[cfg(feature = "delegation")]
use p1_assembly::{Catalog, ToolServices, ToolSpec};
#[cfg(feature = "delegation")]
use p1_contracts::Tool;

#[cfg(feature = "delegation")]
use crate::HostDeps;

#[cfg(feature = "delegation")]
pub(super) fn register_delegation_tools(
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

/// The four `workflow_*` tools over `service` (ADR-0053 item 7). Without a service the
/// keys are not registered at all, so an environment naming one gets the ordinary
/// `UnknownToolModule`.
#[cfg(feature = "workflows")]
pub(crate) fn register_workflow_tools(
    catalog: &mut Catalog,
    service: Option<Arc<dyn p1_workflow::WorkflowService>>,
) {
    let Some(service) = service else {
        return;
    };

    let service_for = service.clone();
    catalog.tool(
        "workflow_start",
        Box::new(move |spec: &ToolSpec, _services: &ToolServices| {
            Ok(apply_face!(
                p1_tool_workflow::WorkflowStartTool::new(service_for.clone()),
                spec,
                p1_tool_workflow::ToolFace
            ))
        }),
    );

    let service_for = service.clone();
    catalog.tool(
        "workflow_status",
        Box::new(move |spec: &ToolSpec, _services: &ToolServices| {
            Ok(apply_face!(
                p1_tool_workflow::WorkflowStatusTool::new(service_for.clone()),
                spec,
                p1_tool_workflow::ToolFace
            ))
        }),
    );

    let service_for = service.clone();
    catalog.tool(
        "workflow_result",
        Box::new(move |spec: &ToolSpec, _services: &ToolServices| {
            Ok(apply_face!(
                p1_tool_workflow::WorkflowResultTool::new(service_for.clone()),
                spec,
                p1_tool_workflow::ToolFace
            ))
        }),
    );

    catalog.tool(
        "workflow_cancel",
        Box::new(move |spec: &ToolSpec, _services: &ToolServices| {
            Ok(apply_face!(
                p1_tool_workflow::WorkflowCancelTool::new(service.clone()),
                spec,
                p1_tool_workflow::ToolFace
            ))
        }),
    );
}
