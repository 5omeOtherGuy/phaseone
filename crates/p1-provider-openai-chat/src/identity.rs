//! A chat client identity (ADR-0211, ADR-0214): a non-secret identity a route presents to a
//! vendor gateway that gates its free tier on the caller looking like the vendor's own client.
//!
//! The core has none. An identity is route data: a route declares it as a table
//! (`[adapter_settings.client_identity]`) that names it and gives its static headers and the
//! ids it derives from a request's cache key, as the subscription connectors' `opencode-zen`
//! route declares the OpenCode CLI's.

/// One client identity. It changes no message encoding and declares no tool: it only adds
/// its static headers and the ids it derives for a request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClientIdentity {
    name: String,
    headers: Vec<(String, String)>,
    ids: Vec<DerivedId>,
}

/// A header whose value is an id derived from the request's cache key: `prefix` and 26
/// lowercase hex digits of the key's 32-digit digest, from digit `from`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DerivedId {
    header: String,
    prefix: String,
    #[serde(default)]
    from: usize,
}

/// How many digest digits an id carries.
const ID_DIGITS: usize = 26;
/// How many digits the digest has.
const DIGEST_DIGITS: usize = 32;

impl ClientIdentity {
    /// The spelling a route file uses.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The header names the identity owns: a route may not send a second value for one of
    /// them. Its `user-agent` replaces p1's own instead.
    pub fn owned_headers(&self) -> impl Iterator<Item = &str> {
        self.headers
            .iter()
            .map(|(name, _)| name.as_str())
            .chain(self.ids.iter().map(|id| id.header.as_str()))
            .filter(|name| !name.eq_ignore_ascii_case("user-agent"))
    }

    /// The headers the identity adds to a request: its static, non-secret values, then the
    /// ids derived from the request's cache key (fresh ones without a key).
    pub fn headers(&self, cache_key: Option<&str>) -> Vec<(String, String)> {
        let mut headers = self.headers.clone();
        if !self.ids.is_empty() {
            let digest = digest(cache_key);
            headers.extend(self.ids.iter().map(|id| {
                (
                    id.header.clone(),
                    format!("{}{}", id.prefix, &digest[id.from..id.from + ID_DIGITS]),
                )
            }));
        }
        headers
    }
}

impl<'de> serde::Deserialize<'de> for ClientIdentity {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(serde::Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Declared {
            name: String,
            #[serde(default)]
            headers: Vec<(String, String)>,
            #[serde(default)]
            ids: Vec<DerivedId>,
        }
        let declared = match <serde_json::Value as serde::Deserialize>::deserialize(deserializer)? {
            table @ serde_json::Value::Object(_) => {
                serde_json::from_value::<Declared>(table).map_err(serde::de::Error::custom)?
            }
            serde_json::Value::String(name) => {
                return Err(serde::de::Error::custom(format!(
                    "unknown client identity `{name}`: the core has none, so a route declares \
                     its identity as a table (`name`, `headers`, `ids`), as \
                     connectors/opencode/routes/opencode-zen.toml declares `opencode`"
                )));
            }
            _ => {
                return Err(serde::de::Error::custom(
                    "a client identity is a declared table (`name`, `headers`, `ids`)",
                ));
            }
        };
        let token = |text: &str| {
            !text.is_empty()
                && text
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b))
        };
        if !token(&declared.name) {
            return Err(serde::de::Error::custom(format!(
                "client identity name `{}` is not a token",
                declared.name
            )));
        }
        let mut names = std::collections::BTreeSet::new();
        // The adapter's own protocol headers; the broker's and the secret-shaped names are
        // `p1_provider_http::declarable_header`'s.
        let reserved = ["content-type", "accept"];
        let mut header = |name: &str| {
            p1_provider_http::declarable_header(name)
                && !reserved.contains(&name.to_ascii_lowercase().as_str())
                && names.insert(name.to_ascii_lowercase())
        };
        for (name, value) in &declared.headers {
            if !header(name) || !p1_provider_http::declarable_header_value(value) {
                return Err(serde::de::Error::custom(format!(
                    "client identity `{}`: header `{name}` is not one an identity may send",
                    declared.name
                )));
            }
        }
        for id in &declared.ids {
            if !header(&id.header)
                || id.header.eq_ignore_ascii_case("user-agent")
                || id.from + ID_DIGITS > DIGEST_DIGITS
                || !id
                    .prefix
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'_')
            {
                return Err(serde::de::Error::custom(format!(
                    "client identity `{}`: id header `{}` is not one an identity may derive",
                    declared.name, id.header
                )));
            }
        }
        Ok(Self {
            name: declared.name,
            headers: declared.headers,
            ids: declared.ids,
        })
    }
}

