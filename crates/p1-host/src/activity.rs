//! The host's session-completion state: the session record ([`ActivityLog`]) fed
//! from the event stream, the tee that feeds it, and the per-agent handoff of the
//! `finish` tool's outcome ([`FinishOutcome`], the cell only the hub writes).
//!
//! The tee forwards EVERY event to the renderer unchanged (the renderer keeps its
//! exact behaviour) and records finished calls into an [`ActivityLog`]: the order
//! of the last `WritesFiles` call and every `Executes` run with its parsed shell
//! exit code (host-observed for evidence tools). The log is ONE owner per agent — a worker gets its own — and nothing
//! here is global.
//!
//! The `finish` tool instance is built by the catalog, before the host knows the
//! assembled tools or has a renderer to tee. [`CompletionHub`] is the seam: the
//! catalog issues a fresh `(log, outcome)` pair per assembly; the host takes the
//! pair for the agent it just assembled and installs the tee with that agent's
//! tools. Child assemblies are serialised by the worker service, so a take always
//! returns the pair belonging to the assembly that just ran.
//!
//! For the `p1/finish` component the hub is also the `completion` capability
//! (ADR-0083 §2): the accepted state stays the host's. The component reads the record,
//! checks a call and submits a candidate; the hub re-verifies every candidate against
//! its own record ([`ActivityLog::evidence_runs`], the last file change, the policy it
//! chose, the contract it holds) and commits only what passes, and [`CompletionGate`]
//! turns a refusal into the call's tool error.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};

use p1_contracts::tool::ResultDescription;
use p1_contracts::{
    AgentEvent, BoxFuture, CallDescription, DeclarationKind, Effect, EventSink, JournalRecord,
    RecordBody, Tool, ToolCall, ToolContext, ToolDeclaration, ToolIdentity, ToolInput, ToolOutcome,
    ToolResultItem, ToolStatus,
};
use p1_finish_guest::{
    Accepted, CompletionPolicy, Evidence, OutputContract, ShellRun, StructuredResult,
};
use p1_module_runtime::completion::{
    Candidate, CompletionPolicy as WirePolicy, Evidence as CandidateEvidence, ShellRun as WireRun,
    StructuredResult as WireStructured,
};
use p1_module_runtime::{
    CompletionService, ExecutionLimits, LoadedModule, Services, ToolError, wasm_tool,
};
use p1_redact::{MaskCounter, redacted};

use crate::fingerprint::{self, Fingerprint, FingerprintError};

#[cfg(feature = "delegation")]
use p1_workers::{FinishReport, WorkerReport};

use crate::catalog::capabilities::{SemanticCapability, carries};
#[cfg(feature = "delegation")]
use crate::frontend::FrontEnd;

/// One finished tool call, in finish order.
struct Finished {
    name: String,
    effect: Effect,
    status: ToolStatus,
    order: u64,
    command: Option<String>,
    exit_code: Option<i32>,
    /// The tool that ran the call records command evidence (ADR-0083 rule 2): only such a
    /// run can verify a completion the hub commits.
    records_evidence: bool,
    /// ADR-0055: this successful `Executes` call changed the workspace. It is a file
    /// change for the `finish` check and progress for the stall guard, exactly as a
    /// `WritesFiles` call is.
    changed_workspace: bool,
}

/// What was learned about a call when it started, keyed by call id.
struct Pending {
    effect: Effect,
    command: Option<String>,
    records_evidence: bool,
    synthetic: bool,
    background: bool,
}

/// Where a command's workspace changes are measured (ADR-0055), and what the last
/// fingerprint was.
struct Watch {
    workspace: PathBuf,
    /// The paths the host itself appends to (its session journals).
    ignored: Vec<PathBuf>,
    /// The fingerprint after the last successful `Executes` call — or the baseline the
    /// first such call took. `None` until that baseline exists.
    last: Option<Fingerprint>,
}

/// The host's session record, what a `finish` call is checked against. One per agent;
/// fed only through [`ActivityTee`].
#[derive(Default)]
pub struct ActivityLog {
    next_order: AtomicU64,
    // Background ends race foreground completion; baseline comparison and publication
    // must see the same completed file-change history.
    completion: Mutex<()>,
    pending: Mutex<HashMap<String, Pending>>,
    finished: Mutex<Vec<Finished>>,
    /// The model-facing name of this agent's `finish` tool, set once the catalog
    /// built it. Progress counts every OTHER finished call.
    finish_name: Mutex<Option<String>>,
    /// Committed `ContextReplaced` events, and how many had been committed at the
    /// last progress. The difference is the host's §3c idle-summary count.
    replacements: AtomicU64,
    replacements_at_progress: AtomicU64,
    /// The workspace a command's changes are measured in (ADR-0055). `None` until the
    /// host sets it, so a log that rebuilds a resumed session is never fingerprinted:
    /// the journal does not carry what a past command did to the workspace.
    watch: Mutex<Option<Watch>>,
    /// The FIRST workspace-fingerprint error of this session (ADR-0055 item 4): the
    /// host falls back to the tool-declared rule, and says so once.
    fingerprint_error: Mutex<Option<String>>,
}

impl ActivityLog {
    /// Observe the verification baseline of a background command.
    pub(crate) fn job_started(&self) -> Option<u64> {
        let _completion = self.completion.lock().unwrap();
        self.take_baseline();
        self.last_file_change()
    }

    pub(crate) fn job_finished(&self, command: String, baseline: Option<u64>, exit: Option<i32>) {
        let _completion = self.completion.lock().unwrap();
        let changed_workspace = self.workspace_changed();
        let exit_code =
            (exit == Some(0) && !changed_workspace && baseline == self.last_file_change())
                .then_some(0);
        let order = self.next_order.fetch_add(1, Ordering::SeqCst) + 1;
        self.finished.lock().unwrap().push(Finished {
            name: "background job".into(),
            effect: Effect::Executes,
            status: ToolStatus::Ok,
            order,
            command: Some(command),
            exit_code,
            records_evidence: true,
            changed_workspace,
        });
        if changed_workspace {
            self.note_progress();
        }
    }

    /// Record a call that is about to run, with the effect its assembled tool
    /// classified. The command is read from a `shell` call's input so a later
    /// `finish` can compare named commands.
    pub fn record_started(&self, call: &ToolCall, effect: Effect) {
        self.record_started_by(call, effect, false);
    }

    /// As [`ActivityLog::record_started`], saying whether the tool that runs the call
    /// records command evidence ([`records_command_evidence`]): the host knows that from
    /// the tool's loader-built or native identity, never from the call.
    pub fn record_started_by(&self, call: &ToolCall, effect: Effect, records_evidence: bool) {
        self.record_started_with_origin(call, effect, records_evidence, false);
    }

    fn record_started_with_origin(
        &self,
        call: &ToolCall,
        effect: Effect,
        records_evidence: bool,
        synthetic: bool,
    ) {
        let command = match effect {
            Effect::Executes => shell_command(call),
            _ => None,
        };
        self.pending.lock().unwrap().insert(
            call.call_id.clone(),
            Pending {
                effect,
                command,
                records_evidence,
                synthetic,
                background: matches!(&call.input, ToolInput::Json(raw) if serde_json::from_str::<serde_json::Value>(raw).is_ok_and(|v| v["background"] == true)),
            },
        );
        if effect == Effect::Executes {
            self.take_baseline();
        }
    }

    /// Record a finished call. `order` is assigned here, monotonically.
    pub fn record_finished(&self, result: &ToolResultItem) {
        self.record_finished_with_exit(result, None);
    }

    /// `observed_exit` comes from the process capability, never guest output.
    pub fn record_finished_with_exit(&self, result: &ToolResultItem, observed_exit: Option<i32>) {
        self.record_finished_inner(result, observed_exit, false);
    }

    /// A `ToolFinished` replayed from a journal written before the host recorded
    /// exits (`exit_code` absent): for a tool that records command evidence the
    /// footer WAS the host's evidence format then, so it is read exactly as that
    /// host read it. A current journal carries `Some(..)`, so a component footer
    /// has no authority there.
    fn record_finished_from_legacy_journal(&self, result: &ToolResultItem) {
        self.record_finished_inner(result, None, true);
    }

    fn record_finished_inner(
        &self,
        result: &ToolResultItem,
        observed_exit: Option<i32>,
        legacy_footer: bool,
    ) {
        let _completion = self.completion.lock().unwrap();
        let pending = self.pending.lock().unwrap().remove(&result.call_id);
        let effect = pending.as_ref().map_or(Effect::ReadOnly, |p| p.effect);
        let records_evidence = pending.as_ref().is_some_and(|p| p.records_evidence);
        let synthetic = pending.as_ref().is_some_and(|p| p.synthetic);
        let background = pending.as_ref().is_some_and(|p| p.background);
        let command = pending.and_then(|p| p.command);
        let order = self.next_order.fetch_add(1, Ordering::SeqCst) + 1;
        let exit_code =
            if !background && effect == Effect::Executes && result.status == ToolStatus::Ok {
                if records_evidence {
                    // Only an explicit in-memory test double supplies synthetic exits,
                    // and only a journal from before host-observed exits may read the
                    // footer. Guest text has no authority when the tool is a module.
                    if synthetic || legacy_footer {
                        observed_exit.or_else(|| parse_exit_code(&result.content))
                    } else {
                        observed_exit
                    }
                } else {
                    parse_exit_code(&result.content)
                }
            } else {
                None
            };
        // ADR-0055: a successful command that changed the workspace is a file change
        // and progress, exactly as a `WritesFiles` call is. A failed command is never
        // fingerprinted, and an unchanged workspace changes nothing.
        let changed_workspace = !background
            && effect == Effect::Executes
            && result.status == ToolStatus::Ok
            && self.workspace_changed();
        self.finished.lock().unwrap().push(Finished {
            name: result.name.clone(),
            effect,
            status: result.status,
            order,
            command,
            exit_code,
            records_evidence,
            changed_workspace,
        });
        // §3c progress: a workspace mutation (a `WritesFiles` call or a command that
        // changed the workspace), or a `finish` call of any status.
        let is_finish = self
            .finish_name
            .lock()
            .unwrap()
            .as_deref()
            .is_some_and(|name| name == result.name);
        if is_finish
            || changed_workspace
            || (effect == Effect::WritesFiles && result.status == ToolStatus::Ok)
        {
            self.note_progress();
        }
    }

    /// Tell the log which directory a command's workspace changes are measured in,
    /// and which paths the host itself writes there (ADR-0055). The host calls this
    /// ONCE, after any replay: a replayed call is never fingerprinted, because the
    /// journal does not carry what a past command did.
    pub fn watch_workspace(&self, workspace: &Path, ignored: &[PathBuf]) {
        *self.watch.lock().unwrap() = Some(Watch {
            workspace: workspace.to_path_buf(),
            ignored: ignored.to_vec(),
            last: None,
        });
    }

