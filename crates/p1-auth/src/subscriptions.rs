//! The subscription connectors' credential methods (ADR-0200, ADR-0211): the OAuth logins of
//! the Claude Code and Codex CLIs and the API keys the OpenCode and Pi CLIs keep, as
//! implementations of the credential-method port ([`crate::method`]). Only a build with the
//! `subscriptions` feature has them; [`methods`] is what [`crate::methods`] adds for them.

use std::path::PathBuf;
use std::sync::Arc;

use p1_contracts::{BoxFuture, ProviderError};
use p1_provider_http::{Credential, HttpRequest, Transport};
use serde_json::{Value, json};

use crate::api_key::SubscriptionCredentials;
use crate::auth;
use crate::claude_code::{ClaudeCodeCredentials, DEFAULT_SCOPES, OAUTH_BETA};
use crate::codex::{CodexCliCredentials, percent_encode};
use crate::locations::Locations;
use crate::method::{BorrowedLogin, CredentialMethod, OauthRefresh, RefreshedTokens, StoreForm};
use crate::resolve::{Entry, Presence, SourceName};
use crate::spec::{BorrowStore, CredentialKind, CredentialSpec};

/// The Claude Code CLI's credentials file as a source.
pub const CLAUDE_CODE_LOGIN: SourceName = SourceName::Login {
    name: "Claude Code login",
    guidance: "run Claude Code login",
};
/// The Codex CLI's auth file as a source.
pub const CODEX_LOGIN: SourceName = SourceName::Login {
    name: "Codex login",
    guidance: "run `codex login`",
};
/// The OpenCode CLI's login file as a source.
pub const OPENCODE_LOGIN: SourceName = SourceName::Login {
    name: "opencode login",
    guidance: "log in with opencode",
};
/// The Pi CLI's login file as a source.
pub const PI_LOGIN: SourceName = SourceName::Login {
    name: "pi login",
    guidance: "log in with pi",
};

/// The `claude-code-oauth` method.
pub const CLAUDE_CODE_OAUTH: CredentialKind = CredentialKind::of(&ClaudeCodeOauth);
/// The `codex-oauth` method.
pub const CODEX_OAUTH: CredentialKind = CredentialKind::of(&CodexOauth);

/// The connectors' methods, in the order `p1 login` lists them.
pub(crate) fn methods() -> Vec<&'static dyn CredentialMethod> {
    vec![&ClaudeCodeOauth, &CodexOauth]
}

/// `claude-code-oauth`: the Claude Code CLI login — the environment variable (a bearer token),
/// p1's store, then Claude Code's own credentials file in the route's `login_dir` or the default
/// directory (ADR-0074).
pub struct ClaudeCodeOauth;

impl CredentialMethod for ClaudeCodeOauth {
    fn name(&self) -> &'static str {
        "claude-code-oauth"
    }

    fn owner(&self) -> &'static str {
        "the Claude Code CLI (`claude`)"
    }

    fn store(&self) -> StoreForm {
        StoreForm::Oauth(&CLAUDE_CODE_REFRESH)
    }

    fn takes_login_dir(&self) -> bool {
        true
    }

    /// A `login_dir` is spelled plainly: an absolute directory or one below the home.
    fn validate(&self, spec: &CredentialSpec) -> Result<(), String> {
        let Some(dir) = &spec.login_dir else {
            return Ok(());
        };
        // The home directory itself (`~`, `~/`) is not a Claude Code config directory.
        let Some(rest) = dir.strip_prefix("~/").or_else(|| dir.strip_prefix('/')) else {
            return Err(format!(
                "`[credential]` login_dir \"{dir}\" is neither an absolute path nor a \
                 directory below the home (`~/<dir>`)"
            ));
        };
        // Spelled plainly: no `.` or `..` component can walk it back to the home, out of it,
        // or to the root.
        let components: Vec<&str> = rest.split('/').filter(|part| !part.is_empty()).collect();
        if components.iter().any(|part| *part == "." || *part == "..") {
            return Err(format!(
                "`[credential]` login_dir \"{dir}\" contains a `.` or `..` component; write \
                 the directory plainly"
            ));
        }
        if components.is_empty() {
            return Err(format!(
                "`[credential]` login_dir \"{dir}\" names the home or the root directory; \
                 write an absolute Claude Code config directory or one below the home"
            ));
        }
        Ok(())
    }

    fn borrowed(&self, spec: &CredentialSpec) -> Vec<Box<dyn BorrowedLogin>> {
        // The route's own login directory when it names one (ADR-0074).
        vec![Box::new(ClaudeCodeLogin(spec.login_dir.clone()))]
    }

    /// The credentials file of the login directory the route names; the default directory's
    /// is one of the fixed files every file tool refuses.
    fn owned_files(&self, spec: &CredentialSpec, locations: &Locations) -> Vec<PathBuf> {
        spec.login_dir
            .as_deref()
            .and_then(|dir| locations.claude_code_path(Some(dir)))
            .into_iter()
            .collect()
    }

    fn usage_probe(&self) -> Option<&'static str> {
        Some("claude")
    }
}

