//! The JSON wire forms of `p1-module-protocol` (protocol.md): history items and usage in,
//! history items and usage out. Text that is not its family's form is refused with the
//! reason, never guessed at.

use p1_bindings_context_policy::generated::Prepared as WitPrepared;
use p1_bindings_context_policy::generated::p1::module::types::StopReason as WitStopReason;
use p1_contracts::serde_json;
use p1_contracts::{Item, Prepared, ProviderError, StopReason, Usage};
use p1_module_protocol::{WireItem, WireProviderError, WireUsage};

pub(crate) fn history(items: &[String]) -> Result<Vec<Item>, String> {
    items
        .iter()
        .enumerate()
        .map(|(index, text)| {
            serde_json::from_str::<WireItem>(text)
                .map(Item::from)
                .map_err(|error| format!("history item {index} is not a history item: {error}"))
        })
        .collect()
}

pub(crate) fn usage(text: &str) -> Result<Usage, String> {
    serde_json::from_str::<WireUsage>(text)
        .map(Usage::from)
        .map_err(|error| format!("the usage is not a usage value: {error}"))
}

pub(crate) fn prepared(prepared: Prepared) -> WitPrepared {
    WitPrepared {
        items: prepared
            .items
            .into_iter()
            .map(|item| item_json(&WireItem::from(item)))
            .collect(),
        usage: prepared
            .usage
            .map(|usage| usage_json(&WireUsage::from(usage))),
    }
}

pub(crate) fn stop(stop: WitStopReason) -> StopReason {
    match stop {
        WitStopReason::EndTurn => StopReason::EndTurn,
        WitStopReason::ToolUse => StopReason::ToolUse,
        WitStopReason::MaxOutputTokens => StopReason::MaxOutputTokens,
        WitStopReason::ContextWindowExceeded => StopReason::ContextWindowExceeded,
        WitStopReason::Refusal => StopReason::Refusal,
        WitStopReason::Paused => StopReason::Paused,
        WitStopReason::Other => StopReason::Other,
    }
}

/// A provider error's message as the native policy reports it (`ProviderError`'s
/// `Display`), so both drivers give the same failure reason.
pub(crate) fn error_text(text: &str) -> String {
    match serde_json::from_str::<WireProviderError>(text) {
        Ok(error) => ProviderError::from(error).to_string(),
        Err(error) => format!("the host's provider error is not a provider error: {error}"),
    }
}

/// The wire types serialize to JSON without a failure case (no maps with non-string keys,
/// no non-finite numbers), so a failure here is a bug and traps.
fn item_json(item: &WireItem) -> String {
    serde_json::to_string(item).expect("a wire item always serializes")
}

fn usage_json(usage: &WireUsage) -> String {
    serde_json::to_string(usage).expect("a wire usage always serializes")
}
