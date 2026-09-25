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
#[cfg(feature = "workflows")]
pub mod workflow;
#[cfg(feature = "workflows")]
pub(crate) mod worktree;
// Providers, the standard tools and the WebAssembly modules each have their own file,
// so the stream that owns one edits it without touching the others (plan §3). The
// tools file uses `apply_face!` too, so it is declared below the macro as well.
mod modules;
mod providers;
mod tools;

#[cfg(feature = "delegation")]
use delegation::register_delegation_tools;
// Re-exported so the `crate::catalog::…` paths other crates and the tests use stay valid.
use providers::register_providers;
pub use providers::{
    WHOLE_PROVIDERS, chat_route, credential_line, credential_line_for_route, messages_route,
    reject_profile, responses_route, route_provider,
};
use tools::register_standard_tools;
// Re-exported so `crate::catalog::register_workflow_tools` stays the path its callers use.
#[cfg(feature = "workflows")]
pub(crate) use workflow::register_workflow_tools;

/// Build the catalog from the injected dependencies.
///
/// Provider keys: one key per route file found in `<environments dir>/../routes`
/// (`docs/design/routes-and-profiles.md` §2) — the Messages adapter's
/// `anthropic-subscription` and the Responses adapter's `openai-codex-subscription`
/// routes among them. Tool keys: `read`, `edit`, `write`, `grep`, `shell`,
/// `apply_patch`, and — with the `delegation` feature and a worker service present —
/// the four `worker_*` tools.
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
    let mut catalog = Catalog::new();
    register_providers(&mut catalog, deps)?;
    register_standard_tools(
        &mut catalog,
        deps,
        sandbox,
        sandbox_write,
        sandbox_read,
        env_pass,
        completion,
    );
    register_delegation_tools(&mut catalog, deps, service)?;
    #[cfg(feature = "workflows")]
    register_workflow_tools(&mut catalog, deps.workflow_service.clone());
    if let Some(hook) = &deps.catalog_hook {
        hook(&mut catalog);
    }
    Ok(catalog)
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
    let mut catalog = Catalog::new();

    register_providers(&mut catalog, deps)?;
    register_standard_tools(
        &mut catalog,
        deps,
        sandbox,
        sandbox_write,
        sandbox_read,
        env_pass,
        completion,
    );

    if let Some(hook) = &deps.catalog_hook {
        hook(&mut catalog);
    }
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
