//! The [`Provider`] implementation: contract in, codex stream out.

use std::sync::Arc;

use p1_contracts::{
    BoxFuture, CacheKeySupport, CancellationToken, Origin, Provider, ProviderError,
    ProviderErrorKind, ProviderRequest, ProviderStream, RouteDescription,
};
use p1_provider_http::{
    Credential, CredentialSource, DriveRequest, HttpRequest, ResponseParser, RetryPolicy,
    Transport, drive,
};

use crate::parser::CodexResponseParser;
use crate::request::{
    DEFAULT_BASE_URL, ROUTE, build_headers, build_request, clamped_cache_key, resolve_base_url,
    validate as validate_options,
};

/// Namespaces the OTHER compiled adapters own inside `ModelOptions::native`. An
/// explicit option from one of them was silently dropped on a route switch
/// before; it is now an error naming the option, this route and this adapter
/// (ADR-0039). Keys in no adapter's namespace keep their meaning: ignored.
const FOREIGN_NATIVE_PREFIXES: &[&str] = &["anthropic-messages.", "openai-chat."];

/// The adapter half of this route's identity, for error messages.
const ADAPTER: &str = "openai-responses";

/// The ChatGPT/Codex subscription route as a provider.
pub struct OpenAiCodexProvider {
    model: String,
    transport: Arc<dyn Transport>,
    credentials: Arc<dyn CredentialSource>,
    retry: RetryPolicy,
    base_url: String,
}

impl OpenAiCodexProvider {
    /// Build a provider. Construction reads no credential file: the token file
    /// is only opened by the first `access` call, inside the driver. The
    /// credential source is wrapped so a credential without a ChatGPT account id
    /// is reported as an authentication failure, never a malformed request.
    pub fn new(
        model: &str,
        transport: Arc<dyn Transport>,
        credentials: Arc<dyn CredentialSource>,
    ) -> Self {
        Self {
            model: model.to_string(),
            transport,
            credentials: Arc::new(AccountIdGuard { inner: credentials }),
            retry: RetryPolicy::default(),
            base_url: DEFAULT_BASE_URL.to_string(),
        }
    }

    /// Override the transient retry policy (tests and hosts).
    pub fn with_retry(mut self, retry: RetryPolicy) -> Self {
        self.retry = retry;
        self
    }

    /// Override the endpoint base. The `/codex/responses` path is appended when
    /// the base does not already end with it.
    pub fn with_base_url(mut self, url: &str) -> Self {
        self.base_url = url.to_string();
        self
    }
}

impl Provider for OpenAiCodexProvider {
    fn describe(&self) -> RouteDescription {
        RouteDescription {
            origin: Origin {
                route: ROUTE.to_string(),
                model: self.model.clone(),
            },
            supports_freeform_tools: true,
            mandatory_prompt_prefix: None,
            reports_cost: false,
            // The request builder sends `options.cache_key` as the body's
            // `prompt_cache_key` and the session identity headers.
            cache_key: CacheKeySupport::Optional,
        }
    }

    fn validate(&self, request: &ProviderRequest) -> Result<(), ProviderError> {
        for key in request.options.native.keys() {
            if FOREIGN_NATIVE_PREFIXES
                .iter()
                .any(|prefix| key.starts_with(prefix))
            {
                return Err(ProviderError::new(
                    ProviderErrorKind::InvalidRequest,
                    format!(
                        "option \"{key}\" is not consumed by route \"{ROUTE}\" \
                         (adapter {ADAPTER}): it belongs to another adapter's namespace"
                    ),
                ));
            }
        }
        if request.options.cache_key.as_deref() == Some("") {
            return Err(ProviderError::new(
                ProviderErrorKind::InvalidRequest,
                "cache_key must not be empty: set a stable nonempty key or leave it unset",
            ));
        }
        validate_options(&request.options)
    }

    fn stream<'a>(
        &'a self,
        request: ProviderRequest,
        cancel: CancellationToken,
    ) -> BoxFuture<'a, Result<ProviderStream, ProviderError>> {
        Box::pin(async move {
            // These are the only setup failures: unsupported explicit options or
            // a request the route cannot encode. Everything network-shaped is a
            // terminal event inside the returned stream.
            self.validate(&request)?;
            let url = resolve_base_url(&self.base_url)?;
            let body = build_request(&self.model, &request)?;
            let body = serde_json::to_vec(&body).map_err(|_| {
                ProviderError::new(
                    ProviderErrorKind::InvalidRequest,
                    "failed to serialize the request body",
                )
            })?;
            // The same clamped key as the body's `prompt_cache_key`, sent as
            // the session identity headers.
            let cache_key = clamped_cache_key(&request.options);

            let transport = self.transport.clone();
            let credentials = self.credentials.clone();
            let model = self.model.clone();
            Ok(drive(DriveRequest {
                transport,
                credentials,
                build: Box::new(move |credential: &Credential| {
                    // Unreachable by construction: `AccountIdGuard` turns a
                    // credential without an account id into an authentication
                    // failure before the driver can build a request.
                    let headers = build_headers(credential, cache_key.as_deref())
                        .expect("the credential guard guarantees a ChatGPT account id");
                    HttpRequest {
                        url: url.clone(),
                        headers,
                        body: body.clone(),
                    }
                }),
                new_parser: Box::new(move || {
                    Box::new(CodexResponseParser::new(&model)) as Box<dyn ResponseParser>
                }),
                retry: self.retry,
                cancel,
            }))
        })
    }
}

impl std::fmt::Debug for OpenAiCodexProvider {
    /// Never prints the credential source; only the route identity is safe.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OpenAiCodexProvider")
            .field("model", &self.model)
            .field("base_url", &self.base_url)
            .finish_non_exhaustive()
    }
}

