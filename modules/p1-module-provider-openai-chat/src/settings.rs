//! What `configure` makes of `provider-settings`: the route and the profile the native host
//! composes a `ChatProvider` from, as a pure function so a host test can hold it against
//! the native construction, and where the broker sends the lowered request.
//!
//! `adapter-settings` is the route file's `[adapter_settings]` table as JSON plus three
//! reserved keys the host adds (the settings contract of S4.6): `model_profile`
//! (`{stem, toml}`, the selected profile file), `route_headers` (the route's `[headers]`
//! table) and `model_binding` (the binding's `context_limit` and `output_limit`). The chat
//! route is built from them exactly as `chat_route_from` in p1-host's catalog builds it.

use std::collections::BTreeMap;

use p1_contracts::{ProviderError, ProviderErrorKind};
use p1_model_profile::ModelProfile;
use p1_provider_openai_chat::{ChatAdapterSettings, ChatLimits, ChatRoute, validate_composition};
use serde_json::{Map, Value};

/// The `user-agent` the native host compiles in front of a chat route's own headers. The
/// module workspace and the root workspace share one version, so this is its value.
const USER_AGENT: &str = concat!("p1/", env!("CARGO_PKG_VERSION"));

/// One configured instance: what the native constructor holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Composition {
    pub route: ChatRoute,
    pub wire_model: String,
    pub profile: ModelProfile,
}

/// The selected profile file, as the host reads it.
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct ProfileFile {
    stem: String,
    toml: String,
}

/// The route's binding of the selected profile. Absent fields are unknown.
#[derive(Default, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct ModelBinding {
    /// Carried by the contract; like the native host, nothing consumes it yet.
    #[serde(default, rename = "context_limit")]
    _context_limit: Option<u64>,
    #[serde(default)]
    output_limit: Option<u32>,
}

fn invalid(message: impl Into<String>) -> ProviderError {
    ProviderError::new(ProviderErrorKind::InvalidRequest, message)
}

/// Compose the instance from the `provider-settings` fields, failing where the native
/// host and constructor fail. `adapter_settings` is JSON text.
pub fn compose(
    origin_route: &str,
    endpoint: &str,
    wire_model: &str,
    adapter_settings: &str,
) -> Result<Composition, ProviderError> {
    let mut table: Map<String, Value> = serde_json::from_str(adapter_settings)
        .map_err(|_| invalid("the adapter settings are not a JSON object"))?;
    let profile = table
        .remove("model_profile")
        .ok_or_else(|| invalid("the adapter settings name no model profile"))?;
    let profile: ProfileFile = serde_json::from_value(profile)
        .map_err(|_| invalid("the adapter settings' model profile is not {stem, toml}"))?;
    let profile = ModelProfile::from_toml(&profile.stem, &profile.toml).map_err(invalid)?;
    // A `BTreeMap`, as the host's parsed `[headers]` table: the headers follow sorted by
    // name, so their order is stable.
    let route_headers: BTreeMap<String, String> = match table.remove("route_headers") {
        Some(headers) => serde_json::from_value(headers)
            .map_err(|_| invalid("the adapter settings' route headers are not strings"))?,
        None => BTreeMap::new(),
    };
    let binding: ModelBinding = match table.remove("model_binding") {
        Some(binding) => serde_json::from_value(binding)
            .map_err(|_| invalid("the adapter settings' model binding is not valid"))?,
        None => ModelBinding::default(),
    };
    let settings: ChatAdapterSettings = serde_json::from_value(Value::Object(table))
        .map_err(|error| invalid(format!("invalid `[adapter_settings]`: {error}")))?;
    let mut headers = vec![("user-agent".to_owned(), USER_AGENT.to_owned())];
    headers.extend(route_headers);
    let route = ChatRoute {
        origin_route: origin_route.to_owned(),
        endpoint: endpoint.to_owned(),
        headers,
        session_header: settings.session_header,
        dialect: settings.dialect,
        client_identity: settings.client_identity,
        limits: ChatLimits {
            max_output_tokens: lower_ceiling(profile.max_output_tokens, binding.output_limit),
        },
    };
    validate_composition(&route, wire_model, &profile)?;
    Ok(Composition {
        route,
        wire_model: wire_model.to_owned(),
        profile,
    })
}

/// A route may restrict a profile's ceiling, never enlarge it. Unknown on one side keeps
/// the known one; unknown on both stays unknown. The native host's rule, verbatim.
fn lower_ceiling(profile: Option<u32>, route: Option<u32>) -> Option<u32> {
    match (profile, route) {
        (Some(profile), Some(route)) => Some(profile.min(route)),
        (profile, route) => profile.or(route),
    }
}

