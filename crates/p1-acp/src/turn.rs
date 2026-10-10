//! Prompt outcomes. Driver #673 must copy these fallback rows to p1-extensions.md.
//!
//! | p1 stop | ACP v1 stop | Rationale |
//! | --- | --- | --- |
//! | ToolUse | end_turn | A completed turn has no further tool execution to await. |
//! | ContextWindowExceeded | max_tokens | The provider exhausted a token bound. |
//! | Paused | end_turn | ACP v1 has no resumable-pause outcome. |
//! | Other | end_turn | No more specific terminal reason is known. |
//!
//! Failures preserve the seam's safe diagnostic text. The codec chooses the
//! JSON-RPC error code; no wire types or credential traffic reach this mapping.

use p1_contracts::{StopReason, TurnEnd};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurnStop {
    Finished,
    OutputLimit,
    Refused,
    Cancelled,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnError {
    pub message: String,
}

pub fn prompt_outcome(end: TurnEnd) -> Result<TurnStop, TurnError> {
    match end {
        TurnEnd::Cancelled => Ok(TurnStop::Cancelled),
        TurnEnd::Completed { stop } => Ok(match stop {
            StopReason::EndTurn | StopReason::ToolUse | StopReason::Paused | StopReason::Other => {
                TurnStop::Finished
            }
            StopReason::MaxOutputTokens | StopReason::ContextWindowExceeded => {
                TurnStop::OutputLimit
            }
            StopReason::Refusal => TurnStop::Refused,
        }),
        TurnEnd::ProviderFailed { error } => Err(TurnError {
            message: error.to_string(),
        }),
        TurnEnd::CommitFailed { message } | TurnEnd::ContextFailed { message } => {
            Err(TurnError { message })
        }
    }
}
