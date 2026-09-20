//! Screen state: everything the renderers read and the key handlers mutate.
//!
//! The right pane is a core feature, planned from M1 (owner direction): width
//! cycles `off / 40ch / 56ch / split` on `^W`, mode cycles on `^Tab`, events
//! PROMOTE a mode, and pinning always wins (SPEC §5). Focus mode (owner
//! direction, carried over from the iris TUI) folds passive chrome away so
//! the transcript owns the screen; the first edit reveals the composer again.

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
    /// An approval or a worker review: self-pinning until decided.
    Blocked,
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
                (slot @ Some(_), None) => {
                    let _ = slot;
                    None
                }
                (None, _) => None,
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
    /// Revealed while focus mode would hide it (an edit, a paste, `esc` down).
    pub revealed: bool,
}

impl Composer {
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
    }

    pub fn take(&mut self) -> String {
        self.cursor = 0;
        self.revealed = false;
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
    pub pane_width: PaneWidth,
    pub pane_mode: PaneMode,
    /// `^P`: no event may swap a pinned mode (SPEC §5).
    pub pinned: bool,
    pub promotion: Promotion,
    /// Focus mode: passive chrome folds away (see SPEC §"Focus mode").
    pub focus: bool,
    pub working: Option<Working>,
    pub spend: Spend,
    /// The goal, host-owned and shown as a quotation (SPEC §4.7).
    pub goal: Option<String>,
    pub reduced_motion: bool,
    /// Forced ledger overlay at narrow widths (`^L`).
    pub ledger_overlay: bool,
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
    /// moves (working indicator, spend, promotion).
    pub fn apply(&mut self, event: &AgentEvent, now_ms: u64) {
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
                self.working = None;
                // A failed tool promotes a 3s peek (SPEC §5 promotion table).
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
        self.transcript.apply(event, None);
    }

    /// A PEEK is a two-line banner that never moves the ledger and never
    /// displaces a pin or an operator-blocked state (SPEC §5).
    pub fn peek(&mut self, lines: [String; 2], now_ms: u64) {
        if self.pinned || matches!(self.promotion, Promotion::Blocked) {
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

        s.promotion = Promotion::Blocked;
        s.peek(["a".into(), "b".into()], 5_000);
        assert_eq!(s.promotion, Promotion::Blocked);
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