/// Reject a credential that cannot name the ChatGPT account. This is the one
/// enforcement point for `build_headers`' account-id requirement on the live
/// path, so the header builder itself is infallible once a credential exists.
struct AccountIdGuard {
    inner: Arc<dyn CredentialSource>,
}

impl CredentialSource for AccountIdGuard {
    fn access<'a>(&'a self) -> BoxFuture<'a, Result<Credential, ProviderError>> {
        Box::pin(async move {
            let credential = self.inner.access().await?;
            require_account_id(credential)
        })
    }

    fn refresh<'a>(
        &'a self,
        rejected: &'a Credential,
    ) -> BoxFuture<'a, Result<Credential, ProviderError>> {
        Box::pin(async move {
            let credential = self.inner.refresh(rejected).await?;
            require_account_id(credential)
        })
    }
}

fn require_account_id(credential: Credential) -> Result<Credential, ProviderError> {
    match credential.account_id.as_deref() {
        Some(account_id) if !account_id.is_empty() => Ok(credential),
        _ => Err(ProviderError::new(
            ProviderErrorKind::Authentication,
            "the Codex credential has no ChatGPT account id",
        )),
    }
}

#[cfg(test)]
mod tests {
    use futures_util::StreamExt;
    use p1_contracts::{ModelOptions, Outcome, Provider};
    use p1_provider_http::testing::ScriptedTransport;

    use super::*;

    struct NoCredentials;

    impl CredentialSource for NoCredentials {
        fn access<'a>(&'a self) -> BoxFuture<'a, Result<Credential, ProviderError>> {
            Box::pin(async move {
                Ok(Credential {
                    bearer: "SENTINEL-ACCESS".to_string(),
                    account_id: None,
                })
            })
        }

        fn refresh<'a>(
            &'a self,
            _rejected: &'a Credential,
        ) -> BoxFuture<'a, Result<Credential, ProviderError>> {
            Box::pin(async move {
                Ok(Credential {
                    bearer: "SENTINEL-ACCESS".to_string(),
                    account_id: None,
                })
            })
        }
    }

    fn provider() -> OpenAiCodexProvider {
        OpenAiCodexProvider::new(
            "gpt-test",
            Arc::new(ScriptedTransport::new(Vec::new())),
            Arc::new(NoCredentials),
        )
        .with_base_url("https://example.test/backend")
    }

    #[test]
    fn describe_reports_the_route_and_freeform_support() {
        let description = provider().describe();
        assert_eq!(description.origin.route, ROUTE);
        assert_eq!(description.origin.model, "gpt-test");
        assert!(description.supports_freeform_tools);
        assert!(description.mandatory_prompt_prefix.is_none());
        assert!(!description.reports_cost);
        assert_eq!(description.cache_key, CacheKeySupport::Optional);
    }

    #[test]
    fn validate_rejects_max_output_tokens() {
        let options = ModelOptions {
            max_output_tokens: Some(1),
            ..ModelOptions::default()
        };
        let request = ProviderRequest {
            system_prompt: String::new(),
            history: Vec::new(),
            tools: Vec::new(),
            options,
        };
        assert_eq!(
            provider().validate(&request).unwrap_err().kind,
            ProviderErrorKind::InvalidRequest
        );
    }

    #[test]
    fn an_empty_explicit_cache_key_is_rejected_not_sent() {
        let options = ModelOptions {
            cache_key: Some(String::new()),
            ..ModelOptions::default()
        };
        let request = ProviderRequest {
            system_prompt: String::new(),
            history: Vec::new(),
            tools: Vec::new(),
            options,
        };
        let error = provider().validate(&request).unwrap_err();
        assert_eq!(error.kind, ProviderErrorKind::InvalidRequest);
        assert!(error.message.contains("cache_key"), "{}", error);
    }

    #[test]
    fn a_native_option_in_another_adapters_namespace_is_an_error() {
        for key in ["anthropic-messages.thinking", "openai-chat.future_flag"] {
            let mut options = ModelOptions::default();
            options
                .native
                .insert(key.to_string(), serde_json::json!(true));
            let request = ProviderRequest {
                system_prompt: String::new(),
                history: Vec::new(),
                tools: Vec::new(),
                options,
            };
            let error = provider().validate(&request).unwrap_err();
            assert_eq!(error.kind, ProviderErrorKind::InvalidRequest, "{key}");
            for part in [
                format!("option \"{key}\""),
                format!("route \"{ROUTE}\""),
                format!("(adapter {ADAPTER})"),
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
        let request = ProviderRequest {
            system_prompt: String::new(),
            history: Vec::new(),
            tools: Vec::new(),
            options,
        };
        assert!(provider().validate(&request).is_ok());
    }

    #[tokio::test]
    async fn a_credential_without_an_account_id_fails_authentication() {
        let provider = provider();
        let request = ProviderRequest {
            system_prompt: String::new(),
            history: Vec::new(),
            tools: Vec::new(),
            options: ModelOptions::default(),
        };
        let mut stream = provider
            .stream(request, CancellationToken::new())
            .await
            .expect("setup succeeds");
        let mut events = Vec::new();
        while let Some(event) = stream.next().await {
            events.push(event);
        }
        match events.last() {
            Some(p1_contracts::StreamEvent::Finished(Outcome::Failed(error))) => {
                assert_eq!(error.kind, ProviderErrorKind::Authentication);
            }
            other => panic!("expected authentication failure, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn debug_does_not_print_credentials() {
        let provider = provider();
        let text = format!("{provider:?}");
        assert!(!text.contains("SENTINEL-ACCESS"));
        assert!(text.contains("gpt-test"));
    }
}
