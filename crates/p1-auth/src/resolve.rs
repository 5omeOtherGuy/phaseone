//! The chain (spec §3) and the "which source" report (spec §4).
//!
//! One route's credential is read from the first source that HAS an entry: the
//! documented environment variable, then p1's store, then the borrowed logins. A
//! source that has an entry which is unusable is an error naming it, never a silent
//! fall-through to the next one. Access is LAZY and re-evaluated on every call, so a
//! key rotated in a file, or a variable that appears, is picked up without a
//! restart.

use std::sync::Arc;

use p1_contracts::{BoxFuture, ProviderError};
use p1_provider_http::{Credential, CredentialSource, Transport};

use crate::api_key::{SubscriptionCredentials, usable_key};
use crate::claude_code::ClaudeCodeCredentials;
use crate::codex::CodexCliCredentials;
use crate::locations::Locations;
use crate::store::{OauthDialect, StoreApiKey, StoreOauth};
use crate::{BorrowStore, CredentialKind, CredentialSpec, auth};

/// The place a credential was read from, as a person names it. Holds no value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SourceName {
    /// The documented environment variable of the route, by NAME.
    Env(String),
    /// p1's own store.
    P1Store,
    /// The OpenCode CLI's login file.
    OpencodeLogin,
    /// The Pi CLI's login file.
    PiLogin,
    /// The Claude Code CLI's credentials file.
    ClaudeCodeLogin,
    /// The Codex CLI's auth file.
    CodexLogin,
}

impl std::fmt::Display for SourceName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SourceName::Env(name) => write!(f, "env {name}"),
            SourceName::P1Store => f.write_str("p1 store"),
            SourceName::OpencodeLogin => f.write_str("opencode login"),
            SourceName::PiLogin => f.write_str("pi login"),
            SourceName::ClaudeCodeLogin => f.write_str("Claude Code login"),
            SourceName::CodexLogin => f.write_str("Codex login"),
        }
    }
}

/// The credential policy a route's `[credential]` table declares (spec §2, ADR-0061).
/// A route that does not write the field keeps the legacy chain.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CredentialPolicy {
    /// Every source the kind has: the documented environment variable, p1's store,
    /// then another tool's login where the kind has one.
    Chain,
    /// The documented environment variable and p1's OWN store only. No other tool's
    /// login file is opened, whatever the outcome — absent, unusable or rejected.
    StoreOnly,
    /// No source at all: the route sends no credential, and an egress proxy injects
    /// the provider's credential after the request leaves the process (issue #134).
    /// Nothing is read, and there is nothing to refresh.
    ProxyInjected,
}

impl CredentialPolicy {
    /// The marker appended to the source line, so `p1 env show` and `p1 login --list`
    /// make the policy visible. Empty for the legacy chain, which is the default —
    /// and for `ProxyInjected`, whose line already says what it is.
    pub fn marker(self) -> &'static str {
        match self {
            CredentialPolicy::Chain => "",
            CredentialPolicy::StoreOnly => " [p1 store only]",
            CredentialPolicy::ProxyInjected => "",
        }
    }

    /// The policy name on its own, without the display spacing.
    pub fn name(self) -> &'static str {
        match self {
            CredentialPolicy::Chain => "chain",
            CredentialPolicy::StoreOnly => "store-only",
            CredentialPolicy::ProxyInjected => "proxy-injected",
        }
    }
}

impl std::fmt::Display for CredentialPolicy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.name())
    }
}

/// What a source looks like from the outside: an entry, no entry, or an entry that
/// cannot be used. The reason names no value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Presence {
    Present,
    Absent,
    Unusable(String),
}

impl std::fmt::Display for Presence {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Presence::Present => f.write_str("present"),
            Presence::Absent => f.write_str("absent"),
            Presence::Unusable(reason) => write!(f, "unusable({reason})"),
        }
    }
}

/// Which source a route's credential would come from, and every source tried. No
/// field can hold a credential value: the only strings are a variable NAME and a
/// reason this crate wrote.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceReport {
    /// The source the first usable entry lives in, or `None` when none is usable.
    pub chosen: Option<SourceName>,
    /// Every source of the chain, in the order it is tried.
    pub tried: Vec<(SourceName, Presence)>,
    /// The policy the route declared. `StoreOnly` means no other tool's login is in
    /// the chain at all, so `tried` can never name one (spec §4, ADR-0061).
    pub policy: CredentialPolicy,
}