/// Where an HTTP request of a route with `endpoint` goes, as `(base, path)`: the broker
/// sends to `base` + `path`, which is `endpoint`, the URL the native provider posts to.
///
/// A chat route's endpoint is the complete completions URL, so the portable lowering's path
/// is empty, while the frozen `http-request.path` starts with `/`. The endpoint's last
/// segment is therefore the path and the base loses it; the component lowers `path` and
/// the host gives the route authority `base` (S4.7), both through this one rule.
pub fn http_target(endpoint: &str) -> (String, String) {
    let host = endpoint.find("://").map_or(0, |scheme| scheme + 3);
    match endpoint[host..].rfind('/') {
        Some(slash) => {
            let (base, path) = endpoint.split_at(host + slash);
            (base.to_owned(), path.to_owned())
        }
        None => (endpoint.to_owned(), "/".to_owned()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use p1_contracts::{CacheKeySupport, Effort, Origin, RouteDescription};
    use p1_model_profile::ThinkingPolicy;
    use p1_provider_openai_chat::{ChatDialect, ClientIdentity};

    /// `profiles/mimo-v2.6-flash-free.toml` as shipped, comments dropped.
    const PROFILE: &str = r#"
id        = "mimo-v2.6-flash-free"
revision  = 1
model_id  = "mimo-v2.6-flash-free"
family    = "mimo"
thinking  = "enabled"

efforts        = ["high"]
default_effort = "high"

context_tokens    = 200000
max_output_tokens = 32000
"#;

    /// What the host sends for `routes/opencode-zen-1.toml` bound to that profile: the
    /// `[adapter_settings]` table plus the reserved keys (the file has no `[headers]`).
    fn settings(route_headers: Value, model_binding: Value) -> String {
        serde_json::json!({
            "dialect": "thinking-with-reasoning-alias",
            "session_header": "x-opencode-session",
            "client_identity": "opencode",
            "model_profile": {"stem": "mimo-v2.6-flash-free", "toml": PROFILE},
            "route_headers": route_headers,
            "model_binding": model_binding,
        })
        .to_string()
    }

    fn zen(route_headers: Value, model_binding: Value) -> Result<Composition, ProviderError> {
        compose(
            "openai-chat/opencode-zen-1",
            "https://opencode.ai/zen/v1/chat/completions",
            "mimo-v2.6-flash-free",
            &settings(route_headers, model_binding),
        )
    }

    #[test]
    fn the_shipped_route_composes_as_the_native_host_builds_it() {
        let composition = zen(serde_json::json!({}), serde_json::json!({})).unwrap();
        // `chat_route_from` in p1-host's catalog, over the same route file and binding.
        assert_eq!(
            composition.route,
            ChatRoute {
                origin_route: "openai-chat/opencode-zen-1".into(),
                endpoint: "https://opencode.ai/zen/v1/chat/completions".into(),
                headers: vec![("user-agent".into(), "p1/0.0.1".into())],
                session_header: Some("x-opencode-session".into()),
                dialect: ChatDialect::ThinkingWithReasoningAlias,
                client_identity: Some(ClientIdentity::Opencode),
                limits: ChatLimits {
                    max_output_tokens: Some(32000),
                },
            }
        );
        assert_eq!(
            composition.profile,
            ModelProfile {
                id: "mimo-v2.6-flash-free".into(),
                revision: 1,
                model_id: "mimo-v2.6-flash-free".into(),
                family: "mimo".into(),
                thinking: ThinkingPolicy::Enabled,
                efforts: vec![Effort::High],
                default_effort: Some(Effort::High),
                thinking_budgets: Default::default(),
                context_tokens: Some(200000),
                max_output_tokens: Some(32000),
            }
        );
        assert_eq!(
            composition.route.describe(&composition.wire_model),
            RouteDescription {
                origin: Origin {
                    route: "openai-chat/opencode-zen-1".into(),
                    model: "mimo-v2.6-flash-free".into(),
                },
                supports_freeform_tools: false,
                mandatory_prompt_prefix: None,
                reports_cost: false,
                cache_key: CacheKeySupport::Optional,
            }
        );
    }

    #[test]
    fn route_headers_follow_the_user_agent_sorted_and_the_binding_lowers_the_ceiling() {
        let composition = zen(
            serde_json::json!({"x-zeta": "z", "x-alpha": "a"}),
            serde_json::json!({"context_limit": 100000, "output_limit": 8000}),
        )
        .unwrap();
        assert_eq!(
            composition.route.headers,
            vec![
                ("user-agent".to_owned(), "p1/0.0.1".to_owned()),
                ("x-alpha".to_owned(), "a".to_owned()),
                ("x-zeta".to_owned(), "z".to_owned()),
            ]
        );
        assert_eq!(composition.route.limits.max_output_tokens, Some(8000));
        // A binding never raises the profile's ceiling.
        let raised = zen(
            serde_json::json!({}),
            serde_json::json!({"output_limit": 64000}),
        )
        .unwrap();
        assert_eq!(raised.route.limits.max_output_tokens, Some(32000));
    }

    #[test]
    fn an_unknown_adapter_setting_or_binding_field_is_refused() {
        let mut settings: Value =
            serde_json::from_str(&settings(serde_json::json!({}), serde_json::json!({}))).unwrap();
        settings["surprise"] = Value::Bool(true);
        let error = compose(
            "openai-chat/opencode-zen-1",
            "https://opencode.ai/zen/v1/chat/completions",
            "mimo-v2.6-flash-free",
            &settings.to_string(),
        )
        .unwrap_err();
        assert!(error.message.starts_with("invalid `[adapter_settings]`"));
        assert!(
            zen(
                serde_json::json!({}),
                serde_json::json!({"wire_model": "x"})
            )
            .is_err()
        );
    }

    #[test]
    fn the_http_target_is_the_endpoint_split_before_its_last_segment() {
        for endpoint in [
            "https://opencode.ai/zen/v1/chat/completions",
            "https://opencode.ai/zen/v1/chat/completions/",
            "https://example.test",
            "https://example.test/",
        ] {
            let (base, path) = http_target(endpoint);
            assert!(path.starts_with('/'), "{endpoint}: {path}");
            let url = format!("{base}{path}");
            assert!(
                url == endpoint || url == format!("{endpoint}/"),
                "{endpoint}"
            );
        }
        assert_eq!(
            http_target("https://opencode.ai/zen/v1/chat/completions"),
            (
                "https://opencode.ai/zen/v1/chat".to_owned(),
                "/completions".to_owned()
            )
        );
    }
}
