//! Route files (`routes/<id>.toml`): how an ACCOUNT and ENDPOINT are reached, as
//! data (`docs/design/routes-and-profiles.md` §1.2). A route file names an ADAPTER
//! KEY that `p1-host::catalog` has compiled in, and it holds a credential
//! REFERENCE, never a value: the reference is `p1_auth::CredentialSpec`, the one
//! crate that knows where a credential is read from (ADR-0040). The lookup
//! directory is the one profiles use: `<environments dir>/../routes`.
//!
//! Loading is total: a file stem that disagrees with `id`, a secret-looking header,
//! an unknown adapter, an unknown credential kind, a credential kind whose source is
//! not data-driven yet, a settings key the adapter does not know, or an unusable
//! model binding are all load errors reported before any provider is built.

use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use p1_auth::{CredentialKind, CredentialSpec};

use crate::accounts::Account;
use serde::Deserialize;

include!(concat!(env!("OUT_DIR"), "/shipped_routes.rs"));

/// The adapter keys a route file may name. `catalog` dispatches on exactly this set;
/// an unknown `adapter` is a load error listing these.
pub const ADAPTER_KEYS: &[&str] = &["openai-chat", "anthropic-messages", "openai-responses"];

/// One `[models.<profile id>]` entry: the wire model this route reaches that profile
/// by, plus the route's own ceilings on it.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelBinding {
    /// The name this route's wire protocol knows the model by. It is NOT the profile
    /// id: one profile may be bound to different wire names on different routes.
    pub wire_model: String,
    /// An optional route ceiling on the context window. It lowers the profile's
    /// ceiling, never raises it. Parsed and carried; nothing consumes it yet.
    #[serde(default)]
    pub context_limit: Option<u64>,
    /// An optional route ceiling on output tokens. It lowers the profile's ceiling,
    /// never raises it.
    #[serde(default)]
    pub output_limit: Option<u32>,
}

/// One route bound to one account (ADR-0139): what the catalog registers and every
/// consumer reads. The route file's own fields are kept as written; `credential` is
/// the bound account's reference, and `origin_route` is the replay origin of this
/// route × account pair. The host interprets the route's fields only to route:
/// `[adapter_settings]` is checked when the file loads against the host's copy of the
/// settings type of the adapter named by `adapter` ([`RouteFile::settings`]), and the
/// table itself reaches the component that serves the route as-is
/// (`docs/design/routes-and-profiles.md` §1.2).
///
/// Deserializing one directly accepts only a route with an inline `[credential]`
/// (its implicit account); a route that names an account is bound by
/// [`load_all_routes`] and [`load_route_by_id`], which see the account files.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(try_from = "RouteToml")]
pub struct RouteFile {
    /// The catalog key: the route id for the route's primary account, else
    /// `<route id>@<account id>`.
    pub id: String,
    /// The route file's id (the file stem).
    pub route: String,
    /// The bound account's id; a route's implicit account carries the route id.
    pub account: String,
    /// `Origin.route` of this pair: the file's `origin_route` for the route's implicit
    /// account, else `<origin_route>@<account id>` (ADR-0139 §7).
    pub origin_route: String,
    /// A compiled adapter key ([`ADAPTER_KEYS`]).
    pub adapter: String,
    pub endpoint: String,
    /// The bound account's credential reference.
    pub credential: CredentialSpec,
    /// Reuse a shipped API-key route's store entry on the same endpoint origin.
    /// Environment lookup still uses this route's explicit credential spec.
    pub credential_route: Option<String>,
    /// p1's store key of the bound account (ADR-0040; `credential_route` for a route
    /// that names one, ADR-0134). Read it through [`RouteFile::credential_route_id`],
    /// which keeps an implicit account's key following the route's own fields.
    pub store_id: String,
    /// The bound account's declared endpoint origins.
    pub account_origins: Vec<String>,
    /// Bound to the route's own inline `[credential]`: the account's identity follows
    /// the route's (`id`, `credential_route`, `endpoint`), exactly as before accounts.
    pub implicit: bool,
    /// Native transport policy, never part of the component's adapter settings.
    pub retry_policy: RouteRetryPolicy,
    pub first_byte_timeout_secs: Option<u64>,
    pub stream_idle_timeout_secs: Option<u64>,
    /// Static, non-secret headers. Authentication comes exclusively from the account,
    /// so a secret-looking name here is a load error.
    pub headers: BTreeMap<String, String>,
    /// Kept as an uninterpreted table; [`RouteFile::settings`] types it.
    pub adapter_settings: Option<toml::Value>,
    /// Profile id -> binding. A profile without an entry is NOT served by this route.
    pub models: BTreeMap<String, ModelBinding>,
    /// The route file and the account's file (the route file for an implicit
    /// account); empty for a route deserialized directly.
    pub source: PathBuf,
    pub account_source: PathBuf,
}

/// The TOML surface of `routes/<id>.toml`. A route names its credential either
/// inline (`[credential]`, its implicit account) or through a default `account`, or
/// neither (the account comes from the environment or the only covering account).
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RouteToml {
    pub id: String,
    pub origin_route: String,
    pub adapter: String,
    pub endpoint: String,
    #[serde(default)]
    pub credential: Option<CredentialSpec>,
    /// The route's default account (ADR-0139 §3 rule 4).
    #[serde(default)]
    pub account: Option<String>,
    #[serde(default)]
    pub credential_route: Option<String>,
    #[serde(default)]
    pub retry_policy: RouteRetryPolicy,
    #[serde(default, deserialize_with = "first_byte_timeout_secs")]
    pub first_byte_timeout_secs: Option<u64>,
    #[serde(default, deserialize_with = "stream_idle_timeout_secs")]
    pub stream_idle_timeout_secs: Option<u64>,
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    #[serde(default)]
    pub adapter_settings: Option<toml::Value>,
    #[serde(default)]
    pub models: BTreeMap<String, ModelBinding>,
}

impl TryFrom<RouteToml> for RouteFile {
    type Error = String;

    fn try_from(mut route: RouteToml) -> Result<Self, String> {
        rename_dialect(&route.adapter, &mut route.adapter_settings)?;
        let account = route.implicit_account(Path::new("")).ok_or_else(|| {
            format!(
                "route `{}` has no inline `[credential]`; a route that uses an account file is \
                 loaded through its environment directories",
                route.id
            )
        })?;
        Ok(route.bind(route.id.clone(), &account))
    }
}

impl RouteToml {
    fn validate(&self, stem: &str) -> Result<(), String> {
        if self.id != stem {
            return Err(format!(
                "route id \"{}\" must equal the file stem \"{stem}\"",
                self.id
            ));
        }
        if self.id.contains('@') {
            return Err(format!(
                "route id \"{}\" contains `@`, which separates a route from its account",
                self.id
            ));
        }
        if self.origin_route.is_empty() || self.endpoint.is_empty() {
            return Err("`origin_route` and `endpoint` must be nonempty".into());
        }
        if !ADAPTER_KEYS.contains(&self.adapter.as_str()) {
            return Err(format!(
                "unknown adapter \"{}\"; the known adapters are {}",
                self.adapter,
                known_adapters()
            ));
        }
        for (name, value) in &self.headers {
            if is_secret_header(name) {
                return Err(format!(
                    "header \"{name}\" looks like a credential; a route file carries static, \
                     non-secret headers only, and authentication comes from `[credential]`"
                ));
            }
            if !is_header_name(name) {
                return Err(format!("`[headers]` name \"{name}\" is not a header name"));
            }
            if value.is_empty() || !value.bytes().all(|b| (32..=126).contains(&b)) {
                return Err(format!(
                    "`[headers]` value for \"{name}\" is not printable ASCII"
                ));
            }
        }
        if self.credential.is_some() && self.account.is_some() {
            return Err(
                "the route has both an inline `[credential]` and a default `account`; keep one"
                    .into(),
            );
        }
        if let Some(account) = &self.account
            && !p1_assembly::is_account_id(account)
        {
            return Err(format!("`account` \"{account}\" is not an account id"));
        }
        if let Some(credential) = &self.credential {
            credential.validate()?;
        }
        if let Some(source) = &self.credential_route
            && (!self.credential.as_ref().is_some_and(|credential| {
                credential.kind == CredentialKind::ApiKey && credential.store_only
            }) || !SHIPPED_ROUTES.iter().any(|&(id, endpoint, kind)| {
                id == source
                    && kind == "api-key"
                    && endpoint_origin(endpoint) == endpoint_origin(&self.endpoint)
            }))
        {
            return Err("`credential_route` requires a store-only API key and a shipped API-key route on the same endpoint origin".into());
        }
        for (id, binding) in &self.models {
            if id.trim().is_empty() || binding.wire_model.trim().is_empty() {
                return Err(
                    "every `[models.<profile id>]` entry needs a nonempty profile id and \
                     `wire_model`"
                        .into(),
                );
            }
            if binding.output_limit == Some(0) || binding.context_limit == Some(0) {
                return Err(format!(
                    "`[models.\"{id}\"]` declares a limit of 0; omit a limit that is unknown"
                ));
            }
        }
        adapter_settings(&self.adapter, &self.adapter_settings)?;
        Ok(())
    }

