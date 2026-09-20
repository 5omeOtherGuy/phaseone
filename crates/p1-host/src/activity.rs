//! The host's session-completion state: the `SessionActivity` implementation fed
//! from the event stream, the tee that feeds it, and the per-agent handoff of the
//! `finish` tool's outcome.
//!
//! The tee forwards EVERY event to the renderer unchanged (the renderer keeps its
//! exact behaviour) and records finished calls into an [`ActivityLog`]: the order
//! of the last `WritesFiles` call and every `Executes` run with its parsed shell
//! exit code. The log is ONE owner per agent — a worker gets its own — and nothing
//! here is global.
//!
//! The `finish` tool instance is built by the catalog, before the host knows the
//! assembled tools or has a renderer to tee. [`CompletionHub`] is the seam: the
//! catalog issues a fresh `(log, outcome)` pair per assembly; the host takes the
//! pair for the agent it just assembled and installs the tee with that agent's
//! tools. Child assemblies are serialised by the worker service, so a take always
//! returns the pair belonging to the assembly that just ran.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use p1_contracts::{
    AgentEvent, Effect, EventSink, JournalRecord, RecordBody, Tool, ToolCall, ToolInput,
    ToolResultItem, ToolStatus,
};
use p1_tool_finish::{FinishOutcome, SessionActivity, ShellRun};

/// One finished tool call, in finish order.
struct Finished {
    name: String,
    effect: Effect,
    status: ToolStatus,
    order: u64,
    command: Option<String>,
    exit_code: Option<i32>,
}

/// What was learned about a call when it started, keyed by call id.
struct Pending {
    effect: Effect,
    command: Option<String>,
}

/// The host's [`SessionActivity`]. One per agent; fed only through [`ActivityTee`].
#[derive(Default)]
pub struct ActivityLog {
    next_order: AtomicU64,
    pending: Mutex<HashMap<String, Pending>>,
    finished: Mutex<Vec<Finished>>,
    /// The model-facing name of this agent's `finish` tool, set once the catalog
    /// built it. Progress counts every OTHER finished call.
    finish_name: Mutex<Option<String>>,
}

impl ActivityLog {
    /// Record a call that is about to run, with the effect its assembled tool
    /// classified. The command is read from a `shell` call's input so a later
    /// `finish` can compare named commands.
    pub fn record_started(&self, call: &ToolCall, effect: Effect) {
        let command = match effect {
            Effect::Executes => shell_command(call),
            _ => None,
        };
        self.pending
            .lock()
            .unwrap()
            .insert(call.call_id.clone(), Pending { effect, command });
    }

    /// Record a finished call. `order` is assigned here, monotonically.
    pub fn record_finished(&self, result: &ToolResultItem) {
        let pending = self.pending.lock().unwrap().remove(&result.call_id);
        let effect = pending.as_ref().map_or(Effect::ReadOnly, |p| p.effect);
        let command = pending.and_then(|p| p.command);
        let order = self.next_order.fetch_add(1, Ordering::SeqCst) + 1;
        let exit_code = if effect == Effect::Executes && result.status == ToolStatus::Ok {
            parse_exit_code(&result.content)
        } else {
            None
        };
        self.finished.lock().unwrap().push(Finished {
            name: result.name.clone(),
            effect,
            status: result.status,
            order,
            command,
            exit_code,
        });
    }

    /// The model-facing name of this agent's `finish` tool.
    pub fn set_finish_name(&self, name: String) {
        *self.finish_name.lock().unwrap() = Some(name);
    }

