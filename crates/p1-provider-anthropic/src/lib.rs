//! The Claude subscription route of the Anthropic Messages API.
//!
//! `anthropic-messages/claude-subscription` is the OAuth (Claude Code login)
//! lane: `POST {base}/v1/messages`, streaming SSE, a mandatory identity block in
//! `system`, and thinking replayed byte-exact only for the same route + model.
//! Wire facts are `docs/design/routes.md` §A; the adapter shape is
//! `docs/design/providers.md`.
//!
//! The crate owns no tools and no prompt policy: [`build_request`] and
//! [`build_headers`] are pure functions over the contracts, the SSE parser is a
//! pure state machine, and [`AnthropicProvider`] wires both onto the shared
//! [`p1_provider_http::drive`] loop. Credentials are reused from the existing
//! Claude Code login by [`ClaudeCodeCredentials`]; p1 has no login flow.
//!
//! No credential value, header value or response-body text ever reaches a
//! [`p1_contracts::ProviderError`], a `Debug` output or a panic message.

mod credentials;
mod parser;
mod provider;
mod request;

pub use credentials::ClaudeCodeCredentials;
pub use provider::AnthropicProvider;
pub use request::{build_headers, build_request};

/// Route identifier carried on every [`p1_contracts::Origin`] this adapter
/// produces or accepts for replay.
pub const ROUTE: &str = "anthropic-messages/claude-subscription";
