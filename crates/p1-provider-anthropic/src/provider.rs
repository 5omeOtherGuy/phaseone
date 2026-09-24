//! The provider adapter: `describe`, `validate`, `stream`.
//!
//! The adapter holds one composed instance — a [`MessagesRoute`], a wire model and
//! a [`ModelProfile`] — and one credential source, and translates a
//! [`ProviderRequest`] into a Messages request. All retry, refresh, cancellation
//! and terminal-event policy lives in [`p1_provider_http::drive`]; this module
//! only decides whether a request is buildable at all, through the same pure
//! lowering function the request builder uses (ADR-0039, spec §7.3).

use std::sync::Arc;

use p1_contracts::tool::DeclarationKind;
use p1_contracts::{
    BoxFuture, CacheKeySupport, CancellationToken, ModelOptions, Origin, Provider, ProviderError,
    ProviderErrorKind, ProviderRequest, ProviderStream, RouteDescription,
};
use p1_model_profile::ModelProfile;
use p1_provider_http::{
    Credential, CredentialSource, DriveRequest, HttpRequest, ResponseParser, RetryPolicy,
    Transport, drive,
};

use crate::MessagesRoute;
use crate::parser::AnthropicParser;
use crate::request::{build_headers, build_messages, build_request, lower};

/// `native` keys in this namespace are route-specific. None are known in this
/// slice, so any key here is rejected.
const NATIVE_PREFIX: &str = "anthropic-messages.";

/// Namespaces the OTHER compiled adapters own inside `ModelOptions::native`. An
/// explicit option from one of them was silently dropped on a route switch
/// before; it is now an error naming the option, this route and this adapter
/// (ADR-0039). Keys in no adapter's namespace keep their meaning: ignored.
const FOREIGN_NATIVE_PREFIXES: &[&str] = &["openai-responses.", "openai-chat."];

/// One route file composed with one profile and one credential source.
pub struct AnthropicProvider {
    route: MessagesRoute,
    wire_model: String,
    profile: Arc<ModelProfile>,
    transport: Arc<dyn Transport>,
    credentials: Arc<dyn CredentialSource>,
    retry: RetryPolicy,
}

impl AnthropicProvider {
    /// Compose the adapter from its three inputs (ADR-0039). A profile whose
    /// thinking policy the Messages wire cannot express fails HERE, before any
    /// request exists.
    pub fn new(
        route: MessagesRoute,
        wire_model: &str,
        profile: Arc<ModelProfile>,
        transport: Arc<dyn Transport>,
        credentials: Arc<dyn CredentialSource>,
    ) -> Result<Self, ProviderError> {
        validate_composition(&route, wire_model, &profile)?;
        Ok(Self {
            route,
            wire_model: wire_model.to_string(),
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

    /// Replace the route's endpoint (embedding and tests). The route data of a
    /// composed provider comes from its file.
    pub fn with_base_url(mut self, url: &str) -> Self {
        self.route.endpoint = url.trim_end_matches('/').to_string();
        self
    }

    fn origin(&self) -> Origin {
        self.route.origin(&self.wire_model)
    }
}

/// The one composition check, shared by the constructor and the pure request
/// builder: the route data is usable, the profile is valid, and the profile's
/// policy has an encoding here. Nothing is decided by a second, parallel table.
pub(crate) fn validate_composition(
    route: &MessagesRoute,
    wire_model: &str,
    profile: &ModelProfile,
) -> Result<(), ProviderError> {
    route.validate()?;
    profile.validate()?;
    if wire_model.is_empty() {
        return Err(ProviderError::new(
            ProviderErrorKind::InvalidRequest,
            "wire model must be nonempty",
        ));
    }
    // The pure lowering decides: an `enabled`/`preserved` profile has no Messages
    // encoding, so it is refused here, at construction.
    lower(profile, &ModelOptions::default())?;
    Ok(())
}

impl std::fmt::Debug for AnthropicProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AnthropicProvider")
            .field("route", &self.route)
            .field("wire_model", &self.wire_model)
            .finish_non_exhaustive()
    }
}

impl Provider for AnthropicProvider {
    fn describe(&self) -> RouteDescription {
        RouteDescription {
            origin: self.origin(),
            // The Messages route declares JSON-schema function tools only.
            supports_freeform_tools: false,
            mandatory_prompt_prefix: Some(crate::request::IDENTITY.to_string()),
            // A subscription bills by plan, not per request: cost is unknown.
            reports_cost: false,
            // This route caches with `cache_control` markers; `options.cache_key`
            // never reaches the wire, so an explicit one is rejected below.
            cache_key: CacheKeySupport::Unsupported,
        }
    }

