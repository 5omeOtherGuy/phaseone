//! A composed Chat Completions provider: wire dialect, route data and model policy.
//! Credentials are supplied through CredentialSource; this crate performs no login lookup.
//!
//! The crate is split like `p1-provider-http` (ADR-0071). PORTABLE, always compiled:
//! the route and settings types, composition validation, [`validate_request`], the
//! credential-free lowering ([`lower_request`]), the [`ChatParser`] and its
//! `on_http_error` classification — what a provider WebAssembly component needs.
//! NATIVE, behind the default `native` feature: `ChatProvider`, everything that
//! touches a transport or a credential.
mod parser;
#[cfg(feature = "native")]
mod provider;
mod replay;
mod request;

use p1_contracts::{
    CacheKeySupport, Effort, Origin, ProviderError, ProviderRequest, RouteDescription,
};
use p1_model_profile::{ModelProfile, ThinkingPolicy};
pub use parser::ChatParser;
#[cfg(feature = "native")]
pub use provider::ChatProvider;
pub use replay::{REPLAY_VERSION, Replay, decode, encode};
pub use request::{build_request, validate as validate_request};

/// Implemented encodings, not service names. Unknown extensions require an implementation.
/// The names are the kebab-case spellings a route file's `[adapter_settings]` uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ChatDialect {
    /// Enabled thinking with replayable reasoning_content and its equivalent reasoning alias.
    #[default]
    ThinkingWithReasoningAlias,
    /// Enabled/preserved thinking, replayable reasoning_content and its equivalent reasoning
    /// alias, and streaming function inputs.
    RetainedThinking,
}

/// The non-secret client identity a route can present to a vendor gateway that gates its
/// free tier on the caller looking like the vendor's own client (owner decision 2026-09-24).
/// Unlike a dialect, it changes no message encoding and declares no tool: it only adds the
/// identity's static headers and a generated session id. The tool names a gate wants are an
/// environment's business, never the adapter's. The names are the kebab-case spellings a
/// route file's `[adapter_settings]` uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ClientIdentity {
    /// Present the OpenCode CLI's identity to the OpenCode Zen gateway: its `user-agent`
    /// and its `x-opencode-*` headers with a generated `ses_`/`msg_` id. The system prompt
    /// and the declared tools are NOT changed; the `zen`/`zen2`/`zen3` environments grant
    /// the `bash`/`read` names the gate wants.
    Opencode,
}

/// The `[adapter_settings]` table of a route whose `adapter` is `openai-chat`: fields
/// this adapter owns, parsed by this adapter (`docs/design/routes-and-profiles.md`
/// §1.2). A key this struct does not name is rejected rather than ignored.
#[derive(Debug, Clone, PartialEq, Eq, Default, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChatAdapterSettings {
    pub dialect: ChatDialect,
    /// The header a cache/session key travels in. `None`: the route carries none, so
    /// an explicit cache key is an error for this route.
    #[serde(default)]
    pub session_header: Option<String>,
    /// A non-secret client identity the route presents instead of p1's own. `None`
    /// (the default): p1 sends its own `user-agent` and the cache key as the session
    /// header, and no foreign client is impersonated.
    #[serde(default)]
    pub client_identity: Option<ClientIdentity>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ChatLimits {
    /// A route ceiling may restrict a model profile, never enlarge it.
    pub max_output_tokens: Option<u32>,
}

