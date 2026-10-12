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
//! Linux-only today: every credential file is read, locked and replaced through a
//! checked, pinned directory handle ([`credential_file`], issue #484) built on
//! `std::os::unix` and `rustix`, and there is no cfg scaffolding for other systems
//! (spec §3).
//!
//! No credential value ever reaches a `Debug`, a `Display`, an error message or a
//! [`SourceReport`]: the report's fields cannot hold one.

#[cfg(feature = "subscriptions")]
mod api_key;
#[cfg(feature = "subscriptions")]
mod claude_code;
#[cfg(feature = "subscriptions")]
mod codex;
mod credential_file;
mod locations;
mod method;
mod refresh_http;
mod resolve;
mod spec;
pub mod store;
#[cfg(feature = "subscriptions")]
mod subscriptions;

#[cfg(feature = "subscriptions")]
pub use api_key::SubscriptionCredentials;
#[cfg(feature = "subscriptions")]
pub use claude_code::ClaudeCodeCredentials;
#[cfg(feature = "subscriptions")]
pub use codex::{Clock, CodexCliCredentials};
pub use credential_file::check_owned_directory;
pub use locations::Locations;
pub use method::{
    ApiKeyMethod, BorrowedLogin, CredentialMethod, NoneMethod, OauthRefresh, RefreshedTokens,
    StoreForm, method, methods,
};
pub use resolve::{
    CredentialPolicy, Entry, Presence, SourceName, SourceReport, describe, resolve,
    resolve_with_store_origin,
};
pub use spec::{BorrowSource, BorrowStore, CredentialKind, CredentialSpec};
#[cfg(feature = "subscriptions")]
pub use subscriptions::{
    CLAUDE_CODE_LOGIN, CLAUDE_CODE_OAUTH, CODEX_LOGIN, CODEX_OAUTH, OPENCODE_LOGIN, PI_LOGIN,
};

use p1_contracts::{ProviderError, ProviderErrorKind};

/// What a build without the `subscriptions` feature says of a credential method, dialect,
/// borrowed login or usage probe that only the subscription connectors implement
/// (ADR-0200): it names where they live, so a refused file says what is missing.
pub const WITHOUT_SUBSCRIPTIONS: &str = "part of the subscription connectors \
     (connectors/subscriptions), and this p1 is built without them (cargo feature \
     `subscriptions`)";

/// A key is a token a header can carry: non-empty, printable ASCII, no spaces.
pub(crate) fn usable_key(value: &str) -> bool {
    !value.is_empty() && value.bytes().all(|b| (33..=126).contains(&b))
}

/// The one error kind a credential problem ever is.
fn auth(message: impl Into<String>) -> ProviderError {
    ProviderError::new(ProviderErrorKind::Authentication, message)
}
