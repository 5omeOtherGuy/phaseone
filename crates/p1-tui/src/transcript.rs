//! The transcript model (handoff §6, §11): one ordered list of blocks that the
//! renderers lay out. Conversation earns no chrome; tool output does (a Block).
//!
//! The model consumes `AgentEvent`s (observation only, SPEC-adjacent contract:
//! events never carry UI decisions) and stores everything renderers need as
//! plain data, so the whole transcript is fixture-testable without a runtime.
//! Every event arrives with its stamp: the turn, reasoning and call clocks are
//! measured here, so a renderer only needs `now_ms` to draw live elapsed times.

use std::collections::HashMap;
use std::sync::Arc;

use crate::face::{CallFace, GenericDescriber, ResultFace, ToolDescriber};

use p1_contracts::{
    AgentEvent, ProviderErrorKind, StopReason, ToolCall, ToolResultItem, ToolStatus, TurnEnd, Usage,
};

use crate::fold::{Fold, FoldId};

/// One block in the transcript. Order is history; nothing is re-sorted.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq)]
pub enum Block {
    /// `›` operator input. `steering` marks a message delivered into a running
    /// turn (§6.2); follow-ups become ordinary turns and carry no tag.
    Operator { text: String, steering: bool },
    /// Assistant prose, unadorned.
    Prose { lines: Vec<String> },
    /// Reasoning collapses to `· reasoning Ns`; `^R` expands. While it streams
    /// `elapsed_ms` is `None` and the renderer counts from `started_ms`.
    Reasoning {
        lines: Vec<String>,
        expanded: bool,
        elapsed_ms: Option<u64>,
        started_ms: Option<u64>,
    },
    /// One tool call, from `▸ name arg` to its settled result.
    Call(ToolRow),
    /// Verbatim host text (the idle prelude): exact spacing, never
    /// wrapped — these are aligned affordance rows, not prose.
    Info { lines: Vec<String> },
    /// A turn ending or error (§6.7, §7.8).
    Notice(TurnNotice),
    /// A display-only meta row (§6.6). The text starts with its glyph (`· …`,
    /// `↳ …`), exactly as the host writes its notes.
    Meta { text: String },
    /// A meta row with right-aligned dim facts (context replacement usage).
    MetaFacts { text: String, facts: String },
    /// The host's settled line for a worker's end (§6.8).
    WorkerReport(WorkerReport),
    /// Output of an operator slash command (§6.9).
    CommandOutput(CommandOutput),
}

/// A turn notice (§6.7): a headline, then labelled fact rows in §7.8 order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnNotice {
    pub kind: NoticeKind,
    pub headline: String,
    pub facts: Vec<(NoticeFact, String)>,
}

/// The headline glyph: `✗` for what broke, faint `·` for what merely stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoticeKind {
    Failed,
    Stopped,
}

/// A notice fact label. `Next` is a key or command hint, so it is the one faint value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoticeFact {
    Cost,
    Kept,
    Next,
    Reason,
}

impl NoticeFact {
    pub fn label(self) -> &'static str {
        match self {
            Self::Cost => "cost",
            Self::Kept => "kept",
            Self::Next => "next",
            Self::Reason => "reason",
        }
    }
}

/// A worker's end as the parent sees it (§6.8, ADR-0050 §6).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkerReport {
    pub id: String,
    pub route: String,
    pub end: WorkerEnd,
    /// Preformatted like the WORKERS pane (`2m10s`), when known.
    pub elapsed: Option<String>,
    pub cost_micro_usd: Option<u64>,
    pub grants: Vec<String>,
    /// The report line: `done · verified · …`, `blocked: needs edit — …`.
    pub line: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkerEnd {
    Done,
    NotVerified,
    Blocked,
    Failed,
    Stalled,
    Cancelled,
}

/// A slash command's settled output (§6.9): a BLOCK+ header, BLOCK body rows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandOutput {
    pub command: String,
    pub argument: String,
    pub facts: String,
    pub body: Vec<CommandRow>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommandRow {
    /// A dim group label (`COMMANDS`).
    Head(String),
    /// An ink key in an 18-cell column, then a dim description.
    Entry { key: String, text: String },
}

/// What the turn working row says (§6.5).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TurnWorking {
    pub phase: TurnPhase,
    pub elapsed_ms: Option<u64>,
    /// 1-based request number, once a request started.
    pub request: Option<u32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TurnPhase {
    #[default]
    Waiting,
    Reasoning,
    Streaming,
    Preparing,
    /// No event starts a summarization yet; reachable only from host state.
    Summarizing,
}

impl TurnPhase {
    pub fn label(self) -> &'static str {
        match self {
            Self::Waiting => "waiting",
            Self::Reasoning => "reasoning",
            Self::Streaming => "streaming",
            Self::Preparing => "preparing",
            Self::Summarizing => "summarizing context",
        }
    }
}

