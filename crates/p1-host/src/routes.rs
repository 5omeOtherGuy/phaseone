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

use p1_auth::CredentialSpec;
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

/// One parsed `routes/<id>.toml`, validated. The host interprets these fields only to
/// route: `[adapter_settings]` is checked when the file loads against the host's copy
/// of the settings type of the adapter named by `adapter` ([`RouteFile::settings`]),
/// and the table itself reaches the component that serves the route as-is
/// (`docs/design/routes-and-profiles.md` §1.2).
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RouteFile {
    /// Must equal the file stem: the key an environment's `route` names.
    pub id: String,
    /// `Origin.route`, explicit in the file so it cannot drift from the route id.
    pub origin_route: String,
    /// A compiled adapter key ([`ADAPTER_KEYS`]).
    pub adapter: String,
    pub endpoint: String,
    pub credential: CredentialSpec,
    /// Native transport policy, never part of the component's adapter settings.
    #[serde(default)]
    pub retry_policy: RouteRetryPolicy,
    /// Static, non-secret headers. Authentication comes exclusively from
    /// `[credential]`, so a secret-looking name here is a load error.
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    /// Kept as an uninterpreted table; [`RouteFile::settings`] types it.
    #[serde(default)]
    pub adapter_settings: Option<toml::Value>,
    /// Profile id -> binding. A profile without an entry is NOT served by this route.
    #[serde(default)]
    pub models: BTreeMap<String, ModelBinding>,
}

/// Route-scoped retry presets (ADR-0137); omission preserves existing behavior.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RouteRetryPolicy {
    #[default]
    Default,
    Deepseek,
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
}

