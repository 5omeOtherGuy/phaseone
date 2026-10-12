//! A declared client identity with static headers and derived ids (ADR-0214): the OpenCode
//! CLI's identity as the subscription connectors' `opencode-zen` route declares it, so the
//! suites that pin that wire cover every part of a declared identity.

#![allow(dead_code)]

use p1_provider_openai_chat::ClientIdentity;

/// The identity's `[adapter_settings.client_identity]` table.
pub fn opencode_table() -> serde_json::Value {
    serde_json::json!({
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
    })
}

/// The identity, as that table declares it.
pub fn opencode() -> ClientIdentity {
    serde_json::from_value(opencode_table()).expect("the declared identity")
}
