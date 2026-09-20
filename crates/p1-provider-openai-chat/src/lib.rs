//! Function-tool Chat Completions on the OpenCode Go and Z.ai subscriptions.
//! No public API fallback: each route fixes its subscription endpoint.
mod credentials;
mod parser;
mod request;

pub use credentials::SubscriptionCredentials;
use p1_contracts::{
    BoxFuture, CancellationToken, Origin, Provider, ProviderError, ProviderRequest, ProviderStream,
    RouteDescription,
};
use p1_provider_http::{
    CredentialSource, DriveRequest, HttpRequest, RetryPolicy, Transport, drive,
};
pub use request::build_request;
use std::sync::Arc;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubscriptionRoute {
    OpenCodeGo,
    Glm,
}

impl SubscriptionRoute {
    pub fn id(self) -> &'static str {
        match self {
            Self::OpenCodeGo => "openai-chat/opencode-go-subscription",
            Self::Glm => "openai-chat/glm-subscription",
        }
    }
    pub fn endpoint(self) -> &'static str {
        match self {
            Self::OpenCodeGo => "https://opencode.ai/zen/go/v1/chat/completions",
            Self::Glm => "https://api.z.ai/api/coding/paas/v4/chat/completions",
        }
    }
    pub fn origin(self, model: &str) -> Origin {
        Origin {
            route: self.id().into(),
            model: model.into(),
        }
    }
}

pub struct ChatProvider {
    route: SubscriptionRoute,
    model: String,
    transport: Arc<dyn Transport>,
    credentials: Arc<dyn CredentialSource>,
    retry: RetryPolicy,
}
impl ChatProvider {
    pub fn new(
        route: SubscriptionRoute,
        model: &str,
        transport: Arc<dyn Transport>,
        credentials: Arc<dyn CredentialSource>,
    ) -> Self {
        Self {
            route,
            model: model.into(),
            transport,
            credentials,
            retry: RetryPolicy::default(),
        }
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
            .field("model", &self.model)
            .finish_non_exhaustive()
    }
}
impl Provider for ChatProvider {
    fn describe(&self) -> RouteDescription {
        RouteDescription {
            origin: self.route.origin(&self.model),
            supports_freeform_tools: false,
            mandatory_prompt_prefix: None,
            reports_cost: false,
        }
    }
    fn validate(&self, request: &ProviderRequest) -> Result<(), ProviderError> {
        request::validate(self.route, request)
    }
    fn stream<'a>(
        &'a self,
        request: ProviderRequest,
        cancel: CancellationToken,
    ) -> BoxFuture<'a, Result<ProviderStream, ProviderError>> {
        Box::pin(async move {
            let body = serde_json::to_vec(&build_request(self.route, &self.model, &request)?)
                .map_err(|_| request::invalid("cannot encode request"))?;
            let route = self.route;
            let model = self.model.clone();
            let cache_key = request.options.cache_key;
            Ok(drive(DriveRequest {
                transport: self.transport.clone(),
                credentials: self.credentials.clone(),
                build: Box::new(move |credential| {
                    let mut headers = vec![
                        ("content-type".into(), "application/json".into()),
                        ("accept".into(), "text/event-stream".into()),
                        (
                            "user-agent".into(),
                            concat!("p1/", env!("CARGO_PKG_VERSION")).into(),
                        ),
                        (
                            "authorization".into(),
                            format!("Bearer {}", credential.bearer),
                        ),
                    ];
                    if route == SubscriptionRoute::OpenCodeGo {
                        if let Some(key) = &cache_key {
                            headers.push(("x-opencode-session".into(), key.clone()));
                        }
                    }
                    HttpRequest {
                        url: route.endpoint().into(),
                        headers,
                        body: body.clone(),
                    }
                }),
                new_parser: Box::new(move || {
                    Box::new(parser::ChatParser::new(route.origin(&model)))
                }),
                retry: self.retry,
                cancel,
            }))
        })
    }
}
