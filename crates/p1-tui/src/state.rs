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

/// The composer: the operator's multiline input. In focus mode it hides while
/// empty; the first edit reveals it (input drives disclosure).
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Composer {
    pub text: String,
    /// Cursor as a CHAR index into `text`.
    pub cursor: usize,
    /// Revealed while focus mode would hide it (an edit, a paste, `^G`).
    pub revealed: bool,
    /// While `^G` edits the goal: the text the composer held before, which `esc` restores.
    pub goal_edit: Option<String>,
}

impl Composer {
    /// `^G`: put `/goal <current goal>` in the composer, cursor at the end (handoff §8.4).
    pub fn begin_goal_edit(&mut self, goal: Option<&str>) {
        if self.goal_edit.is_none() {
            self.goal_edit = Some(std::mem::take(&mut self.text));
        }
        self.text = format!("/goal {}", goal.unwrap_or_default());
        self.cursor = self.text.chars().count();
        self.revealed = true;
    }

    /// `esc` during a goal edit: the previous composer text comes back.
    pub fn keep(&mut self) {
        if let Some(previous) = self.goal_edit.take() {
            self.cursor = previous.chars().count();
            self.revealed = !previous.is_empty();
            self.text = previous;
        }
    }

    pub fn editing_goal(&self) -> bool {
        self.goal_edit.is_some()
    }

    pub fn insert(&mut self, ch: char) {
        let byte = self.byte_index();
        self.text.insert(byte, ch);
        self.cursor += 1;
        self.revealed = true;
    }

    pub fn backspace(&mut self) {
        if self.cursor == 0 {
            return;
        }
        let byte = self.byte_index();
        let prev = self.text[..byte].chars().last().unwrap();
        self.text.replace_range(byte - prev.len_utf8()..byte, "");
        self.cursor -= 1;
        // Clearing the composer hides it again in focus mode (handoff §8.5).
        if self.text.is_empty() {
            self.revealed = false;
        }
    }

    pub fn left(&mut self) {
        self.cursor = self.cursor.saturating_sub(1);
    }

    pub fn right(&mut self) {
        if self.cursor < self.text.chars().count() {
            self.cursor += 1;
        }
    }

    pub fn take(&mut self) -> String {
        self.cursor = 0;
        self.revealed = false;
        self.goal_edit = None;
        std::mem::take(&mut self.text)
    }

    /// Whether the composer occupies rows right now: always outside focus
    /// mode; inside, only once revealed or while it holds text.
    pub fn visible(&self, focus: bool) -> bool {
        !focus || self.revealed || !self.text.is_empty()
    }

    fn byte_index(&self) -> usize {
        self.text
            .char_indices()
            .nth(self.cursor)
            .map(|(i, _)| i)
            .unwrap_or(self.text.len())
    }
}

/// The whole screen. Renderers borrow it; the driver owns it.
#[derive(Debug, Default)]
pub struct Screen {
    pub transcript: Transcript,
    pub composer: Composer,
    pub statusbar: crate::render::statusbar::StatusBar,
    pub pane_width: PaneWidth,
    pub pane_mode: PaneMode,
    /// The width the operator held before a live worker forced the pane open
    /// (`Off` → `Ch56`); the demotion gives it back. `None` when no such
    /// promotion is outstanding, or when a pin or an operator change owns the
    /// width instead.
    pub promotion_saved_width: Option<PaneWidth>,
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
    /// `^F`: the pane owns `↑ ↓` (WORKERS select, OUTPUT scroll) until `esc` or `^F`.
    pub pane_focused: bool,
    /// The full diff review's view state (handoff §7.5).
    pub review: FullReview,
}

