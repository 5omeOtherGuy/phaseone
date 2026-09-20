//! A composed Chat Completions provider: wire dialect, route data and model policy.
//! Credentials are supplied through CredentialSource; this crate performs no login lookup.
mod parser;
mod request;

use p1_contracts::{
    BoxFuture, CacheKeySupport, CancellationToken, Effort, Origin, Provider, ProviderError,
    ProviderRequest, ProviderStream, RouteDescription,
};
use p1_model_profile::{ModelProfile, ThinkingPolicy};
use p1_provider_http::{
    CredentialSource, DriveRequest, HttpRequest, RetryPolicy, Transport, drive,
};
pub use request::build_request;
use std::sync::Arc;

/// Implemented encodings, not service names. Unknown extensions require an implementation.
/// The names are the kebab-case spellings a route file's `[adapter_settings]` uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ChatDialect {
    /// Enabled thinking, replayable reasoning_content plus the equivalent reasoning alias.
    ThinkingWithReasoningAlias,
    /// Enabled/preserved thinking, reasoning_content replay, and streaming function inputs.
    RetainedThinking,
}

/// The `[adapter_settings]` table of a route whose `adapter` is `openai-chat`: fields
/// this adapter owns, parsed by this adapter (`docs/design/routes-and-profiles.md`
/// §1.2). A key this struct does not name is rejected rather than ignored.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChatAdapterSettings {
    pub dialect: ChatDialect,
    /// The header a cache/session key travels in. `None`: the route carries none, so
    /// an explicit cache key is an error for this route.
    #[serde(default)]
    pub session_header: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ChatLimits {
    /// A route ceiling may restrict a model profile, never enlarge it.
    pub max_output_tokens: Option<u32>,
}

#[derive(Clone, PartialEq, Eq)]
pub struct ChatRoute {
    pub origin_route: String,
    pub endpoint: String,
    /// Non-secret headers only. Authentication comes exclusively from CredentialSource.
    pub headers: Vec<(String, String)>,
    pub session_header: Option<String>,
    pub dialect: ChatDialect,
    pub limits: ChatLimits,
}
impl std::fmt::Debug for ChatRoute {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ChatRoute")
            .field("origin_route", &self.origin_route)
            .field("dialect", &self.dialect)
            .field("limits", &self.limits)
            .finish_non_exhaustive()
    }
}
impl ChatRoute {
    pub fn origin(&self, wire_model: &str) -> Origin {
        Origin {
            route: self.origin_route.clone(),
            model: wire_model.into(),
        }
    }
    fn validate(&self) -> Result<(), ProviderError> {
        let endpoint = self
            .endpoint
            .strip_prefix("https://")
            .ok_or_else(|| request::invalid("chat endpoint requires HTTPS"))?;
        if endpoint.split('/').next().is_none_or(str::is_empty)
            || endpoint.contains(['@', '?', '#'])
            || !endpoint.bytes().all(|b| (33..=126).contains(&b))
            || self.origin_route.is_empty()
            || self.limits.max_output_tokens == Some(0)
        {
            return Err(request::invalid(
                "invalid chat route identity, endpoint or limit",
            ));
        }
        let mut names = std::collections::BTreeSet::new();
        for (name, value) in &self.headers {
            if !header_name(name)
                || !names.insert(name.to_ascii_lowercase())
                || value.is_empty()
                || !value.bytes().all(|b| (32..=126).contains(&b))
            {
                return Err(request::invalid("invalid or duplicate static chat header"));
            }
        }
        if let Some(name) = &self.session_header
            && (!header_name(name) || names.contains(&name.to_ascii_lowercase()))
        {
            return Err(request::invalid("invalid or conflicting session header"));
        }
        Ok(())
    }
}
fn header_name(name: &str) -> bool {
    !name.is_empty()
        && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
        && !matches!(
            name.to_ascii_lowercase().as_str(),
            "authorization"
                | "proxy-authorization"
                | "cookie"
                | "x-api-key"
                | "api-key"
                | "content-type"
                | "accept"
                | "host"
                | "content-length"
                | "transfer-encoding"
        )
}

