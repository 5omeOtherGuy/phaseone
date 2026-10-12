//! A declared Claude dialect with an OAuth bearer, an identity block, betas and client headers
//! (ADR-0214): the facts the subscription connectors' `anthropic-subscription` route declares,
//! so the suites that pin that wire cover every part of a declared dialect.

use p1_provider_anthropic::MessagesAccount;

/// The dialect, as `[adapter_settings.dialect]` declares it.
pub fn claude_code_subscription() -> MessagesAccount {
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
