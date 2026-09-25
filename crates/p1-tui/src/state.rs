//! Screen state: everything the renderers read and the key handlers mutate.
//!
//! The right pane is a core feature, planned from M1 (owner direction): width
//! cycles `off / 40ch / 56ch / split` on `^W`, mode cycles on `^Tab`, events
//! PROMOTE a mode, and pinning always wins (SPEC §5). Focus mode (owner
//! direction, carried over from the iris TUI) folds passive chrome away so
//! the transcript owns the screen; the first edit reveals the composer again.

use std::collections::VecDeque;

use p1_contracts::{AgentEvent, Usage};

use crate::render::picker::Picker;
use crate::render::status::StatusGroup;
use crate::transcript::Transcript;

/// A pending approval: the blocking view that owns the screen until decided
/// (SPEC §4.4 diff review, §4.5 permission prompt).
#[derive(Debug, Clone, PartialEq)]
pub enum Approval {
    Diff(crate::render::diff::DiffView),
    Permission(crate::render::permission::PermissionView),
}

/// Pane width states, in `^W` cycle order (SPEC §5).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PaneWidth {
    Off,
    #[default]
    Ch40,
    Ch56,
    Split,
}

impl PaneWidth {
    pub fn cycle(self) -> Self {
        match self {
            Self::Off => Self::Ch40,
            Self::Ch40 => Self::Ch56,
            Self::Ch56 => Self::Split,
            Self::Split => Self::Off,
        }
    }

    /// The columns the pane occupies, at terminal width `cols`. `Split` takes
    /// half; `None` when the pane is off or the terminal is under the floor.
    pub fn columns(self, cols: usize) -> Option<usize> {
        if cols < PANE_FLOOR_COLS {
            return None;
        }
        match self {
            Self::Off => None,
            Self::Ch40 => Some(40),
            Self::Ch56 => Some(56),
            Self::Split => Some(cols / 2),
        }
    }
}

/// Below ~100 columns the pane collapses and the transcript takes full width
/// (SPEC §6). `^L` forces it back as an overlay at any width.
pub const PANE_FLOOR_COLS: usize = 100;

/// Pane modes, in `^Tab` cycle order (SPEC §5).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PaneMode {
    #[default]
    Ledger,
    Output,
    Diff,
    Workers,
}

impl PaneMode {
    pub fn cycle(self) -> Self {
        match self {
            Self::Ledger => Self::Output,
            Self::Output => Self::Diff,
            Self::Diff => Self::Workers,
            Self::Workers => Self::Ledger,
        }
    }
}

/// Why the pane is showing what it is showing (SPEC §5 promotion). Anything
/// unresolved leaves one counted line behind, never a badge.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum Promotion {
    /// No event is asking for attention.
    #[default]
    None,
    /// A two-line PEEK banner over the ledger, expires `until_ms`.
    Peek { lines: [String; 2], until_ms: u64 },
    // An approval is its own state (`Screen::approval`); a peek never
    // displaces it (checked there).
}

/// Running totals for the LEDGER spend section. Each part starts at a known
/// zero; the first unreported part makes THAT part unknown, forever — unknown
/// is never summed into a fake zero and never recovers mid-session.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Spend {
    pub input: Option<u64>,
    pub output: Option<u64>,
    pub cached: Option<u64>,
    pub cost_micro_usd: Option<u64>,
    /// Responses recorded. Zero responses is not a known zero spend — the
    /// ledger renders every part `—` until the first response lands.
    pub responses: u64,
}

impl Default for Spend {
    /// Nothing recorded yet is a KNOWN zero; `None` is reserved for poisoned.
    fn default() -> Self {
        Self {
            input: Some(0),
            output: Some(0),
            cached: Some(0),
            cost_micro_usd: Some(0),
            responses: 0,
        }
    }
}

impl Spend {
    pub fn record(&mut self, usage: Option<&Usage>) {
        self.responses += 1;
        let add = |slot: &mut Option<u64>, part: Option<u64>| {
            *slot = match (*slot, part) {
                (Some(total), Some(part)) => Some(total + part),
                _ => None,
            };
        };
        match usage {
            // A response with no usage at all poisons every part.
            None => {
                self.input = None;
                self.output = None;
                self.cached = None;
                self.cost_micro_usd = None;
            }
            Some(u) => {
                // The input total is known as soon as the uncached part is;
                // cache parts are ADDED where the route reports them.
                let input = u
                    .input_uncached
                    .map(|base| base + u.cache_read.unwrap_or(0) + u.cache_write.unwrap_or(0));
                add(&mut self.input, input);
                add(&mut self.output, u.output);
                add(&mut self.cached, u.cache_read);
                add(&mut self.cost_micro_usd, u.cost_micro_usd);
            }
        }
    }

    /// `hit%` of input served from cache, when both parts are known.
    pub fn cache_hit_percent(&self) -> Option<u64> {
        match (self.cached, self.input) {
            (Some(cached), Some(input)) if input > 0 => Some(cached * 100 / input),
            _ => None,
        }
    }
}

/// The live working state: what the LED chase is labelling.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Working {
    pub label: String,
    pub started_ms: u64,
}

pub use crate::editor::Composer;