/// The full diff review: shown over a diff approval while `open`. Paging files and scrolling
/// are view-only; the decision is about the whole call.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct FullReview {
    pub open: bool,
    /// The file on screen, an index into the call's files.
    pub file: usize,
    /// The first diff row shown.
    pub scroll: usize,
    /// Diff body rows of the last frame (`review::body_rows`), recorded by the screen like
    /// `last_rendered`, so paging moves by what is visible.
    pub body_rows: usize,
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
        self.transcript.apply(event, elapsed);
        match event {
            AgentEvent::TurnStarted => {
                self.working = Some(Working {
                    label: String::new(),
                    started_ms: now_ms,
                });
            }
            AgentEvent::ToolStarted { call } => {
                let working = self.working.get_or_insert_with(|| Working {
                    label: String::new(),
                    started_ms: now_ms,
                });
                working.label.clone_from(&call.name);
            }
            AgentEvent::ToolFinished { result } => {
                // The working indicator clears only at TurnFinished: a turn
                // that streams after a tool call is still working (and ⏎
                // must keep meaning "queue steering").
                if !matches!(result.status, p1_contracts::ToolStatus::Ok) {
                    self.peek(
                        [
                            format!("{} failed", result.name),
                            first_line(&result.content),
                        ],
                        now_ms,
                    );
                }
            }
            AgentEvent::ResponseCompleted { usage, .. } => {
                self.spend.record(usage.as_ref());
            }
            AgentEvent::TurnFinished { .. } => {
                self.working = None;
            }
            _ => {}
        }
    }

    /// Queue operator input for the next boundary (SPEC §4.2).
    pub fn queue(&mut self, follow_up: bool, text: String) {
        self.queued.push_back(Queued { follow_up, text });
    }

    /// `PgUp` (`pages` > 0) / `PgDn`: scroll by the transcript rows minus two, so two rows of
    /// context stay on screen (handoff §8.3). Paging past the end returns to the live tail.
    pub fn page(&mut self, pages: isize) {
        let step = self.last_rendered.1.saturating_sub(2).max(1) as isize;
        self.scroll_by(pages * step);
    }

    /// `esc` while scrolled back: follow the newest rows again.
    pub fn live_tail(&mut self) {
        self.scroll_top = None;
    }

    /// The scroll mark while the view is pinned above the live tail. It takes the view's last
    /// row, so the rows below are counted from one row higher.
    pub fn scroll_mark(&self) -> Option<crate::render::scroll::ScrollMark> {
        let top = self.scroll_top?;
        let (total, fits) = self.last_rendered;
        let shown = fits.saturating_sub(1);
        // The working label names the latest started tool; it runs while any call is open.
        let running = self
            .working
            .as_ref()
            .filter(|w| !self.call_started.is_empty() && !w.label.is_empty())
            .map(|w| format!("{} running", w.label));
        Some(crate::render::scroll::ScrollMark {
            below: total.saturating_sub(top + shown),
            running,
            row: top + 1,
            total,
        })
    }

    /// `/` in an empty composer: type it and open command completion over the composer text.
    pub fn open_completion(&mut self) {
        self.composer.insert('/');
        let mut menu = Picker::commands();
        let model = self.statusbar.model.clone().unwrap_or_else(|| "—".into());
        let effort = self
            .statusbar
            .effort
            .clone()
            .unwrap_or_else(|| "default".into());
        menu.set_value("/model", format!("{model}:{effort}"));
        menu.set_value("/effort", effort);
        menu.set_value("/focus", if self.focus { "on" } else { "off" });
        menu.filter = self.composer.text.clone();
        menu.select_first();
        self.picker = Some(menu);
    }

    /// A key typed while a menu is open: completion edits the composer and filters by its
    /// text; any other menu filters by what is typed into it.
    pub fn menu_input(&mut self, ch: Option<char>) {
        let Some(menu) = &mut self.picker else {
            return;
        };
        if menu.completion {
            match ch {
                Some(ch) => self.composer.insert(ch),
                None => self.composer.backspace(),
            }
            if self.composer.text.is_empty() {
                self.picker = None;
                return;
            }
            menu.filter = self.composer.text.clone();
        } else {
            match ch {
                Some(ch) => menu.filter.push(ch),
                None => {
                    menu.filter.pop();
                }
            }
        }
        menu.select_first();
    }

    /// `tab`: complete the composer to the focused command and close the menu.
    pub fn complete_menu(&mut self) {
        if let Some(label) = self.picker.as_ref().and_then(Picker::completion) {
            self.composer.text = format!("{label} ");
            self.composer.cursor = self.composer.text.chars().count();
            self.composer.revealed = true;
            self.picker = None;
        }
    }

    /// `^G`: edit the goal in the composer, prefilled with the current goal.
    pub fn edit_goal(&mut self) {
        self.composer.begin_goal_edit(self.goal.as_deref());
    }

    /// Files in the call under review. An approval carries one prepared diff today.
    fn review_files(&self) -> usize {
        usize::from(matches!(self.approval, Some(Approval::Diff(_))))
    }

    /// `^D`: open or close the full review of the diff on screen.
    pub fn toggle_review(&mut self) {
        if self.review_files() == 0 {
            return;
        }
        self.review = FullReview {
            open: !self.review.open,
            ..FullReview::default()
        };
    }

    /// `tab` / `⇧tab` in the full review: the next / previous file, wrapping.
    pub fn review_file(&mut self, delta: isize) {
        let files = self.review_files();
        if files == 0 {
            return;
        }
        let next = (self.review.file as isize + delta).rem_euclid(files as isize);
        self.review.file = next as usize;
        self.review.scroll = 0;
    }

    /// `PgUp` (`pages` > 0) / `PgDn` in the full review: page the diff body, clamped to it.
    pub fn review_page(&mut self, pages: isize) {
        let Some(Approval::Diff(view)) = &self.approval else {
            return;
        };
        let body = self.review.body_rows.max(1);
        let step = body.saturating_sub(2).max(1) as isize;
        let last = view.rows.len().saturating_sub(body);
        self.review.scroll = self
            .review
            .scroll
            .saturating_add_signed(-pages * step)
            .min(last);
    }

    /// Perform a view-only key decision (`input::decide`): nothing here reaches the agent.
    pub fn apply_view(&mut self, command: crate::input::ViewCommand) {
        use crate::input::ViewCommand as V;
        match command {
            V::PageUp => self.page(1),
            V::PageDown => self.page(-1),
            V::LiveTail => self.live_tail(),
            V::OpenCompletion => self.open_completion(),
            V::MenuInput(ch) => self.menu_input(Some(ch)),
            V::MenuBackspace => self.menu_input(None),
            V::Complete => self.complete_menu(),
            V::Effort(delta) => {
                if let Some(menu) = &mut self.picker {
                    menu.step_effort(delta);
                }
            }
            V::TogglePaneFocus => self.pane_focused = !self.pane_focused,
            V::EditGoal => self.edit_goal(),
            V::KeepComposer => self.composer.keep(),
            V::ToggleReview => self.toggle_review(),
            V::ReviewFile(delta) => self.review_file(delta),
            V::ReviewPage(pages) => self.review_page(pages),
        }
    }

    /// Scroll the transcript `delta` rows up (positive) or down (negative).
    /// Scrolling to the newest rows releases the pin back to the live tail.
    pub fn scroll_by(&mut self, delta: isize) {
        let (len, fits) = self.last_rendered;
        let max_top = len.saturating_sub(fits);
        let top = self.scroll_top.unwrap_or(max_top);
        let next = top.saturating_add_signed(-delta).min(max_top);
        self.scroll_top = (next < max_top).then_some(next);
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
            if !self.pinned && self.pane_mode != PaneMode::Workers {
                self.pane_mode = PaneMode::Workers;
            }
            // The width force is a promotion and a pin always wins it. Save
            // what the operator had so the demotion can restore it.
            if !self.pinned && matches!(self.pane_width, PaneWidth::Off) {
                self.promotion_saved_width = Some(self.pane_width);
                self.pane_width = PaneWidth::Ch56;
            }
        } else if self.pane_mode == PaneMode::Workers && !self.pinned {
            self.pane_mode = PaneMode::Ledger;
            // Give back the operator's width only while it is still the one
            // this promotion set: a `^W` since then is their choice to keep.
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
        self.pane_mode = PaneMode::Output;
        if matches!(self.pane_width, PaneWidth::Off) {
            self.pane_width = PaneWidth::Ch56;
        }
    }

    /// Scroll the OUTPUT pane's content.
    pub fn scroll_output_by(&mut self, delta: isize) {
        if let Some(output) = &mut self.output {
            output.scroll = output.scroll.saturating_add_signed(delta);
        }
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
        if let Promotion::Peek { until_ms, .. } = self.promotion
            && now_ms >= until_ms
        {
            self.promotion = Promotion::None;
        }
    }
}