/// One tool call row of the §3 column grid.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolRow {
    pub name: String,
    /// The one-line argument summary (newlines as `␤`, bounded).
    pub summary: String,
    pub status: RowStatus,
    /// Full output exactly as the model saw it, when a result arrived. The
    /// fold decision is renderable from this plus the status.
    /// Set on FAILED rows (the evidence block reads it); Ok rows keep no
    /// copy — oversized output lives only behind its fold handle.
    pub output: Option<String>,
    /// Output line count, settled once (the renderer never rescans content).
    pub line_count: usize,
    /// The fold handle when the output was registered (the `^O open` hint).
    pub fold: Option<crate::fold::FoldId>,
    pub elapsed_ms: Option<u64>,
    pub call_id: String,
    pub call: Option<ToolCall>,
    pub face: CallFace,
    pub result_face: Option<ResultFace>,
    pub input_preview: Option<String>,
}

/// The lifecycle of a call row: running (or parked on the operator), then
/// settled with the tool's status.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowStatus {
    Running,
    AwaitingApproval,
    Settled(ToolStatus),
}

impl ToolRow {
    /// The fold presentation of a settled row's output, when it earns one.
    /// Only a FAILED call expands its evidence inline (SPEC §4.3: a settled
    /// call collapses to one line); a successful call's oversized output is
    /// registered under its handle and opened with `^O`, never shown.
    pub fn fold(&self) -> Option<Fold> {
        let output = self.output.as_deref()?;
        match self.status {
            RowStatus::Settled(ToolStatus::Ok) => None,
            RowStatus::Settled(_) => Some(Fold::present_bounded(
                output,
                false,
                self.face.kind == crate::face::TargetKind::Command,
            )),
            RowStatus::Running | RowStatus::AwaitingApproval => None,
        }
    }

    /// A started call still running: it carries its own `▪▪▪`. A preparing
    /// row (input deltas, no name yet) does not.
    fn is_running_call(&self) -> bool {
        self.status == RowStatus::Running && !self.name.is_empty()
    }
}

/// Usage summed over one turn; a part poisoned by an unreported value stays
/// unknown (the same rule as the ledger's spend).
#[derive(Debug, Clone, Copy)]
struct TurnUsage {
    responses: u64,
    input: Option<u64>,
    output: Option<u64>,
    cost_micro_usd: Option<u64>,
}

impl Default for TurnUsage {
    /// Nothing recorded yet is a known zero; `None` is reserved for poisoned.
    fn default() -> Self {
        Self {
            responses: 0,
            input: Some(0),
            output: Some(0),
            cost_micro_usd: Some(0),
        }
    }
}

impl TurnUsage {
    fn record(&mut self, usage: Option<&Usage>) {
        self.responses += 1;
        let add = |slot: &mut Option<u64>, part: Option<u64>| {
            *slot = match (*slot, part) {
                (Some(total), Some(part)) => Some(total + part),
                _ => None,
            };
        };
        let input = usage.and_then(|u| {
            u.input_uncached
                .map(|base| base + u.cache_read.unwrap_or(0) + u.cache_write.unwrap_or(0))
        });
        add(&mut self.input, input);
        add(&mut self.output, usage.and_then(|u| u.output));
        add(
            &mut self.cost_micro_usd,
            usage.and_then(|u| u.cost_micro_usd),
        );
    }
}

/// The live turn: its clock, phase and what a notice needs to say at its end.
#[derive(Debug, Clone, Default)]
struct Turn {
    /// `None` when the turn's start carried no stamp.
    started_ms: Option<u64>,
    phase: TurnPhase,
    request: Option<u32>,
    usage: TurnUsage,
    /// Text streamed by the current request and not yet kept by a response.
    streamed_chars: usize,
    /// Calls this turn settled as cancelled, by name.
    cancelled: Vec<String>,
}

/// The transcript: blocks in order, plus the registry of full outputs behind
/// fold handles so `^O` opens the same object the transcript folded away.
pub struct Transcript {
    pub blocks: Vec<Block>,
    outputs: HashMap<FoldId, String>,
    /// The most recently registered fold — what `^O` opens.
    pub latest_fold: Option<FoldId>,
    /// Index of the block streaming text deltas land in, while it is open.
    open_text: Option<usize>,
    open_reasoning: Option<usize>,
    /// Call rows not yet settled, by provider call id, so a result settles the
    /// row its call started.
    running: HashMap<String, usize>,
    /// When each running call started (its `ToolStarted` stamp).
    call_started: HashMap<String, u64>,
    turn: Option<Turn>,
    /// Steering the operator queued, delivered at the next inbox boundary.
    steering: Vec<String>,
    describer: Arc<dyn ToolDescriber>,
    /// Whether a backticked span names a path that exists in the workspace.
    path_exists: fn(&str) -> bool,
}

impl std::fmt::Debug for Transcript {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Transcript")
            .field("blocks", &self.blocks)
            .field("latest_fold", &self.latest_fold)
            .finish_non_exhaustive()
    }
}

impl Default for Transcript {
    fn default() -> Self {
        Self::new()
    }
}

/// The default path check: nothing is known to exist, so prose stays verbatim.
fn no_path(_: &str) -> bool {
    false
}

impl Transcript {
    pub fn new() -> Self {
        Self::with_describer(Arc::new(GenericDescriber))
    }