    /// The first workspace-fingerprint error of this session, if any. The host prints
    /// it once, so a fallback to the tool-declared rule is never silent (ADR-0055
    /// item 4).
    pub fn fingerprint_error(&self) -> Option<String> {
        self.fingerprint_error.lock().unwrap().clone()
    }

    /// The baseline the first `Executes` call of the session takes: the fingerprint is
    /// computed once BEFORE the first command and after each successful one
    /// (ADR-0055 item 1), so the first command's own change is seen too.
    fn take_baseline(&self) {
        let mut watch = self.watch.lock().unwrap();
        let Some(watch) = watch.as_mut() else { return };
        if watch.last.is_some() {
            return;
        }
        match fingerprint::take_ignoring(&watch.workspace, &watch.ignored) {
            Ok(baseline) => watch.last = Some(baseline),
            Err(error) => self.remember_fingerprint_error(error),
        }
    }

    /// Fingerprint the workspace now and compare it with the last one (ADR-0055
    /// item 2). `false` — today's behaviour — whenever there is no workspace to
    /// measure, no baseline yet, or the fingerprint failed.
    fn workspace_changed(&self) -> bool {
        let mut watch = self.watch.lock().unwrap();
        let Some(watch) = watch.as_mut() else {
            return false;
        };
        match fingerprint::take_ignoring(&watch.workspace, &watch.ignored) {
            Ok(current) => {
                let changed = watch.last.is_some_and(|last| last != current);
                watch.last = Some(current);
                changed
            }
            Err(error) => {
                self.remember_fingerprint_error(error);
                false
            }
        }
    }

    fn remember_fingerprint_error(&self, error: FingerprintError) {
        let mut cell = self.fingerprint_error.lock().unwrap();
        if cell.is_none() {
            *cell = Some(error.to_string());
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
                    let tool = by_name.get(call.name.as_str());
                    let effect = tool.map_or(Effect::ReadOnly, |tool| tool.effect(call));
                    let evidence = tool.is_some_and(|tool| records_command_evidence(tool.as_ref()));
                    let synthetic = tool.is_some_and(|tool| tool.synthetic_command_result());
                    self.record_started_with_origin(call, effect, evidence, synthetic);
                }
                RecordBody::ToolFinished { result, exit_code } => match exit_code {
                    // The host wrote an observation for this call (possibly "no
                    // exit"): only it has authority over the footer.
                    Some(observed) => self.record_finished_with_exit(result, *observed),
                    // The field is absent: a journal older than host-observed exits,
                    // where the footer was the host's evidence format.
                    None => self.record_finished_from_legacy_journal(result),
                },
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

    /// The runs the completion hub counts as evidence (ADR-0083 rule 2): the finished
    /// `Executes` calls of a tool that records command evidence, ordered as
    /// [`ActivityLog::shell_runs`] orders every run.
    pub fn evidence_runs(&self) -> Vec<ShellRun> {
        self.runs(true)
    }

    /// The successful `Executes` runs, all of them or only those of an evidence tool.
    fn runs(&self, evidence_only: bool) -> Vec<ShellRun> {
        self.finished
            .lock()
            .unwrap()
            .iter()
            .filter(|entry| entry.effect == Effect::Executes && entry.status == ToolStatus::Ok)
            .filter(|entry| !evidence_only || entry.records_evidence)
            .map(|entry| ShellRun {
                command: entry.command.clone().unwrap_or_default(),
                exit_code: entry.exit_code,
                // ADR-0055 item 2, decided deliberately: a command that changed the
                // workspace IS a file change (its own order above), and its run is
                // reported one order PAST that change — the run and the change are one
                // record, and the change is what the run produced, not something that
                // happened before it. ADR-0037's rule ("a run older than the last file
                // change is stale") then keeps the changing command's own run counting,
                // which its acceptance test requires (`rm marker` stays a run that
                // counts after it removed the file), while EVERY earlier run is stale:
                // that is the point of the ADR — a `cargo test` before a heredoc write
                // must be repeated.
                order: if entry.changed_workspace {
                    entry.order + 1
                } else {
                    entry.order
                },
            })
            .collect()
    }
}

#[cfg(test)]
mod background_job_tests {
    use super::*;
    #[test]
    fn background_failed_cancelled_and_timed_out_ends_never_verify() {
        let log = ActivityLog::default();
        for exit in [Some(1), None, None] {
            log.job_finished("check".into(), log.job_started(), exit);
            assert_eq!(log.evidence_runs().last().unwrap().exit_code, None);
        }
        let dir = tempfile::tempdir().unwrap();
        log.watch_workspace(dir.path(), &[]);
        let baseline = log.job_started();
        std::fs::write(dir.path().join("changed"), "new").unwrap();
        log.job_finished("changed check".into(), baseline, Some(0));
        assert_eq!(log.evidence_runs().last().unwrap().exit_code, None);
        assert!(log.last_file_change().is_some());
        let resumed = ActivityLog::default();
        resumed.replay(&[], &[]);
        assert!(resumed.evidence_runs().is_empty());
    }
    #[test]
    fn background_success_counts_at_end_but_intervening_change_invalidates() {
        let log = ActivityLog::default();
        let baseline = log.job_started();
        assert!(log.evidence_runs().is_empty());
        log.job_finished("cargo test".into(), baseline, Some(0));
        assert_eq!(log.evidence_runs().len(), 1);
        let baseline = log.job_started();
        log.finished.lock().unwrap().push(Finished {
            name: "write".into(),
            effect: Effect::WritesFiles,
            status: ToolStatus::Ok,
            order: log.next_order.fetch_add(1, Ordering::SeqCst) + 1,
            command: None,
            exit_code: None,
            records_evidence: false,
            changed_workspace: false,
        });
        log.job_finished("cargo check".into(), baseline, Some(0));
        assert_eq!(log.evidence_runs().last().unwrap().exit_code, None);
        log.job_finished("failed".into(), log.job_started(), Some(1));
        assert_eq!(log.evidence_runs().last().unwrap().exit_code, None);
    }
}

impl ActivityLog {
    /// `order` of the last successful call that changed a file: a `WritesFiles` call, or
    /// an `Executes` call that changed the workspace (ADR-0055).
    pub fn last_file_change(&self) -> Option<u64> {
        self.finished
            .lock()
            .unwrap()
            .iter()
            .rev()
            .find(|entry| {
                entry.status == ToolStatus::Ok
                    && (entry.effect == Effect::WritesFiles || entry.changed_workspace)
            })
            .map(|entry| entry.order)
    }

    /// Every successful `Executes` run, whatever tool ran it, as `completion.shell-runs`
    /// reports them; the hub counts only [`ActivityLog::evidence_runs`].
    pub fn shell_runs(&self) -> Vec<ShellRun> {
        self.runs(false)
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
                let effect = tool
                    .as_ref()
                    .map_or(Effect::ReadOnly, |tool| tool.effect(call));
                let evidence = tool
                    .as_ref()
                    .is_some_and(|tool| records_command_evidence(tool.as_ref()));
                let synthetic = tool.is_some_and(|tool| tool.synthetic_command_result());
                self.log
                    .record_started_with_origin(call, effect, evidence, synthetic);
            }
            AgentEvent::ToolFinished { result } => {
                let tool = self.tools.lock().unwrap().get(&result.name).cloned();
                let exit = tool.and_then(|tool| tool.take_command_exit_code(&result.call_id));
                self.log.record_finished_with_exit(result, exit);
            }
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
/// - a successful call of the `finish` tool is identified by the `reports-completion`
///   capability of the tool's verified identity, and its JSON input (kept from its
///   `ToolStarted`) is what says
///   `status`, `needs` and `summary`: the tool's own content does not carry them;
/// - the EVIDENCE is not the model's word: it is read from the child's own
///   [`FinishOutcome`] cell, which the child's `finish` tool wrote from what it
///   accepted (ADR-0051 item 2/3).
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
    /// The child's `finish` outcome cell: where the accepted evidence is read. The
    /// child's `finish` tool (re-wrapped, never replaced, across a re-grant) writes
    /// this same cell for the child's whole life.
    outcome: FinishOutcome,
    front_end: Arc<dyn FrontEnd>,
    worker_id: String,
    description: String,
    /// A workflow step's end is shown by the workflow observer's step line, which
    /// knows the call, the role and the schema check; a second line would repeat it.
    silent_end: bool,
}

#[cfg(feature = "delegation")]
impl WorkerReportTap {
    /// `tools` are the child's ASSEMBLED tools: their model-facing names become the
    /// report's `tools`, and the one whose identity carries `reports-completion` is the
    /// one whose calls are read. `outcome` is the completion cell the child's `finish`
    /// tool writes — [`FinishOutcome::default`] when the child has no `finish` tool.
    /// `description` is the child's route/model, shown by the front end. With
    /// `silent_end` the front end is not told the turn ended (a workflow step).
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        inner: Arc<dyn EventSink>,
        report: Arc<Mutex<WorkerReport>>,
        tools: &[Arc<dyn Tool>],
        outcome: FinishOutcome,
        front_end: Arc<dyn FrontEnd>,
        worker_id: String,
        description: String,
        silent_end: bool,
    ) -> Self {
        let tap = Self {
            inner,
            report,
            finish_names: Mutex::new(std::collections::HashSet::new()),
            pending_finish: Mutex::new(HashMap::new()),
            outcome,
            front_end,
            worker_id,
            description,
            silent_end,
        };
        tap.retool(tools);
        tap
    }

    /// The child was re-assembled with a larger grant (ADR-0050 item 6): the report's
    /// `tools` becomes the new assembly's model-facing names, and the `finish` tool is
    /// found again by the `reports-completion` capability of its identity, so a new tool
    /// set keeps reporting
    /// correctly. The report's other fields (the `finish` of the turn, the missing
    /// calls) are the turn's, and stay.
    pub fn retool(&self, tools: &[Arc<dyn Tool>]) {
        let names: std::collections::HashSet<String> = tools
            .iter()
            .filter(|tool| carries(tool.as_ref(), SemanticCapability::ReportsCompletion))
            .map(|tool| tool.declaration().name.clone())
            .collect();
        *self.finish_names.lock().unwrap() = names;
        self.report.lock().unwrap().tools = tools
            .iter()
            .map(|tool| tool.declaration().name.clone())
            .collect();
    }

