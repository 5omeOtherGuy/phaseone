//! The delegation family of the catalog: the four `worker_*` tool registrations and
//! the step that appends them to every main agent. Kept apart from `run.rs` so the
//! family can later be built as a module of its own without touching the run drivers.
//!
//! The members are components (ADR-0085, S6.7) and official-release HOST ENTRIES (S6.11,
//! D083b): the eight packages of [`MEMBER_ENTRIES`] are loaded by package name from the
//! one release manifest (`official_release_manifest`, with its debug fallback) and verified
//! against it, never selected by `modules.lock`; a release missing one, or shipping one
//! that does not verify, fails the catalog build naming the package. Each is registered
//! under its catalog key (`worker_start`, …, `workflow_cancel`) through the module path
//! (`register_host_entry`), and each instance is linked through the member hook
//! ([`worker_member_services`], `workflow::member_services`) against the parent's
//! [`WorkerScope`]. No native member is registered any more, and neither native tool crate is
//! a dependency: a resume reads its workers with `p1_workers::journal` (`children.rs`) and
//! `p1 workflow run` prints `p1_workflow::render_report` (`run.rs`).
//!
//! An environment that names a member package by its `modules.lock` key still gets only the
//! members of that family it names; every other main agent gets the whole family appended.
//!
//! Either family can be switched off at run time (ADR-0085 item 6, S6.8): `[capabilities]`
//! in `settings.toml` ([`Capabilities`]). A disabled family is left out of the next main
//! agent's assembly, so its tools and their `{{#tool:...}}` prompt sections go with it, and
//! an environment that names one of its members fails that assembly. The cargo features
//! stay a build option; they are no longer the user's switch.

#[cfg(feature = "delegation")]
use std::collections::HashMap;
#[cfg(feature = "delegation")]
use std::path::Path;
#[cfg(feature = "delegation")]
use std::sync::Arc;
#[cfg(feature = "delegation")]
use std::sync::atomic::{AtomicU64, Ordering};

use p1_assembly::EnvironmentFile;
#[cfg(feature = "delegation")]
use p1_assembly::{Catalog, ToolServices, ToolSpec};
#[cfg(feature = "delegation")]
use p1_contracts::BoxFuture;
#[cfg(feature = "delegation")]
use p1_module_runtime::delegation::{WorkerLists, WorkerServices};
#[cfg(feature = "delegation")]
use p1_module_runtime::{LoadedModule, Loader, ReleaseManifest, Services};
#[cfg(feature = "delegation")]
use p1_workers::{
    ChildId, ChildSpec, ScopeKey, WorkerError, WorkerScope, WorkerScopes, WorkerService,
    WorkersControl, WorkersStart,
};

#[cfg(feature = "delegation")]
use crate::HostDeps;
#[cfg(feature = "delegation")]
use crate::catalog::modules::{
    ModuleServices, ModulesError, register_host_entry, register_named_host_entry,
};
#[cfg(feature = "workflows")]
use crate::catalog::workflow::{WORKFLOW_MODULES, WORKFLOW_TOOLS};
use serde::Deserialize;

/// `[capabilities]` in `settings.toml` (ADR-0085 item 6): which delegation families the
/// next main-agent assembly gets. Both default to enabled, today's behaviour, so an absent
/// table or key changes nothing. It is read once when a main agent's assembly generation
/// starts (a run, `p1 env show`, `p1 workflow run`) and kept for that generation, as the
/// generation's worker scopes are: a model switch reassembles with the flags its session
/// started with, so a running session's tool set never changes under its children.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Capabilities {
    /// The four `worker_*` members.
    pub workers: bool,
    /// The four `workflow_*` members, and `p1 workflow run`.
    pub workflows: bool,
}

impl Default for Capabilities {
    fn default() -> Self {
        Self {
            workers: true,
            workflows: true,
        }
    }
}

/// The capabilities from the `settings.toml` the host reads its other settings from.
pub(crate) fn enabled_capabilities(deps: &crate::HostDeps) -> Result<Capabilities, String> {
    Ok(crate::models::load_settings(&crate::auth::locations(deps))?.capabilities)
}

/// The explicit refusal of a disabled family: never a silent skip, and never the native
/// member in place of the package the environment named.
#[cfg(feature = "delegation")]
pub(crate) fn disabled(family: &str) -> String {
    format!("{family} are disabled (`[capabilities] {family} = false` in settings.toml)")
}

