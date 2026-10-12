//! A declared Responses dialect with an account-id header, no output cap, a client identity
//! and session headers (ADR-0214): the facts the subscription connectors'
//! `openai-codex-subscription` route declares, so the suites that pin that wire cover every
//! part of a declared dialect.

#![allow(dead_code)]

use p1_provider_openai::ResponsesAccount;

/// The dialect's `[adapter_settings.dialect]` table.
pub fn codex_subscription_table() -> serde_json::Value {
    serde_json::json!({
        "name": "codex-subscription",
        "path": "/codex/responses",
        "account_id_header": "chatgpt-account-id",
        "output_cap": false,
        "client_headers": [["originator", "p1"]],
        "sse_headers": [["OpenAI-Beta", "responses=experimental"]],
        "session_headers": ["session_id", "conversation_id"],
    })
}

/// The dialect, as that table declares it.
pub fn codex_subscription() -> ResponsesAccount {
    serde_json::from_value(codex_subscription_table()).expect("the declared dialect")
}