    /// Rebuild the log from a resumed session's journal (completion.md §3), in
    /// record order, BEFORE the first turn. A verification run before the restart
    /// then still counts, and a file change before it still invalidates.
    ///
    /// `ToolStarted` carries no call name or input, so those come from the
    /// `AssistantCompleted` item that produced the call. The effect is looked up on
    /// the tool assembled NOW, by model-facing name. A tool that no longer exists is
    /// treated as NOT writing and NOT executing: we cannot know what it did, and
    /// guessing that a vanished tool wrote a file would be worse than missing it.
    pub fn replay(&self, tools: &[Arc<dyn Tool>], records: &[JournalRecord]) {
        let mut calls: std::collections::HashMap<&str, &ToolCall> =
            std::collections::HashMap::new();
        for record in records {
            if let RecordBody::AssistantCompleted { item, .. } = &record.body {
                for call in item.tool_calls() {
                    calls.insert(call.call_id.as_str(), call);
                }
            }
        }
        let by_name: std::collections::HashMap<&str, &Arc<dyn Tool>> = tools
            .iter()
            .map(|tool| (tool.declaration().name.as_str(), tool))
            .collect();
        for record in records {
            match &record.body {
                RecordBody::ToolStarted { call_id, .. } => {
                    let Some(call) = calls.get(call_id.as_str()) else {
                        continue;
                    };
                    let effect = by_name
                        .get(call.name.as_str())
                        .map_or(Effect::ReadOnly, |tool| tool.effect(call));
                    self.record_started(call, effect);
                }
                RecordBody::ToolFinished { result } => self.record_finished(result),
                _ => {}
            }
        }
    }

    /// Finished calls other than the `finish` tool — the host's progress signal
    /// between continuations.
    pub fn non_finish_finishes(&self) -> u64 {
        let finish = self.finish_name.lock().unwrap().clone();
        let finished = self.finished.lock().unwrap();
        match finish {
            Some(name) => finished.iter().filter(|entry| entry.name != name).count() as u64,
            None => finished.len() as u64,
        }
    }
}

impl SessionActivity for ActivityLog {
    fn last_file_change(&self) -> Option<u64> {
        self.finished
            .lock()
            .unwrap()
            .iter()
            .rev()
            .find(|entry| entry.effect == Effect::WritesFiles && entry.status == ToolStatus::Ok)
            .map(|entry| entry.order)
    }

    fn shell_runs(&self) -> Vec<ShellRun> {
        self.finished
            .lock()
            .unwrap()
            .iter()
            .filter(|entry| entry.effect == Effect::Executes && entry.status == ToolStatus::Ok)
            .map(|entry| ShellRun {
                command: entry.command.clone().unwrap_or_default(),
                exit_code: entry.exit_code,
                order: entry.order,
            })
            .collect()
    }
}

/// The command of a `shell` call, or `None` for anything else. The `shell` tool
/// is the only assembled `Executes` tool, and its input is `{"command": …}`.
fn shell_command(call: &ToolCall) -> Option<String> {
    let raw = match &call.input {
        ToolInput::Json(raw) => raw,
        ToolInput::Text(_) => return None,
    };
    let value: serde_json::Value = serde_json::from_str(raw).ok()?;
    value.get("command")?.as_str().map(str::to_string)
}

/// The `[exit code: N]` footer the `shell` tool appends. Absent → `None`, which
/// never counts as success.
fn parse_exit_code(content: &str) -> Option<i32> {
    const MARKER: &str = "[exit code: ";
    let start = content.rfind(MARKER)? + MARKER.len();
    let rest = &content[start..];
    let end = rest.find(']')?;
    rest[..end].trim().parse().ok()
}

/// Forwards every event to the renderer unchanged, and records tool activity into
/// one agent's [`ActivityLog`]. The assembled tools are held here so the effect of
/// each call comes from the tool that will actually run it.
pub struct ActivityTee {
    inner: Arc<dyn EventSink>,
    log: Arc<ActivityLog>,
    tools: HashMap<String, Arc<dyn Tool>>,
}

impl ActivityTee {
    pub fn new(inner: Arc<dyn EventSink>, log: Arc<ActivityLog>, tools: &[Arc<dyn Tool>]) -> Self {
        let tools = tools
            .iter()
            .map(|tool| (tool.declaration().name.clone(), tool.clone()))
            .collect();
        Self { inner, log, tools }
    }
}

