//! The Anthropic Messages wire adapter.
//!
//! The adapter is composed from three independent inputs (ADR-0039): this wire
//! adapter, a [`MessagesRoute`] (an account and endpoint, data in `routes/<id>.toml`)
//! and a [`p1_model_profile::ModelProfile`] (what the model is, data in
//! `profiles/<id>.toml`). The shipped `anthropic-subscription` route reaches the
//! Claude Code subscription (`Origin.route` `anthropic-messages/claude-subscription`):
//! `POST {endpoint}/v1/messages`, streaming SSE, the account's mandatory identity
//! block in `system`, and thinking replayed byte-exact only for the same route +
//! model. Wire facts are `docs/design/routes.md` §A; the adapter shape is
//! `docs/design/providers.md`.
//!
//! The crate owns no tools, no prompt policy and no credential lookup:
//! [`build_request`] and [`build_headers`] are pure functions over the contracts,
//! the SSE parser is a pure state machine, and [`AnthropicProvider`] wires both onto
//! the shared [`p1_provider_http::drive`] loop. Where a credential comes from is
//! `p1-auth`'s business; this adapter only ever sees an
//! [`Arc<dyn CredentialSource>`](p1_provider_http::CredentialSource).
//!
//! No credential value, header value or response-body text ever reaches a
//! [`p1_contracts::ProviderError`], a `Debug` output or a panic message.
//!
//! The crate is split like `p1-provider-http` (ADR-0071). PORTABLE, always
//! compiled: the route and settings types, composition validation,
//! [`validate_request`], the credential-free lowering ([`lower_request`]), the
//! [`AnthropicParser`] and its `on_http_error` classification — what a provider
//! WebAssembly component needs. NATIVE, behind the default `native` feature:
//! [`AnthropicProvider`] and [`build_headers`], everything that touches a
//! transport or a credential.

mod parser;
#[cfg(feature = "native")]
mod provider;
mod replay;
mod request;

pub use parser::AnthropicParser;
#[cfg(feature = "native")]
pub use provider::AnthropicProvider;
pub use replay::{REPLAY_VERSION, Replay, WireBlock, decode, encode};
#[cfg(feature = "native")]
pub use request::build_headers;
pub use request::{
    LoweredRequest, build_headers_without_credential, build_request, lower_request,
    validate_composition, validate_request, with_long_context,
};

/// The `origin_route` of the shipped `routes/anthropic-subscription.toml`, byte for
/// byte what this adapter wrote into every [`p1_contracts::Origin`] before route data
/// existed: sessions recorded then stay resumable (ADR-0033, spec §4). The route file
/// is the source of truth for a composed provider; this constant names today's value.
pub const ROUTE: &str = "anthropic-messages/claude-subscription";

/// The IMPLEMENTED account behaviours of the Messages adapter. A route file names
/// one and the adapter compiles the wire facts that account requires — the Claude
/// Code identity prefix and the OAuth beta/version header set — exactly as
/// `openai-chat`'s dialects do. Named by behaviour, never free-form headers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum MessagesAccount {
    /// The Claude Code subscription login: an OAuth bearer, the CLI identity block
    /// and the betas this account accepts.
    ClaudeCodeSubscription,
}

/// The `[adapter_settings]` table of a route whose `adapter` is
/// `anthropic-messages`: fields this adapter owns, parsed by this adapter
/// (`docs/design/routes-and-profiles.md` §1.2). A key this struct does not name is
/// rejected rather than ignored.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MessagesAdapterSettings {
    pub account: MessagesAccount,
    /// Request the 1M-token context window (the `context-1m` beta). Off unless the
    /// route file says so.
    #[serde(default)]
    pub long_context: bool,
}

/// How one Messages account and endpoint are reached: the data a route file
/// supplies and the provider is composed from (spec §7.2). It holds no credential
/// value; authentication comes exclusively from a `CredentialSource`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MessagesRoute {
    /// `Origin.route` for every request this route makes and every replay it
    /// accepts. Explicit in the route file so it never drifts from the route id.
    pub origin_route: String,
    /// The API base URL. The adapter appends `/v1/messages`.
    pub endpoint: String,
    pub account: MessagesAccount,
    /// Send the 1M-context beta on every request of this route.
    pub long_context: bool,
}

impl MessagesRoute {
    pub fn origin(&self, wire_model: &str) -> p1_contracts::Origin {
        p1_contracts::Origin {
            route: self.origin_route.clone(),
            model: wire_model.to_string(),
        }
    }

    /// What a provider composed from this route and `wire_model` is.
    pub fn describe(&self, wire_model: &str) -> p1_contracts::RouteDescription {
        p1_contracts::RouteDescription {
            origin: self.origin(wire_model),
            // The Messages route declares JSON-schema function tools only.
            supports_freeform_tools: false,
            mandatory_prompt_prefix: Some(crate::request::IDENTITY.to_string()),
            // A subscription bills by plan, not per request: cost is unknown.
            reports_cost: false,
            // This route caches with `cache_control` markers; `options.cache_key`
            // never reaches the wire, so an explicit one is rejected by validation.
            cache_key: p1_contracts::CacheKeySupport::Unsupported,
        }
    }

    fn validate(&self) -> Result<(), p1_contracts::ProviderError> {
        let invalid = |message: &str| {
            p1_contracts::ProviderError::new(
                p1_contracts::ProviderErrorKind::InvalidRequest,
                message,
            )
        };
        if self.origin_route.is_empty() {
            return Err(invalid("the Messages route needs a nonempty origin route"));
        }
        let Some(rest) = self.endpoint.strip_prefix("https://") else {
            return Err(invalid("the Messages route endpoint requires HTTPS"));
        };
        if rest.split('/').next().is_none_or(str::is_empty)
            || rest.contains(['@', '?', '#'])
            || !rest.bytes().all(|byte| (33..=126).contains(&byte))
        {
            return Err(invalid("invalid Messages route endpoint"));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    /// The manifest is the split's guard: a transport dependency that the default
    /// `native` feature does not gate would let a guest build reach sockets, a
    /// runtime or credentials without any compile error here.
    const MANIFEST: &str = include_str!("../Cargo.toml");

    #[test]
    fn the_transport_crate_is_native_only_through_the_default_native_feature() {
        assert!(MANIFEST.contains("default = [\"native\"]"));
        assert!(MANIFEST.contains("native = [\"p1-provider-http/native\"]"));
        let dependencies = MANIFEST
            .split("[dependencies]")
            .nth(1)
            .and_then(|rest| rest.split("\n[").next())
            .expect("a dependencies table");
        let http = dependencies
            .lines()
            .find(|line| line.starts_with("p1-provider-http = "))
            .expect("p1-provider-http is a dependency");
        assert!(http.contains("default-features = false"), "{http}");
    }

    #[test]
    fn no_dependency_on_p1_auth_or_a_runtime() {
        let dependencies = MANIFEST
            .split("[dependencies]")
            .nth(1)
            .and_then(|rest| rest.split("\n[").next())
            .expect("a dependencies table");
        for name in ["p1-auth", "tokio", "futures", "reqwest"] {
            assert!(!dependencies.contains(name), "{name} in [dependencies]");
        }
    }
}