/// The worker family's member module ids, as the packages' manifests name them, in the
/// order the members are appended ([`WORKER_TOOLS`] has the same order).
#[cfg(feature = "delegation")]
pub const WORKER_MODULES: [&str; 4] = [
    "p1/worker-start",
    "p1/worker-result",
    "p1/worker-continue",
    "p1/worker-cancel",
];

/// The members' catalog keys, in the order the host appends them to a main agent: each is
/// its package's host entry.
#[cfg(feature = "delegation")]
pub(crate) const WORKER_TOOLS: [&str; 4] = [
    "worker_start",
    "worker_result",
    "worker_continue",
    "worker_cancel",
];

/// The `modules.lock` key a member package is selected by: the module id without the
/// reserved `p1/` namespace, the lock's documented entry shape (`[modules.<name>]` with
/// `package = "p1/<name>"`). The host names the family by it when it decides which path an
/// environment takes, because an environment names lock keys, never package ids.
#[cfg(feature = "delegation")]
pub(crate) fn lock_key(module: &str) -> &str {
    module.strip_prefix("p1/").unwrap_or(module)
}

/// Whether `environment` names a member package of the family `modules` by its lock key:
/// then it has the members of that family it names, and none is appended.
#[cfg(feature = "delegation")]
fn names_a_member_package(environment: &EnvironmentFile, modules: &[&str]) -> bool {
    environment.tools.iter().any(|tool| {
        modules
            .iter()
            .any(|&module| tool.module == lock_key(module))
    })
}

