//! The standard tool entries of the catalog. Each tool's registration stays its own
//! block, so the stream that turns one tool into a module replaces only that block.
//!
//! Since S3.8 the `shell` and `finish` entries are official-release HOST ENTRIES
//! (ADR-0083, D083b, D-XO-49): `HOST_ENTRIES` lists the two keys beside the file tools',
//! and [`register_composed_host_entries`] — the shared host-entry step — loads each package
//! (`p1/shell`, `p1/finish`) BY NAME from the installed release manifest, verifies it
//! against that same manifest and hands it to the registration this file owns, which
//! links it exactly the allocation and the services its manifest grants — `process` (the
//! native [`ProcessService`] the host assembles from the workspace, the
//! environment snapshot, the `--env-pass` names and the sandbox) plus `clock` for
//! `shell`, and the completion hub's `completion` for `finish`. No native fallback: a
//! missing release or package fails the catalog build naming the module. What depends on
//! the host and not on the component — the sandbox paragraph and variant, the face, and
//! `finish`'s policy/contract declaration — the host presents here (ADR-0083 §1 and §2).
//!
//! Since S7.10-R1 (ADR-0095) the five file tools are release host entries as well — `read`
//! since S1.8.1, and `edit`, `write`, `apply_patch` and `grep` since their crates left the
//! host's normal graph — so this file registers NO native file tool: those keys are served by
//! the release's `p1/read`, `p1/edit`, `p1/write`, `p1/patch` and `p1/search` components,
//! loaded and verified by the shared host-entry step and linked through the service hook this
//! file owns ([`module_services`] → [`capability_services_for`], now over the host's
//! `p1_module_runtime::file_services`). A release that does not carry one of them fails the
//! catalog build naming the module, and nothing compiled in answers for the key.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, Weak};

use p1_assembly::{Catalog, ToolServices, ToolSpec};
use p1_contracts::Tool;
use p1_contracts::tool::{ResultDescription, ToolFace};
use p1_contracts::{
    BoxFuture, CallDescription, Concurrency, Effect, ToolCall, ToolContext, ToolDeclaration,
    ToolIdentity, ToolOutcome, ToolResultItem,
};
use p1_module_runtime::process::{ExitRecords, ProcessCapability, ProcessService, Sandbox};
use p1_module_runtime::{
    CallOutputs, ExecutionLimits, LoadedModule, OutputStore, Services, wasm_tool,
};
use p1_redact::MaskCounter;
use p1_workspace::{MutationPolicy, ObservedFiles, Workspace};

use crate::HostDeps;
use crate::activity::{AgentRole, CompletionHub, finish_component};
use crate::cli::SandboxMode;

use super::capabilities::NativeDeclaration;
use super::modules::{HostEntryRegistration, ModuleServices, register_composed_host_entries};

/// The catalog keys of the two host entries this file composes its own tool for (S3.8):
/// `HOST_ENTRIES` names the packages the release must ship for them.
const SHELL: &str = "shell";
const FINISH: &str = "finish";

/// The module identity of the search component (its verified manifest name): the one file-tool
/// package whose row grants the walk and no mutation, linked the search capability alone.
const SEARCH_MODULE: &str = "p1/search";

/// The capability services a module tool is linked with, from the assembling agent's own:
/// the read side of its workspace and its observations (the host's read service over
/// `p1-workspace`, `p1_module_runtime::file_services`, S1.8), the search tool's walk beside
/// it, and — for the mutating rows — the owned mutation with the mode the row grants (S2).
/// The credential files under the host's home are refused there exactly as the native `read`
/// refuses them (issue #142), so selecting a package never widens what an agent can read.
/// Each service is linked only where a package's manifest grants it.
///
/// The hook keys on the module identity the loader verified (`p1/edit`, `p1/patch`, …), so
/// a lock entry can serve another key of the same package without moving a grant.
pub(super) fn module_services(
    deps: &HostDeps,
    routes: &[crate::routes::RouteFile],
) -> ModuleServices {
    let home = deps.home.clone();
    let github_token = github_token(deps.shell_env.as_deref());
    let locations = crate::auth::locations(deps);
    let mut credential_paths = locations.credential_paths();
    // Use the provider factories' snapshot, never a later on-disk login directory.
    // Include every loaded login: its aliases are reachable from any tool.
    for route in routes {
        if let Some(dir) = route.credential.login_dir.as_deref()
            && let Some(dir) = locations.claude_code_dir(Some(dir))
        {
            credential_paths.push(dir.join(".credentials.json"));
        }
    }
    Arc::new(move |module: &str, services: &ToolServices| {
        if matches!(
            module,
            "p1/read-github"
                | "p1/list-directory-github"
                | "p1/glob-github"
                | "p1/search-github"
                | "p1/commit-search"
                | "p1/diff-github"
                | "p1/list-repositories"
        ) {
            if let Some(token) = &github_token {
                services.mask.secrets().register(token);
            }
            return Services {
                github: Some(Arc::new(p1_module_runtime::github::GithubCapability::new(
                    github_token.clone(),
                ))),
                ..Services::default()
            };
        }
        capability_services_for(
            module,
            services
                .workspace
                .clone()
                .with_credential_paths(credential_paths.clone()),
            agent_observations(services),
            home.clone(),
        )
    })
}

/// Use the host snapshot (including an explicitly empty test snapshot), never
/// expose environment access to the component or consult gh's credential files.
fn github_token(snapshot: Option<&[(std::ffi::OsString, std::ffi::OsString)]>) -> Option<String> {
    [
        "P1_GITHUB_TOKEN",
        "AMPI_GITHUB_TOKEN",
        "MMR_GITHUB_TOKEN",
        "GITHUB_TOKEN",
        "GH_TOKEN",
        "GITHUB_PERSONAL_ACCESS_TOKEN",
    ]
    .iter()
    .find_map(|name| {
        let value = match snapshot {
            Some(values) => values
                .iter()
                .find(|(key, _)| key == name)
                .and_then(|(_, v)| v.to_str().map(str::to_owned)),
            None => std::env::var(name).ok(),
        };
        value.filter(|s| !s.trim().is_empty())
    })
}