pub struct ChatProvider {
    route: ChatRoute,
    wire_model: String,
    profile: Arc<ModelProfile>,
    transport: Arc<dyn Transport>,
    credentials: Arc<dyn CredentialSource>,
    retry: RetryPolicy,
}
impl ChatProvider {
    pub fn new(
        route: ChatRoute,
        wire_model: &str,
        profile: Arc<ModelProfile>,
        transport: Arc<dyn Transport>,
        credentials: Arc<dyn CredentialSource>,
    ) -> Result<Self, ProviderError> {
        validate_composition(&route, wire_model, &profile)?;
        Ok(Self {
            route,
            wire_model: wire_model.into(),
            profile,
            transport,
            credentials,
            retry: RetryPolicy::default(),
        })
    }
    pub fn with_retry(mut self, retry: RetryPolicy) -> Self {
        self.retry = retry;
        self
    }
}
pub(crate) fn validate_composition(
    route: &ChatRoute,
    wire_model: &str,
    profile: &ModelProfile,
) -> Result<(), ProviderError> {
    route.validate()?;
    profile.validate()?;
    if wire_model.is_empty() {
        return Err(request::invalid("wire model must be nonempty"));
    }
    match profile.thinking {
        ThinkingPolicy::Enabled => {}
        ThinkingPolicy::Preserved if route.dialect == ChatDialect::RetainedThinking => {}
        ThinkingPolicy::Preserved => {
            return Err(request::invalid(
                "chat dialect cannot express the profile's preserved thinking requirement",
            ));
        }
        ThinkingPolicy::EffortLevel => {
            return Err(request::invalid(&format!(
                "chat dialect `{}` cannot express the profile's `thinking = \"effort-level\"` policy",
                dialect_name(route.dialect)
            )));
        }
        ThinkingPolicy::Budget => {
            return Err(request::invalid(&format!(
                "chat dialect `{}` cannot express the profile's `thinking = \"budget\"` policy",
                dialect_name(route.dialect)
            )));
        }
    }
    if profile.default_effort.is_none() {
        return Err(request::invalid(
            "chat profile requires a default reasoning effort",
        ));
    }
    if profile
        .efforts
        .iter()
        .any(|effort| !matches!(effort, Effort::Low | Effort::High | Effort::Max))
    {
        return Err(request::invalid(
            "chat dialect cannot encode this profile's reasoning efforts",
        ));
    }
    Ok(())
}

/// The kebab-case file spelling of a `ChatDialect`, for error messages.
fn dialect_name(dialect: ChatDialect) -> &'static str {
    match dialect {
        ChatDialect::ThinkingWithReasoningAlias => "thinking-with-reasoning-alias",
        ChatDialect::RetainedThinking => "retained-thinking",
    }
}
impl std::fmt::Debug for ChatProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ChatProvider")
            .field("route", &self.route)
            .field("wire_model", &self.wire_model)
            .finish_non_exhaustive()
    }
}
impl Provider for ChatProvider {
    fn describe(&self) -> RouteDescription {
        RouteDescription {
            origin: self.route.origin(&self.wire_model),
            supports_freeform_tools: false,
            mandatory_prompt_prefix: None,
            reports_cost: false,
            // `options.cache_key` is consumed exactly as the route's session
            // header; a route without one takes no key at all (validate rejects
            // an explicit key on such a route).
            cache_key: if self.route.session_header.is_some() {
                CacheKeySupport::Optional
            } else {
                CacheKeySupport::Unsupported
            },
        }
    }
    fn validate(&self, request: &ProviderRequest) -> Result<(), ProviderError> {
        request::validate(&self.route, &self.profile, request)
    }
    fn stream<'a>(
        &'a self,
        request: ProviderRequest,
        cancel: CancellationToken,
    ) -> BoxFuture<'a, Result<ProviderStream, ProviderError>> {
        Box::pin(async move {
            let body = serde_json::to_vec(&build_request(
                &self.route,
                &self.wire_model,
                &self.profile,
                &request,
            )?)
            .map_err(|_| request::invalid("cannot encode request"))?;
            let route = self.route.clone();
            let origin = route.origin(&self.wire_model);
            let dialect = route.dialect;
            let cache_key = request.options.cache_key;
            Ok(drive(DriveRequest {
                transport: self.transport.clone(),
                credentials: self.credentials.clone(),
                build: Box::new(move |credential| {
                    let mut headers = route.headers.clone();
                    headers.extend([
                        ("content-type".into(), "application/json".into()),
                        ("accept".into(), "text/event-stream".into()),
                        (
                            "authorization".into(),
                            format!("Bearer {}", credential.bearer),
                        ),
                    ]);
                    if let (Some(name), Some(value)) = (&route.session_header, &cache_key) {
                        headers.push((name.clone(), value.clone()));
                    }
                    HttpRequest {
                        url: route.endpoint.clone(),
                        headers,
                        body: body.clone(),
                    }
                }),
                new_parser: Box::new(move || {
                    Box::new(parser::ChatParser::new(origin.clone(), dialect))
                }),
                retry: self.retry,
                cancel,
            }))
        })
    }
}

