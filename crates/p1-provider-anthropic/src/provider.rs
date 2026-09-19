//! The provider adapter: `describe`, `validate`, `stream`.
//!
//! The adapter holds one model and one credential source and translates a
//! [`ProviderRequest`] into a Messages request. All retry, refresh, cancellation
//! and terminal-event policy lives in [`p1_provider_http::drive`]; this module
//! only decides whether a request is buildable at all.

use std::sync::Arc;

use p1_contracts::tool::DeclarationKind;
use p1_contracts::{
    BoxFuture, CancellationToken, Origin, Provider, ProviderError, ProviderErrorKind,
    ProviderRequest, ProviderStream, RouteDescription,
};
use p1_provider_http::{
    Credential, CredentialSource, DriveRequest, HttpRequest, ResponseParser, RetryPolicy,
    Transport, drive,
};

use crate::ROUTE;
use crate::parser::AnthropicParser;
use crate::request::{DEFAULT_BASE_URL, IDENTITY, build_headers, build_request};

/// `native` keys in this namespace are route-specific. None are known in this
/// slice, so any key here is rejected.
const NATIVE_PREFIX: &str = "anthropic-messages.";

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