impl SourceReport {
    /// The one line `p1 env show` prints (spec §4): the chosen source, or what to
    /// do when no source has an entry. A store-only route appends the policy marker
    /// so the operator sees that no other tool's login is tried; a proxy-injected
    /// route says that p1 sends nothing and the egress proxy supplies the credential.
    pub fn line(&self) -> String {
        if self.policy == CredentialPolicy::ProxyInjected {
            return "none (proxy-injected) — the egress proxy injects the credential".to_string();
        }
        let base = match &self.chosen {
            Some(name) => name.to_string(),
            None => match self.tried.iter().find_map(|(_, presence)| match presence {
                Presence::Unusable(reason) => Some(reason.clone()),
                _ => None,
            }) {
                Some(reason) => format!("none — {reason}"),
                None => format!("none — {}", guidance(&self.tried)),
            },
        };
        format!("{base}{}", self.policy.marker())
    }
}

/// What to do when no source has an entry, built from the sources the chain has.
fn guidance(tried: &[(SourceName, Presence)]) -> String {
    let mut steps: Vec<String> = Vec::new();
    for (name, _) in tried {
        let step = match name {
            SourceName::Env(name) => format!("set {name}"),
            SourceName::P1Store => "add an entry to the p1 store".to_string(),
            SourceName::OpencodeLogin => "log in with opencode".to_string(),
            SourceName::PiLogin => "log in with pi".to_string(),
            SourceName::ClaudeCodeLogin => "run Claude Code login".to_string(),
            SourceName::CodexLogin => "run `codex login`".to_string(),
        };
        if !steps.contains(&step) {
            steps.push(step);
        }
    }
    if steps.is_empty() {
        "no credential source is configured for this route".to_string()
    } else {
        steps.join(", or ")
    }
}

/// One source of the chain, as a `CredentialSource` can be built from it.
pub(crate) trait Entry: Send + Sync {
    fn name(&self) -> SourceName;
    fn presence(&self) -> Presence;
    /// The credential this source currently holds.
    fn current<'a>(&'a self) -> BoxFuture<'a, Result<Credential, ProviderError>>;
    /// Called after a 401/403 with the credential that was rejected.
    fn rotated<'a>(
        &'a self,
        rejected: &'a Credential,
    ) -> BoxFuture<'a, Result<Credential, ProviderError>>;
}

/// The credential source a route file's `[credential]` table describes (spec §2).
pub fn resolve(
    route_id: &str,
    spec: &CredentialSpec,
    transport: Arc<dyn Transport>,
    locations: &Locations,
) -> Arc<dyn CredentialSource> {
    // A `none` route has no chain to build: it reads nothing (issue #134).
    if spec.kind == CredentialKind::None {
        return Arc::new(ProxyInjected {
            route_id: route_id.to_string(),
        });
    }
    let entries = sources(spec)
        .into_iter()
        .map(|source| source.entry(route_id, transport.clone(), locations))
        .collect();
    Arc::new(Resolved { entries })
}

/// Which source a route would read from, without reading a value (spec §4).
pub fn describe(route_id: &str, spec: &CredentialSpec, locations: &Locations) -> SourceReport {
    let mut chosen = None;
    let mut decided = false;
    let mut tried = Vec::new();
    for source in sources(spec) {
        let name = source.name();
        let presence = source.presence(route_id, locations);
        if !decided && presence != Presence::Absent {
            decided = true;
            // An unusable first source is an error, so it is NOT a chosen source.
            if presence == Presence::Present {
                chosen = Some(name.clone());
            }
        }
        tried.push((name, presence));
    }
    SourceReport {
        chosen,
        tried,
        policy: policy(spec),
    }
}

/// The policy a route's `[credential]` table declares (spec §2, ADR-0061).
fn policy(spec: &CredentialSpec) -> CredentialPolicy {
    if spec.kind == CredentialKind::None {
        CredentialPolicy::ProxyInjected
    } else if spec.store_only {
        CredentialPolicy::StoreOnly
    } else {
        CredentialPolicy::Chain
    }
}

/// The sources one route's credential is tried from, in order (spec §2). A store-only
/// route stops after its own store: the borrowed login is not merely skipped when
/// absent, it is never constructed, so no other tool's file is opened at all. A
/// `none` route has NO source: an egress proxy injects the credential (issue #134).
fn sources(spec: &CredentialSpec) -> Vec<Source> {
    let mut sources = Vec::new();
    if spec.kind == CredentialKind::None {
        return sources;
    }
    if let Some(env) = &spec.env {
        sources.push(Source::Env(env.clone()));
    }
    match spec.kind {
        CredentialKind::ApiKey => {
            sources.push(Source::P1StoreApiKey);
            if !spec.store_only {
                for borrow in &spec.borrow {
                    sources.push(Source::Login {
                        store: borrow.store,
                        key: borrow.key.clone(),
                    });
                }
            }
        }
        CredentialKind::ClaudeCodeOauth => {
            sources.push(Source::P1StoreOauth(OauthDialect::ClaudeCode));
            if !spec.store_only {
                // The route's own login directory when it names one (ADR-0075).
                sources.push(Source::ClaudeCodeLogin(spec.login_dir.clone()));
            }
        }
        CredentialKind::CodexOauth => {
            sources.push(Source::P1StoreOauth(OauthDialect::Codex));
            if !spec.store_only {
                sources.push(Source::CodexLogin);
            }
        }
        CredentialKind::None => {}
    }
    sources
}

