//! The ChatGPT/Codex subscription route of the OpenAI Responses API.
//!
//! This adapter translates the p1 provider contract into the wire shape
//! documented in `docs/design/routes.md` §B: HTTPS + SSE, or — when a route asks
//! for `transport = "websocket"` — the vendor's WebSocket mode with SSE as its
//! fallback (ADR-0047, `docs/design/websocket.md`). The donor's continuation,
//! native compaction, structured summaries and login flow are still deliberately
//! not taken; where the credential comes from is `p1-auth`'s business, never a
//! login flow here.
//!
//! The adapter is composed from three independent inputs (ADR-0039): this wire
//! adapter, a [`ResponsesRoute`] (an account and endpoint, data in
//! `routes/<id>.toml`) and a [`p1_model_profile::ModelProfile`] (what the model
//! is, data in `profiles/<id>.toml`). The shipped
//! `openai-codex-subscription` route reaches the ChatGPT/Codex subscription
//! (`Origin.route` `openai-responses/codex-subscription`): `POST
//! {endpoint}/codex/responses`, streaming SSE, and reasoning replayed
//! byte-exact only for the same route + model.
//!
//! Layout:
//! - [`request`]: the pure request builder, header builder and base-URL resolver.
//! - `parser`: the pure SSE state machine, surfaced through [`p1_provider_http::drive`].
//! - `websocket`: the WebSocket transport — handshake, framing, connection
//!   lifetime and the failure policy of `docs/design/websocket.md` §5.
//! - [`websocket_lower`]: its portable decisions (framing, continuation, fallback).
//! - [`provider`]: the [`p1_contracts::Provider`] implementation.
//!
//! The crate is split like `p1-provider-http` (ADR-0071). PORTABLE, always
//! compiled: the route and settings types, composition validation,
//! [`validate_request`], the credential-free lowering ([`lower_request`]), the
//! [`CodexResponseParser`] and its `on_http_error` classification — what a
//! provider WebAssembly component needs. NATIVE, behind the default `native`
//! feature: the provider, [`build_headers`] and the WebSocket transport,
//! everything that touches a transport, a runtime or a credential.

mod parser;
#[cfg(feature = "native")]
mod provider;
mod request;
#[cfg(feature = "native")]
mod websocket;
pub mod websocket_lower;

pub use parser::CodexResponseParser;
#[cfg(feature = "native")]
pub use provider::{Clock, OpenAiCodexProvider, OpenAiCodexProviderBuilder};
#[cfg(feature = "native")]
pub use request::build_headers;
pub use request::{
    LoweredRequest, build_headers_without_credential, build_request, lower_request, request_path,
    resolve_base_url, validate_composition, validate_request,
};

/// The `origin_route` of the shipped `routes/openai-codex-subscription.toml`, byte for
/// byte what this adapter wrote into every [`p1_contracts::Origin`] before route data
/// existed: sessions recorded then stay resumable (ADR-0033, spec §4). The route file is
/// the source of truth for a composed provider; this constant names today's value.
pub const ROUTE: &str = "openai-responses/codex-subscription";

/// The IMPLEMENTED account behaviours of the Responses adapter. A route file names one
/// and the adapter compiles the wire facts that account requires — `store: false`, no
/// output-cap field, the ChatGPT account-id header and the session identity headers —
/// exactly as `openai-chat`'s dialects do. Named by behaviour, never free-form headers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ResponsesAccount {
    /// The ChatGPT/Codex subscription login: an OAuth bearer that must name the
    /// ChatGPT account, and a route that stores no response server-side.
    CodexSubscription,
}

/// The `[adapter_settings]` table of a route whose `adapter` is `openai-responses`:
/// fields this adapter owns, parsed by this adapter
/// (`docs/design/routes-and-profiles.md` §1.2). A key this struct does not name is
/// rejected rather than ignored.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResponsesAdapterSettings {
    pub account: ResponsesAccount,
    /// Absent means [`ResponsesTransport::Sse`], today's transport (ADR-0047 §1).
    #[serde(default)]
    pub transport: ResponsesTransport,
}

/// How a Responses route reaches the model (ADR-0047, `docs/design/websocket.md`
/// §1): HTTPS + SSE, or the vendor's WebSocket mode, which falls back to SSE by
/// the rules of that document's §5. A route file that omits the key gets
/// [`ResponsesTransport::Sse`]; any other value is a route-file error.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ResponsesTransport {
    #[default]
    Sse,
    Websocket,
}

/// How one ChatGPT account and endpoint are reached: the data a route file supplies and
/// the provider is composed from (spec §7.2). It holds no credential value;
/// authentication comes exclusively from a `CredentialSource`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResponsesRoute {
    /// `Origin.route` for every request this route makes and every replay it accepts.
    /// Explicit in the route file so it never drifts from the route id.
    pub origin_route: String,
    /// The API base URL. The adapter appends `/codex/responses`
    /// (see [`resolve_base_url`]).
    pub endpoint: String,
    pub account: ResponsesAccount,
    /// The transport this route asks for. It is route data, like the account: the
    /// WebSocket transport changes only how a request travels, never what it says
    /// (`docs/design/websocket.md` §3), so `Origin` and replay are untouched.
    pub transport: ResponsesTransport,
}

impl ResponsesRoute {
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
            supports_freeform_tools: true,
            mandatory_prompt_prefix: None,
            reports_cost: false,
            // The request builder sends `options.cache_key` as the body's
            // `prompt_cache_key` and the session identity headers.
            cache_key: p1_contracts::CacheKeySupport::Optional,
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
            return Err(invalid("the Responses route needs a nonempty origin route"));
        }
        let Some(rest) = self.endpoint.strip_prefix("https://") else {
            return Err(invalid("the Responses route endpoint requires HTTPS"));
        };
        if rest.split('/').next().is_none_or(str::is_empty)
            || rest.contains(['@', '?', '#'])
            || !rest.bytes().all(|byte| (33..=126).contains(&byte))
        {
            return Err(invalid("invalid Responses route endpoint"));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    /// The manifest is the split's guard: a transport or runtime dependency that
    /// the default `native` feature does not gate would let a guest build reach
    /// sockets, a runtime or credentials without any compile error here.
    const MANIFEST: &str = include_str!("../Cargo.toml");

    fn dependencies() -> &'static str {
        MANIFEST
            .split("[dependencies]")
            .nth(1)
            .and_then(|rest| rest.split("\n[").next())
            .expect("a dependencies table")
    }

    fn dependency_line(name: &str) -> &'static str {
        let prefix = format!("{name} = ");
        dependencies()
            .lines()
            .find(|line| line.starts_with(&prefix))
            .unwrap_or_else(|| panic!("{name} is not a dependency"))
    }

    #[test]
    fn native_only_dependencies_are_optional_and_enabled_by_the_default_native_feature() {
        assert!(MANIFEST.contains("default = [\"native\"]"));
        let native = MANIFEST
            .split("\nnative = [")
            .nth(1)
            .and_then(|rest| rest.split(']').next())
            .expect("a native feature");
        assert!(native.contains("\"p1-provider-http/native\""), "{native}");
        assert!(
            dependency_line("p1-provider-http").contains("default-features = false"),
            "p1-provider-http must not bring its native default"
        );
        for name in ["futures-util", "tokio"] {
            assert!(
                dependency_line(name).contains("optional = true"),
                "{name} must be optional"
            );
            assert!(
                native.contains(&format!("\"dep:{name}\"")),
                "native must enable {name}"
            );
        }
    }

    #[test]
    fn no_dependency_on_p1_auth() {
        assert!(!MANIFEST.contains("p1-auth"));
    }
}