/// The 32 lowercase hex digits a request's ids are cut from. OpenCode shapes its `ses_`/`msg_`
/// ids as `<prefix>_<12 lowercase hex><14 alnum>` (its `Identifier.ascending`), and the Zen
/// gateway's free-tier gate accepts exactly that shape and rejects a short or non-hex-prefixed
/// id with 403. p1 derives the digits from the route's cache key — a pure function, so a
/// resumed session keeps its ids. Without a key, a per-request nonce does the same job.
fn digest(cache_key: Option<&str>) -> String {
    use std::hash::{Hash, Hasher};
    use std::sync::atomic::{AtomicU64, Ordering};
    static NONCE: AtomicU64 = AtomicU64::new(0);
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    match cache_key {
        Some(key) => key.hash(&mut hasher),
        None => {
            // A provider component has no clock import (its capability allocation), and
            // reading one traps, so the component's ids rest on the instance's counter
            // alone; its instance lives for the provider, so the ids stay fresh per request.
            #[cfg(not(target_family = "wasm"))]
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|elapsed| elapsed.as_nanos() as u64)
                .unwrap_or(0)
                .hash(&mut hasher);
            NONCE.fetch_add(1, Ordering::Relaxed).hash(&mut hasher);
        }
    }
    let first = hasher.finish();
    let mut second = std::collections::hash_map::DefaultHasher::new();
    first.hash(&mut second);
    format!("{first:016x}{:016x}", second.finish())
}

/// The OpenCode CLI's identity at the OpenCode Zen gateway, as the subscription connectors'
/// `opencode-zen` route declares it, so this crate's own cases cover a declared identity.
#[cfg(test)]
pub(crate) fn opencode_like() -> ClientIdentity {
    serde_json::from_value(serde_json::json!({
        "name": "opencode",
        "headers": [
            ["user-agent", "opencode/1.18.31 ai-sdk/provider-utils/4.0.23 runtime/bun/1.3.14"],
            ["x-opencode-client", "cli"],
            ["x-opencode-project", "global"],
        ],
        "ids": [
            {"header": "x-opencode-session", "prefix": "ses_", "from": 0},
            {"header": "x-opencode-request", "prefix": "msg_", "from": 6},
        ],
    }))
    .expect("the declared identity")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ChatAdapterSettings;

    #[test]
    fn a_route_declares_its_identity_and_names_none() {
        let settings: ChatAdapterSettings = serde_json::from_value(serde_json::json!({
            "dialect": "effort-level",
            "client_identity": {"name": "acme", "headers": [["x-acme", "cli"]],
                "ids": [{"header": "x-acme-session", "prefix": "s_"}]},
        }))
        .expect("a declared identity");
        let identity = settings.client_identity.expect("the identity");
        assert_eq!(identity.name(), "acme");
        assert_eq!(
            identity.owned_headers().collect::<Vec<_>>(),
            ["x-acme", "x-acme-session"]
        );
        let headers = identity.headers(Some("key"));
        assert_eq!(
            headers,
            identity.headers(Some("key")),
            "a key's ids are stable"
        );
        assert!(headers[1].1.starts_with("s_") && headers[1].1.len() == 2 + ID_DIGITS);
        let opencode = opencode_like();
        assert_eq!(
            opencode.owned_headers().collect::<Vec<_>>(),
            [
                "x-opencode-client",
                "x-opencode-project",
                "x-opencode-session",
                "x-opencode-request"
            ]
        );
        for refused in [
            serde_json::json!("opencode"),
            serde_json::json!({"name": "a", "headers": [["authorization", "x"]]}),
            serde_json::json!({"name": "a", "headers": [["x-api-key", "x"]]}),
            serde_json::json!({"name": "a", "headers": [["x-forwarded-for", "x"]]}),
            serde_json::json!({"name": "a", "ids": [{"header": "x-id", "prefix": "p", "from": 7}]}),
            serde_json::json!({"name": "a", "pigeon": true}),
        ] {
            let error = serde_json::from_value::<ChatAdapterSettings>(serde_json::json!({
                "dialect": "effort-level",
                "client_identity": refused,
            }));
            assert!(error.is_err(), "{refused}");
        }
    }
}
