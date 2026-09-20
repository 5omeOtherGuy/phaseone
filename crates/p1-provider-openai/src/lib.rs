//! The ChatGPT/Codex subscription route of the OpenAI Responses API.
//!
//! This adapter translates the p1 provider contract into the HTTPS + SSE wire
//! shape documented in `docs/design/routes.md` §B. The donor's WebSocket
//! transport, continuation, native compaction, structured summaries and login
//! flow are deliberately not taken; credentials are read from the Codex CLI's
//! own auth file, never written by a login flow.
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
//! - [`provider`]: the [`p1_contracts::Provider`] implementation.
//! - [`credentials`]: a file-based [`p1_provider_http::CredentialSource`] over the
//!   Codex CLI auth file, with rotating-refresh write-back.

mod credentials;
mod parser;
mod provider;
mod request;

pub use credentials::{Clock, CodexCliCredentials};
pub use provider::OpenAiCodexProvider;
pub use request::{build_headers, build_request, resolve_base_url};

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
}

impl ResponsesRoute {
    pub fn origin(&self, wire_model: &str) -> p1_contracts::Origin {
        p1_contracts::Origin {
            route: self.origin_route.clone(),
            model: wire_model.to_string(),
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
