//! The provider adapter: `describe`, `validate`, `stream`.
//!
//! The adapter holds one model and one credential source and translates a
//! [`ProviderRequest`] into a Messages request. All retry, refresh, cancellation
//! and terminal-event policy lives in [`p1_provider_http::drive`]; this module
//! only decides whether a request is buildable at all.

use std::sync::Arc;

use p1_contracts::tool::DeclarationKind;
use p1_contracts::{
    BoxFuture, CacheKeySupport, CancellationToken, Origin, Provider, ProviderError,
    ProviderErrorKind, ProviderRequest, ProviderStream, RouteDescription,
};
use p1_provider_http::{
    Credential, CredentialSource, DriveRequest, HttpRequest, ResponseParser, RetryPolicy,
    Transport, drive,
};

use crate::ROUTE;
use crate::parser::AnthropicParser;
use crate::request::{
    DEFAULT_BASE_URL, IDENTITY, build_headers, build_request, explicit_cap_conflict,
};

/// `native` keys in this namespace are route-specific. None are known in this
/// slice, so any key here is rejected.
const NATIVE_PREFIX: &str = "anthropic-messages.";

/// Namespaces the OTHER compiled adapters own inside `ModelOptions::native`. An
/// explicit option from one of them was silently dropped on a route switch
/// before; it is now an error naming the option, this route and this adapter
/// (ADR-0039). Keys in no adapter's namespace keep their meaning: ignored.
const FOREIGN_NATIVE_PREFIXES: &[&str] = &["openai-responses.", "openai-chat."];

/// The Claude subscription route bound to one model and credential source.
pub struct AnthropicProvider {
    model: String,
    transport: Arc<dyn Transport>,
    credentials: Arc<dyn CredentialSource>,
    retry: RetryPolicy,
    base_url: String,
}

impl AnthropicProvider {
    pub fn new(
        model: &str,
        transport: Arc<dyn Transport>,
        credentials: Arc<dyn CredentialSource>,
    ) -> Self {
        Self {
            model: model.to_string(),
            transport,
            credentials,
            retry: RetryPolicy::default(),
            base_url: DEFAULT_BASE_URL.to_string(),
        }
    }

    pub fn with_retry(mut self, retry: RetryPolicy) -> Self {
        self.retry = retry;
        self
    }

    pub fn with_base_url(mut self, url: &str) -> Self {
        self.base_url = url.trim_end_matches('/').to_string();
        self
    }

    fn origin(&self) -> Origin {
        Origin {
            route: ROUTE.to_string(),
            model: self.model.clone(),
        }
    }
}

impl Provider for AnthropicProvider {
    fn describe(&self) -> RouteDescription {
        RouteDescription {
            origin: self.origin(),
            // The Messages route declares JSON-schema function tools only.
            supports_freeform_tools: false,
            mandatory_prompt_prefix: Some(IDENTITY.to_string()),
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
                        "tool `{}` is declared freeform, which route {ROUTE} cannot carry",
                        tool.name
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
                        "option \"{key}\" is not consumed by route \"{ROUTE}\" \
                         (adapter anthropic-messages): it belongs to another adapter's namespace"
                    ),
                ));
            }
        }
        if request.options.cache_key.is_some() {
            return Err(ProviderError::new(
                ProviderErrorKind::InvalidRequest,
                format!("route {ROUTE} takes no cache key"),
            ));
        }
        if let Some(error) = explicit_cap_conflict(&self.model, &request.options) {
            return Err(error);
        }
        if request.options.max_output_tokens == Some(0) {
            return Err(ProviderError::new(
                ProviderErrorKind::InvalidRequest,
                "max_output_tokens must be greater than zero",
            ));
        }
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
            let body = build_request(&self.model, &request)?;
            let encoded = serde_json::to_vec(&body).map_err(|_| {
                ProviderError::new(
                    ProviderErrorKind::InvalidRequest,
                    "the request body could not be encoded",
                )
            })?;

            let url = format!("{}/v1/messages", self.base_url);
            let model = self.model.clone();
            let build = Box::new(move |credential: &Credential| HttpRequest {
                url: url.clone(),
                headers: build_headers(credential, &body),
                body: encoded.clone(),
            });
            let new_parser =
                Box::new(move || Box::new(AnthropicParser::new(&model)) as Box<dyn ResponseParser>);

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

    fn provider(model: &str) -> AnthropicProvider {
        AnthropicProvider::new(
            model,
            Arc::new(p1_provider_http::testing::ScriptedTransport::new(Vec::new())),
            Arc::new(NoCredentials),
        )
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
                build_request("claude-opus-4-6", &request(options))
                    .unwrap_err()
                    .kind,
                ProviderErrorKind::InvalidRequest
            );
        }
        // One above the budget is fine, and so is every cap on the adaptive lane.
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
                format!("route \"{ROUTE}\""),
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
}
