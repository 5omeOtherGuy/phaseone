//! Credential modules (ADR-0212): a credential method or a borrowed login implemented by a
//! component of the `credential-method` class, and the composition that lists the installed
//! ones.
//!
//! A module is five pure functions over its own login file and its own refresh exchange
//! ([`CredentialModule`]); it imports nothing. Everything else stays here, native and the same
//! for every module: the pinned directory a login file is read and written through, the lock,
//! the bounded exchange, the time and what a token is worth ([`crate::login_file`]), and the
//! check that a refresh request goes over HTTPS to an origin the module's verified manifest
//! declares ([`ModuleMethod`]'s [`OauthRefresh`]).
//!
//! Least exposure: a module is called only for the route whose `[credential]` selects it, with
//! that route's login file and nothing else. It never sees an API access token on the request
//! path: the provider's transport broker attaches the token natively.
//!
//! [`CredentialModules`] is the composition: the root lists what is installed (p1's release and
//! the lock's entries) and hands it to [`crate::Locations::with_modules`]; nothing registers
//! itself. [`Methods`] resolves a route's method name through it, once per name.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};

use p1_provider_http::HttpRequest;
use serde::Deserialize;

use crate::locations::Locations;
use crate::method::{
    ApiKeyMethod, BorrowedLogin, CredentialMethod, NoneMethod, OauthRefresh, RefreshedTokens,
    StoreForm,
};
use crate::spec::{CredentialKind, CredentialSpec};

/// One component of the `credential-method` class: the world's five exports as the host
/// calls them, answers as the JSON texts the component wrote (`docs/design/modules/
/// credential.md`), and the token origins its verified manifest declares.
pub trait CredentialModule: Send + Sync {
    /// What the module provides.
    fn describe(&self) -> Result<String, String>;
    /// The credential `file` holds, or why it holds none usable.
    fn read_login(&self, file: &[u8], key: Option<&str>) -> Result<String, String>;
    /// The refresh request that spends `refresh_token`.
    fn refresh_request(&self, refresh_token: &str, file: Option<&[u8]>) -> Result<String, String>;
    /// What a refresh response grants.
    fn refresh_response(&self, body: &[u8], sent_at_ms: u64) -> Result<String, String>;
    /// The login file with a refresh response written into it.
    fn merge_login(
        &self,
        file: &[u8],
        body: &[u8],
        sent_at_ms: u64,
        now_ms: u64,
        keep_access: bool,
    ) -> Result<Vec<u8>, String>;
    /// The https origins the verified manifest lets the refresh requests go to.
    fn token_origins(&self) -> &[String];
}

/// The installed credential modules, as the composition root lists them.
pub trait CredentialModules: Send + Sync {
    /// The names a route's `kind` or `borrow` may give an installed module, in listing order.
    fn names(&self) -> Vec<String>;
    /// The module installed under `name`, loaded and verified; `Ok(None)` when none is.
    fn module(&self, name: &str) -> Result<Option<Arc<dyn CredentialModule>>, String>;
}

/// No credential module at all: the composition of a lookup that has none.
pub struct NoModules;

impl CredentialModules for NoModules {
    fn names(&self) -> Vec<String> {
        Vec::new()
    }

    fn module(&self, _name: &str) -> Result<Option<Arc<dyn CredentialModule>>, String> {
        Ok(None)
    }
}

/// The credential methods one lookup has: the core's two and the installed modules', each
/// module described once and kept.
pub struct Methods {
    modules: Arc<dyn CredentialModules>,
    described: Mutex<HashMap<String, Result<Arc<ModuleMethod>, String>>>,
}

impl Methods {
    /// The methods of `modules` beside the core's.
    pub fn new(modules: Arc<dyn CredentialModules>) -> Self {
        Self {
            modules,
            described: Mutex::new(HashMap::new()),
        }
    }