    /// The implicit account of an inline `[credential]` (ADR-0139 §6): the route's id,
    /// its endpoint origin, and its store identity (`credential_route` or the id).
    pub fn implicit_account(&self, source: &Path) -> Option<Account> {
        let credential = self.credential.clone()?;
        Some(Account {
            id: self.id.clone(),
            origins: vec![endpoint_origin(&self.endpoint)],
            credential,
            store_id: self
                .credential_route
                .clone()
                .unwrap_or_else(|| self.id.clone()),
            implicit_of: Some(self.id.clone()),
            source: source.to_path_buf(),
            label: None,
            usage: None,
            legacy_routes: BTreeMap::new(),
            legacy_origins: BTreeMap::new(),
        })
    }

    /// This route bound to `account` under the catalog key `key`.
    pub fn bind(&self, key: String, account: &Account) -> RouteFile {
        let own = account.implicit_of.as_deref() == Some(self.id.as_str());
        RouteFile {
            id: key,
            route: self.id.clone(),
            account: account.id.clone(),
            // ADR-0139 §7: a converted pair keeps the origin its sessions recorded.
            origin_route: if own {
                self.origin_route.clone()
            } else {
                account
                    .legacy_origins
                    .get(&self.id)
                    .cloned()
                    .unwrap_or_else(|| format!("{}@{}", self.origin_route, account.id))
            },
            adapter: self.adapter.clone(),
            endpoint: self.endpoint.clone(),
            credential: account.credential.clone(),
            credential_route: if own {
                self.credential_route.clone()
            } else {
                None
            },
            store_id: account.store_id.clone(),
            account_origins: account.origins.clone(),
            implicit: own,
            retry_policy: self.retry_policy,
            first_byte_timeout_secs: self.first_byte_timeout_secs,
            stream_idle_timeout_secs: self.stream_idle_timeout_secs,
            headers: self.headers.clone(),
            adapter_settings: self.adapter_settings.clone(),
            models: self.models.clone(),
            source: PathBuf::new(),
            account_source: account.source.clone(),
        }
    }
}

fn timeout_secs<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
    key: &str,
) -> Result<Option<u64>, D::Error> {
    let seconds = u64::deserialize(deserializer).map_err(|_| {
        <D::Error as serde::de::Error>::custom(format!("`{key}` must be an integer in 30..=1800"))
    })?;
    if !(30..=1800).contains(&seconds) {
        return Err(serde::de::Error::custom(format!(
            "`{key}` must be an integer in 30..=1800"
        )));
    }
    Ok(Some(seconds))
}

fn first_byte_timeout_secs<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<u64>, D::Error> {
    timeout_secs(deserializer, "first_byte_timeout_secs")
}

fn stream_idle_timeout_secs<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<u64>, D::Error> {
    timeout_secs(deserializer, "stream_idle_timeout_secs")
}

/// Route-scoped retry presets (ADR-0137); omission preserves existing behavior.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RouteRetryPolicy {
    #[default]
    Default,
    Deepseek,
    Patient,
}

impl RouteRetryPolicy {
    pub fn resolve(self) -> p1_provider_http::RetryPolicy {
        use p1_provider_http::RetryPolicy;
        use std::time::Duration;
        match self {
            Self::Default => RetryPolicy::default(),
            Self::Deepseek => RetryPolicy {
                max_retries: 5,
                base: Duration::from_millis(500),
                cap: Duration::from_secs(10),
                jitter: Duration::ZERO,
                jitter_percent: 10,
                retry_after_limit: Some(Duration::from_secs(10)),
            },
            Self::Patient => RetryPolicy {
                max_retries: 8,
                base: Duration::from_secs(2),
                cap: Duration::from_secs(32),
                jitter: Duration::ZERO,
                jitter_percent: 10,
                retry_after_limit: Some(Duration::from_secs(300)),
            },
        }
    }
}

/// The keys the host adds to a provider component's `adapter-settings` object, next to
/// the route file's own `[adapter_settings]` keys (ADR-0086). The
/// component removes them before it parses its settings type, which denies unknown
/// fields, so no adapter may ever name a settings field like one of these.
pub const MODEL_PROFILE_KEY: &str = "model_profile";
pub const ROUTE_HEADERS_KEY: &str = "route_headers";
pub const MODEL_BINDING_KEY: &str = "model_binding";

/// The `[adapter_settings]` table of one route, typed by the adapter that named it.
#[derive(Debug, Clone, PartialEq)]
pub enum AdapterSettings {
    OpenAiChat(ChatAdapterSettings),
    AnthropicMessages(MessagesAdapterSettings),
    OpenAiResponses(ResponsesAdapterSettings),
}

// The settings types below are the host's copies of the adapters' own `[adapter_settings]`
// types, field for field and spelling for spelling, so that a route file is checked when it
// loads without the host linking the native adapters (S7.10-R4): the provider COMPONENT is
// what parses them for a request (ADR-0086). The type names are the adapters' too, because
// serde names a type in some errors, and a malformed file must read the same either way; the
// unit tests hold both sides against every shipped route and every malformed case.

/// `openai-chat`'s message encodings (`p1_provider_openai_chat::ChatDialect`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ChatDialect {
    #[default]
    ThinkingWithReasoningAlias,
    RetainedThinking,
}

/// `openai-chat`'s non-secret client identities (`p1_provider_openai_chat::ClientIdentity`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ClientIdentity {
    Opencode,
}

/// `openai-chat`'s `[adapter_settings]` (`p1_provider_openai_chat::ChatAdapterSettings`).
#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChatAdapterSettings {
    pub dialect: ChatDialect,
    #[serde(default)]
    pub session_header: Option<String>,
    #[serde(default)]
    pub client_identity: Option<ClientIdentity>,
}

/// `anthropic-messages`' account behaviours (`p1_provider_anthropic::MessagesAccount`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum MessagesAccount {
    ClaudeCodeSubscription,
    OpencodeGo,
    Zai,
}

/// `anthropic-messages`' `[adapter_settings]` (`p1_provider_anthropic::MessagesAdapterSettings`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MessagesAdapterSettings {
    pub account: MessagesAccount,
    pub long_context: bool,
}

impl<'de> Deserialize<'de> for MessagesAdapterSettings {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        // Retain the public wire-policy shape; credential_header belongs to the
        // host broker, which reads it from the original adapter settings object.
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Settings {
            // ADR-0139 §8: `dialect`, formerly `account`.
            #[serde(rename = "dialect", alias = "account")]
            account: MessagesAccount,
            #[serde(default)]
            long_context: bool,
            #[serde(default)]
            credential_header: Option<String>,
        }
        let settings = Settings::deserialize(deserializer)?;
        let expected =
            (settings.account != MessagesAccount::ClaudeCodeSubscription).then_some("x-api-key");
        if settings.credential_header.as_deref() != expected {
            return Err(serde::de::Error::custom(
                "OpenCode Go and Z.ai require credential_header = x-api-key; Claude accepts no credential_header",
            ));
        }
        Ok(Self {
            account: settings.account,
            long_context: settings.long_context,
        })
    }
}

/// `openai-responses`' account behaviours (`p1_provider_openai::ResponsesAccount`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ResponsesAccount {
    CodexSubscription,
}

/// `openai-responses`' `[adapter_settings]` (`p1_provider_openai::ResponsesAdapterSettings`).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResponsesAdapterSettings {
    /// `dialect` in the route file (ADR-0139 §8), formerly `account`.
    #[serde(rename = "dialect", alias = "account")]
    pub account: ResponsesAccount,
    /// Absent means [`ResponsesTransport::Sse`] (ADR-0047 §1).
    #[serde(default)]
    pub transport: ResponsesTransport,
}

/// How a Responses route reaches the model (`p1_provider_openai::ResponsesTransport`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ResponsesTransport {
    #[default]
    Sse,
    Websocket,
}

impl RouteFile {
    /// Native HTTP/SSE bounds; each omitted key preserves its own default.
    pub fn stream_timeouts(&self) -> p1_provider_http::StreamTimeouts {
        use std::time::Duration;
        let defaults = p1_provider_http::StreamTimeouts::default();
        p1_provider_http::StreamTimeouts {
            first_byte: self
                .first_byte_timeout_secs
                .map(Duration::from_secs)
                .unwrap_or(defaults.first_byte),
            idle: self
                .stream_idle_timeout_secs
                .map(Duration::from_secs)
                .unwrap_or(defaults.idle),
        }
    }

    /// Store identity, distinct from the wire/replay route identity: the bound
    /// account's store key (ADR-0139 §5).
    pub fn credential_route_id(&self) -> &str {
        if self.implicit {
            return self
                .credential_route
                .as_deref()
                .unwrap_or_else(|| self.route_id());
        }
        &self.store_id
    }

    /// The route file's id; for a route bound to its implicit account, the catalog key
    /// without an account suffix, so the identity follows the key as it always did.
    pub fn route_id(&self) -> &str {
        if self.implicit {
            return self.id.split('@').next().unwrap_or(&self.id);
        }
        &self.route
    }

    /// The id `p1 login` takes for this route's account: the route id for its implicit
    /// account, else the account id (ADR-0139 §5).
    pub fn login_id(&self) -> &str {
        if self.implicit {
            self.route_id()
        } else {
            &self.account
        }
    }

    /// Whether the bound account declares this route's endpoint origin. An implicit
    /// account declares exactly its route's endpoint origin.
    pub fn account_covers_endpoint(&self) -> bool {
        self.implicit
            || self
                .account_origins
                .contains(&endpoint_origin(&self.endpoint))
    }

    /// The settings the adapter named by `adapter` takes, checked against the adapter's
    /// own fields: an unknown key or value fails when the route loads, before any
    /// provider component parses the same table for a request.
    pub fn settings(&self) -> Result<AdapterSettings, String> {
        adapter_settings(&self.adapter, &self.adapter_settings)
    }

