//! Screen state: everything the renderers read and the key handlers mutate.
//!
//! The right pane is a core feature, planned from M1 (owner direction): width
//! cycles `narrow / wide / split / off` on `^W`, mode cycles on `^Tab`, events
//! PROMOTE a mode, and pinning always wins (SPEC §5). Focus mode (owner
//! direction, carried over from the iris TUI) folds passive chrome away so
//! the transcript owns the screen; the first edit reveals the composer again.

use std::collections::{HashMap, VecDeque};

use p1_contracts::{AgentEvent, Usage};

use crate::render::home::HomePrelude;
use crate::render::ledger::{
    ContextView, FoldRef, LedgerPane, LedgerSpend, SessionView, WorkersSummary, WorkspaceView,
};
use crate::render::picker::Picker;
use crate::render::workers::{BlockState, WorkerBlock, WorkersPane, display_order};
use crate::transcript::{Block, Transcript};

/// A pending approval (handoff §7.5): inline as the transcript's running element, or the full
/// diff review that owns the screen.
#[derive(Debug, Clone, PartialEq)]
pub enum Approval {
    Diff(crate::render::diff::DiffView),
    Permission(crate::render::permission::PermissionView),
}

/// Pane width states (handoff §4.1). `^W` cycles `narrow → wide → split → off`; `Auto` is the
/// start state: narrow below 160 columns, wide from 160.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PaneWidth {
    #[default]
    Auto,
    /// 38 columns.
    Narrow,
    /// 56 columns.
    Wide,
    /// Half of `W − 6`.
    Split,
    Off,
}

impl PaneWidth {
    /// The state at a terminal `cols` wide: `Auto` becomes narrow or wide.
    pub fn resolve(self, cols: u16) -> Self {
        match self {
            Self::Auto if cols >= 160 => Self::Wide,
            Self::Auto => Self::Narrow,
            other => other,
        }
    }

    /// The next `^W` state. `Auto` cycles like narrow; `Screen::cycle_width` resolves it at the
    /// real width first, so at 160+ columns the start state steps on to split.
    pub fn cycle(self) -> Self {
        match self {
            Self::Off => Self::Narrow,
            Self::Auto | Self::Narrow => Self::Wide,
            Self::Wide => Self::Split,
            Self::Split => Self::Off,
        }
    }
}

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
    /// Only an automatically selected WORKERS mode may fall back on settlement.
    /// Explicit pane navigation relinquishes this ownership. Public so
    /// struct-update test fixtures can construct a screen.
    #[doc(hidden)]
    pub worker_mode_auto: bool,
    /// The width the operator held before a live worker forced the pane open
    /// (`Off` → `Wide`); the demotion gives it back. `None` when no such
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
    /// The WORKERS pane: its rows are refreshed by the driver from the host's worker service
    /// (p1-tui never names p1-workers) through `sync_workers`, which also counts the header;
    /// the pool size and the `↑ ↓` focus are the driver's to set.
    pub workers: WorkersPane,
    /// Whether a worker has ever run this session (handoff §9.1: WORKERS is
    /// available "when a worker was ever started" — unlike `workers`, this
    /// never goes back to `false` once the pane has something worth
    /// revisiting, even after every worker finishes and `sync_workers`
    /// reports an empty snapshot).
    pub workers_ever_started: bool,
    /// A pending approval: drawn inline as the transcript's last element, or as the full diff
    /// review (`review`) that owns the screen (handoff §7.5).
    pub approval: Option<Approval>,
    /// The tool the approval on screen is about: a `PermissionView` names only its command.
    pub approval_tool: String,
    /// Further approvals parked behind the one on screen (`1 of N pending`).
    pub approvals_waiting: usize,
    /// A menu docked above the composer (handoff §6.10).
    pub picker: Option<Picker>,
    /// Retired: `/status` is command output in the transcript now (handoff §6.9), so this can
    /// never be `Some`. Kept only because `input.rs` still asks whether it is open.
    pub status: Option<std::convert::Infallible>,
    /// The home prelude (handoff §6.11): the first rows of the conversation.
    pub home: Option<HomePrelude>,
    /// LEDGER sections the driver fills (§9.2); each is absent until its data exists.
    pub session: Option<SessionView>,
    pub context: Option<ContextView>,
    pub workspace: Option<WorkspaceView>,
    /// The most recent fold handles, newest first (LEDGER FOLDS shows three).
    pub folds: Vec<FoldRef>,
    /// A worker the operator attached to (`a`, handoff §9.5): its transcript replaces the
    /// parent's in the transcript area.
    pub attached: Option<AttachedWorker>,
    /// A running worker awaiting the `y stop   n keep` confirmation.
    pub stop_pending: Option<String>,
    /// Each worker's transcript while it is detached; attach takes the buffer out and detach
    /// puts it back, so worker output survives every view change.
    pub worker_transcripts: HashMap<String, Transcript>,
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
    /// Where the terminal's hardware cursor goes this frame (the composer's text cell, §8.1);
    /// `None` while no editable composer is on screen. The renderer records it.
    pub cursor: Option<(u16, u16)>,
    /// The terminal width of the last frame: `^W` resolves the `Auto` start state at it.
    #[doc(hidden)]
    pub last_width: u16,
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