/// The whole screen. Renderers borrow it; the driver owns it.
#[derive(Debug, Default)]
pub struct Screen {
    pub transcript: Transcript,
    pub repo: String,
    pub branch: String,
    pub model: String,
    pub legacy_keyboard: bool,
    pub effort: String,
    /// Actual caret position computed by the compositor.
    pub cursor_position: Option<(u16, u16)>,
    pub input_history: Vec<String>,
    pub history_position: Option<usize>,
    pub history_draft: String,
    /// The entry history browsing put in the composer.
    pub history_shown: Option<String>,
    /// Edits made to recalled entries while browsing, kept until a submit.
    #[doc(hidden)]
    pub history_edits: std::collections::HashMap<usize, String>,
    /// The stashed draft's caret, restored with it.
    #[doc(hidden)]
    pub history_draft_cursor: usize,
    /// The driver is starting or running a turn (Enter then queues steering,
    /// even before `TurnStarted` arrives).
    pub busy: bool,
    /// `^C` at idle right after a cancel asks for a second press to quit.
    pub quit_armed: bool,
    /// The driver's clock when the current input arrived (fake time in tests).
    pub now_ms: u64,
    /// When the approval on screen appeared, and when the operator last typed
    /// a printable key: a decision key counts only when deliberate.
    pub approval_shown_ms: u64,
    pub last_type_ms: Option<u64>,
    /// Further parked approvals behind the one on screen.
    pub approvals_waiting: usize,
    /// The slash palette's highlighted row, and whether Esc closed it for
    /// the current `/…` text.
    pub palette_selected: usize,
    pub palette_dismissed: bool,
    /// Keyboard selection in the transcript (Tab): the selected tool or
    /// reasoning block. Typing returns to the composer.
    pub selected: Option<usize>,
    pub output_filter: String,
    pub output_search: bool,
    /// Indices of the OUTPUT lines the filter keeps, recomputed only when the
    /// filter or the view changes (not every frame).
    pub output_matches: Option<Vec<usize>>,
    /// The pane mode and width before OUTPUT took the pane, restored on close.
    #[doc(hidden)]
    pub output_saved: Option<(PaneMode, PaneWidth)>,
    /// `/mouse`: the terminal owns the mouse (native selection); p1 gets no
    /// wheel or clicks until it is taken back.
    pub mouse_released: bool,
    /// The last `/find` and where it stands (newest match first).
    pub search: Option<Search>,
    /// The output a /find hit lies in, when its note says "^O opens it".
    pub find_output: Option<crate::fold::FoldId>,
    /// A transient notice shown in the hint row until the given time.
    pub flash: Option<(String, u64)>,
    /// Rows one wheel event scrolls (0 = the default): terminals that send
    /// several events per notch (Ghostty) set 1, so a notch is ~3 rows anywhere.
    pub wheel_step: isize,
    /// The terminal's width at the last frame (what the pane may widen into).
    #[doc(hidden)]
    pub frame_width: u16,
    /// A response is being streamed: a cancel then leaves its usage unknown.
    #[doc(hidden)]
    pub response_open: bool,
    pub transcript_width: usize,
    pub output_horizontal: usize,
    pub output_focus: bool,
    pub color_mode: crate::palette::ColorMode,
    pub composer: Composer,
    pub pane_width: PaneWidth,
    pub promotion_saved_width: Option<PaneWidth>,
    pub pane_mode: PaneMode,
    /// `^P`: no event may swap a pinned mode (SPEC §5).
    pub pinned: bool,
    pub promotion: Promotion,
    /// Focus mode override: `Some` after `/focus on|off`, `None` = automatic
    /// (on at terminal heights of 12 rows or fewer). SPEC §4.3a.
    pub focus_explicit: Option<bool>,
    /// The effective focus mode this frame; the driver sets it from the
    /// frame height and `focus_explicit`.
    pub focus: bool,
    pub working: Option<Working>,
    pub spend: Spend,
    /// The goal, host-owned and shown as a quotation (SPEC §4.7).
    pub goal: Option<String>,
    pub reduced_motion: bool,
    /// The §6 floor line's values (filled by the driver at startup).
    pub env: String,
    pub route: String,
    /// When anything last arrived for the turn: a long silent wait counts its
    /// seconds, so a provider retrying looks different from a frozen screen.
    #[doc(hidden)]
    pub phase_since_ms: u64,
    /// The side pane stays hidden until then (just after an approval).
    #[doc(hidden)]
    pub pane_hold_until: u64,
    /// `--ask`: tool calls wait for a decision (the statusline says `ask`).
    pub asking: bool,
    /// Forced ledger overlay at narrow widths (`^L`).
    pub ledger_overlay: bool,
    /// The OUTPUT pane's open fold, if any (SPEC §5 OUTPUT mode).
    pub output: Option<crate::render::output::OutputView>,
    /// The WORKERS pane's rows, refreshed by the driver from the host's
    /// worker service (p1-tui never names p1-workers).
    pub workers: Vec<crate::render::workers::WorkerRow>,
    /// A pending approval: the blocking, full-width review (SPEC §4.4/§4.5).
    /// While this is `Some` the pane is hidden and the transcript waits.
    pub approval: Option<Approval>,
    /// A docked picker overlay above the composer (SPEC §4.6).
    pub picker: Option<Picker>,
    /// The `/status` overlay (SPEC §4.6).
    pub status: Option<Vec<StatusGroup>>,
    /// Ledger context/task views, filled by the driver (the context breakdown
    /// arrives with the host's stats seam; until then these stay `None`).
    pub context_view: Option<crate::render::ledger::Context>,
    pub context_capacity: Option<(u64, u64)>,
    pub task_view: Option<crate::render::ledger::Task>,
    /// Steering/follow-up text queued for the next boundary, shown above the
    /// composer hints so the operator sees what will land.
    pub queued: VecDeque<Queued>,
    /// When each running call started (the event stamp), for the elapsed
    /// column. Cleared as results arrive. (Private: callers use `apply`.)
    /// Public only so struct-update tests can build a Screen literally.
    #[doc(hidden)]
    pub call_started: std::collections::HashMap<String, u64>,
    /// The transcript's pinned top row; `None` follows the live tail.
    /// New output never yanks a scrolled view back down.
    pub scroll_top: Option<usize>,
    /// The last rendered transcript size (rows, visible rows) — the scroll
    /// math needs it; the renderer records it each frame.
    #[doc(hidden)]
    pub last_rendered: (usize, usize),
    /// Where a detached view stands, as (block, row offset): re-resolved every
    /// frame, so a resize or a block changing height above it never moves the
    /// reader's content (`scroll_top` is only the current width's projection).
    #[doc(hidden)]
    pub scroll_anchor: Option<(usize, usize)>,
    /// The `scroll_top` the last frame wrote; anything else was set by a
    /// command (PageUp, /find) and wins over the anchor.
    #[doc(hidden)]
    pub scroll_written: Option<usize>,
    /// The anchored block's row count when the anchor was taken: a block that
    /// re-wrapped keeps the reader at the same proportion of it.
    #[doc(hidden)]
    pub scroll_anchor_extent: usize,
    /// The transcript width the anchor was taken at: only a re-wrap scales
    /// the offset; a block that expanded or folded keeps it.
    #[doc(hidden)]
    pub scroll_anchor_width: usize,
    /// A view whose top is inside the tail (an approval read from its
    /// header): its row offset into the tail, kept across relayouts.
    #[doc(hidden)]
    pub scroll_tail: Option<usize>,
    /// A new approval asks the next frame to show its header.
    pub approval_reveal: bool,
    /// A block toggled while following: the next frame follows again if its
    /// header is still on screen at the bottom.
    #[doc(hidden)]
    pub refollow: Option<usize>,
    /// First row of a tall overlay (/help at small heights) on screen.
    pub overlay_scroll: usize,
    /// Which overlay the last frame showed: another one starts at its top.
    #[doc(hidden)]
    pub overlay_kind: u8,
    /// The rows the open overlay has taken (it keeps them while open).
    #[doc(hidden)]
    pub overlay_height: usize,
    /// Where the last frame drew a docked overlay (the wheel scrolls it).
    pub overlay_area: ratatui::layout::Rect,
    /// An overlay the last frame had no room to draw (a 0-row transcript):
    /// the palette then does not take the keys.
    #[doc(hidden)]
    pub overlay_hidden: bool,
    /// The last frame drew the pending approval's decision row: only then do
    /// y/a/n decide it.
    #[doc(hidden)]
    pub approval_visible: bool,
    /// ...and every decision row (the `a session` chip may wrap below).
    #[doc(hidden)]
    pub approval_grant_visible: bool,
    /// The last frame showed the working row counting silent seconds (the
    /// driver then draws once a second to advance it).
    #[doc(hidden)]
    pub counting: bool,
    pub tool_hits: Vec<(ratatui::layout::Rect, crate::render::block::Hit)>,
    /// The last frame showed a working indicator (a running header or the
    /// working row): only then does the driver schedule animation frames.
    pub animating: bool,
    pub transcript_area: ratatui::layout::Rect,
    pub composer_area: ratatui::layout::Rect,
    /// The composer rows the last frame painted, and the layout row of the
    /// first: a click places the caret on the glyph under it.
    #[doc(hidden)]
    pub composer_rows: (Vec<crate::editor::Row>, usize),
    pub output_area: ratatui::layout::Rect,
    pub live_area: ratatui::layout::Rect,
}

/// A transcript search: the query, its hits (oldest first) and the one on
/// screen. Hits are (block, row offset), so layout changes never make them stale.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Search {
    pub query: String,
    pub hits: Vec<crate::render::block::SearchHit>,
    pub index: usize,
    /// The width the hits were counted at, and how many blocks were searched
    /// (a repeat searches only what came since).
    pub width: usize,
    pub scanned: usize,
}

/// What `^O` does with an OUTPUT pane shown.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CtrlO {
    Open(crate::fold::FoldId),
    Focus,
    Close,
}