    #[cfg(test)]
    fn typed_settings<T: serde::de::DeserializeOwned>(&self) -> Result<T, String> {
        self.adapter_settings
            .clone()
            .unwrap_or_else(|| toml::Value::Table(toml::Table::new()))
            .try_into::<T>()
            .map_err(|error| format!("invalid `[adapter_settings]`: {error}"))
    }

    /// The `provider-settings.adapter-settings` object a provider component is
    /// configured with for one model this route binds: the `[adapter_settings]` table
    /// unchanged, plus the three reserved keys. The component cannot read files, so
    /// the profile travels as its file stem and text; `route_headers` and
    /// `model_binding` carry the route data the native host folds into the adapter's
    /// route value itself. An absent limit stays absent: unknown is never zero.
    pub fn component_adapter_settings(
        &self,
        binding: &ModelBinding,
        profile_stem: &str,
        profile_toml: &str,
    ) -> serde_json::Value {
        let mut settings = match self.adapter_settings.as_ref().map(serde_json::to_value) {
            Some(Ok(serde_json::Value::Object(table))) => table,
            // `load_route` refuses settings that are not a table the adapter parses, so
            // only a hand-built, unvalidated route lands here, and the component's own
            // parse then names the fields that are missing.
            _ => serde_json::Map::new(),
        };
        let headers: serde_json::Map<String, serde_json::Value> = self
            .headers
            .iter()
            .map(|(name, value)| (name.clone(), serde_json::Value::from(value.as_str())))
            .collect();
        let mut limits = serde_json::Map::new();
        if let Some(context_limit) = binding.context_limit {
            limits.insert("context_limit".into(), context_limit.into());
        }
        if let Some(output_limit) = binding.output_limit {
            limits.insert("output_limit".into(), output_limit.into());
        }
        settings.insert(
            MODEL_PROFILE_KEY.into(),
            serde_json::json!({ "stem": profile_stem, "toml": profile_toml }),
        );
        settings.insert(ROUTE_HEADERS_KEY.into(), headers.into());
        settings.insert(MODEL_BINDING_KEY.into(), limits.into());
        settings.into()
    }

    /// The binding for one profile, or the spec §2 error naming what this route does
    /// serve. There is no pass-through of unknown model names.
    pub fn binding(&self, profile_id: &str) -> Result<&ModelBinding, String> {
        self.models.get(profile_id).ok_or_else(|| {
            let served = if self.models.is_empty() {
                "none".to_string()
            } else {
                self.models
                    .keys()
                    .map(String::as_str)
                    .collect::<Vec<_>>()
                    .join(", ")
            };
            format!(
                "route \"{}\" does not serve profile \"{profile_id}\" (it serves: {served})",
                self.id
            )
        })
    }
}

/// `<dir>/../routes`, for each environments directory the host was given, highest
/// priority first. The same rule `p1-assembly` uses for `<dir>/../profiles`.
pub fn routes_dirs(environment_dirs: &[PathBuf]) -> Vec<PathBuf> {
    environment_dirs
        .iter()
        .map(|dir| dir.join("../routes"))
        .collect()
}

/// Shipped route lookup directories, unaffected by configuration overrides.
/// Credential trust does not read them; its anchor is compiled (ADR-0110).
pub fn shipped_routes_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Ok(exe) = std::env::current_exe()
        && let Some(bin) = exe.parent()
    {
        dirs.push(bin.join("../share/p1/routes"));
    }
    if cfg!(debug_assertions) {
        dirs.push(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../routes"));
    }
    dirs
}

/// The credential trust anchor is compiled from source routes, not installed files
/// (ADR-0110). A malformed shipped TOML file fails the build.
pub fn shipped_origins() -> BTreeMap<String, Vec<String>> {
    let mut origins: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for &(id, endpoint, _) in SHIPPED_ROUTES {
        origins
            .entry(id.into())
            .or_default()
            .push(endpoint_origin(endpoint));
    }
    origins
}

/// Compatibility entry point: installation directories cannot alter the anchor.
pub fn shipped_origins_in(_dirs: &[PathBuf]) -> BTreeMap<String, Vec<String>> {
    shipped_origins()
}

/// `scheme://authority` of an endpoint, lowercased: where a request is sent, whatever
/// its path. Nothing is normalized beyond case, so a spelling the shipped file does not
/// use (an explicit default port) is a different origin — the check fails closed.
pub fn endpoint_origin(endpoint: &str) -> String {
    let (scheme, rest) = endpoint.split_once("://").unwrap_or(("", endpoint));
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    format!("{scheme}://{authority}").to_ascii_lowercase()
}

/// Issue #484: a route that takes the id of a route p1 ships takes that route's stored
/// and borrowed credentials too — they are keyed by the id — so it may only send them to
/// the origin the shipped route names. An override in `P1_CONFIG_DIR` or
/// `P1_ENVIRONMENTS_DIR` that points a shipped id elsewhere is refused before any
/// credential is read. A route with `kind = "none"` sends no credential and is not held
/// to it; a route under a new id has no shipped origin to keep.
pub fn check_shipped_origin(
    route: &RouteFile,
    shipped: &BTreeMap<String, Vec<String>>,
) -> Result<(), String> {
    if route.credential.kind == p1_auth::CredentialKind::None {
        return Ok(());
    }
    let origin = endpoint_origin(&route.endpoint);
    // ADR-0139 §2: a credential is bound by its store identity as well as by the route
    // id, so neither a reused route id nor a reused store entry reaches a new origin.
    for id in [route.route_id(), route.credential_route_id()] {
        let Some(origins) = shipped.get(id) else {
            continue;
        };
        if !origins.contains(&origin) {
            return Err(format!(
                "route `{}` overrides a route p1 ships (`{id}`, by route id or store \
                 identity) but sends its credential to {origin} instead of {}; give the route \
                 file and its account new ids to use another endpoint",
                route.id,
                origins.join(" or ")
            ));
        }
    }
    Ok(())
}

/// ADR-0139 §2: the bound account must declare the route's endpoint origin. Checked
/// before any credential lookup, at assembly and on every access.
pub fn check_account_origin(route: &RouteFile) -> Result<(), String> {
    if route.account_covers_endpoint() {
        return Ok(());
    }
    Err(format!(
        "account `{}` does not list the endpoint origin {} of route `{}` in its `origins` \
         (it lists: {})",
        route.account,
        endpoint_origin(&route.endpoint),
        route.route,
        route.account_origins.join(", ")
    ))
}

/// Bind credential sources as well as shipped ids before resolving any credential
/// (ADR-0110). Origin metadata is read separately from the credential file.
pub fn check_credential_origin(
    route: &RouteFile,
    locations: &p1_auth::Locations,
) -> Result<(), String> {
    use p1_auth::CredentialKind;
    check_account_origin(route)?;
    check_shipped_origin(route, &shipped_origins())?;
    if route.credential.kind == CredentialKind::None || is_loopback_endpoint(&route.endpoint) {
        return Ok(());
    }
    let origin = endpoint_origin(&route.endpoint);
    let borrows = !route.credential.store_only
        && (matches!(
            route.credential.kind,
            CredentialKind::ClaudeCodeOauth | CredentialKind::CodexOauth
        ) || !route.credential.borrow.is_empty());
    if borrows {
        if !SHIPPED_ROUTES.iter().any(|&(_, endpoint, kind)| {
            kind == route.credential.kind.name() && endpoint_origin(endpoint) == origin
        }) {
            return Err(format!(
                "route `{}` cannot send a borrowed {} credential to endpoint {origin}; \
                 use an endpoint origin shipped for the same credential kind",
                route.id,
                route.credential.kind.name()
            ));
        }
        if route.credential.kind != CredentialKind::ApiKey {
            return Ok(());
        }
    }
    // Shipped store identities have already been bound to their compiled origin above.
    if is_shipped_store(route) {
        return Ok(());
    }
    if p1_auth::store::endpoint_origins(route.credential_route_id(), locations)?.contains(&origin) {
        return Ok(());
    }
    let login = route.login_id();
    Err(format!(
        "route `{}` cannot send its credential to untrusted endpoint {origin}; \
         run `p1 login {login}` to store a key with this origin, or \
         `p1 login {login} --trust-endpoint` to trust it for an API key or store-only OAuth",
        route.id
    ))
}

/// Origin required for a store read on this route. Shipped, loopback and borrowed
/// OAuth routes retain their compiled-kind trust; custom keys/store-only OAuth
/// must synchronize approval and credential acquisition with login writes.
pub(crate) fn store_origin_policy(route: &RouteFile) -> (Option<String>, bool) {
    use p1_auth::CredentialKind;
    if route.credential.kind == CredentialKind::None || is_loopback_endpoint(&route.endpoint) {
        return (None, false);
    }
    let required = !is_shipped_store(route)
        && (route.credential.kind == CredentialKind::ApiKey || route.credential.store_only);
    (Some(endpoint_origin(&route.endpoint)), required)
}

/// Shared inspection wording. Refusal probes metadata only, never key variables
/// or credential documents. All inspection commands use this gate.
pub(crate) fn credential_description(route: &RouteFile, locations: &p1_auth::Locations) -> String {
    let line = match check_credential_origin(route, locations) {
        Ok(()) => {
            p1_auth::describe(route.credential_route_id(), &route.credential, locations).line()
        }
        Err(reason) => format!(
            "none — endpoint origin {} is not approved: {reason}; run `p1 login {}` \
             or `p1 login {} --trust-endpoint`{}",
            endpoint_origin(&route.endpoint),
            route.login_id(),
            route.login_id(),
            if route.credential.store_only {
                p1_auth::CredentialPolicy::StoreOnly.marker()
            } else {
                ""
            },
        ),
    };
    p1_redact::redact(&line).text
}