    pub fn with_describer(describer: Arc<dyn ToolDescriber>) -> Self {
        Self {
            blocks: Vec::new(),
            outputs: HashMap::new(),
            latest_fold: None,
            open_text: None,
            open_reasoning: None,
            running: HashMap::new(),
            call_started: HashMap::new(),
            turn: None,
            steering: Vec::new(),
            describer,
            path_exists: no_path,
        }
    }

    /// The host's workspace check for backticked paths in prose (§6.3).
    pub fn set_path_exists(&mut self, exists: fn(&str) -> bool) {
        self.path_exists = exists;
    }

    pub fn path_exists(&self, path: &str) -> bool {
        (self.path_exists)(path)
    }

    /// The full output behind a fold handle (`^O` opens it in the pane).
    pub fn output(&self, id: &FoldId) -> Option<&str> {
        self.outputs.get(id).map(String::as_str)
    }

    /// The operator sent a prompt: the marked turn (§6.2).
    pub fn operator(&mut self, text: impl Into<String>) {
        self.close_streams();
        self.blocks.push(Block::Operator {
            text: text.into(),
            steering: false,
        });
    }

    /// The operator queued steering for the running turn; it enters the
    /// transcript when an inbox boundary delivers it.
    pub fn queue_steering(&mut self, text: impl Into<String>) {
        self.steering.push(text.into());
    }

    /// The host's line for a worker's end (§7.7).
    pub fn worker_report(&mut self, report: WorkerReport) {
        self.close_streams();
        self.blocks.push(Block::WorkerReport(report));
    }

    /// An operator slash command's output (§6.9).
    pub fn command_output(&mut self, output: CommandOutput) {
        self.close_streams();
        self.blocks.push(Block::CommandOutput(output));
    }

    /// The start stamp of a running call.
    pub fn call_started(&self, call_id: &str) -> Option<u64> {
        self.call_started.get(call_id).copied()
    }

    /// Whether a started call is running (it then carries the `▪▪▪`).
    pub fn call_running(&self) -> bool {
        self.running.values().any(
            |index| matches!(self.blocks.get(*index), Some(Block::Call(row)) if row.is_running_call()),
        )
    }

    /// The turn working row's facts at `now_ms`, while a turn is live.
    pub fn turn_working(&self, now_ms: u64) -> Option<TurnWorking> {
        let turn = self.turn.as_ref()?;
        Some(TurnWorking {
            phase: turn.phase,
            elapsed_ms: turn
                .started_ms
                .map(|started| now_ms.saturating_sub(started)),
            request: turn.request,
        })
    }

    /// Apply one observed event stamped `at_ms` (`None` for an unstamped
    /// event, e.g. a replay: its clocks stay unknown). Deltas extend open
    /// stream blocks; lifecycle events open and close them (§11).
    pub fn apply(&mut self, event: &AgentEvent, at_ms: Option<u64>) {
        if !matches!(event, AgentEvent::ReasoningDelta { .. }) {
            self.settle_reasoning(at_ms);
        }
        match event {
            AgentEvent::TurnStarted => {
                self.turn = Some(Turn {
                    started_ms: at_ms,
                    ..Turn::default()
                });
            }
            AgentEvent::RequestStarted { request_index } => {
                let turn = self.turn_mut(at_ms);
                turn.request = Some(request_index + 1);
                turn.phase = TurnPhase::Waiting;
                turn.streamed_chars = 0;
            }
            AgentEvent::TextDelta { text } => {
                if let Some(turn) = &mut self.turn {
                    turn.phase = TurnPhase::Streaming;
                    turn.streamed_chars += text.chars().count();
                }
                self.text_delta(text);
            }
            AgentEvent::ReasoningDelta { text } => {
                if let Some(turn) = &mut self.turn {
                    turn.phase = TurnPhase::Reasoning;
                }
                self.reasoning_delta(text, at_ms);
            }
            AgentEvent::ToolInputDelta { call_id, text } => {
                if let Some(turn) = &mut self.turn {
                    turn.phase = TurnPhase::Preparing;
                }
                self.tool_input_delta(call_id, text);
            }
            // The host usually intercepts notices as its own notes; one that
            // reaches the model is the same meta row.
            AgentEvent::ProviderNotice { text } => {
                self.close_streams();
                self.blocks.push(Block::Meta {
                    text: format!("{} {text}", crate::glyphs::PENDING),
                });
            }
            AgentEvent::ResponseCompleted { usage, .. } => {
                if let Some(turn) = &mut self.turn {
                    turn.usage.record(usage.as_ref());
                    turn.streamed_chars = 0;
                }
                self.close_streams();
            }
            AgentEvent::InboxDelivered { count } => self.inbox_delivered(*count),
            AgentEvent::ContextReplaced {
                items_before,
                items_after,
                usage,
            } => {
                if let Some(turn) = &mut self.turn {
                    turn.usage.record(usage.as_ref());
                }
                self.close_streams();
                self.blocks.push(Block::MetaFacts {
                    text: format!(
                        "{} context summarized · {items_before} → {items_after} items",
                        crate::glyphs::PENDING
                    ),
                    facts: usage_facts(usage.as_ref()),
                });
            }
            AgentEvent::ToolStarted { call } => self.tool_started(call, at_ms),
            AgentEvent::ToolFinished { result } => self.tool_finished(result, at_ms),
            AgentEvent::TurnFinished { end } => self.turn_finished(end, at_ms),
        }
    }

