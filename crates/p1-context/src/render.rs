//! The transcript handed to the summarizer and the per-item blocks it is made of.
//!
//! Rendering is specified in `docs/design/context.md` §2 ("The summarization
//! request"). Reasoning TEXT is omitted from `## Assistant` blocks; the call inputs
//! are truncated to 500 characters and tool results to `tool_result_excerpt_chars`
//! (head and tail halves). Blocks are separated by ONE blank line.

use p1_contracts::{AssistantBlock, InboxKind, Item, ToolStatus};

use crate::SUMMARY_MARKER;
use crate::estimate::ceil_tokens;
use crate::plan::is_summary_item;

/// How many characters of a tool call's input the transcript keeps.
const INPUT_EXCERPT_CHARS: usize = 500;

/// Render the items outside the verbatim tail. If the transcript alone would
/// exceed `budget_tokens`, the OLDEST items after the previous summary are dropped
/// (whole blocks) and replaced by one omission line, until it fits.
pub(crate) fn transcript(items: &[Item], excerpt_chars: usize, budget_tokens: u64) -> String {
    let mut summaries = Vec::new();
    let mut others = Vec::new();
    for item in items {
        if is_summary_item(item) {
            summaries.push(summary_block(item));
        } else {
            others.push(block(item, excerpt_chars));
        }
    }
    let mut dropped = 0;
    loop {
        let rendered = assemble(&summaries, &others, dropped);
        if dropped >= others.len() || ceil_tokens(rendered.chars().count() as u64) <= budget_tokens
        {
            return rendered;
        }
        dropped += 1;
    }
}

fn assemble(summaries: &[String], others: &[String], dropped: usize) -> String {
    let mut blocks: Vec<&str> = Vec::with_capacity(summaries.len() + others.len() + 1);
    blocks.extend(summaries.iter().map(String::as_str));
    let omission;
    if dropped > 0 {
        omission = format!(
            "[{dropped} earlier items omitted: the session was too long to summarize in one pass]"
        );
        blocks.push(&omission);
    }
    blocks.extend(others[dropped..].iter().map(String::as_str));
    blocks.join("\n\n")
}

fn block(item: &Item, excerpt_chars: usize) -> String {
    match item {
        Item::User { text } => format!("## User\n{text}"),
        Item::Inbox {
            kind: InboxKind::Notification,
            text,
        } => format!("## Notification\n{text}"),
        Item::Inbox {
            kind: InboxKind::Steering,
            text,
        } => format!("## Steering\n{text}"),
        Item::Assistant(assistant) => {
            let mut parts: Vec<String> = Vec::new();
            for inner in &assistant.blocks {
                match inner {
                    // Reasoning text is omitted, its replay data lives on the item.
                    AssistantBlock::Reasoning { .. } => {}
                    AssistantBlock::Text { text } => parts.push(text.clone()),
                    AssistantBlock::ToolCall(call) => parts.push(format!(
                        "→ {}({})",
                        call.name,
                        first_chars(call.input.raw(), INPUT_EXCERPT_CHARS)
                    )),
                }
            }
            if parts.is_empty() {
                "## Assistant".to_string()
            } else {
                format!("## Assistant\n{}", parts.join("\n"))
            }
        }
        Item::ToolResult(result) => format!(
            "## Result of {} [{}]\n{}",
            result.name,
            status_name(result.status),
            excerpt(&result.content, excerpt_chars)
        ),
    }
}

fn summary_block(item: &Item) -> String {
    let Item::User { text } = item else {
        return String::new();
    };
    let body = text
        .strip_prefix(SUMMARY_MARKER)
        .map(|rest| rest.strip_prefix('\n').unwrap_or(rest))
        .unwrap_or(text);
    format!("## Previous summary\n{body}")
}

fn first_chars(text: &str, limit: usize) -> String {
    text.chars().take(limit).collect()
}

/// Keep the first and last `limit` characters with a count of what was left out.
fn excerpt(text: &str, limit: usize) -> String {
    let total = text.chars().count();
    if total <= limit {
        return text.to_string();
    }
    let head = limit / 2;
    let tail = limit - head;
    let omitted = total - limit;
    let head_text: String = text.chars().take(head).collect();
    let tail_text: String = text.chars().skip(total - tail).collect();
    format!("{head_text}\n[… {omitted} chars omitted …]\n{tail_text}")
}

/// The snake_case status name the journal uses (context.md §2 Rulings).
fn status_name(status: ToolStatus) -> &'static str {
    match status {
        ToolStatus::Ok => "ok",
        ToolStatus::Error => "error",
        ToolStatus::Unavailable => "unavailable",
        ToolStatus::Denied => "denied",
        ToolStatus::Cancelled => "cancelled",
        ToolStatus::Unknown => "unknown",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn excerpt_keeps_the_head_and_tail_and_counts_what_was_left_out() {
        assert_eq!(excerpt("short", 10), "short");
        assert_eq!(
            excerpt("HHHHHHTTTTTT", 10),
            "HHHHH\n[… 2 chars omitted …]\nTTTTT"
        );
        assert_eq!(excerpt("abcdefghij", 5), "ab\n[… 5 chars omitted …]\nhij");
    }

    #[test]
    fn first_chars_takes_at_most_the_limit() {
        assert_eq!(first_chars("abcdef", 3), "abc");
        assert_eq!(first_chars("ab", 3), "ab");
    }

    #[test]
    fn the_previous_summary_is_never_dropped_and_dropped_items_are_counted() {
        let mut items = vec![Item::User {
            text: format!("{SUMMARY_MARKER}\nold"),
        }];
        for n in 0..6 {
            items.push(Item::Inbox {
                kind: InboxKind::Notification,
                text: format!("item-{n:02}-{}", "x".repeat(100)),
            });
        }
        let rendered = transcript(&items, 2_000, 80);
        assert!(rendered.starts_with("## Previous summary\nold"));
        let first_kept = (0..6)
            .find(|n| rendered.contains(&format!("item-{n:02}-")))
            .expect("some notification survives");
        assert!(first_kept > 0);
        assert!(rendered.contains(&format!("[{first_kept} earlier items omitted")));
        for n in 0..first_kept {
            assert!(!rendered.contains(&format!("item-{n:02}-")));
        }
    }
}
