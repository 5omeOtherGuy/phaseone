//! Token estimation over the model-visible history.
//!
//! The estimate is deliberately crude and deterministic: `ceil(chars / 3.5)` over
//! every character the model can see. It decides WHEN to summarize and whether the
//! tail fits its budget, never how a provider bills.

use p1_contracts::{AssistantBlock, Item};

/// `ceil(chars / 3.5)` over all model-visible text: message text, assistant text,
/// tool inputs and names, replay payloads and tool-result content.
pub fn estimate_tokens(items: &[Item]) -> u64 {
    let chars: u64 = items.iter().map(item_chars).sum();
    ceil_tokens(chars)
}

pub(crate) fn ceil_tokens(chars: u64) -> u64 {
    // ceil(chars / 3.5) == ceil(2 * chars / 7).
    chars.saturating_mul(2).saturating_add(6) / 7
}

pub(crate) fn item_chars(item: &Item) -> u64 {
    match item {
        Item::User { text } => chars(text),
        Item::Inbox { text, .. } => chars(text),
        Item::Assistant(assistant) => assistant.blocks.iter().map(block_chars).sum(),
        Item::ToolResult(result) => chars(&result.content),
    }
}

fn block_chars(block: &AssistantBlock) -> u64 {
    match block {
        AssistantBlock::Text { text } => chars(text),
        AssistantBlock::Reasoning { text, replay } => {
            let replay = replay
                .as_ref()
                .and_then(|data| p1_contracts::serde_json::to_string(&data.payload).ok())
                .map(|json| chars(&json))
                .unwrap_or(0);
            chars(text) + replay
        }
        AssistantBlock::ToolCall(call) => chars(&call.name) + chars(call.input.raw()),
    }
}

fn chars(text: &str) -> u64 {
    text.chars().count() as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ceil_tokens_rounds_up_at_the_seven_halves() {
        assert_eq!(ceil_tokens(0), 0);
        assert_eq!(ceil_tokens(1), 1);
        assert_eq!(ceil_tokens(7), 2);
        assert_eq!(ceil_tokens(8), 3);
        assert_eq!(ceil_tokens(14), 4);
    }
}
