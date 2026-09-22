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

#[cfg(feature = "delegation")]
use p1_workers::{FinishReport, WorkerReport};

#[cfg(feature = "delegation")]
use crate::frontend::FrontEnd;

/// The `finish` tool's identity implementation. A worker's `finish` call is found by
/// this — never by its model-facing name, which a face may change.
#[cfg(feature = "delegation")]
const FINISH_IMPLEMENTATION: &str = "p1-tool-finish";

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
    /// Committed `ContextReplaced` events, and how many had been committed at the
    /// last progress. The difference is the host's §3c idle-summary count.
    replacements: AtomicU64,
    replacements_at_progress: AtomicU64,
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
        // §3c progress: a workspace mutation, or a `finish` call of any status.
        let is_finish = self
            .finish_name
            .lock()
            .unwrap()
            .as_deref()
            .is_some_and(|name| name == result.name);
        if is_finish || (effect == Effect::WritesFiles && result.status == ToolStatus::Ok) {
            self.note_progress();
        }
    }

    /// One committed context replacement (completion.md §3c). Counted by the host
    /// as it sees the `ContextReplaced` event; a resumed run rebuilds nothing here,
    /// so its counter starts at zero.
    pub fn record_replacement(&self) {
        self.replacements.fetch_add(1, Ordering::SeqCst);
    }

    /// The replacements committed since the last progress (completion.md §3c).
    pub fn consecutive_replacements(&self) -> u64 {
        self.replacements
            .load(Ordering::SeqCst)
            .saturating_sub(self.replacements_at_progress.load(Ordering::SeqCst))
    }

    fn note_progress(&self) {
        let replacements = self.replacements.load(Ordering::SeqCst);
        self.replacements_at_progress
            .store(replacements, Ordering::SeqCst);
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
/// each call comes from the tool that will actually run it — behind a lock, because a
/// re-grant replaces that set (ADR-0050 item 6).
pub struct ActivityTee {
    inner: Arc<dyn EventSink>,
    log: Arc<ActivityLog>,
    tools: Mutex<HashMap<String, Arc<dyn Tool>>>,
}

impl ActivityTee {
    pub fn new(inner: Arc<dyn EventSink>, log: Arc<ActivityLog>, tools: &[Arc<dyn Tool>]) -> Self {
        let tee = Self {
            inner,
            log,
            tools: Mutex::new(HashMap::new()),
        };
        tee.retool(tools);
        tee
    }

    /// The agent was re-assembled with another tool set (ADR-0050 item 6): the effect
    /// of every later call comes from the tool that will actually run it, so a
    /// re-granted `write` still counts as a file change and a re-granted `shell` run
    /// still counts for `finish`'s verification.
    pub fn retool(&self, tools: &[Arc<dyn Tool>]) {
        *self.tools.lock().unwrap() = tools
            .iter()
            .map(|tool| (tool.declaration().name.clone(), tool.clone()))
            .collect();
    }
}

impl EventSink for ActivityTee {
    fn emit(&self, event: AgentEvent) {
        match &event {
            AgentEvent::ToolStarted { call } => {
                let tool = self.tools.lock().unwrap().get(&call.name).cloned();
                let effect = tool.map_or(Effect::ReadOnly, |tool| tool.effect(call));
                self.log.record_started(call, effect);
            }
            AgentEvent::ToolFinished { result } => self.log.record_finished(result),
            _ => {}
        }
        self.inner.emit(event);
    }
}

/// The child's worker report tap (ADR-0050 item 6). It sits in the child's sink
/// chain, keeps one [`WorkerReport`] up to date from the child's own events, hands
/// that cell to the service through the child's `report` closure, and tells the
/// front end when a turn ends — so a worker's end is visible even when the parent
/// never asks.
///
/// It reads the child's events only:
/// - `tools` is the assembled tool names, known here at construction;
/// - a call to a tool the child does NOT have is answered by the core with a
///   `ToolFinished` of status `Unavailable` — no `ToolStarted` is emitted for it —
///   and its name is on that result, so the tap counts result names;
/// - a successful call of the `finish` tool is identified by the tool's identity
///   implementation, and its JSON input (kept from its `ToolStarted`) is what says
///   `status`, `needs` and `summary`: the tool's own content does not carry them.
#[cfg(feature = "delegation")]
pub struct WorkerReportTap {
    inner: Arc<dyn EventSink>,
    /// Shared with the child's `report` closure, so the service snapshots the same
    /// value the front end was told.
    report: Arc<Mutex<WorkerReport>>,
    /// Model-facing names of this child's `finish` tool (a face may rename it). A
    /// re-grant replaces the child's tool set, so this is behind a lock.
    finish_names: Mutex<std::collections::HashSet<String>>,
    /// The input of a `finish` call that has started, by call id, until its result
    /// says whether it was accepted.
    pending_finish: Mutex<HashMap<String, String>>,
    front_end: Arc<dyn FrontEnd>,
    worker_id: String,
    description: String,
}

#[cfg(feature = "delegation")]
impl WorkerReportTap {
    /// `tools` are the child's ASSEMBLED tools: their model-facing names become the
    /// report's `tools`, and the one whose identity implementation is `finish` is the
    /// one whose calls are read. `description` is the child's route/model, shown by
    /// the front end.
    pub fn new(
        inner: Arc<dyn EventSink>,
        report: Arc<Mutex<WorkerReport>>,
        tools: &[Arc<dyn Tool>],
        front_end: Arc<dyn FrontEnd>,
        worker_id: String,
        description: String,
    ) -> Self {
        let tap = Self {
            inner,
            report,
            finish_names: Mutex::new(std::collections::HashSet::new()),
            pending_finish: Mutex::new(HashMap::new()),
            front_end,
            worker_id,
            description,
        };
        tap.retool(tools);
        tap
    }

    /// The child was re-assembled with a larger grant (ADR-0050 item 6): the report's
    /// `tools` becomes the new assembly's model-facing names, and the `finish` tool is
    /// found again by its identity implementation, so a new tool set keeps reporting
    /// correctly. The report's other fields (the `finish` of the turn, the missing
    /// calls) are the turn's, and stay.
    pub fn retool(&self, tools: &[Arc<dyn Tool>]) {
        let names: std::collections::HashSet<String> = tools
            .iter()
            .filter(|tool| tool.identity().implementation == FINISH_IMPLEMENTATION)
            .map(|tool| tool.declaration().name.clone())
            .collect();
        *self.finish_names.lock().unwrap() = names;
        self.report.lock().unwrap().tools = tools
            .iter()
            .map(|tool| tool.declaration().name.clone())
            .collect();
    }
}

#[cfg(feature = "delegation")]
impl EventSink for WorkerReportTap {
    fn emit(&self, event: AgentEvent) {
        let turn_finished = matches!(event, AgentEvent::TurnFinished { .. });
        match &event {
            // A continue is a new turn: everything but the granted tools starts over.
            AgentEvent::TurnStarted => self.report.lock().unwrap().reset_turn(),
            AgentEvent::ToolStarted { call } => {
                if self.finish_names.lock().unwrap().contains(&call.name) {
                    self.pending_finish
                        .lock()
                        .unwrap()
                        .insert(call.call_id.clone(), call.input.raw().to_string());
                }
            }
            AgentEvent::ToolFinished { result } => {
                let input = self.pending_finish.lock().unwrap().remove(&result.call_id);
                let mut report = self.report.lock().unwrap();
                if result.status == ToolStatus::Unavailable {
                    report.note_missing_tool(&result.name);
                } else if result.status == ToolStatus::Ok
                    && let Some(input) = input
                    && let Some(finish) = parse_finish_call(&input)
                {
                    // "its LAST successful finish call this turn": an accepted call
                    // replaces an earlier one.
                    report.finish = Some(finish);
                }
            }
            _ => {}
        }
        // Every event reaches the child's own rendering unchanged, in order.
        self.inner.emit(event);
        if turn_finished {
            let report = self.report.lock().unwrap().clone();
            self.front_end
                .worker_ended(&self.worker_id, &self.description, &report);
        }
    }
}

/// The `status`, `needs` and `summary` of one `finish` call's input. `needs` may be
/// a string or an array (joined with `", "`); a blank `needs` is none. Input that is
/// not an object with a string `status` yields no report — the tool rejected it too.
#[cfg(feature = "delegation")]
fn parse_finish_call(raw: &str) -> Option<FinishReport> {
    let value: serde_json::Value = serde_json::from_str(raw).ok()?;
    let status = value.get("status")?.as_str()?.to_string();
    let needs = match value.get("needs") {
        Some(serde_json::Value::String(needs)) => Some(needs.clone()),
        Some(serde_json::Value::Array(items)) => Some(
            items
                .iter()
                .filter_map(|item| item.as_str())
                .collect::<Vec<_>>()
                .join(", "),
        ),
        _ => None,
    }
    .filter(|needs| !needs.trim().is_empty());
    let summary = value
        .get("summary")
        .and_then(|summary| summary.as_str())
        .map(str::to_string);
    Some(FinishReport {
        status,
        needs,
        summary,
    })
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

    /// §3c: replacements accumulate until progress — a successful write or a
    /// `finish` call of any status — resets them. A failed write is not progress.
    #[test]
    fn replacements_reset_on_a_mutation_or_finish_and_survive_a_failed_write() {
        let log = ActivityLog::default();
        log.set_finish_name("finish".into());
        log.record_replacement();
        log.record_replacement();
        assert_eq!(log.consecutive_replacements(), 2);

        log.record_started(
            &call("w", "write", r#"{"file_path":"a","content":"x"}"#),
            Effect::WritesFiles,
        );
        log.record_finished(&result("w", "write", ToolStatus::Ok, "Wrote a (1 bytes)."));
        assert_eq!(log.consecutive_replacements(), 0);

        log.record_replacement();
        assert_eq!(log.consecutive_replacements(), 1);
        // A finish call of ANY status is progress.
        log.record_started(&call("f", "finish", "{}"), Effect::ReadOnly);
        log.record_finished(&result("f", "finish", ToolStatus::Error, "nope"));
        assert_eq!(log.consecutive_replacements(), 0);

        log.record_replacement();
        log.record_started(
            &call("w2", "write", r#"{"file_path":"a","content":"x"}"#),
            Effect::WritesFiles,
        );
        log.record_finished(&result("w2", "write", ToolStatus::Error, "denied"));
        assert_eq!(log.consecutive_replacements(), 1);
    }

    // ------------------------------------------------------ the worker report tap

    /// A front end that records the worker ends it was told about and renders
    /// nothing.
    #[cfg(feature = "delegation")]
    #[derive(Default)]
    struct RecordingWorkerEnds {
        ended: Mutex<Vec<(String, String, WorkerReport)>>,
    }

    #[cfg(feature = "delegation")]
    impl FrontEnd for RecordingWorkerEnds {
        fn event_sink(&self) -> Arc<dyn EventSink> {
            Arc::new(p1_testkit::RecordingEvents::new())
        }

        fn child_event_sink(
            &self,
            _worker_id: &str,
            _route: &str,
            _model: &str,
        ) -> Arc<dyn EventSink> {
            Arc::new(p1_testkit::RecordingEvents::new())
        }

        fn child_started(&self, _worker_id: &str) {}

        fn authorization(&self) -> Arc<dyn p1_contracts::AuthorizationPolicy> {
            Arc::new(p1_testkit::ScriptedAuthorization::permit_all())
        }

        fn parent_assembled(&self, _route: &str, _model: &str, _completion: Option<Completion>) {}

        fn worker_ended(&self, worker_id: &str, description: &str, report: &WorkerReport) {
            self.ended.lock().unwrap().push((
                worker_id.to_string(),
                description.to_string(),
                report.clone(),
            ));
        }

        fn run<'a>(
            &'a self,
            _deps: &'a crate::HostDeps,
            _agent: &'a mut p1_core::Agent,
            _cancel: &'a p1_contracts::CancellationToken,
            _workers: Option<Arc<dyn crate::frontend::WorkerService>>,
            _stall: Option<Arc<crate::run::StallGuard>>,
        ) -> p1_contracts::BoxFuture<'a, i32> {
            Box::pin(async { 0 })
        }

        fn finish(&self) {}
    }

    /// The path the tap relies on: for a call to a tool the agent does NOT have, the
    /// core emits NO `ToolStarted` — and its `ToolFinished` carries the call's name
    /// (taken from the assistant item's tool-call block), which is what the tap
    /// counts. One `finish` call's input is read from its `ToolStarted`, because the
    /// result's content does not carry `status`/`needs`/`summary`.
    #[cfg(feature = "delegation")]
    #[tokio::test]
    async fn an_unavailable_call_is_counted_from_its_finished_result_and_finish_from_its_start() {
        use p1_contracts::{ModelOptions, ToolStatus};
        use p1_core::{Agent, AgentParts};
        use p1_testkit::{
            PassthroughContext, RecordingEvents, RecordingJournal, ScriptedAuthorization,
            ScriptedProvider, json_call, text_response, tool_call_response,
        };

        let tools: Vec<Arc<dyn Tool>> = vec![
            Arc::new(FakeTool::new("read")),
            // The `finish` tool is found by its identity implementation, not by name.
            Arc::new(FakeTool::new("finish").with_identity(FINISH_IMPLEMENTATION, "claude")),
        ];
        let raw = Arc::new(RecordingEvents::new());
        let report = Arc::new(Mutex::new(WorkerReport::new(vec![
            "read".to_string(),
            "finish".to_string(),
        ])));
        let front_end = Arc::new(RecordingWorkerEnds::default());
        let tap = Arc::new(WorkerReportTap::new(
            raw.clone(),
            report.clone(),
            &tools,
            front_end.clone(),
            "w1".to_string(),
            "route/model".to_string(),
        ));

        let provider = ScriptedProvider::new(vec![
            // A tool this worker was not given, twice, then an accepted `finish`.
            tool_call_response(vec![
                json_call("c1", "edit", r#"{"file_path":"a"}"#),
                json_call("c2", "edit", r#"{"file_path":"b"}"#),
            ]),
            tool_call_response(vec![json_call(
                "c3",
                "finish",
                r#"{"status":"blocked","summary":"needs a writer","needs":["edit","write"]}"#,
            )]),
            text_response("gave up"),
        ]);
        let parts = AgentParts {
            provider: Arc::new(provider),
            tools,
            system_prompt: "child".to_string(),
            options: ModelOptions::default(),
            context: Arc::new(PassthroughContext),
            authorization: Arc::new(ScriptedAuthorization::permit_all()),
            journal: Arc::new(RecordingJournal::new()),
            events: tap.clone(),
        };
        let mut agent = Agent::new(parts).expect("agent builds");
        agent
            .run_turn("go".to_string(), p1_contracts::CancellationToken::new())
            .await;

        // No `ToolStarted` for `edit`: the core answers an unknown name with a
        // result only, and that result names the call.
        let events = raw.events();
        assert!(
            !events.iter().any(
                |event| matches!(event, AgentEvent::ToolStarted { call } if call.name == "edit")
            ),
            "the core must not emit ToolStarted for an unassembled tool: {events:?}"
        );
        assert!(
            events.iter().any(|event| matches!(
                event,
                AgentEvent::ToolFinished { result }
                    if result.name == "edit" && result.status == ToolStatus::Unavailable
            )),
            "the unavailable result carries the call's name: {events:?}"
        );

        let snapshot = report.lock().unwrap().clone();
        assert_eq!(
            snapshot.missing_tool_calls,
            vec![("edit".to_string(), 2)],
            "first-seen order, one count per call"
        );
        assert_eq!(
            snapshot.finish,
            Some(FinishReport {
                status: "blocked".to_string(),
                // The array form joins with ", ".
                needs: Some("edit, write".to_string()),
                summary: Some("needs a writer".to_string()),
            })
        );
        assert_eq!(
            snapshot.tools,
            vec!["read".to_string(), "finish".to_string()]
        );

        // The front end heard the end of the turn, once, with that snapshot.
        let ended = front_end.ended.lock().unwrap().clone();
        assert_eq!(ended.len(), 1, "one turn, one end: {ended:?}");
        assert_eq!(ended[0].0, "w1");
        assert_eq!(ended[0].1, "route/model");
        assert_eq!(ended[0].2, snapshot);
    }

    /// Only a SUCCESSFUL `finish` call counts, and a new turn starts over: an error
    /// finish leaves `finish` empty, and a `TurnStarted` clears what the last turn
    /// recorded while keeping the granted tools.
    #[cfg(feature = "delegation")]
    #[test]
    fn the_tap_keeps_only_a_successful_finish_and_resets_each_turn() {
        let tools: Vec<Arc<dyn Tool>> = vec![Arc::new(
            FakeTool::new("finish").with_identity(FINISH_IMPLEMENTATION, "claude"),
        )];
        let report = Arc::new(Mutex::new(WorkerReport::new(vec!["finish".to_string()])));
        let tap = WorkerReportTap::new(
            Arc::new(p1_testkit::RecordingEvents::new()),
            report.clone(),
            &tools,
            Arc::new(RecordingWorkerEnds::default()),
            "w1".to_string(),
            "route/model".to_string(),
        );

        tap.emit(AgentEvent::TurnStarted);
        tap.emit(AgentEvent::ToolStarted {
            call: call(
                "f1",
                "finish",
                r#"{"status":"blocked","summary":"stuck","needs":"edit"}"#,
            ),
        });
        tap.emit(AgentEvent::ToolFinished {
            result: result("f1", "finish", ToolStatus::Error, "Say what you need"),
        });
        tap.emit(AgentEvent::ToolFinished {
            result: result("nope", "grep", ToolStatus::Unavailable, "not available"),
        });
        assert_eq!(
            report.lock().unwrap().finish,
            None,
            "a rejected call is not it"
        );
        assert_eq!(
            report.lock().unwrap().missing_tool_calls,
            vec![("grep".to_string(), 1)]
        );

        // The next turn starts from nothing but the granted tools.
        tap.emit(AgentEvent::TurnStarted);
        let fresh = report.lock().unwrap().clone();
        assert_eq!(fresh.finish, None);
        assert!(fresh.missing_tool_calls.is_empty());
        assert_eq!(fresh.tools, vec!["finish".to_string()]);
    }

    #[cfg(feature = "delegation")]
    #[test]
    fn finish_input_is_read_as_a_string_or_an_array() {
        let report =
            parse_finish_call(r#"{"status":"blocked","summary":"s","needs":["edit","write"]}"#)
                .unwrap();
        assert_eq!(report.needs.as_deref(), Some("edit, write"));
        let report = parse_finish_call(r#"{"status":"done","summary":"s","needs":""}"#).unwrap();
        assert_eq!(report.needs, None, "a blank needs is none");
        let report = parse_finish_call(r#"{"status":"done"}"#).unwrap();
        assert_eq!(
            report,
            FinishReport {
                status: "done".to_string(),
                needs: None,
                summary: None,
            }
        );
        assert_eq!(parse_finish_call("not json"), None);
        assert_eq!(parse_finish_call(r#"{"summary":"s"}"#), None);
    }
}