/// Give every MAIN agent the worker tools (ADR-0050 item 1). Appends a default-face
/// [`ToolSpec`] for each worker member the environment does not already list, in
/// `worker_start`, `worker_result`, `worker_continue`, `worker_cancel` order; an
/// environment that lists one keeps its own entry. An environment that names a member
/// package of a family by its lock key gets nothing appended for that family: it has the
/// members it named, and nothing else (ADR-0085). Every key is a package (S6.11). Called only
/// at the three main-agent assembly sites — never in the child factory, so a worker
/// never gets the worker tools. A no-op when the `delegation` feature is not compiled.
/// With `workflows` the four `workflow_*` tools follow the same way (ADR-0053 item 7).
///
/// A family `capabilities` disables gets nothing appended, and an environment that names
/// one of its members (a native key or a package's lock key) is refused with the
/// family's "... are disabled" error (ADR-0085 item 6).
#[cfg(feature = "delegation")]
pub fn with_worker_tools(
    environment: &mut EnvironmentFile,
    capabilities: Capabilities,
) -> Result<(), String> {
    let mut appended: Vec<&str> = Vec::new();
    if !capabilities.workers {
        refuse_named_members(environment, "workers", &WORKER_MODULES, &WORKER_TOOLS)?;
    } else if !names_a_member_package(environment, &WORKER_MODULES) {
        appended.extend(WORKER_TOOLS);
    }
    #[cfg(feature = "workflows")]
    if !capabilities.workflows {
        refuse_named_members(environment, "workflows", &WORKFLOW_MODULES, &WORKFLOW_TOOLS)?;
    } else if !names_a_member_package(environment, &WORKFLOW_MODULES) {
        appended.extend(WORKFLOW_TOOLS);
    }
    for module in appended {
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
    Ok(())
}

/// Refuse an environment that names a member of the disabled `family`, by its catalog key
/// or its package's lock key, with the family's disabled error naming the member.
#[cfg(feature = "delegation")]
fn refuse_named_members(
    environment: &EnvironmentFile,
    family: &str,
    modules: &[&str],
    tools: &[&str],
) -> Result<(), String> {
    let named = environment.tools.iter().find(|tool| {
        tools.contains(&tool.module.as_str())
            || modules
                .iter()
                .any(|&module| tool.module == lock_key(module))
    });
    match named {
        Some(tool) => Err(format!(
            "{}; the environment `{}` names `{}`",
            disabled(family),
            environment.name,
            tool.module
        )),
        None => Ok(()),
    }
}

/// Assembly generations handed out in this process. Never reused, so a retired generation
/// cannot come back through a later main agent's scopes.
#[cfg(feature = "delegation")]
static GENERATIONS: AtomicU64 = AtomicU64::new(1);

/// The worker scopes of one main agent's assembly generation (ADR-0085 item 4): one
/// [`WorkerScope`] per (generation, operation, parent), where the operation is the family
/// plus the parent, so the members of one family share their parent's children and nothing
/// else. The generation is the main agent's whole session: a model switch or a re-grant
/// between turns keeps it (S6.2), and the host retires it when the agent's assembly is
/// dropped (`run.rs`, B-S6-9, D068).
#[cfg(feature = "delegation")]
pub struct MemberScopes {
    registry: WorkerScopes,
    generation: u64,
}

#[cfg(feature = "delegation")]
impl MemberScopes {
    /// A new generation of scopes over `service`.
    pub fn new(service: Arc<dyn WorkerService>) -> Arc<Self> {
        Arc::new(Self {
            registry: WorkerScopes::new(service),
            generation: GENERATIONS.fetch_add(1, Ordering::Relaxed),
        })
    }

    /// The registry the scopes live in; `retire_generation` is called on it at teardown.
    pub fn registry(&self) -> &WorkerScopes {
        &self.registry
    }

    /// This assembly's generation.
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// The worker family's scope for `parent`. Every member instance of one parent gets a
    /// handle to the same scope while any of them lives.
    pub fn workers(&self, parent: &str) -> WorkerScope {
        self.registry.scope(ScopeKey {
            generation: self.generation,
            operation: format!("workers:{parent}"),
            parent: parent.to_string(),
        })
    }
}

/// The module hook of the worker family (B-S6-9, D068): the services a member instance is
/// linked with. A worker member assembled for a main agent gets its parent's scope; every
/// other instance — another module, or no main agent (a worker never delegates) — gets
/// what `fallback` gives, or no service at all, so a grant it cannot be given fails its
/// assembly with the runtime's `MissingService`.
///
/// The result tool's face is announced after its host-entry instance is assembled,
/// where its final model-facing name is available (ADR-0057).
#[cfg(feature = "delegation")]
pub fn worker_member_services(
    scopes: Arc<MemberScopes>,
    fallback: Option<ModuleServices>,
) -> ModuleServices {
    Arc::new(move |module: &str, services: &ToolServices| {
        if WORKER_MODULES.contains(&module) {
            return match &services.agent {
                Some(parent) => worker_services(&scopes.workers(parent)),
                None => Services::default(),
            };
        }
        match &fallback {
            Some(fallback) => fallback(module, services),
            None => Services::default(),
        }
    })
}

/// The services of one worker member instance: all three worker interfaces over its
/// parent's scope. The runtime links only what the member's manifest grants, so
/// `worker_result` (granted `workers-observe` alone) has no start to call.
#[cfg(feature = "delegation")]
fn worker_services(scope: &WorkerScope) -> Services {
    Services {
        workers: Some(WorkerServices::scoped(scope.clone())),
        ..Services::default()
    }
}

/// Install the family's scopes and module hook for the run whose worker service is
/// `service`: `catalog/modules.rs` links worker members through the hook, and `run.rs`
/// retires the generation when the main agent's assembly is dropped. A new generation
/// is created for every main-agent assembly generation, whatever `capabilities` says, so
/// teardown always has one to retire. With workers disabled the hook is not installed:
/// a worker member that still reached assembly would be linked with no worker service and
/// fail with the runtime's `MissingService`, never fall back to a native member.
#[cfg(feature = "delegation")]
pub(crate) fn install_member_scopes(
    deps: &mut HostDeps,
    service: Arc<dyn WorkerService>,
    capabilities: Capabilities,
) {
    let scopes = MemberScopes::new(service);
    deps.module_services = capabilities
        .workers
        .then(|| worker_member_services(scopes.clone(), None));
    deps.member_scopes = Some(scopes);
}

#[cfg(not(feature = "delegation"))]
pub(crate) fn with_worker_tools(
    _environment: &mut EnvironmentFile,
    _capabilities: Capabilities,
) -> Result<(), String> {
    Ok(())
}

/// The four worker members over `service`, each registered from its host entry under its
/// catalog key (S6.11): the grantable tools and the environment names are fixed here, from
/// this catalog, and every member instance is linked with them ([`WorkerLists`]).
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
    // a member: every tool module this catalog registers, minus `finish` (the
    // factory adds it to every worker), the `worker_*` modules (a worker never
    // delegates) and the `workflow_*` modules (a worker never orchestrates). The environments a worker may run are the host's environment dirs.
    let lists = worker_lists(catalog, deps)?;

    let entries = official_member_entries_for(deps)?;
    let hook = member_hook(deps, service.clone(), lists);
    for (module, key) in WORKER_MODULES.into_iter().zip(WORKER_TOOLS) {
        if !catalog
            .tool_keys()
            .iter()
            .any(|registered| registered == key)
        {
            let loaded = entry(&entries, module)?;
            deps.verified_sources.record(key, &loaded);
            if key == WORKER_TOOLS[1] {
                let service = service.clone();
                register_named_host_entry(
                    catalog,
                    key,
                    loaded,
                    hook.clone(),
                    Arc::new(move |name| service.set_result_tool_name(name)),
                );
            } else {
                register_host_entry(catalog, key, loaded, hook.clone());
            }
        }
    }
    // Only a member actually registered from the official release owns its provenance.
    #[cfg(feature = "workflows")]
    if deps.workflow_service.is_some() {
        for (module, key) in WORKFLOW_MODULES.into_iter().zip(WORKFLOW_TOOLS) {
            if !catalog
                .tool_keys()
                .iter()
                .any(|registered| registered == key)
            {
                let loaded = entry(&entries, module)?;
                deps.verified_sources.record(key, &loaded);
                register_host_entry(catalog, key, loaded, hook.clone());
            }
        }
    }
    Ok(())
}

