//! What `configure` makes of `provider-settings`: the route and the profile the native host
//! composes an `AnthropicProvider` from, as a pure function so a host test can hold it
//! against the native construction.
//!
//! `adapter-settings` is the route file's `[adapter_settings]` table as JSON plus three
//! reserved keys the host adds (the settings contract of S4.6): `model_profile`
//! (`{stem, toml}`, the selected profile file), `route_headers` and `model_binding`. The
//! Messages route has no static headers and no output ceiling, so, as in the native host,
//! the last two are dropped unread.

use p1_contracts::{ProviderError, ProviderErrorKind};
use p1_model_profile::ModelProfile;
use p1_provider_anthropic::{MessagesAdapterSettings, MessagesRoute, validate_composition};
use serde_json::{Map, Value};

/// One configured instance: what the native constructor holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Composition {
    pub route: MessagesRoute,
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
    table.remove("route_headers");
    table.remove("model_binding");
    let settings: MessagesAdapterSettings = serde_json::from_value(Value::Object(table))
        .map_err(|error| invalid(format!("invalid `[adapter_settings]`: {error}")))?;
    let route = MessagesRoute {
        origin_route: origin_route.to_owned(),
        endpoint: endpoint.to_owned(),
        account: settings.account,
        long_context: settings.long_context,
    };
    validate_composition(&route, wire_model, &profile)?;
    Ok(Composition {
        route,
        wire_model: wire_model.to_owned(),
        profile,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use p1_contracts::{CacheKeySupport, Effort, Origin, RouteDescription};
    use p1_model_profile::ThinkingPolicy;
    use p1_provider_anthropic::MessagesAccount;

    /// `profiles/claude-opus-5.toml` as shipped.
    const PROFILE: &str = r#"
id       = "claude-opus-5"
revision = 1
model_id = "claude-opus-5"
family   = "claude"
thinking = "effort-level"

efforts = ["low", "medium", "high", "extra_high", "max"]
"#;

    /// What the host sends for `routes/anthropic-subscription.toml` bound to that profile:
    /// the `[adapter_settings]` table plus the reserved keys.
    fn settings() -> String {
        serde_json::json!({
            "account": "claude-code-subscription",
            "long_context": true,
            "model_profile": {"stem": "claude-opus-5", "toml": PROFILE},
            "route_headers": {},
            "model_binding": {},
        })
        .to_string()
    }

    fn shipped() -> Result<Composition, ProviderError> {
        compose(
            "anthropic-messages/claude-subscription",
            "https://api.anthropic.com",
            "claude-opus-5",
            &settings(),
        )
    }

    #[test]
    fn the_shipped_route_composes_as_the_native_host_builds_it() {
        let composition = shipped().expect("the shipped route composes");
        // `messages_route_from` in p1-host's catalog, over the same route file.
        assert_eq!(
            composition.route,
            MessagesRoute {
                origin_route: "anthropic-messages/claude-subscription".into(),
                endpoint: "https://api.anthropic.com".into(),
                account: MessagesAccount::ClaudeCodeSubscription,
                long_context: true,
            }
        );
        assert_eq!(
            composition.profile,
            ModelProfile {
                id: "claude-opus-5".into(),
                revision: 1,
                model_id: "claude-opus-5".into(),
                family: "claude".into(),
                thinking: ThinkingPolicy::EffortLevel,
                efforts: vec![
                    Effort::Low,
                    Effort::Medium,
                    Effort::High,
                    Effort::ExtraHigh,
                    Effort::Max,
                ],
                default_effort: None,
                thinking_budgets: Default::default(),
                context_tokens: None,
                max_output_tokens: None,
            }
        );
        let description = composition.route.describe(&composition.wire_model);
        assert_eq!(
            description,
            RouteDescription {
                origin: Origin {
                    route: "anthropic-messages/claude-subscription".into(),
                    model: "claude-opus-5".into(),
                },
                supports_freeform_tools: false,
                mandatory_prompt_prefix: description.mandatory_prompt_prefix.clone(),
                reports_cost: false,
                cache_key: CacheKeySupport::Unsupported,
            }
        );
        assert!(description.mandatory_prompt_prefix.is_some());
    }

    #[test]
    fn an_unknown_adapter_setting_is_refused_like_the_native_host() {
        let mut settings: Value = serde_json::from_str(&settings()).unwrap();
        settings["surprise"] = Value::Bool(true);
        let error = compose(
            "anthropic-messages/claude-subscription",
            "https://api.anthropic.com",
            "claude-opus-5",
            &settings.to_string(),
        )
        .unwrap_err();
        assert_eq!(error.kind, ProviderErrorKind::InvalidRequest);
        assert!(error.message.starts_with("invalid `[adapter_settings]`"));
    }

    #[test]
    fn settings_without_a_profile_are_refused() {
        let mut settings: Value = serde_json::from_str(&settings()).unwrap();
        settings.as_object_mut().unwrap().remove("model_profile");
        let error = compose(
            "anthropic-messages/claude-subscription",
            "https://api.anthropic.com",
            "claude-opus-5",
            &settings.to_string(),
        )
        .unwrap_err();
        assert_eq!(error.kind, ProviderErrorKind::InvalidRequest);
    }

    #[test]
    fn a_route_the_native_constructor_refuses_is_refused() {
        let error = compose(
            "anthropic-messages/claude-subscription",
            "http://api.anthropic.com",
            "claude-opus-5",
            &settings(),
        )
        .unwrap_err();
        assert_eq!(error.message, "the Messages route endpoint requires HTTPS");
    }
}
