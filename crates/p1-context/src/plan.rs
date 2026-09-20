//! How a history is cut into units and rebuilt into a replacement.
//!
//! Units and the tail are specified in `docs/design/context.md` §2 ("What is kept
//! verbatim"). The rule that makes tool pairing hold by construction: a unit is an
//! `Assistant` item plus the `ToolResult` items of its calls, and the user/inbox
//! items between two units belong to the FOLLOWING unit, so a unit is never split.

use p1_contracts::Item;

use crate::SUMMARY_MARKER;
use crate::estimate::{ceil_tokens, estimate_tokens, item_chars};

/// The item ranges of one history, in order: the prelude (items before the first
/// assistant), the units, and the trailing user/inbox items after the last unit
/// (which are implicitly whatever follows the last unit's end).
pub(crate) struct Segments {
    pub(crate) prelude_end: usize,
    pub(crate) units: Vec<(usize, usize)>,
}

pub(crate) fn segments(items: &[Item]) -> Segments {
    let Some(first_assistant) = items
        .iter()
        .position(|item| matches!(item, Item::Assistant(_)))
    else {
        // No assistant at all: the whole history is prelude.
        return Segments {
            prelude_end: items.len(),
            units: Vec::new(),
        };
    };
    let mut units = Vec::new();
    let mut index = first_assistant;
    while index < items.len() {
        if matches!(items[index], Item::Assistant(_)) {
            let mut end = index + 1;
            while end < items.len() && matches!(items[end], Item::ToolResult(_)) {
                end += 1;
            }
            // The unit starts where the previous one ended; the user/inbox items
            // in between therefore belong to THIS unit.
            let start = units.last().map_or(first_assistant, |(_, end)| *end);
            units.push((start, end));
            index = end;
        } else {
            index += 1;
        }
    }
    Segments {
        prelude_end: first_assistant,
        units,
    }
}

/// The index where the verbatim tail begins: the start of the longest suffix of
/// units whose estimate fits `keep_recent`, always at least the last unit. With no
/// units the tail is empty, so the whole prelude is summarized.
pub(crate) fn tail_start(items: &[Item], segments: &Segments, keep_recent: u64) -> usize {
    let Some(last) = segments.units.last() else {
        return segments.prelude_end;
    };
    let last_end = last.1;
    let mut start = segments.units.len() - 1;
    while start > 0 {
        let candidate = segments.units[start - 1].0;
        if estimate_tokens(&items[candidate..last_end]) <= keep_recent {
            start -= 1;
        } else {
            break;
        }
    }
    segments.units[start].0
}

/// The units the tail holds. `1` means the tail cannot shrink any further, so a
/// replacement that is not below `summarize_at_tokens` is the best possible one.
pub(crate) fn tail_units(segments: &Segments, tail_start: usize) -> usize {
    segments
        .units
        .iter()
        .filter(|(start, _)| *start >= tail_start)
        .count()
}

/// Build `[summary] + kept user messages + tail`.
///
/// The first user message (the task) is always kept; then the newest ones that fit
/// `user_budget`, walking backwards from the newest until one does not fit. The
/// summary item itself and the kept messages are in the order the history had.
pub(crate) fn build_replacement(
    items: &[Item],
    tail_start: usize,
    user_budget: u64,
    summary: Item,
) -> Vec<Item> {
    let users: Vec<usize> = (0..tail_start)
        .filter(|&index| is_plain_user(&items[index]))
        .collect();
    let mut kept: Vec<usize> = Vec::new();
    let mut used = 0;
    if let Some(&first) = users.first() {
        kept.push(first);
        used = item_chars(&items[first]);
    }
    let first = users.first().copied();
    for &index in users.iter().rev() {
        if Some(index) == first {
            continue;
        }
        let cost = item_chars(&items[index]);
        if ceil_tokens(used + cost) <= user_budget {
            used += cost;
            kept.push(index);
        } else {
            break;
        }
    }
    kept.sort_unstable();
    let mut replacement = Vec::with_capacity(kept.len() + 2);
    replacement.push(summary);
    replacement.extend(kept.into_iter().map(|index| items[index].clone()));
    replacement.extend(items[tail_start..].iter().cloned());
    replacement
}

pub(crate) fn is_summary_item(item: &Item) -> bool {
    matches!(item, Item::User { text } if text.starts_with(SUMMARY_MARKER))
}

fn is_plain_user(item: &Item) -> bool {
    matches!(item, Item::User { text } if !text.starts_with(SUMMARY_MARKER))
}

#[cfg(test)]
mod tests {
    use super::*;
    use p1_contracts::{
        AssistantBlock, AssistantItem, InboxKind, Origin, ToolResultItem, ToolStatus,
    };

    fn text_item(text: &str) -> Item {
        Item::Assistant(AssistantItem {
            origin: Origin {
                route: "r".into(),
                model: "m".into(),
            },
            blocks: vec![AssistantBlock::Text { text: text.into() }],
        })
    }

    fn inbox() -> Item {
        Item::Inbox {
            kind: InboxKind::Steering,
            text: "steer".into(),
        }
    }

    fn result() -> Item {
        Item::ToolResult(ToolResultItem {
            call_id: "c".into(),
            name: "t".into(),
            status: ToolStatus::Ok,
            content: "ok".into(),
        })
    }

    fn user(text: &str) -> Item {
        Item::User { text: text.into() }
    }

    #[test]
    fn a_leading_inbox_belongs_to_the_unit_that_follows_it() {
        let items = vec![text_item("a"), inbox(), text_item("b")];
        let segments = segments(&items);
        assert_eq!(segments.prelude_end, 0);
        assert_eq!(segments.units, vec![(0, 1), (1, 3)]);
    }

    #[test]
    fn the_prelude_is_everything_before_the_first_assistant() {
        let items = vec![user("task"), inbox(), text_item("a"), result()];
        let segments = segments(&items);
        assert_eq!(segments.prelude_end, 2);
        assert_eq!(segments.units, vec![(2, 4)]);
    }

    #[test]
    fn the_tail_keeps_at_least_the_last_unit_and_can_only_grow_with_the_budget() {
        let items = vec![user("task"), text_item("a"), text_item("b")];
        let segments = segments(&items);
        let tight = tail_start(&items, &segments, 0);
        assert_eq!(tight, segments.units[1].0);
        assert_eq!(tail_units(&segments, tight), 1);
        let generous = tail_start(&items, &segments, 100);
        assert_eq!(generous, segments.units[0].0);
        assert_eq!(tail_units(&segments, generous), 2);
    }

    #[test]
    fn build_replacement_is_summary_then_kept_users_then_tail() {
        let items = vec![
            user("task"),
            text_item("old"),
            user("rule"),
            text_item("tail"),
        ];
        let segments = segments(&items);
        let tail = tail_start(&items, &segments, 1);
        let summary = Item::User {
            text: format!("{SUMMARY_MARKER}\ns"),
        };
        let replacement = build_replacement(&items, tail, 100, summary.clone());
        assert_eq!(
            replacement,
            vec![summary, user("task"), user("rule"), text_item("tail")]
        );
    }
}