/// The host's worker lists (D084): the tool modules a worker may be granted and the
/// environment names a worker may run on, read off the assembled catalog. The official host
/// entries and a member a lock selects are both linked with them, so a locked `worker-start`
/// declares the same enums a host entry does.
#[cfg(feature = "delegation")]
pub(crate) fn worker_lists(catalog: &Catalog, deps: &HostDeps) -> Result<WorkerLists, String> {
    worker_lists_with_keys(catalog.tool_keys(), deps)
}

#[cfg(feature = "delegation")]
pub(crate) fn worker_lists_with_keys(
    keys: Vec<String>,
    deps: &HostDeps,
) -> Result<WorkerLists, String> {
    let grantable: Vec<String> = keys
        .into_iter()
        .filter(|key| {
            key != "finish" && !key.starts_with("worker_") && !key.starts_with("workflow_")
        })
        .collect();
    let environments = crate::models::environment_names(&deps.environment_dirs)?;
    Ok(WorkerLists {
        grantable,
        environments,
    })
}

/// A member-services hook with the host's lists and grant check added: every instance of a
/// worker member — a host entry or a package a lock selects — is linked with `lists`, and a
/// module outside the grantable list is refused before the scope is asked.
#[cfg(feature = "delegation")]
pub(crate) fn with_member_lists(family: ModuleServices, lists: WorkerLists) -> ModuleServices {
    Arc::new(move |module: &str, services: &ToolServices| {
        let mut linked = family(module, services);
        if WORKER_MODULES.contains(&module) {
            linked.workers = linked.workers.map(|workers| checked(workers, &lists));
        }
        linked
    })
}

// ------------------------------------------------------------------ host entries

/// The official-release host entries of both families (S6.11, D083b): every member package,
/// by the module id its manifest names. The host loads them by these names from the release
/// manifest, never through `modules.lock`.
#[cfg(feature = "delegation")]
pub fn member_entries() -> Vec<&'static str> {
    let entries = WORKER_MODULES.to_vec();
    #[cfg(feature = "workflows")]
    let entries = [entries, WORKFLOW_MODULES.to_vec()].concat();
    entries
}

/// The verified member entries of one release, by package name.
#[cfg(feature = "delegation")]
pub type MemberEntries = HashMap<&'static str, Arc<LoadedModule>>;

/// Loads every [`member_entries`] package from the release manifest `release_manifest`,
/// verifying each against that same manifest (official source, class, world, protocol,
/// digest, grants: `p1_module_runtime::Loader`). The first package that is missing or does
/// not verify is the error, and the error names it.
#[cfg(feature = "delegation")]
pub fn load_member_entries(release_manifest: &Path) -> Result<MemberEntries, String> {
    load_member_entries_with_loader(release_manifest, None)
}