/// Whether the bound account's store entry is a shipped route's (ADR-0110 anchor).
/// For a route's implicit account the exemption stays keyed by the route id, as before
/// accounts (a `credential_route` still needs its own approval on a new id).
fn is_shipped_store(route: &RouteFile) -> bool {
    let id = if route.implicit {
        route.route_id()
    } else {
        route.credential_route_id()
    };
    SHIPPED_ROUTES.iter().any(|&(shipped, _, _)| shipped == id)
}

fn is_loopback_endpoint(endpoint: &str) -> bool {
    let Some((scheme, rest)) = endpoint.split_once("://") else {
        return false;
    };
    if !matches!(
        scheme.to_ascii_lowercase().as_str(),
        "http" | "https" | "ws" | "wss"
    ) {
        return false;
    }
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    let host = if let Some(ipv6) = authority.strip_prefix('[') {
        let Some((host, suffix)) = ipv6.split_once(']') else {
            return false;
        };
        if !suffix.is_empty() && !valid_port(suffix) {
            return false;
        }
        host
    } else if let Some((host, port)) = authority.split_once(':') {
        if !valid_port(&format!(":{port}")) {
            return false;
        }
        host
    } else {
        authority
    };
    matches!(
        host.to_ascii_lowercase().as_str(),
        "127.0.0.1" | "::1" | "localhost"
    )
}

fn valid_port(suffix: &str) -> bool {
    suffix
        .strip_prefix(':')
        .is_some_and(|port| port.parse::<u16>().is_ok())
}

/// Every `*.toml` in `dir`, sorted by file name, parsed, validated and bound to its
/// primary account, with the accounts of `<dir>/../accounts`. A directory that does not
/// exist holds no routes: an environment naming a route that is not there fails at
/// assembly, where the error can list what exists.
pub fn load_routes(dir: &Path) -> Result<Vec<RouteFile>, String> {
    RouteSet::from_layers(&[(dir.to_path_buf(), dir.join("../accounts"))])?.primaries()
}

/// Every route file the host can see, highest-priority directory first, each bound to
/// its primary account (ADR-0139 §3 rules 4–6): an id found in more than one directory
/// resolves to the first one, exactly like an environment or a profile. Sorted by id,
/// so the catalog registers them in a stable order. A route that names no account and
/// has no single account covering its origin is left out; naming it is an error that
/// lists the candidates ([`load_route_by_id`]).
pub fn load_all_routes(environment_dirs: &[PathBuf]) -> Result<Vec<RouteFile>, String> {
    RouteSet::load(environment_dirs)?.primaries()
}

/// Every route × account pair the catalog registers: each route's primary account
/// under the route id, and every account that declares the route's origin under
/// `<route id>@<account id>` (ADR-0139 §2).
pub fn load_route_pairs(environment_dirs: &[PathBuf]) -> Result<Vec<RouteFile>, String> {
    RouteSet::load(environment_dirs)?.pairs()
}

/// The one route an environment's `route` names (`<route id>` or
/// `<route id>@<account id>`), in the host's search order. A missing file is an error
/// that lists the ids the directories do hold, like the profile lookup's; the host
/// reports it before it builds a provider (spec §2).
pub fn load_route_by_id(environment_dirs: &[PathBuf], id: &str) -> Result<RouteFile, String> {
    let dirs = routes_dirs(environment_dirs);
    // Without account files nothing can rebind or shadow a route by id, and a route with
    // an inline `[credential]` reads only its own file, exactly as before accounts existed.
    let accounts = crate::accounts::accounts_dirs(environment_dirs)
        .iter()
        .any(|dir| {
            std::fs::read_dir(dir).is_ok_and(|mut entries| {
                entries.any(|entry| {
                    entry.is_ok_and(|entry| entry.path().extension() == Some(OsStr::new("toml")))
                })
            })
        });
    if !id.contains('@') && !accounts {
        let Some(path) = dirs
            .iter()
            .map(|dir| dir.join(format!("{id}.toml")))
            .find(|path| path.is_file())
        else {
            return Err(not_found(id, &dirs));
        };
        let route = load_route_toml(&path)?;
        if let Some(account) = route.implicit_account(&path) {
            let mut bound = route.bind(route.id.clone(), &account);
            bound.source = path;
            return Ok(bound);
        }
    }
    RouteSet::load(environment_dirs)?.bind(id)
}

/// The files that define `id` of `kind` (`environments`, `routes`, `profiles` or
/// `accounts`), in search order (ADR-0139 §4): the first is the one used, and every
/// later one is shadowed by it. A directory listed twice counts once.
pub fn definition_files(environment_dirs: &[PathBuf], kind: &str, id: &str) -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = Vec::new();
    for dir in environment_dirs {
        let path = match kind {
            "environments" => dir.join(id).join("environment.toml"),
            _ => dir.join("..").join(kind).join(format!("{id}.toml")),
        };
        let same = |seen: &PathBuf| match (seen.canonicalize(), path.canonicalize()) {
            (Ok(left), Ok(right)) => left == right,
            _ => *seen == path,
        };
        if path.is_file() && !files.iter().any(same) {
            files.push(path);
        }
    }
    files
}

/// The definitions an environment uses — `(kind, files)` for its environment, route,
/// profile and account, the used file first — for `p1 env show` and `p1 models`. An
/// account that is a route's inline `[credential]` lives in that route file; the
/// account line is left out when it is this route's own.
pub fn definitions(
    environment_dirs: &[PathBuf],
    environment: &str,
    bound: &RouteFile,
    profile: &str,
) -> Vec<(&'static str, Vec<PathBuf>)> {
    let mut definitions = vec![
        (
            "environment",
            definition_files(environment_dirs, "environments", environment),
        ),
        (
            "route",
            definition_files(environment_dirs, "routes", &bound.route),
        ),
        (
            "profile",
            definition_files(environment_dirs, "profiles", profile),
        ),
    ];
    if bound.account_source != bound.source {
        let mut files = vec![bound.account_source.clone()];
        files.extend(
            definition_files(environment_dirs, "accounts", &bound.account)
                .into_iter()
                .filter(|path| *path != bound.account_source),
        );
        definitions.push(("account", files));
    }
    definitions
}

/// Every account the host can see (ADR-0139 §5): account files and the implicit
/// accounts of inline `[credential]` tables, first file per id winning, one per store
/// identity (a route that reuses another's entry through `credential_route` adds no
/// account of its own), sorted by id.
pub fn load_all_accounts(environment_dirs: &[PathBuf]) -> Result<Vec<Account>, String> {
    let set = RouteSet::load(environment_dirs)?;
    let mut accounts: Vec<Account> = Vec::new();
    for account in set.accounts {
        let shares = account.implicit_of.is_some() && account.store_id != account.id;
        if !shares
            && !accounts
                .iter()
                .any(|seen| seen.store_id == account.store_id)
        {
            accounts.push(account);
        }
    }
    accounts.sort_by(|left, right| left.id.cmp(&right.id));
    Ok(accounts)
}

/// The account an operator command names (ADR-0139 §5): an account id first, else a
/// route id, which means the account that route binds by default.
pub fn load_account_by_id(environment_dirs: &[PathBuf], id: &str) -> Result<Account, String> {
    let set = RouteSet::load(environment_dirs)?;
    if let Some(account) = set.accounts.iter().find(|account| account.id == id) {
        return Ok(account.clone());
    }
    if let Some((account, _)) = set.legacy(id) {
        return Ok(account.clone());
    }
    if let Ok(entry) = set.route(id) {
        return set.primary(&entry.0, &entry.1)?.ok_or_else(|| {
            format!(
                "route `{id}` names no account and not exactly one account declares its \
                 endpoint origin {} (candidates: {}); name the account",
                endpoint_origin(&entry.0.endpoint),
                ids(&set.covering(&entry.0))
            )
        });
    }
    let mut known: Vec<&str> = set
        .accounts
        .iter()
        .map(|account| account.id.as_str())
        .collect();
    known.sort();
    Err(format!(
        "route `{id}` was not found and no account has that id; accounts: {}",
        known.join(", ")
    ))
}

/// The ids of the routes whose primary account is `account`, for listings.
pub fn routes_using(
    environment_dirs: &[PathBuf],
    account: &Account,
) -> Result<Vec<String>, String> {
    Ok(load_all_routes(environment_dirs)?
        .into_iter()
        .filter(|route| route.account == account.id)
        .map(|route| route.route)
        .collect())
}

fn not_found(id: &str, dirs: &[PathBuf]) -> String {
    let searched = dirs
        .iter()
        .map(|dir| dir.display().to_string())
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "route `{id}` was not found in {searched}; available: {:?}",
        available_route_ids(dirs)
    )
}

/// The route ids the directories hold, for the not-found message. Unreadable
/// directories contribute nothing: this only decorates an error that already fired.
fn available_route_ids(dirs: &[PathBuf]) -> Vec<String> {
    let mut ids: Vec<String> = dirs
        .iter()
        .filter_map(|dir| std::fs::read_dir(dir).ok())
        .flatten()
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.extension() == Some(OsStr::new("toml")))
        .filter_map(|path| path.file_stem().and_then(OsStr::to_str).map(str::to_string))
        .collect();
    ids.sort();
    ids.dedup();
    ids
}