    /// Events outside a `TurnStarted` (a resumed stream, a test) still get a
    /// turn so the working row and the notice have something to count.
    fn turn_mut(&mut self, at_ms: Option<u64>) -> &mut Turn {
        self.turn.get_or_insert_with(|| Turn {
            started_ms: at_ms,
            ..Turn::default()
        })
    }

    /// Reasoning ends at the first non-reasoning event (§6.4). Without a
    /// stamp its span is unknown, and it stops counting.
    fn settle_reasoning(&mut self, at_ms: Option<u64>) {
        if let Some(index) = self.open_reasoning
            && let Some(Block::Reasoning {
                elapsed_ms,
                started_ms,
                ..
            }) = self.blocks.get_mut(index)
            && elapsed_ms.is_none()
            && let Some(started) = *started_ms
        {
            *elapsed_ms = at_ms.map(|at| at.saturating_sub(started));
            if elapsed_ms.is_none() {
                *started_ms = None;
            }
        }
    }

    /// Queued steering becomes operator turns tagged `steering`; whatever the
    /// boundary delivered beyond them is one meta row.
    fn inbox_delivered(&mut self, count: usize) {
        self.close_streams();
        let steered = count.min(self.steering.len());
        for text in self.steering.drain(..steered) {
            self.blocks.push(Block::Operator {
                text,
                steering: true,
            });
        }
        let rest = count - steered;
        if rest > 0 {
            let noun = if rest == 1 { "message" } else { "messages" };
            self.blocks.push(Block::Meta {
                text: format!("{} {rest} inbox {noun} delivered", crate::glyphs::PENDING),
            });
        }
    }

    fn text_delta(&mut self, text: &str) {
        // Text after reasoning within one response is a NEW block: the
        // transcript's order is the model's output order.
        if self.open_reasoning.is_some()
            && self.open_text.is_some_and(
                |i| !matches!(self.blocks.last(), Some(b) if std::ptr::eq(b, &self.blocks[i])),
            )
        {
            self.open_text = None;
        }
        let index = match self.open_text {
            Some(index) => index,
            None => {
                self.blocks.push(Block::Prose { lines: Vec::new() });
                self.open_text = Some(self.blocks.len() - 1);
                self.blocks.len() - 1
            }
        };
        if let Block::Prose { lines, .. } = &mut self.blocks[index] {
            append_text(lines, text);
        }
    }

    fn reasoning_delta(&mut self, text: &str, at_ms: Option<u64>) {
        let index = match self.open_reasoning {
            Some(index) => index,
            None => {
                self.blocks.push(Block::Reasoning {
                    lines: Vec::new(),
                    expanded: false,
                    elapsed_ms: None,
                    started_ms: at_ms,
                });
                self.open_reasoning = Some(self.blocks.len() - 1);
                self.blocks.len() - 1
            }
        };
        if let Block::Reasoning { lines, .. } = &mut self.blocks[index] {
            append_text(lines, text);
        }
    }

    fn tool_started(&mut self, call: &ToolCall, at_ms: Option<u64>) {
        self.close_streams();
        let face = self.describer.call(call);
        let existing = self
            .running
            .remove(&call.call_id)
            .filter(|index| matches!(self.blocks.get(*index), Some(Block::Call(_))));
        let row = ToolRow {
            name: call.name.clone(),
            summary: face.target.clone(),
            status: RowStatus::Running,
            output: None,
            line_count: 0,
            fold: None,
            elapsed_ms: None,
            call_id: call.call_id.clone(),
            call: Some(call.clone()),
            face,
            result_face: None,
            input_preview: None,
        };
        let index = if let Some(index) = existing {
            self.blocks[index] = Block::Call(row);
            index
        } else {
            self.blocks.push(Block::Call(row));
            self.blocks.len() - 1
        };
        self.running.insert(call.call_id.clone(), index);
        if let Some(at_ms) = at_ms {
            self.call_started.insert(call.call_id.clone(), at_ms);
        }
    }

    fn tool_input_delta(&mut self, call_id: &str, text: &str) {
        if let Some(index) = self.running.get(call_id).copied()
            && let Some(Block::Call(row)) = self.blocks.get_mut(index)
        {
            let preview = row.input_preview.get_or_insert_with(String::new);
            preview.push_str(text);
            row.summary = summarize_input(preview.lines().last().unwrap_or(preview));
            row.input_preview = Some(preview.clone());
            return;
        }
        self.close_streams();
        let preview = text.to_string();
        let face = CallFace {
            target: summarize_input(preview.lines().last().unwrap_or(&preview)),
            kind: crate::face::TargetKind::Plain,
        };
        self.blocks.push(Block::Call(ToolRow {
            name: String::new(),
            summary: face.target.clone(),
            status: RowStatus::Running,
            output: None,
            line_count: 0,
            fold: None,
            elapsed_ms: None,
            call_id: call_id.to_string(),
            call: None,
            face,
            result_face: None,
            input_preview: Some(preview),
        }));
        self.running
            .insert(call_id.to_string(), self.blocks.len() - 1);
    }