/// Every assembling agent's ONE observation record, keyed by the agent's one
/// [`MaskCounter`] (held weakly: an agent that is gone leaves its entry to be pruned).
/// Process-wide because one agent's tools are linked through several hook instances (the
/// release's host entries, a lock's packages) and over several catalogs (a reload).
static OBSERVATIONS: Mutex<Vec<(Weak<MaskCounter>, ObservedFiles)>> = Mutex::new(Vec::new());

/// The observation record of the agent `services` assembles for. Each assembly hands its
/// tools a fresh record, which is right for a new agent; a RE-assembly of the same agent
/// (a worker's re-grant, ADR-0050 item 6; a model switch) keeps the record its tools
/// already filled, so a file the agent read is still one it observed (tools.md, the
/// read-before-mutate invariant). Two agents never share one: each has its own counter.
fn agent_observations(services: &ToolServices) -> ObservedFiles {
    let mut records = OBSERVATIONS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    records.retain(|(agent, _)| agent.strong_count() > 0);
    let mask = Arc::as_ptr(&services.mask);
    // A live weak reference keeps its allocation, so no other counter has this address.
    if let Some((_, observed)) = records
        .iter()
        .find(|(agent, _)| std::ptr::eq(agent.as_ptr(), mask))
    {
        return observed.clone();
    }
    records.push((Arc::downgrade(&services.mask), services.observed.clone()));
    services.observed.clone()
}

/// The mutation mode the catalog row of the module `module` (its verified manifest name,
/// e.g. `p1/edit`) assembles: observed for the edit and write components, patch-authorized
/// for patch, and none for a tool that never mutates
/// (`docs/design/modules/workspace-mutation.md`, the per-tool table; ADR-0088 point 4).
pub fn mutation_mode(module: &str) -> Option<MutationPolicy> {
    match module {
        "p1/edit" | "p1/write" => Some(MutationPolicy::Observed),
        "p1/patch" => Some(MutationPolicy::PatchAuthorized),
        _ => None,
    }
}

/// The capability services the module `module` is linked with, from the assembling agent's own
/// workspace and observations: the catalog row's grants. The search component is linked the
/// search capability alone (its manifest grants the walk and no snapshot); every other tool
/// gets the read side, with the mutation its row grants. Public so the module acceptance suite
/// links a component exactly as the host does.
pub fn capability_services_for(
    module: &str,
    workspace: Workspace,
    observed: ObservedFiles,
    home: Option<PathBuf>,
) -> Services {
    if module == SEARCH_MODULE {
        return p1_module_runtime::file_services::search_services(workspace, home);
    }
    p1_module_runtime::file_services::tool_services(
        workspace,
        observed,
        home,
        mutation_mode(module),
    )
}

/// The semantic capabilities a still-native registration declares, each on the identity
/// implementation its constructor builds (`env!("CARGO_PKG_NAME")` of the tool crate). An
/// entry belongs to its tool's registration block and goes with it when that tool becomes
/// a package, whose manifest grants then decide. S3.8 removed the `shell` and `finish`
/// entries with their registrations — their capabilities now come from the selected
/// package's verified manifest through [`super::capabilities::package_capabilities`] — so
/// the array is empty until a future native registration declares here again.
pub(crate) const NATIVE_CAPABILITIES: [NativeDeclaration; 0] = [];

/// What the `shell` entry's factory assembles its process service from, fixed for one catalog
/// build: the sandbox selection and the paths and names it is built with (ADR-0035, ADR-0083 §1).
struct ShellSetup {
    /// Whether the command runs inside the bubblewrap boundary.
    sandbox: SandboxMode,
    /// The paths the sandbox binds writable, from `--sandbox-write`.
    writable: Vec<PathBuf>,
    /// The paths the sandbox binds readable, from `--sandbox-read`.
    readable: Vec<PathBuf>,
    /// The home the sandbox hides, from `HOME`.
    home: Option<PathBuf>,
    /// The runtime directory the sandbox replaces, from `XDG_RUNTIME_DIR`.
    runtime_dir: Option<PathBuf>,
    /// The environment snapshot commands are rebuilt from, from the host's deps.
    shell_env: Option<Vec<(std::ffi::OsString, std::ffi::OsString)>>,
    /// The names `--env-pass` lets through.
    env_pass: Vec<String>,
    /// The run's output store every command is teed into (ADR-0109).
    outputs: Arc<OutputStore>,
    jobs: Arc<crate::jobs::JobHub>,
}

