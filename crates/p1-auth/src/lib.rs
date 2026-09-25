//! Credential lookup for a route (ADR-0040, spec `docs/design/credentials.md`).
//!
//! A credential belongs to a ROUTE, never to a wire adapter or a model profile.
//! This crate owns every place a credential is read from and the order they are
//! tried in: [`resolve`] builds the [`CredentialSource`](p1_provider_http::CredentialSource)
//! a provider adapter is handed, and [`describe`] reports WHICH source a route
//! would use — never a value. It owns the store's WRITE side too:
//! [`store::put_api_key`] and [`store::remove`] are what `p1 login` and `p1 logout`
//! call (spec §6, ADR-0044).
//!
//! The chain, per route kind (spec §2): the documented environment variable, then
//! p1's own store, then the login of another tool the owner already has. The first
//! source that HAS an entry wins; a source that has an entry which is unusable is
//! an error naming it, never a silent fall-through. A refresh is written back to
//! the source the credential came from, never to another one.
//!
//! A route may opt out of every borrowed source with `store_only = true`
//! (ADR-0061): its chain is then the documented environment variable and p1's own
//! store, and no other tool's login file is opened. A route without the field keeps
//! the chain above unchanged.
//!
//! A route may also declare `kind = "none"` (issue #134): it sends NO credential at
//! all, because an egress proxy injects the provider's credential after the request
//! leaves the process. Nothing is loaded — no variable, no store entry, no login —
//! and there is nothing to refresh; [`resolve`] answers with a placeholder no
//! adapter may send, as [`p1_provider_http::CredentialSource::proxy_injected`]
//! declares.
//!
//! Linux-only today: the store's permission check uses
//! `std::os::unix::fs::PermissionsExt`, and there is no cfg scaffolding for other
//! systems (spec §3).
//!
//! No credential value ever reaches a `Debug`, a `Display`, an error message or a
//! [`SourceReport`]: the report's fields cannot hold one.

mod api_key;
mod claude_code;
mod codex;
mod locations;
mod refresh_http;
mod resolve;
mod spec;
pub mod store;

pub use api_key::SubscriptionCredentials;
pub use claude_code::ClaudeCodeCredentials;
pub use codex::{Clock, CodexCliCredentials};
pub use locations::Locations;
pub use resolve::{CredentialPolicy, Presence, SourceName, SourceReport, describe, resolve};
pub use spec::{BorrowSource, BorrowStore, CredentialKind, CredentialSpec};

use p1_contracts::{ProviderError, ProviderErrorKind};

/// The one error kind a credential problem ever is.
fn auth(message: impl Into<String>) -> ProviderError {
    ProviderError::new(ProviderErrorKind::Authentication, message)
}