    fn tool_finished(&mut self, result: &ToolResultItem, at_ms: Option<u64>) {
        let elapsed = self
            .call_started
            .get(&result.call_id)
            .zip(at_ms)
            .map(|(started, at)| at.saturating_sub(*started));
        if result.status == ToolStatus::Cancelled
            && let Some(turn) = &mut self.turn
        {
            turn.cancelled.push(result.name.clone());
        }
        self.settle(
            &result.call_id,
            &result.name,
            RowStatus::Settled(result.status),
            &result.content,
            elapsed,
            Some(result),
        );
    }

    /// Settle the row a call started, wherever the outcome came from (live
    /// event or journal replay). An orphan result — no row ever opened for its
    /// call id — still renders (SPEC §4.9: nothing disappears).
    fn settle(
        &mut self,
        call_id: &str,
        name: &str,
        status: RowStatus,
        content: &str,
        elapsed_ms: Option<u64>,
        result: Option<&ToolResultItem>,
    ) {
        self.call_started.remove(call_id);
        let index = match self.running.remove(call_id) {
            Some(index) => index,
            None => {
                self.blocks.push(Block::Call(ToolRow {
                    name: name.to_string(),
                    summary: String::new(),
                    status,
                    output: None,
                    line_count: 0,
                    fold: None,
                    elapsed_ms: None,
                    call_id: call_id.to_string(),
                    call: None,
                    face: CallFace {
                        target: String::new(),
                        kind: crate::face::TargetKind::Plain,
                    },
                    result_face: None,
                    input_preview: None,
                }));
                self.blocks.len() - 1
            }
        };
        let Block::Call(row) = &mut self.blocks[index] else {
            return;
        };
        row.status = status;
        row.elapsed_ms = elapsed_ms;
        if let Some(result) = result
            && let Some(call) = row.call.as_ref()
        {
            row.result_face = Some(self.describer.result(call, result));
        }
        row.line_count = content.lines().count();
        if !content.is_empty() {
            match status {
                // Failures keep the output on the row (the evidence block reads
                // it); successes store oversized output only behind the handle.
                RowStatus::Settled(ToolStatus::Ok) => {
                    if let Fold::Folded { id, .. } = Fold::present(content) {
                        row.fold = Some(id.clone());
                        self.latest_fold = Some(id.clone());
                        self.outputs.insert(id, content.to_string());
                    }
                }
                _ => {
                    if let Fold::Folded { id, .. } = Fold::present(content) {
                        row.fold = Some(id.clone());
                        self.latest_fold = Some(id.clone());
                        self.outputs.insert(id, content.to_string());
                    }
                    row.output = Some(content.to_string());
                }
            }
        }
    }

    /// `^R`: toggle the most recent reasoning block (SPEC §4.2).
    pub fn toggle_reasoning(&mut self) {
        for block in self.blocks.iter_mut().rev() {
            if let Block::Reasoning { expanded, .. } = block {
                *expanded = !*expanded;
                return;
            }
        }
    }

    /// A meta row from the driver; the text carries its glyph (`· …`).
    pub fn note(&mut self, text: &str) {
        self.close_streams();
        self.blocks.push(Block::Meta {
            text: text.to_string(),
        });
    }

    /// Paint a resumed journal's projected history into transcript blocks, so
    /// a resumed session shows where it stands (the same shapes, unelapsed).
    pub fn paint_history(&mut self, items: &[p1_contracts::Item]) {
        use p1_contracts::{AssistantBlock, Item};
        for item in items {
            match item {
                Item::User { text } => self.operator(text.clone()),
                Item::Inbox { text, .. } => self.blocks.push(Block::Meta { text: text.clone() }),
                Item::Assistant(assistant) => {
                    for block in &assistant.blocks {
                        match block {
                            AssistantBlock::Text { text } => {
                                self.close_streams();
                                self.blocks.push(Block::Prose {
                                    lines: text.lines().map(str::to_string).collect(),
                                });
                            }
                            AssistantBlock::Reasoning { text, .. } => {
                                self.close_streams();
                                self.blocks.push(Block::Reasoning {
                                    lines: text.lines().map(str::to_string).collect(),
                                    expanded: false,
                                    elapsed_ms: None,
                                    started_ms: None,
                                });
                            }
                            AssistantBlock::ToolCall(call) => self.tool_started(call, None),
                        }
                    }
                }
                Item::ToolResult(result) => self.settle(
                    &result.call_id,
                    &result.name,
                    RowStatus::Settled(result.status),
                    &result.content,
                    None,
                    Some(result),
                ),
            }
        }
    }

