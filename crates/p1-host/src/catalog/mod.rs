//! The compile-time catalog: the ONE place in the harness that names concrete
//! provider and tool crates. An environment file can only select keys registered
//! here, so configuration can never load a module that was not compiled in.

use std::path::PathBuf;
use std::sync::Arc;

use p1_assembly::Catalog;
// Only `build_catalog`'s doc link names it now that the tool entries live in `tools.rs`.
#[cfg(doc)]
use p1_assembly::ToolServices;

use crate::HostDeps;
use crate::activity::CompletionHub;
use crate::cli::SandboxMode;

/// Test-only hook run after the built-in catalog is populated. A test registers
/// its fake provider factory here, replacing a real provider key.
pub type CatalogHook = Box<dyn Fn(&mut Catalog) + Send + Sync>;

/// Apply a `ToolSpec`'s optional face override to a tool.
/// No override at all keeps the constructor's default face and identity.
macro_rules! apply_face {
    ($tool:expr, $spec:expr) => {
        apply_face!($tool, $spec, p1_contracts::tool::ToolFace)
    };
    ($tool:expr, $spec:expr, $face:ty) => {{
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
            Arc::new(tool.with_face(<$face>::new(name, description), &variant)) as Arc<dyn Tool>
        }
    }};
}

// The delegation and workflow families and the child assembly live in their own files,
// so a later slice can turn each into a WebAssembly module without touching `run.rs`.
// They are declared after `apply_face!`: a `macro_rules!` macro is only visible to the
// modules declared below its definition.
#[cfg(feature = "delegation")]
pub(crate) mod children;
pub(crate) mod delegation;
#[cfg(feature = "delegation")]
mod subagents;
#[cfg(feature = "workflows")]
pub mod workflow;
#[cfg(feature = "delegation")]
pub(crate) mod worktree;
// Providers, the standard tools and the WebAssembly modules each have their own file,
// so the stream that owns one edits it without touching the others (plan §3). The
// tools file uses `apply_face!` too, so it is declared below the macro as well.
pub mod capabilities;
pub mod modules;
mod providers;
mod tools;

#[cfg(feature = "delegation")]
use delegation::register_delegation_tools;
// Re-exported so the `crate::catalog::…` paths other crates and the tests use stay valid.
use providers::register_providers;
pub use providers::{
    ProviderComponents, WHOLE_PROVIDERS, credential_line, credential_line_for_route,
    provider_component, reject_profile, route_provider,
};
use tools::register_standard_tools;
// The mutation mode of a tool's row and the capability services that row's link assembles
// (`tools.rs`): re-exported so the module acceptance suite can link a component exactly as
// the host links it.
pub use tools::{capability_services_for, mutation_mode};
// Re-exported so `crate::catalog::register_workflow_tools` stays the path its callers use.
#[cfg(feature = "workflows")]
pub(crate) use workflow::register_workflow_tools;

/// Build the catalog from the injected dependencies.
///
/// Provider keys: one key per route file found in `<environments dir>/../routes`
/// (`docs/design/routes-and-profiles.md` §2) — the Messages adapter's
/// `anthropic-subscription` and the Responses adapter's `openai-codex-subscription`
/// routes among them. Tool keys: `read` (the release's `p1/read` host entry, S1.8.1),
/// `edit`, `write`, `grep`, `shell`, `apply_patch`, and — with the `delegation` feature
/// and a worker service present — the four `worker_*` tools.
///
/// A routed key is selected with `route` + `profile` and refuses the whole-provider
/// form; a whole provider refuses a profile. A route file whose id collides with a
/// whole-provider key is a start-up error, reported here before any run.
///
/// Provider construction reads no credential file; the credential sources are
/// resolved lazily on the first `access`. Tools are constructed per agent with
/// that agent's fresh [`ToolServices`].
pub fn build_catalog(
    deps: &HostDeps,
    sandbox: SandboxMode,
    sandbox_write: &[PathBuf],
    sandbox_read: &[PathBuf],
    env_pass: &[String],
    completion: &Arc<CompletionHub>,
) -> Result<Catalog, String> {
    #[cfg(feature = "delegation")]
    return build_catalog_with_workers(
        deps,
        deps.worker_service.clone(),
        sandbox,
        sandbox_write,
        sandbox_read,
        env_pass,
        completion,
    );
    #[cfg(not(feature = "delegation"))]
    build_catalog_inner(
        deps,
        sandbox,
        sandbox_write,
        sandbox_read,
        env_pass,
        completion,
    )
}