    /// What the child's ACCEPTED `finish` call established (ADR-0051 item 3), read
    /// from the child's own outcome cell: the model's input cannot produce it, and a
    /// `blocked` outcome — or none at all — has no evidence line.
    fn accepted_evidence(&self) -> Option<String> {
        match self.outcome.get()? {
            Accepted::Done { evidence, .. } => Some(evidence_text(&evidence)),
            Accepted::Blocked { .. } => None,
        }
    }
}

#[cfg(feature = "delegation")]
impl EventSink for WorkerReportTap {
    fn emit(&self, event: AgentEvent) {
        let turn_finished = matches!(event, AgentEvent::TurnFinished { .. });
        match &event {
            // A continue is a new turn: everything but the granted tools starts over,
            // and the previous turn's evidence is dropped with it.
            AgentEvent::TurnStarted => {
                self.report.lock().unwrap().reset_turn();
                self.outcome.clear();
            }
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
                    && let Some(mut finish) = parse_finish_call(&input)
                {
                    // "its LAST successful finish call this turn": an accepted call
                    // replaces an earlier one. The evidence comes from what the tool
                    // ACCEPTED, never from the input above.
                    finish.evidence = self.accepted_evidence();
                    report.finish = Some(finish);
                }
            }
            _ => {}
        }
        // Every event reaches the child's own rendering unchanged, in order.
        self.inner.emit(event);
        if turn_finished && !self.silent_end {
            let report = self.report.lock().unwrap().clone();
            self.front_end
                .worker_ended(&self.worker_id, &self.description, &report);
        }
    }
}

/// The one sentence a `done` is labelled with. `NotRun` is never printed as verified,
/// whatever it did not run (ADR-0051 item 3).
#[cfg(feature = "delegation")]
fn evidence_text(evidence: &Evidence) -> String {
    match evidence {
        Evidence::CommandsPassed(commands) => format!("commands passed: {}", commands.join(", ")),
        Evidence::NotRun(_) => "not verified; parent verification required".to_string(),
    }
}

/// The `status`, `needs` and `summary` of one `finish` call's input. `needs` may be
/// a string or an array (joined with `", "`); a blank `needs` is none. Input that is
/// not an object with a string `status` yields no report — the tool rejected it too.
/// The evidence is NOT read here: it is not the model's to state (ADR-0051 item 3).
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
        evidence: None,
    })
}

/// Both values an accepted call writes, under ONE lock, so the host can never read a
/// `get` and a `structured` that belong to different calls.
#[derive(Default)]
struct OutcomeCell {
    accepted: Option<Accepted>,
    structured: Option<StructuredResult>,
    /// Bumped by every [`FinishOutcome::set`], under the same lock as the values. The
    /// finish wrapper reads it before and after a call to learn whether THAT call
    /// accepted, instead of comparing the values: the cell is not cleared between
    /// interactive turns, so an identical accepted value from an earlier turn would
    /// compare equal (ADR-0120 point 2).
    generation: u64,
}

/// One agent's accepted `finish` outcome, which the host reads after a turn. Cheap to
/// clone; all clones share one value. Only the completion hub writes it, after it
/// re-verified a component's candidate against its own record (ADR-0083 §2); the host
/// reads and clears it.
#[derive(Clone, Default)]
pub struct FinishOutcome {
    inner: Arc<Mutex<OutcomeCell>>,
}

impl FinishOutcome {
    /// The last accepted outcome, if any.
    pub fn get(&self) -> Option<Accepted> {
        self.inner.lock().unwrap().accepted.clone()
    }

    /// The `result` of the last accepted `done` with its check; `None` after an
    /// accepted `blocked`, after [`FinishOutcome::clear`], or before any call.
    pub fn structured(&self) -> Option<StructuredResult> {
        self.inner.lock().unwrap().structured.clone()
    }

    /// A counter bumped by every [`FinishOutcome::set`]. A caller that reads it before
    /// and after invoking a tool learns whether that call accepted a completion: the
    /// value changes on acceptance alone, whatever the cell held and whatever the
    /// values are (ADR-0120 point 2). [`FinishOutcome::clear`] leaves it alone, so it
    /// stays monotonic and a clear cannot fake an acceptance.
    pub fn generation(&self) -> u64 {
        self.inner.lock().unwrap().generation
    }

    /// Drop both values, so an earlier turn cannot end a later one.
    pub fn clear(&self) {
        let mut cell = self.inner.lock().unwrap();
        cell.accepted = None;
        cell.structured = None;
    }

    /// Store an accepted call; the last one wins.
    pub fn set(&self, accepted: Accepted, structured: Option<StructuredResult>) {
        let mut cell = self.inner.lock().unwrap();
        cell.accepted = Some(accepted);
        cell.structured = structured;
        cell.generation += 1;
    }
}

/// One agent's completion state: the activity it runs against and the cell its
/// `finish` tool writes. `id` names the AGENT this state belongs to: one hub serves the
/// parent and every worker, so the grant state and the call window are keyed on it and a
/// (re-)grant invalidates only the SAME agent's earlier grants (ADR-0083 rule 6).
#[derive(Clone)]
pub struct Completion {
    pub id: u64,
    pub log: Arc<ActivityLog>,
    pub outcome: FinishOutcome,
}

/// Issues one [`Completion`] per assembly and hands it to the host. The catalog's
/// `finish` factory calls [`CompletionHub::issue`]; the host calls
/// [`CompletionHub::take`] for the agent it just assembled.
///
/// It also backs the `completion` capability of a component (ADR-0083 §2): at every
/// assembly boundary [`CompletionHub::grant`] records the agent's [`Completion`] (its
/// record and accepted cell), chooses the policy (rule 7) and holds the output contract;
/// the [`CompletionGrant`] it returns is what the component is linked with and wrapped
/// by, and every candidate the component submits is re-verified here (rules 1–6) before
/// it reaches the accepted cell.
#[derive(Default)]
pub struct CompletionHub {
    issued: Mutex<HashMap<usize, Completion>>,
    /// The `p1/finish` component the finish entry assembled, per assembling agent (its one
    /// [`MaskCounter`]), so a worker's assembly boundary can rebuild the tool under the
    /// policy the hub chose (rule 7) without touching the catalog — from the component
    /// of ITS OWN catalog generation, whatever another agent assembled meanwhile. The
    /// counter is held weakly: an agent that is gone leaves its entry to be pruned.
    finish_modules: Mutex<Vec<(Weak<MaskCounter>, Arc<LoadedModule>)>>,
    /// The next agent id; ids are never reused, so a retired agent's grant state cannot
    /// be reached by a later one.
    next_agent: AtomicU64,
    shared: Arc<HubShared>,
}

/// Discard a completion issued during an assembly that aborts before it can be taken.
/// A successful assembly takes its value before this guard drops.
pub(crate) struct AssemblyIssueGuard<'a> {
    hub: &'a CompletionHub,
    mask: &'a Arc<MaskCounter>,
}

impl Drop for AssemblyIssueGuard<'_> {
    fn drop(&mut self) {
        let _ = self.hub.take(self.mask);
    }
}

impl CompletionHub {
    pub(crate) fn assembly_guard<'a>(
        &'a self,
        mask: &'a Arc<MaskCounter>,
    ) -> AssemblyIssueGuard<'a> {
        AssemblyIssueGuard { hub: self, mask }
    }

    /// The generation of the grant that is live for agent `id` (test observation).
    #[cfg(test)]
    pub(crate) fn live_generation_for_test(&self, id: u64) -> u64 {
        self.shared.agent(id).state.lock().unwrap().generation
    }

    #[cfg(test)]
    pub(crate) fn pending_issued(&self) -> usize {
        self.issued.lock().unwrap().len()
    }

    /// Agents whose grant state something still holds (test observation).
    #[cfg(test)]
    pub(crate) fn live_agents(&self) -> usize {
        let agents = self.shared.agents.lock().unwrap();
        agents
            .values()
            .filter(|agent| agent.strong_count() > 0)
            .count()
    }

    pub fn new() -> Self {
        Self::default()
    }

    /// Build the completion for the assembly that is starting, under a fresh agent id.
    pub fn issue(&self, mask: &Arc<MaskCounter>) -> Completion {
        let completion = Completion {
            id: self.next_agent.fetch_add(1, Ordering::Relaxed) + 1,
            log: Arc::new(ActivityLog::default()),
            outcome: FinishOutcome::default(),
        };
        self.issued
            .lock()
            .unwrap()
            .insert(Arc::as_ptr(mask) as usize, completion.clone());
        completion
    }

    /// The completion issued for the assembly that just finished, if its
    /// environment assembled the `finish` tool.
    pub fn take(&self, mask: &Arc<MaskCounter>) -> Option<Completion> {
        self.issued
            .lock()
            .unwrap()
            .remove(&(Arc::as_ptr(mask) as usize))
    }

    /// Release a retired agent's stored grant state. Existing in-flight grants hold
    /// their own Arc and can finish without changing another agent's entry.
    pub fn retire(&self, id: u64) {
        self.shared.agents.lock().unwrap().remove(&id);
    }

    /// The `p1/finish` component the finish entry assembled for the agent whose counter is
    /// `mask`, registered when it builds it. [`CompletionHub::finish_for`] and
    /// [`CompletionHub::stage_finish_for`] rebuild the tool from the component registered
    /// for the SAME agent, so two agents assembling on different catalog generations never
    /// see each other's.
    pub fn register_finish(&self, mask: &Arc<MaskCounter>, module: &Arc<LoadedModule>) {
        let mut modules = self.finish_modules.lock().unwrap();
        modules.retain(|(agent, _)| agent.strong_count() > 0 && !agent_is(agent, mask));
        modules.push((Arc::downgrade(mask), module.clone()));
    }

    /// The `p1/finish` component registered for the agent whose counter is `mask`.
    fn finish_module(&self, mask: &Arc<MaskCounter>) -> Result<Arc<LoadedModule>, String> {
        self.finish_modules
            .lock()
            .unwrap()
            .iter()
            .find(|(agent, _)| agent_is(agent, mask))
            .map(|(_, module)| module.clone())
            .ok_or_else(|| "no finish component was assembled for this agent".to_string())
    }

    /// An assembly boundary for the component path (ADR-0083 rules 6 and 7): the first
    /// assembly of an agent or a re-grant. `completion` is the agent's record and cell,
    /// kept across its re-grants; `tools` are the other tools assembled with the
    /// component, whose loader-built or native identities choose the policy; `contract`
    /// is the output contract the host set for this agent.
    ///
    /// Every earlier grant of the SAME agent becomes stale: its instances and the calls
    /// that started under it can no longer commit. Another agent's grants are untouched.
    pub fn grant(
        &self,
        completion: Completion,
        tools: &[Arc<dyn Tool>],
        role: AgentRole,
        contract: Option<OutputContract>,
    ) -> CompletionGrant {
        let policy = completion_policy(tools, role);
        let agent = self.shared.agent(completion.id);
        let generation = {
            let mut state = agent.state.lock().unwrap();
            state.generation = state.generation.max(state.staged) + 1;
            state.completion = Some(completion.clone());
            state.policy = policy;
            state.contract = contract.clone();
            state.generation
        };
        CompletionGrant {
            agent,
            generation,
            completion,
            policy,
            contract,
        }
    }

    /// Prepare the grant of a re-assembly that may still be refused, WITHOUT touching the
    /// agent's live grant state: the finish tool the agent runs now stays fresh until
    /// [`StagedFinish::activate`] runs, which the caller does once the new assembly is
    /// installed. Dropping the staged value leaves the agent exactly as it was. The staged
    /// grant carries a generation no earlier grant of this agent has.
    pub fn stage_finish_for(
        &self,
        current: &Arc<dyn Tool>,
        completion: &Completion,
        tools: &[Arc<dyn Tool>],
        role: AgentRole,
        contract: Option<OutputContract>,
        mask: &Arc<MaskCounter>,
    ) -> Result<StagedFinish, String> {
        let module = self.finish_module(mask)?;
        let policy = completion_policy(tools, role);
        let agent = self.shared.agent(completion.id);
        let generation = {
            let mut state = agent.state.lock().unwrap();
            state.staged = state.staged.max(state.generation) + 1;
            state.staged
        };
        let grant = CompletionGrant {
            agent,
            generation,
            completion: completion.clone(),
            policy,
            contract,
        };
        let face = (
            current.declaration().name.as_str(),
            current.identity().variant.as_str(),
        );
        let (tool, name) =
            finish_tool(&module, &grant, Some(face), mask).map_err(|error| error.to_string())?;
        Ok(StagedFinish { tool, grant, name })
    }

    /// A worker's assembly boundary for the `p1/finish` component (ADR-0083 rules 6 and
    /// 7): re-choose the policy from `tools`, re-grant the agent's own `completion`, and
    /// build the component the grant is linked with, presenting `current`'s model-facing
    /// name and variant and the policy's and contract's declaration. `current` is the
    /// tool the catalog assembled for this environment, so the face the environment chose
    /// survives. Errors when no `p1/finish` component was assembled.
    pub fn finish_for(
        &self,
        current: &Arc<dyn Tool>,
        completion: &Completion,
        tools: &[Arc<dyn Tool>],
        role: AgentRole,
        contract: Option<OutputContract>,
        mask: &Arc<MaskCounter>,
    ) -> Result<Arc<dyn Tool>, String> {
        let module = self.finish_module(mask)?;
        let grant = self.grant(completion.clone(), tools, role, contract);
        let face = (
            current.declaration().name.as_str(),
            current.identity().variant.as_str(),
        );
        finish_component(&module, &grant, Some(face), mask).map_err(|error| error.to_string())
    }
}

