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
/// Provider keys: `anthropic-subscription`, `openai-codex-subscription`, plus one
/// key per route file found in `<environments dir>/../routes`
/// (`docs/design/routes-and-profiles.md` §2). Tool keys: `read`, `edit`, `write`,
/// `grep`, `shell`, `apply_patch`, and — with the `delegation` feature and a worker
/// service present — the four `worker_*` tools.
///
/// A routed key is selected with `route` + `profile` and refuses the whole-provider
/// form; the whole providers above refuse a profile. A route file whose id collides
/// with a whole-provider key is a start-up error, reported here before any run.
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
    register_delegation_tools(&mut catalog, service);
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

/// The compiled whole-provider keys: a catalog key that consumes no profile. A route
/// file may not take one of these ids, and a `route` naming one is the wrong-form
/// error the whole provider itself reports.
pub const WHOLE_PROVIDERS: [&str; 2] = ["anthropic-subscription", "openai-codex-subscription"];

fn register_providers(catalog: &mut Catalog, deps: &HostDeps) -> Result<(), String> {
    let transport = deps.transport.clone();
    catalog.provider(
        WHOLE_PROVIDERS[0],
        Box::new(move |spec: &ProviderSpec| {
            reject_profile(spec)?;
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
        WHOLE_PROVIDERS[1],
        Box::new(move |spec: &ProviderSpec| {
            reject_profile(spec)?;
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

    register_routes(catalog, deps)
}

/// Register one factory per route file, under the route id. The closure owns that
/// route's data and calls the compiled constructor for its adapter key; nothing about
/// a route is compiled into this crate (spec §2).
fn register_routes(catalog: &mut Catalog, deps: &HostDeps) -> Result<(), String> {
    for route in crate::routes::load_all_routes(&deps.environment_dirs)? {
        if WHOLE_PROVIDERS.contains(&route.id.as_str()) {
            return Err(format!(
                "route `{}` collides with the compiled whole-provider key of the same name; \
                 rename the route file, a route cannot shadow a whole provider",
                route.id
            ));
        }
        let transport = deps.transport.clone();
        let route = Arc::new(route);
        let data = route.clone();
        catalog.provider(
            &route.id,
            Box::new(move |spec: &ProviderSpec| {
                let profile = require_profile(spec)?;
                let binding = data.binding(&profile.id)?;
                let credentials = crate::auth::SubscriptionCredentials::from_ref(&data.credential)?;
                route_provider(
                    &data,
                    binding,
                    profile,
                    transport.clone(),
                    Arc::new(credentials),
                )
                .map(|provider| Arc::new(provider) as Arc<dyn Provider>)
                .map_err(|error| error.to_string())
            }),
        );
    }
    Ok(())
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

/// The profile an environment selected, for a key that is a chat route. The old
/// form on such a key is a load error that says which form to write instead: a
/// route's model policy lives in a profile, and there is no fallback.
fn require_profile(spec: &ProviderSpec) -> Result<Arc<p1_model_profile::ModelProfile>, String> {
    spec.profile.clone().ok_or_else(|| {
        format!(
            "`{}` is a chat route and needs a model profile: write `route` and `profile` in the \
             environment file instead of `provider`, `model` and `family`",
            spec.key
        )
    })
}

/// A whole provider that consumes no profile. The new form on such a key is a load
/// error that says which form to write instead; there is no fallback.
fn reject_profile(spec: &ProviderSpec) -> Result<(), String> {
    if spec.profile.is_some() {
        return Err(format!(
            "`{}` is a whole provider and takes no profile: write `provider`, `model` and \
             `family` in the environment file instead of `route` and `profile`",
            spec.key
        ));
    }
    Ok(())
}

/// The composition of one route file with one profile binding: the adapter key the
/// file names builds its provider from the file's own data (spec §2 step 4). The
/// catalog factory, the live checks and the tests all come through here, so a route
/// has exactly one construction path.
pub fn route_provider(
    route: &crate::routes::RouteFile,
    binding: &crate::routes::ModelBinding,
    profile: Arc<p1_model_profile::ModelProfile>,
    transport: Arc<dyn p1_provider_http::Transport>,
    credentials: Arc<dyn p1_provider_http::CredentialSource>,
) -> Result<p1_provider_openai_chat::ChatProvider, String> {
    p1_provider_openai_chat::ChatProvider::new(
        chat_route(route, binding, &profile)?,
        &binding.wire_model,
        profile,
        transport,
        credentials,
    )
    .map_err(|error| error.to_string())
}

/// The chat adapter's view of one route file: the file's endpoint and static headers,
/// the settings the adapter parses for itself, and the profile's output ceiling
/// lowered by the binding's.
pub fn chat_route(
    route: &crate::routes::RouteFile,
    binding: &crate::routes::ModelBinding,
    profile: &p1_model_profile::ModelProfile,
) -> Result<p1_provider_openai_chat::ChatRoute, String> {
    use p1_provider_openai_chat::{ChatLimits, ChatRoute};
    // One adapter key today, so the pattern is irrefutable; a second variant turns
    // this into a compile error rather than a silent wrong adapter.
    let crate::routes::AdapterSettings::OpenAiChat(settings) = route.settings()?;
    // `user-agent` stays compiled: it carries this crate's version, so a route file
    // cannot stale it. The file's own headers follow, sorted by name (a `BTreeMap`,
    // so the order is stable), and a file cannot name a secret-looking one.
    let mut headers = vec![(
        "user-agent".to_string(),
        concat!("p1/", env!("CARGO_PKG_VERSION")).to_string(),
    )];
    headers.extend(
        route
            .headers
            .iter()
            .map(|(name, value)| (name.clone(), value.clone())),
    );
    Ok(ChatRoute {
        origin_route: route.origin_route.clone(),
        endpoint: route.endpoint.clone(),
        headers,
        session_header: settings.session_header,
        dialect: settings.dialect,
        limits: ChatLimits {
            max_output_tokens: lower_ceiling(profile.max_output_tokens, binding.output_limit),
        },
    })
}

/// A route may restrict a profile's ceiling, never enlarge it. Unknown on one side
/// keeps the known one; unknown on both stays unknown.
fn lower_ceiling(profile: Option<u32>, route: Option<u32>) -> Option<u32> {
    match (profile, route) {
        (Some(profile), Some(route)) => Some(profile.min(route)),
        (profile, route) => profile.or(route),
    }
}

fn register_standard_tools(
    catalog: &mut Catalog,
    deps: &HostDeps,
    sandbox: SandboxMode,
    sandbox_write: &[PathBuf],
    sandbox_read: &[PathBuf],
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
