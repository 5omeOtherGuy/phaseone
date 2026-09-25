//! Credential access for the provider adapters.
//!
//! The source is a seam so an adapter owns *where* a token lives (a CLI's token
//! file, in this slice) while the shared driver owns *when* a refresh happens:
//! once, after a 401/403, with the credential that was rejected. No credential
//! value may reach a log, an error, a fixture or a `Debug` output.

use p1_contracts::{BoxFuture, ProviderError};

/// One access token plus the account it belongs to, where the route reports one.
#[derive(Clone, PartialEq, Eq)]
pub struct Credential {
    pub bearer: String,
    pub account_id: Option<String>,
}

impl std::fmt::Debug for Credential {
    /// Prints `<redacted>` for every field value; a token must never reach a log.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Credential")
            .field("bearer", &"<redacted>")
            .field("account_id", &"<redacted>")
            .finish()
    }
}

pub trait CredentialSource: Send + Sync {
    /// The credential to use for the next attempt.
    fn access<'a>(&'a self) -> BoxFuture<'a, Result<Credential, ProviderError>>;

    /// Called after a 401/403 with the credential that was rejected. Must not
    /// return the rejected credential again.
    fn refresh<'a>(
        &'a self,
        rejected: &'a Credential,
    ) -> BoxFuture<'a, Result<Credential, ProviderError>>;

    /// Whether an egress proxy — not p1 — injects this route's credential after the
    /// request leaves the process (`[credential] kind = "none"`, issue #134). An
    /// adapter that would send `Authorization` (or any other credential header)
    /// sends NONE when this is true: `access()` hands it a placeholder whose
    /// `bearer` is EMPTY and whose value no adapter may send. The driver never
    /// refreshes such a route; a 401/403 is the proxy's refusal, reported as an
    /// [`ProviderErrorKind::Authentication`](p1_contracts::ProviderErrorKind)
    /// failure naming the missing proxy credential. `false` by default, so every
    /// source that resolves a credential of its own is unchanged.
    fn proxy_injected(&self) -> bool {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_redacts_every_field_value() {
        let credential = Credential {
            bearer: "SENTINEL-SECRET-123".to_string(),
            account_id: Some("account-xyz".to_string()),
        };
        let debug = format!("{credential:?}");
        assert!(!debug.contains("SENTINEL-SECRET-123"), "{debug}");
        assert!(!debug.contains("account-xyz"), "{debug}");
        assert_eq!(debug.matches("<redacted>").count(), 2, "{debug}");
    }
}
