//! The transcript handed to the summarizer and the per-item blocks it is made of.
//!
//! Rendering is specified in `docs/design/context.md` §2 ("The summarization
//! request"). Reasoning TEXT is rendered in `## Assistant` blocks as an excerpt of
//! `reasoning_excerpt_chars` behind `Reasoning (excerpt): ` (ADR-0126; 0 omits it); the
//! call inputs are truncated to 500 characters and tool results to
//! `tool_result_excerpt_chars` (head and tail halves). Blocks are separated by ONE blank
//! line.

use p1_contracts::{AssistantBlock, InboxKind, Item, ToolStatus};

use crate::SUMMARY_MARKER;
use crate::estimate::ceil_tokens;
use crate::plan::is_summary_item;

/// How many characters of a tool call's input the transcript keeps.
const INPUT_EXCERPT_CHARS: usize = 500;
/// The line prefix of a reasoning excerpt inside an `## Assistant` block (ADR-0126).
const REASONING_PREFIX: &str = "Reasoning (excerpt): ";
/// No single untrusted history item may inflate a render by arbitrary bytes. The cap
/// rises with the transcript budget so a block the budget can hold is never shortened.
const MAX_BLOCK_CHARS: usize = 64 * 1024;

/// Render the items outside the verbatim tail. If the transcript alone would
/// exceed `budget_tokens`, the OLDEST items after the previous summary are dropped
/// (whole blocks) and replaced by one omission line, until it fits.
pub(crate) fn transcript(
    items: &[Item],
    excerpt_chars: usize,
    reasoning_chars: usize,
    budget_tokens: u64,
) -> String {
    let text_limit = per_item_limit(budget_tokens);
    let mut summaries = Vec::new();
    let mut others = Vec::new();
    for item in items {
        if is_summary_item(item) {
            summaries.push(summary_block(item));
        } else {
            others.push(block(item, excerpt_chars, reasoning_chars, text_limit));
        }
    }
    let lengths: Vec<u64> = others
        .iter()
        .map(|block| block.chars().count() as u64)
        .collect();
    let summary_chars: u64 = summaries
        .iter()
        .map(|block| block.chars().count() as u64)
        .sum();
    let mut other_chars: u64 = lengths.iter().sum();
    for (dropped, length) in lengths.iter().copied().enumerate() {
        let blocks = summaries.len() + others.len() - dropped + usize::from(dropped > 0);
        let omission_chars = if dropped > 0 {
            format!("[{dropped} earlier items omitted: the session was too long to summarize in one pass]").chars().count() as u64
        } else {
            0
        };
        let chars =
            summary_chars + other_chars + omission_chars + 2 * blocks.saturating_sub(1) as u64;
        if ceil_tokens(chars) <= budget_tokens {
            return assemble(&summaries, &others, dropped);
        }
        other_chars -= length;
    }
    // Dropping every item is the last resort. `assemble` still adds the omission line,
    // which can itself tip a nearly full budget; when it does, the mandatory previous
    // summary goes without the line rather than over the budget.
    let omitted = assemble(&summaries, &others, others.len());
    if ceil_tokens(omitted.chars().count() as u64) <= budget_tokens {
        return omitted;
    }
    summaries.join("\n\n")
}

