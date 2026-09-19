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
