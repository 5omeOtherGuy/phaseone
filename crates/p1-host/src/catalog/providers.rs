//! The provider half of the catalog: one factory per route file, and the composition
//! of a route file with a profile binding into a provider.
//!
//! Since S4.9 a route's `adapter` key names a provider COMPONENT (ADR-0086): the factory
//! activates the component the installed release ships, configured from the route's own data
//! and the selected profile's text, and the native transport broker sends every request. The
//! native adapter a route used to build stays reachable in exactly two cases, both stated in
//! [`ProviderComponents::activate`]: the Responses WebSocket transport (S5.5) and a host with
//! no release module set installed.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use p1_assembly::{Catalog, ProviderSpec};
use p1_contracts::Provider;
use p1_module_runtime::{
    ExecutionLimits, LoadedModule, Loader, ProviderSettings, ReleaseManifest, WasmProvider,
};
use p1_provider_http::{CredentialSource, Transport, ws::WsConnector};

use crate::HostDeps;
use crate::routes::RouteFile;

/// The compiled whole-provider keys: a catalog key that consumes no profile. A route
/// file may not take one of these ids, and a `route` naming one is the wrong-form
/// error the whole provider itself reports. The list is EMPTY since ADR-0039 step 4b:
/// `anthropic-subscription` and `openai-codex-subscription` are route files now. The
/// MECHANISM stays for test fakes and the next whole provider: a key registered
/// outside the route files still reports its wrong form through [`reject_profile`],
/// which is why that helper is kept.
pub const WHOLE_PROVIDERS: [&str; 0] = [];

pub(super) fn register_providers(catalog: &mut Catalog, deps: &HostDeps) -> Result<(), String> {
    register_routes(catalog, deps)
}

