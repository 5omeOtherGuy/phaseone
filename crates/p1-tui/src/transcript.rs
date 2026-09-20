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
    /// Assistant prose, unadorned. Streams open; a response boundary closes.
    Prose { lines: Vec<String>, open: bool },
    /// Reasoning collapses to `· reasoning Ns`; `^R` expands. While streaming,
    /// `open` holds the arriving lines.
    Reasoning {
        lines: Vec<String>,
        open: bool,
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
    /// The one-line argument summary (newlines as `␤`, bounded).
    pub summary: String,
    pub status: RowStatus,
    /// Full output exactly as the model saw it, when a result arrived. The
    /// fold decision is renderable from this plus the status.
    pub output: Option<String>,
    pub elapsed_ms: Option<u64>,
    /// Delegation indents ONCE and never more (SPEC §3).
    pub depth: u8,
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
    outputs: HashMap<FoldId, String>,
    /// The most recently registered fold — what `^O` opens.
    pub latest_fold: Option<FoldId>,
    /// Index of the block streaming text deltas land in, while it is open.
    open_text: Option<usize>,
    open_reasoning: Option<usize>,
    /// Call rows not yet settled, by provider call id, so a result settles the
    /// row its call started.
    running: HashMap<String, usize>,
}

impl Transcript {
    pub fn new() -> Self {
        Self::default()
    }

    /// The full output behind a fold handle (`^O` opens it in the pane).
    pub fn output(&self, id: &FoldId) -> Option<&str> {
        self.outputs.get(id).map(String::as_str)
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
                self.blocks.push(Block::Prose {
                    lines: Vec::new(),
                    open: true,
                });
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
                    open: true,
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

    fn tool_started(&mut self, call: &ToolCall) {
        self.close_streams();
        self.blocks.push(Block::Call(ToolRow {
            name: call.name.clone(),
            summary: summarize_input(call.input.raw()),
            status: RowStatus::Running,
            output: None,
            elapsed_ms: None,
            depth: 0,
        }));
        self.running
            .insert(call.call_id.clone(), self.blocks.len() - 1);
    }

    fn tool_finished(&mut self, result: &ToolResultItem, elapsed_ms: Option<u64>) {
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
        let Some(index) = self.running.remove(call_id) else {
            return;
        };
        let Block::Call(row) = &mut self.blocks[index] else {
            return;
        };
        row.status = status;
        row.elapsed_ms = elapsed_ms;
        if !content.is_empty() {
            if let Some(Fold::Folded { id, .. }) = row.fold_for(content) {
                self.latest_fold = Some(id.clone());
                self.outputs.insert(id, content.to_string());
            }
            row.output = Some(content.to_string());
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
                                    open: false,
                                });
                            }
                            AssistantBlock::Reasoning { text, .. } => {
                                self.close_streams();
                                self.blocks.push(Block::Reasoning {
                                    lines: text.lines().map(str::to_string).collect(),
                                    open: false,
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
    }

    fn turn_finished(&mut self, end: &TurnEnd) {
        self.close_streams();
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
        for index in [self.open_text.take(), self.open_reasoning.take()]
            .into_iter()
            .flatten()
        {
            match &mut self.blocks[index] {
                Block::Prose { open, .. } => *open = false,
                Block::Reasoning { open, .. } => *open = false,
                _ => {}
            }
        }
    }
}

impl ToolRow {
    fn fold_for(&self, output: &str) -> Option<Fold> {
        match Fold::present(output) {
            fold @ Fold::Folded { .. } => Some(fold),
            Fold::Full { .. } => None,
        }
    }
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

/// The one-line input summary for a call row: newlines become `␤`, at most
/// 100 characters (same rule as the host's line renderer).
pub fn summarize_input(raw: &str) -> String {
    raw.replace('\n', "␤").chars().take(100).collect()
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
    fn deltas_stream_into_one_open_prose_block() {
        let mut t = Transcript::new();
        t.apply(&AgentEvent::TextDelta { text: "hel".into() }, None);
        t.apply(
            &AgentEvent::TextDelta {
                text: "lo\nwor".into(),
            },
            None,
        );
        let [Block::Prose { lines, open }] = &t.blocks[..] else {
            panic!("one prose block");
        };
        assert!(open);
        assert_eq!(lines, &["hello", "wor"]);
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
        let [Block::Prose { open, .. }] = &t.blocks[..] else {
            panic!("one prose block");
        };
        assert!(!open);
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
