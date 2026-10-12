//! The subscription connectors' Responses dialect (ADR-0200, ADR-0211): the ChatGPT/Codex
//! subscription, as an implementation of the dialect port. Only a build with the
//! `subscriptions` feature has it.

use crate::ResponsesAccount;
use crate::dialect::ResponsesDialect;

/// The ChatGPT/Codex subscription login: an OAuth bearer that must name the ChatGPT account,
/// and a route that stores no response server-side and takes no output cap.
pub(crate) struct CodexSubscription;

/// The `codex-subscription` dialect.
pub const CODEX_SUBSCRIPTION: ResponsesAccount = ResponsesAccount::of(&CodexSubscription);

impl ResponsesDialect for CodexSubscription {
    fn name(&self) -> &'static str {
        "codex-subscription"
    }

    fn paths(&self) -> (&'static str, &'static str) {
        ("/codex/responses", "/codex")
    }

    fn account_id_header(&self) -> Option<&'static str> {
        Some("chatgpt-account-id")
    }

    fn carries_output_cap(&self) -> bool {
        false
    }

    fn is_codex(&self) -> bool {
        true
    }
}