/// Register one factory per route file, under the route id. The closure owns that
/// route's data and calls the compiled constructor for its adapter key; nothing about
/// a route is compiled into this crate (spec §2).
fn register_routes(catalog: &mut Catalog, deps: &HostDeps) -> Result<(), String> {
    // Composed once per catalog: the credential chain reads no file until a source
    // is accessed, so this stays cheap and touches no login.
    let locations = crate::auth::locations(deps);
    // The provider components the installed release ships, discovered once per catalog
    // through the one manifest path modules are read from (ADR-0079). A host with no module
    // set installed has none, and every route keeps the native adapter.
    let components = Arc::new(ProviderComponents::installed()?);
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
        let components = components.clone();
        let environment_dirs = deps.environment_dirs.clone();
        let route = Arc::new(route);
        let data = route.clone();
        catalog.provider(
            &route.id,
            Box::new(move |spec: &ProviderSpec| {
                let profile = require_profile(spec)?;
                let credentials =
                    crate::auth::credential_source_at(&data, transport.clone(), &locations);
                components.activate(
                    &environment_dirs,
                    &data,
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
/// file names builds its provider from the file's own data (spec §2 step 4). The live
/// checks and the tests come through here, and so does the catalog factory wherever no
/// provider component can serve the route today: [`ProviderComponents::activate`] builds the
/// component the release ships, and this native composition is what remains.
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

/// The provider component each adapter key names (ADR-0086). One `match` at the root: a route
/// file's `adapter` is the whole selection of a component, so there is no registry and
/// nothing registers itself. Route files and profiles are unchanged by this — a profile stays
/// data the component parses.
pub fn provider_component(adapter: &str) -> Result<&'static str, String> {
    match adapter {
        "anthropic-messages" => Ok("p1/provider-anthropic"),
        "openai-responses" => Ok("p1/provider-openai"),
        "openai-chat" => Ok("p1/provider-openai-chat"),
        other => Err(format!(
            "adapter \"{other}\" names no provider component; the adapters with one are {}",
            crate::routes::ADAPTER_KEYS.join(", ")
        )),
    }
}

/// The provider components one installed release ships, and the only way a provider component
/// is read: by its manifest name through the runtime loader, never from a path or bytes a
/// route could name (freeze item 6).
pub struct ProviderComponents {
    /// The release's loader, or `None` when the host has no module set installed.
    loader: Option<Loader>,
    /// One compiled component per module name, so the routes that name the same adapter
    /// configure the same bytes.
    loaded: Mutex<HashMap<String, Arc<LoadedModule>>>,
}

impl ProviderComponents {
    /// A host with no release module set: no route has a component to activate.
    fn none() -> Self {
        Self {
            loader: None,
            loaded: Mutex::new(HashMap::new()),
        }
    }

    /// The components of the release whose manifest is `path`.
    pub fn read(path: &Path) -> Result<Self, String> {
        let name = |message: String| format!("{}: {message}", path.display());
        let manifest = ReleaseManifest::read(path).map_err(|error| name(error.to_string()))?;
        manifest
            .check_unique_digests()
            .map_err(|error| name(error.to_string()))?;
        let root = path.parent().unwrap_or(Path::new("."));
        let loader = Loader::new(manifest, root)
            .map_err(|error| format!("cannot start the module runtime: {error}"))?;
        Ok(Self {
            loader: Some(loader),
            loaded: Mutex::new(HashMap::new()),
        })
    }

    /// The components of the installed release module set: the one discovery path (S1's
    /// `official_release_manifest`, ADR-0079). A build that has no module set — a development
    /// checkout, where S3.8.0's debug discovery is what finds the built components — has
    /// none, and every route keeps the native adapter.
    pub fn installed() -> Result<Self, String> {
        let Some(path) = super::modules::official_release_manifest() else {
            return Ok(Self::none());
        };
        if !path.is_file() {
            return Ok(Self::none());
        }
        Self::read(&path)
    }

    /// The compiled component `name`.
    fn module(&self, name: &str) -> Result<Arc<LoadedModule>, String> {
        let loader = self
            .loader
            .as_ref()
            .expect("a component is only asked of a release module set");
        let mut loaded = self.loaded.lock().expect("the component cache");
        if let Some(module) = loaded.get(name) {
            return Ok(module.clone());
        }
        let module = Arc::new(loader.load(name).map_err(|error| error.to_string())?);
        loaded.insert(name.to_owned(), module.clone());
        Ok(module)
    }

    /// The provider one environment's route file and profile activate, before the first turn:
    /// the route's `adapter` selects the component, the route's own data and the profile's
    /// text configure it, and the broker sends with the route's credential and endpoint, which
    /// no component can name (ADR-0086). A refusal names the route and the module and is
    /// reported like a route-file load error; nothing is sent.
    ///
    /// The native adapter a route used to build stays reachable in exactly two cases: the
    /// Responses route's WebSocket transport, which this broker does not send yet (S5.5), and
    /// a host with no release module set installed (above).
    pub fn activate(
        &self,
        environment_dirs: &[PathBuf],
        route: &RouteFile,
        profile: Arc<p1_model_profile::ModelProfile>,
        transport: Arc<dyn Transport>,
        ws: Arc<dyn WsConnector>,
        credentials: Arc<dyn CredentialSource>,
    ) -> Result<Arc<dyn Provider>, String> {
        let binding = route.binding(&profile.id)?;
        let name = provider_component(&route.adapter)
            .map_err(|reason| selection_refusal(route, reason))?;
        let keeps_native =
            keeps_native(route).map_err(|reason| selection_refusal(route, reason))?;
        if keeps_native || self.loader.is_none() {
            return route_provider(route, binding, profile, transport, ws, credentials);
        }
        let module = self
            .module(name)
            .map_err(|reason| activation_refusal(route, name, reason))?;
        let settings = ProviderSettings {
            origin_route: route.origin_route.clone(),
            endpoint: route.endpoint.clone(),
            model: profile.id.clone(),
            wire_model: binding.wire_model.clone(),
            adapter_settings: route.component_adapter_settings(
                binding,
                &profile.id,
                &profile_text(environment_dirs, &profile.id)
                    .map_err(|reason| activation_refusal(route, name, reason))?,
            ),
        };
        WasmProvider::new(
            &module,
            settings,
            credentials,
            transport,
            ExecutionLimits::default(),
        )
        .map(|provider| Arc::new(provider) as Arc<dyn Provider>)
        .map_err(|error| activation_refusal(route, module.name(), error.to_string()))
    }
}

/// Whether only the native adapter can serve this route today: the Responses route's
/// WebSocket transport is S5.5's, and this broker sends the HTTP lowering only.
fn keeps_native(route: &RouteFile) -> Result<bool, String> {
    Ok(matches!(
        route.settings()?,
        crate::routes::AdapterSettings::OpenAiResponses(settings)
            if settings.transport == p1_provider_openai::ResponsesTransport::Websocket
    ))
}

/// The refusal of a route that cannot select a provider at all: no component serves its
/// adapter key, or its own settings do not parse. Both are the route file's fault, reported
/// like a route-file load error and before anything is sent.
fn selection_refusal(route: &RouteFile, reason: impl std::fmt::Display) -> String {
    format!(
        "route \"{}\" cannot activate its provider: {reason}",
        route.id
    )
}

/// The one wording of an activation refusal: the route that cannot become a provider, the
/// component it names, and why. Nothing has been sent when it is printed.
fn activation_refusal(
    route: &RouteFile,
    component: &str,
    reason: impl std::fmt::Display,
) -> String {
    format!(
        "route \"{}\" cannot activate provider component {component}: {reason}",
        route.id
    )
}

/// The text of `profiles/<id>.toml` next to the environment directories, highest priority
/// first: the same file `p1-assembly` parsed for this environment. A component reads no file,
/// so the profile crosses the boundary as its text (ADR-0086).
fn profile_text(environment_dirs: &[PathBuf], id: &str) -> Result<String, String> {
    let mut searched: Vec<PathBuf> = Vec::new();
    for dir in environment_dirs {
        let path = dir.join("../profiles").join(format!("{id}.toml"));
        match std::fs::read_to_string(&path) {
            Ok(text) => return Ok(text),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => searched.push(path),
            Err(error) => return Err(format!("{}: {error}", path.display())),
        }
    }
    let searched: Vec<String> = searched
        .iter()
        .map(|path| path.display().to_string())
        .collect();
    Err(format!(
        "profile `{id}` was not found in {}",
        searched.join(", ")
    ))
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
        client_identity: settings.client_identity,
        limits: ChatLimits {
            max_output_tokens: lower_ceiling(profile.max_output_tokens, binding.output_limit),
        },
    })
}

/// The Messages adapter's view of one route file: the recorded origin route, the
/// endpoint, the account behaviour the file names (spec §7.2) and whether it requests
/// the 1M context window (`long_context`, ADR-0063). It carries no static headers
/// today, so a `[headers]` table on such a route is empty in every shipped file.
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
        long_context: settings.long_context,
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
