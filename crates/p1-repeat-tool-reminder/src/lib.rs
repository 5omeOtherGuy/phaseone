//! Per-agent advisory reminders, composed around any context policy by the host.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use p1_contracts::{
    AssistantBlock, BoxFuture, Compaction, ContextError, ContextInput, ContextPolicy, InboxKind,
    Item, Prepared, ToolInput,
};

/// Label used by the host to distinguish advisory insertions from summaries.
pub const REMINDER_MARKER: &str = "[p1 repeat-tool reminder]";

/// One instance belongs to one agent, whose context requests are sequential.
pub struct RepeatToolReminder {
    inner: Arc<dyn ContextPolicy>,
    state: Mutex<State>,
}

#[derive(Clone, Default)]
struct State {
    history: Vec<Item>,
    identity: Option<(String, ToolInput)>,
    count: usize,
    pending: HashMap<String, String>,
}

impl RepeatToolReminder {
    pub fn new(inner: Arc<dyn ContextPolicy>) -> Self {
        Self {
            inner,
            state: Mutex::new(State::default()),
        }
    }
}

impl ContextPolicy for RepeatToolReminder {
    fn prepare<'a>(
        &'a self,
        input: ContextInput<'a>,
    ) -> BoxFuture<'a, Result<Option<Prepared>, ContextError>> {
        Box::pin(async move {
            let mut state = self.state.lock().expect("reminder state poisoned").clone();
            let mut reminders = Vec::new();
            // Save the installed replacement as the next cursor, so compaction and trims
            // do not replay calls or erase the live chain.
            let prefix = state
                .history
                .iter()
                .zip(input.history)
                .take_while(|(a, b)| a == b)
                .count();
            let start = if state.history.is_empty() {
                input
                    .history
                    .iter()
                    .rposition(|item| {
                        matches!(
                            item,
                            Item::User { .. }
                                | Item::Inbox {
                                    kind: InboxKind::Steering,
                                    ..
                                }
                        )
                    })
                    .unwrap_or(0)
            } else {
                prefix
            };
            for (index, item) in input.history.iter().enumerate().skip(start) {
                match item {
                    Item::User { .. }
                    | Item::Inbox {
                        kind: InboxKind::Steering,
                        ..
                    } => {
                        state.identity = None;
                        state.count = 0;
                        state.pending.clear();
                    }
                    Item::Assistant(assistant) => {
                        for block in &assistant.blocks {
                            let AssistantBlock::ToolCall(call) = block else {
                                continue;
                            };
                            let normalized = match &call.input {
                                ToolInput::Json(raw) => ToolInput::Json(
                                    serde_json::from_str(raw)
                                        .map(|value| p1_json_order::canonical_json(&value))
                                        .unwrap_or_else(|_| raw.clone()),
                                ),
                                ToolInput::Text(raw) => ToolInput::Text(raw.clone()),
                            };
                            let identity = (call.name.clone(), normalized);
                            if state.identity.as_ref() == Some(&identity) {
                                state.count = state.count.saturating_add(1);
                            } else {
                                state.identity = Some(identity.clone());
                                state.count = 1;
                            }
                            if [3, 5, 8].contains(&state.count) {
                                let preview: String = identity.1.raw().chars().take(500).collect();
                                state.pending.insert(call.call_id.clone(), format!(
                                    "{REMINDER_MARKER} You have called {} with identical arguments {} consecutive times. Arguments (up to 500 characters): {}\nConsider a different approach if this is not making progress. This reminder is advisory; tool calls remain available.",
                                    call.name, state.count, preview,
                                ));
                            }
                        }
                    }
                    Item::ToolResult(result) => {
                        if let Some(text) = state.pending.remove(&result.call_id) {
                            // A resumed agent reconstructs its chain from history, but
                            // the journal may already contain this result's reminder.
                            if !matches!(input.history.get(index + 1),
                                Some(Item::Inbox { kind: InboxKind::Notification, text: old }) if old == &text
                            ) {
                                reminders.push((result.call_id.clone(), text));
                            }
                        }
                    }
                    _ => {}
                }
            }
            let prepared = self
                .inner
                .prepare(ContextInput {
                    history: input.history,
                    last_usage: input.last_usage,
                    cancel: input.cancel,
                })
                .await?;
            let changed = prepared.is_some() || !reminders.is_empty();
            let mut prepared = prepared.unwrap_or_else(|| Prepared {
                items: input.history.to_vec(),
                usage: None,
            });
            for (call_id, text) in reminders {
                // If a summary removed the result, retain the fresh reminder at the
                // end rather than silently losing the nudge at a threshold.
                let position = prepared.items.iter().position(|item| {
                    matches!(item, Item::ToolResult(result) if result.call_id == call_id)
                }).map_or(prepared.items.len(), |index| index + 1);
                prepared.items.insert(
                    position,
                    Item::Inbox {
                        kind: InboxKind::Notification,
                        text,
                    },
                );
            }
            state.history = prepared.items.clone();
            *self.state.lock().expect("reminder state poisoned") = state;
            Ok(changed.then_some(prepared))
        })
    }

    fn compact_now<'a>(
        &'a self,
        input: ContextInput<'a>,
    ) -> BoxFuture<'a, Result<Compaction, ContextError>> {
        Box::pin(async move {
            let result = self.inner.compact_now(input).await?;
            if let Compaction::Replaced { prepared, .. } = &result {
                self.state.lock().expect("reminder state poisoned").history =
                    prepared.items.clone();
            }
            Ok(result)
        })
    }
}