impl EventSink for ActivityTee {
    fn emit(&self, event: AgentEvent) {
        match &event {
            AgentEvent::ToolStarted { call } => {
                let effect = self
                    .tools
                    .get(&call.name)
                    .map_or(Effect::ReadOnly, |tool| tool.effect(call));
                self.log.record_started(call, effect);
            }
            AgentEvent::ToolFinished { result } => self.log.record_finished(result),
            _ => {}
        }
        self.inner.emit(event);
    }
}

/// One agent's completion state: the activity it runs against and the cell its
/// `finish` tool writes.
#[derive(Clone)]
pub struct Completion {
    pub log: Arc<ActivityLog>,
    pub outcome: FinishOutcome,
}

/// Issues one [`Completion`] per assembly and hands it to the host. The catalog's
/// `finish` factory calls [`CompletionHub::issue`]; the host calls
/// [`CompletionHub::take`] for the agent it just assembled.
#[derive(Default)]
pub struct CompletionHub {
    last: Mutex<Option<Completion>>,
}

impl CompletionHub {
    pub fn new() -> Self {
        Self::default()
    }

    /// Build the completion for the assembly that is starting.
    pub fn issue(&self) -> Completion {
        let completion = Completion {
            log: Arc::new(ActivityLog::default()),
            outcome: FinishOutcome::default(),
        };
        *self.last.lock().unwrap() = Some(completion.clone());
        completion
    }