/// A finish tool built over a grant that is not yet the agent's live one
/// ([`CompletionHub::stage_finish_for`]).
pub struct StagedFinish {
    tool: Arc<dyn Tool>,
    grant: CompletionGrant,
    name: String,
}

impl StagedFinish {
    /// The tool to assemble; it is refused as stale until [`StagedFinish::activate`].
    pub fn tool(&self) -> &Arc<dyn Tool> {
        &self.tool
    }

    /// Make the staged grant the agent's live one, which voids the earlier tool's. Only
    /// after the assembly holding [`StagedFinish::tool`] is installed.
    pub fn activate(self) {
        {
            let mut state = self.grant.agent.state.lock().unwrap();
            if self.grant.generation > state.generation {
                state.generation = self.grant.generation;
                state.completion = Some(self.grant.completion.clone());
                state.policy = self.grant.policy;
                state.contract = self.grant.contract.clone();
            }
        }
        self.grant.completion.log.set_finish_name(self.name);
    }
}

/// Whose completion a grant is (ADR-0083 rule 7).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentRole {
    /// The main agent: always `recorded-commands`.
    Main,
    /// A worker: its assembled tools decide.
    Worker,
}

/// The ONE place the host decides whether a tool's runs are evidence (ADR-0083 rule 2):
/// the `records-command-evidence` capability of its identity — the native shell's
/// implementation while it runs, a tool package through the declaration the loader's
/// registration made from its verified manifest (`catalog/capabilities.rs`). Never the
/// model-facing name, a face or the effect a tool classified, so no component can claim it.
pub fn records_command_evidence(tool: &dyn Tool) -> bool {
    carries(tool, SemanticCapability::RecordsCommandEvidence)
}

/// ADR-0083 rule 7: `recorded-commands` when some assembled tool records command evidence,
/// `report-to-parent` otherwise; a main agent always gets `recorded-commands`.
pub fn completion_policy(tools: &[Arc<dyn Tool>], role: AgentRole) -> CompletionPolicy {
    let can_run_commands = tools
        .iter()
        .any(|tool| records_command_evidence(tool.as_ref()));
    if role == AgentRole::Main || can_run_commands {
        CompletionPolicy::RecordedCommands
    } else {
        CompletionPolicy::ReportToParent
    }
}

/// The grant state of ONE agent. One hub serves the parent and every concurrent worker,
/// so each agent gets its own generation, record, policy, contract and call window: a
/// worker's (re-)grant must never invalidate another live worker's (ADR-0083 rule 6).
#[derive(Default)]
struct AgentShared {
    state: Mutex<AgentState>,
    /// One `execute` of THIS agent's component granted `completion` at a time: the frozen
    /// `accept` carries no call identity, so a candidate belongs to the one open window.
    calls: tokio::sync::Mutex<()>,
}

/// The state every grant of one hub shares.
#[derive(Default)]
struct HubShared {
    /// Held weakly: the grants (the assembled `finish` tools, the staged ones) own an
    /// agent's state, so an assembly that fails after its `finish` factory granted a
    /// completion — or a candidate dropped after it took one — leaves nothing behind.
    /// While any grant lives the entry does, so a re-grant still sees every earlier one.
    agents: Mutex<HashMap<u64, Weak<AgentShared>>>,
}

impl HubShared {
    /// The state of `id`, created on first use (again once nothing holds it).
    fn agent(&self, id: u64) -> Arc<AgentShared> {
        let mut agents = self.agents.lock().unwrap();
        if let Some(agent) = agents.get(&id).and_then(Weak::upgrade) {
            return agent;
        }
        agents.retain(|_, agent| agent.strong_count() > 0);
        let agent = Arc::new(AgentShared::default());
        agents.insert(id, Arc::downgrade(&agent));
        agent
    }
}

/// Whether `agent` is the counter `mask`: an assembling agent's identity in the hub. A
/// weak reference keeps its allocation, so no later counter can take its address.
fn agent_is(agent: &Weak<MaskCounter>, mask: &Arc<MaskCounter>) -> bool {
    std::ptr::eq(agent.as_ptr(), Arc::as_ptr(mask))
}

struct AgentState {
    /// Increased at every [`CompletionHub::grant`] for this agent; 0 before the first.
    generation: u64,
    completion: Option<Completion>,
    policy: CompletionPolicy,
    contract: Option<OutputContract>,
    /// The highest generation a staged (not yet live) grant took.
    staged: u64,
    /// The `execute` call a candidate may belong to, if one runs.
    window: Option<Window>,
}

impl Default for AgentState {
    fn default() -> Self {
        Self {
            generation: 0,
            completion: None,
            policy: CompletionPolicy::RecordedCommands,
            contract: None,
            staged: 0,
            window: None,
        }
    }
}

/// One `execute` call of a component: the grant it runs under and the hub's decision on
/// the last candidate it submitted.
struct Window {
    generation: u64,
    decision: Option<Result<(), Refusal>>,
}

/// The rules of ADR-0083 §2 a candidate is re-verified by, each named in a refusal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompletionRule {
    /// 1: `done` with `commands-passed` needs a qualifying run of every command.
    CommandsPassed,
    /// 2: only a tool that records command evidence produces evidence.
    CommandEvidence,
    /// 3: `done` with `not-run` only under the rule's conditions.
    NotRun,
    /// 4: `blocked` needs non-empty `needs`.
    Blocked,
    /// 5: a structured result is the contract's.
    StructuredResult,
    /// 6: only a call of the current assembly's component commits.
    Freshness,
}

impl CompletionRule {
    /// The rule's number in ADR-0083 §2.
    pub fn number(self) -> u8 {
        match self {
            Self::CommandsPassed => 1,
            Self::CommandEvidence => 2,
            Self::NotRun => 3,
            Self::Blocked => 4,
            Self::StructuredResult => 5,
            Self::Freshness => 6,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::CommandsPassed => "commands passed",
            Self::CommandEvidence => "command evidence",
            Self::NotRun => "not run",
            Self::Blocked => "blocked",
            Self::StructuredResult => "structured result",
            Self::Freshness => "freshness",
        }
    }
}

/// Why the hub did not commit a candidate. Its text is the tool error the model reads in
/// place of the call's own outcome.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error(
    "The host did not accept this completion (completion rule {} — {}): {reason}",
    rule.number(),
    rule.name()
)]
pub struct Refusal {
    pub rule: CompletionRule,
    pub reason: String,
}

impl Refusal {
    fn new(rule: CompletionRule, reason: impl Into<String>) -> Self {
        Self {
            rule,
            reason: reason.into(),
        }
    }
}

/// One assembly's grant of the `completion` capability: the service a component is linked
/// with and the gate its calls run through. Cheap to clone.
#[derive(Clone)]
pub struct CompletionGrant {
    agent: Arc<AgentShared>,
    generation: u64,
    completion: Completion,
    policy: CompletionPolicy,
    contract: Option<OutputContract>,
}

impl CompletionGrant {
    /// The policy the hub chose at this assembly boundary.
    pub fn policy(&self) -> CompletionPolicy {
        self.policy
    }

    /// The output contract of this assembly.
    pub fn contract(&self) -> Option<&OutputContract> {
        self.contract.as_ref()
    }

    /// The agent's record and accepted cell.
    pub fn completion(&self) -> &Completion {
        &self.completion
    }

    /// The `completion` service of this assembly, for [`Services::completion`].
    pub fn service(&self) -> Arc<dyn CompletionService> {
        Arc::new(HubService {
            agent: self.agent.clone(),
            generation: self.generation,
        })
    }

