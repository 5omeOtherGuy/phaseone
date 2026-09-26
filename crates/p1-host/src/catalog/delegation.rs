//! The delegation family of the catalog: the four `worker_*` tool registrations and
//! the step that appends them to every main agent. Kept apart from `run.rs` so the
//! family can later be built as a module of its own without touching the run drivers.
//!
//! Two paths build the family (ADR-0085, S6.7):
//! - an environment that names a member PACKAGE (a `modules.lock` key resolving to one of
//!   [`WORKER_MODULES`]) takes the module path for the whole family: the host appends no
//!   native member, the members it names are instantiated from their packages, and each
//!   instance is linked against the parent's [`WorkerScope`] ([`worker_member_services`]);
//! - every other environment keeps the native `p1-tool-delegate` members, appended to each
//!   main agent and bound to the whole worker service, as before. No shipped environment
//!   names a member package (the shipped `modules.lock` is empty), so every shipped
//!   environment takes this path.

#[cfg(feature = "delegation")]
use std::sync::Arc;
#[cfg(feature = "delegation")]
use std::sync::atomic::{AtomicU64, Ordering};

use p1_assembly::EnvironmentFile;
#[cfg(feature = "delegation")]
use p1_assembly::{Catalog, ToolServices, ToolSpec};
#[cfg(feature = "delegation")]
use p1_contracts::Tool;
#[cfg(feature = "delegation")]
use p1_module_runtime::Services;
#[cfg(feature = "delegation")]
use p1_workers::{ScopeKey, WorkerScope, WorkerScopes, WorkerService};

#[cfg(feature = "delegation")]
use crate::HostDeps;
#[cfg(feature = "delegation")]
use crate::catalog::modules::ModuleServices;
#[cfg(feature = "workflows")]
use crate::catalog::workflow::{WORKFLOW_MODULES, WORKFLOW_TOOLS};

/// The worker family's member module ids, as the packages' manifests name them, in the
/// order the native members are appended ([`WORKER_TOOLS`] has the same order).
#[cfg(feature = "delegation")]
pub const WORKER_MODULES: [&str; 4] = [
    "p1/worker-start",
    "p1/worker-result",
    "p1/worker-continue",
    "p1/worker-cancel",
];

/// The native members' catalog keys, in the order the host appends them to a main agent.
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

/// Whether `environment` names a member package of the family `modules`: then it takes
/// the module path for that family, and no native member is appended.
#[cfg(feature = "delegation")]
fn names_a_member_package(environment: &EnvironmentFile, modules: &[&str]) -> bool {
    environment.tools.iter().any(|tool| {
        modules
            .iter()
            .any(|&module| tool.module == lock_key(module))
    })
}

/// Give every MAIN agent the worker tools (ADR-0050 item 1). Appends a default-face
/// [`ToolSpec`] for each native worker member the environment does not already list, in
/// `worker_start`, `worker_result`, `worker_continue`, `worker_cancel` order; an
/// environment that lists one keeps its own entry (which carries a face). An environment
/// that names a member package of a family gets no native member of that family: it has
/// the members it named, each from its package, and nothing else (ADR-0085). Called only
/// at the three main-agent assembly sites — never in the child factory, so a worker
/// never gets the worker tools. A no-op when the `delegation` feature is not compiled.
/// With `workflows` the four `workflow_*` tools follow the same way (ADR-0053 item 7).
#[cfg(feature = "delegation")]
pub(crate) fn with_worker_tools(environment: &mut EnvironmentFile) {
    let mut native: Vec<&str> = Vec::new();
    if !names_a_member_package(environment, &WORKER_MODULES) {
        native.extend(WORKER_TOOLS);
    }
    #[cfg(feature = "workflows")]
    if !names_a_member_package(environment, &WORKFLOW_MODULES) {
        native.extend(WORKFLOW_TOOLS);
    }
    for module in native {
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

/// The services of one worker member instance over `scope`.
#[cfg(feature = "delegation")]
fn worker_services(_scope: &WorkerScope) -> Services {
    // The runtime link of the worker interfaces (S6.7.1) is not in this tree yet: without
    // it no field of `Services` can carry the scope, so the member keeps today's
    // `MissingService` refusal.
    Services::default()
}

/// Install the family's scopes and module hook for the run whose worker service is
/// `service`: `catalog/modules.rs` links worker members through the hook, and `run.rs`
/// retires the generation when the main agent's assembly is dropped.
#[cfg(feature = "delegation")]
pub(crate) fn install_member_scopes(deps: &mut HostDeps, service: Arc<dyn WorkerService>) {
    let scopes = MemberScopes::new(service);
    deps.module_services = Some(worker_member_services(scopes.clone(), None));
    deps.member_scopes = Some(scopes);
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
    fn an_environment_without_member_packages_keeps_every_native_member() {
        let mut env = environment(&["read"]);
        with_worker_tools(&mut env);
        let mut expected = vec!["read"];
        expected.extend(WORKER_TOOLS);
        expected.extend(WORKFLOW_TOOLS);
        assert_eq!(modules(&env), expected);
    }

    #[test]
    fn naming_a_member_package_takes_the_module_path_for_that_family_only() {
        let mut env = environment(&["read", "worker-result"]);
        with_worker_tools(&mut env);
        let mut expected = vec!["read", "worker-result"];
        expected.extend(WORKFLOW_TOOLS);
        assert_eq!(modules(&env), expected, "no native worker member is added");

        let mut env = environment(&["workflow-status"]);
        with_worker_tools(&mut env);
        let mut expected = vec!["workflow-status"];
        expected.extend(WORKER_TOOLS);
        assert_eq!(
            modules(&env),
            expected,
            "no native workflow member is added"
        );
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