/// One queued operator input (SPEC §4.2 hints: steering vs follow-up).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Queued {
    pub follow_up: bool,
    pub text: String,
}

impl Screen {
    pub fn new(reduced_motion: bool) -> Self {
        Self {
            reduced_motion,
            ..Self::default()
        }
    }

    pub fn cycle_width(&mut self) {
        self.promotion_saved_width = None;
        self.pane_width = self.pane_width.cycle();
    }

    pub fn cycle_mode(&mut self) {
        self.pane_mode = self.pane_mode.cycle();
        // A deliberate choice is not a pin, but it outlives a transient peek.
        if matches!(self.promotion, Promotion::Peek { .. }) {
            self.promotion = Promotion::None;
        }
    }

    pub fn toggle_pin(&mut self) {
        self.pinned = !self.pinned;
    }

    /// Observe one agent event: transcript first, then the state the event
    /// moves (working indicator, spend, promotion). `now_ms` times the call
    /// rows: a result's elapsed is measured from its start's stamp.
    pub fn apply(&mut self, event: &AgentEvent, now_ms: u64) {
        let phase_before = self.working.as_ref().map(|w| w.label.clone());
        self.apply_event(event, now_ms);
        // Anything arriving is progress: only silence counts its seconds.
        if self.working.as_ref().map(|w| &w.label) != phase_before.as_ref()
            || !matches!(event, AgentEvent::TurnFinished { .. })
        {
            self.phase_since_ms = now_ms;
        }
    }

    fn apply_event(&mut self, event: &AgentEvent, now_ms: u64) {
        let elapsed = match event {
            AgentEvent::ToolStarted { call } => {
                self.call_started.insert(call.call_id.clone(), now_ms);
                None
            }
            AgentEvent::ToolFinished { result } => self
                .call_started
                .remove(&result.call_id)
                .map(|started| now_ms.saturating_sub(started)),
            _ => None,
        };
        let cut = matches!(
            event,
            AgentEvent::TurnFinished {
                end: p1_contracts::TurnEnd::Cancelled
            }
        );
        self.transcript.apply_at(event, elapsed, Some(now_ms));
        let phase = |working: &mut Option<Working>, label: &str| {
            let working = working.get_or_insert_with(|| Working {
                label: String::new(),
                started_ms: now_ms,
            });
            if working.label != label {
                working.label = label.to_owned();
            }
        };
        match event {
            AgentEvent::TurnStarted => {
                self.working = Some(Working {
                    label: WAITING.into(),
                    started_ms: now_ms,
                });
            }
            AgentEvent::RequestStarted { .. } => phase(&mut self.working, WAITING),
            // A response counts as billed-but-unreported only once it streamed
            // something: a request that failed or was cancelled before its
            // first byte leaves spend known (the rule a resumed journal uses).
            AgentEvent::ReasoningDelta { .. } => {
                self.response_open = true;
                phase(&mut self.working, "thinking")
            }
            AgentEvent::TextDelta { .. } => {
                self.response_open = true;
                phase(&mut self.working, "writing")
            }
            AgentEvent::ToolInputDelta { .. } => self.response_open = true,
            AgentEvent::ToolStarted { call } => phase(&mut self.working, &call.name),
            AgentEvent::ToolFinished { result } => {
                // The working indicator clears only at TurnFinished: a turn
                // that streams after a tool call is still working (and ⏎
                // must keep meaning "queue steering").
                phase(&mut self.working, WAITING);
                // A PEEK is for a tool that FAILED; a call the operator denied
                // or cancelled is not a failure (SPEC §5).
                if matches!(
                    result.status,
                    p1_contracts::ToolStatus::Error | p1_contracts::ToolStatus::Unavailable
                ) {
                    self.peek(
                        [
                            format!("{} failed", result.name),
                            last_line(&result.content),
                        ],
                        now_ms,
                    );
                }
            }
            AgentEvent::ResponseCompleted { usage, .. } => {
                self.response_open = false;
                self.spend.record(usage.as_ref());
                if let Some((window, warn_at)) = self.context_capacity {
                    self.context_view = usage
                        .as_ref()
                        .and_then(|u| {
                            u.input_uncached
                                .map(|n| n + u.cache_read.unwrap_or(0) + u.cache_write.unwrap_or(0))
                        })
                        .map(|used| crate::render::ledger::Context {
                            used,
                            window,
                            warn_at,
                            parts: vec![],
                        });
                }
            }
            AgentEvent::TurnFinished { .. } => {
                // A cancelled turn is marked where it stopped, so a cut-off
                // sentence is never read as a finished answer.
                // A response cut mid-stream was billed without a report.
                if std::mem::take(&mut self.response_open) {
                    self.spend.record(None);
                }
                if cut {
                    let after = self
                        .working
                        .as_ref()
                        .map(|w| {
                            format!(
                                " after {}",
                                crate::render::elapsed(now_ms.saturating_sub(w.started_ms))
                            )
                        })
                        .unwrap_or_default();
                    self.transcript.note(&format!("· cancelled{after}"));
                }
                self.working = None;
            }
            _ => {}
        }
    }

    /// Queue operator input for the next boundary (SPEC §4.2).
    pub fn queue(&mut self, follow_up: bool, text: String) {
        self.queued.push_back(Queued { follow_up, text });
    }