    fn turn_finished(&mut self, end: &TurnEnd, at_ms: Option<u64>) {
        self.close_streams();
        let mut turn = self.turn.take();
        // Calls left running at the turn's end are reconciled, not left
        // spinning: a cancel settles them cancelled, any other end leaves
        // their outcome unknown by definition.
        let cancelled = matches!(end, TurnEnd::Cancelled);
        let mut leftover: Vec<(usize, String)> = self
            .running
            .iter()
            .map(|(id, index)| (*index, id.clone()))
            .collect();
        leftover.sort();
        for (_, call_id) in leftover {
            let name = match self.running.get(&call_id).and_then(|i| self.blocks.get(*i)) {
                Some(Block::Call(row)) if !row.name.is_empty() => row.name.clone(),
                _ => "?".to_string(),
            };
            let status = if cancelled {
                if let Some(turn) = &mut turn {
                    turn.cancelled.push(name.clone());
                }
                ToolStatus::Cancelled
            } else {
                ToolStatus::Unknown
            };
            self.settle(&call_id, &name, RowStatus::Settled(status), "", None, None);
        }
        let dropped = std::mem::take(&mut self.steering).len();
        if let Some(notice) = turn_notice(end, turn.as_ref(), at_ms, dropped) {
            self.blocks.push(Block::Notice(notice));
        }
    }

    fn close_streams(&mut self) {
        self.open_text = None;
        self.open_reasoning = None;
    }
}

/// `in 18.2k · out 1.1k` for a context replacement; `—` when nothing was reported.
fn usage_facts(usage: Option<&Usage>) -> String {
    let Some(usage) = usage else {
        return crate::render::UNKNOWN.into();
    };
    let input = usage
        .input_uncached
        .map(|base| base + usage.cache_read.unwrap_or(0) + usage.cache_write.unwrap_or(0));
    let part =
        |v: Option<u64>| v.map_or_else(|| crate::render::UNKNOWN.into(), crate::render::tokens);
    format!("in {} · out {}", part(input), part(usage.output))
}

/// The `cost` fact (§7.8): requests, then the turn's summed usage, then what
/// a broken stream lost. Unknown parts are omitted; an unknown price is `—`.
fn turn_cost(turn: &Turn, with_requests: bool, lost_stream: bool) -> String {
    let mut parts = Vec::new();
    if with_requests && let Some(request) = turn.request {
        parts.push(format!("request {request}"));
    }
    let usage = &turn.usage;
    if usage.responses > 0 {
        if let Some(input) = usage.input {
            parts.push(format!("in {}", crate::render::tokens(input)));
        }
        if let Some(output) = usage.output {
            parts.push(format!("out {}", crate::render::tokens(output)));
        }
    }
    if lost_stream && turn.streamed_chars > 0 {
        parts.push(format!(
            "{} out streamed, not kept",
            crate::render::tokens(turn.streamed_chars as u64)
        ));
    } else {
        parts.push(
            usage
                .cost_micro_usd
                .filter(|_| usage.responses > 0)
                .map_or_else(|| crate::render::UNKNOWN.into(), crate::render::cost_string),
        );
    }
    parts.join(" · ")
}

/// The §7.8 table: what a turn's end says, when it says anything.
fn turn_notice(
    end: &TurnEnd,
    turn: Option<&Turn>,
    at_ms: Option<u64>,
    dropped: usize,
) -> Option<TurnNotice> {
    use NoticeFact::{Cost, Kept, Next};
    let journal = || (Kept, "journal".to_string());
    let with_detail = |head: &str, detail: &str| {
        if detail.is_empty() {
            head.to_string()
        } else {
            format!("{head} · {detail}")
        }
    };
    let stopped = |headline: &str| TurnNotice {
        kind: NoticeKind::Stopped,
        headline: headline.into(),
        facts: Vec::new(),
    };
    let failed = |headline: String, facts: Vec<(NoticeFact, String)>| TurnNotice {
        kind: NoticeKind::Failed,
        headline,
        facts,
    };
    let cost = |with_requests: bool, lost_stream: bool| {
        turn.map(|t| (Cost, turn_cost(t, with_requests, lost_stream)))
    };
    let larger_window = || (Next, "/model to a larger window".to_string());
    Some(match end {
        TurnEnd::Completed { stop } => match stop {
            StopReason::EndTurn | StopReason::ToolUse => return None,
            StopReason::MaxOutputTokens => TurnNotice {
                facts: cost(false, false).into_iter().collect(),
                ..stopped("stopped · max output tokens")
            },
            StopReason::ContextWindowExceeded => failed(
                "context window exceeded".into(),
                vec![journal(), larger_window()],
            ),
            StopReason::Refusal => stopped("stopped · the model refused"),
            StopReason::Paused => stopped("stopped · paused by the provider"),
            StopReason::Other => stopped("stopped"),
        },
        TurnEnd::Cancelled => {
            let headline = match turn.and_then(|t| t.started_ms).zip(at_ms) {
                Some((started, at)) => format!(
                    "cancelled at {}",
                    crate::render::elapsed(at.saturating_sub(started))
                ),
                None => "cancelled".into(),
            };
            let mut kept = vec!["journal".to_string()];
            for name in turn.map(|t| t.cancelled.as_slice()).unwrap_or_default() {
                kept.push(format!("{name} settled as cancelled"));
            }
            if dropped > 0 {
                kept.push(format!("dropped {dropped} queued"));
            }
            TurnNotice {
                facts: cost(true, false)
                    .into_iter()
                    .chain([(Kept, kept.join(" · "))])
                    .collect(),
                ..stopped(&headline)
            }
        }
        TurnEnd::ProviderFailed { error } => {
            let message = error.message.as_str();
            match error.kind {
                ProviderErrorKind::Authentication => failed(
                    with_detail("authentication failed", message),
                    vec![journal(), (Next, "p1 login --list".into())],
                ),
                ProviderErrorKind::InsufficientBalance => failed(
                    format!(
                        "{} · not retried",
                        with_detail("account exhausted", message)
                    ),
                    vec![
                        journal(),
                        (Next, "/model to continue on another route".into()),
                    ],
                ),
                ProviderErrorKind::RateLimited => failed(
                    with_detail("rate limited", message),
                    cost(true, false)
                        .into_iter()
                        .chain([journal(), (Next, "wait for the window, or /model".into())])
                        .collect(),
                ),
                ProviderErrorKind::Transport => failed(
                    format!("connection failed · {error}"),
                    cost(true, true)
                        .into_iter()
                        .chain([journal(), (Next, "send again to continue · /model".into())])
                        .collect(),
                ),
                ProviderErrorKind::ContextWindowExceeded => failed(
                    with_detail("context window exceeded", message),
                    vec![journal(), larger_window()],
                ),
                ProviderErrorKind::Protocol | ProviderErrorKind::InvalidRequest => failed(
                    with_detail(&format!("provider error · {:?}", error.kind), message),
                    vec![journal()],
                ),
            }
        }
        TurnEnd::CommitFailed { message } => {
            // "free space" is only advice when the disk is what failed.
            let next = if message.contains("No space left") || message.contains("os error 28") {
                "free space, then restart p1 to resume"
            } else {
                "restart p1 to resume from the journal"
            };
            failed(
                with_detail("journal commit failed", message),
                vec![
                    (
                        Kept,
                        "nothing after the last committed record happened".into(),
                    ),
                    (Next, next.into()),
                ],
            )
        }
        TurnEnd::ContextFailed { message } => failed(
            with_detail("context failed", message),
            vec![
                (
                    Cost,
                    format!("summary request · {}", crate::render::UNKNOWN),
                ),
                (Kept, "history unchanged · journal".into()),
                larger_window(),
            ],
        ),
    })
}