#[cfg(test)]
mod test_config {
    use super::*;
    pub fn route(retained: bool) -> ChatRoute {
        ChatRoute {
            origin_route: if retained {
                "openai-chat/glm-subscription"
            } else {
                "openai-chat/opencode-go-subscription"
            }
            .into(),
            endpoint: "https://example.test/chat/completions".into(),
            headers: vec![],
            session_header: (!retained).then(|| "x-session".into()),
            dialect: if retained {
                ChatDialect::RetainedThinking
            } else {
                ChatDialect::ThinkingWithReasoningAlias
            },
            limits: ChatLimits::default(),
        }
    }
    pub fn profile(retained: bool) -> ModelProfile {
        ModelProfile {
            id: "canonical-model".into(),
            revision: 1,
            model_id: "canonical-model".into(),
            family: "test".into(),
            thinking: if retained {
                ThinkingPolicy::Preserved
            } else {
                ThinkingPolicy::Enabled
            },
            efforts: if retained {
                vec![Effort::Low, Effort::High, Effort::Max]
            } else {
                vec![Effort::High, Effort::Max]
            },
            default_effort: Some(Effort::High),
            thinking_budgets: std::collections::BTreeMap::new(),
            context_tokens: None,
            max_output_tokens: retained.then_some(131_072),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn all_efforts() -> Vec<Effort> {
        vec![
            Effort::Low,
            Effort::Medium,
            Effort::High,
            Effort::ExtraHigh,
            Effort::Max,
        ]
    }

    fn profile(
        thinking: ThinkingPolicy,
        efforts: Vec<Effort>,
        thinking_budgets: BTreeMap<Effort, u32>,
    ) -> ModelProfile {
        ModelProfile {
            id: "claude-example".into(),
            revision: 1,
            model_id: "claude-example".into(),
            family: "claude".into(),
            thinking,
            efforts,
            default_effort: None,
            thinking_budgets,
            context_tokens: None,
            max_output_tokens: None,
        }
    }

    #[test]
    fn chat_refuses_the_effort_level_and_budget_policies_by_variant_and_dialect() {
        let efforts = all_efforts();
        let budgets: BTreeMap<Effort, u32> =
            efforts.iter().map(|effort| (*effort, 32_768)).collect();
        let cases = [
            (ThinkingPolicy::EffortLevel, "effort-level", BTreeMap::new()),
            (ThinkingPolicy::Budget, "budget", budgets),
        ];
        for retained in [false, true] {
            let route = test_config::route(retained);
            let dialect = dialect_name(route.dialect);
            for (thinking, variant, table) in &cases {
                let profile = profile(*thinking, efforts.clone(), table.clone());
                assert!(profile.validate().is_ok(), "{variant}");
                let error = validate_composition(&route, "claude-example", &profile).unwrap_err();
                assert_eq!(
                    error.kind,
                    p1_contracts::ProviderErrorKind::InvalidRequest,
                    "{variant}"
                );
                assert!(
                    error.message.contains(variant),
                    "{variant}: {}",
                    error.message
                );
                assert!(
                    error.message.contains(dialect),
                    "{variant}: {}",
                    error.message
                );
            }
        }
    }

    #[test]
    fn chat_refuses_a_profile_without_a_default_effort() {
        let route = test_config::route(false);
        let profile = profile(
            ThinkingPolicy::Enabled,
            vec![Effort::Low, Effort::High, Effort::Max],
            BTreeMap::new(),
        );
        let error = validate_composition(&route, "claude-example", &profile).unwrap_err();
        assert!(error.message.contains("default"), "{}", error.message);
    }
}