enum Source {
    Env(String),
    P1StoreApiKey,
    P1StoreOauth(OauthDialect),
    Login {
        store: BorrowStore,
        key: String,
    },
    /// Claude Code's login, in the route's `login_dir` or the default directory.
    ClaudeCodeLogin(Option<String>),
    CodexLogin,
}

impl Source {
    fn name(&self) -> SourceName {
        match self {
            Source::Env(name) => SourceName::Env(name.clone()),
            Source::P1StoreApiKey | Source::P1StoreOauth(_) => SourceName::P1Store,
            Source::Login { store, .. } => match store {
                BorrowStore::Opencode => SourceName::OpencodeLogin,
                BorrowStore::Pi => SourceName::PiLogin,
            },
            Source::ClaudeCodeLogin(_) => SourceName::ClaudeCodeLogin,
            Source::CodexLogin => SourceName::CodexLogin,
        }
    }

    fn presence(&self, route_id: &str, locations: &Locations) -> Presence {
        match self {
            Source::Env(name) => match locations.env(name) {
                None => Presence::Absent,
                Some(value) if usable_key(&value) => Presence::Present,
                Some(_) => Presence::Unusable(format!(
                    "the environment variable {name} holds no usable token (printable ASCII, no spaces)"
                )),
            },
            Source::P1StoreApiKey => {
                crate::store::presence(locations, route_id, CredentialKind::ApiKey)
            }
            Source::P1StoreOauth(dialect) => crate::store::presence(
                locations,
                route_id,
                match dialect {
                    OauthDialect::ClaudeCode => CredentialKind::ClaudeCodeOauth,
                    OauthDialect::Codex => CredentialKind::CodexOauth,
                },
            ),
            Source::Login { store, key } => match login_path(*store, locations) {
                None => Presence::Absent,
                Some(path) => crate::api_key::presence_at(&path, key, *store),
            },
            Source::ClaudeCodeLogin(dir) => match locations.claude_code_path(dir.as_deref()) {
                None => Presence::Absent,
                Some(path) => crate::claude_code::presence_at(&path),
            },
            Source::CodexLogin => match locations.codex_path() {
                None => Presence::Absent,
                Some(path) => crate::codex::presence_at(&path),
            },
        }
    }

    fn entry(
        self,
        route_id: &str,
        transport: Arc<dyn Transport>,
        locations: &Locations,
    ) -> Box<dyn Entry> {
        let missing = |name: SourceName| Box::new(Missing { name }) as Box<dyn Entry>;
        match self {
            Source::Env(name) => Box::new(EnvEntry {
                name,
                env: locations.lookup(),
            }),
            Source::P1StoreApiKey => Box::new(StoreApiKey::new(locations, route_id)),
            Source::P1StoreOauth(dialect) => {
                Box::new(StoreOauth::new(locations, route_id, dialect, transport))
            }
            Source::Login { store, key } => match login_path(store, locations) {
                Some(path) => Box::new(SubscriptionCredentials::at(path, &key, store)),
                None => missing(self_name(&store)),
            },
            Source::ClaudeCodeLogin(dir) => match locations.claude_code_path(dir.as_deref()) {
                Some(path) => Box::new(ClaudeCodeCredentials::at(path, transport)),
                None => missing(SourceName::ClaudeCodeLogin),
            },
            Source::CodexLogin => match locations.codex_path() {
                Some(path) => Box::new(CodexCliCredentials::at(path, transport)),
                None => missing(SourceName::CodexLogin),
            },
        }
    }
}

/// The source name of a borrow store.
fn self_name(store: &BorrowStore) -> SourceName {
    match store {
        BorrowStore::Opencode => SourceName::OpencodeLogin,
        BorrowStore::Pi => SourceName::PiLogin,
    }
}

/// The credential file one borrow store points at.
fn login_path(store: BorrowStore, locations: &Locations) -> Option<std::path::PathBuf> {
    match store {
        BorrowStore::Opencode => locations.opencode_login_path(),
        BorrowStore::Pi => locations.pi_login_path(),
    }
}

/// A source the host has no location for (no home, no such directory): it can
/// never have an entry, so it is skipped rather than guessed at.
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

/// The environment variable as a source: a bearer token, never refreshed.
struct EnvEntry {
    name: String,
    env: crate::locations::EnvLookup,
}

impl Entry for EnvEntry {
    fn name(&self) -> SourceName {
        SourceName::Env(self.name.clone())
    }