    fn validate(&self, request: &ProviderRequest) -> Result<(), ProviderError> {
        for tool in &request.tools {
            if matches!(tool.kind, DeclarationKind::Freeform { .. }) {
                return Err(ProviderError::new(
                    ProviderErrorKind::InvalidRequest,
                    format!(
                        "tool `{}` is declared freeform, which route {} cannot carry",
                        tool.name, self.route.origin_route
                    ),
                ));
            }
        }
        for key in request.options.native.keys() {
            if key.starts_with(NATIVE_PREFIX) {
                return Err(ProviderError::new(
                    ProviderErrorKind::InvalidRequest,
                    format!("unsupported route-native option `{key}`"),
                ));
            }
            if FOREIGN_NATIVE_PREFIXES
                .iter()
                .any(|prefix| key.starts_with(prefix))
            {
                return Err(ProviderError::new(
                    ProviderErrorKind::InvalidRequest,
                    format!(
                        "option \"{key}\" is not consumed by route \"{}\" \
                         (adapter anthropic-messages): it belongs to another adapter's namespace",
                        self.route.origin_route
                    ),
                ));
            }
        }
        if request.options.cache_key.is_some() {
            return Err(ProviderError::new(
                ProviderErrorKind::InvalidRequest,
                format!("route {} takes no cache key", self.route.origin_route),
            ));
        }
        if request.options.max_output_tokens == Some(0) {
            return Err(ProviderError::new(
                ProviderErrorKind::InvalidRequest,
                "max_output_tokens must be greater than zero",
            ));
        }
        // The harness's context window, when the request carries one: a value this
        // reader cannot read is refused HERE, before anything is sent, so a
        // malformed window never silently loses the 1M beta (ADR-0065).
        crate::request::context_window_tokens(&request.options)?;
        // The model policy: the same lowering the request builder runs, so
        // `validate` can never accept a request the builder would reject.
        lower(&self.profile, &request.options)?;
        // The history: everything else the Messages wire cannot carry is lowered
        // (a freeform call travels as `{"input": …}`), so the message mapping is
        // the check. A transcript whose first message would be an assistant turn
        // is refused by name before anything is sent (ADR-0049).
        build_messages(&self.route.origin_route, &self.wire_model, &request.history)?;
        Ok(())
    }