/// The worker whose own transcript is on screen (handoff §9.5), buffered by the driver from
/// the worker's event stream.
#[derive(Debug)]
pub struct AttachedWorker {
    pub id: String,
    /// `env/profile`: the attach band names it and the statusline chip shows it.
    pub route: String,
    pub state: BlockState,
    pub transcript: Transcript,
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
        self.pane_width = self.pane_width.resolve(self.last_width).cycle();
        self.promotion_saved_width = None;
    }

    /// The modes `^Tab` may land on right now (handoff §9.1): LEDGER always;
    /// OUTPUT once a handle was opened; WORKERS once a worker has ever run.
    /// DIFF needs the session-diff seam (§14.4) — not available yet.
    pub fn available_modes(&self) -> Vec<PaneMode> {
        let mut modes = vec![PaneMode::Ledger];
        if self.output.is_some() {
            modes.push(PaneMode::Output);
        }
        if self.workers_ever_started {
            modes.push(PaneMode::Workers);
        }
        modes
    }

    /// `^Tab` cycles only through `available_modes` (handoff §9.1): a mode
    /// with nothing to show is never landed on, and the current one always
    /// appears in the list it cycles through (it is showing something).
    pub fn cycle_mode(&mut self) {
        let available = self.available_modes();
        self.pane_mode = match available.iter().position(|m| *m == self.pane_mode) {
            Some(at) => available[(at + 1) % available.len()],
            None => available.first().copied().unwrap_or(self.pane_mode),
        };
        if self.pane_mode != PaneMode::Workers {
            self.detach_worker();
        }
        self.worker_mode_auto = false;
        // A deliberate choice is not a pin, but it outlives a transient peek.
        if matches!(self.promotion, Promotion::Peek { .. }) {
            self.promotion = Promotion::None;
        }
    }

    pub fn toggle_pin(&mut self) {
        self.pinned = !self.pinned;
    }

    /// Observe one agent event: transcript first, then the state the event
    /// moves (working indicator, spend, promotion). `now_ms` is the event's
    /// stamp: the transcript times the turn, reasoning and call rows from it.
    pub fn apply(&mut self, event: &AgentEvent, now_ms: u64) {
        match event {
            AgentEvent::ToolStarted { call } => {
                self.call_started.insert(call.call_id.clone(), now_ms);
            }
            AgentEvent::ToolFinished { result } => {
                self.call_started.remove(&result.call_id);
            }
            _ => {}
        }
        self.transcript.apply(event, Some(now_ms));
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
                self.record_fold(&result.call_id);
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

    /// Observe one worker event without moving any parent state (handoff §9.5).
    pub fn apply_worker(&mut self, id: &str, event: &AgentEvent, at_ms: u64) {
        if let Some(worker) = &mut self.attached
            && worker.id == id
        {
            worker.transcript.apply(event, Some(at_ms));
        } else {
            self.worker_transcripts
                .entry(id.to_string())
                .or_default()
                .apply(event, Some(at_ms));
        }
    }

    /// Attach the focused WORKERS row, taking over its buffered transcript (handoff §9.5).
    pub fn attach_selected(&mut self) {
        if self.pane_mode != PaneMode::Workers {
            return;
        }
        let Some(id) = self.workers.focused.clone() else {
            return;
        };
        let Some(row) = self.workers.workers.iter().find(|worker| worker.id == id) else {
            return;
        };
        if self.attached.as_ref().is_some_and(|worker| worker.id == id) {
            return;
        }
        let route = row.route.clone();
        let state = row.state;
        self.detach_worker();
        let transcript = self.worker_transcripts.remove(&id).unwrap_or_default();
        self.attached = Some(AttachedWorker {
            id,
            route,
            state,
            transcript,
        });
        // An attachment owns WORKERS, so settlement must not demote the focused view.
        self.worker_mode_auto = false;
    }

    /// Return to the parent transcript without changing pane focus or selection.
    pub fn detach_worker(&mut self) {
        if let Some(worker) = self.attached.take() {
            self.worker_transcripts.insert(worker.id, worker.transcript);
        }
    }

    /// Ask before stopping the attached worker, or the selected one when detached.
    pub fn ask_stop(&mut self) {
        let target = self
            .attached
            .as_ref()
            .map(|worker| worker.id.clone())
            .or(self.workers.focused.clone());
        let Some(target) = target else {
            return;
        };
        if self.workers.workers.iter().any(|worker| {
            worker.id == target
                && matches!(
                    worker.state,
                    BlockState::Running
                        | BlockState::Queued
                        | BlockState::NeedsReview
                        | BlockState::Stalled
                )
        }) {
            self.stop_pending = Some(target);
        }
    }

    /// Dismiss a pending worker stop without changing the worker.
    pub fn keep_worker(&mut self) {
        self.stop_pending = None;
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

    // Adapted from iris-donor/src/ui/tui/pager.rs ScrollState (pin
    // 5b04a1ad3412ad0bb663b6355f77a024aec0ddfa, MIT): a stale anchor is read
    // through the current layout bound.
    /// The scroll mark while the view is pinned above the live tail. It takes the view's last
    /// row, so the rows below are counted from one row higher.
    pub fn scroll_mark(&self) -> Option<crate::render::scroll::ScrollMark> {
        let (total, fits) = self.last_rendered;
        let shown = fits.saturating_sub(1);
        let top = self.scroll_top?.min(total.saturating_sub(shown));
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
            V::TogglePaneFocus => self.toggle_pane_focus(),
            V::AttachWorker => self.attach_selected(),
            V::AskStopWorker => self.ask_stop(),
            V::KeepWorker => self.keep_worker(),
            V::DetachWorker => self.detach_worker(),
            V::EditGoal => self.edit_goal(),
            V::KeepComposer => self.composer.keep(),
            V::ToggleReview => self.toggle_review(),
            V::ReviewFile(delta) => self.review_file(delta),
            V::ReviewPage(pages) => self.review_page(pages),
        }
    }

    // Adapted from iris-donor/src/ui/tui/pager.rs ScrollState (pin
    // 5b04a1ad3412ad0bb663b6355f77a024aec0ddfa, MIT): movement starts from the
    // top the current frame actually draws.
    /// Scroll the transcript `delta` rows up (positive) or down (negative).
    /// Scrolling to the newest rows releases the pin back to the live tail.
    pub fn scroll_by(&mut self, delta: isize) {
        let (len, fits) = self.last_rendered;
        let max_top = len.saturating_sub(fits);
        let top = self.scroll_top.unwrap_or(max_top).min(max_top);
        let next = top.saturating_add_signed(-delta).min(max_top);
        self.scroll_top = (next < max_top).then_some(next);
    }

    /// Sync the WORKERS pane from a fresh snapshot, handling the promotion
    /// rule (SPEC §5, handoff §9.1): a newly live delegate promotes the pane
    /// to WORKERS; new review attention SELF-PINS WORKERS (`^P` need not be
    /// pressed — a parked approval is the operator's turn). Repeated snapshots
    /// do not undo navigation. On settlement only an automatically selected,
    /// unpinned WORKERS pane falls back to LEDGER.
    pub fn sync_workers(&mut self, rows: Vec<WorkerBlock>) {
        self.workers_ever_started |= !rows.is_empty();
        let newly_active = |state| {
            rows.iter().any(|row| {
                row.state == state
                    && !self
                        .workers
                        .workers
                        .iter()
                        .any(|old| old.id == row.id && old.state == state)
            })
        };
        let new_live = newly_active(BlockState::Running);
        let new_review = newly_active(BlockState::NeedsReview);
        let count = |state| rows.iter().filter(|w| w.state == state).count() as u64;
        let live = count(BlockState::Running);
        let queued = count(BlockState::Queued);
        let needs_review = count(BlockState::NeedsReview) > 0;
        self.workers.header.live = live;
        self.workers.header.queued = (queued > 0).then_some(queued);
        // A focus on a worker that left the snapshot has nothing left to point at.
        if let Some(focused) = &self.workers.focused
            && !rows.iter().any(|w| &w.id == focused)
        {
            self.workers.focused = None;
        }
        if self.stop_pending.as_ref().is_some_and(|id| {
            !rows.iter().any(|worker| {
                &worker.id == id
                    && matches!(
                        worker.state,
                        BlockState::Running
                            | BlockState::Queued
                            | BlockState::NeedsReview
                            | BlockState::Stalled
                    )
            })
        }) {
            self.stop_pending = None;
        }
        self.workers.workers = rows;
        if let Some(worker) = &mut self.attached {
            let id = &worker.id;
            if let Some(row) = self.workers.workers.iter().find(|row| &row.id == id) {
                worker.state = row.state;
            }
        }
        // A parked approval is the operator's turn: SELF-pin, no `^P` needed,
        // so nothing later demotes it out from under them. Only NEW attention
        // moves the mode; a refresh of the same row cannot undo ^Tab.
        let pinned_before = self.pinned;
        if new_review {
            self.pinned = true;
        }
        if new_review || (new_live && !pinned_before) {
            if self.pane_mode != PaneMode::Workers {
                self.pane_mode = PaneMode::Workers;
                self.worker_mode_auto = true;
            }
            // The width force is a promotion and a pin from BEFORE this sync
            // always wins it. Save what the operator had so the demotion can
            // restore it.
            if !pinned_before && matches!(self.pane_width, PaneWidth::Off) {
                self.promotion_saved_width = Some(self.pane_width);
                self.pane_width = PaneWidth::Wide;
            }
        }
        if !needs_review && live == 0 && !self.pinned {
            if self.worker_mode_auto && self.pane_mode == PaneMode::Workers {
                self.pane_mode = PaneMode::Ledger;
            }
            self.worker_mode_auto = false;
            // Give back the operator's width only while it is still the one
            // this promotion set: a `^W` since then is their choice to keep.
            if let Some(saved) = self.promotion_saved_width.take()
                && self.pane_width == PaneWidth::Wide
            {
                self.pane_width = saved;
            }
        }
        if self.pane_focused
            && self.pane_mode == PaneMode::Workers
            && self.workers.focused.is_none()
        {
            self.select_first_worker();
        }
    }
    /// Open a fold handle in the OUTPUT pane (`^O`): switches the pane to
    /// OUTPUT mode and widens it if it is hidden.
    pub fn open_output(&mut self, view: crate::render::output::OutputView) {
        self.output = Some(view);
        self.pane_mode = PaneMode::Output;
        self.detach_worker();
        self.worker_mode_auto = false;
        self.promotion_saved_width = None;
        if matches!(self.pane_width, PaneWidth::Off) {
            self.pane_width = PaneWidth::Wide;
        }
    }

    fn toggle_pane_focus(&mut self) {
        self.pane_focused = !self.pane_focused;
        if !self.pane_focused {
            self.workers.focused = None;
            self.detach_worker();
            return;
        }
        if self.pane_mode != PaneMode::Workers {
            return;
        }
        let order = display_order(&self.workers);
        let selected_is_present = self
            .workers
            .focused
            .as_deref()
            .is_some_and(|id| order.iter().any(|worker| worker.id == id));
        if !selected_is_present {
            self.select_first_worker();
        }
    }

    /// Move the focused WORKERS selection, or scroll OUTPUT when another pane mode owns arrows.
    pub fn pane_step(&mut self, delta: isize) {
        if self.pane_mode != PaneMode::Workers {
            self.scroll_output_by(delta);
            return;
        }
        let order = display_order(&self.workers);
        let Some(last) = order.len().checked_sub(1) else {
            return;
        };
        let current = self.workers.focused.as_deref().and_then(|id| {
            order
                .iter()
                .position(|worker| worker.id == id)
                .map(|index| index as isize)
        });
        let next = match current {
            Some(current) => current.saturating_add(delta).clamp(0, last as isize) as usize,
            None if delta > 0 => 0,
            None if delta < 0 => last,
            None => return,
        };
        self.workers.focused = Some(order[next].id.clone());
    }

    fn select_first_worker(&mut self) {
        self.workers.focused = display_order(&self.workers)
            .first()
            .map(|worker| worker.id.clone());
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

    /// The LEDGER pane from screen state (§9.2). SPEND appears with the first response; the
    /// WORKERS summary while the worker snapshot holds anyone.
    pub fn ledger(&self) -> LedgerPane {
        let workers = &self.workers.workers;
        let count = |states: &[BlockState]| {
            workers.iter().filter(|w| states.contains(&w.state)).count() as u64
        };
        LedgerPane {
            goal: self.goal.clone(),
            session: self.session.clone(),
            context: self.context.clone(),
            workspace: self.workspace.clone(),
            spend: (self.spend.responses > 0).then(|| LedgerSpend {
                input: self.spend.input,
                output: self.spend.output,
                cache_hit_percent: self.spend.cache_hit_percent(),
                cost_micro_usd: self.spend.cost_micro_usd,
            }),
            workers: (!workers.is_empty()).then(|| WorkersSummary {
                live: count(&[BlockState::Running]),
                done: count(&[BlockState::Done, BlockState::DoneUnverified]),
            }),
            folds: self.folds.clone(),
        }
    }

    /// A settled call that registered a fold handle becomes the newest LEDGER fold.
    fn record_fold(&mut self, call_id: &str) {
        let row = self
            .transcript
            .blocks
            .iter()
            .rev()
            .find_map(|block| match block {
                Block::Call(row) if row.call_id == call_id => Some(row),
                _ => None,
            });
        let Some(row) = row else {
            return;
        };
        let Some(id) = &row.fold else {
            return;
        };
        let handle = id.to_string();
        let fold = FoldRef {
            handle: handle.clone(),
            kind: row.name.clone(),
            lines: row.line_count as u64,
        };
        self.folds.retain(|f| f.handle != handle);
        self.folds.insert(0, fold);
        self.folds.truncate(FOLDS_SHOWN);
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

/// LEDGER FOLDS lists this many handles (§9.2).
const FOLDS_SHOWN: usize = 3;

fn first_line(text: &str) -> String {
    text.lines().next().unwrap_or("").chars().take(60).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use p1_contracts::{ToolCall, ToolInput, ToolResultItem, ToolStatus};

    fn worker(id: &str, state: BlockState) -> WorkerBlock {
        WorkerBlock {
            id: id.into(),
            task: "task".into(),
            route: "deepseek/v4.1-flash".into(),
            model: None,
            state,
            elapsed: None,
            cost_micro_usd: None,
            tokens: None,
            context_window: None,
            grants: "read finish".into(),
            activity: String::new(),
        }
    }

    #[test]
    fn the_worker_header_counts_running_and_queued_and_a_lost_focus_clears() {
        let mut s = Screen::new(false);
        s.workers.focused = Some("w9".into());
        s.sync_workers(vec![
            worker("w1", BlockState::Running),
            worker("w2", BlockState::NeedsReview),
            worker("w3", BlockState::Queued),
            worker("w4", BlockState::Done),
        ]);
        assert_eq!(s.workers.header.live, 1);
        assert_eq!(s.workers.header.queued, Some(1));
        assert_eq!(s.workers.focused, None);
        let summary = s.ledger().workers.expect("a snapshot with workers");
        assert_eq!((summary.live, summary.done), (1, 1));
        s.sync_workers(vec![worker("w4", BlockState::Done)]);
        assert_eq!(s.workers.header.queued, None);
    }

    #[test]
    fn a_folded_result_becomes_the_newest_ledger_fold() {
        let mut s = Screen::new(false);
        let big: String = (0..60).map(|n| format!("line {n}\n")).collect();
        for (id, content) in [("c1", big.as_str()), ("c2", "short")] {
            s.apply(
                &AgentEvent::ToolStarted {
                    call: ToolCall {
                        call_id: id.into(),
                        name: "shell".into(),
                        input: ToolInput::Json("{}".into()),
                    },
                },
                0,
            );
            s.apply(
                &AgentEvent::ToolFinished {
                    result: ToolResultItem {
                        call_id: id.into(),
                        name: "shell".into(),
                        status: ToolStatus::Ok,
                        content: content.into(),
                    },
                },
                10,
            );
        }
        assert_eq!(s.folds.len(), 1, "only the folded output has a handle");
        assert_eq!((s.folds[0].kind.as_str(), s.folds[0].lines), ("shell", 60));
        assert_eq!(s.ledger().folds, s.folds);
    }

    #[test]
    fn shrunk_transcript_scrolls_from_the_clamped_top() {
        let mut up = Screen::new(false);
        up.last_rendered = (100, 20);
        up.scroll_top = Some(40);
        up.last_rendered = (30, 20);
        up.scroll_by(1);
        assert_eq!(
            up.scroll_top,
            Some(9),
            "30 - 20 - 1 from the rendered bottom"
        );

        let mut down = Screen::new(false);
        down.last_rendered = (100, 20);
        down.scroll_top = Some(40);
        down.last_rendered = (30, 20);
        down.scroll_by(-1);
        assert_eq!(down.scroll_top, None, "the bottom releases the pin");

        let mut page = Screen::new(false);
        page.last_rendered = (100, 20);
        page.scroll_top = Some(40);
        page.last_rendered = (30, 20);
        page.page(1);
        assert_eq!(
            page.scroll_top,
            Some(0),
            "paging from row 10 saturates at row 0"
        );
    }

    #[test]
    fn shrunk_transcript_scroll_mark_names_the_first_drawn_row() {
        let mut s = Screen::new(false);
        s.last_rendered = (100, 20);
        s.scroll_top = Some(40);
        s.last_rendered = (30, 20);

        let mark = s.scroll_mark().unwrap();
        let stored_top = 40;
        let (total, fits) = s.last_rendered;
        assert_eq!((mark.below, mark.row, mark.total), (0, 12, 30));
        assert_eq!(
            mark.row - 1,
            stored_top.min(total.saturating_sub(fits.saturating_sub(1)))
        );
    }

    #[test]
    fn shrunk_transcript_stays_within_the_rendered_bounds() {
        for len in 0..=60 {
            for fits in 2..=30 {
                for top in 0..=80 {
                    for delta in [-3, -1, 1, 3] {
                        let mut s = Screen::new(false);
                        s.last_rendered = (len, fits);
                        s.scroll_top = Some(top);
                        s.scroll_by(delta);
                        if let Some(next) = s.scroll_top {
                            assert!(
                                len <= fits || next < len - fits,
                                "len={len}, fits={fits}, top={top}, delta={delta}, next={next}"
                            );
                        }
                    }

                    let mut s = Screen::new(false);
                    s.last_rendered = (len, fits);
                    s.scroll_top = Some(top);
                    let mark = s.scroll_mark().unwrap();
                    assert!(
                        mark.row <= len.max(1),
                        "len={len}, fits={fits}, top={top}, row={}",
                        mark.row
                    );
                }
            }
        }
    }

    #[test]
    fn shrunk_transcript_refollows_when_it_fits() {
        for delta in [1, -1] {
            let mut s = Screen::new(false);
            s.last_rendered = (10, 20);
            s.scroll_top = Some(5);
            s.scroll_by(delta);
            assert_eq!(s.scroll_top, None, "delta={delta}");
        }
    }

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
                PaneWidth::Narrow,
                PaneWidth::Wide,
                PaneWidth::Split,
                PaneWidth::Off,
                PaneWidth::Narrow
            ]
        );
    }

    #[test]
    fn the_start_width_is_narrow_below_160_columns_and_wide_from_160() {
        assert_eq!(PaneWidth::default(), PaneWidth::Auto);
        assert_eq!(PaneWidth::Auto.resolve(159), PaneWidth::Narrow);
        assert_eq!(PaneWidth::Auto.resolve(160), PaneWidth::Wide);
        assert_eq!(PaneWidth::Split.resolve(200), PaneWidth::Split);
        // `^W` steps on from what the start state showed.
        let mut s = Screen::new(false);
        s.last_width = 120;
        s.cycle_width();
        assert_eq!(s.pane_width, PaneWidth::Wide);
        let mut s = Screen::new(false);
        s.last_width = 200;
        s.cycle_width();
        assert_eq!(s.pane_width, PaneWidth::Split);
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
        let row = |state| worker("w1", state);
        // Pinned: the width force never overrides the operator.
        let mut s = Screen::new(false);
        s.pinned = true;
        s.pane_width = PaneWidth::Off;
        s.sync_workers(vec![row(BlockState::Running)]);
        assert_eq!(s.pane_width, PaneWidth::Off, "pinning wins over the force");
        assert_eq!(s.promotion_saved_width, None, "a pin saves nothing");
        // Unpinned Off: force to Wide, remember Off, restore it on demotion.
        let mut s = Screen::new(false);
        s.pane_width = PaneWidth::Off;
        s.sync_workers(vec![row(BlockState::Running)]);
        assert_eq!(s.pane_width, PaneWidth::Wide);
        assert_eq!(s.promotion_saved_width, Some(PaneWidth::Off));
        s.sync_workers(vec![row(BlockState::Done)]);
        assert_eq!(s.pane_width, PaneWidth::Off);
        assert_eq!(s.promotion_saved_width, None);
        // A `^W` during the promotion is the operator's; demotion keeps it.
        let mut s = Screen::new(false);
        s.pane_width = PaneWidth::Off;
        s.sync_workers(vec![row(BlockState::Running)]);
        s.pane_width = PaneWidth::Narrow;
        s.sync_workers(vec![row(BlockState::Done)]);
        assert_eq!(
            s.pane_width,
            PaneWidth::Narrow,
            "an operator change survives"
        );
        assert_eq!(s.promotion_saved_width, None);
    }

    #[test]
    fn available_modes_gate_on_what_the_pane_has_to_show() {
        let mut s = Screen::new(false);
        assert_eq!(s.available_modes(), vec![PaneMode::Ledger]);
        s.output = Some(crate::render::output::OutputView {
            id: crate::fold::FoldId::of("x"),
            lines: vec!["a".into()],
            scroll: 0,
        });
        assert_eq!(
            s.available_modes(),
            vec![PaneMode::Ledger, PaneMode::Output]
        );
        s.workers_ever_started = true;
        assert_eq!(
            s.available_modes(),
            vec![PaneMode::Ledger, PaneMode::Output, PaneMode::Workers]
        );
        // DIFF has no seam yet (handoff §14.4): never available.
        assert!(!s.available_modes().contains(&PaneMode::Diff));
    }

    #[test]
    fn workers_ever_started_never_reverts_once_a_worker_ran() {
        let mut s = Screen::new(false);
        assert!(!s.workers_ever_started);
        s.sync_workers(vec![worker("w1", BlockState::Done)]);
        assert!(s.workers_ever_started);
        // The worker service can report an empty snapshot later; the pane
        // stays available (there is history worth revisiting).
        s.sync_workers(vec![]);
        assert!(s.workers_ever_started);
        assert_eq!(s.available_modes().last(), Some(&PaneMode::Workers));
    }

    #[test]
    fn tab_cycles_only_through_available_modes_and_wraps() {
        let mut s = Screen::new(false);
        // Only LEDGER is available: cycling is a no-op, never lands on a mode
        // with nothing to show.
        s.cycle_mode();
        assert_eq!(s.pane_mode, PaneMode::Ledger);
        s.workers_ever_started = true;
        s.cycle_mode();
        assert_eq!(s.pane_mode, PaneMode::Workers);
        s.cycle_mode();
        assert_eq!(s.pane_mode, PaneMode::Ledger, "wraps back to the start");
        // Opening OUTPUT mid-cycle inserts it into the rotation.
        s.output = Some(crate::render::output::OutputView {
            id: crate::fold::FoldId::of("x"),
            lines: vec![],
            scroll: 0,
        });
        s.cycle_mode();
        assert_eq!(s.pane_mode, PaneMode::Output);
    }

    #[test]
    fn a_worker_needing_review_self_pins_workers() {
        let row = |state| worker("w1", state);
        let mut s = Screen::new(false);
        assert!(!s.pinned);
        s.sync_workers(vec![row(BlockState::NeedsReview)]);
        assert_eq!(s.pane_mode, PaneMode::Workers);
        assert!(s.pinned, "a parked approval self-pins, no ^P needed");
        // The self-pin holds even once the review resolves — only the
        // operator's `^P` releases a pin (SPEC §5/handoff §9.1).
        s.sync_workers(vec![row(BlockState::Done)]);
        assert_eq!(s.pane_mode, PaneMode::Workers);
        assert!(s.pinned);
    }
}