    fn presence(&self) -> Presence {
        match (self.env)(&self.name) {
            None => Presence::Absent,
            Some(value) if usable_key(&value) => Presence::Present,
            Some(_) => Presence::Unusable(format!(
                "the environment variable {} holds no usable token (printable ASCII, no spaces)",
                self.name
            )),
        }
    }

    fn current<'a>(&'a self) -> BoxFuture<'a, Result<Credential, ProviderError>> {
        Box::pin(async move {
            match (self.env)(&self.name) {
                Some(value) if usable_key(&value) => Ok(Credential {
                    bearer: value,
                    account_id: None,
                }),
                Some(_) => Err(auth(format!(
                    "the environment variable {} holds no usable token (printable ASCII, no spaces)",
                    self.name
                ))),
                None => Err(auth(format!(
                    "the environment variable {} is no longer set",
                    self.name
                ))),
            }
        })
    }

    fn rotated<'a>(
        &'a self,
        rejected: &'a Credential,
    ) -> BoxFuture<'a, Result<Credential, ProviderError>> {
        Box::pin(async move {
            match (self.env)(&self.name) {
                // A variable-backed credential is never refreshed: a new value is
                // the only way to replace it, and the message says so.
                Some(value) if value != rejected.bearer => Ok(Credential {
                    bearer: value,
                    account_id: None,
                }),
                Some(_) => Err(auth(format!(
                    "the environment variable {} was rejected and is never refreshed; set it to a \
                     new value",
                    self.name
                ))),
                None => Err(auth(format!(
                    "the environment variable {} is no longer set; set it again",
                    self.name
                ))),
            }
        })
    }
}

/// The resolved chain: the first source with an entry answers every call.
struct Resolved {
    entries: Vec<Box<dyn Entry>>,
}

impl Resolved {
    /// The source that answers this call, or the error that stops the chain.
    fn select(&self) -> Result<&dyn Entry, ProviderError> {
        for entry in &self.entries {
            match entry.presence() {
                Presence::Absent => continue,
                Presence::Present => return Ok(entry.as_ref()),
                Presence::Unusable(reason) => {
                    return Err(auth(format!("{} is unusable: {reason}", entry.name())));
                }
            }
        }
        let tried: Vec<(SourceName, Presence)> = self
            .entries
            .iter()
            .map(|entry| (entry.name(), entry.presence()))
            .collect();
        Err(auth(format!(
            "no credential source has an entry for this route: {}",
            guidance(&tried)
        )))
    }
}

impl CredentialSource for Resolved {
    fn access<'a>(&'a self) -> BoxFuture<'a, Result<Credential, ProviderError>> {
        Box::pin(async move { self.select()?.current().await })
    }

    fn refresh<'a>(
        &'a self,
        rejected: &'a Credential,
    ) -> BoxFuture<'a, Result<Credential, ProviderError>> {
        Box::pin(async move { self.select()?.rotated(rejected).await })
    }
}

/// A route that sends NO credential: an egress proxy injects the provider's
/// credential after the request leaves the process (issue #134). It reads nothing —
/// no environment variable, no store entry, no other tool's login — and it has
/// nothing to refresh.
///
/// [`CredentialSource::access`] answers with a placeholder whose `bearer` is EMPTY:
/// the driver needs a credential to build a request, and the adapter must send no
/// authentication header for it. That is what
/// [`CredentialSource::proxy_injected`] tells every adapter, and the driver never
/// calls [`CredentialSource::refresh`] on such a route; a direct caller that does
/// still gets the refusal below, never a value.
struct ProxyInjected {
    /// Named in the refusal, so the operator knows which route to fix.
    route_id: String,
}

impl ProxyInjected {
    /// The refusal a rejected request on this route reports. The message names the
    /// missing proxy credential and never a key p1 could hold.
    fn refusal(&self) -> ProviderError {
        auth(format!(
            "route \"{}\" sends no credential (`kind = \"none\"`): the egress proxy must inject \
             the proxy credential, and p1 has none to refresh",
            self.route_id
        ))
    }
}

impl CredentialSource for ProxyInjected {
    fn access<'a>(&'a self) -> BoxFuture<'a, Result<Credential, ProviderError>> {
        // Nothing is read: this placeholder exists so the driver can build a request
        // with no authentication header at all.
        Box::pin(async move {
            Ok(Credential {
                bearer: String::new(),
                account_id: None,
            })
        })
    }

    fn refresh<'a>(
        &'a self,
        _rejected: &'a Credential,
    ) -> BoxFuture<'a, Result<Credential, ProviderError>> {
        Box::pin(async move { Err(self.refusal()) })
    }

    fn proxy_injected(&self) -> bool {
        true
    }
}
