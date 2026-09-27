//! What `configure` makes of `provider-settings`: the route and the profile the native host
//! composes an `OpenAiCodexProvider` from, as a pure function so a host test can hold it
//! against the native construction, and where the broker sends the lowered request.
//!
//! `adapter-settings` is the route file's `[adapter_settings]` table as JSON plus three
//! reserved keys the host adds (the settings contract of S4.6): `model_profile`
//! (`{stem, toml}`, the selected profile file), `route_headers` and `model_binding`. The
//! Responses route has no static headers and no output ceiling, so, as in the native host,
//! the last two are dropped unread.

use p1_contracts::{ProviderError, ProviderErrorKind};
use p1_model_profile::ModelProfile;
use p1_provider_openai::{
    ResponsesAdapterSettings, ResponsesRoute, request_path, validate_composition,
};
use serde_json::{Map, Value};

/// One configured instance: what the native constructor holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Composition {
    pub route: ResponsesRoute,
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
    let settings: ResponsesAdapterSettings = serde_json::from_value(Value::Object(table))
        .map_err(|error| invalid(format!("invalid `[adapter_settings]`: {error}")))?;
    let route = ResponsesRoute {
        origin_route: origin_route.to_owned(),
        endpoint: endpoint.to_owned(),
        account: settings.account,
        transport: settings.transport,
    };
    validate_composition(&route, wire_model, &profile)?;
    Ok(Composition {
        route,
        wire_model: wire_model.to_owned(),
        profile,
    })
}

/// Where an HTTP request of a route with `endpoint` goes, as `(base, path)`: the broker
/// sends to `base` + `path`, the URL the native provider posts to (`resolve_base_url`).
///
/// The frozen `http-request.path` starts with `/`, but the portable lowering's path is
/// empty for an endpoint that already names `/codex/responses`. Then the endpoint's last
/// segment is the path and the base loses it; the component lowers `path` and the host
/// gives the route authority `base` (S4.7), both through this one rule.
pub fn http_target(endpoint: &str) -> Result<(String, String), ProviderError> {
    let path = request_path(endpoint)?;
    let base = endpoint.trim().trim_end_matches('/');
    if !path.is_empty() {
        return Ok((base.to_owned(), path.to_owned()));
    }
    let host = base.find("://").map_or(0, |scheme| scheme + 3);
    Ok(match base[host..].rfind('/') {
        Some(slash) => {
            let (base, path) = base.split_at(host + slash);
            (base.to_owned(), path.to_owned())
        }
        None => (base.to_owned(), "/".to_owned()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use p1_contracts::{CacheKeySupport, Effort, Origin, RouteDescription};
    use p1_model_profile::ThinkingPolicy;
    use p1_provider_openai::{ResponsesAccount, ResponsesTransport, resolve_base_url};

    /// `profiles/gpt-6-sol.toml` as shipped.
    const PROFILE: &str = r#"
id       = "gpt-6-sol"
revision = 1
model_id = "gpt-6-sol"
family   = "gpt"
thinking = "effort-level"

efforts = ["low", "medium", "high", "extra_high", "max"]
"#;

    /// What the host sends for `routes/openai-codex-subscription.toml` bound to that
    /// profile: the `[adapter_settings]` table plus the reserved keys.
    fn settings() -> String {
        serde_json::json!({
            "account": "codex-subscription",
            "transport": "websocket",
            "model_profile": {"stem": "gpt-6-sol", "toml": PROFILE},
            "route_headers": {},
            "model_binding": {},
        })
        .to_string()
    }

    #[test]
    fn the_shipped_route_composes_as_the_native_host_builds_it() {
        let composition = compose(
            "openai-responses/codex-subscription",
            "https://chatgpt.com/backend-api",
            "gpt-6-sol",
            &settings(),
        )
        .expect("the shipped route composes");
        // `responses_route_from` in p1-host's catalog, over the same route file.
        assert_eq!(
            composition.route,
            ResponsesRoute {
                origin_route: "openai-responses/codex-subscription".into(),
                endpoint: "https://chatgpt.com/backend-api".into(),
                account: ResponsesAccount::CodexSubscription,
                transport: ResponsesTransport::Websocket,
            }
        );
        assert_eq!(
            composition.profile,
            ModelProfile {
                id: "gpt-6-sol".into(),
                revision: 1,
                model_id: "gpt-6-sol".into(),
                family: "gpt".into(),
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
        assert_eq!(
            composition.route.describe(&composition.wire_model),
            RouteDescription {
                origin: Origin {
                    route: "openai-responses/codex-subscription".into(),
                    model: "gpt-6-sol".into(),
                },
                supports_freeform_tools: true,
                mandatory_prompt_prefix: None,
                reports_cost: false,
                cache_key: CacheKeySupport::Optional,
            }
        );
    }

    #[test]
    fn an_unknown_adapter_setting_is_refused_like_the_native_host() {
        let mut settings: Value = serde_json::from_str(&settings()).unwrap();
        settings["surprise"] = Value::Bool(true);
        let error = compose(
            "openai-responses/codex-subscription",
            "https://chatgpt.com/backend-api",
            "gpt-6-sol",
            &settings.to_string(),
        )
        .unwrap_err();
        assert_eq!(error.kind, ProviderErrorKind::InvalidRequest);
        assert!(error.message.starts_with("invalid `[adapter_settings]`"));
    }

    #[test]
    fn the_http_target_is_the_native_url_with_a_path_that_starts_with_a_slash() {
        for endpoint in [
            "https://chatgpt.com/backend-api",
            "https://chatgpt.com/backend-api/",
            "https://chatgpt.com/backend-api/codex",
            "https://chatgpt.com/backend-api/codex/responses",
            "https://chatgpt.com/backend-api/codex/responses/",
        ] {
            let (base, path) = http_target(endpoint).unwrap();
            assert!(path.starts_with('/'), "{endpoint}: {path}");
            assert_eq!(
                format!("{base}{path}"),
                resolve_base_url(endpoint).unwrap(),
                "{endpoint}"
            );
        }
        assert_eq!(
            http_target("https://chatgpt.com/backend-api/codex/responses").unwrap(),
            (
                "https://chatgpt.com/backend-api/codex".to_owned(),
                "/responses".to_owned()
            )
        );
    }
}