    /// Opens the window of one `execute` call under this grant: a candidate commits only
    /// while it is open and only if no re-grant of this agent happened since.
    /// [`CompletionGate`] opens one around every call; closing it returns the hub's
    /// refusal of the call's last candidate, if it refused it.
    pub fn open_window(&self) -> CallWindow {
        self.agent.state.lock().unwrap().window = Some(Window {
            generation: self.generation,
            decision: None,
        });
        CallWindow {
            agent: self.agent.clone(),
        }
    }
}

/// An open [`Window`]. Dropping it closes it, so an abandoned or cancelled call leaves no
/// window a later candidate could commit into.
pub struct CallWindow {
    agent: Arc<AgentShared>,
}

impl CallWindow {
    /// Closes the window: the refusal of the call's last candidate, or `None` when the
    /// last candidate committed or none was submitted.
    pub fn close(self) -> Option<Refusal> {
        let window = self.agent.state.lock().unwrap().window.take();
        match window.and_then(|window| window.decision) {
            Some(Err(refusal)) => Some(refusal),
            _ => None,
        }
    }
}

impl Drop for CallWindow {
    fn drop(&mut self) {
        self.agent.state.lock().unwrap().window = None;
    }
}

/// The hub as one assembly's `completion` service, over its own agent's state.
struct HubService {
    agent: Arc<AgentShared>,
    generation: u64,
}

impl HubService {
    fn log(&self) -> Option<Arc<ActivityLog>> {
        let state = self.agent.state.lock().unwrap();
        state
            .completion
            .as_ref()
            .map(|completion| completion.log.clone())
    }
}

impl CompletionService for HubService {
    fn last_file_change(&self) -> Option<u64> {
        self.log().and_then(|log| log.last_file_change())
    }

    fn shell_runs(&self) -> Vec<WireRun> {
        // Every finished `executes` call, as the frozen WIT says; which of them count as
        // evidence is decided again when a candidate arrives.
        self.log()
            .map(|log| log.shell_runs())
            .unwrap_or_default()
            .into_iter()
            .map(|run| WireRun {
                command: run.command,
                exit_code: run.exit_code,
                order: run.order,
            })
            .collect()
    }

    fn policy(&self) -> WirePolicy {
        match self.agent.state.lock().unwrap().policy {
            CompletionPolicy::RecordedCommands => WirePolicy::RecordedCommands,
            CompletionPolicy::ReportToParent => WirePolicy::ReportToParent,
        }
    }

    fn output_contract(&self) -> Option<String> {
        let state = self.agent.state.lock().unwrap();
        state
            .contract
            .as_ref()
            .map(|contract| contract.schema().to_string())
    }

    fn accept(&self, candidate: Candidate, structured: Option<WireStructured>) {
        let mut state = self.agent.state.lock().unwrap();
        let decision = decide(&state, self.generation, candidate, structured);
        let decision = decision.map(|(accepted, structured, completion)| {
            completion.outcome.set(accepted, structured);
        });
        // A candidate outside any window is refused and has no call to report to.
        if let Some(window) = state.window.as_mut() {
            window.decision = Some(decision);
        }
    }
}

/// What a committed candidate stores, and where.
type Commit = (Accepted, Option<StructuredResult>, Completion);

/// Re-verify a candidate against the hub's own record (ADR-0083 §2, rules 1–6). `Ok` is
/// what commits — the hub's evidence, reason and schema verdict, never the component's.
fn decide(
    state: &AgentState,
    generation: u64,
    candidate: Candidate,
    structured: Option<WireStructured>,
) -> Result<Commit, Refusal> {
    // Rule 6: within an `execute` call of the current assembly's component only.
    let Some(window) = &state.window else {
        return Err(Refusal::new(
            CompletionRule::Freshness,
            "a completion is accepted only during a call of the tool the current assembly granted it",
        ));
    };
    if generation != state.generation || window.generation != state.generation {
        return Err(Refusal::new(
            CompletionRule::Freshness,
            "this call belongs to an assembly the host has replaced since (a re-grant); call the tool again",
        ));
    }
    let Some(completion) = state.completion.clone() else {
        return Err(Refusal::new(
            CompletionRule::Freshness,
            "no agent was granted completion",
        ));
    };
    match candidate {
        Candidate::Blocked {
            summary,
            needs,
            tried,
        } => {
            // Rule 4.
            if needs.trim().is_empty() {
                return Err(Refusal::new(
                    CompletionRule::Blocked,
                    "a blocked completion must say what it needs",
                ));
            }
            Ok((
                Accepted::Blocked {
                    summary,
                    needs,
                    tried,
                },
                None,
                completion,
            ))
        }
        Candidate::Done { summary, evidence } => {
            let evidence = verify_evidence(state, &completion.log, evidence)?;
            let structured = verify_structured(state.contract.as_ref(), structured)?;
            Ok((
                Accepted::Done { summary, evidence },
                Some(structured),
                completion,
            ))
        }
    }
}

/// Rules 1–3: the evidence the hub commits for a `done`.
fn verify_evidence(
    state: &AgentState,
    log: &ActivityLog,
    claimed: CandidateEvidence,
) -> Result<Evidence, Refusal> {
    let last_change = log.last_file_change();
    match claimed {
        // Rule 3: the reason is the hub's, never the component's text.
        CandidateEvidence::NotRun(_) => match state.policy {
            CompletionPolicy::ReportToParent => Ok(Evidence::NotRun(
                p1_finish_guest::NOT_RUN_NO_COMMAND_TOOL.to_string(),
            )),
            CompletionPolicy::RecordedCommands if last_change.is_none() => Ok(Evidence::NotRun(
                p1_finish_guest::NOT_RUN_NO_FILE_CHANGED.to_string(),
            )),
            CompletionPolicy::RecordedCommands => Err(Refusal::new(
                CompletionRule::NotRun,
                "this session changed files, so a completion without a verifying command is not accepted",
            )),
        },
        CandidateEvidence::CommandsPassed(commands) => {
            if commands.is_empty() {
                return Err(Refusal::new(
                    CompletionRule::CommandsPassed,
                    "a completion must name the commands that verified it",
                ));
            }
            let evidence = log.evidence_runs();
            let every_run = log.shell_runs();
            let mut passed = Vec::with_capacity(commands.len());
            for command in &commands {
                if let Some(failure) =
                    p1_finish_guest::command_failure(command, &evidence, last_change)
                {
                    // Rule 2: a run that would count but for the tool that ran it.
                    if p1_finish_guest::command_failure(command, &every_run, last_change).is_none()
                    {
                        return Err(Refusal::new(
                            CompletionRule::CommandEvidence,
                            format!(
                                "`{command}` was run only by a tool that does not record command evidence, so its run verifies nothing"
                            ),
                        ));
                    }
                    return Err(Refusal::new(CompletionRule::CommandsPassed, failure));
                }
                passed.push(p1_finish_guest::normalise_command(command));
            }
            Ok(Evidence::CommandsPassed(passed))
        }
    }
}

/// Rule 5: the hub's own check of the result against the contract it holds.
fn verify_structured(
    contract: Option<&OutputContract>,
    claimed: Option<WireStructured>,
) -> Result<StructuredResult, Refusal> {
    let value = claimed.and_then(|claimed| claimed.value);
    match (contract, value) {
        (None, None) => Ok(StructuredResult::checked(None, None)),
        (None, Some(_)) => Err(Refusal::new(
            CompletionRule::StructuredResult,
            "no structured result was requested for this task",
        )),
        (Some(_), None) => Err(Refusal::new(
            CompletionRule::StructuredResult,
            "this task requires a structured result with `done`",
        )),
        (Some(contract), Some(text)) => {
            let value = serde_json::from_str(&text).map_err(|error| {
                Refusal::new(
                    CompletionRule::StructuredResult,
                    format!("the structured result is not JSON: {error}"),
                )
            })?;
            Ok(StructuredResult::checked(Some(contract), Some(value)))
        }
    }
}

/// The host adapter of a component granted `completion` (ADR-0083 §2, after the rules):
/// every call runs inside a window of the component's grant, and when the hub refused the
/// call's candidate, the call's outcome is replaced by an ordinary tool error naming the
/// rule, so the model reads it in the same turn. Generic: it names no tool, and presents
/// the component's own declaration unless the host gives the one to present.
pub struct CompletionGate {
    inner: Arc<dyn Tool>,
    grant: CompletionGrant,
    declaration: ToolDeclaration,
    /// The identity to present, when the host applies a face (a `+sandbox` variant or an
    /// environment's own variant). `None` presents the component's loader-built identity.
    identity: Option<ToolIdentity>,
}

impl CompletionGate {
    /// Gate `inner`, a tool linked with `grant`'s service.
    pub fn new(inner: Arc<dyn Tool>, grant: CompletionGrant) -> Self {
        let declaration = inner.declaration().clone();
        Self {
            inner,
            grant,
            declaration,
            identity: None,
        }
    }

    /// Present `declaration` instead of the component's: the component reads its
    /// declaration on the restricted path, where `completion` is not linked, so what
    /// depends on the grant is the host's to present.
    pub fn presenting(mut self, declaration: ToolDeclaration) -> Self {
        self.declaration = declaration;
        self
    }

    /// Present `identity` instead of the component's: only the variant moves — the
    /// implementation stays the loader's (ADR-0087).
    pub fn identified(mut self, identity: ToolIdentity) -> Self {
        self.identity = Some(identity);
        self
    }
}

impl Tool for CompletionGate {
    fn declaration(&self) -> &ToolDeclaration {
        &self.declaration
    }

    fn identity(&self) -> &ToolIdentity {
        self.identity
            .as_ref()
            .unwrap_or_else(|| self.inner.identity())
    }

    fn effect(&self, call: &ToolCall) -> Effect {
        self.inner.effect(call)
    }

    fn describe(&self, call: &ToolCall) -> CallDescription {
        self.inner.describe(call)
    }

    fn describe_result(&self, call: &ToolCall, result: &ToolResultItem) -> ResultDescription {
        self.inner.describe_result(call, result)
    }

    fn ends_turn(&self, outcome: &ToolOutcome) -> bool {
        self.inner.ends_turn(outcome)
    }

    fn execute<'a>(
        &'a self,
        call: &'a ToolCall,
        context: ToolContext,
    ) -> BoxFuture<'a, ToolOutcome> {
        Box::pin(async move {
            let _one_call_at_a_time = self.grant.agent.calls.lock().await;
            let window = self.grant.open_window();
            let outcome = self.inner.execute(call, context).await;
            match window.close() {
                Some(refusal) => ToolOutcome::error(refusal.to_string()),
                None => outcome,
            }
        })
    }
}

