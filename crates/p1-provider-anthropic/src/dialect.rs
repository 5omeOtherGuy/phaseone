//! The Messages dialect port (ADR-0211): the wire facts one family of accounts requires, in
//! one trait the adapter asks instead of matching a closed list of accounts.
//!
//! The adapter's core dialects are [`ApiKey`], [`OpencodeGo`] and [`Zai`]; [`dialects`] is the
//! one place they are listed. Any other account's dialect is route data (ADR-0214): a route
//! declares it as a table (`[adapter_settings.dialect]`, [`Declared`]) that names it and gives
//! its facts, as the subscription connectors' route files declare the Claude Code subscription.

/// One Messages dialect.
pub trait MessagesDialect: Send + Sync + 'static {
    /// The spelling a route file uses (`dialect = "…"`, or a declared dialect's `name`).
    fn name(&self) -> &str;

    /// Whether the account reaches Claude models, so the Claude lowering, the prompt-cache
    /// markers and the Claude context beta apply.
    fn is_claude(&self) -> bool;

    /// The header the broker also places the key in, or `None` for a bearer alone.
    fn credential_header(&self) -> Option<&str>;

    /// The block the account requires first in `system`; empty for none.
    fn identity(&self) -> &str {
        ""
    }

    /// The betas every request on the account carries, comma-separated; empty for none.
    fn base_beta(&self) -> &str {
        ""
    }

    /// The client headers a Claude account's requests carry after the protocol's own, in
    /// order; none for most.
    fn client_headers(&self) -> &[(String, String)] {
        &[]
    }
}

/// The public Messages API with an Anthropic API key (ADR-0197).
pub(crate) struct ApiKey;
/// OpenCode Go's DeepSeek Messages endpoint.
pub(crate) struct OpencodeGo;
/// Z.ai's GLM Messages endpoint.
pub(crate) struct Zai;

impl MessagesDialect for ApiKey {
    fn name(&self) -> &str {
        "api-key"
    }

    fn is_claude(&self) -> bool {
        true
    }

    fn credential_header(&self) -> Option<&str> {
        Some("x-api-key")
    }
}

impl MessagesDialect for OpencodeGo {
    fn name(&self) -> &str {
        "opencode-go"
    }

    fn is_claude(&self) -> bool {
        false
    }

    fn credential_header(&self) -> Option<&str> {
        Some("x-api-key")
    }
}

impl MessagesDialect for Zai {
    fn name(&self) -> &str {
        "zai"
    }

    fn is_claude(&self) -> bool {
        false
    }

    fn credential_header(&self) -> Option<&str> {
        Some("x-api-key")
    }
}

/// Every core Messages dialect. The one place they are listed; nothing registers itself, and
/// every other dialect is [`Declared`] by the route that uses it.
pub fn dialects() -> Vec<&'static dyn MessagesDialect> {
    vec![&OpencodeGo, &Zai, &ApiKey]
}

/// A dialect a route declares (ADR-0214): `[adapter_settings.dialect]` with its name and
/// facts. Only a Claude account carries an identity block, betas or client headers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Declared {
    name: String,
    claude: bool,
    credential_header: Option<String>,
    identity: String,
    base_beta: String,
    client_headers: Vec<(String, String)>,
}

/// The headers the adapter itself sets, which a declared dialect may not set again.
const ADAPTER_HEADERS: [&str; 7] = [
    "content-type",
    "accept",
    "anthropic-version",
    "user-agent",
    "authorization",
    "x-api-key",
    "anthropic-beta",
];

impl Declared {
    fn parse(table: DeclaredToml) -> Result<Self, String> {
        let token = |text: &str| {
            !text.is_empty()
                && text
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b))
        };
        if !token(&table.name) || dialects().iter().any(|core| core.name() == table.name) {
            return Err(format!(
                "a declared dialect needs a name of its own, not `{}`",
                table.name
            ));
        }
        if !table.claude
            && (!table.identity.is_empty()
                || !table.betas.is_empty()
                || !table.client_headers.is_empty())
        {
            return Err(format!(
                "dialect `{}`: only a Claude account (`claude = true`) carries an identity, betas or client headers",
                table.name
            ));
        }
        if let Some(header) = &table.credential_header
            && !token(header)
        {
            return Err(format!(
                "dialect `{}`: credential_header `{header}` is not a header name",
                table.name
            ));
        }
        if table.identity.chars().any(char::is_control) {
            return Err(format!(
                "dialect `{}`: the identity block is one line of text",
                table.name
            ));
        }
        if let Some(beta) = table.betas.iter().find(|beta| !token(beta)) {
            return Err(format!(
                "dialect `{}`: beta `{beta}` is not a token",
                table.name
            ));
        }
        for (name, value) in &table.client_headers {
            if !token(name)
                || ADAPTER_HEADERS.contains(&name.to_ascii_lowercase().as_str())
                || value.is_empty()
                || !value.bytes().all(|b| (32..=126).contains(&b))
            {
                return Err(format!(
                    "dialect `{}`: client header `{name}` is not one a dialect may send",
                    table.name
                ));
            }
        }
        Ok(Self {
            name: table.name,
            claude: table.claude,
            credential_header: table.credential_header,
            identity: table.identity,
            base_beta: table.betas.join(","),
            client_headers: table.client_headers,
        })
    }
}

