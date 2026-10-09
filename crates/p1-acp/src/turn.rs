//! Prompt outcomes. Driver #673 must copy these fallback rows to p1-extensions.md.
//!
//! | p1 stop | ACP stop | Rationale |
//! | --- | --- | --- |
//! | ToolUse | end_turn | A completed turn has no further tool execution to await. |
//! | ContextWindowExceeded | max_tokens | The provider exhausted a token bound. |
//! | Paused | end_turn | ACP v1 has no resumable-pause outcome. |
//! | Other | end_turn | No more specific terminal reason is known. |
//!
//! Failures use JSON-RPC internal error (-32603), preserving the seam's safe
//! diagnostic text. This crate never handles credentials or provider traffic.

use agent_client_protocol_schema::v1::{Error, PromptResponse, StopReason};
use p1_contracts::{StopReason as P1Stop, TurnEnd};

pub fn prompt_outcome(end: TurnEnd) -> Result<PromptResponse, Error> {
    let stop = match end {
        TurnEnd::Cancelled => StopReason::Cancelled,
        TurnEnd::Completed { stop } => match stop {
            P1Stop::EndTurn | P1Stop::ToolUse | P1Stop::Paused | P1Stop::Other => {
                StopReason::EndTurn
            }
            P1Stop::MaxOutputTokens | P1Stop::ContextWindowExceeded => StopReason::MaxTokens,
            P1Stop::Refusal => StopReason::Refusal,
        },
        TurnEnd::ProviderFailed { error } => return Err(Error::new(-32603, error.to_string())),
        TurnEnd::CommitFailed { message } | TurnEnd::ContextFailed { message } => {
            return Err(Error::new(-32603, message));
        }
    };
    Ok(PromptResponse::new(stop))
}