    /// The module installed under `name`, described, or why it cannot be used. `Ok(None)` when
    /// no module is installed under that name.
    pub fn module(&self, name: &str) -> Result<Option<Arc<ModuleMethod>>, String> {
        let mut described = self
            .described
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if let Some(known) = described.get(name) {
            return known.clone().map(Some);
        }
        let Some(module) = self.modules.module(name)? else {
            return Ok(None);
        };
        let method = ModuleMethod::new(name, module).map(Arc::new);
        described.insert(name.to_owned(), method.clone());
        method.map(Some)
    }

    /// The method `kind` names, or why this p1 has none by that name.
    pub fn method(&self, kind: &CredentialKind) -> Result<Arc<dyn CredentialMethod>, String> {
        if let Some(core) = kind.core() {
            return Ok(core);
        }
        match self.module(kind.name())? {
            Some(method) if method.description.method.is_some() => Ok(method),
            Some(_) => Err(format!(
                "the credential module \"{}\" provides a borrowed login, not a method a route's \
                 `kind` can name",
                kind.name()
            )),
            None => Err(self.missing(kind.name())),
        }
    }

    /// The keyed login a route's `borrow = ["<store>:<key>"]` names.
    pub fn keyed_login(&self, store: &str) -> Result<Arc<ModuleMethod>, String> {
        match self.module(store)? {
            Some(module) if module.login().is_some_and(|login| login.keyed) => Ok(module),
            Some(_) => Err(format!(
                "the credential module \"{store}\" provides no keyed login a `borrow` entry can \
                 name"
            )),
            None => Err(self.missing(store)),
        }
    }

    /// Every method this lookup has, in the order `p1 login --list` shows them: the API key,
    /// the installed modules' methods, then `none`.
    pub fn listed(&self) -> Vec<Arc<dyn CredentialMethod>> {
        let mut listed: Vec<Arc<dyn CredentialMethod>> = vec![Arc::new(ApiKeyMethod)];
        for name in self.modules.names() {
            if let Ok(Some(method)) = self.module(&name)
                && method.description.method.is_some()
            {
                listed.push(method);
            }
        }
        listed.push(Arc::new(NoneMethod));
        listed
    }

    fn missing(&self, name: &str) -> String {
        let mut known = vec!["api-key".to_owned()];
        known.extend(self.modules.names());
        known.push("none".to_owned());
        format!(
            "credential method \"{name}\" is not one this p1 has ({}); a method a credential \
             module implements is available once that module is installed (`p1 modules install`)",
            known.join(", ")
        )
    }
}

/// What a module says it provides: a method a route's `kind` names, a login file, or both.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Description {
    /// The method, when routes may name the module as their `kind`.
    #[serde(default)]
    pub method: Option<MethodDescription>,
    /// The login file the module reads, when it has one.
    #[serde(default)]
    pub login: Option<LoginDescription>,
}

/// A module's method.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MethodDescription {
    /// As `p1 login --list` shows it; the name when absent.
    #[serde(default)]
    pub label: Option<String>,
    /// Who mints its credential, for `p1 login`'s explanations.
    pub owner: String,
    /// What p1's store keeps for its routes.
    pub store: StoreKind,
    /// The usage probe its credential answers (ADR-0139).
    #[serde(default)]
    pub usage_probe: Option<String>,
}

/// The store forms a module's method may name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum StoreKind {
    /// A pasted API key.
    ApiKey,
    /// OAuth tokens, refreshed through the module.
    Oauth,
}

/// Where a module's login file is and how it is used.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LoginDescription {
    /// The login as a person names it (`Claude Code login`).
    pub title: String,
    /// The step that creates an entry (`run Claude Code login`).
    pub guidance: String,
    /// The environment variable naming the login's directory, when the tool has one.
    #[serde(default)]
    pub dir_env: Option<String>,
    /// The directory below `dir_env`'s value, when the variable names a parent.
    #[serde(default)]
    pub dir_env_subdir: Option<String>,
    /// The directory below the home, when the variable is unset.
    pub home_dir: String,
    /// The file's name inside the directory.
    pub file: String,
    /// The lock file the tool and p1 take while rotating; `<file>.lock` when absent.
    #[serde(default)]
    pub lock: Option<String>,
    /// Whether a route may name the directory it borrows (`login_dir`, ADR-0074).
    #[serde(default)]
    pub route_dir: bool,
    /// A store of keys a route's `borrow` names by key, never refreshed.
    #[serde(default)]
    pub keyed: bool,
}