impl MessagesDialect for Declared {
    fn name(&self) -> &str {
        &self.name
    }

    fn is_claude(&self) -> bool {
        self.claude
    }

    fn credential_header(&self) -> Option<&str> {
        self.credential_header.as_deref()
    }

    fn identity(&self) -> &str {
        &self.identity
    }

    fn base_beta(&self) -> &str {
        &self.base_beta
    }

    fn client_headers(&self) -> &[(String, String)] {
        &self.client_headers
    }
}

/// `[adapter_settings.dialect]` as a route file writes it.
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct DeclaredToml {
    name: String,
    #[serde(default)]
    claude: bool,
    #[serde(default)]
    credential_header: Option<String>,
    #[serde(default)]
    identity: String,
    #[serde(default)]
    betas: Vec<String>,
    #[serde(default)]
    client_headers: Vec<(String, String)>,
}

impl TryFrom<DeclaredToml> for Declared {
    type Error = String;

    fn try_from(table: DeclaredToml) -> Result<Self, String> {
        Self::parse(table)
    }
}

/// A declared Claude dialect with an OAuth bearer, an identity block, betas and client
/// headers: the facts the subscription connectors' `anthropic-subscription` route declares,
/// so this crate's own cases cover every part of a declared dialect.
#[cfg(test)]
pub(crate) fn claude_bearer() -> crate::MessagesAccount {
    serde_json::from_value(serde_json::json!({
        "name": "claude-code-subscription",
        "claude": true,
        "identity": "You are Claude Code, Anthropic's official CLI for Claude.",
        "betas": ["oauth-2025-04-20", "claude-code-20250219"],
        "client_headers": [
            ["anthropic-dangerous-direct-browser-access", "true"],
            ["x-app", "cli"],
        ],
    }))
    .expect("the declared dialect")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{MessagesAccount, MessagesAdapterSettings};

    #[test]
    fn the_core_lists_its_dialects_and_a_route_names_or_declares_one() {
        let names: Vec<&str> = dialects().iter().map(|dialect| dialect.name()).collect();
        assert_eq!(names, ["opencode-go", "zai", "api-key"]);
        let settings: MessagesAdapterSettings =
            serde_json::from_str(r#"{"dialect":"zai","credential_header":"x-api-key"}"#)
                .expect("a core dialect");
        assert_eq!(settings.account, MessagesAccount::ZAI);
        let error = serde_json::from_str::<MessagesAdapterSettings>(r#"{"dialect":"pigeon"}"#)
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("pigeon") && error.contains("api-key"),
            "{error}"
        );
        let declared: MessagesAdapterSettings = serde_json::from_str(
            r#"{"dialect":{"name":"claude-bearer","claude":true,"identity":"You are a CLI.",
                "betas":["one-2025","two-2026"],"client_headers":[["x-app","cli"]]}}"#,
        )
        .expect("a declared dialect");
        let dialect = declared.account.dialect();
        assert_eq!(dialect.name(), "claude-bearer");
        assert_eq!(dialect.credential_header(), None);
        assert_eq!(dialect.identity(), "You are a CLI.");
        assert_eq!(dialect.base_beta(), "one-2025,two-2026");
        for refused in [
            r#"{"name":"zai"}"#,
            r#"{"name":"open","identity":"An identity on a non-Claude account."}"#,
            r#"{"name":"open","claude":true,"client_headers":[["anthropic-beta","x"]]}"#,
            r#"{"name":"open","claude":true,"betas":["a b"]}"#,
            r#"{"name":"open","pigeon":true}"#,
        ] {
            let text = format!(r#"{{"dialect":{refused}}}"#);
            assert!(
                serde_json::from_str::<MessagesAdapterSettings>(&text).is_err(),
                "{refused}"
            );
        }
    }
}
