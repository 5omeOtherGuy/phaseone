//! The Responses dialect port (ADR-0211): the wire facts one family of accounts requires, in
//! one trait the adapter asks instead of matching a closed list of accounts.
//!
//! The adapter's core dialect is [`ApiKey`]; [`dialects`] is the one place core dialects are
//! listed. Any other account's dialect is route data (ADR-0214): a route declares it as a table
//! (`[adapter_settings.dialect]`, [`Declared`]) that names it and gives its facts, as the
//! subscription connectors' route files declare the ChatGPT/Codex subscription.

/// One Responses dialect.
pub trait ResponsesDialect: Send + Sync + 'static {
    /// The spelling a route file uses (`dialect = "…"`, or a declared dialect's `name`).
    fn name(&self) -> &str;

    /// The responses path relative to the endpoint and the version segment before it, e.g.
    /// (`/v1/responses`, `/v1`).
    fn paths(&self) -> (&str, &str);

    /// The header that names the account, when the account needs one: a credential without
    /// that id is an authentication failure.
    fn account_id_header(&self) -> Option<&str> {
        None
    }

    /// Whether the wire takes an output-cap field; an explicit cap on one that does not is an
    /// error rather than a field the server would ignore.
    fn carries_output_cap(&self) -> bool {
        true
    }

    /// The client identity both transports send ahead of p1's own `User-Agent`, in order.
    fn client_headers(&self) -> &[(String, String)] {
        &[]
    }

    /// The headers an SSE request sends after the client identity and before the protocol's
    /// own, in order.
    fn sse_headers(&self) -> &[(String, String)] {
        &[]
    }

    /// The headers an SSE request carries its cache key in, besides the body's
    /// `prompt_cache_key`.
    fn session_headers(&self) -> &[String] {
        &[]
    }
}

/// The public Responses API with an OpenAI API key (ADR-0197).
pub(crate) struct ApiKey;

impl ResponsesDialect for ApiKey {
    fn name(&self) -> &str {
        "api-key"
    }

    fn paths(&self) -> (&str, &str) {
        ("/v1/responses", "/v1")
    }
}

/// Every core Responses dialect. The one place they are listed; nothing registers itself, and
/// every other dialect is [`Declared`] by the route that uses it.
pub fn dialects() -> Vec<&'static dyn ResponsesDialect> {
    vec![&ApiKey]
}

/// A dialect a route declares (ADR-0214): `[adapter_settings.dialect]` with its name and
/// facts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Declared {
    name: String,
    path: String,
    account_id_header: Option<String>,
    output_cap: bool,
    client_headers: Vec<(String, String)>,
    sse_headers: Vec<(String, String)>,
    session_headers: Vec<String>,
}

/// The headers the adapter itself sets, which a declared dialect may not set again.
const ADAPTER_HEADERS: [&str; 8] = [
    "authorization",
    "user-agent",
    "content-type",
    "accept",
    "session-id",
    "x-client-request-id",
    "host",
    "content-length",
];

impl Declared {
    fn parse(table: DeclaredToml) -> Result<Self, String> {
        let name = &table.name;
        let token = |text: &str| {
            !text.is_empty()
                && text
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b))
        };
        if !token(name) || dialects().iter().any(|core| core.name() == name) {
            return Err(format!(
                "a declared dialect needs a name of its own, not `{name}`"
            ));
        }
        let segments = table.path.strip_prefix('/').map(|rest| rest.split('/'));
        if !table.path.ends_with("/responses")
            || segments.is_none_or(|mut parts| {
                parts.any(|part| {
                    part.is_empty()
                        || !part
                            .bytes()
                            .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
                })
            })
        {
            return Err(format!(
                "dialect `{name}`: path `{}` is not a `/…/responses` path",
                table.path
            ));
        }
        let mut names = std::collections::BTreeSet::new();
        let mut header = |header: &str, value: Option<&str>| {
            token(header)
                && !ADAPTER_HEADERS.contains(&header.to_ascii_lowercase().as_str())
                && names.insert(header.to_ascii_lowercase())
                && value.is_none_or(|value| {
                    !value.is_empty() && value.bytes().all(|b| (32..=126).contains(&b))
                })
        };
        for (header_name, value) in table.client_headers.iter().chain(&table.sse_headers) {
            if !header(header_name, Some(value)) {
                return Err(format!(
                    "dialect `{name}`: header `{header_name}` is not one a dialect may send"
                ));
            }
        }
        for header_name in table
            .session_headers
            .iter()
            .chain(table.account_id_header.as_ref())
        {
            if !header(header_name, None) {
                return Err(format!(
                    "dialect `{name}`: header `{header_name}` is not one a dialect may send"
                ));
            }
        }
        Ok(Self {
            name: table.name,
            path: table.path,
            account_id_header: table.account_id_header,
            output_cap: table.output_cap,
            client_headers: table.client_headers,
            sse_headers: table.sse_headers,
            session_headers: table.session_headers,
        })
    }
}