impl LoginDescription {
    /// The lock file's name.
    pub fn lock_name(&self) -> String {
        self.lock
            .clone()
            .unwrap_or_else(|| format!("{}.lock", self.file))
    }

    /// The login file a route borrows: in its `login_dir` when it names one, else in the
    /// directory the variable names, else below the home. `None` when it needs a home and the
    /// host has none.
    pub fn path(&self, locations: &Locations, route_dir: Option<&str>) -> Option<PathBuf> {
        let dir = match route_dir {
            Some(dir) => locations.expand_home(dir).filter(|dir| {
                // A named directory that is the home itself is no login directory.
                locations
                    .home()
                    .is_none_or(|home| dir.components().ne(home.components()))
            }),
            None => self
                .dir_env
                .as_deref()
                .and_then(|name| locations.dir_var(name))
                .map(|dir| match &self.dir_env_subdir {
                    Some(subdir) => dir.join(subdir),
                    None => dir,
                })
                .or_else(|| locations.home().map(|home| home.join(&self.home_dir))),
        };
        dir.map(|dir| dir.join(&self.file))
    }
}

/// What a login file holds, as a module read it.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LoginTokens {
    /// The access token, or `None` when a keyed login has no entry for the key.
    pub access: Option<String>,
    /// The refresh token, when the login records one.
    #[serde(default)]
    pub refresh: Option<String>,
    /// When the access token expires, in milliseconds since the Unix epoch; `None` when the
    /// login records no expiry.
    #[serde(default)]
    pub expires_at_ms: Option<u64>,
    /// The account the token belongs to, when the provider needs it named.
    #[serde(default)]
    pub account_id: Option<String>,
}

/// A refresh request as a module built it.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RequestText {
    url: String,
    #[serde(default)]
    headers: Vec<(String, String)>,
    #[serde(default)]
    body: String,
}

/// One field of a refresh response, checked by the module on its own.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Checked<T> {
    /// The value.
    Ok(T),
    /// Why the response holds none usable, naming no value.
    Err(String),
}

impl<T> Checked<T> {
    /// As a `Result`.
    pub fn into_result(self) -> Result<T, String> {
        match self {
            Checked::Ok(value) => Ok(value),
            Checked::Err(reason) => Err(reason),
        }
    }
}

/// What a refresh response grants, each field checked on its own: a rotated refresh token is
/// kept even when the rest of the response cannot be used.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Granted {
    /// The new access token.
    pub access: Checked<String>,
    /// The rotated refresh token; `None` when the server rotates none.
    #[serde(default)]
    pub refresh: Option<String>,
    /// When the new access token expires (milliseconds since the Unix epoch), `None` for an
    /// opaque token with no recorded expiry.
    pub expires_at_ms: Checked<Option<u64>>,
    /// The account the new token belongs to, when it names one.
    #[serde(default)]
    pub account_id: Option<String>,
}

/// One installed credential module, described: a method, a login, or both.
pub struct ModuleMethod {
    name: String,
    description: Description,
    module: Arc<dyn CredentialModule>,
}