/// The registration of the `shell` host entry (S3.8): the loaded `p1/shell` component
/// linked with the host-assembled, sandboxed process service and presented under the
/// sandbox paragraph and the `+sandbox` variant, then the environment's face (ADR-0083
/// §1). The package is the one the shared step loaded from the release
/// ([`register_composed_host_entries`]), so a catalog compiles it once for its whole life
/// and the next catalog reads the release again.
fn shell_entry(
    deps: &HostDeps,
    sandbox: SandboxMode,
    sandbox_write: &[PathBuf],
    sandbox_read: &[PathBuf],
    env_pass: &[String],
) -> HostEntryRegistration {
    let setup = Arc::new(ShellSetup {
        sandbox,
        writable: sandbox_write.to_vec(),
        readable: sandbox_read.to_vec(),
        home: deps.home.clone(),
        runtime_dir: deps.runtime_dir.clone(),
        shell_env: deps.shell_env.clone(),
        env_pass: env_pass.to_vec(),
        outputs: deps.tool_outputs.clone(),
        jobs: deps.jobs.clone(),
    });
    Box::new(move |catalog: &mut Catalog, module: Arc<LoadedModule>| {
        // The shell's semantic capability comes from the package's verified manifest grant
        // (`process`), never from a native declaration, which left with its registration.
        super::capabilities::declare_package(&module);
        let loaded = module;
        let setup = setup.clone();
        catalog.tool(
            SHELL,
            Box::new(move |spec: &ToolSpec, services: &ToolServices| {
                // The execution boundary is the native process service, assembled here and
                // fixed for this agent: a request carries only the command text and its time
                // limit (ADR-0083 §1). The sandbox is applied before the face, so a face
                // override keeps the sandbox paragraph and the `+sandbox` variant.
                let mut process = ProcessService::new(services.workspace.root());
                // ADR-0122 point 3: the shell sees the run's scratch directory in
                // `P1_SCRATCH`, the same path the file tools confine to.
                if let Some(scratch) = services.workspace.scratch_root() {
                    process = process.with_scratch(scratch);
                }
                if let Some(snapshot) = &setup.shell_env {
                    process = process.with_env_snapshot(snapshot.clone());
                }
                let process = process.with_env_pass(setup.env_pass.clone());
                let (process, sandboxed) = match setup.sandbox {
                    SandboxMode::Off => (process, false),
                    SandboxMode::Workspace => {
                        let Some(home) = setup.home.clone() else {
                            return Err(
                                "--sandbox workspace needs HOME to know which home to hide: set \
                                 HOME, or pass --sandbox off"
                                    .to_string(),
                            );
                        };
                        let mut sandbox = Sandbox::for_home(home);
                        sandbox.readable = setup.readable.clone();
                        sandbox.writable = setup.writable.clone();
                        // The scratch directory is bound writable like a `--sandbox-write`
                        // path, so a command may write there (ADR-0122 point 3).
                        if let Some(scratch) = services.workspace.scratch_root() {
                            sandbox.writable.push(scratch.to_path_buf());
                        }
                        sandbox.runtime_dir = setup.runtime_dir.clone();
                        (
                            process
                                .sandboxed(sandbox)
                                .map_err(|error| error.to_string())?,
                            true,
                        )
                    }
                };
                let observed = Arc::new(Mutex::new(Vec::new()));
                // ADR-0109: each call's commands are teed into the run's store, masked with
                // this agent's credentials, and the same call's `tool-outputs.produced`
                // names them, so the process capability and the store view are built per call.
                let process = Arc::new(process);
                let store = setup.outputs.clone();
                let secrets = services.mask.secrets().clone();
                let recorded = observed.clone();
                let jobs = setup.jobs.install(
                    &services.mask,
                    Arc::new(p1_module_runtime::jobs::JobRegistry::new(
                        process.clone(),
                        store.clone(),
                        secrets.clone(),
                    )),
                );
                let linked = Services::call_scoped(move || {
                    let outputs = CallOutputs::new(store.clone(), secrets.clone());
                    // One handover slot per call: Shared shell calls run side by side
                    // (ADR-0118), and each must read only its own handed-over job.
                    let handover = p1_module_runtime::jobs::Handover::new();
                    Services {
                        process: Some(Arc::new(
                            ProcessCapability::new(process.clone())
                                .recording(recorded.clone())
                                .storing(outputs.clone())
                                // ADR-0123: a foreground command that reaches its deadline
                                // is handed over to this session's jobs, not killed.
                                .adopting(jobs.clone())
                                .handing_over_to(handover.clone()),
                        )),
                        process_jobs: Some(Arc::new(crate::jobs::JobStarter(
                            jobs.clone(),
                            handover,
                        ))),
                        tool_outputs: Some(Arc::new(outputs)),
                        ..Services::default()
                    }
                });
                let component =
                    wasm_tool(&loaded, linked, ExecutionLimits::default(), &services.mask)
                        .map_err(|error| error.to_string())?;
                // `[tool_concurrency] shell_reads = false`: every shell call runs alone,
                // whatever the classifier says (ADR-0118 amendment 2026-10-09).
                Ok(apply_face!(
                    FacedTool::new(component, sandboxed)
                        .recording(observed)
                        .running_alone(!services.tool_concurrency.shell_reads),
                    spec
                ))
            }),
        );
        Ok(())
    })
}

/// The registration of the `finish` host entry (S3.8): the loaded `p1/finish` component
/// over the host's completion hub, under the hub's gate and the declaration its policy and
/// output contract choose (ADR-0083 §2). The manifest grants `completion`, and the hub's
/// service is the only thing that can back it: a package granted a capability whose service
/// is absent fails assembly (`MissingService`) rather than running unlinked. The
/// assembly-time declaration is the MAIN agent's (the hub always gives a main agent
/// `recorded-commands`); a worker's policy and its output contract are applied by the hub at
/// that worker's assembly boundary (`catalog/children.rs`, rule 7).
fn finish_entry(completion: &Arc<CompletionHub>) -> HostEntryRegistration {
    let hub = completion.clone();
    Box::new(move |catalog: &mut Catalog, module: Arc<LoadedModule>| {
        // The finish's semantic capability comes from the package's verified manifest grant
        // (`completion`); nothing native declares it any more.
        super::capabilities::declare_package(&module);
        let loaded = module;
        let hub = hub.clone();
        catalog.tool(
            FINISH,
            Box::new(move |spec: &ToolSpec, services: &ToolServices| {
                // Per assembling agent: a worker's boundary rebuilds from THIS catalog's
                // component, whatever another generation's assembly registered since.
                hub.register_finish(&services.mask, &loaded);
                let completion = hub.issue(&services.mask);
                let grant = hub.grant(completion, &[], AgentRole::Main, None);
                let tool = finish_component(&loaded, &grant, None, &services.mask)
                    .map_err(|error| error.to_string())?;
                let faced = apply_face!(FacedTool::new(tool, false), spec);
                // A main agent never passes through `finish_for`, so the log must learn the
                // environment's name for finish here, or calls under it count as progress.
                grant
                    .completion()
                    .log
                    .set_finish_name(faced.declaration().name.clone());
                Ok(faced)
            }),
        );
        Ok(())
    })
}

/// Presents a module tool under the host's presentation (ADR-0083 §1): the environment's
/// face, and for the shell the sandbox paragraph and the `+sandbox` variant. It follows the
/// native `ShellTool`'s two-step composition exactly — the sandbox first, then the
/// environment's face — so the assembled name, description and variant are byte-identical
/// to what the native entry produced. The identity's IMPLEMENTATION stays the loader's
/// (ADR-0087): the host moves only the variant it presents.
struct FacedTool {
    inner: Arc<dyn Tool>,
    /// The name and description BEFORE the sandbox paragraph, and the variant BEFORE the
    /// suffix, so composing the sandbox and a face in either order never stacks.
    face: ToolFace,
    variant: String,
    sandboxed: bool,
    declaration: ToolDeclaration,
    identity: ToolIdentity,
    observed: Option<ExitRecords>,
    completed: Mutex<HashMap<String, i32>>,
    /// The environment ran this tool's calls alone whatever the component says
    /// (`[tool_concurrency] shell_reads = false`, ADR-0118 amendment 2026-10-09).
    runs_alone: bool,
}