/// `codex-oauth`: the Codex CLI login — the environment variable (a bearer token), p1's
/// store, then the Codex CLI's own auth file.
pub struct CodexOauth;

impl CredentialMethod for CodexOauth {
    fn name(&self) -> &'static str {
        "codex-oauth"
    }

    fn owner(&self) -> &'static str {
        "the Codex CLI (`codex login`)"
    }

    fn store(&self) -> StoreForm {
        StoreForm::Oauth(&CODEX_REFRESH)
    }

    fn borrowed(&self, _spec: &CredentialSpec) -> Vec<Box<dyn BorrowedLogin>> {
        vec![Box::new(CodexLogin)]
    }

    fn usage_probe(&self) -> Option<&'static str> {
        Some("codex")
    }
}

/// The borrowed logins an `api-key` table lists (`borrow = ["opencode:go", …]`).
pub(crate) fn borrowed_keys(spec: &CredentialSpec) -> Vec<Box<dyn BorrowedLogin>> {
    spec.borrow
        .iter()
        .map(|borrow| {
            Box::new(KeyLogin {
                store: borrow.store,
                key: borrow.key.clone(),
            }) as Box<dyn BorrowedLogin>
        })
        .collect()
}

// ------------------------------------------------------------------ token refresh

/// The refresh of a stored Claude Code login, byte for byte what the borrowed login sends.
pub(crate) struct ClaudeCodeRefresh;
/// The refresh of a stored Codex login, byte for byte what the borrowed login sends.
pub(crate) struct CodexRefresh;

pub(crate) static CLAUDE_CODE_REFRESH: ClaudeCodeRefresh = ClaudeCodeRefresh;
pub(crate) static CODEX_REFRESH: CodexRefresh = CodexRefresh;

impl OauthRefresh for ClaudeCodeRefresh {
    fn request(&self, refresh_token: &str) -> HttpRequest {
        HttpRequest {
            url: crate::claude_code::TOKEN_URL.to_string(),
            headers: vec![
                ("content-type".to_string(), "application/json".to_string()),
                ("anthropic-beta".to_string(), OAUTH_BETA.to_string()),
            ],
            body: serde_json::to_vec(&json!({
                "grant_type": "refresh_token",
                "refresh_token": refresh_token,
                "client_id": crate::claude_code::CLIENT_ID,
                "scope": DEFAULT_SCOPES,
            }))
            .unwrap_or_default(),
        }
    }

    fn parse(&self, response: &Value) -> RefreshedTokens {
        RefreshedTokens::from_token_response(response)
    }
}

impl OauthRefresh for CodexRefresh {
    fn request(&self, refresh_token: &str) -> HttpRequest {
        HttpRequest {
            url: crate::codex::TOKEN_URL.to_string(),
            headers: vec![
                (
                    "Content-Type".to_string(),
                    "application/x-www-form-urlencoded".to_string(),
                ),
                ("Accept".to_string(), "application/json".to_string()),
            ],
            body: format!(
                "grant_type=refresh_token&refresh_token={}&client_id={}",
                percent_encode(refresh_token),
                percent_encode(crate::codex::CLIENT_ID),
            )
            .into_bytes(),
        }
    }