impl ModuleMethod {
    /// The module installed under `name`, described. A description that is not one, or a
    /// method whose store form needs a login it lacks, refuses the module.
    pub fn new(name: &str, module: Arc<dyn CredentialModule>) -> Result<Self, String> {
        let refused =
            |why: String| format!("the credential module \"{name}\" cannot be used: {why}");
        let text = module.describe().map_err(&refused)?;
        let description: Description = serde_json::from_str(&text)
            .map_err(|error| refused(format!("its description is not one: {error}")))?;
        if description.method.is_none() && description.login.is_none() {
            return Err(refused("it describes neither a method nor a login".into()));
        }
        if let Some(login) = &description.login {
            for file in [&login.file, &login.lock_name()] {
                if file.is_empty() || file.contains('/') || file == "." || file == ".." {
                    return Err(refused(format!(
                        "its login file name {file:?} is not a name"
                    )));
                }
            }
            if login.keyed && description.method.is_some() {
                return Err(refused(
                    "a keyed login is borrowed by API-key routes; it names no method".into(),
                ));
            }
        }
        Ok(Self {
            name: name.to_owned(),
            description,
            module,
        })
    }

    /// The name routes give it.
    pub fn module_name(&self) -> &str {
        &self.name
    }

    /// What the module provides.
    pub fn description(&self) -> &Description {
        &self.description
    }

    /// Its login file, when it has one.
    pub fn login(&self) -> Option<&LoginDescription> {
        self.description.login.as_ref()
    }

    /// The source name of its login.
    pub fn source_name(&self) -> Option<crate::SourceName> {
        self.login().map(|login| crate::SourceName::Login {
            name: login.title.clone(),
            guidance: login.guidance.clone(),
        })
    }

    /// What `file` holds for `key`. `Ok(None)` when a keyed login has no entry for it.
    pub fn read(&self, file: &[u8], key: Option<&str>) -> Result<Option<LoginTokens>, String> {
        let text = self.module.read_login(file, key)?;
        let tokens: LoginTokens = serde_json::from_str(&text).map_err(|error| {
            format!(
                "the credential module \"{}\" answered no login: {error}",
                self.name
            )
        })?;
        let Some(access) = &tokens.access else {
            return Ok(None);
        };
        // Both go into headers: anything a header cannot carry is refused here, whatever the
        // module answered.
        if !crate::usable_key(access) {
            return Err("it holds an access token that is not a header-safe token".into());
        }
        if tokens
            .account_id
            .as_deref()
            .is_some_and(|id| !crate::usable_key(id))
        {
            return Err("it holds an account id that is not a header-safe token".into());
        }
        Ok(Some(tokens))
    }

    /// The refresh request that spends `refresh_token`, refused unless it goes over HTTPS to
    /// an origin the module's verified manifest declares (ADR-0212).
    pub fn refresh_request(
        &self,
        refresh_token: &str,
        file: Option<&[u8]>,
    ) -> Result<HttpRequest, String> {
        let text = self.module.refresh_request(refresh_token, file)?;
        let request: RequestText = serde_json::from_str(&text).map_err(|error| {
            format!(
                "the credential module \"{}\" built no refresh request: {error}",
                self.name
            )
        })?;
        let origin = https_origin(&request.url);
        if !origin.as_deref().is_some_and(|origin| {
            self.module
                .token_origins()
                .iter()
                .any(|declared| declared == origin)
        }) {
            return Err(format!(
                "the credential module \"{}\" asked to send a refresh to {}, which is not an \
                 https origin its manifest declares ({}); nothing was sent",
                self.name,
                origin.as_deref().unwrap_or("a URL that is not https"),
                if self.module.token_origins().is_empty() {
                    "none".to_owned()
                } else {
                    self.module.token_origins().join(", ")
                }
            ));
        }
        Ok(HttpRequest {
            url: request.url,
            headers: request.headers,
            body: request.body.into_bytes(),
        })
    }

    /// What a refresh response `body` grants, its expiry counted from `sent_at_ms`.
    pub fn granted(&self, body: &[u8], sent_at_ms: u64) -> Result<Granted, String> {
        let text = self.module.refresh_response(body, sent_at_ms)?;
        let mut granted: Granted = serde_json::from_str(&text).map_err(|error| {
            format!(
                "the credential module \"{}\" read no refresh response: {error}",
                self.name
            )
        })?;
        if let Checked::Ok(access) = &granted.access
            && !crate::usable_key(access)
        {
            granted.access = Checked::Err(
                "the token refresh response holds an access token that is not a header-safe \
                 token"
                    .into(),
            );
        }
        if granted
            .account_id
            .as_deref()
            .is_some_and(|id| !crate::usable_key(id))
        {
            granted.account_id = None;
        }
        Ok(granted)
    }