impl FacedTool {
    fn new(inner: Arc<dyn Tool>, sandboxed: bool) -> Self {
        let declaration = inner.declaration().clone();
        Self {
            face: ToolFace::new(declaration.name.clone(), declaration.description.clone()),
            variant: inner.identity().variant.clone(),
            sandboxed,
            declaration,
            identity: inner.identity().clone(),
            observed: None,
            completed: Mutex::new(HashMap::new()),
            runs_alone: false,
            inner,
        }
        .composed()
    }

    /// Run every call alone, whatever the component reports (`shell_reads = false`).
    fn running_alone(mut self, alone: bool) -> Self {
        self.runs_alone = alone;
        self
    }

    fn recording(mut self, observed: ExitRecords) -> Self {
        self.observed = Some(observed);
        self
    }

    /// Replace the presentation's name, description and variant; the sandbox paragraph and
    /// suffix survive the way they survive the native `ShellTool`'s `with_face`.
    fn with_face(self, face: ToolFace, variant: &str) -> Self {
        Self {
            face,
            variant: variant.to_string(),
            ..self
        }
        .composed()
    }

    /// Recompute the declaration and identity from the face, the variant and whether the
    /// sandbox is on. Called by every constructor, so the order of the sandbox and a face
    /// never doubles the paragraph or the suffix.
    fn composed(mut self) -> Self {
        let description = if self.sandboxed {
            format!(
                "{}\n{}",
                self.face.description,
                p1_shell_guest::SANDBOX_PARAGRAPH
            )
        } else {
            self.face.description.clone()
        };
        let variant = if self.sandboxed {
            format!("{}{}", self.variant, p1_shell_guest::SANDBOX_VARIANT_SUFFIX)
        } else {
            self.variant.clone()
        };
        self.declaration = ToolDeclaration {
            name: self.face.name.clone(),
            description,
            kind: self.inner.declaration().kind.clone(),
        };
        self.identity = ToolIdentity {
            implementation: self.inner.identity().implementation.clone(),
            variant,
        };
        self
    }
}

impl Tool for FacedTool {
    fn declaration(&self) -> &ToolDeclaration {
        &self.declaration
    }

    fn identity(&self) -> &ToolIdentity {
        &self.identity
    }

    fn effect(&self, call: &ToolCall) -> Effect {
        self.inner.effect(call)
    }

    /// ADR-0118: the component's answer, unless the environment runs this tool alone.
    fn concurrency(&self, call: &ToolCall) -> Concurrency {
        if self.runs_alone {
            Concurrency::Exclusive
        } else {
            self.inner.concurrency(call)
        }
    }

    fn take_command_exit_code(&self, call_id: &str) -> Option<i32> {
        self.completed.lock().unwrap().remove(call_id)
    }

    fn command_exit_code(&self, call_id: &str) -> Option<i32> {
        self.completed.lock().unwrap().get(call_id).copied()
    }

    fn describe(&self, call: &ToolCall) -> CallDescription {
        self.inner.describe(call)
    }

    fn describe_result(&self, call: &ToolCall, result: &ToolResultItem) -> ResultDescription {
        self.inner.describe_result(call, result)
    }

    /// ADR-0120: a delegating wrapper forwards the inner tool's answer.
    fn ends_turn(&self, outcome: &ToolOutcome) -> bool {
        self.inner.ends_turn(outcome)
    }

    fn execute<'a>(
        &'a self,
        call: &'a ToolCall,
        context: ToolContext,
    ) -> BoxFuture<'a, ToolOutcome> {
        Box::pin(async move {
            let token = context.cancel.clone();
            let result = self.inner.execute(call, context).await;
            if let Some(observed) = &self.observed {
                let mut records = observed.lock().unwrap();
                let mut exit = None;
                records.retain(|(owner, code)| {
                    if *owner == token {
                        exit = Some(*code);
                        false
                    } else {
                        true
                    }
                });
                if let Some(exit) = exit {
                    self.completed
                        .lock()
                        .unwrap()
                        .insert(call.call_id.clone(), exit);
                }
            }
            result
        })
    }
}

