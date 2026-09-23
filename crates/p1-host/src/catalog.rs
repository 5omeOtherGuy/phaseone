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

/// Same override rules as [`apply_face`], for the workflow tools' own `ToolFace`
/// type.
#[cfg(feature = "workflows")]
macro_rules! apply_workflow_face {
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
            Arc::new(tool.with_face(p1_tool_workflow::ToolFace::new(name, description), &variant))
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

/// The compiled whole-provider keys: a catalog key that consumes no profile. A route
/// file may not take one of these ids, and a `route` naming one is the wrong-form
/// error the whole provider itself reports. The list is EMPTY since ADR-0039 step 4b:
/// `anthropic-subscription` and `openai-codex-subscription` are route files now. The
/// MECHANISM stays for test fakes and the next whole provider: a key registered
/// outside the route files still reports its wrong form through [`reject_profile`],
/// which is why that helper is kept.
pub const WHOLE_PROVIDERS: [&str; 0] = [];

fn register_providers(catalog: &mut Catalog, deps: &HostDeps) -> Result<(), String> {
    register_routes(catalog, deps)
}

/// Register one factory per route file, under the route id. The closure owns that
/// route's data and calls the compiled constructor for its adapter key; nothing about
/// a route is compiled into this crate (spec §2).
fn register_routes(catalog: &mut Catalog, deps: &HostDeps) -> Result<(), String> {
    // Composed once per catalog: the credential chain reads no file until a source
    // is accessed, so this stays cheap and touches no login.
    let locations = crate::auth::locations(deps);
    for route in crate::routes::load_all_routes(&deps.environment_dirs)? {
        if WHOLE_PROVIDERS.contains(&route.id.as_str()) {
            return Err(format!(
                "route `{}` collides with the compiled whole-provider key of the same name; \
                 rename the route file, a route cannot shadow a whole provider",
                route.id
            ));
        }
        let transport = deps.transport.clone();
        // ADR-0047 §1: the host composes the REAL WebSocket connector next to the
        // HTTP transport, once per catalog. Composition opens no socket: only a
        // request on a route that asks for `transport = "websocket"` connects.
        let ws: Arc<dyn p1_provider_http::ws::WsConnector> =
            Arc::new(p1_provider_http::ws::TungsteniteConnector::new());
        let locations = locations.clone();
        let route = Arc::new(route);
        let data = route.clone();
        catalog.provider(
            &route.id,
            Box::new(move |spec: &ProviderSpec| {
                let profile = require_profile(spec)?;
                let binding = data.binding(&profile.id)?;
                let credentials =
                    crate::auth::credential_source_at(&data, transport.clone(), &locations);
                route_provider(
                    &data,
                    binding,
                    profile,
                    transport.clone(),
                    ws.clone(),
                    credentials,
                )
            }),
        );
    }
    Ok(())
}

/// The one line `p1 env show` prints for a route's credential (spec §4): WHICH
/// source it would come from, never a value. `None` for an environment that names
/// no route (a whole provider, which has no `[credential]` table).
pub fn credential_line(
    environment: &p1_assembly::EnvironmentFile,
    environment_dirs: &[PathBuf],
    locations: &p1_auth::Locations,
) -> Result<Option<String>, String> {
    if environment.profile.is_none() {
        return Ok(None);
    }
    credential_line_for_route(&environment.provider, environment_dirs, locations).map(Some)
}

/// The same line for a route id alone: `p1 models` prints it for every model, so
/// both commands show one wording from one probe.
pub fn credential_line_for_route(
    route_id: &str,
    environment_dirs: &[PathBuf],
    locations: &p1_auth::Locations,
) -> Result<String, String> {
    let route = crate::routes::load_route_by_id(environment_dirs, route_id)?;
    Ok(p1_auth::describe(&route.id, &route.credential, locations).line())
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
/// error that says which form to write instead; there is no fallback. No shipped key
/// is whole any more (see [`WHOLE_PROVIDERS`]); this stays PUBLIC because it is the
/// half of the whole-provider mechanism a test fake's factory composes with — the
/// next whole provider registers exactly like the fakes do.
pub fn reject_profile(spec: &ProviderSpec) -> Result<(), String> {
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
///
/// `ws` is the WebSocket connector, next to the HTTP transport (ADR-0047 §1): the
/// production factory passes the real one and a test or live check injects its own,
/// so a test that composes a shipped WebSocket route never opens a socket. A route
/// that does not ask for WebSocket ignores it.
pub fn route_provider(
    route: &crate::routes::RouteFile,
    binding: &crate::routes::ModelBinding,
    profile: Arc<p1_model_profile::ModelProfile>,
    transport: Arc<dyn p1_provider_http::Transport>,
    ws: Arc<dyn p1_provider_http::ws::WsConnector>,
    credentials: Arc<dyn p1_provider_http::CredentialSource>,
) -> Result<Arc<dyn Provider>, String> {
    use crate::routes::AdapterSettings;
    match route.settings()? {
        AdapterSettings::OpenAiChat(settings) => {
            let provider = p1_provider_openai_chat::ChatProvider::new(
                chat_route_from(route, binding, &profile, settings)?,
                &binding.wire_model,
                profile,
                transport,
                credentials,
            )
            .map_err(|error| error.to_string())?;
            Ok(Arc::new(provider) as Arc<dyn Provider>)
        }
        AdapterSettings::AnthropicMessages(settings) => {
            let provider = p1_provider_anthropic::AnthropicProvider::new(
                messages_route_from(route, settings),
                &binding.wire_model,
                profile,
                transport,
                credentials,
            )
            .map_err(|error| error.to_string())?;
            Ok(Arc::new(provider) as Arc<dyn Provider>)
        }
        AdapterSettings::OpenAiResponses(settings) => {
            // ADR-0047 §1: a route that asks for `transport = "websocket"` gets the
            // injected connector here, at composition. The provider refuses a
            // WebSocket route without one, so the two cannot drift apart.
            let transport_mode = settings.transport;
            let mut composition = p1_provider_openai::OpenAiCodexProvider::builder(
                responses_route_from(route, settings),
                &binding.wire_model,
                profile,
                transport,
                credentials,
            );
            if transport_mode == p1_provider_openai::ResponsesTransport::Websocket {
                composition = composition.with_ws_connector(ws);
            }
            let provider = composition.build().map_err(|error| error.to_string())?;
            Ok(Arc::new(provider) as Arc<dyn Provider>)
        }
    }
}

/// The chat adapter's view of one route file: the file's endpoint and static headers,
/// the settings the adapter parses for itself, and the profile's output ceiling
/// lowered by the binding's.
pub fn chat_route(
    route: &crate::routes::RouteFile,
    binding: &crate::routes::ModelBinding,
    profile: &p1_model_profile::ModelProfile,
) -> Result<p1_provider_openai_chat::ChatRoute, String> {
    // A chat route file names `openai-chat`; any other adapter key is the wrong
    // function, not a silent fallback.
    let crate::routes::AdapterSettings::OpenAiChat(settings) = route.settings()? else {
        return Err(format!(
            "route \"{}\" names adapter \"{}\", not openai-chat",
            route.id, route.adapter
        ));
    };
    chat_route_from(route, binding, profile, settings)
}

fn chat_route_from(
    route: &crate::routes::RouteFile,
    binding: &crate::routes::ModelBinding,
    profile: &p1_model_profile::ModelProfile,
    settings: p1_provider_openai_chat::ChatAdapterSettings,
) -> Result<p1_provider_openai_chat::ChatRoute, String> {
    use p1_provider_openai_chat::{ChatLimits, ChatRoute};
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

/// The Messages adapter's view of one route file: the recorded origin route, the
/// endpoint and the account behaviour the file names (spec §7.2). It carries no
/// static headers today, so a `[headers]` table on such a route is empty in every
/// shipped file; nothing else about a Messages route is data.
pub fn messages_route(
    route: &crate::routes::RouteFile,
) -> Result<p1_provider_anthropic::MessagesRoute, String> {
    let crate::routes::AdapterSettings::AnthropicMessages(settings) = route.settings()? else {
        return Err(format!(
            "route \"{}\" names adapter \"{}\", not anthropic-messages",
            route.id, route.adapter
        ));
    };
    Ok(messages_route_from(route, settings))
}

fn messages_route_from(
    route: &crate::routes::RouteFile,
    settings: p1_provider_anthropic::MessagesAdapterSettings,
) -> p1_provider_anthropic::MessagesRoute {
    p1_provider_anthropic::MessagesRoute {
        origin_route: route.origin_route.clone(),
        endpoint: route.endpoint.clone(),
        account: settings.account,
    }
}

/// The Responses adapter's view of one route file: the recorded origin route, the
/// endpoint and the account behaviour the file names (spec §7.2). Like a Messages
/// route it carries no static headers today, so a `[headers]` table on such a route
/// is empty in every shipped file; nothing else about a Responses route is data.
pub fn responses_route(
    route: &crate::routes::RouteFile,
) -> Result<p1_provider_openai::ResponsesRoute, String> {
    let crate::routes::AdapterSettings::OpenAiResponses(settings) = route.settings()? else {
        return Err(format!(
            "route \"{}\" names adapter \"{}\", not openai-responses",
            route.id, route.adapter
        ));
    };
    Ok(responses_route_from(route, settings))
}

fn responses_route_from(
    route: &crate::routes::RouteFile,
    settings: p1_provider_openai::ResponsesAdapterSettings,
) -> p1_provider_openai::ResponsesRoute {
    p1_provider_openai::ResponsesRoute {
        origin_route: route.origin_route.clone(),
        endpoint: route.endpoint.clone(),
        account: settings.account,
        transport: settings.transport,
    }
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
            Ok(apply_delegate_face!(
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
                p1_tool_delegate::WorkerContinueTool::new(service_for.clone(), grantable.clone()),
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
            Ok(apply_workflow_face!(
                p1_tool_workflow::WorkflowStartTool::new(service_for.clone()),
                spec
            ))
        }),
    );

    let service_for = service.clone();
    catalog.tool(
        "workflow_status",
        Box::new(move |spec: &ToolSpec, _services: &ToolServices| {
            Ok(apply_workflow_face!(
                p1_tool_workflow::WorkflowStatusTool::new(service_for.clone()),
                spec
            ))
        }),
    );

    let service_for = service.clone();
    catalog.tool(
        "workflow_result",
        Box::new(move |spec: &ToolSpec, _services: &ToolServices| {
            Ok(apply_workflow_face!(
                p1_tool_workflow::WorkflowResultTool::new(service_for.clone()),
                spec
            ))
        }),
    );

    catalog.tool(
        "workflow_cancel",
        Box::new(move |spec: &ToolSpec, _services: &ToolServices| {
            Ok(apply_workflow_face!(
                p1_tool_workflow::WorkflowCancelTool::new(service.clone()),
                spec
            ))
        }),
    );
}