/// The host's wrapper for the `p1/finish` module (ADR-0120 point 2): it answers
/// [`Tool::ends_turn`] from the agent's accepted completion record ([`FinishOutcome`]) for
/// the call just executed. It sits ABOVE the redaction adapter, where the host's own record
/// is reachable; the completion gate and the masking stay below it, untouched. Every other
/// method delegates, so the module's presentation and behaviour do not change.
struct FinishTurnEnd {
    inner: Arc<dyn Tool>,
    /// The host's accepted-completion cell, shared with the completion hub.
    outcome: FinishOutcome,
    /// Whether the last `execute` committed an accepted completion. `ends_turn` is called
    /// immediately after that execute, for the same call (core step 3g), and the core runs
    /// one agent's calls strictly sequentially, so one flag is enough.
    accepted: Mutex<bool>,
}

impl FinishTurnEnd {
    fn new(inner: Arc<dyn Tool>, outcome: FinishOutcome) -> Self {
        Self {
            inner,
            outcome,
            accepted: Mutex::new(false),
        }
    }
}

impl Tool for FinishTurnEnd {
    fn declaration(&self) -> &ToolDeclaration {
        self.inner.declaration()
    }

    fn identity(&self) -> &ToolIdentity {
        self.inner.identity()
    }

    fn effect(&self, call: &ToolCall) -> Effect {
        self.inner.effect(call)
    }

    fn synthetic_command_result(&self) -> bool {
        self.inner.synthetic_command_result()
    }

    fn take_command_exit_code(&self, call_id: &str) -> Option<i32> {
        self.inner.take_command_exit_code(call_id)
    }

    fn command_exit_code(&self, call_id: &str) -> Option<i32> {
        self.inner.command_exit_code(call_id)
    }

    fn describe(&self, call: &ToolCall) -> CallDescription {
        self.inner.describe(call)
    }

    fn describe_result(&self, call: &ToolCall, result: &ToolResultItem) -> ResultDescription {
        self.inner.describe_result(call, result)
    }

    fn ends_turn(&self, _outcome: &ToolOutcome) -> bool {
        *self.accepted.lock().unwrap()
    }

    fn execute<'a>(
        &'a self,
        call: &'a ToolCall,
        context: ToolContext,
    ) -> BoxFuture<'a, ToolOutcome> {
        Box::pin(async move {
            // Acceptance is detected per call from the cell's generation counter, never
            // from the values: the cell is NOT cleared between interactive turns
            // (run_interactive never clears it), so an IDENTICAL accepted value from an
            // earlier turn would compare equal and wrongly report "not accepted". The
            // generation moves only when THIS call's acceptance calls `set`.
            let before = self.outcome.generation();
            let outcome = self.inner.execute(call, context).await;
            let after = self.outcome.generation();
            *self.accepted.lock().unwrap() = after != before;
            outcome
        })
    }
}

/// The finish entry's assembly of the `p1/finish` component under `grant` (ADR-0083 §2):
/// linked with the grant's `completion` service, gated by [`CompletionGate`], presenting
/// the declaration of the grant's policy and contract — byte-identical to the native
/// tool's, because both come from `p1-finish-guest` — and masked as every assembled tool
/// is, the refusal text included.
///
/// `face` is the model-facing `(name, variant)` the host presents INSTEAD of the
/// component's own, for the environment's face a worker was assembled with; `None`
/// presents the component's own name and its loader-built variant.
pub fn finish_component(
    module: &LoadedModule,
    grant: &CompletionGrant,
    face: Option<(&str, &str)>,
    mask: &Arc<MaskCounter>,
) -> Result<Arc<dyn Tool>, ToolError> {
    let (tool, name) = finish_tool(module, grant, face, mask)?;
    grant.completion().log.set_finish_name(name);
    Ok(tool)
}

/// The finish tool over `grant` and the model-facing name it presents, without naming it
/// in the log: a staged grant names it only when it becomes the live one.
fn finish_tool(
    module: &LoadedModule,
    grant: &CompletionGrant,
    face: Option<(&str, &str)>,
    mask: &Arc<MaskCounter>,
) -> Result<(Arc<dyn Tool>, String), ToolError> {
    let services = Services {
        completion: Some(grant.service()),
        ..Services::default()
    };
    let component = wasm_tool(module, services, ExecutionLimits::default(), mask)?;
    let (name, variant) = match face {
        Some((name, variant)) => (name.to_owned(), variant.to_owned()),
        None => (
            component.declaration().name.clone(),
            component.identity().variant.clone(),
        ),
    };
    let declaration = ToolDeclaration {
        name: name.clone(),
        description: p1_finish_guest::description(grant.policy(), grant.contract()),
        kind: DeclarationKind::Function {
            input_schema: p1_finish_guest::input_schema(grant.contract()),
        },
    };
    let identity = ToolIdentity {
        implementation: component.identity().implementation.clone(),
        variant,
    };
    let gate = CompletionGate::new(component, grant.clone())
        .presenting(declaration)
        .identified(identity);
    // ADR-0120: the host answers `ends_turn` for the module from its own accepted record;
    // the wrapper sits above the redaction adapter so the answer reaches the core.
    let tool: Arc<dyn Tool> = Arc::new(FinishTurnEnd::new(
        redacted(Arc::new(gate), mask),
        grant.completion().outcome.clone(),
    ));
    Ok((tool, name))
}

#[cfg(test)]
mod tests {
    use super::*;
    use p1_contracts::{
        AssistantBlock, AssistantItem, Origin, RecordBody, StopReason, ToolIdentity,
    };
    use p1_testkit::FakeTool;

    /// The identity implementation the real `finish` tool builds (S3.8: the `p1/finish`
    /// package, declared through its verified manifest): a fake that takes it is found as
    /// the `finish` tool through that identity's declared capability.
    #[cfg(feature = "delegation")]
    fn finish_implementation() -> String {
        static NAME: std::sync::OnceLock<String> = std::sync::OnceLock::new();
        NAME.get_or_init(|| {
            let module = crate::catalog::capabilities::built_package("p1-module-finish");
            crate::catalog::capabilities::declare_package(&module);
            module.name().to_owned()
        })
        .clone()
    }

    #[test]
    fn aborted_assembly_retires_issued_completion() {
        use p1_assembly::{Catalog, EnvironmentFile, Substitutions, ToolSpec, assemble};
        let hub = Arc::new(CompletionHub::new());
        let mask = Arc::new(MaskCounter::new());
        let mut catalog = Catalog::new();
        catalog.provider(
            "scripted",
            Box::new(|_| {
                Ok(Arc::new(p1_testkit::ScriptedProvider::new(Vec::new()))
                    as Arc<dyn p1_contracts::Provider>)
            }),
        );
        let issue = hub.clone();
        let assembly_mask = mask.clone();
        catalog.tool(
            "finish",
            Box::new(move |spec: &ToolSpec, _: &p1_assembly::ToolServices| {
                issue.issue(&assembly_mask);
                Ok(Arc::new(FakeTool::new(&spec.module)) as Arc<dyn Tool>)
            }),
        );
        catalog.tool(
            "broken",
            Box::new(|_: &ToolSpec, _: &p1_assembly::ToolServices| Err("factory refused".into())),
        );
        let environment = EnvironmentFile {
            name: "broken".into(),
            family: "test".into(),
            provider: "scripted".into(),
            model: "m".into(),
            profile: None,
            profile_text: None,
            options: Default::default(),
            tools: ["finish", "broken"]
                .into_iter()
                .map(|module| ToolSpec {
                    module: module.into(),
                    name: None,
                    description: None,
                    variant: None,
                })
                .collect(),
            prompt_template: String::new(),
            context: None,
            summarize_prompt: None,
        };
        let workspace = tempfile::tempdir().unwrap();
        for _ in 0..3 {
            let failed = {
                let _assembly = hub.assembly_guard(&mask);
                let failed = assemble(
                    &catalog,
                    &environment,
                    workspace.path(),
                    &Substitutions {
                        workspace: "/work".into(),
                        date: "2026-01-01".into(),
                        os: "linux".into(),
                    },
                );
                assert_eq!(hub.pending_issued(), 1);
                failed
            };
            assert!(failed.is_err());
            assert_eq!(hub.pending_issued(), 0);
        }
    }

    fn live_generation(hub: &CompletionHub, id: u64) -> u64 {
        hub.shared.agent(id).state.lock().unwrap().generation
    }

    #[tokio::test]
    async fn a_staged_finish_leaves_the_live_grant_fresh_until_it_is_activated() {
        let hub = CompletionHub::new();
        let mask = Arc::new(MaskCounter::new());
        let completion = hub.issue(&mask);
        let module = Arc::new(crate::catalog::capabilities::built_package(
            "p1-module-finish",
        ));
        hub.register_finish(&mask, &module);
        let live = hub.grant(completion.clone(), &[], AgentRole::Main, None);
        let installed = finish_component(&module, &live, None, &mask).expect("live finish");
        let before = live_generation(&hub, completion.id);

        // A reload builds its finish over the session's completion, then its install is refused:
        // dropping the staged value must leave the installed tool's grant fresh.
        let staged = hub
            .stage_finish_for(&installed, &completion, &[], AgentRole::Main, None, &mask)
            .expect("staged finish");
        assert_eq!(live_generation(&hub, completion.id), before);
        drop(staged);
        assert_eq!(live_generation(&hub, completion.id), before);

        // The install that succeeded makes the staged grant the live one, and only then is
        // the earlier tool stale.
        let staged = hub
            .stage_finish_for(&installed, &completion, &[], AgentRole::Main, None, &mask)
            .expect("staged finish");
        assert_eq!(live_generation(&hub, completion.id), before);
        staged.activate();
        assert!(live_generation(&hub, completion.id) > before);
        // Numbers never repeat: a grant after the dropped and the activated stagings is newer
        // than both.
        let latest = live_generation(&hub, completion.id);
        hub.grant(completion.clone(), &[], AgentRole::Main, None);
        assert!(live_generation(&hub, completion.id) > latest);
    }

    /// Each agent rebuilds its `finish` from the component ITS assembly registered: a
    /// later assembly of another agent (another catalog generation, on a script thread)
    /// cannot slip its component in. Here that other component is not even a finish, so
    /// rebuilding from it fails to link.
    #[tokio::test]
    async fn each_agent_rebuilds_finish_from_its_own_assemblys_component() {
        let hub = CompletionHub::new();
        let worker = Arc::new(MaskCounter::new());
        let other = Arc::new(MaskCounter::new());
        let finish = Arc::new(crate::catalog::capabilities::built_package(
            "p1-module-finish",
        ));
        hub.register_finish(&worker, &finish);
        let completion = hub.issue(&worker);
        let live = hub.grant(completion.clone(), &[], AgentRole::Worker, None);
        let installed = finish_component(&finish, &live, None, &worker).expect("live finish");

        hub.register_finish(
            &other,
            &Arc::new(crate::catalog::capabilities::built_package(
                "p1-module-read",
            )),
        );

        hub.stage_finish_for(
            &installed,
            &completion,
            &[],
            AgentRole::Worker,
            None,
            &worker,
        )
        .expect("the worker's own finish component is staged");
        hub.finish_for(
            &installed,
            &completion,
            &[],
            AgentRole::Worker,
            None,
            &worker,
        )
        .expect("the worker's own finish component is rebuilt");
        assert!(
            hub.finish_for(
                &installed,
                &completion,
                &[],
                AgentRole::Worker,
                None,
                &Arc::new(MaskCounter::new()),
            )
            .is_err(),
            "an agent that assembled no finish has none to rebuild"
        );
    }

