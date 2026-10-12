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
//! p1's core has two methods, [`ApiKeyMethod`] and [`NoneMethod`]. The subscription connectors add the
//! OAuth logins they borrow (`claude-code-oauth`, `codex-oauth`) as implementations of this
//! port, in a build with the `subscriptions` feature; [`methods`] is the one place the
//! implementations a build has are listed.

use std::path::PathBuf;
use std::sync::Arc;

use p1_provider_http::{HttpRequest, Transport};
use serde_json::Value;

use crate::locations::Locations;
use crate::resolve::{Entry, Presence, SourceName};
use crate::spec::CredentialSpec;

/// One credential method (ADR-0211).
pub trait CredentialMethod: Send + Sync + 'static {
    /// The spelling a route or account file uses for it.
    fn name(&self) -> &'static str;

    /// The method as `p1 login --list` shows it.
    fn label(&self) -> &'static str {
        self.name()
    }

    /// Who mints a credential of this method, for `p1 login`'s explanations.
    fn owner(&self) -> &'static str;

    /// What p1's own store keeps for a route of this method.
    fn store(&self) -> StoreForm;

    /// The checks of a `[credential]` table naming this method, after the shared ones of
    /// [`CredentialSpec::validate`].
    fn validate(&self, _spec: &CredentialSpec) -> Result<(), String> {
        Ok(())
    }

    /// Whether a route of this method may name the `login_dir` it borrows (ADR-0074); the
    /// method checks the directory in [`CredentialMethod::validate`].
    fn takes_login_dir(&self) -> bool {
        false
    }

    /// The other tools' logins a route of this method reads after p1's store, in order.
    /// Never asked for a store-only route, whose chain ends at the store (ADR-0061).
    fn borrowed(&self, _spec: &CredentialSpec) -> Vec<Box<dyn BorrowedLogin>> {
        Vec::new()
    }

    /// The credential files a route of this method reads beyond the fixed ones every file
    /// tool refuses (`p1_workspace::CredentialPolicy`): a login directory the route names.
    fn owned_files(&self, _spec: &CredentialSpec, _locations: &Locations) -> Vec<PathBuf> {
        Vec::new()
    }

    /// The usage probe a route of this method answers when its account names none
    /// (`p1-usage`, ADR-0139), by the probe's name.
    fn usage_probe(&self) -> Option<&'static str> {
        None
    }
}

/// What p1's own store keeps for a method's routes (spec §3).
#[derive(Clone, Copy)]
pub enum StoreForm {
    /// Nothing: the route sends no credential.
    Nothing,
    /// A pasted API key, which `p1 login` writes.
    ApiKey,
    /// OAuth tokens, refreshed through the method's token endpoint.
    Oauth(&'static dyn OauthRefresh),
}

impl StoreForm {
    /// Whether the store keeps an API key.
    pub fn is_api_key(self) -> bool {
        matches!(self, StoreForm::ApiKey)
    }

    /// Whether the store keeps OAuth tokens.
    pub fn is_oauth(self) -> bool {
        matches!(self, StoreForm::Oauth(_))
    }
}

/// How an OAuth method's tokens are refreshed: the request to its token endpoint and what
/// its answer holds. Nothing here names a value in an error.
pub trait OauthRefresh: Send + Sync + 'static {
    /// The refresh request for `refresh_token`.
    fn request(&self, refresh_token: &str) -> HttpRequest;
    /// The rotated tokens of a refresh response, each checked on its own.
    fn parse(&self, response: &Value) -> RefreshedTokens;
}

/// A refresh response, each field checked on its own: a rotated refresh token is kept even
/// when the rest of the response cannot be used, so the login survives.
pub struct RefreshedTokens {
    /// The new access token, or why it cannot be used.
    pub access: Result<String, &'static str>,
    /// The rotated refresh token; `None` when the server rotates none.
    pub refresh: Option<String>,
    /// The granted lifetime in milliseconds, or why it cannot be used.
    pub lifetime_ms: Result<u64, &'static str>,
}

impl RefreshedTokens {
    /// The tokens of a standard OAuth 2 token response (`access_token`, `refresh_token`,
    /// `expires_in` in seconds), each checked on its own.
    pub fn from_token_response(value: &Value) -> Self {
        let text = |field: &str| {
            value
                .get(field)
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|text| !text.is_empty())
                .map(str::to_string)
        };
        let access = match text("access_token") {
            None => Err("the p1 store token refresh response is missing an access token"),
            Some(access) if !crate::usable_key(&access) => Err(
                "the p1 store token refresh response holds an access token that is not a \
                 header-safe token",
            ),
            Some(access) => Ok(access),
        };
        let lifetime_ms = match value.get("expires_in").and_then(Value::as_u64) {
            None => Err("the p1 store token refresh response is missing the token lifetime"),
            Some(seconds) => crate::refresh_http::lifetime_ms(seconds).ok_or(
                "the p1 store token refresh response granted a token lifetime too short to use",
            ),
        };
        Self {
            access,
            refresh: text("refresh_token"),
            lifetime_ms,
        }
    }
}

/// Another tool's login a method reads after p1's store (ADR-0040: read-only reuse, a
/// rotation written back to the same file).
pub trait BorrowedLogin: Send + Sync {
    /// The source as a person names it.
    fn name(&self) -> SourceName;
    /// Whether the login has an entry the chain can use; reads no value into the result.
    fn presence(&self, locations: &Locations) -> Presence;
    /// The login as a source of the chain.
    fn entry(&self, locations: &Locations, transport: Arc<dyn Transport>) -> Box<dyn Entry>;
}

/// `api-key`: an API key from the documented environment variable, p1's store, then the
/// borrowed logins the table lists.
pub struct ApiKeyMethod;

impl CredentialMethod for ApiKeyMethod {
    fn name(&self) -> &'static str {
        "api-key"
    }

    fn owner(&self) -> &'static str {
        "the documented key environment variable"
    }

    fn store(&self) -> StoreForm {
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

    fn borrowed(&self, spec: &CredentialSpec) -> Vec<Box<dyn BorrowedLogin>> {
        // Without the connectors there is no borrow store, so `borrow` is empty.
        #[cfg(feature = "subscriptions")]
        return crate::subscriptions::borrowed_keys(spec);
        #[cfg(not(feature = "subscriptions"))]
        {
            let _ = spec;
            Vec::new()
        }
    }
}

/// `none`: no credential at all; an egress proxy injects the provider's credential after the
/// request leaves the process (issue #134). Nothing is read and nothing is refreshed.
pub struct NoneMethod;

impl CredentialMethod for NoneMethod {
    fn name(&self) -> &'static str {
        "none"
    }

    fn label(&self) -> &'static str {
        "none (proxy-injected)"
    }

    fn owner(&self) -> &'static str {
        "the egress proxy that injects it"
    }

    fn store(&self) -> StoreForm {
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

/// Every credential method this build has, in the order `p1 login` lists them: the core's,
/// then the subscription connectors' when the build has them. The one place the
/// implementations are listed; nothing registers itself.
pub fn methods() -> Vec<&'static dyn CredentialMethod> {
    let mut methods: Vec<&'static dyn CredentialMethod> = vec![&ApiKeyMethod];
    #[cfg(feature = "subscriptions")]
    methods.extend(crate::subscriptions::methods());
    methods.push(&NoneMethod);
    methods
}

/// The method spelled `name`, if this build has it.
pub fn method(name: &str) -> Option<&'static dyn CredentialMethod> {
    methods().into_iter().find(|method| method.name() == name)
}