impl ResponsesDialect for Declared {
    fn name(&self) -> &str {
        &self.name
    }

    fn paths(&self) -> (&str, &str) {
        let parent = self
            .path
            .strip_suffix("/responses")
            .expect("a declared path ends in /responses");
        (&self.path, parent)
    }

    fn account_id_header(&self) -> Option<&str> {
        self.account_id_header.as_deref()
    }

    fn carries_output_cap(&self) -> bool {
        self.output_cap
    }

    fn client_headers(&self) -> &[(String, String)] {
        &self.client_headers
    }

    fn sse_headers(&self) -> &[(String, String)] {
        &self.sse_headers
    }

    fn session_headers(&self) -> &[String] {
        &self.session_headers
    }
}

/// `[adapter_settings.dialect]` as a route file writes it.
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct DeclaredToml {
    name: String,
    path: String,
    #[serde(default)]
    account_id_header: Option<String>,
    #[serde(default = "output_cap")]
    output_cap: bool,
    #[serde(default)]
    client_headers: Vec<(String, String)>,
    #[serde(default)]
    sse_headers: Vec<(String, String)>,
    #[serde(default)]
    session_headers: Vec<String>,
}

fn output_cap() -> bool {
    true
}

impl TryFrom<DeclaredToml> for Declared {
    type Error = String;

    fn try_from(table: DeclaredToml) -> Result<Self, String> {
        Self::parse(table)
    }
}

/// A declared dialect with the ChatGPT/Codex subscription's facts, as the subscription
/// connectors' `openai-codex-subscription` route declares them, so this crate's own cases
/// cover every part of a declared dialect.
#[cfg(test)]
pub(crate) fn codex_like() -> crate::ResponsesAccount {
    serde_json::from_value(serde_json::json!({
        "name": "codex-subscription",
        "path": "/codex/responses",
        "account_id_header": "chatgpt-account-id",
        "output_cap": false,
        "client_headers": [["originator", "p1"]],
        "sse_headers": [["OpenAI-Beta", "responses=experimental"]],
        "session_headers": ["session_id", "conversation_id"],
    }))
    .expect("the declared dialect")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ResponsesAccount, ResponsesAdapterSettings};

    #[test]
    fn the_core_lists_its_dialect_and_a_route_names_or_declares_one() {
        let names: Vec<&str> = dialects().iter().map(|dialect| dialect.name()).collect();
        assert_eq!(names, ["api-key"]);
        let settings: ResponsesAdapterSettings =
            serde_json::from_str(r#"{"dialect":"api-key"}"#).expect("the core dialect");
        assert_eq!(settings.account, ResponsesAccount::API_KEY);
        let error = serde_json::from_str::<ResponsesAdapterSettings>(r#"{"dialect":"pigeon"}"#)
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("pigeon") && error.contains("api-key"),
            "{error}"
        );
        let codex = codex_like();
        assert_eq!(codex.dialect().paths(), ("/codex/responses", "/codex"));
        assert_eq!(codex.account_id_header(), Some("chatgpt-account-id"));
        for refused in [
            r#"{"name":"api-key","path":"/v1/responses"}"#,
            r#"{"name":"own","path":"/v1/chat"}"#,
            r#"{"name":"own","path":"/v1/responses","client_headers":[["User-Agent","x"]]}"#,
            r#"{"name":"own","path":"/v1/responses","session_headers":["a b"]}"#,
            r#"{"name":"own","path":"/v1/responses","pigeon":true}"#,
        ] {
            let text = format!(r#"{{"dialect":{refused}}}"#);
            assert!(
                serde_json::from_str::<ResponsesAdapterSettings>(&text).is_err(),
                "{refused}"
            );
        }
    }
}
