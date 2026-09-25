//! The transcript model (SPEC §3, §4): one ordered list of blocks that the
//! renderers lay out. Conversation earns no chrome; tool output does (a fold
//! block). Settled calls collapse to one line — the fold block appears only
//! where the outcome needs the evidence: a failed call shows its output head.
//!
//! The model consumes `AgentEvent`s (observation only, SPEC-adjacent contract:
//! events never carry UI decisions) and stores everything renderers need as
//! plain data, so the whole transcript is fixture-testable without a runtime.

use std::collections::HashMap;

use p1_contracts::{AgentEvent, ToolCall, ToolResultItem, ToolStatus, TurnEnd};

use crate::fold::{Fold, FoldId};

/// One block in the transcript. Order is history; nothing is re-sorted.
#[derive(Debug, Clone, PartialEq)]
pub enum Block {
    /// `›` operator input — the one marked turn.
    Operator { text: String },
    /// Assistant prose, unadorned.
    Prose { lines: Vec<String> },
    /// Reasoning collapses to `· reasoning Ns`; `^R` expands.
    Reasoning {
        lines: Vec<String>,
        expanded: bool,
        elapsed_ms: Option<u64>,
    },
    /// One tool call, from `▸ name arg` to its settled one-line result.
    Call(ToolRow),
    /// Verbatim host text (the §4.1 idle prelude): exact spacing, never
    /// wrapped — these are aligned affordance rows, not prose.
    Info { lines: Vec<String> },
    /// A turn-level notice (§4.9): what broke, what it cost, what is intact.
    Notice { lines: Vec<String> },
    /// A quiet meta line (context replacement, inbox delivery): DIM, one row.
    Meta { text: String },
}

/// One tool call row of the §3 column grid.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolRow {
    pub name: String,
    /// Original tool input, retained for rendering successful edits and writes.
    pub input: String,
    /// The one-line argument summary (newlines as `␤`, bounded).
    pub summary: String,
    pub status: RowStatus,
    /// Full output exactly as the model saw it, when a result arrived. The
    /// fold decision is renderable from this plus the status.
    pub output: Option<String>,
    pub output_id: Option<FoldId>,
    pub elapsed_ms: Option<u64>,
    /// Delegation indents ONCE and never more (SPEC §3).
    pub depth: u8,
    /// The tool actually ran (a `ToolStarted`); a denied or never-started call
    /// shows only its reason, no run facts.
    pub started: bool,
    /// The change as reviewed before it ran (edit/write/patch), with real line
    /// numbers and context: the settled block shows the same rows.
    pub diff: Option<Vec<crate::render::diff::DiffRow>>,
}

/// The lifecycle of a call row: running, then settled with the tool's status.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowStatus {
    Running,
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
            RowStatus::Settled(_) => Some(Fold::present(output)),
            RowStatus::Running => None,
        }
    }
}

/// The transcript: blocks in order, plus the registry of full outputs behind
/// fold handles so `^O` opens the same object the transcript folded away.
#[derive(Debug, Default)]
pub struct Transcript {
    pub blocks: Vec<Block>,
    /// Per-block disclosure override: absent retains the compact preview.
    pub disclosures: HashMap<usize, bool>,
    pub(crate) render_cache: std::cell::RefCell<crate::render::block::Cache>,
    outputs: HashMap<FoldId, String>,
    /// The most recently registered fold — what `^O` opens.
    pub latest_fold: Option<FoldId>,
    /// Index of the block streaming text deltas land in, while it is open.
    open_text: Option<usize>,
    open_reasoning: Option<usize>,
    /// Call rows not yet settled, by provider call id, so a result settles the
    /// row its call started.
    running: HashMap<String, usize>,
    /// Every call row by call id: a late result (a reconciled call after resume)
    /// updates its row instead of adding a second one.
    rows_by_id: HashMap<String, usize>,
    /// Calls the host announced before they started (a parked approval), so a
    /// denied or never-started call still shows what it would have done.
    announced: HashMap<String, ToolCall>,
    /// When the open reasoning stream began, for its `· reasoning 4.2s` row.
    reasoning_started_ms: Option<u64>,
    /// Pre-execution diffs by call id, waiting for their row.
    diffs: HashMap<String, Vec<crate::render::diff::DiffRow>>,
}

