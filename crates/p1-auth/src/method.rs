//! The credential-method port (ADR-0211): what a route's `[credential]` method means, in one
//! trait every caller asks instead of matching a closed list of kinds.
//!
//! A method names itself as route and account files spell it (`kind`/`method`), checks the
//! table that names it, says what p1's own store keeps for it, lists the other tools' logins
//! it reads after the store, the files those logins are, and the usage probe its credential
//! serves. The chain itself — the documented environment variable, then p1's store, then the
//! borrowed logins unless the route is store-only — is [`crate::resolve`]'s, the same for
//! every method.
//!
//! p1's core has two methods, [`ApiKeyMethod`] and [`NoneMethod`]. Every other method is a
//! credential module's ([`crate::module`], ADR-0212): the composition root lists the installed
//! ones, and [`crate::Methods`] resolves a route's method name through that list.

use std::path::PathBuf;
use std::sync::Arc;

use p1_provider_http::{HttpRequest, Transport};

use crate::locations::Locations;
use crate::resolve::{Entry, Presence, SourceName};
use crate::spec::CredentialSpec;

/// One credential method (ADR-0211).
pub trait CredentialMethod: Send + Sync + 'static {
    /// The spelling a route or account file uses for it.
    fn name(&self) -> &str;

    /// The method as `p1 login --list` shows it.
    fn label(&self) -> &str {
        self.name()
    }

    /// Who mints a credential of this method, for `p1 login`'s explanations.
    fn owner(&self) -> &str;

    /// What p1's own store keeps for a route of this method.
    fn store(self: Arc<Self>) -> StoreForm;

    /// The checks of a `[credential]` table naming this method, after the shared ones of
    /// [`CredentialSpec::validate`].
    fn validate(&self, _spec: &CredentialSpec) -> Result<(), String> {
        Ok(())
    }

    /// Whether a route of this method may name the `login_dir` it borrows (ADR-0074).
    fn takes_login_dir(&self) -> bool {
        false
    }

    /// The other tools' logins a route of this method reads after p1's store, in order.
    /// Never asked for a store-only route, whose chain ends at the store (ADR-0061).
    fn borrowed(self: Arc<Self>, _spec: &CredentialSpec) -> Vec<Box<dyn BorrowedLogin>> {
        Vec::new()
    }

    /// The credential files a route of this method reads beyond the fixed ones every file
    /// tool refuses (`p1_workspace::CredentialPolicy`).
    fn owned_files(&self, _spec: &CredentialSpec, _locations: &Locations) -> Vec<PathBuf> {
        Vec::new()
    }

    /// Whether `p1 login <route> --import` can copy the login this method borrows into p1's
    /// store as an OAuth entry (ADR-0074).
    fn imports(&self) -> bool {
        false
    }

    /// The usage probe a route of this method answers when its account names none
    /// (`p1-usage`, ADR-0139), by the probe's name.
    fn usage_probe(&self) -> Option<&str> {
        None
    }
}

/// What p1's own store keeps for a method's routes (spec §3).
#[derive(Clone)]
pub enum StoreForm {
    /// Nothing: the route sends no credential.
    Nothing,
    /// A pasted API key, which `p1 login` writes.
    ApiKey,
    /// OAuth tokens, refreshed through the method's token endpoint.
    Oauth(Arc<dyn OauthRefresh>),
}

impl StoreForm {
    /// Whether the store keeps an API key.
    pub fn is_api_key(&self) -> bool {
        matches!(self, StoreForm::ApiKey)
    }

    /// Whether the store keeps OAuth tokens.
    pub fn is_oauth(&self) -> bool {
        matches!(self, StoreForm::Oauth(_))
    }
}