#[derive(Clone, PartialEq, Eq, Default)]
pub struct ChatRoute {
    pub origin_route: String,
    pub endpoint: String,
    /// Non-secret headers only. Authentication comes exclusively from CredentialSource.
    pub headers: Vec<(String, String)>,
    pub session_header: Option<String>,
    pub dialect: ChatDialect,
    pub client_identity: Option<ClientIdentity>,
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
    /// What a provider composed from this route and `wire_model` is.
    pub fn describe(&self, wire_model: &str) -> RouteDescription {
        RouteDescription {
            origin: self.origin(wire_model),
            supports_freeform_tools: false,
            mandatory_prompt_prefix: None,
            reports_cost: false,
            // `options.cache_key` is consumed exactly as the route's session
            // header; a route without one takes no key at all (validate rejects
            // an explicit key on such a route).
            cache_key: if self.session_header.is_some() {
                CacheKeySupport::Optional
            } else {
                CacheKeySupport::Unsupported
            },
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
            if self.client_identity.is_some()
                && OPENCODE_IDENTITY_OWNED_HEADERS
                    .iter()
                    .any(|owned| name.eq_ignore_ascii_case(owned))
            {
                return Err(request::invalid(&format!(
                    "static chat header `{name}` is owned by the client identity"
                )));
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
/// The `user-agent` OpenCode's CLI sends to the Zen gateway (captured 2026-09-24,
/// opencode 1.18.31; see `docs/design/zen-client-identity-evidence.md`).
const OPENCODE_USER_AGENT: &str =
    "opencode/1.18.31 ai-sdk/provider-utils/4.0.23 runtime/bun/1.3.14";

/// The static header names owned by the OpenCode identity. These must be reserved
/// so a route cannot send a second value for a header the identity also sets.
const OPENCODE_IDENTITY_OWNED_HEADERS: [&str; 4] = [
    "x-opencode-client",
    "x-opencode-project",
    "x-opencode-session",
    "x-opencode-request",
];

/// The headers a `client_identity` adds to a request. Static, non-secret values
/// plus ids generated for this request from the route's cache key.
fn client_identity_headers(
    identity: ClientIdentity,
    cache_key: Option<&str>,
) -> Vec<(String, String)> {
    match identity {
        ClientIdentity::Opencode => {
            let (session, request) = opencode_ids(cache_key);
            vec![
                ("user-agent".into(), OPENCODE_USER_AGENT.into()),
                ("x-opencode-client".into(), "cli".into()),
                ("x-opencode-project".into(), "global".into()),
                ("x-opencode-session".into(), session),
                ("x-opencode-request".into(), request),
            ]
        }
    }
}

/// The `ses_`/`msg_` ids OpenCode shapes as `<prefix>_<12 lowercase hex><14 alnum>`
/// (its `Identifier.ascending`): the gateway's free-tier gate accepts exactly that
/// shape and rejects a short or non-hex-prefixed id with 403. p1 derives both ids
/// from the route's cache key — a pure function, so a resumed session keeps them.
/// Without a key, a per-request nonce does the same job.
fn opencode_ids(cache_key: Option<&str>) -> (String, String) {
    use std::hash::{Hash, Hasher};
    use std::sync::atomic::{AtomicU64, Ordering};
    static NONCE: AtomicU64 = AtomicU64::new(0);
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    match cache_key {
        Some(key) => key.hash(&mut hasher),
        None => {
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|elapsed| elapsed.as_nanos() as u64)
                .unwrap_or(0)
                .hash(&mut hasher);
            NONCE.fetch_add(1, Ordering::Relaxed).hash(&mut hasher);
        }
    }
    let first = hasher.finish();
    let mut second = std::collections::hash_map::DefaultHasher::new();
    first.hash(&mut second);
    let hex = format!("{first:016x}{:016x}", second.finish());
    (
        format!("ses_{}", &hex[..26]),
        format!("msg_{}", &hex[6..32]),
    )
}

type Headers = Vec<(String, String)>;

/// The request's header set in two halves. The native provider puts the credential
/// between them, where it has always been, so a credential-free set never reorders
/// the rest. `cache_key` is the request's own key, unclamped.
fn headers(route: &ChatRoute, cache_key: Option<&str>) -> (Headers, Headers) {
    let identity = route.client_identity;
    let mut headers = route.headers.clone();
    // The identity replaces p1's own user-agent; the identity's session
    // header replaces the generic cache-key session header.
    if identity.is_some() {
        headers.retain(|(name, _)| !name.eq_ignore_ascii_case("user-agent"));
    }
    headers.extend([
        ("content-type".into(), "application/json".into()),
        ("accept".into(), "text/event-stream".into()),
    ]);
    let mut tail = Vec::new();
    match identity {
        None => {
            if let (Some(name), Some(value)) = (&route.session_header, cache_key) {
                tail.push((name.clone(), value.to_string()));
            }
        }
        Some(identity) => {
            tail.extend(client_identity_headers(identity, cache_key));
        }
    }
    (headers, tail)
}

/// Every header a request on `route` sends except the credential, in the native
/// order. A client identity without a cache key generates fresh ids on each call.
pub fn build_headers_without_credential(
    route: &ChatRoute,
    cache_key: Option<&str>,
) -> Vec<(String, String)> {
    let (mut headers, tail) = headers(route, cache_key);
    headers.extend(tail);
    headers
}

/// One request lowered for the wire WITHOUT any credential: what the native
/// provider sends, minus its `authorization` header.
#[derive(Clone, PartialEq, Eq)]
pub struct LoweredRequest {
    /// Appended to the route's endpoint: empty, because a chat route's endpoint
    /// is the complete completions URL.
    pub path: &'static str,
    /// Every header the native request sends except the credential, in its order.
    pub headers: Vec<(String, String)>,
    /// The encoded JSON body.
    pub body: Vec<u8>,
}

/// Validate and lower one request exactly as the native provider does before it
/// opens a transport, so both fail with the same error and send the same bytes.
pub fn lower_request(
    route: &ChatRoute,
    wire_model: &str,
    profile: &ModelProfile,
    request: &ProviderRequest,
) -> Result<LoweredRequest, ProviderError> {
    let body = serde_json::to_vec(&build_request(route, wire_model, profile, request)?)
        .map_err(|_| request::invalid("cannot encode request"))?;
    Ok(LoweredRequest {
        path: "",
        headers: build_headers_without_credential(route, request.options.cache_key.as_deref()),
        body,
    })
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

/// The one composition check, shared by the constructor and the pure request
/// builder: the route data is usable, the profile is valid, and the dialect can
/// express the profile's policy.
pub fn validate_composition(
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
            client_identity: None,
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

#[cfg(test)]
mod manifest_tests {
    /// The manifest is the split's guard: a transport dependency that the default
    /// `native` feature does not gate would let a guest build reach sockets, a
    /// runtime or credentials without any compile error here.
    const MANIFEST: &str = include_str!("../Cargo.toml");

    fn dependencies() -> &'static str {
        MANIFEST
            .split("[dependencies]")
            .nth(1)
            .and_then(|rest| rest.split("\n[").next())
            .expect("a dependencies table")
    }

    #[test]
    fn the_transport_crate_is_native_only_through_the_default_native_feature() {
        assert!(MANIFEST.contains("default = [\"native\"]"));
        assert!(MANIFEST.contains("native = [\"p1-provider-http/native\"]"));
        let http = dependencies()
            .lines()
            .find(|line| line.starts_with("p1-provider-http = "))
            .expect("p1-provider-http is a dependency");
        assert!(http.contains("default-features = false"), "{http}");
    }

    #[test]
    fn no_dependency_on_p1_auth_or_a_runtime() {
        for name in ["p1-auth", "tokio", "futures", "reqwest"] {
            assert!(!dependencies().contains(name), "{name} in [dependencies]");
        }
    }
}