impl Transcript {
    pub fn new() -> Self {
        Self::default()
    }

    /// The full output behind a fold handle (`^O` opens it in the pane).
    pub fn output(&self, id: &FoldId) -> Option<&str> {
        self.outputs.get(id).map(String::as_str)
    }

    /// Styled blocks laid out so far (layout is lazy: tests assert that a
    /// resize re-renders what shows, not the history).
    pub fn rendered_events(&self) -> usize {
        self.render_cache.borrow().rendered_events
    }

    pub(crate) fn running_indices(&self) -> impl Iterator<Item = usize> + '_ {
        self.running.values().copied()
    }

    pub fn output_handles(&self) -> Vec<(FoldId, usize)> {
        let mut handles: Vec<_> = self
            .outputs
            .iter()
            .filter(|(id, _)| id.0.len() == 6)
            .map(|(id, body)| (id.clone(), body.lines().count()))
            .collect();
        handles.sort_by(|a, b| a.0.0.cmp(&b.0.0));
        handles
    }

    /// The operator sent a prompt: the marked turn (SPEC §2 `›`).
    pub fn operator(&mut self, text: impl Into<String>) {
        self.close_streams();
        self.blocks.push(Block::Operator { text: text.into() });
    }

    /// Apply one observed event. Deltas extend open stream blocks; lifecycle
    /// events open and close them. `ToolInputDelta` is display-only freeform
    /// preview and intentionally ignored: a row appears at `ToolStarted`.
    pub fn apply(&mut self, event: &AgentEvent, elapsed_ms: Option<u64>) {
        self.apply_at(event, elapsed_ms, None);
    }

    /// [`Transcript::apply`] with the event's stamp, which times reasoning.
    pub fn apply_at(&mut self, event: &AgentEvent, elapsed_ms: Option<u64>, now_ms: Option<u64>) {
        if let (Some(now), Some(index)) = (now_ms, self.open_reasoning) {
            match event {
                AgentEvent::ReasoningDelta { .. } => {
                    self.reasoning_started_ms.get_or_insert(now);
                }
                // Anything else that follows reasoning ends it.
                _ => {
                    if let Some(started) = self.reasoning_started_ms.take()
                        && let Block::Reasoning { elapsed_ms, .. } = &mut self.blocks[index]
                    {
                        *elapsed_ms = Some(now.saturating_sub(started));
                        self.render_cache.borrow_mut().invalidate(index);
                    }
                }
            }
        } else if let (Some(now), AgentEvent::ReasoningDelta { .. }) = (now_ms, event) {
            self.reasoning_started_ms = Some(now);
        }
        let dirty = match event {
            AgentEvent::ToolFinished { result } => self.running.get(&result.call_id).copied(),
            AgentEvent::TextDelta { .. } => self.open_text,
            AgentEvent::ReasoningDelta { .. } => self.open_reasoning,
            _ => None,
        };
        if let Some(index) = dirty {
            self.render_cache.borrow_mut().invalidate(index);
        }
        match event {
            AgentEvent::TurnStarted | AgentEvent::RequestStarted { .. } => {}
            AgentEvent::TextDelta { text } => self.text_delta(text),
            AgentEvent::ReasoningDelta { text } => self.reasoning_delta(text),
            AgentEvent::ToolInputDelta { .. } => {}
            AgentEvent::ResponseCompleted { .. } => self.close_streams(),
            AgentEvent::InboxDelivered { count } => {
                self.blocks.push(Block::Meta {
                    text: format!(
                        "{} {count} notification(s) delivered",
                        crate::glyphs::PENDING
                    ),
                });
            }
            AgentEvent::ContextReplaced {
                items_before,
                items_after,
                ..
            } => {
                self.blocks.push(Block::Meta {
                    text: format!("context: summarized {items_before} → {items_after} items"),
                });
            }
            AgentEvent::ToolStarted { call } => self.tool_started(call),
            AgentEvent::ToolFinished { result } => self.tool_finished(result, elapsed_ms),
            AgentEvent::TurnFinished { end } => self.turn_finished(end),
        }
    }

    fn text_delta(&mut self, text: &str) {
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

    fn reasoning_delta(&mut self, text: &str) {
        let index = match self.open_reasoning {
            Some(index) => index,
            None => {
                self.blocks.push(Block::Reasoning {
                    lines: Vec::new(),
                    expanded: false,
                    elapsed_ms: None,
                });
                self.open_reasoning = Some(self.blocks.len() - 1);
                self.blocks.len() - 1
            }
        };
        if let Block::Reasoning { lines, .. } = &mut self.blocks[index] {
            append_text(lines, text);
        }
    }

    /// The host saw a call before it started (it parked for approval): keep its
    /// input so the row can show it whatever the decision.
    pub fn announce(&mut self, call: &ToolCall) {
        self.announced.insert(call.call_id.clone(), call.clone());
    }

    /// The change a call will make, computed by the host before it runs (it can
    /// still read the old file): the settled block shows these rows.
    pub fn attach_diff(&mut self, call_id: &str, rows: Vec<crate::render::diff::DiffRow>) {
        match self.rows_by_id.get(call_id).copied() {
            Some(index) => {
                if let Some(Block::Call(row)) = self.blocks.get_mut(index) {
                    row.diff = Some(rows);
                    self.render_cache.borrow_mut().invalidate(index);
                }
            }
            None => {
                self.diffs.insert(call_id.to_owned(), rows);
            }
        }
    }

    fn tool_started(&mut self, call: &ToolCall) {
        self.add_call(call, true);
    }

    /// A call row: `started` when the tool ran (a live `ToolStarted`), not when
    /// it is only listed in a response being replayed.
    fn add_call(&mut self, call: &ToolCall, started: bool) {
        self.close_streams();
        self.announced.remove(&call.call_id);
        if let Some(&index) = self.rows_by_id.get(&call.call_id)
            && let Some(Block::Call(row)) = self.blocks.get_mut(index)
        {
            row.started |= started;
            self.running.insert(call.call_id.clone(), index);
            self.render_cache.borrow_mut().invalidate(index);
            return;
        }
        self.rows_by_id
            .insert(call.call_id.clone(), self.blocks.len());
        self.blocks.push(Block::Call(ToolRow {
            name: call.name.clone(),
            input: call.input.raw().to_owned(),
            summary: summarize_call(&call.name, call.input.raw()),
            status: RowStatus::Running,
            output: None,
            output_id: None,
            elapsed_ms: None,
            depth: 0,
            started,
            diff: self.diffs.remove(&call.call_id),
        }));
        self.running
            .insert(call.call_id.clone(), self.blocks.len() - 1);
    }

    fn tool_finished(&mut self, result: &ToolResultItem, elapsed_ms: Option<u64>) {
        if !self.running.contains_key(&result.call_id)
            && !self.rows_by_id.contains_key(&result.call_id)
            && !self.announced.contains_key(&result.call_id)
        {
            // Unannounced and unstarted: name the row after the result.
            self.announced.insert(
                result.call_id.clone(),
                ToolCall {
                    call_id: result.call_id.clone(),
                    name: result.name.clone(),
                    input: p1_contracts::ToolInput::Json(String::new()),
                },
            );
        }
        self.settle(
            &result.call_id,
            RowStatus::Settled(result.status),
            &result.content,
            elapsed_ms,
        );
    }

    /// Settle the row a call started, wherever the outcome came from (live
    /// event or journal replay).
    fn settle(&mut self, call_id: &str, status: RowStatus, content: &str, elapsed_ms: Option<u64>) {
        let index = match self.running.remove(call_id) {
            Some(index) => index,
            // A result for a call that never started (denied, unavailable,
            // cancelled before execution) or one reconciled after a resume: it
            // still gets its row — nothing the model asked for is invisible.
            None => match self.rows_by_id.get(call_id) {
                Some(&index) => {
                    self.render_cache.borrow_mut().invalidate(index);
                    index
                }
                None => {
                    self.close_streams();
                    let call = self.announced.remove(call_id);
                    let (name, input) = match &call {
                        Some(call) => (call.name.clone(), call.input.raw().to_owned()),
                        None => (String::new(), String::new()),
                    };
                    self.rows_by_id
                        .insert(call_id.to_owned(), self.blocks.len());
                    self.blocks.push(Block::Call(ToolRow {
                        summary: summarize_call(&name, &input),
                        name,
                        input,
                        status: RowStatus::Running,
                        output: None,
                        output_id: None,
                        elapsed_ms: None,
                        depth: 0,
                        started: false,
                        diff: None,
                    }));
                    self.blocks.len() - 1
                }
            },
        };
        self.render_cache.borrow_mut().invalidate(index);
        let Block::Call(row) = &mut self.blocks[index] else {
            return;
        };
        row.status = status;
        if elapsed_ms.is_some() {
            row.elapsed_ms = elapsed_ms;
        }
        // A denial's reason, or a tool that was never there, is not output:
        // nothing to open, copy or list.
        // Nor is the note of a call cancelled before it started.
        if matches!(
            status,
            RowStatus::Settled(ToolStatus::Denied | ToolStatus::Unavailable)
        ) || (status == RowStatus::Settled(ToolStatus::Cancelled) && !row.started)
        {
            row.output = (!content.is_empty()).then(|| content.to_string());
            return;
        }
        if !content.is_empty() {
            // The call id is journal-stable, so replay regenerates the same
            // short handle. Probe for collisions rather than silently losing output.
            let legacy = FoldId::of(content);
            let mut slot = call_id.bytes().fold(0x811c9dc5u32, |h, b| {
                (h ^ u32::from(b)).wrapping_mul(0x01000193)
            }) as usize
                & 0xffff;
            let id = match row.output_id.clone() {
                // A late result for a settled row keeps its handle.
                Some(id) => id,
                None => loop {
                    let id = FoldId(format!("h-{slot:04x}"));
                    if !self.outputs.contains_key(&id) {
                        break id;
                    }
                    slot += 1;
                },
            };
            let input: serde_json::Value = serde_json::from_str(&row.input).unwrap_or_default();
            let full = if matches!(status, RowStatus::Settled(ToolStatus::Ok))
                && row.name == "write"
            {
                input
                    .get("content")
                    .and_then(|v| v.as_str())
                    .unwrap_or(content)
                    .to_owned()
            } else if matches!(status, RowStatus::Settled(ToolStatus::Ok)) && row.name == "edit" {
                match (
                    input.get("old_string").and_then(|v| v.as_str()),
                    input.get("new_string").and_then(|v| v.as_str()),
                ) {
                    (Some(old), Some(new)) => old
                        .lines()
                        .map(|s| format!("- {s}"))
                        .chain(new.lines().map(|s| format!("+ {s}")))
                        .collect::<Vec<_>>()
                        .join("\n"),
                    _ => content.to_owned(),
                }
            } else {
                content.to_owned()
            };
            self.outputs.insert(legacy, content.to_owned());
            self.outputs.insert(id.clone(), full);
            row.output_id = Some(id.clone());
            row.output = Some(content.to_string());
            // `^O` and the pane's `newer` note mean an output the transcript
            // folds — one shown whole inline has nothing more to open.
            if crate::render::block::foldable(row) {
                self.latest_fold = Some(id);
            }
        }
    }

    /// `^R`: toggle the most recent reasoning block (SPEC §4.2).
    pub fn toggle_reasoning(&mut self) {
        for (index, block) in self.blocks.iter_mut().enumerate().rev() {
            if let Block::Reasoning { expanded, .. } = block {
                *expanded = !*expanded;
                self.render_cache.borrow_mut().invalidate(index);
                return;
            }
        }
    }

    /// Append ` · suffix` to the newest block when it is a note starting with
    /// `prefix` (one line per event). False when there is no such note.
    pub fn extend_note(&mut self, prefix: &str, suffix: &str) -> bool {
        let index = self.blocks.len().saturating_sub(1);
        match self.blocks.last_mut() {
            Some(Block::Meta { text }) if text.starts_with(prefix) => {
                text.push_str(" · ");
                text.push_str(suffix);
                self.render_cache.borrow_mut().invalidate(index);
                true
            }
            _ => false,
        }
    }

    /// Toggle one reasoning block (a click on its row).
    pub fn toggle_reasoning_at(&mut self, index: usize) {
        if let Some(Block::Reasoning { expanded, .. }) = self.blocks.get_mut(index) {
            *expanded = !*expanded;
            self.render_cache.borrow_mut().invalidate(index);
        }
    }

    /// A quiet meta line from the driver (`· …`), e.g. an unknown command.
    pub fn note(&mut self, text: &str) {
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
                                });
                            }
                            AssistantBlock::ToolCall(call) => self.tool_started(call),
                        }
                    }
                }
                Item::ToolResult(result) => self.settle(
                    &result.call_id,
                    RowStatus::Settled(result.status),
                    &result.content,
                    None,
                ),
            }
        }
        self.close_streams();
        self.settle_dangling(ToolStatus::Unknown, DANGLING);
    }

    /// Settle every row still running: a turn that ended (or a session that was
    /// left) cannot leave a working indicator behind.
    fn settle_dangling(&mut self, status: ToolStatus, content: &str) {
        let mut ids: Vec<(String, usize)> = self.running.drain().collect();
        ids.sort_by_key(|(_, index)| *index);
        for (id, index) in ids {
            self.running.insert(id.clone(), index);
            self.settle(&id, RowStatus::Settled(status), content, None);
        }
    }

    /// Paint a resumed session from its journal: what the operator saw, not
    /// the model-visible projection — partial replies with their cancel mark,
    /// provider failures, steering as operator input, history from before a
    /// compaction. Returns the usage records for the spend totals.
    pub fn replay(
        &mut self,
        records: &[p1_contracts::JournalRecord],
    ) -> Vec<Option<p1_contracts::Usage>> {
        use p1_contracts::{AssistantBlock, InboxKind, InterruptionReason, RecordBody};
        let mut usage = vec![];
        // A turn cancelled during a tool call or an approval leaves no record of
        // its own: a cancelled result that no response follows marks it.
        let mut cut = false;
        for record in records {
            let starts_turn = matches!(
                record.body,
                RecordBody::UserInput { .. } | RecordBody::Inbox { .. }
            );
            if cut && starts_turn {
                self.note("· cancelled");
            }
            if starts_turn || matches!(record.body, RecordBody::AssistantCompleted { .. }) {
                cut = false;
            }
            match &record.body {
                RecordBody::Environment { .. } => {}
                RecordBody::ToolStarted { call_id, .. } => {
                    if let Some(&index) = self.rows_by_id.get(call_id)
                        && let Some(Block::Call(row)) = self.blocks.get_mut(index)
                    {
                        row.started = true;
                    }
                }
                RecordBody::UserInput { text } => self.operator(text.clone()),
                RecordBody::Inbox { kind, text } => match kind {
                    InboxKind::Steering => self.operator(text.clone()),
                    InboxKind::Notification => self.note(&format!(
                        "· notification: {}",
                        text.lines().next().unwrap_or("")
                    )),
                },
                RecordBody::AssistantCompleted { item, usage: u, .. } => {
                    usage.push(*u);
                    for block in &item.blocks {
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
                                });
                            }
                            AssistantBlock::ToolCall(call) => self.add_call(call, false),
                        }
                    }
                }
                RecordBody::AssistantInterrupted {
                    reason,
                    partial_text,
                    error,
                } => {
                    // A cut response was billed but reported no usage: unknown.
                    if !partial_text.is_empty() {
                        usage.push(None);
                    }
                    if !partial_text.trim().is_empty() {
                        self.close_streams();
                        self.blocks.push(Block::Prose {
                            lines: partial_text.lines().map(str::to_string).collect(),
                        });
                    }
                    match (reason, error) {
                        (InterruptionReason::ProviderFailed, Some(error)) => {
                            self.blocks.push(Block::Notice {
                                lines: vec![format!("provider failed: {error}")],
                            })
                        }
                        (InterruptionReason::ProviderFailed, None) => {
                            self.blocks.push(Block::Notice {
                                lines: vec!["provider failed".into()],
                            })
                        }
                        (InterruptionReason::Cancelled, _) => self.note("· cancelled"),
                    }
                    self.settle_dangling(ToolStatus::Cancelled, "[cancelled]");
                }
                RecordBody::ToolFinished { result } => {
                    cut = result.status == ToolStatus::Cancelled
                        || (result.status == ToolStatus::Denied
                            && result.content == crate::runtime::CANCEL_DENY);
                    self.settle(
                        &result.call_id,
                        RowStatus::Settled(result.status),
                        &result.content,
                        None,
                    )
                }
                RecordBody::ContextReplaced { items, usage: u } => {
                    usage.push(*u);
                    self.note(&format!(
                        "· context summarized — the model now sees {} items",
                        items.len()
                    ));
                }
            }
        }
        if cut {
            self.note("· cancelled");
        }
        self.close_streams();
        self.settle_dangling(ToolStatus::Unknown, DANGLING);
        usage
    }

    fn turn_finished(&mut self, end: &TurnEnd) {
        self.close_streams();
        self.settle_dangling(
            if matches!(end, TurnEnd::Cancelled) {
                ToolStatus::Cancelled
            } else {
                ToolStatus::Unknown
            },
            "[cancelled]",
        );
        let lines = match end {
            TurnEnd::Completed { .. } | TurnEnd::Cancelled => None,
            TurnEnd::ProviderFailed { error } => Some(vec![format!("provider failed: {error}")]),
            TurnEnd::CommitFailed { message } => Some(vec![format!("commit failed: {message}")]),
            TurnEnd::ContextFailed { message } => Some(vec![format!("context failed: {message}")]),
        };
        if let Some(lines) = lines {
            self.blocks.push(Block::Notice { lines });
        }
    }

    fn close_streams(&mut self) {
        self.open_text = None;
        self.open_reasoning = None;
        self.reasoning_started_ms = None;
    }

    /// Whether a text or reasoning stream is open (a cancel then cut a reply).
    pub fn streaming(&self) -> bool {
        self.open_text.is_some() || self.open_reasoning.is_some()
    }
}