/// `anthropic-messages`' `[adapter_settings]` (`p1_provider_anthropic::MessagesAdapterSettings`).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MessagesAdapterSettings {
    pub account: MessagesAccount,
    #[serde(default)]
    pub long_context: bool,
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
    /// The settings the adapter named by `adapter` takes, checked against the adapter's
    /// own fields: an unknown key or value fails when the route loads, before any
    /// provider component parses the same table for a request.
    pub fn settings(&self) -> Result<AdapterSettings, String> {
        match self.adapter.as_str() {
            "openai-chat" => self
                .typed_settings::<ChatAdapterSettings>()
                .map(AdapterSettings::OpenAiChat),
            "anthropic-messages" => self
                .typed_settings::<MessagesAdapterSettings>()
                .map(AdapterSettings::AnthropicMessages),
            "openai-responses" => self
                .typed_settings::<ResponsesAdapterSettings>()
                .map(AdapterSettings::OpenAiResponses),
            other => Err(format!(
                "unknown adapter \"{other}\"; the known adapters are {}",
                known_adapters()
            )),
        }
    }

    fn typed_settings<T: serde::de::DeserializeOwned>(&self) -> Result<T, String> {
        let table = self
            .adapter_settings
            .clone()
            .unwrap_or_else(|| toml::Value::Table(toml::Table::new()));
        table
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

    fn validate(&self, stem: &str) -> Result<(), String> {
        if self.id != stem {
            return Err(format!(
                "route id \"{}\" must equal the file stem \"{stem}\"",
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
        self.credential.validate()?;
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
        self.settings()?;
        Ok(())
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
    let Some(origins) = shipped.get(&route.id) else {
        return Ok(());
    };
    let origin = endpoint_origin(&route.endpoint);
    if origins.contains(&origin) {
        return Ok(());
    }
    Err(format!(
        "route `{}` overrides a route p1 ships but sends its credential to {origin} instead \
         of {}; give the route file a new id to use another endpoint",
        route.id,
        origins.join(" or ")
    ))
}

/// Bind credential sources as well as shipped ids before resolving any credential
/// (ADR-0110). Origin metadata is read separately from the credential file.
pub fn check_credential_origin(
    route: &RouteFile,
    locations: &p1_auth::Locations,
) -> Result<(), String> {
    use p1_auth::CredentialKind;
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
    // Shipped ids have already been bound to their compiled origin above.
    if SHIPPED_ROUTES.iter().any(|&(id, _, _)| id == route.id) {
        return Ok(());
    }
    if p1_auth::store::endpoint_origin(&route.id, locations)?.as_deref() == Some(&origin) {
        return Ok(());
    }
    Err(format!(
        "route `{}` cannot send its credential to untrusted endpoint {origin}; \
         run `p1 login {}` to store a key with this origin, or \
         `p1 login {} --trust-endpoint` to trust it for an environment key",
        route.id, route.id, route.id
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
    let required = !SHIPPED_ROUTES.iter().any(|&(id, _, _)| id == route.id)
        && (route.credential.kind == CredentialKind::ApiKey || route.credential.store_only);
    (Some(endpoint_origin(&route.endpoint)), required)
}

/// Shared inspection wording. Refusal probes metadata only, never key variables
/// or credential documents. All inspection commands use this gate.
pub(crate) fn credential_description(route: &RouteFile, locations: &p1_auth::Locations) -> String {
    let line = match check_credential_origin(route, locations) {
        Ok(()) => p1_auth::describe(&route.id, &route.credential, locations).line(),
        Err(reason) => format!(
            "none — endpoint origin {} is not approved: {reason}; run `p1 login {}` \
             or `p1 login {} --trust-endpoint`{}",
            endpoint_origin(&route.endpoint),
            route.id,
            route.id,
            if route.credential.store_only {
                p1_auth::CredentialPolicy::StoreOnly.marker()
            } else {
                ""
            },
        ),
    };
    p1_redact::redact(&line).text
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

/// Every `*.toml` in `dir`, sorted by file name, parsed and validated. A directory
/// that does not exist holds no routes: an environment naming a route that is not
/// there fails at assembly, where the error can list what exists.
pub fn load_routes(dir: &Path) -> Result<Vec<RouteFile>, String> {
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
    paths.iter().map(|path| load_route(path)).collect()
}

/// Every route file the host can see, highest-priority directory first: an id found
/// in more than one directory resolves to the first one, exactly like an environment
/// or a profile. Sorted by id, so the catalog registers them in a stable order.
pub fn load_all_routes(environment_dirs: &[PathBuf]) -> Result<Vec<RouteFile>, String> {
    let mut routes: Vec<RouteFile> = Vec::new();
    for dir in routes_dirs(environment_dirs) {
        for route in load_routes(&dir)? {
            if !routes.iter().any(|seen| seen.id == route.id) {
                routes.push(route);
            }
        }
    }
    routes.sort_by(|left, right| left.id.cmp(&right.id));
    Ok(routes)
}

/// The one route file an environment's `route` names, in the host's search order.
/// A missing file is an error that lists the ids the directories do hold, like the
/// profile lookup's; the host reports it before it builds a provider (spec §2).
pub fn load_route_by_id(environment_dirs: &[PathBuf], id: &str) -> Result<RouteFile, String> {
    let dirs = routes_dirs(environment_dirs);
    for dir in &dirs {
        let path = dir.join(format!("{id}.toml"));
        if path.is_file() {
            return load_route(&path);
        }
    }
    let searched = dirs
        .iter()
        .map(|dir| dir.display().to_string())
        .collect::<Vec<_>>()
        .join(", ");
    Err(format!(
        "route `{id}` was not found in {searched}; available: {:?}",
        available_route_ids(&dirs)
    ))
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

/// Parse and validate one route file. Every error names the file.
pub fn load_route(path: &Path) -> Result<RouteFile, String> {
    let name = |message: String| format!("{}: {message}", path.display());
    let stem = path
        .file_stem()
        .and_then(OsStr::to_str)
        .ok_or_else(|| name("the route file name is not UTF-8".into()))?;
    let text = std::fs::read_to_string(path).map_err(|error| name(error.to_string()))?;
    let route: RouteFile = toml::from_str(&text).map_err(|error| name(error.to_string()))?;
    route.validate(stem).map_err(name)?;
    Ok(route)
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
            "cline-pass-1",
            "cline-pass-2",
        ];
        let routes = load_routes(&repo("routes")).unwrap();
        let mut seen = 0;
        for route in routes {
            if selected.contains(&route.id.as_str()) {
                seen += 1;
                assert_eq!(route.retry_policy, RouteRetryPolicy::Deepseek);
                let policy = route.retry_policy.resolve();
                assert_eq!(policy.max_retries, 5);
                assert_eq!(policy.base, Duration::from_millis(500));
                assert_eq!(policy.cap, Duration::from_secs(10));
                assert_eq!(policy.jitter, Duration::ZERO);
                assert_eq!(policy.jitter_percent, 10);
                assert_eq!(policy.retry_after_limit, Some(Duration::from_secs(10)));
            } else {
                assert_eq!(
                    route.retry_policy.resolve(),
                    RetryPolicy::default(),
                    "{}",
                    route.id
                );
            }
        }
        assert_eq!(seen, selected.len());
        let original = std::fs::read_to_string(repo("routes/cline-pass-1.toml")).unwrap();
        let omitted = original
            .lines()
            .filter(|line| !line.starts_with("retry_policy"))
            .collect::<Vec<_>>()
            .join("\n");
        let route: RouteFile = toml::from_str(&omitted).unwrap();
        assert_eq!(route.retry_policy.resolve(), RetryPolicy::default());
        assert!(
            toml::from_str::<RouteFile>(
                &original.replace("retry_policy = \"deepseek\"", "retry_policy = \"unknown\"")
            )
            .is_err()
        );
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
        let route: RouteFile = toml::from_str(
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
        let mut renamed = moved;
        renamed.id = "my-own-route".to_string();
        assert!(check_shipped_origin(&renamed, &shipped).is_ok());
    }
}
