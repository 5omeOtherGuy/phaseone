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

mod parser;
mod provider;
mod request;

pub use provider::AnthropicProvider;
pub use request::{
    CONTEXT_1M_BETA, CONTEXT_1M_BETA_THRESHOLD, build_headers, build_request, context_window_tokens,
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
}

impl MessagesRoute {
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
