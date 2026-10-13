//! Reads durable journal records, never user-facing names. No execution.
//!
//! Two tools propose plans: `p1/todo`, whose committed result is the snapshot (ADR-0159),
//! and the Codex face `p1/update-plan` (ADR-0219), whose result is Codex's `Plan updated`
//! and whose plan is the call's journalled arguments, kept from the assistant item that
//! made the call until the call commits.
use p1_contracts::history::{AssistantBlock, ToolInput};
use p1_contracts::plan::{PlanEntry, PlanSource};
use p1_contracts::{JournalRecord, RecordBody, ToolStatus};
use std::collections::HashMap;
use std::sync::Mutex;

#[derive(Default)]
pub struct SessionPlan(Mutex<State>);

/// Where a started plan call's snapshot comes from when it commits.
enum Source {
    /// `p1/todo`: the committed result's content.
    Result,
    /// `p1/update-plan`: the call's own arguments.
    Arguments(String),
}

#[derive(Default)]
struct State {
    /// The arguments of every tool call of the latest assistant item, by call id.
    arguments: HashMap<String, String>,
    calls: HashMap<String, Source>,
    snapshot: Option<Vec<PlanEntry>>,
}
impl PlanSource for SessionPlan {
    fn observe(&self, record: &JournalRecord) -> Option<Vec<PlanEntry>> {
        let mut state = self.0.lock().unwrap();
        match &record.body {
            RecordBody::AssistantCompleted { item, .. } => {
                state.arguments = item
                    .blocks
                    .iter()
                    .filter_map(|block| match block {
                        AssistantBlock::ToolCall(call) => match &call.input {
                            ToolInput::Json(raw) => Some((call.call_id.clone(), raw.clone())),
                            ToolInput::Text(_) => None,
                        },
                        _ => None,
                    })
                    .collect();
            }
            RecordBody::ToolStarted { call_id, identity } => {
                state.calls.remove(call_id);
                match identity.implementation.as_str() {
                    "p1/todo" | "p1-tool-todo" => {
                        state.calls.insert(call_id.clone(), Source::Result);
                    }
                    "p1/update-plan" => {
                        if let Some(arguments) = state.arguments.remove(call_id) {
                            state
                                .calls
                                .insert(call_id.clone(), Source::Arguments(arguments));
                        }
                    }
                    _ => {}
                }
            }
            RecordBody::ToolFinished { result, .. } => {
                let source = state.calls.remove(&result.call_id)?;
                if result.status != ToolStatus::Ok {
                    return None;
                }
                let entries = match source {
                    Source::Result => p1_todo_guest::snapshot(&result.content).ok()?,
                    Source::Arguments(arguments) => {
                        p1_update_plan_guest::snapshot(&arguments).ok()?
                    }
                };
                state.snapshot = Some(entries.clone());
                return Some(entries);
            }
            _ => {}
        }
        None
    }
    fn snapshot(&self) -> Option<Vec<PlanEntry>> {
        self.0.lock().unwrap().snapshot.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use p1_contracts::plan::{PlanPriority, PlanStatus};
    use p1_contracts::{ToolIdentity, ToolResultItem};

    fn record(body: RecordBody) -> JournalRecord {
        JournalRecord { seq: 0, body }
    }
    fn start(source: &SessionPlan, implementation: &str) {
        source.observe(&record(RecordBody::ToolStarted {
            call_id: "c1".into(),
            identity: ToolIdentity {
                implementation: implementation.into(),
                variant: "renamed".into(),
            },
        }));
    }
    fn finish(status: ToolStatus, content: &str) -> JournalRecord {
        record(RecordBody::ToolFinished {
            result: ToolResultItem {
                call_id: "c1".into(),
                name: "my_plan".into(),
                status,
                content: content.into(),
            },
            exit_code: Some(None),
        })
    }
    const LIST: &str =
        r#"{"todos":[{"content":"Ship fix","status":"in_progress","priority":"high"}]}"#;

    #[test]
    fn only_known_successful_committed_results_replace_and_replay_survives_compaction() {
        let source = SessionPlan::default();
        assert_eq!(source.snapshot(), None);
        assert_eq!(source.observe(&finish(ToolStatus::Ok, LIST)), None);
        start(&source, "other-tool");
        assert_eq!(source.observe(&finish(ToolStatus::Ok, LIST)), None);
        start(&source, "p1/todo");
        assert_eq!(source.snapshot(), None, "unfinished calls do not publish");
        let expected = vec![PlanEntry {
            content: "Ship fix".into(),
            status: PlanStatus::InProgress,
            priority: PlanPriority::High,
        }];
        assert_eq!(
            source.observe(&finish(ToolStatus::Ok, LIST)),
            Some(expected.clone())
        );
        for status in [
            ToolStatus::Error,
            ToolStatus::Denied,
            ToolStatus::Cancelled,
            ToolStatus::Unknown,
            ToolStatus::Unavailable,
        ] {
            start(&source, "p1/todo");
            assert_eq!(source.observe(&finish(status, r#"{"todos":[]}"#)), None);
            assert_eq!(source.snapshot(), Some(expected.clone()));
        }
        start(&source, "p1/todo");
        assert_eq!(source.observe(&finish(ToolStatus::Ok, "malformed")), None);
        source.observe(&record(RecordBody::ContextReplaced {
            items: vec![],
            usage: None,
        }));
        assert_eq!(source.snapshot(), Some(expected.clone()));
        let replayed = SessionPlan::default();
        start(&replayed, "p1/todo");
        replayed.observe(&finish(ToolStatus::Ok, LIST));
        replayed.observe(&record(RecordBody::ContextReplaced {
            items: vec![],
            usage: None,
        }));
        assert_eq!(replayed.snapshot(), Some(expected));
        start(&source, "p1/todo");
        assert_eq!(
            source.observe(&finish(ToolStatus::Ok, r#"{"todos":[]}"#)),
            Some(vec![])
        );
        assert_ne!(
            source.snapshot(),
            replayed.snapshot(),
            "sessions own independent state"
        );
    }

    fn assistant_call(call_id: &str, input: ToolInput) -> JournalRecord {
        use p1_contracts::history::{AssistantItem, Origin, ToolCall};
        record(RecordBody::AssistantCompleted {
            item: AssistantItem {
                origin: Origin {
                    route: "r".into(),
                    model: "m".into(),
                },
                blocks: vec![AssistantBlock::ToolCall(ToolCall {
                    call_id: call_id.into(),
                    name: "update_plan".into(),
                    input,
                })],
            },
            stop: p1_contracts::StopReason::ToolUse,
            usage: None,
        })
    }

    #[test]
    fn an_update_plan_call_publishes_the_plan_of_its_own_arguments_when_it_commits() {
        let source = SessionPlan::default();
        let arguments = r#"{"plan":[{"step":"Read the code","status":"completed"},{"step":"Fix it","status":"in_progress"}]}"#;
        source.observe(&assistant_call("c1", ToolInput::Json(arguments.into())));
        start(&source, "p1/update-plan");
        assert_eq!(source.snapshot(), None, "unfinished calls do not publish");
        let expected = vec![
            PlanEntry {
                content: "Read the code".into(),
                status: PlanStatus::Completed,
                priority: PlanPriority::Medium,
            },
            PlanEntry {
                content: "Fix it".into(),
                status: PlanStatus::InProgress,
                priority: PlanPriority::Medium,
            },
        ];
        assert_eq!(
            source.observe(&finish(ToolStatus::Ok, "Plan updated")),
            Some(expected.clone()),
            "the model-facing text carries no snapshot; the arguments do"
        );
        // A failed call keeps the snapshot; a call the assistant item did not make (no
        // arguments) publishes nothing; a freeform input is not a plan.
        source.observe(&assistant_call(
            "c1",
            ToolInput::Json(r#"{"plan":[]}"#.into()),
        ));
        start(&source, "p1/update-plan");
        assert_eq!(source.observe(&finish(ToolStatus::Error, "nope")), None);
        assert_eq!(source.snapshot(), Some(expected.clone()));
        start(&source, "p1/update-plan");
        assert_eq!(
            source.observe(&finish(ToolStatus::Ok, "Plan updated")),
            None
        );
        source.observe(&assistant_call(
            "c1",
            ToolInput::Text("*** Begin Patch".into()),
        ));
        start(&source, "p1/update-plan");
        assert_eq!(
            source.observe(&finish(ToolStatus::Ok, "Plan updated")),
            None
        );
        assert_eq!(source.snapshot(), Some(expected));
    }
}