    /// The completion issued for the assembly that just finished, if its
    /// environment assembled the `finish` tool.
    pub fn take(&self) -> Option<Completion> {
        self.last.lock().unwrap().take()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use p1_contracts::{
        AssistantBlock, AssistantItem, Origin, RecordBody, StopReason, ToolIdentity,
    };
    use p1_testkit::FakeTool;

    fn result(call_id: &str, name: &str, status: ToolStatus, content: &str) -> ToolResultItem {
        ToolResultItem {
            call_id: call_id.into(),
            name: name.into(),
            status,
            content: content.into(),
        }
    }

    fn call(call_id: &str, name: &str, raw: &str) -> ToolCall {
        ToolCall {
            call_id: call_id.into(),
            name: name.into(),
            input: ToolInput::Json(raw.into()),
        }
    }

    fn assistant(calls: Vec<ToolCall>) -> JournalRecord {
        JournalRecord {
            seq: 0,
            body: RecordBody::AssistantCompleted {
                item: AssistantItem {
                    origin: Origin {
                        route: "r".into(),
                        model: "m".into(),
                    },
                    blocks: calls.into_iter().map(AssistantBlock::ToolCall).collect(),
                },
                stop: StopReason::ToolUse,
                usage: None,
            },
        }
    }

    fn started(call_id: &str) -> JournalRecord {
        JournalRecord {
            seq: 0,
            body: RecordBody::ToolStarted {
                call_id: call_id.into(),
                identity: ToolIdentity {
                    implementation: "p1-tool".into(),
                    variant: "claude".into(),
                },
            },
        }
    }

    fn finished(call_id: &str, name: &str, status: ToolStatus, content: &str) -> JournalRecord {
        JournalRecord {
            seq: 0,
            body: RecordBody::ToolFinished {
                result: result(call_id, name, status, content),
            },
        }
    }

    #[test]
    fn replay_rebuilds_file_changes_and_shell_runs_in_order() {
        let log = ActivityLog::default();
        let write = call("w", "write", r#"{"file_path":"a","content":"x"}"#);
        let shell = call("s", "shell", r#"{"command":"cargo test"}"#);
        let tools: Vec<Arc<dyn Tool>> = vec![
            Arc::new(FakeTool::new("write").with_effect(Effect::WritesFiles)),
            Arc::new(FakeTool::new("shell").with_effect(Effect::Executes)),
        ];
        let records = vec![
            assistant(vec![write]),
            started("w"),
            finished("w", "write", ToolStatus::Ok, "Wrote a (1 bytes)."),
            assistant(vec![shell]),
            started("s"),
            finished("s", "shell", ToolStatus::Ok, "ok\n[exit code: 0]"),
        ];

        log.replay(&tools, &records);

        assert_eq!(log.last_file_change(), Some(1));
        assert_eq!(
            log.shell_runs(),
            vec![ShellRun {
                command: "cargo test".into(),
                exit_code: Some(0),
                order: 2,
            }]
        );
        assert_eq!(log.non_finish_finishes(), 2);
    }

    #[test]
    fn replay_treats_a_tool_that_no_longer_exists_as_not_writing() {
        let log = ActivityLog::default();
        let vanish = call("v", "vanish", r#"{"path":"a"}"#);
        let records = vec![
            assistant(vec![vanish]),
            started("v"),
            finished("v", "vanish", ToolStatus::Ok, "did something"),
        ];

        // No assembled tool named `vanish`: its effect is unknown, so it is NOT a
        // file change and NOT a shell run.
        log.replay(&[], &records);

        assert_eq!(log.last_file_change(), None);
        assert!(log.shell_runs().is_empty());
        assert_eq!(log.non_finish_finishes(), 1);
    }

    #[test]
    fn parses_the_shell_exit_footer_and_rejects_anything_else() {
        assert_eq!(parse_exit_code("out\n[exit code: 0]"), Some(0));
        assert_eq!(parse_exit_code("out\n[exit code: 128]"), Some(128));
        assert_eq!(parse_exit_code("out\n[timed out after 1 s]"), None);
        assert_eq!(parse_exit_code("out"), None);
    }

    #[test]
    fn finished_calls_get_increasing_orders_and_drive_both_queries() {
        let log = ActivityLog::default();
        log.record_started(
            &ToolCall {
                call_id: "w".into(),
                name: "write".into(),
                input: ToolInput::Json("{}".into()),
            },
            Effect::WritesFiles,
        );
        log.record_finished(&result("w", "write", ToolStatus::Ok, "Wrote a (1 bytes)."));
        log.record_started(
            &ToolCall {
                call_id: "s".into(),
                name: "shell".into(),
                input: ToolInput::Json(r#"{"command":"cargo test"}"#.into()),
            },
            Effect::Executes,
        );
        log.record_finished(&result("s", "shell", ToolStatus::Ok, "ok\n[exit code: 0]"));

        assert_eq!(log.last_file_change(), Some(1));
        assert_eq!(
            log.shell_runs(),
            vec![ShellRun {
                command: "cargo test".into(),
                exit_code: Some(0),
                order: 2,
            }]
        );
    }

    #[test]
    fn denied_and_failed_calls_are_not_file_changes_or_runs() {
        let log = ActivityLog::default();
        for (call_id, name, effect) in [
            ("w", "write", Effect::WritesFiles),
            ("s", "shell", Effect::Executes),
        ] {
            log.record_started(
                &ToolCall {
                    call_id: call_id.into(),
                    name: name.into(),
                    input: ToolInput::Json(r#"{"command":"x"}"#.into()),
                },
                effect,
            );
            log.record_finished(&result(call_id, name, ToolStatus::Denied, "denied"));
        }
        assert_eq!(log.last_file_change(), None);
        assert!(log.shell_runs().is_empty());
        // They still count as finished for the progress signal.
        assert_eq!(log.non_finish_finishes(), 2);
    }

    #[test]
    fn finish_calls_do_not_count_as_progress() {
        let log = ActivityLog::default();
        log.set_finish_name("finish".into());
        log.record_started(
            &ToolCall {
                call_id: "f".into(),
                name: "finish".into(),
                input: ToolInput::Json("{}".into()),
            },
            Effect::ReadOnly,
        );
        log.record_finished(&result("f", "finish", ToolStatus::Error, "nope"));
        log.record_started(
            &ToolCall {
                call_id: "r".into(),
                name: "read".into(),
                input: ToolInput::Json("{}".into()),
            },
            Effect::ReadOnly,
        );
        log.record_finished(&result("r", "read", ToolStatus::Ok, "contents"));
        assert_eq!(log.non_finish_finishes(), 1);
    }
}