/// As [`build_catalog`], with the worker tools bound to `service` instead of
/// `deps.worker_service` (used by `p1 env show`, which starts no workers).
#[cfg(feature = "delegation")]
pub fn build_catalog_with_workers(
    deps: &HostDeps,
    service: Option<Arc<dyn p1_workers::WorkerService>>,
    sandbox: SandboxMode,
    sandbox_write: &[PathBuf],
    sandbox_read: &[PathBuf],
    env_pass: &[String],
    completion: &Arc<CompletionHub>,
) -> Result<Catalog, String> {
    deps.verified_sources.clear();
    deps.build_loaders.clear();
    let mut catalog = Catalog::new();
    // Every route × account pair (ADR-0139 §2): an environment with an `account`
    // names `<route>@<account>`.
    let routes = crate::routes::load_route_pairs(&deps.environment_dirs)?;
    register_providers(&mut catalog, deps, &routes)?;
    register_standard_tools(
        &mut catalog,
        deps,
        sandbox,
        sandbox_write,
        sandbox_read,
        env_pass,
        completion,
    )?;
    // The official-release host entries (S1.8.1, D083b 2) stand where the compiled-in
    // registrations they replace stood: before the delegation family, whose grantable list is
    // the catalog's own tool keys, so `read` is grantable to a worker exactly as the native
    // registration made it. `shell` and `finish` registered with the standard tools (S3.8),
    // through the same host-entry step.
    modules::register_host_entries(&mut catalog, deps, &routes)?;
    // Register locked packages before the worker family snapshots grantable keys.
    // Locked member keys are left to their family's registration below.
    modules::register_locked_modules(&mut catalog, deps, &routes)?;
    register_skill(&mut catalog);
    register_delegation_tools(&mut catalog, deps, service)?;
    #[cfg(feature = "workflows")]
    workflow::register_workflow_tools_with_sources(
        &mut catalog,
        deps.workflow_service.clone(),
        &deps.verified_sources,
        &deps.build_loaders,
        modules::release_for_build(deps).as_deref(),
    )?;
    if let Some(hook) = &deps.catalog_hook {
        hook(&mut catalog);
    }
    let catalog = with_run_roots(catalog, deps)?;
    Ok(catalog)
}

/// ADR-0122: put the run's scratch root (point 2) and its shared workspace mutation
/// counter (point 5) on the catalog, so every agent assembled from it is confined with
/// the same second root and moves the same counter the host's activity log reads.
fn with_run_roots(mut catalog: Catalog, deps: &HostDeps) -> Result<Catalog, String> {
    catalog = catalog.with_mutations(deps.mutations.clone());
    if let Some(scratch) = &deps.scratch {
        catalog = catalog.with_scratch(scratch.clone());
    }
    let locations = crate::auth::locations(deps);
    let settings = crate::models::load_settings(&locations)?;
    let global = p1_assembly::expand_home(
        settings
            .instructions_global
            .as_deref()
            .unwrap_or("~/.agents/AGENTS.md"),
        deps.home.as_deref(),
    );
    let home = deps.home.clone();
    let credentials = locations.credential_paths();
    Ok(catalog
        .with_instruction_sources(p1_assembly::InstructionSources {
            home: deps.home.clone(),
            global,
            credential_paths: credentials.clone(),
        })
        .with_skills(
            Box::new(move |workspace, settings| {
                Arc::new(p1_skill_fs::FilesystemSkills::discover(
                    workspace,
                    home.as_deref(),
                    &settings.roots,
                    &credentials,
                ))
            }),
            p1_tool_skill::listing,
        ))
}

fn register_skill(catalog: &mut Catalog) {
    use p1_contracts::Tool;
    catalog.tool(
        "skill",
        Box::new(|spec, services| {
            let source = services
                .skills
                .clone()
                .ok_or("skill source not configured")?;
            Ok(apply_face!(p1_tool_skill::SkillTool::new(source), spec))
        }),
    );
}

#[cfg(not(feature = "delegation"))]
fn build_catalog_inner(
    deps: &HostDeps,
    sandbox: SandboxMode,
    sandbox_write: &[PathBuf],
    sandbox_read: &[PathBuf],
    env_pass: &[String],
    completion: &Arc<CompletionHub>,
) -> Result<Catalog, String> {
    deps.verified_sources.clear();
    deps.build_loaders.clear();
    let mut catalog = Catalog::new();
    // Every route × account pair (ADR-0139 §2): an environment with an `account`
    // names `<route>@<account>`.
    let routes = crate::routes::load_route_pairs(&deps.environment_dirs)?;

    register_providers(&mut catalog, deps, &routes)?;
    register_standard_tools(
        &mut catalog,
        deps,
        sandbox,
        sandbox_write,
        sandbox_read,
        env_pass,
        completion,
    )?;
    // The official-release host entries and then the locked modules (S1.8.1, D083b 2): a
    // lock that selects a host entry's key already kept the host entry out. `shell` and
    // `finish` registered with the standard tools (S3.8), through the same host-entry step.
    modules::register_host_entries(&mut catalog, deps, &routes)?;
    modules::register_locked_modules(&mut catalog, deps, &routes)?;
    register_skill(&mut catalog);

    if let Some(hook) = &deps.catalog_hook {
        hook(&mut catalog);
    }
    let catalog = with_run_roots(catalog, deps)?;
    Ok(catalog)
}

/// Resolve a loaded environment against the route files, before `assemble` is
/// called (spec §2 steps 1–3): the route file it names must exist and must bind the
/// profile the environment selected, and the environment's model becomes that
/// binding's WIRE model. A whole-provider environment is left alone — the provider
/// factory reports the wrong form.
pub fn resolve_environment(
    environment: &mut p1_assembly::EnvironmentFile,
    environment_dirs: &[PathBuf],
) -> Result<(), String> {
    if environment.profile.is_none() || WHOLE_PROVIDERS.contains(&environment.provider.as_str()) {
        return Ok(());
    }
    let route = crate::routes::load_route_by_id(environment_dirs, &environment.provider)?;
    let profile = environment
        .profile
        .as_ref()
        .ok_or_else(|| format!("`{}` needs a model profile", environment.provider))?;
    let binding = route.binding(&profile.id)?;
    environment.model = binding.wire_model.clone();
    Ok(())
}