pub(super) fn register_standard_tools(
    catalog: &mut Catalog,
    deps: &HostDeps,
    sandbox: SandboxMode,
    sandbox_write: &[PathBuf],
    sandbox_read: &[PathBuf],
    env_pass: &[String],
    completion: &Arc<CompletionHub>,
) -> Result<(), String> {
    // S7.10-R1: no native file tool is registered here any more. `read` left in S1.8.1 and
    // `edit`, `write`, `apply_patch` and `grep` with this slice: each key is a `HOST_ENTRIES`
    // entry, so `modules::register_host_entries` loads the release's package of that key, verifies
    // it against the release manifest and registers it through this file's service hook
    // (`module_services`). A release that does not carry one of them fails the catalog build
    // naming the module — there is no native fallback to hide behind. A `modules.lock` entry that
    // names one of the keys still wins over the release's, exactly as it does for `read`.
    //
    // S3.8: the `shell` and `finish` entries are the release's `p1/shell` and `p1/finish` host
    // entries (ADR-0083, D083b, D-XO-49). They are listed in `HOST_ENTRIES` beside the file
    // tools', and the one shared host-entry step loads each package from the release manifest,
    // verifies it against that same manifest — its class allocation included — and hands it to
    // the registration built here, so `load_release_module` is gone. A key a user lock names takes
    // the lock's package instead, through the same registration, so it keeps the host's
    // process service and completion hub.
    register_composed_host_entries(
        catalog,
        deps,
        &[
            (
                SHELL,
                shell_entry(deps, sandbox, sandbox_write, sandbox_read, env_pass),
            ),
            (FINISH, finish_entry(completion)),
        ],
    )
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use p1_module_runtime::{LinkError, ToolError};
    use p1_redact::MaskCounter;

    use super::*;
    use crate::catalog::modules::{HOST_ENTRIES, quiet_deps};

    /// ADR-0118 test 9 and the owner amendment: the host's presentation forwards
    /// `concurrency`, unless the environment runs the tool alone (`shell_reads = false`).
    #[test]
    fn a_faced_tool_forwards_concurrency_unless_it_runs_alone() {
        let call = ToolCall {
            call_id: "c1".into(),
            name: "probe".into(),
            input: p1_contracts::ToolInput::Json("{}".into()),
        };
        for answer in [Concurrency::Shared, Concurrency::Exclusive] {
            let inner: Arc<dyn Tool> =
                Arc::new(p1_testkit::FakeTool::new("probe").with_concurrency(answer));
            assert_eq!(
                FacedTool::new(inner.clone(), false).concurrency(&call),
                answer
            );
            let alone = FacedTool::new(inner, true).running_alone(true);
            assert_eq!(alone.concurrency(&call), Concurrency::Exclusive);
            let faced = alone.with_face(ToolFace::new("bash", "runs"), "claude");
            assert_eq!(
                faced.concurrency(&call),
                Concurrency::Exclusive,
                "a face keeps the environment's choice"
            );
        }
    }

    fn credential_fixture(
        root: &std::path::Path,
        login_dir: Option<&str>,
    ) -> (HostDeps, ToolServices) {
        let environments = root.join("environments");
        std::fs::create_dir_all(&environments).unwrap();
        std::fs::create_dir_all(root.join("routes")).unwrap();
        let login = login_dir
            .map(|dir| format!("login_dir = {dir:?}\n"))
            .unwrap_or_default();
        std::fs::write(
            root.join("routes/test.toml"),
            format!("id = \"test\"\norigin_route = \"test\"\nadapter = \"anthropic-messages\"\nendpoint = \"https://api.anthropic.com\"\n[credential]\nkind = \"claude-code-oauth\"\n{login}\n[adapter_settings]\naccount = \"claude-code-subscription\"\n"),
        ).unwrap();
        let mut deps = quiet_deps(vec![environments]);
        deps.home = Some(root.join("home"));
        deps.shell_env = Some(
            [
                ("CLAUDE_CONFIG_DIR", root.join("injected-claude")),
                ("CODEX_HOME", root.join("injected-codex")),
                ("XDG_CONFIG_HOME", root.join("injected-config")),
                ("XDG_DATA_HOME", root.join("injected-data")),
                ("PI_CODING_AGENT_DIR", root.join("injected-pi")),
            ]
            .into_iter()
            .map(|(key, value)| (key.into(), value.into_os_string()))
            .collect(),
        );
        let services = ToolServices {
            workspace: Workspace::new(root).unwrap(),
            observed: ObservedFiles::new(),
            mask: Arc::new(MaskCounter::new()),
            agent: None,
            environment: String::new(),
            prompt_template: String::new(),
            modules: Vec::new(),
            allowed_children: None,
            tool_concurrency: Default::default(),
        };
        (deps, services)
    }

    fn credential_hook(
        root: &std::path::Path,
        login_dir: Option<&str>,
    ) -> (ModuleServices, ToolServices) {
        let (deps, services) = credential_fixture(root, login_dir);
        let routes = crate::routes::load_all_routes(&deps.environment_dirs).unwrap();
        (module_services(&deps, &routes), services)
    }

    #[test]
    fn github_credential_snapshot_has_explicit_precedence_and_empty_is_anonymous() {
        let snapshot = vec![
            ("GITHUB_TOKEN".into(), "fixture-fallback".into()),
            ("P1_GITHUB_TOKEN".into(), " ".into()),
            ("AMPI_GITHUB_TOKEN".into(), "fixture-selected".into()),
        ];
        assert_eq!(
            github_token(Some(&snapshot)).as_deref(),
            Some("fixture-selected")
        );
        assert_eq!(github_token(Some(&[])), None);
    }

    #[test]
    fn only_github_packages_receive_remote_service_and_register_credential_mask() {
        let root = tempfile::tempdir().unwrap();
        let (mut deps, services) = credential_fixture(root.path(), None);
        deps.shell_env = Some(vec![(
            "P1_GITHUB_TOKEN".into(),
            "fixture-github-credential".into(),
        )]);
        let hook = module_services(&deps, &[]);
        assert!(hook("p1/read", &services).github.is_none());
        assert!(
            !services
                .mask
                .secrets()
                .contains_secret("fixture-github-credential")
        );
        for package in [
            "read-github",
            "list-directory-github",
            "glob-github",
            "search-github",
            "commit-search",
            "diff-github",
            "list-repositories",
        ] {
            let linked = hook(&format!("p1/{package}"), &services);
            assert!(linked.github.is_some());
            assert!(linked.process.is_none());
            assert!(linked.workspace.is_none());
        }
        assert!(
            services
                .mask
                .secrets()
                .contains_secret("fixture-github-credential")
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn changed_route_on_disk_keeps_snapshot_login_alias_denied() {
        route_snapshot_alias_denied(false).await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn invalid_route_on_disk_keeps_snapshot_login_alias_denied_without_panic() {
        route_snapshot_alias_denied(true).await;
    }

    #[cfg(unix)]
    async fn route_snapshot_alias_denied(invalid: bool) {
        let root = tempfile::tempdir().unwrap();
        let login = root.path().join("home/login-a");
        std::fs::create_dir_all(&login).unwrap();
        let credential = login.join(".credentials.json");
        std::fs::write(&credential, b"fixture").unwrap();
        std::fs::hard_link(&credential, root.path().join("innocent.txt")).unwrap();
        let (deps, assembly) = credential_fixture(root.path(), Some("~/login-a"));
        let routes = crate::routes::load_all_routes(&deps.environment_dirs).unwrap();
        assert_eq!(routes[0].credential.login_dir.as_deref(), Some("~/login-a"));
        let route_path = root.path().join("routes/test.toml");
        let text = if invalid {
            "not a valid route".into()
        } else {
            std::fs::read_to_string(&route_path)
                .unwrap()
                .replace("~/login-a", "~/login-b")
        };
        std::fs::write(route_path, text).unwrap();

        // Loading already happened: later registration must not consult the changed file.
        let mut catalog = Catalog::new();
        super::super::providers::register_providers(&mut catalog, &deps, &routes).unwrap();
        let hook = module_services(&deps, &routes);
        assert_credential_denied(&hook, &assembly, "innocent.txt").await;
        assert_eq!(std::fs::read(&credential).unwrap(), b"fixture");
    }

    async fn assert_credential_denied(hook: &ModuleServices, assembly: &ToolServices, path: &str) {
        for module in ["p1/read", "p1/search"] {
            let services = hook(module, assembly);
            let workspace = services.workspace.unwrap();
            for result in [
                workspace.read(path.into(), 0, 64).await.map(|_| ()),
                workspace.stat(path.into()).await.map(|_| ()),
            ] {
                assert!(
                    matches!(result, Err(p1_module_runtime::FsError::Io(ref message)) if message.contains("refuses credential files")),
                    "{module}: {path}: {result:?}"
                );
            }
        }
        let services = hook("p1/patch", assembly);
        let mutation = services.workspace_mutation.unwrap().begin().await;
        let result = mutation.write(path.into(), b"replacement".to_vec()).await;
        assert!(
            matches!(result, Err(p1_module_runtime::FsError::Io(ref message)) if message.contains("refuses credential files")),
            "write {path}: {result:?}"
        );
    }

    #[tokio::test]
    async fn injected_environment_credential_locations_refuse_read_stat_and_write() {
        let root = tempfile::tempdir().unwrap();
        let (hook, assembly) = credential_hook(root.path(), None);
        for path in [
            "injected-codex/auth.json",
            "injected-config/p1/auth.json",
            "injected-config/keys/entry",
            "injected-data/opencode/auth.json",
            "injected-pi/auth.json",
            "injected-claude/.credentials.json",
        ] {
            let file = root.path().join(path);
            std::fs::create_dir_all(file.parent().unwrap()).unwrap();
            std::fs::write(&file, b"fixture").unwrap();
            assert_credential_denied(&hook, &assembly, path).await;
            assert_eq!(std::fs::read(&file).unwrap(), b"fixture");
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn injected_environment_route_login_hard_link_aliases_are_refused() {
        route_login_alias_denied(false).await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn named_route_login_hard_link_aliases_are_refused() {
        route_login_alias_denied(true).await;
    }

    #[cfg(unix)]
    async fn route_login_alias_denied(named: bool) {
        let root = tempfile::tempdir().unwrap();
        let directory = if named {
            root.path().join("home/route-login")
        } else {
            root.path().join("injected-claude")
        };
        std::fs::create_dir_all(&directory).unwrap();
        let file = directory.join(".credentials.json");
        std::fs::write(&file, b"fixture").unwrap();
        std::fs::hard_link(&file, root.path().join("innocent.txt")).unwrap();
        let (hook, assembly) = credential_hook(root.path(), named.then_some("~/route-login"));
        assert_credential_denied(&hook, &assembly, "innocent.txt").await;
        assert_eq!(std::fs::read(&file).unwrap(), b"fixture");
    }

    const LOCKED_SHELL: &str = "format = \"p1-modules-lock/1\"\n\n[modules.shell]\n\
        package = \"p1/shell\"\nversion = \"0.0.1\"\ndigest = \"sha256:\
        0000000000000000000000000000000000000000000000000000000000000000\"\n\
        world = \"p1:module/tool@1.0.0\"\nprotocol = \"1.0\"\n";

    /// The release package `HOST_ENTRIES` names for `key`: the one the shared step loads.
    fn package_of(key: &str) -> &'static str {
        HOST_ENTRIES
            .iter()
            .find(|(entry, _)| *entry == key)
            .unwrap_or_else(|| panic!("`{key}` is one of the host entries"))
            .1
    }

    /// A user lock that names `shell` keeps the release's entry out, exactly as it does for
    /// `read` (S1.8.1): the shared step registers nothing here, and the locked-module
    /// registration is what the key runs.
    #[test]
    fn the_host_entry_stays_out_unless_a_lock_names_the_key() {
        let root = tempfile::tempdir().unwrap();
        let environments = root.path().join("environments");
        std::fs::create_dir_all(&environments).unwrap();
        let dirs = [environments];
        assert!(
            !super::super::modules::lock_selects(&dirs, SHELL),
            "no lock selects nothing"
        );

        std::fs::write(
            root.path().join("modules.lock"),
            "format = \"p1-modules-lock/1\"\n\n[modules]\n",
        )
        .unwrap();
        assert!(
            !super::super::modules::lock_selects(&dirs, SHELL),
            "the shipped empty lock"
        );

        std::fs::write(root.path().join("modules.lock"), LOCKED_SHELL).unwrap();
        assert!(super::super::modules::lock_selects(&dirs, SHELL));
        assert!(!super::super::modules::lock_selects(&dirs, "grep"));
    }

    /// A lock naming one module, as an installation writes it.
    fn lock_naming(module: &str, package: &str) -> String {
        format!(
            "format = \"p1-modules-lock/1\"\n\n[modules.{module}]\npackage = \"{package}\"\n\
             version = \"0.0.1\"\ndigest = \"sha256:\
             0000000000000000000000000000000000000000000000000000000000000000\"\n\
             world = \"p1:module/tool@1.0.0\"\nprotocol = \"1.0\"\n"
        )
    }

    /// S7.10-R1 (ADR-0095): the five file-tool keys are the release's host entries, and this
    /// file registers NO native tool for any of them. Building the standard entries over a
    /// catalog with no lock leaves exactly the two entries the HOST composes its own tool around
    /// (`shell`, `finish`): `read`, `edit`, `write`, `apply_patch` and `grep` come from the
    /// shared host-entry step ([`super::super::modules::register_host_entries`], over
    /// [`HOST_ENTRIES`]) or from a `modules.lock` entry an installation wrote — a release (or a
    /// lock) that cannot serve one of them fails the assembly naming the module instead of
    /// quietly dispatching a compiled-in tool.
    #[test]
    fn the_file_tool_keys_have_no_native_registration() {
        for (key, package) in [
            ("read", "p1/read"),
            ("edit", "p1/edit"),
            ("write", "p1/write"),
            ("apply_patch", "p1/patch"),
            ("grep", "p1/search"),
        ] {
            assert_eq!(package_of(key), package, "`{key}` is a host entry");
        }

        let root = tempfile::tempdir().unwrap();
        let environments = root.path().join("environments");
        std::fs::create_dir_all(&environments).unwrap();
        std::fs::write(
            root.path().join("modules.lock"),
            "format = \"p1-modules-lock/1\"\n\n[modules]\n",
        )
        .unwrap();
        let deps = quiet_deps(vec![environments]);
        let mut catalog = Catalog::new();
        register_standard_tools(
            &mut catalog,
            &deps,
            SandboxMode::Off,
            &[],
            &[],
            &[],
            &Arc::new(CompletionHub::new()),
        )
        .expect("the standard entries register");

        let mut keys = catalog.tool_keys();
        keys.sort();
        assert_eq!(
            keys,
            [FINISH, SHELL],
            "only the host's own composed entries come from this file"
        );
        for key in ["read", "edit", "write", "apply_patch", "grep"] {
            assert!(
                !catalog
                    .tool_keys()
                    .iter()
                    .any(|registered| registered == key),
                "`{key}` has no native registration: {key}"
            );
        }

        // A lock that names one of the keys is the locked-module registration's, not this
        // file's: the key is still not registered here.
        std::fs::write(
            root.path().join("modules.lock"),
            lock_naming("grep", "p1/search"),
        )
        .unwrap();
        let mut locked = Catalog::new();
        register_standard_tools(
            &mut locked,
            &deps,
            SandboxMode::Off,
            &[],
            &[],
            &[],
            &Arc::new(CompletionHub::new()),
        )
        .expect("a lock-selected key is not this file's");
        let mut keys = locked.tool_keys();
        keys.sort();
        assert_eq!(keys, [FINISH, SHELL]);
    }

    /// The host's forwarder (`module_services`) links every tool of one agent with the
    /// workspace the assembly gave it — so the catalog's ONE write gate, shared by a parent
    /// and its workers — and with THAT agent's observations: a re-assembly of the same
    /// agent (a worker's re-grant) keeps them, and another agent never sees them.
    #[tokio::test]
    async fn the_forwarder_keeps_each_agents_observations_and_shares_the_write_gate() {
        use std::task::{Context, Waker};

        let root = tempfile::tempdir().unwrap();
        let environments = root.path().join("environments");
        std::fs::create_dir_all(&environments).unwrap();
        let workspace = root.path().join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::write(workspace.join("a.txt"), "one").unwrap();
        let hook = module_services(&quiet_deps(vec![environments]), &[]);
        // What every assembly from one catalog is given: a fresh record, the catalog's gate.
        let gate = p1_workspace::WriteGate::new();
        let assembly = |mask: &Arc<MaskCounter>| ToolServices {
            workspace: Workspace::new(&workspace)
                .unwrap()
                .with_write_gate(gate.clone()),
            observed: ObservedFiles::new(),
            mask: mask.clone(),
            agent: None,
            environment: String::new(),
            prompt_template: String::new(),
            modules: Vec::new(),
            allowed_children: None,
            tool_concurrency: Default::default(),
        };
        let parent = Arc::new(MaskCounter::new());
        let worker = Arc::new(MaskCounter::new());

        let read = hook("p1/read", &assembly(&worker));
        read.snapshot
            .expect("the read row links the snapshot")
            .observe("a.txt".into(), b"one".to_vec())
            .await
            .unwrap();
        // The worker re-assembled with `edit` added: the assembly's record is fresh.
        let regranted = hook("p1/edit", &assembly(&worker));
        assert_eq!(
            regranted
                .snapshot
                .clone()
                .unwrap()
                .check("a.txt".into(), b"one".to_vec())
                .await
                .unwrap(),
            p1_module_runtime::SnapshotObservation::Unchanged,
            "the re-assembled worker still observed what it read"
        );
        let parents = hook("p1/edit", &assembly(&parent));
        assert_eq!(
            parents
                .snapshot
                .clone()
                .unwrap()
                .check("a.txt".into(), b"one".to_vec())
                .await
                .unwrap(),
            p1_module_runtime::SnapshotObservation::NeverObserved,
            "another agent never shares the worker's observations"
        );

        // One gate: while the worker holds it, the parent's mutation cannot begin.
        let held = regranted
            .workspace_mutation
            .expect("the edit row links the mutation")
            .begin()
            .await;
        let parent_mutation = parents.workspace_mutation.expect("the edit row");
        let mut waiting = std::pin::pin!(parent_mutation.begin());
        assert!(
            waiting
                .as_mut()
                .poll(&mut Context::from_waker(Waker::noop()))
                .is_pending(),
            "the parent waits for the worker's write gate"
        );
        drop(held);
        waiting.await;
    }

    #[test]
    fn the_mutation_mode_is_the_row_of_the_component() {
        assert_eq!(mutation_mode("p1/edit"), Some(MutationPolicy::Observed));
        assert_eq!(mutation_mode("p1/write"), Some(MutationPolicy::Observed));
        assert_eq!(
            mutation_mode("p1/patch"),
            Some(MutationPolicy::PatchAuthorized)
        );
        // The read component links no mutation, and neither does any other module: a
        // component is granted only what its own row assembles.
        for module in ["p1/read", "p1/search", "p1/worker-start", "p1/summary"] {
            assert_eq!(mutation_mode(module), None, "{module}");
        }
    }

    /// One catalog compiles a shipped package at most once, and the assembly it serves is
    /// therefore fixed for that catalog's whole life: no package can be replaced under a
    /// turn in progress (ADR-0083 rule 7). A replacement is read by the NEXT generation,
    /// whose catalog is built again from the release and installed between complete turns
    /// (ADR-0078 §4, the machinery S5.7's reload suite covers) — never by the running one.
    ///
    /// The shared step loads the package and hands it to the entry's registration, so the
    /// catalog's `shell` entry holds that one compilation; the next catalog's step reads the
    /// release again and hands out its own.
    #[test]
    fn a_catalog_loads_its_package_once_and_the_next_catalog_reads_the_release_again() {
        let root = tempfile::tempdir().unwrap();
        let environments = root.path().join("environments");
        std::fs::create_dir_all(&environments).unwrap();
        let deps = quiet_deps(vec![environments]);
        let loaded = Arc::new(Mutex::new(Vec::new()));
        for _ in 0..2 {
            let recorded = loaded.clone();
            register_composed_host_entries(
                &mut Catalog::new(),
                &deps,
                &[(
                    SHELL,
                    Box::new(move |_catalog: &mut Catalog, module: Arc<LoadedModule>| {
                        recorded.lock().unwrap().push(module);
                        Ok(())
                    }),
                )],
            )
            .expect("the release's p1/shell loads");
        }
        let loaded = loaded.lock().unwrap();
        assert_eq!(
            loaded.len(),
            2,
            "one load per catalog, one hand-over per load"
        );
        let (one, replacement) = (&loaded[0], &loaded[1]);
        assert_eq!(one.identity().implementation, package_of(SHELL));
        assert!(
            !Arc::ptr_eq(one, replacement),
            "a new generation gets its own compilation of the release"
        );
    }

    /// A host entry's package is linked with exactly the services its manifest grants: the
    /// `p1/shell` manifest imports `process`, so linking the loaded package without the host's
    /// process service is a `MissingService`, never a component that runs unlinked. The
    /// completion half of the same rule is
    /// `crates/p1-module-tests/tests/finish_boundary.rs`, and the entry that DOES link the
    /// service is `tests/activation.rs`.
    #[tokio::test]
    async fn a_shell_service_that_is_absent_fails_assembly() {
        let root = tempfile::tempdir().unwrap();
        let environments = root.path().join("environments");
        std::fs::create_dir_all(&environments).unwrap();
        let deps = quiet_deps(vec![environments]);
        let shell = Arc::new(Mutex::new(None));
        let recorded = shell.clone();
        register_composed_host_entries(
            &mut Catalog::new(),
            &deps,
            &[(
                SHELL,
                Box::new(move |_catalog: &mut Catalog, module: Arc<LoadedModule>| {
                    *recorded.lock().unwrap() = Some(module);
                    Ok(())
                }),
            )],
        )
        .expect("the release's p1/shell loads");
        let module = shell
            .lock()
            .unwrap()
            .clone()
            .expect("the shell package was handed over");
        assert_eq!(module.identity().implementation, package_of(SHELL));
        match wasm_tool(
            &module,
            Services::default(),
            ExecutionLimits::default(),
            &Arc::new(MaskCounter::new()),
        ) {
            Err(ToolError::Link {
                source: LinkError::MissingService(capability),
                ..
            }) => assert_eq!(capability, "process"),
            Err(other) => panic!("wrong error: {other}"),
            Ok(_) => panic!("the shell must not link without a process service"),
        }
    }
    #[tokio::test]
    async fn background_component_start_never_verifies_and_end_notifies_once() {
        use p1_contracts::{ToolInput, ToolResultItem, ToolStatus};
        use p1_module_runtime::jobs::{JobFinished, JobObserver, JobState};
        struct Observe {
            log: Arc<crate::activity::ActivityLog>,
            send: tokio::sync::mpsc::UnboundedSender<JobFinished>,
        }
        impl JobObserver for Observe {
            fn started(&self) -> Option<u64> {
                self.log.job_started()
            }
            fn ended(
                &self,
                command: &str,
                baseline: Option<u64>,
                status: &p1_module_runtime::ExitStatus,
            ) {
                self.log.job_finished(
                    command.into(),
                    baseline,
                    match status {
                        p1_module_runtime::ExitStatus::Code(n) => Some(*n),
                        _ => None,
                    },
                );
            }
            fn finished(&self, job: JobFinished) {
                self.send.send(job).unwrap();
            }
        }
        let root = tempfile::tempdir().unwrap();
        let deps = quiet_deps(vec![]);
        let mask = Arc::new(MaskCounter::new());
        let module = crate::catalog::capabilities::built_package("p1-module-shell");
        let process = Arc::new(ProcessService::new(root.path()));
        let jobs = deps.jobs.install(
            &mask,
            Arc::new(p1_module_runtime::jobs::JobRegistry::new(
                process.clone(),
                deps.tool_outputs.clone(),
                mask.secrets().clone(),
            )),
        );
        let shell = wasm_tool(
            &module,
            Services {
                process: Some(Arc::new(ProcessCapability::new(process))),
                process_jobs: Some(Arc::new(crate::jobs::JobStarter(
                    jobs.clone(),
                    jobs.handover(),
                ))),
                tool_outputs: Some(Arc::new(CallOutputs::new(
                    deps.tool_outputs.clone(),
                    mask.secrets().clone(),
                ))),
                ..Services::default()
            },
            ExecutionLimits::default(),
            &mask,
        )
        .unwrap();
        let log = Arc::new(crate::activity::ActivityLog::default());
        let (send, mut receive) = tokio::sync::mpsc::unbounded_channel();
        jobs.observe(Arc::new(Observe {
            log: log.clone(),
            send,
        }));
        assert!(
            std::process::Command::new("mkfifo")
                .arg(root.path().join("release"))
                .status()
                .unwrap()
                .success()
        );
        let command = "read answer < release; printf 'unfiltered output'";
        let call = p1_contracts::ToolCall {
            call_id: "start".into(),
            name: "shell".into(),
            input: ToolInput::Json(
                serde_json::json!({"command":command,"background":true}).to_string(),
            ),
        };
        log.record_started_by(&call, Effect::Executes, true);
        let result = shell
            .execute(
                &call,
                ToolContext {
                    cancel: p1_contracts::CancellationToken::new(),
                },
            )
            .await;
        assert_eq!(result.status, ToolStatus::Ok);
        assert!(result.content.contains("Background job j1 started"));
        assert_eq!(shell.take_command_exit_code("start"), None);
        log.record_finished_with_exit(
            &ToolResultItem {
                call_id: "start".into(),
                name: "shell".into(),
                status: result.status,
                content: result.content,
            },
            None,
        );
        assert_eq!(log.evidence_runs()[0].exit_code, None);
        assert!(matches!(
            jobs.status("j1").unwrap(),
            JobState::Running { .. }
        ));
        let release = root.path().join("release");
        tokio::task::spawn_blocking(move || std::fs::write(release, "go\n"))
            .await
            .unwrap()
            .unwrap();
        let notification = receive.recv().await.unwrap();
        assert_eq!(notification.tail, "unfiltered output");
        assert_eq!(
            jobs.status("j1").unwrap(),
            JobState::Ended(notification.end)
        );
        let runs = log.evidence_runs();
        assert_eq!(runs.last().unwrap().exit_code, Some(0));
        assert!(runs.last().unwrap().order > runs[0].order);
        assert!(receive.try_recv().is_err());
        jobs.shutdown().await;
    }
}