#[cfg(feature = "delegation")]
pub(crate) fn load_member_entries_with_loader(
    release_manifest: &Path,
    loaders: Option<&super::modules::BuildLoaders>,
) -> Result<MemberEntries, String> {
    let unreadable = |error: String| {
        format!(
            "cannot load the host entries {}: {}: {error}",
            member_entries().join(", "),
            release_manifest.display()
        )
    };
    let manifest = match loaders {
        Some(loaders) => loaders.manifest_for(release_manifest),
        None => ReleaseManifest::read(release_manifest),
    }
    .map_err(|error| unreadable(error.to_string()))?;
    manifest
        .check_unique_digests()
        .map_err(|error| unreadable(error.to_string()))?;
    // A release that lacks a member is refused before any member is compiled, naming it.
    if let Some(package) = member_entries()
        .into_iter()
        .find(|package| manifest.entry(package).is_none())
    {
        return Err(format!(
            "host entry {package} from {}: the release manifest has no such component",
            release_manifest.display()
        ));
    }
    let loader = match loaders {
        Some(loaders) => loaders
            .for_release(release_manifest, manifest)
            .map_err(|error| unreadable(error.to_string()))?,
        None => {
            let root = release_manifest.parent().unwrap_or(Path::new("."));
            Arc::new(Loader::new(manifest, root).map_err(|error| unreadable(error.to_string()))?)
        }
    };
    let mut entries = HashMap::new();
    for package in member_entries() {
        let module = loader.load(package).map_err(|error| {
            format!(
                "host entry {package} from {}: {error}",
                release_manifest.display()
            )
        })?;
        entries.insert(package, Arc::new(module));
    }
    Ok(entries)
}

/// Load the member entries for this catalog build. An in-place install at the same
/// manifest path must not reuse components from the previous generation.
#[cfg(feature = "delegation")]
pub(crate) fn official_member_entries_for(deps: &HostDeps) -> Result<MemberEntries, String> {
    let release = super::modules::release_for_build(deps)
        .ok_or_else(|| ModulesError::NoRelease.to_string())?;
    load_member_entries_with_loader(&release, Some(&deps.build_loaders))
}

/// The loaded host entry `module`.
#[cfg(feature = "delegation")]
fn entry(entries: &MemberEntries, module: &str) -> Result<Arc<LoadedModule>, String> {
    entries
        .get(module)
        .cloned()
        .ok_or_else(|| format!("{module} is not one of p1's host entries"))
}

/// The hook the host entries of one catalog are linked through. In a run (the run composed
/// its member scopes, `children.rs`) it is the run's own member hook, `deps.module_services`,
/// so a disabled family's members link nothing and fail with `MissingService`. A catalog no
/// run composed (`p1 env show`, whose `service` starts nothing) gets the members linked over
/// scopes of its own `service`, for whatever agent assembles them, so the environment still
/// shows them. Either way a worker member gets `lists` and the host's grant check.
#[cfg(feature = "delegation")]
fn member_hook(
    deps: &HostDeps,
    service: Arc<dyn WorkerService>,
    lists: WorkerLists,
) -> ModuleServices {
    let family: ModuleServices = if deps.member_scopes.is_some() {
        deps.module_services
            .clone()
            .unwrap_or_else(|| Arc::new(|_: &str, _: &ToolServices| Services::default()))
    } else {
        inspection_services(MemberScopes::new(service))
    };
    with_member_lists(family, lists)
}

/// The member hook of a catalog no run composed: the worker members over `scopes`, keyed
/// by the assembling agent or, for `p1 env show`'s plain assembly, by no agent.
#[cfg(feature = "delegation")]
fn inspection_services(scopes: Arc<MemberScopes>) -> ModuleServices {
    Arc::new(move |module: &str, services: &ToolServices| {
        if WORKER_MODULES.contains(&module) {
            worker_services(&scopes.workers(services.agent.as_deref().unwrap_or_default()))
        } else {
            Services::default()
        }
    })
}

/// A worker member's services with the host's lists and its grant check: the member's
/// schemas and texts name the lists, and a module outside `grantable` is refused before
/// the scope is asked, with the native tools' text.
#[cfg(feature = "delegation")]
fn checked(workers: WorkerServices, lists: &WorkerLists) -> WorkerServices {
    let grantable: Arc<[String]> = lists.grantable.clone().into();
    WorkerServices {
        start: workers.start.map(|inner| {
            Arc::new(GrantChecked {
                inner,
                grantable: grantable.clone(),
            }) as Arc<dyn WorkersStart>
        }),
        observe: workers.observe,
        control: workers.control.map(|inner| {
            Arc::new(GrantChecked {
                inner,
                grantable: grantable.clone(),
            }) as Arc<dyn WorkersControl>
        }),
        lists: lists.clone(),
    }
}