/// Parse, validate and bind one route file. A route with an inline `[credential]`
/// binds its implicit account; any other binds through the accounts of
/// `<routes dir>/../accounts`. Every error names the file.
pub fn load_route(path: &Path) -> Result<RouteFile, String> {
    let route = load_route_toml(path)?;
    if let Some(account) = route.implicit_account(path) {
        let mut bound = route.bind(route.id.clone(), &account);
        bound.source = path.to_path_buf();
        return Ok(bound);
    }
    let dir = path.parent().unwrap_or(Path::new("."));
    RouteSet::from_layers(&[(dir.to_path_buf(), dir.join("../accounts"))])?.bind(&route.id)
}

/// Parse and validate one route file as written. Every error names the file.
pub fn load_route_toml(path: &Path) -> Result<RouteToml, String> {
    let name = |message: String| format!("{}: {message}", path.display());
    let stem = path
        .file_stem()
        .and_then(OsStr::to_str)
        .ok_or_else(|| name("the route file name is not UTF-8".into()))?;
    let text = std::fs::read_to_string(path).map_err(|error| name(error.to_string()))?;
    let mut route: RouteToml = toml::from_str(&text).map_err(|error| name(error.to_string()))?;
    rename_dialect(&route.adapter, &mut route.adapter_settings).map_err(name)?;
    route.validate(stem).map_err(name)?;
    Ok(route)
}

/// The adapters whose `[adapter_settings]` setting `dialect` was named `account`
/// before ADR-0139 §8.
const RENAMED_DIALECT: [&str; 2] = ["anthropic-messages", "openai-responses"];

/// ADR-0139 §8: `[adapter_settings] account` is read as `dialect`, so the provider
/// component is handed only `dialect`; both spellings together are an error.
fn rename_dialect(adapter: &str, settings: &mut Option<toml::Value>) -> Result<(), String> {
    let Some(toml::Value::Table(table)) = settings else {
        return Ok(());
    };
    if !RENAMED_DIALECT.contains(&adapter) || !table.contains_key("account") {
        return Ok(());
    }
    if table.contains_key("dialect") {
        return Err(
            "`[adapter_settings]` names both `dialect` and its old spelling `account`; keep \
             `dialect`"
                .into(),
        );
    }
    let value = table.remove("account").expect("checked above");
    table.insert("dialect".into(), value);
    Ok(())
}

/// The load warning for a route file that still spells `dialect` as `account`
/// (ADR-0139 §8), shown by `p1 env show`.
pub fn dialect_warning(route: &RouteFile) -> Option<String> {
    if !RENAMED_DIALECT.contains(&route.adapter.as_str()) {
        return None;
    }
    let text = std::fs::read_to_string(&route.source).ok()?;
    let table: toml::Table = toml::from_str(&text).ok()?;
    table
        .get("adapter_settings")?
        .get("account")
        .is_some()
        .then(|| {
            format!(
                "warning: {}: `[adapter_settings] account` is now spelled `dialect` (ADR-0139)",
                route.source.display()
            )
        })
}

fn load_route_tomls(dir: &Path) -> Result<Vec<(RouteToml, PathBuf)>, String> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => {
            return Err(format!(
                "cannot read the route directory {}: {error}",
                dir.display()
            ));
        }
    };
    let mut paths: Vec<PathBuf> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.extension() == Some(OsStr::new("toml")))
        .collect();
    paths.sort();
    paths
        .into_iter()
        .map(|path| load_route_toml(&path).map(|route| (route, path)))
        .collect()
}

/// The routes and accounts the host can see (ADR-0139 §4): per id, the first file in
/// directory order wins whole. An implicit account takes part in the same order as
/// the account files of its directory; one id defined twice in one directory is an
/// error.
struct RouteSet {
    routes: Vec<(RouteToml, PathBuf)>,
    accounts: Vec<Account>,
    routes_dirs: Vec<PathBuf>,
}

impl RouteSet {
    fn load(environment_dirs: &[PathBuf]) -> Result<Self, String> {
        let layers: Vec<(PathBuf, PathBuf)> = routes_dirs(environment_dirs)
            .into_iter()
            .zip(crate::accounts::accounts_dirs(environment_dirs))
            .collect();
        Self::from_layers(&layers)
    }

    /// `layers`: (routes dir, accounts dir), highest priority first.
    fn from_layers(layers: &[(PathBuf, PathBuf)]) -> Result<Self, String> {
        let mut routes: Vec<(RouteToml, PathBuf, usize)> = Vec::new();
        for (layer, (routes_dir, _)) in layers.iter().enumerate() {
            for (route, path) in load_route_tomls(routes_dir)? {
                if !routes.iter().any(|(seen, _, _)| seen.id == route.id) {
                    routes.push((route, path, layer));
                }
            }
        }
        let mut accounts: Vec<Account> = Vec::new();
        for (layer, (_, accounts_dir)) in layers.iter().enumerate() {
            let mut here = crate::accounts::load_accounts(accounts_dir)?;
            here.extend(
                routes
                    .iter()
                    .filter(|(_, _, from)| *from == layer)
                    .filter_map(|(route, path, _)| route.implicit_account(path)),
            );
            for (index, account) in here.iter().enumerate() {
                if let Some(other) = here[..index].iter().find(|other| other.id == account.id) {
                    return Err(format!(
                        "account `{}` is defined twice: {} and {}",
                        account.id,
                        other.source.display(),
                        account.source.display()
                    ));
                }
                // A legacy route id means one route with one account (ADR-0139 §6); across
                // directories the first one wins, as every definition does.
                for old in account.legacy_routes.keys() {
                    if let Some(other) = here[..index]
                        .iter()
                        .find(|other| other.legacy_routes.contains_key(old))
                    {
                        return Err(format!(
                            "the legacy route id `{old}` is claimed by account `{}` ({}) and \
                             account `{}` ({})",
                            other.id,
                            other.source.display(),
                            account.id,
                            account.source.display()
                        ));
                    }
                }
            }
            for account in here {
                match accounts.iter_mut().find(|seen| seen.id == account.id) {
                    // A user copy of a former per-account route shadows the converted account
                    // of the same id (ADR-0139 §6) and keeps its session origins on the
                    // canonical routes (§7, §9): the copy's sessions resume as before.
                    Some(seen) if seen.implicit_of.is_some() => {
                        for (route, origin) in account.legacy_origins {
                            seen.legacy_origins.entry(route).or_insert(origin);
                        }
                    }
                    Some(_) => {}
                    None => accounts.push(account),
                }
            }
        }
        // Two loaded account files never share a store entry and its origin approvals
        // (ADR-0139 §1); a shadowed file is not loaded. A route's inline credential may
        // share one, as `credential_route` and a user copy of a converted route do.
        for (index, account) in accounts.iter().enumerate() {
            if account.implicit_of.is_none()
                && let Some(other) = accounts[..index]
                    .iter()
                    .find(|other| other.implicit_of.is_none() && other.store_id == account.store_id)
            {
                return Err(format!(
                    "accounts `{}` ({}) and `{}` ({}) both use the store entry `{}`; give one \
                     its own `store_id`",
                    other.id,
                    other.source.display(),
                    account.id,
                    account.source.display(),
                    account.store_id
                ));
            }
        }
        routes.sort_by(|left, right| left.0.id.cmp(&right.0.id));
        Ok(Self {
            routes: routes
                .into_iter()
                .map(|(route, path, _)| (route, path))
                .collect(),
            accounts,
            routes_dirs: layers.iter().map(|(dir, _)| dir.clone()).collect(),
        })
    }

    fn route(&self, id: &str) -> Result<&(RouteToml, PathBuf), String> {
        self.routes
            .iter()
            .find(|(route, _)| route.id == id)
            .ok_or_else(|| not_found(id, &self.routes_dirs))
    }

    /// The account whose `[legacy_routes]` names `id`, and the route it now means. A
    /// route file with the old id itself wins, as any file does (ADR-0139 §6).
    fn legacy(&self, id: &str) -> Option<(&Account, &str)> {
        if self.routes.iter().any(|(route, _)| route.id == id) {
            return None;
        }
        // Accounts are in priority order: the first claim wins.
        self.accounts.iter().find_map(|account| {
            account
                .legacy_routes
                .get(id)
                .map(|route| (account, route.as_str()))
        })
    }

    /// `old` bound as the route it now means with its account; `named` is an account the
    /// key also names, which must be that one (ADR-0139 §3 rule 3).
    fn bind_legacy(
        &self,
        key: &str,
        old: &str,
        named: Option<&str>,
    ) -> Option<Result<RouteFile, String>> {
        let (account, route_id) = self.legacy(old)?;
        Some((|| {
            if let Some(named) = named
                && named != account.id
            {
                return Err(format!(
                    "route `{old}` is a legacy route id meaning `{route_id}` with account `{}`, \
                     but account `{named}` is named too; name the route `{route_id}`",
                    account.id
                ));
            }
            let entry = self.route(route_id)?;
            if !account.covers(&entry.0.endpoint) {
                return Err(format!(
                    "account `{}` maps the legacy route id `{old}` to route `{route_id}` but \
                     does not list its endpoint origin {}",
                    account.id,
                    endpoint_origin(&entry.0.endpoint)
                ));
            }
            Ok(self.bind_with(key.to_string(), entry, account))
        })())
    }

    fn covering(&self, route: &RouteToml) -> Vec<&Account> {
        self.accounts
            .iter()
            .filter(|account| account.covers(&route.endpoint))
            .collect()
    }

