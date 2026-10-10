//! Reads durable tool results, not arguments or user-facing names. No execution.
use p1_contracts::plan::{PlanEntry, PlanSource};
use p1_contracts::{JournalRecord, RecordBody, ToolStatus};
use std::collections::HashSet;
use std::sync::Mutex;

#[derive(Default)]
pub struct SessionPlan(Mutex<State>);
#[derive(Default)]
struct State {
    calls: HashSet<String>,
    snapshot: Option<Vec<PlanEntry>>,
}
impl PlanSource for SessionPlan {
    fn observe(&self, record: &JournalRecord) -> Option<Vec<PlanEntry>> {
        let mut state = self.0.lock().unwrap();
        match &record.body {
            RecordBody::ToolStarted { call_id, identity } => {
                state.calls.remove(call_id);
                if matches!(identity.implementation.as_str(), "p1/todo" | "p1-tool-todo") {
                    state.calls.insert(call_id.clone());
                }
            }
            RecordBody::ToolFinished { result, .. }
                if state.calls.remove(&result.call_id) && result.status == ToolStatus::Ok =>
            {
                let entries = p1_todo_guest::snapshot(&result.content).ok()?;
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
}