/// A worker interface that checks a grant against the host's grantable list first, as the
/// native `worker_start` and `worker_continue` did before calling the service: nothing
/// starts and nothing reaches the child when a module is not grantable.
#[cfg(feature = "delegation")]
struct GrantChecked<T: ?Sized> {
    inner: Arc<T>,
    grantable: Arc<[String]>,
}

#[cfg(feature = "delegation")]
impl<T: ?Sized> GrantChecked<T> {
    /// The native refusal of the first module in `modules` a worker cannot be granted, after
    /// the tool's own prefix; the members relay it verbatim.
    fn refusal(&self, modules: &[String]) -> Option<String> {
        let module = modules
            .iter()
            .find(|module| !self.grantable.contains(module))?;
        Some(format!(
            "`{module}` is not a tool module a worker can be granted. Valid tools: {}",
            self.grantable.join(", ")
        ))
    }
}

#[cfg(feature = "delegation")]
impl WorkersStart for GrantChecked<dyn WorkersStart> {
    fn start<'a>(&'a self, spec: ChildSpec) -> BoxFuture<'a, Result<ChildId, WorkerError>> {
        // `invalid-environment` is the start error the member renders as
        // "Cannot start worker: <reason>", the native text.
        match self.refusal(&spec.tools) {
            Some(reason) => Box::pin(async move { Err(WorkerError::InvalidEnvironment(reason)) }),
            None => self.inner.start(spec),
        }
    }
}

#[cfg(feature = "delegation")]
impl WorkersControl for GrantChecked<dyn WorkersControl> {
    fn cancel<'a>(&'a self, id: &'a ChildId) -> BoxFuture<'a, Result<(), WorkerError>> {
        self.inner.cancel(id)
    }

    fn continue_child<'a>(
        &'a self,
        id: &'a ChildId,
        message: String,
        add_tools: Vec<String>,
    ) -> BoxFuture<'a, Result<(), WorkerError>> {
        // Checked before the id, as the native tool checks it: an unknown module is refused
        // whatever the child's state, and `worker_continue` relays the reason verbatim.
        match self.refusal(&add_tools) {
            Some(reason) => Box::pin(async move { Err(WorkerError::Regrant(reason)) }),
            None => self.inner.continue_child(id, message, add_tools),
        }
    }
}

/// Which path each family takes (ADR-0085, S6.7): an environment naming a member package
/// gets that family's named members only; the other family keeps its native members.
#[cfg(all(test, feature = "workflows"))]
mod tests {
    use super::*;
    use p1_contracts::ModelOptions;

    fn environment(modules: &[&str]) -> EnvironmentFile {
        EnvironmentFile {
            name: "family-test".into(),
            family: "test".into(),
            provider: "scripted".into(),
            model: "m".into(),
            profile: None,
            profile_text: None,
            options: ModelOptions::default(),
            tools: modules
                .iter()
                .map(|module| ToolSpec {
                    module: (*module).into(),
                    name: None,
                    description: None,
                    variant: None,
                })
                .collect(),
            prompt_template: String::new(),
            context: None,
            summarize_prompt: None,
        }
    }

    fn modules(environment: &EnvironmentFile) -> Vec<&str> {
        environment
            .tools
            .iter()
            .map(|tool| tool.module.as_str())
            .collect()
    }

    #[test]
    fn member_identity_comes_from_loaded_bytes() {
        let release =
            super::super::modules::official_release_manifest().expect("release manifest path");
        let entries = load_member_entries_with_loader(
            &release,
            Some(&super::super::modules::BuildLoaders::default()),
        )
        .expect("verified member entries");
        let sources = super::super::modules::VerifiedSources::default();
        let (module, key) = WORKER_MODULES
            .into_iter()
            .zip(WORKER_TOOLS)
            .next()
            .expect("worker member");
        let loaded = entries.get(module).expect("member loaded");
        sources.record(key, loaded);
        let recorded = sources.resolve(key).expect("member identity");
        assert_eq!(recorded.digest, loaded.digest().to_string());
        assert_eq!(recorded.abi, loaded.abi());
    }