    /// An assembly that fails after the `finish` factory issued AND granted its
    /// completion leaves no grant state behind — whether it failed before taking the
    /// completion or after (a session candidate whose later step fails).
    #[test]
    fn a_failed_assembly_leaves_no_grant_state_behind() {
        let hub = CompletionHub::new();
        let mask = Arc::new(MaskCounter::new());
        for takes_it in [false, true] {
            let _assembly = hub.assembly_guard(&mask);
            let completion = hub.issue(&mask);
            // What the factory links its component with.
            let grant = hub.grant(completion, &[], AgentRole::Main, None);
            assert_eq!(hub.live_agents(), 1);
            let taken = takes_it.then(|| hub.take(&mask));
            // The assembly fails: its tools, and what it took, are dropped.
            drop(grant);
            drop(taken);
        }
        assert_eq!(hub.live_agents(), 0, "no failed assembly left a grant");

        // A live grant keeps its agent's state, and a re-grant still supersedes it.
        let completion = hub.issue(&mask);
        let first = hub.grant(completion.clone(), &[], AgentRole::Main, None);
        let second = hub.grant(completion, &[], AgentRole::Main, None);
        assert_eq!(hub.live_agents(), 1);
        assert!(second.generation > first.generation);
    }

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
                // A host that observed no exit: an explicit `null`.
                exit_code: Some(None),
            },
        }
    }

    #[test]
    fn background_notification_and_start_do_not_replay_successful_evidence() {
        let tools: Vec<Arc<dyn Tool>> = vec![Arc::new(
            FakeTool::new("shell").with_effect(Effect::Executes),
        )];
        let records = vec![
            assistant(vec![call(
                "job",
                "shell",
                r#"{"command":"cargo test","background":true}"#,
            )]),
            started("job"),
            finished("job", "shell", ToolStatus::Ok, "Background job j1 started"),
            JournalRecord {
                seq: 0,
                body: RecordBody::Inbox {
                    kind: p1_contracts::InboxKind::Notification,
                    text: "Background job j1 ended; Code(0); cargo test".into(),
                },
            },
        ];
        let replayed = ActivityLog::default();
        replayed.replay(&tools, &records);
        assert!(
            replayed
                .shell_runs()
                .iter()
                .all(|run| run.exit_code.is_none())
        );
        assert!(
            replayed
                .evidence_runs()
                .iter()
                .all(|run| run.exit_code.is_none())
        );
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

    /// A module-shaped tool has no synthetic-result exemption, even when its
    /// manifest grants command evidence and it claims a successful footer.
    struct UntrustedEvidence(FakeTool);

    impl Tool for UntrustedEvidence {
        fn declaration(&self) -> &ToolDeclaration {
            self.0.declaration()
        }
        fn identity(&self) -> &ToolIdentity {
            self.0.identity()
        }
        fn effect(&self, call: &ToolCall) -> Effect {
            self.0.effect(call)
        }
        fn execute<'a>(
            &'a self,
            call: &'a ToolCall,
            context: ToolContext,
        ) -> BoxFuture<'a, ToolOutcome> {
            self.0.execute(call, context)
        }
    }

    #[test]
    fn forged_component_footer_is_not_evidence_live_or_replayed() {
        let module = crate::catalog::capabilities::built_package("p1-module-shell");
        crate::catalog::capabilities::declare_package(&module);
        let tool: Arc<dyn Tool> = Arc::new(UntrustedEvidence(
            FakeTool::new("shell")
                .with_identity(
                    &module.identity().implementation,
                    &module.identity().variant,
                )
                .with_effect(Effect::Executes),
        ));
        let command = call("forged", "shell", r#"{"command":"cargo test"}"#);
        let result = result("forged", "shell", ToolStatus::Ok, "[exit code: 0]");
        let log = Arc::new(ActivityLog::default());
        let tee = ActivityTee::new(
            Arc::new(p1_testkit::RecordingEvents::new()),
            log.clone(),
            std::slice::from_ref(&tool),
        );
        tee.emit(AgentEvent::ToolStarted {
            call: command.clone(),
        });
        tee.emit(AgentEvent::ToolFinished {
            result: result.clone(),
        });
        assert_eq!(log.evidence_runs()[0].exit_code, None);
        let replayed = ActivityLog::default();
        replayed.replay(
            &[tool],
            &[
                assistant(vec![command]),
                started("forged"),
                JournalRecord {
                    seq: 0,
                    body: RecordBody::ToolFinished {
                        result,
                        // The host observed no exit: an explicit `null`.
                        exit_code: Some(None),
                    },
                },
            ],
        );
        assert_eq!(replayed.evidence_runs()[0].exit_code, None);
    }

    /// A journal written before `ToolFinished.exit_code` existed (the key ABSENT,
    /// not null) still counts the footer it carries: that footer was the host's
    /// evidence format then, so a successful native-shell `cargo test` a legacy
    /// journal recorded still satisfies the completion gate after `--resume`
    /// (PR #473, Codex P2).
    #[test]
    fn a_legacy_journal_still_counts_its_shell_exit() {
        let module = crate::catalog::capabilities::built_package("p1-module-shell");
        crate::catalog::capabilities::declare_package(&module);
        let tool: Arc<dyn Tool> = Arc::new(UntrustedEvidence(
            FakeTool::new("shell")
                .with_identity(
                    &module.identity().implementation,
                    &module.identity().variant,
                )
                .with_effect(Effect::Executes),
        ));
        let command = call("legacy", "shell", r#"{"command":"cargo test"}"#);
        let replayed = ActivityLog::default();
        replayed.replay(
            &[tool],
            &[
                assistant(vec![command]),
                started("legacy"),
                JournalRecord {
                    seq: 0,
                    body: RecordBody::ToolFinished {
                        result: result("legacy", "shell", ToolStatus::Ok, "ok\n[exit code: 0]"),
                        // Absent: a journal written before host-observed exits.
                        exit_code: None,
                    },
                },
            ],
        );
        assert_eq!(replayed.evidence_runs()[0].exit_code, Some(0));
    }

    /// The host's journalled exit is the ONE value replay trusts for a module tool.
    #[test]
    fn replayed_host_exit_is_evidence_for_a_module_tool() {
        let module = crate::catalog::capabilities::built_package("p1-module-shell");
        crate::catalog::capabilities::declare_package(&module);
        let tool: Arc<dyn Tool> = Arc::new(UntrustedEvidence(
            FakeTool::new("shell")
                .with_identity(
                    &module.identity().implementation,
                    &module.identity().variant,
                )
                .with_effect(Effect::Executes),
        ));
        let command = call("ran", "shell", r#"{"command":"true"}"#);
        // A footer that claims failure cannot override the host's observed success.
        let result = result("ran", "shell", ToolStatus::Ok, "[exit code: 1]");
        let replayed = ActivityLog::default();
        replayed.replay(
            &[tool],
            &[
                assistant(vec![command]),
                started("ran"),
                JournalRecord {
                    seq: 0,
                    body: RecordBody::ToolFinished {
                        result,
                        exit_code: Some(Some(0)),
                    },
                },
            ],
        );
        assert_eq!(replayed.evidence_runs()[0].exit_code, Some(0));
    }

    #[test]
    fn host_observed_exit_overrides_spoofed_footer() {
        let log = ActivityLog::default();
        let spoof = call("spoof", "shell", r#"{"command":"false"}"#);
        log.record_started_by(&spoof, Effect::Executes, true);
        log.record_finished_with_exit(
            &result("spoof", "shell", ToolStatus::Ok, "[exit code: 0]"),
            Some(1),
        );
        assert_eq!(log.evidence_runs()[0].exit_code, Some(1));
        let synthetic = call("no-process", "shell", r#"{"command":"true"}"#);
        log.record_started_by(&synthetic, Effect::Executes, true);
        log.record_finished(&result(
            "no-process",
            "shell",
            ToolStatus::Ok,
            "[exit code: 0]",
        ));
        assert_eq!(log.evidence_runs()[1].exit_code, None);
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
    fn concurrent_assemblies_take_only_their_own_completion() {
        let hub = Arc::new(CompletionHub::new());
        let first = Arc::new(MaskCounter::new());
        let second = Arc::new(MaskCounter::new());
        let a = hub.issue(&first);
        let b = hub.issue(&second);
        assert_eq!(hub.take(&first).expect("first assembly").id, a.id);
        assert_eq!(hub.take(&second).expect("second assembly").id, b.id);
        assert!(hub.take(&first).is_none());
    }

    #[test]
    fn retiring_a_completion_removes_its_grant_state() {
        let hub = CompletionHub::new();
        let mask = Arc::new(MaskCounter::new());
        let completion = hub.issue(&mask);
        let grant = hub.grant(completion.clone(), &[], AgentRole::Main, None);
        assert!(
            hub.shared
                .agents
                .lock()
                .unwrap()
                .contains_key(&completion.id)
        );
        hub.retire(completion.id);
        assert!(
            !hub.shared
                .agents
                .lock()
                .unwrap()
                .contains_key(&completion.id)
        );
        drop(grant);
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

    // ------------------------------------------------- the workspace fingerprint

    /// A command whose work really landed: the file appears while the call runs, so
    /// the post-command fingerprint differs from the baseline taken at its start.
    fn shell_that_writes(workspace: &Path, call_id: &str, file: &str) -> ActivityLog {
        let log = ActivityLog::default();
        log.watch_workspace(workspace, &[]);
        log.record_started(
            &call(call_id, "shell", r#"{"command":"write it"}"#),
            Effect::Executes,
        );
        std::fs::write(workspace.join(file), "hi\n").unwrap();
        log.record_finished(&result(
            call_id,
            "shell",
            ToolStatus::Ok,
            "ok\n[exit code: 0]",
        ));
        log
    }

    /// ADR-0055: a successful command that changed the workspace is progress AND a
    /// file change, exactly as a `WritesFiles` call is.
    #[test]
    fn a_command_that_changed_the_workspace_is_progress_and_a_file_change() {
        let workspace = tempfile::tempdir().unwrap();
        let log = shell_that_writes(workspace.path(), "s1", "out.txt");

        assert_eq!(log.consecutive_replacements(), 0, "progress for §3c");
        assert_eq!(
            log.last_file_change(),
            Some(1),
            "the change is the command's own record"
        );
        assert_eq!(
            log.shell_runs(),
            vec![ShellRun {
                command: "write it".into(),
                exit_code: Some(0),
                // Reported one order PAST the change it produced, so ADR-0037's rule
                // does not invalidate the run that caused it, while every earlier run
                // is stale (ADR-0055 item 2).
                order: 2,
            }]
        );
        assert_eq!(log.fingerprint_error(), None);
    }

    /// ADR-0055 item 3: an unchanged workspace changes nothing — a successful `ls` is
    /// neither progress nor a file change, as today.
    #[test]
    fn a_command_that_changed_nothing_changes_nothing() {
        let workspace = tempfile::tempdir().unwrap();
        std::fs::write(workspace.path().join("note.txt"), "one\n").unwrap();
        let log = ActivityLog::default();
        log.watch_workspace(workspace.path(), &[]);
        log.record_replacement();
        log.record_replacement();
        log.record_started(
            &call("s1", "shell", r#"{"command":"ls"}"#),
            Effect::Executes,
        );
        log.record_finished(&result(
            "s1",
            "shell",
            ToolStatus::Ok,
            "note.txt\n[exit code: 0]",
        ));

        assert_eq!(log.consecutive_replacements(), 2, "not progress");
        assert_eq!(log.last_file_change(), None);
        assert_eq!(log.shell_runs().len(), 1);
    }

    /// A failed command is never fingerprinted: nothing changed, and a command that
    /// failed halfway is not progress either.
    #[test]
    fn a_failed_command_is_never_fingerprinted() {
        let workspace = tempfile::tempdir().unwrap();
        let log = ActivityLog::default();
        log.watch_workspace(workspace.path(), &[]);
        log.record_replacement();
        log.record_started(
            &call("s1", "shell", r#"{"command":"false"}"#),
            Effect::Executes,
        );
        std::fs::write(workspace.path().join("half-written.txt"), "x").unwrap();
        log.record_finished(&result(
            "s1",
            "shell",
            ToolStatus::Error,
            "boom\n[exit code: 1]",
        ));

        assert_eq!(
            log.consecutive_replacements(),
            1,
            "a failed run is not progress"
        );
        assert_eq!(log.last_file_change(), None);
        assert!(log.shell_runs().is_empty(), "a failed run is not a run");
    }

    /// ADR-0055 item 4: a fingerprint that cannot be taken falls back to today's rule
    /// and the error is remembered ONCE, for the run report's one-line note.
    #[test]
    fn a_fingerprint_error_falls_back_and_is_remembered_once() {
        let workspace = tempfile::tempdir().unwrap();
        let missing = workspace.path().join("gone");
        let log = ActivityLog::default();
        log.watch_workspace(&missing, &[]);
        log.record_replacement();
        log.record_started(
            &call("s1", "shell", r#"{"command":"ls"}"#),
            Effect::Executes,
        );
        log.record_finished(&result(
            "s1",
            "shell",
            ToolStatus::Ok,
            "out\n[exit code: 0]",
        ));

        assert_eq!(
            log.consecutive_replacements(),
            1,
            "today's rule still applies"
        );
        assert_eq!(log.last_file_change(), None);
        let error = log.fingerprint_error().expect("the error is exposed");
        assert!(
            error.contains("gone"),
            "the error names the workspace: {error}"
        );
    }

    /// A log the host never told about a workspace — a replayed session — keeps
    /// today's behaviour: nothing is fingerprinted.
    #[test]
    fn a_log_without_a_workspace_never_fingerprints() {
        let workspace = tempfile::tempdir().unwrap();
        let log = ActivityLog::default();
        log.record_started(
            &call("s1", "shell", r#"{"command":"write it"}"#),
            Effect::Executes,
        );
        std::fs::write(workspace.path().join("out.txt"), "hi\n").unwrap();
        log.record_finished(&result("s1", "shell", ToolStatus::Ok, "ok\n[exit code: 0]"));

        assert_eq!(log.last_file_change(), None);
        assert_eq!(log.fingerprint_error(), None);
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
            // The `finish` tool is found by its identity's capability, not by name.
            Arc::new(FakeTool::new("finish").with_identity(&finish_implementation(), "claude")),
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
            FinishOutcome::default(),
            front_end.clone(),
            "w1".to_string(),
            "route/model".to_string(),
            false,
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
                // A `blocked` outcome carries no evidence line.
                evidence: None,
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
            FakeTool::new("finish").with_identity(&finish_implementation(), "claude"),
        )];
        let report = Arc::new(Mutex::new(WorkerReport::new(vec!["finish".to_string()])));
        let tap = WorkerReportTap::new(
            Arc::new(p1_testkit::RecordingEvents::new()),
            report.clone(),
            &tools,
            FinishOutcome::default(),
            Arc::new(RecordingWorkerEnds::default()),
            "w1".to_string(),
            "route/model".to_string(),
            false,
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

    /// The report's `finish` is the tool whose identity carries `reports-completion`,
    /// whatever faces say: a tool merely NAMED `finish` is not read, and the real one
    /// under another name is.
    #[cfg(feature = "delegation")]
    #[test]
    fn the_tap_reads_the_capable_tool_not_the_one_named_finish() {
        let tools: Vec<Arc<dyn Tool>> = vec![
            Arc::new(FakeTool::new("finish").with_identity("fake-finish", "claude")),
            Arc::new(FakeTool::new("done").with_identity(&finish_implementation(), "claude")),
        ];
        let report = Arc::new(Mutex::new(WorkerReport::new(vec![
            "finish".to_string(),
            "done".to_string(),
        ])));
        let tap = WorkerReportTap::new(
            Arc::new(p1_testkit::RecordingEvents::new()),
            report.clone(),
            &tools,
            FinishOutcome::default(),
            Arc::new(RecordingWorkerEnds::default()),
            "w1".to_string(),
            "route/model".to_string(),
            false,
        );
        tap.emit(AgentEvent::TurnStarted);
        // The real one first: a later call of the named fake would replace it if the
        // tap read it.
        for name in ["done", "finish"] {
            let input = format!(r#"{{"status":"blocked","summary":"{name}","needs":"edit"}}"#);
            tap.emit(AgentEvent::ToolStarted {
                call: call(name, name, &input),
            });
            tap.emit(AgentEvent::ToolFinished {
                result: result(name, name, ToolStatus::Ok, "ok"),
            });
        }
        let finish = report.lock().unwrap().finish.clone().expect("a finish");
        assert_eq!(finish.summary.as_deref(), Some("done"));
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
                // The model's input never carries evidence (ADR-0051 item 3).
                evidence: None,
            }
        );
        assert_eq!(parse_finish_call("not json"), None);
        assert_eq!(parse_finish_call(r#"{"summary":"s"}"#), None);
    }

    /// The evidence line comes from the child's ACCEPTED outcome — the cell the
    /// child's `finish` tool wrote — never from the model's input, and the next turn
    /// clears it (ADR-0051 item 3).
    #[cfg(feature = "delegation")]
    #[tokio::test]
    async fn the_tap_labels_the_accepted_outcome_and_clears_it_next_turn() {
        use p1_contracts::{CancellationToken, ToolContext};

        struct Fixed {
            change: Option<u64>,
            runs: Vec<ShellRun>,
        }

        impl p1_tool_finish::SessionActivity for Fixed {
            fn last_file_change(&self) -> Option<u64> {
                self.change
            }

            fn shell_runs(&self) -> Vec<ShellRun> {
                self.runs.clone()
            }
        }

        // `verification` as the model named it, the session's record, and the line the
        // report must carry.
        let cases = [
            (
                r#"["none"]"#,
                Fixed {
                    change: None,
                    runs: Vec::new(),
                },
                "not verified; parent verification required",
            ),
            (
                r#"["cargo test"]"#,
                Fixed {
                    change: None,
                    runs: vec![ShellRun {
                        command: "cargo test".to_string(),
                        exit_code: Some(0),
                        order: 1,
                    }],
                },
                "commands passed: cargo test",
            ),
        ];
        for (verification, activity, expected) in cases {
            let outcome = FinishOutcome::default();
            // The native tool accepts into its own cell; what it accepted is committed to
            // the host's cell, as the hub commits a verified candidate (ADR-0083 §2).
            let written = p1_tool_finish::FinishOutcome::default();
            let finish = p1_tool_finish::FinishTool::new(Arc::new(activity), written.clone());
            let tools: Vec<Arc<dyn Tool>> = vec![Arc::new(
                FakeTool::new("finish").with_identity(&finish_implementation(), "claude"),
            )];
            let report = Arc::new(Mutex::new(WorkerReport::new(vec!["finish".to_string()])));
            let tap = WorkerReportTap::new(
                Arc::new(p1_testkit::RecordingEvents::new()),
                report.clone(),
                &tools,
                outcome.clone(),
                Arc::new(RecordingWorkerEnds::default()),
                "w1".to_string(),
                "route/model".to_string(),
                false,
            );

            tap.emit(AgentEvent::TurnStarted);
            let raw =
                format!(r#"{{"status":"done","summary":"did it","verification":{verification}}}"#);
            // The model calls `finish`: the tool accepts and writes the outcome cell.
            let accepted = finish
                .execute(
                    &call("f1", "finish", &raw),
                    ToolContext {
                        cancel: CancellationToken::new(),
                    },
                )
                .await;
            assert_eq!(accepted.status, ToolStatus::Ok, "{}", accepted.content);
            outcome.set(written.get().expect("accepted"), written.structured());
            // The tap sees the same call through the child's events.
            tap.emit(AgentEvent::ToolStarted {
                call: call("f1", "finish", &raw),
            });
            tap.emit(AgentEvent::ToolFinished {
                result: result("f1", "finish", ToolStatus::Ok, "Finished."),
            });
            let snapshot = report.lock().unwrap().clone();
            assert_eq!(
                snapshot
                    .finish
                    .and_then(|finish| finish.evidence)
                    .as_deref(),
                Some(expected),
                "verification: {verification}"
            );

            // A continue is a new turn: the previous turn's evidence goes with it.
            tap.emit(AgentEvent::TurnStarted);
            assert_eq!(report.lock().unwrap().finish, None);
            assert_eq!(outcome.get(), None, "the outcome cell is cleared too");
        }
    }
}