/// How an OAuth method's tokens are refreshed: the request to its token endpoint and what
/// its answer holds. Nothing here names a value in an error.
pub trait OauthRefresh: Send + Sync + 'static {
    /// The refresh request for `refresh_token`, or why none may be sent.
    fn request(&self, refresh_token: &str) -> Result<HttpRequest, String>;
    /// The rotated tokens of a refresh response `body`, each checked on its own, the expiry
    /// counted from `sent_at_ms`.
    fn parse(&self, body: &[u8], sent_at_ms: u64) -> RefreshedTokens;
}

/// A refresh response, each field checked on its own: a rotated refresh token is kept even
/// when the rest of the response cannot be used, so the login survives.
pub struct RefreshedTokens {
    /// The new access token, or why it cannot be used.
    pub access: Result<String, String>,
    /// The rotated refresh token; `None` when the server rotates none.
    pub refresh: Option<String>,
    /// When the new token expires (milliseconds since the Unix epoch, `None` when it records
    /// none), or why the response's expiry cannot be used.
    pub expires_at_ms: Result<Option<u64>, String>,
}

/// Another tool's login a method reads after p1's store (ADR-0040: read-only reuse, a
/// rotation written back to the same file).
pub trait BorrowedLogin: Send + Sync {
    /// The source as a person names it.
    fn name(&self, locations: &Locations) -> SourceName;
    /// Whether the login has an entry the chain can use; reads no value into the result.
    fn presence(&self, locations: &Locations) -> Presence;
    /// The login as a source of the chain.
    fn entry(&self, locations: &Locations, transport: Arc<dyn Transport>) -> Box<dyn Entry>;
}

/// `api-key`: an API key from the documented environment variable, p1's store, then the
/// keyed logins the table borrows from (`borrow = ["<store>:<key>"]`).
pub struct ApiKeyMethod;

impl CredentialMethod for ApiKeyMethod {
    fn name(&self) -> &str {
        "api-key"
    }

    fn owner(&self) -> &str {
        "the documented key environment variable"
    }

    fn store(self: Arc<Self>) -> StoreForm {
        StoreForm::ApiKey
    }

    /// An API key is documented by the environment variable that holds it.
    fn validate(&self, spec: &CredentialSpec) -> Result<(), String> {
        if spec.env.is_none() {
            return Err(
                "`[credential]` kind \"api-key\" names no environment variable; write \
                 `env = \"…\"` with the documented variable for this route"
                    .into(),
            );
        }
        Ok(())
    }

    fn borrowed(self: Arc<Self>, spec: &CredentialSpec) -> Vec<Box<dyn BorrowedLogin>> {
        spec.borrow
            .iter()
            .map(|borrow| {
                Box::new(crate::login_file::BorrowedKey::new(
                    borrow.store.clone(),
                    borrow.key.clone(),
                )) as Box<dyn BorrowedLogin>
            })
            .collect()
    }
}

/// `none`: no credential at all; an egress proxy injects the provider's credential after the
/// request leaves the process (issue #134). Nothing is read and nothing is refreshed.
pub struct NoneMethod;

impl CredentialMethod for NoneMethod {
    fn name(&self) -> &str {
        "none"
    }

    fn label(&self) -> &str {
        "none (proxy-injected)"
    }

    fn owner(&self) -> &str {
        "the egress proxy that injects it"
    }

    fn store(self: Arc<Self>) -> StoreForm {
        StoreForm::Nothing
    }

    /// A route that sends no credential names no source at all.
    fn validate(&self, spec: &CredentialSpec) -> Result<(), String> {
        if spec.env.is_some() {
            return Err(
                "`[credential]` kind \"none\" also names an environment variable; a route \
                 that sends no credential reads no variable — delete the `env` line"
                    .into(),
            );
        }
        if !spec.borrow.is_empty() {
            return Err(
                "`[credential]` kind \"none\" also lists `borrow` entries; a route that \
                 sends no credential reads no borrowed login"
                    .into(),
            );
        }
        if spec.store_only {
            return Err(
                "`[credential]` kind \"none\" also sets `store_only`; a route that sends no \
                 credential reads no store — delete the `store_only` line"
                    .into(),
            );
        }
        Ok(())
    }
}