/// A previous summary is mandatory; refuse rather than sending an over-window request.
pub(crate) fn checked_transcript(
    items: &[Item],
    excerpt_chars: usize,
    reasoning_chars: usize,
    budget_tokens: u64,
) -> Result<String, String> {
    let mandatory_chars: u64 = items
        .iter()
        .filter(|item| is_summary_item(item))
        .map(|item| match item {
            Item::User { text } => {
                text.strip_prefix(SUMMARY_MARKER)
                    .map(|rest| rest.strip_prefix('\n').unwrap_or(rest))
                    .unwrap_or(text)
                    .chars()
                    .count() as u64
                    + "## Previous summary\n".chars().count() as u64
            }
            _ => 0,
        })
        .sum();
    if ceil_tokens(mandatory_chars) > budget_tokens {
        return Err("previous summary exceeds the summarizer transcript budget".into());
    }
    // `transcript` drops every non-mandatory block, and the omission line too when
    // that line would not fit, so the returned transcript is within the budget. The
    // final check keeps the contract even for a pathological stack of summaries whose
    // separators the mandatory count does not include.
    let rendered = transcript(items, excerpt_chars, reasoning_chars, budget_tokens);
    if ceil_tokens(rendered.chars().count() as u64) > budget_tokens {
        return Err("the summarizer transcript cannot fit the budget".into());
    }
    Ok(rendered)
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

/// The most characters one rendered item may contribute. The transcript budget is the
/// real limit: a block that fits it is rendered whole, and a larger one is bounded here
/// (so one item cannot force a render of arbitrary size) and then dropped by the budget
/// loop instead of being silently shortened.
fn per_item_limit(budget_tokens: u64) -> usize {
    // The inverse of `ceil_tokens`: the chars that fit are at most floor(budget * 7 / 2).
    let budget_chars = budget_tokens.saturating_mul(7) / 2;
    usize::try_from(budget_chars.saturating_add(1))
        .unwrap_or(usize::MAX)
        .max(MAX_BLOCK_CHARS)
}

fn block(item: &Item, excerpt_chars: usize, reasoning_chars: usize, text_limit: usize) -> String {
    match item {
        Item::User { text } => format!("## User\n{}", first_chars(text, text_limit)),
        Item::Inbox {
            kind: InboxKind::Notification,
            text,
        } => format!("## Notification\n{}", first_chars(text, text_limit)),
        Item::Inbox {
            kind: InboxKind::Steering,
            text,
        } => format!("## Steering\n{}", first_chars(text, text_limit)),
        Item::Assistant(assistant) => {
            let mut parts: Vec<String> = Vec::new();
            let mut used = 0usize;
            for inner in &assistant.blocks {
                if used >= text_limit {
                    break;
                }
                match inner {
                    // Replay data is never rendered; it lives on the item.
                    AssistantBlock::Reasoning { text, .. }
                        if reasoning_chars > 0 && !text.is_empty() =>
                    {
                        let line = format!("{REASONING_PREFIX}{}", excerpt(text, reasoning_chars));
                        let part = first_chars(&line, text_limit - used);
                        used += part.chars().count();
                        parts.push(part);
                    }
                    AssistantBlock::Reasoning { .. } => {}
                    AssistantBlock::Text { text } => {
                        let part = first_chars(text, text_limit - used);
                        used += part.chars().count();
                        parts.push(part);
                    }
                    AssistantBlock::ToolCall(call) => {
                        let part = format!(
                            "→ {}({})",
                            call.name,
                            first_chars(call.input.raw(), INPUT_EXCERPT_CHARS)
                        );
                        used += part.chars().count();
                        parts.push(part);
                    }
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
    // Never silently shorten a previous summary: checked_transcript rejects it if too large.
    format!("## Previous summary\n{body}")
}

fn first_chars(text: &str, limit: usize) -> String {
    text.chars().take(limit).collect()
}

/// Keep the first and last `limit` characters with a count of what was left out. Also
/// what the trim (ADR-0127) puts in place of an old tool result.
pub(crate) fn excerpt(text: &str, limit: usize) -> String {
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
    fn many_large_blocks_with_tiny_budget_only_assemble_retained_suffix() {
        let items: Vec<Item> = (0..2_000)
            .map(|_| Item::Inbox {
                kind: InboxKind::Notification,
                text: "x".repeat(2_000),
            })
            .collect();
        let rendered = transcript(&items, 200, 0, 25);
        assert!(rendered.len() < 4_000);
        assert!(rendered.contains("earlier items omitted"));
    }

    #[test]
    fn a_full_summary_with_one_huge_item_never_exceeds_the_budget() {
        let items = vec![
            Item::User {
                text: format!("{SUMMARY_MARKER}\nold"),
            },
            Item::Inbox {
                kind: InboxKind::Notification,
                text: "x".repeat(2_000),
            },
        ];
        let budget = 10;
        let rendered = checked_transcript(&items, 200, 0, budget).unwrap();
        assert!(
            rendered.starts_with("## Previous summary\nold"),
            "{rendered}"
        );
        assert!(
            ceil_tokens(rendered.chars().count() as u64) <= budget,
            "{rendered}"
        );
        assert!(!rendered.contains("omitted"), "{rendered}");
    }

    #[test]
    fn a_user_block_that_fits_the_transcript_budget_is_not_truncated() {
        let tail = "the-end-of-a-long-message";
        let long = format!("{}\n{tail}", "x".repeat(MAX_BLOCK_CHARS + 1_000));
        let budget = ceil_tokens(long.chars().count() as u64 + 16) + 10;
        let rendered = transcript(&[Item::User { text: long }], 200, 0, budget);
        assert!(
            rendered.ends_with(tail),
            "a block the budget can hold must not lose its suffix"
        );
    }

    #[test]
    fn an_assistant_text_block_that_fits_the_transcript_budget_is_not_truncated() {
        use p1_contracts::{AssistantItem, Origin};
        let tail = "the-end-of-a-long-answer";
        let long = format!("{}\n{tail}", "y".repeat(MAX_BLOCK_CHARS + 1_000));
        let assistant = Item::Assistant(AssistantItem {
            origin: Origin {
                route: "route".into(),
                model: "model".into(),
            },
            blocks: vec![AssistantBlock::Text { text: long }],
        });
        let budget = ceil_tokens((MAX_BLOCK_CHARS + 1_000 + 32) as u64) + 10;
        let rendered = transcript(&[assistant], 200, 0, budget);
        assert!(
            rendered.ends_with(tail),
            "an assistant block the budget can hold must not lose its suffix"
        );
    }

    #[test]
    fn oversized_previous_summary_cannot_escape_the_budget() {
        let items = vec![Item::User {
            text: format!("{SUMMARY_MARKER}\n{}", "x".repeat(20_000)),
        }];
        assert!(checked_transcript(&items, 200, 0, 100).is_err());
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
        let rendered = transcript(&items, 2_000, 0, 80);
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
    #[test]
    fn an_assistant_block_renders_a_reasoning_excerpt_before_its_text_and_calls() {
        use p1_contracts::{AssistantItem, Origin, ToolCall, ToolInput};
        let item = |reasoning: &str| {
            Item::Assistant(AssistantItem {
                origin: Origin {
                    route: "route".into(),
                    model: "model".into(),
                },
                blocks: vec![
                    AssistantBlock::Reasoning {
                        text: reasoning.into(),
                        replay: None,
                    },
                    AssistantBlock::Text {
                        text: "done".into(),
                    },
                    AssistantBlock::ToolCall(ToolCall {
                        call_id: "c1".into(),
                        name: "lookup".into(),
                        input: ToolInput::Json("{}".into()),
                    }),
                ],
            })
        };
        let reasoning = "R".repeat(5_000);
        let expected = format!(
            "## Assistant\nReasoning (excerpt): {}\n[… 4000 chars omitted …]\n{}\ndone\n→ lookup({{}})",
            "R".repeat(500),
            "R".repeat(500)
        );
        assert_eq!(
            block(&item(&reasoning), 2_000, 1_000, MAX_BLOCK_CHARS),
            expected
        );
        assert_eq!(
            block(&item(&reasoning), 2_000, 0, MAX_BLOCK_CHARS),
            "## Assistant\ndone\n→ lookup({})"
        );
        assert_eq!(
            block(&item(""), 2_000, 1_000, MAX_BLOCK_CHARS),
            "## Assistant\ndone\n→ lookup({})"
        );
    }
}