    fn stream<'a>(
        &'a self,
        request: ProviderRequest,
        cancel: CancellationToken,
    ) -> BoxFuture<'a, Result<ProviderStream, ProviderError>> {
        Box::pin(async move {
            // The ONLY setup failures: a request this route cannot carry, or one
            // that cannot be built. All network-side failures are reported by the
            // stream's terminal event.
            self.validate(&request)?;
            let body = build_request(&self.route, &self.wire_model, &self.profile, &request)?;
            let encoded = serde_json::to_vec(&body).map_err(|_| {
                ProviderError::new(
                    ProviderErrorKind::InvalidRequest,
                    "the request body could not be encoded",
                )
            })?;

            let url = format!("{}/v1/messages", self.route.endpoint);
            let account = self.route.account;
            let origin_route = self.route.origin_route.clone();
            let model = self.wire_model.clone();
            // The window is read ONCE from this request's options and rides into the
            // header builder: the 1M beta is a wire-visible consequence of it, and it
            // stays the same across every retry of this request (ADR-0065).
            let window_tokens = crate::request::context_window_tokens(&request.options)?;
            let build = Box::new(move |credential: &Credential| HttpRequest {
                url: url.clone(),
                headers: build_headers(account, credential, &body, window_tokens),
                body: encoded.clone(),
            });
            let new_parser = Box::new(move || {
                Box::new(AnthropicParser::new(&origin_route, &model)) as Box<dyn ResponseParser>
            });

            Ok(drive(DriveRequest {
                transport: self.transport.clone(),
                credentials: self.credentials.clone(),
                build,
                new_parser,
                retry: self.retry,
                cancel,
            }))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use p1_contracts::{Effort, Item, ModelOptions};
    use p1_model_profile::ThinkingPolicy;
    use std::collections::BTreeMap;

    /// A synthetic Messages route: today's shipped origin route, so the origin in
    /// these unit expectations is the recorded one (spec §7.2).
    fn route() -> MessagesRoute {
        MessagesRoute {
            origin_route: crate::ROUTE.to_string(),
            endpoint: "https://api.anthropic.com".to_string(),
            account: crate::MessagesAccount::ClaudeCodeSubscription,
        }
    }

    /// The profile the OLD model-name rule selected: `claude-opus-4-6` /
    /// `claude-sonnet-4-6` took the manual budget table, the three adaptive
    /// prefixes took an effort level. This mapping documents what the explicit
    /// `profiles/claude-*.toml` records replaced; the expectations are unchanged.
    fn profile(model: &str) -> Arc<ModelProfile> {
        let effort_level = ["claude-fable-5", "claude-opus-5", "claude-sonnet-5"]
            .iter()
            .any(|prefix| model.starts_with(prefix));
        let efforts = vec![
            Effort::Low,
            Effort::Medium,
            Effort::High,
            Effort::ExtraHigh,
            Effort::Max,
        ];
        Arc::new(ModelProfile {
            id: model.to_string(),
            revision: 1,
            model_id: model.to_string(),
            family: "claude".to_string(),
            thinking: if effort_level {
                ThinkingPolicy::EffortLevel
            } else {
                ThinkingPolicy::Budget
            },
            efforts: efforts.clone(),
            default_effort: None,
            thinking_budgets: if effort_level {
                BTreeMap::new()
            } else {
                [
                    (Effort::Low, 4_096),
                    (Effort::Medium, 10_240),
                    (Effort::High, 20_480),
                    (Effort::ExtraHigh, 32_768),
                    (Effort::Max, 32_768),
                ]
                .into_iter()
                .collect()
            },
            context_tokens: None,
            max_output_tokens: None,
        })
    }

    fn provider(model: &str) -> AnthropicProvider {
        AnthropicProvider::new(
            route(),
            model,
            profile(model),
            Arc::new(p1_provider_http::testing::ScriptedTransport::new(Vec::new())),
            Arc::new(NoCredentials),
        )
        .expect("the profile is expressible on the Messages wire")
    }

    struct NoCredentials;

    impl CredentialSource for NoCredentials {
        fn access<'a>(&'a self) -> BoxFuture<'a, Result<Credential, ProviderError>> {
            Box::pin(async {
                Err(ProviderError::new(
                    ProviderErrorKind::Authentication,
                    "no credential",
                ))
            })
        }

        fn refresh<'a>(
            &'a self,
            _rejected: &'a Credential,
        ) -> BoxFuture<'a, Result<Credential, ProviderError>> {
            Box::pin(async {
                Err(ProviderError::new(
                    ProviderErrorKind::Authentication,
                    "no credential",
                ))
            })
        }
    }

    fn request(options: ModelOptions) -> ProviderRequest {
        ProviderRequest {
            system_prompt: "SYS".to_string(),
            history: vec![Item::User {
                text: "hi".to_string(),
            }],
            tools: Vec::new(),
            options,
        }
    }

    #[test]
    fn describe_reports_no_cache_key_support() {
        assert_eq!(
            provider("claude-opus-4-6").describe().cache_key,
            CacheKeySupport::Unsupported
        );
    }

    #[test]
    fn an_explicit_cache_key_is_rejected_because_the_route_takes_none() {
        let options = ModelOptions {
            cache_key: Some("p1-abc".to_string()),
            ..ModelOptions::default()
        };
        let error = provider("claude-opus-4-6")
            .validate(&request(options))
            .unwrap_err();
        assert_eq!(error.kind, ProviderErrorKind::InvalidRequest);
        assert!(error.message.contains("takes no cache key"), "{}", error);
    }

    #[test]
    fn an_explicit_cap_the_manual_budget_meets_is_rejected_with_the_minimum() {
        // Manual lane: budget 4_096 for `low`. A cap at or below it conflicts.
        for cap in [1_000u32, 4_096] {
            let options = ModelOptions {
                reasoning_effort: Some(Effort::Low),
                max_output_tokens: Some(cap),
                ..ModelOptions::default()
            };
            let error = provider("claude-opus-4-6")
                .validate(&request(options.clone()))
                .unwrap_err();
            assert_eq!(error.kind, ProviderErrorKind::InvalidRequest, "{cap}");
            for part in [
                format!("max_output_tokens {cap}"),
                "thinking budget 4096".to_string(),
                // The smallest cap that leaves room for the budget.
                "is 4097".to_string(),
            ] {
                assert!(error.message.contains(&part), "{}: {}", error, part);
            }
            // The pure builder rejects the same request.
            assert_eq!(
                build_request(
                    &route(),
                    "claude-opus-4-6",
                    &profile("claude-opus-4-6"),
                    &request(options)
                )
                .unwrap_err()
                .kind,
                ProviderErrorKind::InvalidRequest
            );
        }
        // One above the budget is fine, and so is every cap on the effort-level lane.
        let options = ModelOptions {
            reasoning_effort: Some(Effort::Low),
            max_output_tokens: Some(4_097),
            ..ModelOptions::default()
        };
        assert!(
            provider("claude-opus-4-6")
                .validate(&request(options))
                .is_ok()
        );
        let options = ModelOptions {
            reasoning_effort: Some(Effort::Max),
            max_output_tokens: Some(1_024),
            ..ModelOptions::default()
        };
        assert!(
            provider("claude-sonnet-5")
                .validate(&request(options))
                .is_ok()
        );
    }

    #[test]
    fn a_native_option_in_another_adapters_namespace_is_an_error() {
        for key in ["openai-responses.verbosity", "openai-chat.future_flag"] {
            let mut options = ModelOptions::default();
            options
                .native
                .insert(key.to_string(), serde_json::json!(true));
            let error = provider("claude-opus-4-6")
                .validate(&request(options))
                .unwrap_err();
            assert_eq!(error.kind, ProviderErrorKind::InvalidRequest, "{key}");
            for part in [
                format!("option \"{key}\""),
                format!("route \"{}\"", crate::ROUTE),
                "(adapter anthropic-messages)".to_string(),
            ] {
                assert!(error.message.contains(&part), "{}: {}", error, part);
            }
        }
    }

    #[test]
    fn a_native_option_in_no_adapters_namespace_keeps_its_meaning() {
        let mut options = ModelOptions::default();
        options
            .native
            .insert("unrelated.option".to_string(), serde_json::json!(1));
        options
            .native
            .insert("bare".to_string(), serde_json::json!(2));
        assert!(
            provider("claude-opus-4-6")
                .validate(&request(options))
                .is_ok()
        );
    }

    #[test]
    fn an_effort_the_profile_does_not_list_is_rejected_by_validate_and_the_builder() {
        let profile = Arc::new(ModelProfile {
            efforts: vec![Effort::High],
            thinking_budgets: [(Effort::High, 20_480)].into_iter().collect(),
            ..(*profile("claude-opus-4-6")).clone()
        });
        let provider = AnthropicProvider::new(
            route(),
            "claude-opus-4-6",
            profile.clone(),
            Arc::new(p1_provider_http::testing::ScriptedTransport::new(Vec::new())),
            Arc::new(NoCredentials),
        )
        .expect("the policy is expressible");
        let options = ModelOptions {
            reasoning_effort: Some(Effort::Low),
            ..ModelOptions::default()
        };
        let error = provider.validate(&request(options.clone())).unwrap_err();
        assert_eq!(error.kind, ProviderErrorKind::InvalidRequest);
        assert_eq!(
            build_request(&route(), "claude-opus-4-6", &profile, &request(options))
                .unwrap_err()
                .kind,
            ProviderErrorKind::InvalidRequest
        );
    }

    #[test]
    fn the_messages_adapter_refuses_the_enabled_and_preserved_policies() {
        for (policy, variant) in [
            (ThinkingPolicy::Enabled, "enabled"),
            (ThinkingPolicy::Preserved, "preserved"),
        ] {
            let profile = Arc::new(ModelProfile {
                thinking: policy,
                thinking_budgets: BTreeMap::new(),
                ..(*profile("claude-opus-4-6")).clone()
            });
            assert!(profile.validate().is_ok(), "{variant}");
            let error = AnthropicProvider::new(
                route(),
                "claude-opus-4-6",
                profile.clone(),
                Arc::new(p1_provider_http::testing::ScriptedTransport::new(Vec::new())),
                Arc::new(NoCredentials),
            )
            .unwrap_err();
            assert_eq!(error.kind, ProviderErrorKind::InvalidRequest, "{variant}");
            assert!(
                error.message.contains(&format!("thinking = \"{variant}\"")),
                "{variant}: {}",
                error.message
            );
            assert!(error.message.contains("Messages"), "{variant}: {error}");
            // The pure builder refuses it too: one lowering, not two rules.
            assert!(
                build_request(
                    &route(),
                    "claude-opus-4-6",
                    &profile,
                    &request(ModelOptions::default())
                )
                .is_err(),
                "{variant}"
            );
        }
    }
}
