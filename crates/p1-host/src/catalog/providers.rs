//! The provider half of the catalog: one factory per route file, and the composition
//! of a route file with a profile binding into a provider.

use std::path::PathBuf;
use std::sync::Arc;

use p1_assembly::{Catalog, ProviderSpec};
use p1_contracts::Provider;

use crate::HostDeps;

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
