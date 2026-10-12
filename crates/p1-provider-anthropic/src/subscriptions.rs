//! The subscription connectors' Messages dialect (ADR-0200, ADR-0211): the Claude Code
//! subscription, as an implementation of the dialect port. Only a build with the
//! `subscriptions` feature has it.

use crate::MessagesAccount;
use crate::dialect::MessagesDialect;
use crate::request::{BASE_BETA, IDENTITY};

/// The Claude Code subscription login: an OAuth bearer, the CLI identity block and the betas
/// this account accepts.
pub(crate) struct ClaudeCodeSubscription;

/// The `claude-code-subscription` dialect.
pub const CLAUDE_CODE_SUBSCRIPTION: MessagesAccount = MessagesAccount::of(&ClaudeCodeSubscription);

impl MessagesDialect for ClaudeCodeSubscription {
    fn name(&self) -> &'static str {
        "claude-code-subscription"
    }

    fn is_claude(&self) -> bool {
        true
    }

    /// The OAuth bearer travels as `authorization` alone.
    fn credential_header(&self) -> Option<&'static str> {
        None
    }

    fn identity(&self) -> &'static str {
        IDENTITY
    }

    fn base_beta(&self) -> &'static str {
        BASE_BETA
    }

    /// The Claude Code CLI's client identity.
    fn client_headers(&self) -> &'static [(&'static str, &'static str)] {
        &[
            ("anthropic-dangerous-direct-browser-access", "true"),
            ("x-app", "cli"),
        ]
    }
}