/// What a resumed call shows when the session ended before its result.
const DANGLING: &str = "No result was recorded before the session ended.";

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
pub fn summarize_call(name: &str, raw: &str) -> String {
    let key = match name {
        "shell" => "command",
        "edit" | "patch" | "write" | "read" => "file_path",
        "search" => "pattern",
        "finish" => "status",
        "worker_start" => "task",
        _ => "",
    };
    if let Some(value) = json_string_field(raw, key) {
        return summarize_input(value);
    }
    summarize_input(raw)
}

/// Extract a string field's value from flat JSON, for DISPLAY only: this is a
/// summary, not a parse — a miss or an escape edge case shows the raw input
/// instead. p1-tui carries no JSON dependency for a display hint.
fn json_string_field<'a>(raw: &'a str, key: &str) -> Option<&'a str> {
    if key.is_empty() {
        return None;
    }
    let needle = format!("\"{key}\"");
    let start = raw.find(&needle)? + needle.len();
    let rest = raw[start..].trim_start_matches([' ', ':']);
    let rest = rest.strip_prefix('"')?;
    // Find the closing quote, skipping \-escaped characters.
    let mut end = 0;
    let mut escaped = false;
    for (i, c) in rest.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        match c {
            '\\' => escaped = true,
            '"' => {
                end = i;
                break;
            }
            _ => {}
        }
    }
    if end == 0 {
        return None;
    }
    Some(&rest[..end])
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
        assert_eq!(
            summarize_call("worker_stop", r#"{"id":"w1"}"#),
            r#"{"id":"w1"}"#
        );
        assert_eq!(
            summarize_call("shell", r#"{"cmd":"ls"}"#),
            r#"{"cmd":"ls"}"#
        );
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
        t.apply(&call("c1", "shell", "cargo test"), None);
        t.apply(&call("c2", "read", "src/lib.rs"), None);
        t.apply(
            &result("c1", "shell", ToolStatus::Error, &big),
            Some(11_400),
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
            matches!(&t.blocks[..], [Block::Notice { lines }] if lines[0].contains("connection dropped"))
        );
    }
}