    fn parse(&self, response: &Value) -> RefreshedTokens {
        RefreshedTokens::from_token_response(response)
    }
}

// ------------------------------------------------------------------ borrowed logins

/// Claude Code's login, in the route's `login_dir` or the default directory.
struct ClaudeCodeLogin(Option<String>);

impl BorrowedLogin for ClaudeCodeLogin {
    fn name(&self) -> SourceName {
        CLAUDE_CODE_LOGIN
    }

    fn presence(&self, locations: &Locations) -> Presence {
        match locations.claude_code_path(self.0.as_deref()) {
            None => Presence::Absent,
            Some(path) => crate::claude_code::presence_at(&path),
        }
    }

    fn entry(&self, locations: &Locations, transport: Arc<dyn Transport>) -> Box<dyn Entry> {
        match locations.claude_code_path(self.0.as_deref()) {
            Some(path) => Box::new(ClaudeCodeCredentials::at(path, transport)),
            None => missing(CLAUDE_CODE_LOGIN),
        }
    }
}

/// The Codex CLI's login.
struct CodexLogin;

impl BorrowedLogin for CodexLogin {
    fn name(&self) -> SourceName {
        CODEX_LOGIN
    }

    fn presence(&self, locations: &Locations) -> Presence {
        match locations.codex_path() {
            None => Presence::Absent,
            Some(path) => crate::codex::presence_at(&path),
        }
    }

    fn entry(&self, locations: &Locations, transport: Arc<dyn Transport>) -> Box<dyn Entry> {
        match locations.codex_path() {
            Some(path) => Box::new(CodexCliCredentials::at(path, transport)),
            None => missing(CODEX_LOGIN),
        }
    }
}

/// An API key another CLI keeps, by its store and the provider key inside its file.
struct KeyLogin {
    store: BorrowStore,
    key: String,
}

impl KeyLogin {
    fn path(&self, locations: &Locations) -> Option<PathBuf> {
        match self.store {
            BorrowStore::Opencode => locations.opencode_login_path(),
            BorrowStore::Pi => locations.pi_login_path(),
        }
    }
}

impl BorrowedLogin for KeyLogin {
    fn name(&self) -> SourceName {
        match self.store {
            BorrowStore::Opencode => OPENCODE_LOGIN,
            BorrowStore::Pi => PI_LOGIN,
        }
    }

    fn presence(&self, locations: &Locations) -> Presence {
        match self.path(locations) {
            None => Presence::Absent,
            Some(path) => crate::api_key::presence_at(&path, &self.key, self.store),
        }
    }

    fn entry(&self, locations: &Locations, _transport: Arc<dyn Transport>) -> Box<dyn Entry> {
        match self.path(locations) {
            Some(path) => Box::new(SubscriptionCredentials::at(path, &self.key, self.store)),
            None => missing(self.name()),
        }
    }
}

/// The entry of a borrowed source the host has no location for.
fn missing(name: SourceName) -> Box<dyn Entry> {
    Box::new(Missing { name })
}

/// A source the host has no location for (no home, no such directory): it can never have an
/// entry, so it is skipped rather than guessed at.
struct Missing {
    name: SourceName,
}

impl Entry for Missing {
    fn name(&self) -> SourceName {
        self.name.clone()
    }

    fn presence(&self) -> Presence {
        Presence::Absent
    }

    fn current<'a>(&'a self) -> BoxFuture<'a, Result<Credential, ProviderError>> {
        Box::pin(async move {
            Err(auth(format!(
                "{} has no location on this system; set HOME or the variable that names it",
                self.name
            )))
        })
    }

    fn rotated<'a>(
        &'a self,
        _rejected: &'a Credential,
    ) -> BoxFuture<'a, Result<Credential, ProviderError>> {
        self.current()
    }
}
