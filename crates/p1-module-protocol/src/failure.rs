//! Mapping module-side failures into the closed native shapes (freeze item 5).
//!
//! A module failure never introduces a new error kind or tool status: the core, the
//! journal, the UI and the transport broker's retry rules all branch on the closed sets
//! in `p1-contracts`, and a kind they have never seen would have no defined handling.

use p1_contracts::{Outcome, ProviderError, ProviderErrorKind, ToolOutcome, ToolStatus};

use crate::WireProviderErrorKind;

/// What can go wrong on the module side of the boundary.
///
/// Messages carried here are produced by the host (runtime trap text, parser error), never
/// copied from guest memory, so mapping them into model- or operator-visible text cannot
/// leak what the guest held.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ModuleFailure {
    /// The guest trapped (panic, unreachable, out-of-bounds access).
    #[error("module trapped: {0}")]
    Trap(String),
    /// The call was cancelled by the core or the operator.
    #[error("module call cancelled")]
    Cancelled,
    /// The call ran past its wall-clock deadline (epoch interruption).
    #[error("module call exceeded its deadline")]
    DeadlineExceeded,
    /// The call used up its fuel.
    #[error("module call exhausted its fuel")]
    FuelExhausted,
    /// The guest returned output that does not parse into the wire type; the message is
    /// the parse error, never the output itself.
    #[error("module returned invalid output: {0}")]
    InvalidOutput(String),
    /// A host import failed with an already-classified provider error.
    #[error("module host call failed: {0}")]
    Host(ProviderError),
}

impl ModuleFailure {
    /// The tool result for a failed tool module call. Total: every failure is a result the
    /// model sees, because a tool call must always be answered.
    ///
    /// `Cancelled` is `ToolStatus::Cancelled` with empty content, as native tools answer a
    /// cancellation, so the core treats both alike. Every other failure is
    /// `ToolStatus::Error`: the tool did not do what was asked, and the model must be told
    /// which failure it was so it can narrow the request or try another way. The text
    /// warns that effects may be partial, since a trap never undoes a native effect.
    pub fn into_tool_outcome(self) -> ToolOutcome {
        let content = match self {
            Self::Cancelled => {
                return ToolOutcome {
                    status: ToolStatus::Cancelled,
                    content: String::new(),
                };
            }
            Self::Trap(message) => format!(
                "tool failed: the tool module trapped ({message}). Effects before the failure \
                 may be partial; check the state before retrying."
            ),
            Self::DeadlineExceeded => "tool failed: the tool module exceeded its time limit. \
                 Effects before the failure may be partial; check the state and retry with a \
                 smaller request."
                .to_owned(),
            Self::FuelExhausted => "tool failed: the tool module exhausted its compute budget. \
                 Effects before the failure may be partial; check the state and retry with a \
                 smaller request."
                .to_owned(),
            Self::InvalidOutput(message) => format!(
                "tool failed: the tool module returned an invalid result ({message}). Effects \
                 before the failure may be partial; check the state before retrying."
            ),
            Self::Host(error) => format!(
                "tool failed: a host service the tool module called failed ({}: {}).",
                WireProviderErrorKind::from(error.kind).name(),
                error.message
            ),
        };
        ToolOutcome {
            status: ToolStatus::Error,
            content,
        }
    }

    /// The terminal stream outcome for a failed provider module call. Total: a stream must
    /// end with exactly one terminal outcome whatever happened to the module.
    ///
    /// The kinds follow the native transport broker's retry semantics, which stay native:
    /// - `Cancelled` → `Outcome::Cancelled`: nothing failed, the caller asked to stop.
    /// - `Trap`, `InvalidOutput`, `FuelExhausted` → `Protocol`, which the broker does not
    ///   retry: the guest is deterministic, so the same request would fail the same way
    ///   (fuel is counted per instruction, not per second).
    /// - `DeadlineExceeded` → `Transport`, which the broker may retry: a wall-clock deadline
    ///   depends on host waits such as the network, so a second attempt can succeed.
    /// - `Host(e)` → `e` unchanged: the host import already classified it, and
    ///   reclassifying would change its retry rule.
    pub fn into_provider_outcome(self) -> Outcome {
        let (kind, message) = match self {
            Self::Cancelled => return Outcome::Cancelled,
            Self::Host(error) => return Outcome::Failed(error),
            Self::Trap(message) => (
                ProviderErrorKind::Protocol,
                format!("provider module trapped: {message}"),
            ),
            Self::InvalidOutput(message) => (
                ProviderErrorKind::Protocol,
                format!("provider module returned invalid output: {message}"),
            ),
            Self::FuelExhausted => (
                ProviderErrorKind::Protocol,
                "provider module exhausted its fuel".to_owned(),
            ),
            Self::DeadlineExceeded => (
                ProviderErrorKind::Transport,
                "provider module exceeded its deadline".to_owned(),
            ),
        };
        Outcome::Failed(ProviderError::new(kind, message))
    }
}
