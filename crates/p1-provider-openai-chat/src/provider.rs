//! The native [`Provider`]: the composed route, model and profile over the shared
//! transport and drive loop. Everything it sends is the portable lowering plus the
//! credential header.

use std::sync::Arc;

use p1_contracts::{
    BoxFuture, CancellationToken, Provider, ProviderError, ProviderRequest, ProviderStream,
    RouteDescription,
};
use p1_model_profile::ModelProfile;
use p1_provider_http::{
    CredentialSource, DriveRequest, HttpRequest, RetryPolicy, Transport, drive,
};

use crate::{ChatRoute, build_request, headers, parser, request, validate_composition};

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
        self.route.describe(&self.wire_model)
    }
    fn validate(&self, request: &ProviderRequest) -> Result<(), ProviderError> {
        request::validate(&self.route, &self.wire_model, &self.profile, request)
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
            // A route whose credential an egress proxy injects sends NO authentication
            // header (issue #134): the credential the source hands us is a placeholder.
            let proxy_injected = self.credentials.proxy_injected();
            let build_headers = {
                let route = route.clone();
                move |credential: &p1_provider_http::Credential| {
                    // Built per attempt, as before the split: an identity without a
                    // cache key generates fresh ids for every attempt.
                    let (mut headers, tail) = headers(&route, cache_key.as_deref());
                    if !proxy_injected {
                        headers.push((
                            "authorization".into(),
                            format!("Bearer {}", credential.bearer),
                        ));
                    }
                    headers.extend(tail);
                    HttpRequest {
                        url: route.endpoint.clone(),
                        headers,
                        body: body.clone(),
                    }
                }
            };
            Ok(drive(DriveRequest {
                transport: self.transport.clone(),
                credentials: self.credentials.clone(),
                build: Box::new(build_headers),
                new_parser: Box::new(move || {
                    Box::new(parser::ChatParser::new(origin.clone(), dialect))
                }),
                retry: self.retry,
                cancel,
            }))
        })
    }
}