    fn named_account(&self, route: &RouteToml, id: &str) -> Result<&Account, String> {
        let account = self
            .accounts
            .iter()
            .find(|account| account.id == id)
            .ok_or_else(|| {
                format!(
                    "account `{id}` for route `{}` is neither an account file nor a route's \
                 inline `[credential]`; accounts that declare its origin: {}",
                    route.id,
                    ids(&self.covering(route))
                )
            })?;
        if !account.covers(&route.endpoint) {
            return Err(format!(
                "account `{id}` does not list the endpoint origin {} of route `{}` in its \
                 `origins` (it lists: {})",
                endpoint_origin(&route.endpoint),
                route.id,
                account.origins.join(", ")
            ));
        }
        Ok(account)
    }

    /// The route's primary account (ADR-0139 §3 rules 4–6), `None` when it names none
    /// and not exactly one account declares its origin.
    fn primary(&self, route: &RouteToml, path: &Path) -> Result<Option<Account>, String> {
        if route.credential.is_some() {
            // The implicit account takes part in first-file-wins (ADR-0139 §6): an account
            // file with the route's id in a higher directory replaces it.
            return Ok(self
                .accounts
                .iter()
                .find(|account| account.id == route.id)
                .cloned()
                .or_else(|| route.implicit_account(path)));
        }
        if let Some(id) = &route.account {
            return self.named_account(route, id).cloned().map(Some);
        }
        match self.covering(route).as_slice() {
            [only] => Ok(Some((*only).clone())),
            _ => Ok(None),
        }
    }

    fn bind_with(
        &self,
        key: String,
        (route, path): &(RouteToml, PathBuf),
        account: &Account,
    ) -> RouteFile {
        let mut bound = route.bind(key, account);
        bound.source = path.clone();
        bound
    }

    /// `<route id>` (its primary account) or `<route id>@<account id>`.
    fn bind(&self, key: &str) -> Result<RouteFile, String> {
        let (route_id, account_id) = match key.split_once('@') {
            Some((route, account)) => (route, Some(account)),
            None => (key, None),
        };
        let entry = match self.route(route_id) {
            Ok(entry) => entry,
            Err(missing) => {
                return match self.bind_legacy(key, route_id, account_id) {
                    Some(bound) => self.unique_origin(bound?),
                    None => Err(missing),
                };
            }
        };
        let account = match account_id {
            Some(id) => self.named_account(&entry.0, id)?.clone(),
            None => self.primary(&entry.0, &entry.1)?.ok_or_else(|| {
                format!(
                    "route `{route_id}` names no account and not exactly one account declares \
                     its endpoint origin {} (candidates: {}); name one with `account` in the \
                     environment or the route",
                    endpoint_origin(&entry.0.endpoint),
                    ids(&self.covering(&entry.0))
                )
            })?,
        };
        self.unique_origin(self.bind_with(key.to_string(), entry, &account))
    }

    /// ADR-0139 §7: two pairs that resolve to one replay origin string must have the
    /// same adapter, endpoint origin and store identity; otherwise `bound` fails, so
    /// a session can never resume across two wires or two credentials by accident.
    fn unique_origin(&self, bound: RouteFile) -> Result<RouteFile, String> {
        // Every pair that binds; a route that does not bind fails on its own, not here.
        for other in self.bindable_pairs() {
            if other.origin_route == bound.origin_route
                && (other.adapter != bound.adapter
                    || endpoint_origin(&other.endpoint) != endpoint_origin(&bound.endpoint)
                    || other.credential_route_id() != bound.credential_route_id())
            {
                return Err(format!(
                    "`{}` and `{}` both record the origin `{}` but differ in adapter, endpoint \
                     origin or store identity; give one its own `origin_route`",
                    bound.id, other.id, bound.origin_route
                ));
            }
        }
        Ok(bound)
    }

    fn primaries(&self) -> Result<Vec<RouteFile>, String> {
        let mut bound = Vec::new();
        for entry in &self.routes {
            if let Some(account) = self.primary(&entry.0, &entry.1)? {
                bound.push(self.bind_with(entry.0.id.clone(), entry, &account));
            }
        }
        Ok(bound)
    }

    fn pairs(&self) -> Result<Vec<RouteFile>, String> {
        self.pairs_with(self.primaries()?)
    }

    /// [`Self::pairs`] without the primaries that do not bind.
    fn bindable_pairs(&self) -> Vec<RouteFile> {
        let primaries = self
            .routes
            .iter()
            .filter_map(|entry| {
                let account = self.primary(&entry.0, &entry.1).ok()??;
                Some(self.bind_with(entry.0.id.clone(), entry, &account))
            })
            .collect();
        self.pairs_with(primaries).unwrap_or_default()
    }

    fn pairs_with(&self, primaries: Vec<RouteFile>) -> Result<Vec<RouteFile>, String> {
        let mut bound = primaries;
        for entry in &self.routes {
            for account in self.covering(&entry.0) {
                let key = format!("{}@{}", entry.0.id, account.id);
                bound.push(self.bind_with(key, entry, account));
            }
        }
        // Every legacy route id keeps its catalog key, alone and with its own account
        // named (an environment may name both, ADR-0139 §3 rule 3).
        for account in &self.accounts {
            for old in account.legacy_routes.keys() {
                for key in [old.clone(), format!("{old}@{}", account.id)] {
                    if let Some(Ok(route)) = self.bind_legacy(&key, old, Some(&account.id)) {
                        bound.push(route);
                    }
                }
            }
        }
        bound.sort_by(|left, right| left.id.cmp(&right.id));
        Ok(bound)
    }
}

fn ids(accounts: &[&Account]) -> String {
    if accounts.is_empty() {
        return "none".into();
    }
    accounts
        .iter()
        .map(|account| account.id.as_str())
        .collect::<Vec<_>>()
        .join(", ")
}

/// `[adapter_settings]` typed by the adapter that names it.
fn adapter_settings(adapter: &str, table: &Option<toml::Value>) -> Result<AdapterSettings, String> {
    fn typed<T: serde::de::DeserializeOwned>(table: &Option<toml::Value>) -> Result<T, String> {
        table
            .clone()
            .unwrap_or_else(|| toml::Value::Table(toml::Table::new()))
            .try_into::<T>()
            .map_err(|error| format!("invalid `[adapter_settings]`: {error}"))
    }
    match adapter {
        "openai-chat" => typed::<ChatAdapterSettings>(table).map(AdapterSettings::OpenAiChat),
        "anthropic-messages" => {
            typed::<MessagesAdapterSettings>(table).map(AdapterSettings::AnthropicMessages)
        }
        "openai-responses" => {
            typed::<ResponsesAdapterSettings>(table).map(AdapterSettings::OpenAiResponses)
        }
        other => Err(format!(
            "unknown adapter \"{other}\"; the known adapters are {}",
            known_adapters()
        )),
    }
}

fn known_adapters() -> String {
    ADAPTER_KEYS.join(", ")
}

/// A header name that must never appear in a route file: it could hold a secret by
/// accident (spec 1.2). The adapter rejects these names too, so such a file could
/// never build a provider anyway.
fn is_secret_header(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    name.starts_with("x-auth")
        || matches!(
            name.as_str(),
            "authorization" | "proxy-authorization" | "x-api-key" | "api-key" | "cookie"
        )
}