    /// Route one mouse event. Returns whether anything visible changed, so the
    /// driver draws only for events that did something.
    /// Mouse routing follows Iris's pager: rendered headers toggle individually,
    /// wheel scrolling detaches follow until the bottom is reached again.
    pub fn on_mouse(&mut self, mouse: crossterm::event::MouseEvent) -> bool {
        use crate::render::block::Hit;
        use crossterm::event::{MouseButton, MouseEventKind};
        let point = ratatui::layout::Position::new(mouse.column, mouse.row);
        if let MouseEventKind::ScrollUp | MouseEventKind::ScrollDown = mouse.kind {
            let rows = self.wheel_rows(1);
            return self.wheel(
                mouse.column,
                mouse.row,
                mouse.kind == MouseEventKind::ScrollUp,
                rows,
            );
        }
        if self.picker.is_some() || self.status.is_some() || self.ledger_overlay {
            return false;
        }
        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                if self.live_area.contains(point) {
                    self.scroll_top = None;
                    return true;
                }
                if self.output_area.contains(point) {
                    // Only a pane that shows an output takes the keys.
                    if self.output.is_some() && self.pane_mode == PaneMode::Output {
                        self.output_focus = true;
                    }
                    return true;
                }
                if self.composer_area.contains(point) || self.transcript_area.contains(point) {
                    self.output_focus = false;
                }
                if self.composer_area.contains(point) {
                    self.selected = None;
                    let (rows, first) = &self.composer_rows;
                    let row = (point.y - self.composer_area.y) as usize;
                    if row >= *first && row - first < rows.len() {
                        let col = (point.x - self.composer_area.x).saturating_sub(4) as usize;
                        let rows = rows.clone();
                        self.composer.place(&rows, row - first, col);
                    }
                    return true;
                }
                let hit = self
                    .tool_hits
                    .iter()
                    .find(|(rect, _)| rect.contains(point))
                    .map(|(_, hit)| hit.clone());
                match hit {
                    Some(Hit::Header(index)) => self.toggle_disclosure(index),
                    Some(Hit::Reasoning(index)) => {
                        self.hold_view();
                        self.transcript.toggle_reasoning_at(index);
                    }
                    Some(Hit::Fold(id)) => {
                        self.open_fold(&id);
                    }
                    Some(Hit::Operator(index)) => {
                        self.hold_view();
                        match self.transcript.disclosures.get(&index) {
                            Some(true) => self.transcript.disclosures.remove(&index),
                            _ => self.transcript.disclosures.insert(index, true),
                        };
                        self.transcript.render_cache.borrow_mut().invalidate(index);
                    }
                    None => {}
                }
                true
            }
            _ => false,
        }
    }

    /// The reader is scrolled back and the newest block starts below the view:
    /// what streams into it cannot be seen (only the `↓ N` count changes).
    pub fn tail_offscreen(&self) -> bool {
        let Some(top) = self.scroll_top else {
            return false;
        };
        let last = self.transcript.blocks.len().saturating_sub(1);
        crate::render::block::block_start(&self.transcript, last)
            .is_some_and(|start| start > top + self.last_rendered.1)
    }

    /// Keep the rows on screen where they are while a block changes height:
    /// a following view pins its current top; the next frame re-follows if the
    /// change left nothing below.
    fn hold_view(&mut self) {
        if self.scroll_top.is_none() {
            self.scroll_top = Some(self.last_rendered.0.saturating_sub(self.last_rendered.1));
        }
    }

    /// Toggle one tool block between its preview and its other state (fully
    /// expanded when it folds, header-only when it fits). Clicks and keys share it.
    pub fn toggle_disclosure(&mut self, index: usize) {
        let Some(crate::transcript::Block::Call(row)) = self.transcript.blocks.get(index) else {
            return;
        };
        // A running call has nothing to show or hide yet.
        if row.status == crate::transcript::RowStatus::Running {
            return;
        }
        let next = crate::render::block::next_disclosure(
            row,
            self.transcript.disclosures.get(&index).copied(),
        );
        // Folding a block read from inside it (its sticky header): the view
        // lands on its header, not on whatever follows the shorter block.
        if let Some(top) = self.scroll_top
            && let Some((block, offset)) = crate::render::block::anchor_of(&self.transcript, top)
            && block == index
            && offset > usize::from(crate::render::block::separated(&self.transcript, index))
            && let Some(start) = crate::render::block::block_start(&self.transcript, index)
        {
            self.scroll_top = Some(start);
        }
        // A following view keeps following when the header stays in view.
        if self.scroll_top.is_none() {
            self.refollow = Some(index);
        }
        self.hold_view();
        match next {
            Some(state) => self.transcript.disclosures.insert(index, state),
            None => self.transcript.disclosures.remove(&index),
        };
        self.transcript.render_cache.borrow_mut().invalidate(index);
    }

    /// The commands the palette offers for the current draft (empty when the
    /// palette is closed).
    pub fn palette(&self) -> Vec<&'static crate::commands::Command> {
        // Not on a recalled entry as recalled: a `/command` from history must
        // not trap ↑↓. Once it is edited the palette helps again.
        if self.palette_dismissed
            || self.approval.is_some()
            || self.output_focus
            || (self.history_position.is_some()
                && self.history_shown.as_deref() == Some(self.composer.text.as_str()))
        {
            return vec![];
        }
        crate::commands::matches(&self.composer.text)
    }

    /// Keep the palette consistent with the draft: a draft that no longer
    /// starts with `/` reopens it for next time; the selection stays in range.
    pub fn sync_palette(&mut self) {
        if !self.composer.text.starts_with('/') {
            self.palette_dismissed = false;
            self.palette_selected = 0;
        }
        let n = crate::commands::matches(&self.composer.text).len();
        self.palette_selected = self.palette_selected.min(n.saturating_sub(1));
    }

    /// Complete the highlighted palette command into the draft; returns it.
    pub fn palette_complete(&mut self) -> Option<&'static crate::commands::Command> {
        let command = *self.palette().get(self.palette_selected)?;
        let text = match command.args {
            crate::commands::Args::None => format!("/{}", command.name),
            _ => format!("/{} ", command.name),
        };
        self.composer.set_text(text);
        Some(command)
    }

    /// Blocks keyboard selection steps over: tool calls and reasoning.
    fn selectable(&self, index: usize) -> bool {
        matches!(
            self.transcript.blocks.get(index),
            Some(crate::transcript::Block::Call(_) | crate::transcript::Block::Reasoning { .. })
        )
    }

    /// Tab: select the newest selectable block on screen (or overall).
    pub fn select_first(&mut self) -> bool {
        let top = self
            .scroll_top
            .unwrap_or(self.last_rendered.0.saturating_sub(self.last_rendered.1));
        let header = |hit: &crate::render::block::Hit| match hit {
            crate::render::block::Hit::Header(i) | crate::render::block::Hit::Reasoning(i) => {
                Some(*i)
            }
            _ => None,
        };
        // What the last frame showed, the sticky header of an expanded block
        // included; else the rows at the view's top.
        let on_screen = self
            .tool_hits
            .iter()
            .filter_map(|(rect, hit)| Some((rect.y, header(hit)?)))
            .max_by_key(|(y, _)| *y)
            .map(|(_, i)| i)
            .or_else(|| {
                crate::render::block::hits(&self.transcript, top, self.last_rendered.1)
                    .iter()
                    .rev()
                    .find_map(|(_, hit)| header(hit))
            });
        let newest = (0..self.transcript.blocks.len())
            .rev()
            .find(|i| self.selectable(*i));
        self.selected = on_screen.or(newest);
        self.reveal_selected();
        self.selected.is_some()
    }

    /// Up/Down in selection: the previous / next selectable block.
    pub fn select_step(&mut self, delta: isize) {
        let Some(current) = self.selected else {
            return;
        };
        let mut i = current as isize;
        loop {
            i += delta;
            if i < 0 || i as usize >= self.transcript.blocks.len() {
                return;
            }
            if self.selectable(i as usize) {
                self.selected = Some(i as usize);
                self.reveal_selected();
                return;
            }
        }
    }

    /// Scroll just enough to show the selected block's first row.
    fn reveal_selected(&mut self) {
        let Some(index) = self.selected else {
            return;
        };
        let width = self.transcript_width.max(20);
        let total = crate::render::block::measure(&self.transcript, width);
        let Some(row) = crate::render::block::block_start(&self.transcript, index) else {
            return;
        };
        let height = self.last_rendered.1.max(1);
        let max_top = self.last_rendered.0.max(total).saturating_sub(height);
        let top = self.scroll_top.unwrap_or(max_top);
        // Keep some of the block below its header in view, not the header
        // parked on the last row.
        let margin = (height / 3).clamp(1, 6);
        if row < top {
            self.scroll_top = Some(row.saturating_sub(1));
        } else if row + margin >= top + height {
            let next = (row + margin + 1).saturating_sub(height);
            self.scroll_top = (next < max_top).then_some(next);
        }
    }

    /// `^R`: the selected reasoning block, else the newest one on screen, else
    /// the newest of all (brought into view) — never an invisible change.
    pub fn toggle_reasoning(&mut self) {
        use crate::render::block::Hit;
        let is_reasoning = |s: &Self, i: usize| {
            matches!(
                s.transcript.blocks.get(i),
                Some(crate::transcript::Block::Reasoning { .. })
            )
        };
        let top = self
            .scroll_top
            .unwrap_or(self.last_rendered.0.saturating_sub(self.last_rendered.1));
        let target = self
            .selected
            .filter(|i| is_reasoning(self, *i))
            .or_else(|| {
                crate::render::block::hits(&self.transcript, top, self.last_rendered.1)
                    .iter()
                    .rev()
                    .find_map(|(_, h)| match h {
                        Hit::Reasoning(i) => Some(*i),
                        _ => None,
                    })
            });
        match target {
            Some(i) => {
                self.hold_view();
                self.transcript.toggle_reasoning_at(i);
            }
            None => {
                let newest = (0..self.transcript.blocks.len())
                    .rev()
                    .find(|i| is_reasoning(self, *i));
                if let Some(i) = newest {
                    self.transcript.toggle_reasoning_at(i);
                    let selected = self.selected.replace(i);
                    self.reveal_selected();
                    self.selected = selected;
                }
            }
        }
    }

    /// Enter on a selected block: toggle it, exactly as a click would.
    pub fn toggle_selected(&mut self) {
        match self
            .selected
            .and_then(|i| self.transcript.blocks.get(i).map(|b| (i, b)))
        {
            Some((i, crate::transcript::Block::Call(_))) => self.toggle_disclosure(i),
            Some((i, crate::transcript::Block::Reasoning { .. })) => {
                self.hold_view();
                self.transcript.toggle_reasoning_at(i);
            }
            _ => {}
        }
    }

    /// The selected block's output handle, if it has one.
    pub fn selected_output(&self) -> Option<crate::fold::FoldId> {
        match self.selected.and_then(|i| self.transcript.blocks.get(i)) {
            Some(crate::transcript::Block::Call(row)) => row.output_id.clone(),
            _ => None,
        }
    }

    /// Open a retained output by handle in the OUTPUT pane. False when the
    /// handle is unknown.
    pub fn open_fold(&mut self, id: &crate::fold::FoldId) -> bool {
        let Some(content) = self.transcript.output(id) else {
            return false;
        };
        // A pending approval is never covered: the pane waits for the answer.
        if self.approval.is_some() {
            self.flash_text("answer the approval first — y allow · n deny · ^C cancels");
            return true;
        }
        let mut lines: Vec<String> = content.lines().map(str::to_owned).collect();
        // The block states the exit; the pane shows the output itself.
        if lines
            .last()
            .is_some_and(|l| crate::render::block::is_trailer(l))
        {
            lines.pop();
        }
        let view = crate::render::output::OutputView {
            id: id.clone(),
            lines,
            scroll: 0,
        };
        self.open_output(view);
        true
    }

    /// What `^O` does while an OUTPUT pane is shown (`None` when none is): the
    /// fold row on screen when it is another output, else an output newer than
    /// the one shown, else focus the pane — or close it when focused. The
    /// driver acts on it and the pane's hint names it, so the two agree.
    pub fn pane_ctrl_o(&self) -> Option<CtrlO> {
        let shown = self
            .output
            .as_ref()
            .filter(|_| self.pane_mode == PaneMode::Output)?
            .id
            .clone();
        let fold_row = self.tool_hits.iter().rev().find_map(|(_, hit)| match hit {
            crate::render::block::Hit::Fold(id) => Some(id.clone()),
            _ => None,
        });
        let at = |id: &crate::fold::FoldId| {
            self.transcript.blocks.iter().position(|b| {
                matches!(b, crate::transcript::Block::Call(r) if r.output_id.as_ref() == Some(id))
            })
        };
        let target = match fold_row {
            Some(row) if row == shown => None,
            Some(row) => Some(row),
            None => self
                .transcript
                .latest_fold
                .clone()
                .filter(|latest| *latest != shown && at(latest) > at(&shown)),
        };
        Some(match target {
            Some(id) => CtrlO::Open(id),
            None if self.output_focus => CtrlO::Close,
            None => CtrlO::Focus,
        })
    }

    /// Open an output with the first line holding `needle` at the top (a /find
    /// hit inside it).
    pub fn open_fold_at(&mut self, id: &crate::fold::FoldId, needle: &str) -> bool {
        if !self.open_fold(id) {
            return false;
        }
        let needle = needle.to_lowercase();
        let at = self.output.as_ref().and_then(|view| {
            view.lines.iter().position(|l| {
                crate::render::block::clean(l)
                    .to_lowercase()
                    .contains(&needle)
            })
        });
        if let Some(at) = at.filter(|_| !needle.is_empty()) {
            self.scroll_output_by(at as isize);
            // A match past the right edge: pan it into view.
            let column = self.output.as_ref().and_then(|view| {
                let line = crate::render::block::clean(&view.lines[at]).to_lowercase();
                line.find(&needle)
                    .map(|byte| crate::wrap::cell_width(&line[..byte]))
            });
            let grid = (self.output_area.width as usize).saturating_sub(9).max(20);
            if let Some(column) = column.filter(|c| *c + needle.len() > grid) {
                self.output_horizontal = column.saturating_sub(8);
            }
        }
        true
    }

    /// Scroll the transcript `delta` rows up (positive) or down (negative).
    /// Scrolling to the newest rows releases the pin back to the live tail.
    /// A pending approval is part of the transcript, so it scrolls the same way.
    pub fn scroll_by(&mut self, delta: isize) {
        let (len, fits) = self.last_rendered;
        let max_top = len.saturating_sub(fits);
        let top = self.scroll_top.unwrap_or(max_top).min(max_top);
        let next = top.saturating_add_signed(-delta).min(max_top);
        // Scrolling down to within a row or two of the end re-follows: a
        // stream grows the transcript between frames, so the exact bottom is
        // a moving target.
        let slack = if delta < 0 { 2 } else { 0 };
        self.scroll_top = (next + slack < max_top).then_some(next);
    }

    /// Sync the WORKERS pane from a fresh snapshot, handling the promotion
    /// rule (SPEC §5): a live delegate promotes the pane to WORKERS while any
    /// worker runs; when none is live and nothing needs review, an UNPINNED
    /// WORKERS pane falls back to LEDGER.
    pub fn sync_workers(&mut self, rows: Vec<crate::render::workers::WorkerRow>) {
        let live = rows
            .iter()
            .any(|w| w.state == crate::render::workers::WorkerState::Running);
        self.workers = rows;
        if live {
            if !self.pinned && !self.output_focus && self.pane_mode != PaneMode::Workers {
                self.pane_mode = PaneMode::Workers;
            }
            if !self.pinned && !self.output_focus && matches!(self.pane_width, PaneWidth::Off) {
                self.promotion_saved_width = Some(self.pane_width);
                self.pane_width = PaneWidth::Ch56;
            }
        } else if self.pane_mode == PaneMode::Workers && !self.pinned {
            self.pane_mode = PaneMode::Ledger;
            if let Some(saved) = self.promotion_saved_width.take()
                && self.pane_width == PaneWidth::Ch56
            {
                self.pane_width = saved;
            }
        }
    }
    /// Open a fold handle in the OUTPUT pane (`^O`): switches the pane to
    /// OUTPUT mode and widens it if it is hidden.
    pub fn open_output(&mut self, view: crate::render::output::OutputView) {
        self.output = Some(view);
        self.output_focus = true;
        self.output_search = false;
        self.output_horizontal = 0;
        self.output_filter.clear();
        self.output_matches = None;
        if self.pane_mode != PaneMode::Output || self.output_saved.is_none() {
            self.output_saved
                .get_or_insert((self.pane_mode, self.pane_width));
        }
        self.pane_mode = PaneMode::Output;
        // OUTPUT reads at 56 columns (SPEC §5) when that fits beside the
        // transcript; the operator's width returns on close.
        let inner = (self.frame_width as usize).saturating_sub(4);
        if matches!(self.pane_width, PaneWidth::Off | PaneWidth::Ch40)
            && (self.frame_width == 0 || 56 + 50 <= inner)
        {
            self.pane_width = PaneWidth::Ch56;
        } else if self.pane_width == PaneWidth::Off && 40 + 50 <= inner {
            // A side pane where one fits, rather than a full-screen overlay.
            self.pane_width = PaneWidth::Ch40;
        }
    }

    /// Close the OUTPUT pane: the mode and width it replaced come back, unless
    /// the operator pinned the pane or changed its width meanwhile.
    pub fn close_output(&mut self) {
        self.output_focus = false;
        self.output_search = false;
        self.ledger_overlay = false;
        if let Some((mode, width)) = self.output_saved.take() {
            // The pane the output replaced comes back: its mode (never OUTPUT
            // itself — Esc closes it) and its width, Off included.
            if self.pane_mode == PaneMode::Output && !self.pinned {
                self.pane_mode = if mode == PaneMode::Output {
                    PaneMode::Ledger
                } else {
                    mode
                };
            }
            if !self.pinned {
                self.pane_width = width;
            }
        } else if self.pane_mode == PaneMode::Output && !self.pinned {
            self.pane_mode = PaneMode::Ledger;
        }
    }

    /// Recompute which OUTPUT lines the filter keeps (matching the text as it
    /// is shown: escapes stripped, tabs expanded) and go back to the first match.
    pub fn refilter_output(&mut self) {
        let needle = self.output_filter.to_lowercase();
        self.output_matches = match (&self.output, needle.is_empty()) {
            (Some(view), false) => Some(
                view.lines
                    .iter()
                    .enumerate()
                    .filter(|(_, l)| {
                        crate::render::block::clean(l)
                            .to_lowercase()
                            .contains(&needle)
                    })
                    .map(|(i, _)| i)
                    .collect(),
            ),
            _ => None,
        };
        if let Some(o) = &mut self.output {
            o.scroll = 0;
        }
    }

    /// Rows of output the pane shows at once (its page).
    pub fn output_page(&self) -> isize {
        (self.output_area.height as isize - 3).max(1)
    }

    /// Scroll the OUTPUT pane's content; the last page always stays full.
    pub fn scroll_output_by(&mut self, delta: isize) {
        let filtered = !self.output_filter.is_empty();
        let count = match (&self.output_matches, &self.output) {
            (Some(matches), _) if filtered => matches.len(),
            (_, Some(output)) => output.lines.len(),
            _ => return,
        };
        let page = (self.output_area.height as usize)
            .saturating_sub(2 + usize::from(filtered || self.output_search))
            .max(1);
        if let Some(output) = &mut self.output {
            output.scroll = output
                .scroll
                .saturating_add_signed(delta)
                .min(count.saturating_sub(page));
        }
    }

    /// Pan long output lines; stops once the longest line's end is in view.
    pub fn pan_output(&mut self, delta: isize) {
        // The `‹` cut mark takes a cell once panned.
        let grid = (self.output_area.width as usize).saturating_sub(9).max(1);
        let max = self
            .output
            .as_ref()
            .and_then(|o| {
                o.lines
                    .iter()
                    .map(|l| crate::wrap::cell_width(&crate::render::block::clean(l)))
                    .max()
            })
            .unwrap_or(0)
            .saturating_sub(grid);
        self.output_horizontal = self.output_horizontal.saturating_add_signed(delta).min(max);
    }

    /// Rows `events` wheel events scroll.
    pub fn wheel_rows(&self, events: usize) -> isize {
        let step = if self.wheel_step > 0 {
            self.wheel_step
        } else {
            WHEEL
        };
        step * events as isize
    }

    /// The wheel over (`column`, `row`): the pane under the pointer scrolls.
    pub fn wheel(&mut self, column: u16, row: u16, up: bool, rows: isize) -> bool {
        // An open picker or /help follows the wheel like its ↑↓ keys (without
        // wrapping round at the ends, as a scroll never does).
        if let Some(picker) = &mut self.picker {
            let before = picker.selected;
            picker.move_selection(if up { -1 } else { 1 });
            if (up && picker.selected > before) || (!up && picker.selected < before) {
                picker.selected = before;
            }
            return true;
        }
        let point = ratatui::layout::Position::new(column, row);
        let palette = self.palette().len();
        if palette > 0 && self.overlay_area.contains(point) {
            self.palette_selected = if up {
                self.palette_selected.saturating_sub(1)
            } else {
                (self.palette_selected + 1).min(palette - 1)
            };
            return true;
        }
        if self.status.is_some() {
            self.overlay_scroll = if up {
                self.overlay_scroll.saturating_sub(rows as usize)
            } else {
                self.overlay_scroll + rows as usize
            };
            return true;
        }
        if self.ledger_overlay {
            return false;
        }
        if self.output_area.contains(point) && self.approval.is_none() {
            self.scroll_output_by(if up { -rows } else { rows });
        } else {
            self.scroll_by(if up { rows } else { -rows });
        }
        true
    }

    /// The `/outputs` chooser: newest first, what produced each output.
    pub fn outputs_picker(&self) -> crate::render::picker::Picker {
        use crate::render::picker::{Picker, PickerGroup, PickerRow};
        let rows = self
            .transcript
            .blocks
            .iter()
            .rev()
            .filter_map(|block| match block {
                crate::transcript::Block::Call(row) => {
                    let id = row.output_id.clone()?;
                    let lines = self.transcript.output(&id).map_or(0, |o| {
                        o.lines()
                            .filter(|l| !crate::render::block::is_trailer(l))
                            .count()
                    });
                    // A failed call says so here too, as its block does.
                    let failed = if crate::render::block::call_failed(row) {
                        " · ✗"
                    } else {
                        ""
                    };
                    Some(PickerRow {
                        label: format!(
                            "{:<6} {}",
                            row.name,
                            crate::render::block::call_argument(row)
                        ),
                        value: format!(
                            "{id} · {}{failed}",
                            crate::render::block::plural(lines, "line")
                        ),
                        available: true,
                    })
                }
                _ => None,
            })
            .collect();
        Picker {
            groups: vec![PickerGroup {
                header: "OUTPUTS".into(),
                rows,
            }],
            filter: String::new(),
            selected: 0,
        }
    }

    /// `/find`: jump to the newest match; the same query again steps to older
    /// ones (wrapping). A match in collapsed reasoning opens it; a match only in
    /// a folded tool body lands on that block and says where the text is.
    /// False when nothing matches.
    pub fn find_next(&mut self, query: &str) -> bool {
        use crate::render::block;
        let width = self.transcript_width.max(20);
        self.find_output = None;
        // A repeat steps from the current match to the next older one; blocks
        // that arrived (or grew) since the last step are searched too, and
        // only those.
        let previous = self
            .search
            .take()
            .filter(|s| s.query.eq_ignore_ascii_case(query) && s.width == width);
        let current = previous
            .as_ref()
            .and_then(|s| s.hits.get(s.index).map(|h| (h.block, h.ordinal)));
        let hits = match previous {
            Some(mut search) => {
                let from = search.scanned.saturating_sub(1);
                // Blocks toggled or re-wrapped since are searched again too.
                let mut changed: Vec<usize> = search
                    .hits
                    .iter()
                    .filter(|h| {
                        h.block < from
                            && h.layout != block::layout_key(&self.transcript, h.block, width)
                    })
                    .map(|h| h.block)
                    .collect();
                changed.dedup();
                search
                    .hits
                    .retain(|h| h.block < from && !changed.contains(&h.block));
                for index in changed {
                    let at = search.hits.partition_point(|h| h.block < index);
                    let fresh = block::find_hits_in(&self.transcript, width, query, index);
                    search.hits.splice(at..at, fresh);
                }
                search
                    .hits
                    .extend(block::find_hits_from(&self.transcript, width, query, from));
                search.hits
            }
            None => block::find_hits(&self.transcript, width, query),
        };
        if hits.is_empty() {
            return false;
        }
        let index = match current
            .and_then(|current| hits.iter().position(|h| (h.block, h.ordinal) == current))
        {
            Some(at) => at.checked_sub(1).unwrap_or(hits.len() - 1),
            None => hits.len() - 1,
        };
        self.search = Some(Search {
            query: query.to_owned(),
            index,
            hits,
            width,
            scanned: self.transcript.blocks.len(),
        });
        let search = self.search.as_ref().unwrap();
        let (position, count) = (search.hits.len() - index, search.hits.len());
        let mut hit = search.hits[index];
        let mut note = String::new();
        if !hit.visible {
            let needle = query.to_lowercase();
            // Hidden text is shown where it can be: collapsed reasoning and a
            // folded long prompt open; a tool output says where the text is,
            // handle first (it survives a cut), and ^O opens it at the match.
            let reveal = match self.transcript.blocks.get(hit.block) {
                Some(crate::transcript::Block::Reasoning {
                    expanded: false, ..
                }) => {
                    self.transcript.toggle_reasoning_at(hit.block);
                    true
                }
                Some(crate::transcript::Block::Operator { .. }) => {
                    self.transcript.disclosures.insert(hit.block, true);
                    self.transcript
                        .render_cache
                        .borrow_mut()
                        .invalidate(hit.block);
                    true
                }
                Some(crate::transcript::Block::Call(row)) => {
                    let in_output = row
                        .output
                        .as_deref()
                        .is_some_and(|o| o.to_lowercase().contains(&needle));
                    let disclosure = self.transcript.disclosures.get(&hit.block).copied();
                    note = match (&row.output_id, in_output) {
                        (Some(id), true) => {
                            self.find_output = Some(id.clone());
                            let where_ = match disclosure {
                                Some(false) => "hidden",
                                Some(true) => "past the edge",
                                None if !block::foldable(row) => "past the edge",
                                None => "folded",
                            };
                            format!(" · {id} {where_} · ^O opens it")
                        }
                        _ if block::call_input_contains(row, &needle) => {
                            " · in the call's input".into()
                        }
                        _ => String::new(),
                    };
                    false
                }
                // Text in one source line that no row shows was cut at the
                // right edge (a code line); otherwise it runs across rows.
                Some(
                    crate::transcript::Block::Prose { lines }
                    | crate::transcript::Block::Notice { lines },
                ) if lines.iter().any(|l| l.to_lowercase().contains(&needle)) => {
                    note = " · past the right edge".into();
                    false
                }
                _ => {
                    note = " · across a line break".into();
                    false
                }
            };
            if reveal {
                block::resolve_hit(&self.transcript, &mut hit, width, query);
                let search = self.search.as_mut().unwrap();
                search.hits[index] = hit;
            }
        }
        block::measure(&self.transcript, width);
        let row = block::block_start(&self.transcript, hit.block).unwrap_or(0) + hit.offset;
        // The match sits a third of the way down, with context above it.
        self.scroll_top = Some(row.saturating_sub(self.last_rendered.1 / 3));
        self.flash_text(format!("{position}/{count}{note} · find \"{query}\""));
        true
    }

    /// Re-resolve the current match's row if its block changed layout since
    /// (the renderer calls this before painting the highlight).
    pub fn refresh_search(&mut self) {
        let width = self.transcript_width.max(20);
        if let Some(search) = &mut self.search
            && let Some(hit) = search.hits.get_mut(search.index)
        {
            crate::render::block::resolve_hit(&self.transcript, hit, width, &search.query);
        }
    }

    pub fn search_row(&self) -> Option<usize> {
        let search = self.search.as_ref()?;
        let hit = search.hits.get(search.index).filter(|h| h.visible)?;
        Some(crate::render::block::block_start(&self.transcript, hit.block)? + hit.offset)
    }

    /// A transient notice in the hint row (local confirmations, feedback): it
    /// fades after a few seconds and never enters the transcript.
    pub fn flash_text(&mut self, text: impl Into<String>) {
        self.flash = Some((text.into(), self.now_ms + FLASH_MS));
    }

    /// A PEEK is a two-line banner that never moves the ledger and never
    /// displaces a pin or an operator-blocked state (SPEC §5).
    pub fn peek(&mut self, lines: [String; 2], now_ms: u64) {
        if self.pinned || self.approval.is_some() {
            return;
        }
        self.promotion = Promotion::Peek {
            lines,
            until_ms: now_ms + PEEK_MS,
        };
    }

    /// The LEDGER view, built from screen state. The context breakdown stays
    /// `None` until the host's context-stats seam lands (issue #12 plan).
    pub fn ledger(&self) -> crate::render::ledger::Ledger {
        crate::render::ledger::Ledger {
            goal: self.goal.clone(),
            context: self.context_view.clone(),
            window: self.context_capacity.map(|(window, _)| window),
            task: self.task_view.clone(),
            spend: crate::render::ledger::SpendView {
                responses: self.spend.responses,
                input: self.spend.input,
                output: self.spend.output,
                cache_hit_percent: self.spend.cache_hit_percent(),
                cost_micro_usd: self.spend.cost_micro_usd,
            },
        }
    }

    /// Expire a peek whose time has passed. Driven by the render tick's clock.
    pub fn tick(&mut self, now_ms: u64) {
        if self
            .flash
            .as_ref()
            .is_some_and(|(_, until)| now_ms >= *until)
        {
            self.flash = None;
        }
        if let Promotion::Peek { until_ms, .. } = self.promotion
            && now_ms >= until_ms
        {
            self.promotion = Promotion::None;
        }
    }
}