/// Append streamed text to a line buffer, splitting on newlines.
fn append_text(lines: &mut Vec<String>, text: &str) {
    for (n, part) in text.split('\n').enumerate() {
        if n > 0 || lines.is_empty() {
            lines.push(String::new());
        }
        lines.last_mut().expect("a line exists").push_str(part);
    }
}

/// The one-line input summary for a call row (SPEC §3). Raw JSON is not a
/// summary: a structured call shows its salient field (the command, the
/// path, the pattern); anything else falls back to the bounded raw form.
pub fn summarize_input(raw: &str) -> String {
    raw.replace('\n', "␤").chars().take(100).collect()
}

/// The display summary for a call: the salient field when the input is JSON
/// with one, else the bounded raw input.
pub fn summarize_call(_name: &str, raw: &str) -> String {
    crate::face::GenericDescriber
        .call(&ToolCall {
            call_id: String::new(),
            name: String::new(),
            input: p1_contracts::ToolInput::Text(raw.to_string()),
        })
        .target
}

/// Extract a string field's value from flat JSON, for DISPLAY only: this is a
/// summary, not a parse — a miss or an escape edge case shows the raw input
/// instead. p1-tui carries no JSON dependency for a display hint.
#[allow(dead_code)]
fn json_string_field<'a>(raw: &'a str, key: &str) -> Option<&'a str> {
    if key.is_empty() {
        return None;
    }
    let needle = format!("\"{key}\"");
    let start = raw.find(&needle)? + needle.len();
    let rest = raw[start..].trim_start_matches([' ', ':']);
    let rest = rest.strip_prefix('"')?;
    // Find the closing quote, skipping \-escaped characters.
    let mut end = None;
    let mut escaped = false;
    for (i, c) in rest.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        match c {
            '\\' => escaped = true,
            '"' => {
                end = Some(i);
                break;
            }
            _ => {}
        }
    }
    Some(&rest[..end?])
}

#[cfg(test)]
mod tests {
    use super::*;
    use p1_contracts::{ToolInput, ToolStatus};

    fn call(id: &str, name: &str, input: &str) -> AgentEvent {
        AgentEvent::ToolStarted {
            call: ToolCall {
                call_id: id.into(),
                name: name.into(),
                input: ToolInput::Json(input.into()),
            },
        }
    }

    fn result(id: &str, name: &str, status: ToolStatus, content: &str) -> AgentEvent {
        AgentEvent::ToolFinished {
            result: ToolResultItem {
                call_id: id.into(),
                name: name.into(),
                status,
                content: content.into(),
            },
        }
    }