/// The adapter rejects content-type, accept, host, content-length and
/// transfer-encoding as static headers; those are protocol-level and set by the
/// transport, so a route file naming one is an error here rather than later.
fn is_header_name(name: &str) -> bool {
    !name.is_empty()
        && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
        && !matches!(
            name.to_ascii_lowercase().as_str(),
            "content-type" | "accept" | "host" | "content-length" | "transfer-encoding"
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    const RESERVED: [&str; 3] = [MODEL_PROFILE_KEY, ROUTE_HEADERS_KEY, MODEL_BINDING_KEY];

    fn repo(relative: &str) -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .join(relative)
    }

    #[test]
    fn retry_presets_are_explicit_and_other_shipped_routes_keep_the_default() {
        use p1_provider_http::RetryPolicy;
        use std::time::Duration;
        let selected = [
            "opencode-go-subscription",
            "opencode-go-1-subscription",
            "opencode-go-2-subscription",
            "opencode-go-3-subscription",
            "opencode-go-messages",
            "opencode-go-messages-1",
            "opencode-go-messages-2",
            "opencode-go-messages-3",
            "opencode-go-glm",
            "cline-pass-1",
            "cline-pass-2",
        ];
        // ADR-0139 §6: the old per-account route ids resolve to the converted routes.
        let environments = [repo("environments")];
        for id in selected {
            let route = load_route_by_id(&environments, id).unwrap();
            assert_eq!(route.retry_policy, RouteRetryPolicy::Deepseek, "{id}");
            let policy = route.retry_policy.resolve();
            assert_eq!(policy.max_retries, 5);
            assert_eq!(policy.base, Duration::from_millis(500));
            assert_eq!(policy.cap, Duration::from_secs(10));
            assert_eq!(policy.jitter, Duration::ZERO);
            assert_eq!(policy.jitter_percent, 10);
            assert_eq!(policy.retry_after_limit, Some(Duration::from_secs(10)));
        }
        // ADR-0145; Kimi joins GLM on the shared preset (#645 R7).
        let patient = ["glm-subscription", "kimi-coding-subscription"];
        for id in patient {
            let route = load_route_by_id(&environments, id).unwrap();
            assert_eq!(route.retry_policy, RouteRetryPolicy::Patient, "{id}");
            assert_eq!(
                route.retry_policy.resolve(),
                RetryPolicy {
                    max_retries: 8,
                    base: Duration::from_secs(2),
                    cap: Duration::from_secs(32),
                    jitter: Duration::ZERO,
                    jitter_percent: 10,
                    retry_after_limit: Some(Duration::from_secs(300)),
                }
            );
        }
        assert_eq!(RouteRetryPolicy::Default.resolve().max_retries, 3);
        let deepseek = [
            "opencode-go-subscription",
            "opencode-go-messages",
            "opencode-go-glm",
            "cline-pass",
        ];
        for route in load_routes(&repo("routes")).unwrap() {
            if !deepseek.contains(&route.route.as_str()) && !patient.contains(&route.route.as_str())
            {
                assert_eq!(
                    route.retry_policy.resolve(),
                    RetryPolicy::default(),
                    "{}",
                    route.id
                );
            }
        }
        let original = std::fs::read_to_string(repo("routes/cline-pass.toml")).unwrap();
        let omitted = original
            .lines()
            .filter(|line| !line.starts_with("retry_policy"))
            .collect::<Vec<_>>()
            .join("\n");
        let route: RouteToml = toml::from_str(&omitted).unwrap();
        assert_eq!(route.retry_policy.resolve(), RetryPolicy::default());
        assert!(
            toml::from_str::<RouteToml>(
                &original.replace("retry_policy = \"deepseek\"", "retry_policy = \"unknown\"")
            )
            .is_err()
        );
    }

    #[test]
    fn route_timeout_keys_validate_independently_and_preserve_omitted_defaults() {
        use std::time::Duration;
        let original = "id = 'test'\norigin_route = 'test'\nadapter = 'openai-chat'\nendpoint = 'https://provider.test/v1'\n[credential]\nkind = 'none'\n";
        let omitted: RouteFile = toml::from_str(original).unwrap();
        assert_eq!(
            omitted.stream_timeouts().first_byte,
            Duration::from_secs(120)
        );
        assert_eq!(omitted.stream_timeouts().idle, Duration::from_secs(300));
        for key in ["first_byte_timeout_secs", "stream_idle_timeout_secs"] {
            for seconds in [30, 480, 1800] {
                let route: RouteFile =
                    toml::from_str(&format!("{key} = {seconds}\n{original}")).unwrap();
                let bounds = route.stream_timeouts();
                if key == "first_byte_timeout_secs" {
                    assert_eq!(bounds.first_byte, Duration::from_secs(seconds));
                    assert_eq!(bounds.idle, Duration::from_secs(300));
                } else {
                    assert_eq!(bounds.first_byte, Duration::from_secs(120));
                    assert_eq!(bounds.idle, Duration::from_secs(seconds));
                }
            }
            for invalid in ["0", "29", "1801", "-1", "480.0", "'480'", "true", "[]"] {
                let error = toml::from_str::<RouteFile>(&format!("{key} = {invalid}\n{original}"))
                    .unwrap_err()
                    .to_string();
                assert!(error.contains(key), "{error}");
            }
        }
    }

    /// What a component does with the object: drop the reserved keys, then parse the
    /// rest as the settings type its adapter key selects.
    fn component_settings(
        adapter: &str,
        mut object: serde_json::Map<String, serde_json::Value>,
    ) -> Result<AdapterSettings, String> {
        for key in RESERVED {
            object.remove(key);
        }
        typed_from_json(adapter, serde_json::Value::Object(object))
    }

    fn typed_from_json(adapter: &str, value: serde_json::Value) -> Result<AdapterSettings, String> {
        let text = |error: serde_json::Error| error.to_string();
        match adapter {
            "openai-chat" => serde_json::from_value(value)
                .map(AdapterSettings::OpenAiChat)
                .map_err(text),
            "anthropic-messages" => serde_json::from_value(value)
                .map(AdapterSettings::AnthropicMessages)
                .map_err(text),
            "openai-responses" => serde_json::from_value(value)
                .map(AdapterSettings::OpenAiResponses)
                .map_err(text),
            other => Err(format!("unknown adapter {other}")),
        }
    }

    #[test]
    fn every_shipped_route_and_binding_yields_the_component_settings_object() {
        let routes = load_routes(&repo("routes")).expect("the shipped routes load");
        let mut adapters_seen: Vec<&str> = Vec::new();
        let mut bindings_seen = 0;
        for route in &routes {
            let native = route.settings().expect("a shipped route's settings parse");
            for (profile_id, binding) in &route.models {
                let profile_toml =
                    std::fs::read_to_string(repo(&format!("profiles/{profile_id}.toml")))
                        .unwrap_or_else(|error| {
                            panic!("{}: profile {profile_id}: {error}", route.id)
                        });
                let value = route.component_adapter_settings(binding, profile_id, &profile_toml);
                let serde_json::Value::Object(object) = value else {
                    panic!("{}: the settings are not a JSON object", route.id);
                };

                let profile = &object[MODEL_PROFILE_KEY];
                assert_eq!(
                    profile,
                    &serde_json::json!({ "stem": profile_id, "toml": profile_toml }),
                    "{}",
                    route.id
                );
                p1_model_profile::ModelProfile::from_toml(profile_id, &profile_toml)
                    .unwrap_or_else(|error| panic!("{}: {error}", route.id));

                let headers: BTreeMap<String, String> =
                    serde_json::from_value(object[ROUTE_HEADERS_KEY].clone())
                        .expect("route_headers is an object of strings");
                assert_eq!(headers, route.headers, "{}", route.id);

                let limits = object[MODEL_BINDING_KEY]
                    .as_object()
                    .expect("model_binding is an object");
                assert_eq!(
                    limits
                        .get("context_limit")
                        .and_then(serde_json::Value::as_u64),
                    binding.context_limit,
                    "{}",
                    route.id
                );
                assert_eq!(
                    limits
                        .get("output_limit")
                        .and_then(serde_json::Value::as_u64)
                        .map(|limit| u32::try_from(limit).expect("an output limit fits u32")),
                    binding.output_limit,
                    "{}",
                    route.id
                );
                assert!(
                    limits
                        .keys()
                        .all(|key| key == "context_limit" || key == "output_limit"),
                    "{}: {limits:?}",
                    route.id
                );

                let parsed = component_settings(&route.adapter, object)
                    .unwrap_or_else(|error| panic!("{}: {error}", route.id));
                assert_eq!(parsed, native, "{}", route.id);
                bindings_seen += 1;
            }
            if !adapters_seen.contains(&route.adapter.as_str()) {
                adapters_seen.push(route.adapter.as_str());
            }
        }
        adapters_seen.sort_unstable();
        let mut expected = ADAPTER_KEYS.to_vec();
        expected.sort_unstable();
        assert_eq!(adapters_seen, expected, "every adapter has a shipped route");
        assert!(bindings_seen > 0, "the shipped routes bind models");
    }

    #[test]
    fn no_adapter_settings_type_accepts_a_reserved_key() {
        for adapter in ADAPTER_KEYS {
            for key in RESERVED {
                let object = serde_json::json!({ key: {} });
                let error = typed_from_json(adapter, object)
                    .expect_err("a reserved key is not a settings field");
                assert!(
                    error.contains("unknown field") && error.contains(key),
                    "{adapter}/{key}: {error}"
                );
            }
        }
    }

    /// The settings as the adapter crates parse them: the validation `load_route` ran before
    /// S7.10-R4 moved it into this module. `Debug` is the comparison because the host's copies
    /// carry the adapters' type, field and variant names.
    fn adapter_crate_settings(route: &RouteFile) -> Result<String, String> {
        match route.adapter.as_str() {
            "openai-chat" => route
                .typed_settings::<p1_provider_openai_chat::ChatAdapterSettings>()
                .map(|settings| format!("{settings:?}")),
            "anthropic-messages" => route
                .typed_settings::<p1_provider_anthropic::MessagesAdapterSettings>()
                .map(|settings| format!("{settings:?}")),
            "openai-responses" => route
                .typed_settings::<p1_provider_openai::ResponsesAdapterSettings>()
                .map(|settings| format!("{settings:?}")),
            other => panic!("no adapter crate for {other}"),
        }
    }

    fn host_settings(route: &RouteFile) -> Result<String, String> {
        route.settings().map(|settings| match settings {
            AdapterSettings::OpenAiChat(settings) => format!("{settings:?}"),
            AdapterSettings::AnthropicMessages(settings) => format!("{settings:?}"),
            AdapterSettings::OpenAiResponses(settings) => format!("{settings:?}"),
        })
    }

    #[test]
    fn the_host_settings_agree_with_the_adapter_crates_on_every_shipped_route() {
        let routes = load_routes(&repo("routes")).expect("the shipped routes load");
        assert!(!routes.is_empty());
        for route in &routes {
            let old = adapter_crate_settings(route);
            assert!(old.is_ok(), "{}: {old:?}", route.id);
            assert_eq!(host_settings(route), old, "{}", route.id);
        }
    }

    /// `adapter_settings` values, one per adapter key, that the adapter crates accept or
    /// refuse: every field's wrong value, a wrong type, a missing required field, an unknown
    /// and a reserved key, a value that is no table and an absent table. `None` omits the key.
    const SETTINGS_CASES: &[(&str, Option<&str>)] = &[
        ("openai-chat", Some(r#"{ dialect = "retained-thinking" }"#)),
        (
            "openai-chat",
            Some(
                r#"{ dialect = "thinking-with-reasoning-alias", session_header = "x-session-affinity", client_identity = "opencode" }"#,
            ),
        ),
        ("openai-chat", Some(r#"{ dialect = "plain" }"#)),
        ("openai-chat", Some(r#"{ dialect = 1 }"#)),
        ("openai-chat", Some("{}")),
        ("openai-chat", None),
        (
            "openai-chat",
            Some(r#"{ dialect = "retained-thinking", client_identity = "claude-code" }"#),
        ),
        (
            "openai-chat",
            Some(r#"{ dialect = "retained-thinking", client_identity = 3 }"#),
        ),
        (
            "openai-chat",
            Some(r#"{ dialect = "retained-thinking", session_header = 5 }"#),
        ),
        (
            "openai-chat",
            Some(r#"{ dialect = "retained-thinking", max_tokens = 10 }"#),
        ),
        (
            "openai-chat",
            Some(r#"{ dialect = "retained-thinking", route_headers = {} }"#),
        ),
        ("openai-chat", Some(r#""retained-thinking""#)),
        (
            "anthropic-messages",
            Some(r#"{ account = "claude-code-subscription", long_context = true }"#),
        ),
        (
            "anthropic-messages",
            Some(r#"{ account = "claude-code-subscription" }"#),
        ),
        ("anthropic-messages", Some(r#"{ account = "api-key" }"#)),
        ("anthropic-messages", Some(r#"{ account = ["x"] }"#)),
        ("anthropic-messages", Some("{}")),
        ("anthropic-messages", None),
        (
            "anthropic-messages",
            Some(r#"{ account = "claude-code-subscription", long_context = "yes" }"#),
        ),
        (
            "anthropic-messages",
            Some(r#"{ account = "claude-code-subscription", headers = {} }"#),
        ),
        (
            "anthropic-messages",
            Some(r#"{ account = "claude-code-subscription", model_profile = {} }"#),
        ),
        ("anthropic-messages", Some("7")),
        (
            "openai-responses",
            Some(r#"{ account = "codex-subscription", transport = "websocket" }"#),
        ),
        (
            "openai-responses",
            Some(r#"{ account = "codex-subscription", transport = "sse" }"#),
        ),
        (
            "openai-responses",
            Some(r#"{ account = "codex-subscription" }"#),
        ),
        ("openai-responses", Some(r#"{ account = "plus" }"#)),
        ("openai-responses", Some("{}")),
        ("openai-responses", None),
        (
            "openai-responses",
            Some(r#"{ account = "codex-subscription", transport = "http" }"#),
        ),
        (
            "openai-responses",
            Some(r#"{ account = "codex-subscription", transport = "WebSocket" }"#),
        ),
        (
            "openai-responses",
            Some(r#"{ account = "codex-subscription", transport = true }"#),
        ),
        (
            "openai-responses",
            Some(r#"{ account = "codex-subscription", store = false }"#),
        ),
        (
            "openai-responses",
            Some(r#"{ account = "codex-subscription", model_binding = {} }"#),
        ),
        ("openai-responses", Some("[]")),
    ];

    fn case_route_text(adapter: &str, settings: Option<&str>) -> String {
        let settings = settings
            .map(|value| format!("adapter_settings = {value}\n"))
            .unwrap_or_default();
        format!(
            r#"id = "case"
origin_route = "{adapter}/case"
adapter = "{adapter}"
endpoint = "https://example.invalid/v1"
{settings}
[credential]
kind = "api-key"
env = "EXAMPLE_API_KEY"
borrow = []
store_only = true

[models."m"]
wire_model = "m"
"#
        )
    }

    #[test]
    fn the_host_settings_agree_with_the_adapter_crates_on_every_case() {
        let mut refused = 0;
        for (adapter, settings) in SETTINGS_CASES {
            let route: RouteFile = toml::from_str(&case_route_text(adapter, *settings))
                .unwrap_or_else(|error| panic!("{adapter} {settings:?}: {error}"));
            let old = adapter_crate_settings(&route);
            assert_eq!(host_settings(&route), old, "{adapter} {settings:?}");
            refused += usize::from(old.is_err());
        }
        assert!(refused >= 20, "the cases refuse: {refused}");
    }

    /// A malformed `[adapter_settings]` still fails when the route file LOADS, with the
    /// adapter crate's own words behind the file's name: the same stage and message as when
    /// `load_route` parsed the table with the adapter crate's type.
    #[test]
    fn a_malformed_route_fails_at_load_with_the_adapter_crates_error() {
        let scratch = tempfile::tempdir().expect("a scratch dir");
        let path = scratch.path().join("case.toml");
        for (adapter, settings) in SETTINGS_CASES {
            let text = case_route_text(adapter, *settings);
            std::fs::write(&path, &text).expect("the case file is written");
            let route: RouteFile = toml::from_str(&text).expect("the case parses");
            let expected = adapter_crate_settings(&route)
                .map(|_| ())
                .map_err(|error| format!("{}: {error}", path.display()));
            assert_eq!(
                load_route(&path).map(|_| ()),
                expected,
                "{adapter} {settings:?}"
            );
        }
    }

    #[test]
    fn route_headers_and_known_limits_travel_and_unknown_limits_stay_absent() {
        let route: RouteToml = toml::from_str(
            r#"
            id = "example"
            origin_route = "openai-chat/example"
            adapter = "openai-chat"
            endpoint = "https://example.invalid/v1/chat/completions"

            [credential]
            kind = "api-key"
            env = "EXAMPLE_API_KEY"
            borrow = []
            store_only = true

            [headers]
            x-title = "p1"

            [adapter_settings]
            dialect = "retained-thinking"

            [models."known"]
            wire_model = "known-wire"
            context_limit = 200000
            output_limit = 32000

            [models."unknown"]
            wire_model = "unknown-wire"
            "#,
        )
        .expect("the example route parses");
        route
            .validate("example")
            .expect("the example route is valid");
        let route = RouteFile::try_from(route).expect("the route has an inline credential");

        let known = route.component_adapter_settings(&route.models["known"], "known", "");
        assert_eq!(
            known[ROUTE_HEADERS_KEY],
            serde_json::json!({ "x-title": "p1" })
        );
        assert_eq!(
            known[MODEL_BINDING_KEY],
            serde_json::json!({ "context_limit": 200000, "output_limit": 32000 })
        );
        assert_eq!(known["dialect"], serde_json::json!("retained-thinking"));

        let unknown = route.component_adapter_settings(&route.models["unknown"], "unknown", "");
        assert_eq!(unknown[MODEL_BINDING_KEY], serde_json::json!({}));
    }

    #[test]
    fn a_route_without_adapter_settings_or_headers_still_carries_the_reserved_keys() {
        let mut route: RouteFile = toml::from_str(
            r#"
            id = "bare"
            origin_route = "openai-chat/bare"
            adapter = "openai-chat"
            endpoint = "https://example.invalid/v1/chat/completions"

            [credential]
            kind = "api-key"
            env = "EXAMPLE_API_KEY"
            borrow = []
            store_only = true

            [models."m"]
            wire_model = "m"
            "#,
        )
        .expect("the bare route parses");
        route.adapter_settings = None;
        let value = route.component_adapter_settings(&route.models["m"], "m", "id = \"m\"");
        let object = value.as_object().expect("an object");
        let mut keys: Vec<&str> = object.keys().map(String::as_str).collect();
        keys.sort_unstable();
        assert_eq!(keys, ["model_binding", "model_profile", "route_headers"]);
        assert_eq!(object[ROUTE_HEADERS_KEY], serde_json::json!({}));
    }

    /// Finding 10: a route that takes a shipped id keeps the shipped origin, whatever
    /// directory it comes from; `kind = "none"` and a new id are not held to it.
    #[test]
    fn a_shipped_route_id_cannot_send_its_credential_to_another_origin() {
        let shipped = shipped_origins_in(&[repo("routes")]);
        let anthropic = load_route(&repo("routes/anthropic-subscription.toml")).unwrap();
        assert!(check_shipped_origin(&anthropic, &shipped).is_ok());

        let mut moved = anthropic.clone();
        moved.endpoint = "https://attacker.example/v1".to_string();
        let error = check_shipped_origin(&moved, &shipped).unwrap_err();
        assert!(error.contains("https://attacker.example"), "{error}");
        assert!(error.contains("https://api.anthropic.com"), "{error}");
        // A longer path on the same origin is the same place.
        moved.endpoint = "HTTPS://API.anthropic.com/other/path".to_string();
        assert!(check_shipped_origin(&moved, &shipped).is_ok());
        // A different port or a userinfo trick is another origin.
        for endpoint in [
            "https://api.anthropic.com:8443",
            "https://api.anthropic.com@attacker.example",
            "http://api.anthropic.com",
        ] {
            moved.endpoint = endpoint.to_string();
            assert!(
                check_shipped_origin(&moved, &shipped).is_err(),
                "{endpoint}"
            );
        }

        let mut proxied = moved.clone();
        proxied.credential = serde_json::from_str(r#"{"kind":"none"}"#).unwrap();
        assert!(check_shipped_origin(&proxied, &shipped).is_ok());
        // A route and an account under new ids: nothing shipped is named (ADR-0139: the
        // shipped route binds an account file, so its store identity is renamed too).
        let mut renamed = moved;
        renamed.id = "my-own-route".to_string();
        renamed.route = "my-own-route".to_string();
        renamed.store_id = "my-own-account".to_string();
        assert!(check_shipped_origin(&renamed, &shipped).is_ok());
    }
}