    /// `file` with the response `body` written into it.
    pub fn merge(
        &self,
        file: &[u8],
        body: &[u8],
        sent_at_ms: u64,
        now_ms: u64,
        keep_access: bool,
    ) -> Result<Vec<u8>, String> {
        self.module
            .merge_login(file, body, sent_at_ms, now_ms, keep_access)
    }
}

/// The `scheme://host[:port]` of an https URL, lowercase; `None` for any other URL.
fn https_origin(url: &str) -> Option<String> {
    let rest = url.strip_prefix("https://")?;
    let authority = rest.split(['/', '?', '#']).next()?;
    if authority.is_empty() || authority.contains('@') {
        return None;
    }
    Some(format!("https://{}", authority.to_ascii_lowercase()))
}

impl CredentialMethod for ModuleMethod {
    fn name(&self) -> &str {
        &self.name
    }

    fn label(&self) -> &str {
        self.description
            .method
            .as_ref()
            .and_then(|method| method.label.as_deref())
            .unwrap_or(&self.name)
    }

    fn owner(&self) -> &str {
        self.description
            .method
            .as_ref()
            .map_or("its credential module", |method| method.owner.as_str())
    }

    fn store(self: Arc<Self>) -> StoreForm {
        match self.description.method.as_ref().map(|method| method.store) {
            Some(StoreKind::Oauth) => StoreForm::Oauth(self),
            Some(StoreKind::ApiKey) | None => StoreForm::ApiKey,
        }
    }

    fn takes_login_dir(&self) -> bool {
        self.login().is_some_and(|login| login.route_dir)
    }

    fn validate(&self, spec: &CredentialSpec) -> Result<(), String> {
        if let Some(dir) = &spec.login_dir {
            crate::spec::check_login_dir(dir)?;
        }
        Ok(())
    }

    fn borrowed(self: Arc<Self>, spec: &CredentialSpec) -> Vec<Box<dyn BorrowedLogin>> {
        if self.login().is_none_or(|login| login.keyed) {
            return Vec::new();
        }
        vec![Box::new(crate::login_file::BorrowedFile::new(
            self,
            spec.login_dir.clone(),
        ))]
    }

    fn owned_files(&self, spec: &CredentialSpec, locations: &Locations) -> Vec<PathBuf> {
        self.login()
            .and_then(|login| login.path(locations, spec.login_dir.as_deref()))
            .into_iter()
            .collect()
    }

    fn imports(&self) -> bool {
        self.description
            .method
            .as_ref()
            .is_some_and(|method| method.store == StoreKind::Oauth)
            && self.login().is_some_and(|login| !login.keyed)
    }

    /// A probe the module names that is no module name names none: the name reaches messages
    /// the owner reads.
    fn usage_probe(&self) -> Option<&str> {
        self.description
            .method
            .as_ref()
            .and_then(|method| method.usage_probe.as_deref())
            .filter(|name| crate::is_module_name(name))
    }
}

impl OauthRefresh for ModuleMethod {
    fn request(&self, refresh_token: &str) -> Result<HttpRequest, String> {
        self.refresh_request(refresh_token, None)
    }

    fn parse(&self, body: &[u8], sent_at_ms: u64) -> RefreshedTokens {
        match self.granted(body, sent_at_ms) {
            Ok(granted) => RefreshedTokens {
                access: granted.access.into_result(),
                refresh: granted.refresh,
                expires_at_ms: granted.expires_at_ms.into_result(),
            },
            Err(reason) => RefreshedTokens {
                access: Err(reason.clone()),
                refresh: None,
                expires_at_ms: Err(reason),
            },
        }
    }
}
