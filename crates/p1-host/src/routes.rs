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

/// One parsed `routes/<id>.toml`, validated. The host never interprets these fields
/// beyond routing: `[adapter_settings]` is handed to the adapter named by `adapter`
/// as-is (`docs/design/routes-and-profiles.md` §1.2).
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

/// The keys the host adds to a provider component's `adapter-settings` object, next to
/// the route file's own `[adapter_settings]` keys (the provider-components ADR). The
/// component removes them before it parses its settings type, which denies unknown
/// fields, so no adapter may ever name a settings field like one of these.
pub const MODEL_PROFILE_KEY: &str = "model_profile";
pub const ROUTE_HEADERS_KEY: &str = "route_headers";
pub const MODEL_BINDING_KEY: &str = "model_binding";

/// The `[adapter_settings]` table of one route, typed by the adapter that named it.
#[derive(Debug, Clone, PartialEq)]
pub enum AdapterSettings {
    OpenAiChat(p1_provider_openai_chat::ChatAdapterSettings),
    AnthropicMessages(p1_provider_anthropic::MessagesAdapterSettings),
    OpenAiResponses(p1_provider_openai::ResponsesAdapterSettings),
}

impl RouteFile {
    /// The settings the adapter named by `adapter` parses for itself. The host knows
    /// only which type an adapter key selects; the fields belong to the adapter, so
    /// an unknown key fails there, next to the code that would consume it.
    pub fn settings(&self) -> Result<AdapterSettings, String> {
        match self.adapter.as_str() {
            "openai-chat" => self
                .typed_settings::<p1_provider_openai_chat::ChatAdapterSettings>()
                .map(AdapterSettings::OpenAiChat),
            "anthropic-messages" => self
                .typed_settings::<p1_provider_anthropic::MessagesAdapterSettings>()
                .map(AdapterSettings::AnthropicMessages),
            "openai-responses" => self
                .typed_settings::<p1_provider_openai::ResponsesAdapterSettings>()
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
}
