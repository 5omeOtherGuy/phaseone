//! The Messages dialect port (ADR-0211): the wire facts one family of accounts requires, in
//! one trait the adapter asks instead of matching a closed list of accounts.
//!
//! The adapter's core dialects are [`ApiKey`], [`OpencodeGo`] and [`Zai`]; the subscription
//! connectors add the Claude Code subscription (`claude-code-subscription`) as an
//! implementation in a build with the `subscriptions` feature. [`dialects`] is the one place
//! a build's dialects are listed.

/// One Messages dialect.
pub trait MessagesDialect: Send + Sync + 'static {
    /// The spelling a route file uses (`dialect = "…"`).
    fn name(&self) -> &'static str;

    /// Whether the account reaches Claude models, so the Claude lowering, the prompt-cache
    /// markers and the Claude context beta apply.
    fn is_claude(&self) -> bool;

    /// The header the broker also places the key in, or `None` for a bearer alone.
    fn credential_header(&self) -> Option<&'static str>;

    /// The block the account requires first in `system`; empty for none.
    fn identity(&self) -> &'static str {
        ""
    }

    /// The betas every request on the account carries; empty for none.
    fn base_beta(&self) -> &'static str {
        ""
    }

    /// The client headers a Claude account's requests carry after the protocol's own, in
    /// order; none for most.
    fn client_headers(&self) -> &'static [(&'static str, &'static str)] {
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
    fn name(&self) -> &'static str {
        "api-key"
    }

    fn is_claude(&self) -> bool {
        true
    }

    fn credential_header(&self) -> Option<&'static str> {
        Some("x-api-key")
    }
}

impl MessagesDialect for OpencodeGo {
    fn name(&self) -> &'static str {
        "opencode-go"
    }

    fn is_claude(&self) -> bool {
        false
    }

    fn credential_header(&self) -> Option<&'static str> {
        Some("x-api-key")
    }
}

impl MessagesDialect for Zai {
    fn name(&self) -> &'static str {
        "zai"
    }

    fn is_claude(&self) -> bool {
        false
    }

    fn credential_header(&self) -> Option<&'static str> {
        Some("x-api-key")
    }
}

/// Every Messages dialect this build has: the subscription connectors' when the build has
/// them, then the core's. The one place they are listed; nothing registers itself.
pub fn dialects() -> Vec<&'static dyn MessagesDialect> {
    let mut dialects: Vec<&'static dyn MessagesDialect> = Vec::new();
    #[cfg(feature = "subscriptions")]
    dialects.push(&crate::subscriptions::ClaudeCodeSubscription);
    dialects.extend([&OpencodeGo as &dyn MessagesDialect, &Zai, &ApiKey]);
    dialects
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{MessagesAccount, MessagesAdapterSettings};

    #[test]
    fn a_build_lists_its_dialects_and_a_route_names_one() {
        let names: Vec<&str> = dialects().iter().map(|dialect| dialect.name()).collect();
        #[cfg(feature = "subscriptions")]
        assert_eq!(
            names,
            ["claude-code-subscription", "opencode-go", "zai", "api-key"]
        );
        #[cfg(not(feature = "subscriptions"))]
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
    }
}