/// The peek banner's lifetime (SPEC §5: 3 s).
pub const PEEK_MS: u64 = 3_000;

fn first_line(text: &str) -> String {
    text.lines().next().unwrap_or("").chars().take(60).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use p1_contracts::{ToolCall, ToolInput, ToolResultItem, ToolStatus};

    #[test]
    fn paging_moves_by_the_transcript_rows_minus_two_and_returns_to_the_tail() {
        let mut s = Screen::new(false);
        s.last_rendered = (100, 20);
        s.page(1);
        assert_eq!(s.scroll_top, Some(62), "80 − 18");
        s.page(1);
        assert_eq!(s.scroll_top, Some(44));
        s.page(-1);
        s.page(-1);
        assert_eq!(s.scroll_top, None, "paging past the end is the live tail");
        s.page(1);
        s.live_tail();
        assert_eq!(s.scroll_top, None);
    }

    #[test]
    fn the_scroll_mark_counts_rows_below_and_names_the_running_tool() {
        let mut s = Screen::new(false);
        s.last_rendered = (47, 33);
        assert_eq!(s.scroll_mark(), None, "no mark at the live tail");
        s.scroll_top = Some(0);
        let mark = s.scroll_mark().unwrap();
        // The mark takes the view's last row: 32 rows shown, 15 below.
        assert_eq!((mark.below, mark.row, mark.total), (15, 1, 47));
        assert_eq!(mark.running, None);
        s.apply(
            &AgentEvent::ToolStarted {
                call: ToolCall {
                    call_id: "c1".into(),
                    name: "shell".into(),
                    input: ToolInput::Json("{}".into()),
                },
            },
            0,
        );
        assert_eq!(
            s.scroll_mark().unwrap().running.as_deref(),
            Some("shell running")
        );
    }

    #[test]
    fn completion_types_filters_and_completes_through_the_composer() {
        use crate::input::ViewCommand as V;
        let mut s = Screen::new(false);
        s.apply_view(V::OpenCompletion);
        assert_eq!(s.composer.text, "/");
        s.apply_view(V::MenuInput('m'));
        s.apply_view(V::MenuInput('o'));
        let menu = s.picker.as_ref().unwrap();
        assert_eq!(menu.filter, "/mo");
        assert_eq!(menu.visible().len(), 2, "/model and /models");
        s.apply_view(V::Complete);
        assert_eq!(s.composer.text, "/model ");
        assert!(s.picker.is_none());
        // Deleting the `/` closes completion.
        s.composer = Composer::default();
        s.apply_view(V::OpenCompletion);
        s.apply_view(V::MenuBackspace);
        assert!(s.picker.is_none());
        assert_eq!(s.composer.text, "");
    }

    #[test]
    fn the_full_review_toggles_only_over_a_diff_and_pages_within_it() {
        use crate::render::diff::{DiffRow, DiffView};
        let mut s = Screen::new(false);
        s.toggle_review();
        assert!(!s.review.open, "nothing to review");
        s.approval = Some(Approval::Diff(DiffView {
            tool: "edit".into(),
            file: "a.rs".into(),
            summary: String::new(),
            position: (1, 1),
            rows: (0..30)
                .map(|n| DiffRow::Add {
                    line: n + 1,
                    text: String::new(),
                })
                .collect(),
            grantable: true,
        }));
        s.toggle_review();
        assert!(s.review.open);
        s.review.body_rows = 12;
        s.review_page(-1);
        assert_eq!(s.review.scroll, 10);
        s.review_page(-5);
        assert_eq!(s.review.scroll, 18, "clamped at the last full body");
        s.review_page(1);
        assert_eq!(s.review.scroll, 8);
        s.review_file(1);
        assert_eq!((s.review.file, s.review.scroll), (0, 0), "one file wraps");
        s.toggle_review();
        assert!(!s.review.open);
    }

    #[test]
    fn a_goal_edit_restores_the_draft_on_esc_and_ends_on_submit() {
        let mut c = Composer::default();
        c.insert('d');
        c.begin_goal_edit(None);
        assert_eq!(c.text, "/goal ");
        c.keep();
        assert_eq!(c.text, "d");
        c.begin_goal_edit(Some("g"));
        assert_eq!(c.take(), "/goal g");
        assert!(!c.editing_goal());
    }

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

    #[test]
    fn a_live_worker_promotes_the_width_and_restores_the_operator_width() {
        use crate::render::workers::{WorkerRow, WorkerState};
        let row = |state| WorkerRow {
            id: "w1".into(),
            summary: "w1".into(),
            route: "deepseek/v4.1-flash".into(),
            state,
            elapsed: None,
            cost_micro_usd: None,
            details: vec![],
        };
        // Pinned: the width force never overrides the operator.
        let mut s = Screen::new(false);
        s.pinned = true;
        s.pane_width = PaneWidth::Off;
        s.sync_workers(vec![row(WorkerState::Running)]);
        assert_eq!(s.pane_width, PaneWidth::Off, "pinning wins over the force");
        assert_eq!(s.promotion_saved_width, None, "a pin saves nothing");
        // Unpinned Off: force to Ch56, remember Off, restore it on demotion.
        let mut s = Screen::new(false);
        s.pane_width = PaneWidth::Off;
        s.sync_workers(vec![row(WorkerState::Running)]);
        assert_eq!(s.pane_width, PaneWidth::Ch56);
        assert_eq!(s.promotion_saved_width, Some(PaneWidth::Off));
        s.sync_workers(vec![row(WorkerState::Done)]);
        assert_eq!(s.pane_width, PaneWidth::Off);
        assert_eq!(s.promotion_saved_width, None);
        // A `^W` during the promotion is the operator's; demotion keeps it.
        let mut s = Screen::new(false);
        s.pane_width = PaneWidth::Off;
        s.sync_workers(vec![row(WorkerState::Running)]);
        s.pane_width = PaneWidth::Ch40;
        s.sync_workers(vec![row(WorkerState::Done)]);
        assert_eq!(s.pane_width, PaneWidth::Ch40, "an operator change survives");
        assert_eq!(s.promotion_saved_width, None);
    }
}