/// The peek banner's lifetime (SPEC §5: 3 s).
pub const PEEK_MS: u64 = 3_000;

/// How long a flash notice stays in the hint row.
pub const FLASH_MS: u64 = 4_000;

/// Rows one wheel event scrolls.
pub const WHEEL: isize = 3;

/// The working row's label while the model is being asked.
pub(crate) const WAITING: &str = "waiting for the model";

/// The detail line of a failure peek: the last non-empty line, where errors
/// and verdicts usually are.
fn last_line(text: &str) -> String {
    text.lines()
        .rev()
        .find(|l| !l.trim().is_empty() && !l.starts_with("[exit code"))
        .unwrap_or("")
        .chars()
        .take(60)
        .collect()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EditAction {
    Home,
    End,
    Delete,
    ClearLine,
    KillEnd,
    DeleteWord,
    DeleteWordRight,
    WordLeft,
    WordRight,
    Up,
    Down,
}

impl Screen {
    /// Record a submitted prompt: history, browsing reset, back to the live tail.
    pub fn remember_input(&mut self, text: &str) {
        self.remember(text);
        self.scroll_top = None;
    }

    /// Put `text` into prompt history (steering and follow-ups too) without
    /// moving the transcript.
    pub fn remember(&mut self, text: &str) {
        if !text.trim().is_empty() && self.input_history.last().is_none_or(|s| s != text) {
            self.input_history.push(text.to_owned());
        }
        self.history_position = None;
        self.history_shown = None;
        self.history_draft.clear();
        self.history_edits.clear();
    }

    /// `^C` with a draft: clear it, recoverably (Up brings it back). While
    /// browsing history it first leaves the recalled entry for the draft that
    /// was stashed when browsing began — that draft is never discarded.
    pub fn clear_draft(&mut self) {
        if self.history_position.is_some() {
            // An edit made to the recalled entry is not lost: it goes to history.
            let edited = (self.history_shown.as_deref() != Some(self.composer.text.as_str())
                && !self.composer.text.trim().is_empty())
            .then(|| self.composer.text.clone());
            let mut edits: Vec<(usize, String)> = self.history_edits.drain().collect();
            edits.sort();
            for (_, text) in edits {
                if Some(&text) != edited.as_ref() && !text.trim().is_empty() {
                    self.input_history.push(text);
                }
            }
            if let Some(edited) = edited {
                self.input_history.push(edited);
            }
            let draft = std::mem::take(&mut self.history_draft);
            self.composer.set_text(draft);
            self.composer.cursor = self
                .history_draft_cursor
                .min(self.composer.text.chars().count());
            self.history_position = None;
            self.history_shown = None;
            self.history_edits.clear();
            return;
        }
        let text = self.composer.take();
        self.remember(&text);
    }

    pub fn edit(&mut self, action: EditAction) {
        use EditAction::*;
        let c = &mut self.composer;
        match action {
            Home => c.home(),
            End => c.end(),
            Delete => c.delete(),
            ClearLine => c.clear_line(),
            KillEnd => c.kill_end(),
            DeleteWord => c.delete_word(),
            DeleteWordRight => c.delete_word_right(),
            WordLeft => c.word_left(),
            WordRight => c.word_right(),
            Up | Down => {
                let dir = if action == Up { -1 } else { 1 };
                // Inside a recalled entry rows move the caret; at its first/last
                // row the next entry follows (Down past the newest restores the draft).
                if !c.move_row(dir) {
                    self.history_step(dir);
                }
                return;
            }
        }
        self.composer.revealed = self.composer.revealed || !self.composer.text.is_empty();
    }

    /// Readline-style history: the draft is stashed once when browsing starts
    /// (with its caret) and comes back past the newest entry; an edited entry
    /// keeps its edit while browsing continues. Nothing typed is ever lost.
    fn history_step(&mut self, dir: isize) {
        let pos = match self.history_position {
            Some(pos) => {
                let text = self.composer.text.clone();
                if self.input_history.get(pos) == Some(&text) {
                    self.history_edits.remove(&pos);
                } else {
                    self.history_edits.insert(pos, text);
                }
                pos
            }
            None if dir < 0 && !self.input_history.is_empty() => {
                self.history_draft = self.composer.text.clone();
                self.history_draft_cursor = self.composer.cursor;
                self.input_history.len()
            }
            None => return,
        };
        if dir < 0 {
            if pos > 0 {
                self.show_history(pos - 1);
            }
        } else if pos + 1 >= self.input_history.len() {
            let draft = std::mem::take(&mut self.history_draft);
            self.composer.set_text(draft);
            self.composer.cursor = self
                .history_draft_cursor
                .min(self.composer.text.chars().count());
            self.history_position = None;
            self.history_shown = None;
        } else {
            self.show_history(pos + 1);
        }
    }

    fn show_history(&mut self, index: usize) {
        let text = self
            .history_edits
            .get(&index)
            .cloned()
            .unwrap_or_else(|| self.input_history[index].clone());
        self.composer.set_text(text.clone());
        self.history_position = Some(index);
        self.history_shown = Some(text);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use p1_contracts::{ToolCall, ToolInput, ToolResultItem, ToolStatus};

    #[test]
    fn width_cycles_through_the_four_states() {
        let mut w = PaneWidth::Off;
        let order: Vec<PaneWidth> = (0..5)
            .map(|_| {
                w = w.cycle();
                w
            })
            .collect();
        assert_eq!(
            order,
            [
                PaneWidth::Ch40,
                PaneWidth::Ch56,
                PaneWidth::Split,
                PaneWidth::Off,
                PaneWidth::Ch40
            ]
        );
    }

    #[test]
    fn the_pane_collapses_under_the_floor() {
        assert_eq!(PaneWidth::Ch40.columns(120), Some(40));
        assert_eq!(PaneWidth::Ch40.columns(80), None);
        assert_eq!(PaneWidth::Split.columns(120), Some(60));
        assert_eq!(PaneWidth::Off.columns(120), None);
    }

    #[test]
    fn a_peek_expires_and_never_displaces_a_block() {
        let mut s = Screen::new(false);
        s.peek(["a".into(), "b".into()], 1_000);
        assert!(matches!(s.promotion, Promotion::Peek { .. }));
        s.tick(3_999);
        assert!(matches!(s.promotion, Promotion::Peek { .. }));
        s.tick(4_000);
        assert_eq!(s.promotion, Promotion::None);

        // An approval on screen blocks peeks exactly like a pin.
        s.approval = Some(crate::state::Approval::Permission(
            crate::render::permission::PermissionView {
                tool: "shell".into(),
                command: "rm -rf /".into(),
                rows: vec![],
                grantable: false,
            },
        ));
        s.peek(["a".into(), "b".into()], 5_000);
        assert_eq!(s.promotion, Promotion::None);
        s.promotion = Promotion::None;
        s.pinned = true;
        s.peek(["a".into(), "b".into()], 5_000);
        assert_eq!(s.promotion, Promotion::None);
    }

    #[test]
    fn a_failed_tool_peeks_an_ok_tool_does_not() {
        let mut s = Screen::new(false);
        let started = AgentEvent::ToolStarted {
            call: ToolCall {
                call_id: "c1".into(),
                name: "shell".into(),
                input: ToolInput::Json("{}".into()),
            },
        };
        s.apply(&started, 100);
        assert_eq!(s.working.as_ref().map(|w| w.label.as_str()), Some("shell"));
        s.apply(
            &AgentEvent::ToolFinished {
                result: ToolResultItem {
                    call_id: "c1".into(),
                    name: "shell".into(),
                    status: ToolStatus::Error,
                    content: "boom".into(),
                },
            },
            200,
        );
        assert!(matches!(s.promotion, Promotion::Peek { .. }));
        // Working clears only at TurnFinished, not at the tool's end.
        assert!(s.working.is_some());
    }

    #[test]
    fn working_clears_at_turn_finished_not_at_tool_finished() {
        let mut s = Screen::new(false);
        s.apply(&AgentEvent::TurnStarted, 0);
        assert!(s.working.is_some());
        s.apply(
            &AgentEvent::TurnFinished {
                end: p1_contracts::TurnEnd::Completed {
                    stop: p1_contracts::StopReason::EndTurn,
                },
            },
            100,
        );
        assert!(s.working.is_none());
    }

    #[test]
    fn spend_sums_known_and_never_fakes_unknown() {
        let mut spend = Spend::default();
        spend.record(Some(&Usage {
            input_uncached: Some(100),
            cache_read: Some(300),
            output: Some(10),
            ..Usage::default()
        }));
        assert_eq!(spend.input, Some(400));
        assert_eq!(spend.cache_hit_percent(), Some(75));
        assert_eq!(spend.cost_micro_usd, None);
        // One response with no usage at all poisons every part to unknown.
        spend.record(None);
        assert_eq!(spend.input, None);
        assert_eq!(spend.output, None);
        // A route that never reports cost leaves cost unknown while the
        // reported parts keep summing.
        let mut spend = Spend::default();
        spend.record(Some(&Usage {
            input_uncached: Some(10),
            ..Usage::default()
        }));
        spend.record(Some(&Usage {
            input_uncached: Some(5),
            ..Usage::default()
        }));
        assert_eq!(spend.input, Some(15));
        assert_eq!(spend.cost_micro_usd, None);
    }
}
