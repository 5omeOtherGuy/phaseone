//! The native adapters' route values composed from one route file: what `p1_host::catalog`
//! exported as `chat_route`, `messages_route` and `responses_route` until S7.10-R4 took the
//! adapter crates out of the host's normal dependencies. Production activates the provider
//! COMPONENT a route names (ADR-0086); these values are the reference the tests hold that
//! component, and the Responses route's native WebSocket transport, against.
//!
//! Shared by path with the other crates whose tests compose the native adapters
//! (`p1-module-tests`), so every copy of the composition is this one.

#![allow(dead_code)]

use p1_host::routes::{ModelBinding, RouteFile};
use p1_model_profile::ModelProfile;

/// The route file's `[adapter_settings]` as the value the adapter crate's own settings type
/// parses (an absent table is an empty one).
fn adapter_settings(route: &RouteFile) -> Result<serde_json::Value, String> {
    Ok(route
        .adapter_settings
        .as_ref()
        .map(serde_json::to_value)
        .transpose()
        .map_err(|error| error.to_string())?
        .unwrap_or_else(|| serde_json::json!({})))
}

fn invalid(error: serde_json::Error) -> String {
    format!("invalid `[adapter_settings]`: {error}")
}

fn require_adapter(route: &RouteFile, adapter: &str) -> Result<(), String> {
    if route.adapter == adapter {
        Ok(())
    } else {
        Err(format!(
            "route \"{}\" names adapter \"{}\", not {adapter}",
            route.id, route.adapter
        ))
    }
}

/// The chat adapter's view of one route file: the file's endpoint and static headers behind
/// the compiled `user-agent`, the settings the adapter parses for itself, and the profile's
/// output ceiling lowered by the binding's.
pub fn chat_route(
    route: &RouteFile,
    binding: &ModelBinding,
    profile: &ModelProfile,
) -> Result<p1_provider_openai_chat::ChatRoute, String> {
    use p1_provider_openai_chat::{ChatAdapterSettings, ChatLimits, ChatRoute};
    require_adapter(route, "openai-chat")?;
    let settings: ChatAdapterSettings =
        serde_json::from_value(adapter_settings(route)?).map_err(invalid)?;
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

/// The Messages adapter's view of one route file: the recorded origin route, the endpoint,
/// the account behaviour and whether it requests the 1M context window (ADR-0063).
pub fn messages_route(route: &RouteFile) -> Result<p1_provider_anthropic::MessagesRoute, String> {
    require_adapter(route, "anthropic-messages")?;
    let settings: p1_provider_anthropic::MessagesAdapterSettings =
        serde_json::from_value(adapter_settings(route)?).map_err(invalid)?;
    Ok(p1_provider_anthropic::MessagesRoute {
        origin_route: route.origin_route.clone(),
        endpoint: route.endpoint.clone(),
        account: settings.account,
        long_context: settings.long_context,
    })
}

/// The Responses adapter's view of one route file: the recorded origin route, the endpoint,
/// the account behaviour and the transport.
pub fn responses_route(route: &RouteFile) -> Result<p1_provider_openai::ResponsesRoute, String> {
    require_adapter(route, "openai-responses")?;
    let settings: p1_provider_openai::ResponsesAdapterSettings =
        serde_json::from_value(adapter_settings(route)?).map_err(invalid)?;
    Ok(p1_provider_openai::ResponsesRoute {
        origin_route: route.origin_route.clone(),
        endpoint: route.endpoint.clone(),
        account: settings.account,
        transport: settings.transport,
    })
}

/// A route may restrict a profile's ceiling, never enlarge it. Unknown on one side keeps the
/// known one; unknown on both stays unknown.
fn lower_ceiling(profile: Option<u32>, route: Option<u32>) -> Option<u32> {
    match (profile, route) {
        (Some(profile), Some(route)) => Some(profile.min(route)),
        (profile, route) => profile.or(route),
    }
}