    #[test]
    fn a_locked_tool_registered_before_worker_members_is_grantable() {
        let mut catalog = Catalog::new();
        catalog.tool(
            "extra_locked_tool",
            Box::new(|spec: &ToolSpec, _: &ToolServices| {
                Ok(Arc::new(p1_testkit::FakeTool::new(&spec.module))
                    as Arc<dyn p1_contracts::Tool>)
            }),
        );
        let deps = crate::catalog::modules::quiet_deps(vec![]);
        let lists = worker_lists(&catalog, &deps).expect("worker lists");
        assert!(lists.grantable.iter().any(|key| key == "extra_locked_tool"));
    }

    #[test]
    fn an_environment_without_member_packages_keeps_every_native_member() {
        let mut env = environment(&["read"]);
        with_worker_tools(&mut env, Capabilities::default()).expect("enabled");
        let mut expected = vec!["read"];
        expected.extend(WORKER_TOOLS);
        expected.extend(WORKFLOW_TOOLS);
        assert_eq!(modules(&env), expected);
    }

    #[test]
    fn naming_a_member_package_takes_the_module_path_for_that_family_only() {
        let mut env = environment(&["read", "worker-result"]);
        with_worker_tools(&mut env, Capabilities::default()).expect("enabled");
        let mut expected = vec!["read", "worker-result"];
        expected.extend(WORKFLOW_TOOLS);
        assert_eq!(modules(&env), expected, "no native worker member is added");

        let mut env = environment(&["workflow-status"]);
        with_worker_tools(&mut env, Capabilities::default()).expect("enabled");
        let mut expected = vec!["workflow-status"];
        expected.extend(WORKER_TOOLS);
        assert_eq!(
            modules(&env),
            expected,
            "no native workflow member is added"
        );
    }

    /// A disabled family appends nothing and leaves the other family as it was.
    #[test]
    fn a_disabled_family_is_left_out_of_the_next_assembly() {
        let workers_off = Capabilities {
            workers: false,
            workflows: true,
        };
        let mut env = environment(&["read"]);
        with_worker_tools(&mut env, workers_off).expect("nothing disabled is named");
        let mut expected = vec!["read"];
        expected.extend(WORKFLOW_TOOLS);
        assert_eq!(modules(&env), expected);

        let neither = Capabilities {
            workers: false,
            workflows: false,
        };
        let mut env = environment(&["read"]);
        with_worker_tools(&mut env, neither).expect("nothing disabled is named");
        assert_eq!(modules(&env), ["read"]);
    }

    /// Naming a member of a disabled family, by native key or by lock key, is the family's
    /// explicit error, never a skip or the native member in its place.
    #[test]
    fn naming_a_disabled_member_is_the_familys_error() {
        let neither = Capabilities {
            workers: false,
            workflows: false,
        };
        for (module, family) in [
            ("worker_result", "workers"),
            ("worker-start", "workers"),
            ("workflow_status", "workflows"),
            ("workflow-cancel", "workflows"),
        ] {
            let mut env = environment(&["read", module]);
            let error = with_worker_tools(&mut env, neither).expect_err(module);
            assert!(
                error.starts_with(&format!("{family} are disabled")),
                "{module}: {error}"
            );
            assert!(error.contains(&format!("`{module}`")), "{module}: {error}");
        }
    }

    /// Absent, the table and each key default to enabled; an unknown key is refused.
    #[test]
    fn the_capabilities_table_defaults_to_enabled() {
        let parse = |text: &str| toml::from_str::<Capabilities>(text);
        assert_eq!(parse("").expect("empty"), Capabilities::default());
        assert_eq!(
            parse("workflows = false").expect("one key"),
            Capabilities {
                workers: true,
                workflows: false,
            }
        );
        assert!(parse("delegation = false").is_err());
    }

    #[test]
    fn the_constants_name_module_ids_and_their_lock_keys() {
        assert_eq!(
            WORKER_MODULES.map(lock_key),
            [
                "worker-start",
                "worker-result",
                "worker-continue",
                "worker-cancel"
            ]
        );
        assert_eq!(
            WORKFLOW_MODULES.map(lock_key),
            [
                "workflow-start",
                "workflow-status",
                "workflow-result",
                "workflow-cancel"
            ]
        );
    }
}