    #[test]
    fn summarize_call_shows_the_salient_field() {
        assert_eq!(
            summarize_call("shell", r#"{"command":"cargo test -p p1-tui"}"#),
            "cargo test -p p1-tui"
        );
        assert_eq!(
            summarize_call("edit", r#"{"file_path":"src/lib.rs","old_string":"a"}"#),
            "src/lib.rs"
        );
        assert_eq!(
            summarize_call("search", r#"{"pattern":"block_until_ready"}"#),
            "block_until_ready"
        );
    }

    #[test]
    fn summarize_call_falls_back_to_the_raw_input() {
        // An unmapped tool, or a miss on the salient key, shows the bounded
        // raw input rather than nothing.
        assert_eq!(summarize_call("unmapped", r#"{"id":"w1"}"#), "w1");
        assert_eq!(summarize_call("unmapped", r#"{"cmd":"ls"}"#), "ls");
    }

    #[test]
    fn deltas_stream_into_one_prose_block_until_a_boundary() {
        let mut t = Transcript::new();
        t.apply(&AgentEvent::TextDelta { text: "hel".into() }, None);
        t.apply(
            &AgentEvent::TextDelta {
                text: "lo\nwor".into(),
            },
            None,
        );
        let [Block::Prose { lines }] = &t.blocks[..] else {
            panic!("one prose block");
        };
        assert_eq!(lines, &["hello", "wor"]);
        // A response boundary closes the stream: the next delta is a NEW block.
        t.apply(
            &AgentEvent::ResponseCompleted {
                model: "m".into(),
                stop: p1_contracts::StopReason::EndTurn,
                usage: None,
            },
            None,
        );
        t.apply(
            &AgentEvent::TextDelta {
                text: "next".into(),
            },
            None,
        );
        assert_eq!(t.blocks.len(), 2);
    }

    #[test]
    fn a_response_boundary_closes_the_stream() {
        let mut t = Transcript::new();
        t.apply(&AgentEvent::TextDelta { text: "a".into() }, None);
        t.apply(
            &AgentEvent::ResponseCompleted {
                model: "m".into(),
                stop: p1_contracts::StopReason::EndTurn,
                usage: None,
            },
            None,
        );
        t.apply(&AgentEvent::TextDelta { text: "b".into() }, None);
        let [Block::Prose { .. }, Block::Prose { .. }] = &t.blocks[..] else {
            panic!("two prose blocks");
        };
    }

    #[test]
    fn a_result_settles_its_own_row_and_registers_the_fold() {
        let mut t = Transcript::new();
        let big: String = (0..90).map(|n| format!("line {n}\n")).collect();
        t.apply(&call("c1", "shell", "cargo test"), Some(1_000));
        t.apply(&call("c2", "read", "src/lib.rs"), Some(1_100));
        t.apply(
            &result("c1", "shell", ToolStatus::Error, &big),
            Some(12_400),
        );
        let [Block::Call(shell), Block::Call(read)] = &t.blocks[..] else {
            panic!("two call rows");
        };
        assert_eq!(shell.status, RowStatus::Settled(ToolStatus::Error));
        assert_eq!(shell.elapsed_ms, Some(11_400));
        assert_eq!(read.status, RowStatus::Running);
        // The folded output is addressable by its handle.
        let Some(Fold::Folded { id, .. }) = shell.fold() else {
            panic!("a failed large output folds");
        };
        assert_eq!(t.output(&id), Some(big.as_str()));
    }

    #[test]
    fn a_successful_small_output_stays_one_line() {
        let mut t = Transcript::new();
        t.apply(&call("c1", "read", "src/lib.rs"), None);
        t.apply(&result("c1", "read", ToolStatus::Ok, "fn main() {}"), None);
        let [Block::Call(row)] = &t.blocks[..] else {
            panic!("one call row");
        };
        assert_eq!(row.fold(), None);
    }

    #[test]
    fn toggle_reasoning_expands_and_collapses_the_last_block() {
        let mut t = Transcript::new();
        t.apply(&AgentEvent::ReasoningDelta { text: "why".into() }, None);
        t.apply(
            &AgentEvent::ResponseCompleted {
                model: "m".into(),
                stop: p1_contracts::StopReason::EndTurn,
                usage: None,
            },
            None,
        );
        t.toggle_reasoning();
        let [Block::Reasoning { expanded, .. }] = &t.blocks[..] else {
            panic!("one reasoning block");
        };
        assert!(expanded);
        t.toggle_reasoning();
        let [Block::Reasoning { expanded, .. }] = &t.blocks[..] else {
            panic!("one reasoning block");
        };
        assert!(!expanded);
    }

    #[test]
    fn summarize_bounds_and_joins() {
        assert_eq!(summarize_input("a\nb"), "a␤b");
        assert_eq!(summarize_input(&"x".repeat(200)).chars().count(), 100);
        assert_eq!(summarize_call("shell", r#"{"command":"ls -la"}"#), "ls -la");
        assert_eq!(summarize_call("read", "not json"), "not json");
    }

    #[test]
    fn a_failed_turn_leaves_a_notice_not_a_banner() {
        let mut t = Transcript::new();
        t.apply(
            &AgentEvent::TurnFinished {
                end: TurnEnd::ProviderFailed {
                    error: p1_contracts::ProviderError {
                        kind: p1_contracts::ProviderErrorKind::Transport,
                        message: "connection dropped".into(),
                    },
                },
            },
            None,
        );
        assert!(
            matches!(&t.blocks[..], [Block::Notice(notice)] if notice.headline.contains("connection dropped"))
        );
    }
}
