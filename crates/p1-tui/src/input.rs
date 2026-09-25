//! Key input (SPEC §7): crossterm events in, commands out. This module only
//! DECIDES what a key means in the current screen state; the driver performs
//! the effects. Single-character decisions (`y`/`a`/`p`/`n`) stay single
//! characters — never a button row.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::state::Screen;

/// What a keypress asks the driver to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    /// `⏎` idle: send the composer text as the next prompt.
    Submit(String),
    /// `⏎` while working: queue the text as steering for the next boundary.
    QueueSteering(String),
    /// `⌥⏎` while working: queue a follow-up, injected only when the agent
    /// would stop (SPEC §7: idle `⌥⏎` is a plain newline instead).
    QueueFollowUp(String),
    /// `⌥⏎` while working on a `/word`: a command runs now; an unknown word
    /// is noted first and, confirmed, queued as a follow-up.
    SubmitFollowUp(String),
    /// Composer editing.
    Insert(char),
    Edit(crate::state::EditAction),
    PaneLeft,
    PaneRight,
    PaneFirst,
    ScrollFirst,
    ScrollLast,
    OutputSearch,
    OutputFilter(char),
    OutputFilterBackspace,
    OutputFilterDone,
    PaneLast,
    Newline,
    Backspace,
    Left,
    Right,
    /// `^C`: cancel the turn; quit when idle with an empty composer.
    CancelOrQuit,
    /// `^C` at idle with a draft: clear it (Up brings it back).
    ClearDraft,
    /// A key typed while the OUTPUT pane had focus: focus returns to the
    /// composer and the key is handled there (Iris type-through).
    ReturnToComposer(KeyEvent),
    /// Enter/Tab in the OUTPUT pane: back to the composer, nothing typed.
    FocusComposer,
    /// `Alt+↑/↓`: scroll the transcript by lines.
    ScrollLines(isize),
    /// `Esc` while typing an output filter: restore the previous filter.
    OutputFilterCancel,
    /// `y` in the OUTPUT pane: copy what it shows (the filtered lines, or all).
    CopyOutput,
    /// Close /help, /status or the ledger overlay and type the key in the draft.
    CloseOverlayAndType(KeyEvent),
    /// Scroll a tall overlay (/help at small heights).
    OverlayScroll(isize),
    /// Esc on a filtered OUTPUT pane: drop the filter first.
    OutputFilterClear,
    /// The slash palette: move, complete (Tab), close (Esc).
    PaletteMove(isize),
    PaletteComplete,
    PaletteRun,
    PaletteDismiss,
    /// Keyboard selection of transcript blocks (Tab).
    SelectBlocks,
    SelectStep(isize),
    ToggleSelected,
    OpenSelected,
    CopySelected,
    /// Leave selection; with a key, handle it in the composer (typing wins).
    EndSelection(Option<KeyEvent>),
    /// `^Y`: insert the last killed text; `^Z` / `^_`: undo the last edit.
    Yank,
    Undo,
    /// Approval decisions (SPEC §4.4/§4.5).
    ApproveOnce,
    ApproveSession,
    ApproveProject,
    Deny,
    /// `^D` / `^A`: next file / all files in a multi-file approval.
    NextFile,
    AllFiles,
    /// `^O`: open the fold handle at the cursor in the pane.
    OpenFold,
    /// `^R`: expand the collapsed reasoning block.
    ExpandReasoning,
    CyclePaneMode,
    CyclePaneWidth,
    TogglePin,
    ToggleLedgerOverlay,
    /// `^G`: edit the goal (prefills `/goal ` in the composer).
    EditGoal,
    /// `esc`: dismiss the topmost overlay.
    Dismiss,
    /// Picker movement and choice.
    PickerUp,
    PickerDown,
    PickerAccept,
    /// `PageUp` / `PageDown`: scroll the transcript.
    ScrollUp,
    ScrollDown,
    /// A layer key (`^O`, `^L`, pane keys) while an approval waits: nothing
    /// may cover the decision, so the driver only says why.
    DecideFirst,
    /// `p` at an approval: shown, unavailable until a trust store exists.
    ProjectUnavailable,
    /// `Up` / `Down` with the OUTPUT pane open: scroll the pane.
    PaneUp,
    PaneDown,
}

/// An approval must be on screen this long before a key can decide it.
pub const APPROVAL_ARM_MS: u64 = 400;
/// A decision key right after another printable key is typing, not deciding.
pub const TYPING_PAUSE_MS: u64 = 300;

/// Whether Enter queues steering: a turn is running, or the driver is about to
/// start one (the gap before `TurnStarted` arrives must not submit twice).
fn busy(screen: &Screen) -> bool {
    screen.working.is_some() || screen.busy
}

/// Map one key event to commands, given the screen state. Modal layers win in
/// order: approval (blocking) → picker/status overlay → the composer.
pub fn handle(screen: &Screen, key: KeyEvent) -> Option<Command> {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let alt = key.modifiers.contains(KeyModifiers::ALT);

    // A layer somehow over an approval (the driver never opens one) owns the
    // keys, Esc closes it: a decision is only made while the approval shows.
    let covered = screen.ledger_overlay
        || screen.status.is_some()
        || screen.picker.is_some()
        || screen.output_focus;
    if let Some(approval) = screen.approval.as_ref().filter(|_| !covered) {
        let grantable = match approval {
            crate::state::Approval::Diff(v) => v.grantable,
            crate::state::Approval::Permission(v) => v.grantable,
        };
        // Keys typed as the approval appeared, mid-word or into a message are
        // not decisions: they go to the draft. A decision is a key over an
        // empty draft, after the approval has been up a moment and after a
        // pause in typing (the driver then also waits a beat for a following
        // key before committing it). With text in the draft, y/a/n are text.
        let deliberate = screen.approval_visible
            && screen.composer.text.trim().is_empty()
            && screen.now_ms >= screen.approval_shown_ms + APPROVAL_ARM_MS
            && screen
                .last_type_ms
                .is_none_or(|t| screen.now_ms >= t + TYPING_PAUSE_MS);
        let plain = !ctrl && !alt;
        match key.code {
            KeyCode::Char('y' | 'Y') if plain && deliberate => return Some(Command::ApproveOnce),
            KeyCode::Char('a' | 'A')
                if plain && deliberate && grantable && screen.approval_grant_visible =>
            {
                return Some(Command::ApproveSession);
            }
            KeyCode::Char('n' | 'N') if plain && deliberate => return Some(Command::Deny),
            KeyCode::Char('p' | 'P') if plain && deliberate => {
                return Some(Command::ProjectUnavailable);
            }
            // ^C during the turn cancels it (the parked call is denied); an
            // approval at idle (a worker's) is simply denied.
            KeyCode::Char('c') if ctrl && busy(screen) => return Some(Command::CancelOrQuit),
            KeyCode::Char('c') if ctrl => return Some(Command::Deny),
            KeyCode::PageUp => return Some(Command::ScrollUp),
            KeyCode::PageDown => return Some(Command::ScrollDown),
            KeyCode::Esc => return None,
            KeyCode::Char('o' | 'l' | 'w' | 'p') if ctrl => return Some(Command::DecideFirst),
            KeyCode::Tab if ctrl => return Some(Command::DecideFirst),
            KeyCode::F(6) => return Some(Command::DecideFirst),
            // Everything else edits the draft below the approval as usual.
            _ => {}
        }
    }
    // During a turn ^C always cancels it, whatever is open; at idle it closes
    // the topmost layer first and never quits through an overlay or a draft.
    let idle_ctrl_c = ctrl && key.code == KeyCode::Char('c') && !busy(screen);
    if screen.ledger_overlay {
        match key.code {
            KeyCode::Esc | KeyCode::Enter => return Some(Command::Dismiss),
            KeyCode::Char(_) if idle_ctrl_c => return Some(Command::Dismiss),
            // Typing closes the overlay and lands in the draft.
            KeyCode::Char(_) | KeyCode::Backspace if !ctrl && !alt => {
                return Some(Command::CloseOverlayAndType(key));
            }
            _ => {}
        }
    }
    // A list the last frame had no room to draw does not take keys: any key
    // closes it (the terminal is too short to show it).
    if (screen.status.is_some() || screen.picker.is_some()) && screen.overlay_hidden {
        return Some(Command::Dismiss);
    }
    if screen.status.is_some() && screen.picker.is_none() {
        return match (key.code, ctrl) {
            (KeyCode::Up, false) => Some(Command::OverlayScroll(-1)),
            (KeyCode::Down, false) => Some(Command::OverlayScroll(1)),
            (KeyCode::PageUp, false) => Some(Command::OverlayScroll(-10)),
            (KeyCode::PageDown, false) => Some(Command::OverlayScroll(10)),
            (KeyCode::Enter | KeyCode::Esc, false) => Some(Command::Dismiss),
            (KeyCode::Char('c'), true) if idle_ctrl_c => Some(Command::Dismiss),
            (KeyCode::Char('c'), true) => Some(Command::CancelOrQuit),
            (KeyCode::Char(_) | KeyCode::Backspace, false) if !alt => {
                Some(Command::CloseOverlayAndType(key))
            }
            _ => None,
        };
    }
    if screen.picker.is_some() || screen.status.is_some() {
        return match (key.code, ctrl) {
            (KeyCode::Up, false) => Some(Command::PickerUp),
            (KeyCode::Down, false) => Some(Command::PickerDown),
            // Enter accepts a picker's selection; on a status overlay there is
            // nothing to accept, so it dismisses (like esc).
            (KeyCode::Enter, false) if screen.picker.is_some() => Some(Command::PickerAccept),
            (KeyCode::Enter, false) => Some(Command::Dismiss),
            (KeyCode::Esc, false) => Some(Command::Dismiss),
            (KeyCode::Char('c'), true) if idle_ctrl_c => Some(Command::Dismiss),
            (KeyCode::Char('c'), true) => Some(Command::CancelOrQuit),
            _ => None,
        };
    }
    if screen.output_focus {
        if screen.output_search {
            return match key.code {
                KeyCode::Enter => Some(Command::OutputFilterDone),
                KeyCode::Esc => Some(Command::OutputFilterCancel),
                KeyCode::Backspace => Some(Command::OutputFilterBackspace),
                KeyCode::Up => Some(Command::PaneUp),
                KeyCode::Down => Some(Command::PaneDown),
                KeyCode::PageUp => Some(Command::ScrollUp),
                KeyCode::PageDown => Some(Command::ScrollDown),
                KeyCode::Char('c') if ctrl && idle_ctrl_c => Some(Command::OutputFilterCancel),
                KeyCode::Char('c') if ctrl => Some(Command::CancelOrQuit),
                KeyCode::Char(c) if !ctrl && !alt => Some(Command::OutputFilter(c)),
                _ => None,
            };
        }
        return match key.code {
            KeyCode::Char('/') if !ctrl && !alt => Some(Command::OutputSearch),
            KeyCode::Esc if !screen.output_filter.is_empty() => Some(Command::OutputFilterClear),
            KeyCode::Esc => Some(Command::Dismiss),
            KeyCode::Up => Some(Command::PaneUp),
            KeyCode::Down => Some(Command::PaneDown),
            KeyCode::Left => Some(Command::PaneLeft),
            KeyCode::Right => Some(Command::PaneRight),
            KeyCode::Home if ctrl => Some(Command::ScrollFirst),
            KeyCode::End if ctrl => Some(Command::ScrollLast),
            KeyCode::Home => Some(Command::PaneFirst),
            KeyCode::End => Some(Command::PaneLast),
            KeyCode::PageUp => Some(Command::ScrollUp),
            KeyCode::PageDown => Some(Command::ScrollDown),
            // ^O: the newer output when there is one, else close.
            KeyCode::Char('o') if ctrl => Some(Command::OpenFold),
            KeyCode::Char('w') if ctrl => Some(Command::CyclePaneWidth),
            KeyCode::Char('p') if ctrl => Some(Command::TogglePin),
            KeyCode::Char('l') if ctrl => Some(Command::ToggleLedgerOverlay),
            KeyCode::Char('c') if ctrl && idle_ctrl_c => Some(Command::Dismiss),
            KeyCode::Char('c') if ctrl => Some(Command::CancelOrQuit),
            KeyCode::Char('y') if !ctrl && !alt => Some(Command::CopyOutput),
            KeyCode::Enter | KeyCode::Tab => Some(Command::FocusComposer),
            // Typing wins: a printable key or Backspace goes back to the draft.
            KeyCode::Char(_) | KeyCode::Backspace if !ctrl && !alt => {
                Some(Command::ReturnToComposer(key))
            }
            _ => None,
        };
    }
    use crate::state::EditAction as E;
    if screen.selected.is_some() {
        return Some(match key.code {
            KeyCode::Up if !ctrl && !alt => Command::SelectStep(-1),
            KeyCode::Down if !ctrl && !alt => Command::SelectStep(1),
            KeyCode::Enter if !ctrl && !alt => Command::ToggleSelected,
            KeyCode::Char('o') if !alt => Command::OpenSelected,
            KeyCode::Char('y') if !ctrl && !alt => Command::CopySelected,
            KeyCode::Esc | KeyCode::Tab => Command::EndSelection(None),
            KeyCode::PageUp => Command::ScrollUp,
            KeyCode::PageDown => Command::ScrollDown,
            KeyCode::Char('c') if ctrl && busy(screen) => Command::CancelOrQuit,
            KeyCode::Char('c') if ctrl => Command::EndSelection(None),
            _ => Command::EndSelection(Some(key)),
        });
    }
    if key.code == KeyCode::Enter && key.modifiers.contains(KeyModifiers::SHIFT) {
        return Some(Command::Newline);
    }
    // The slash palette owns ↑↓, Tab and Esc while it shows (Iris palette).
    if !screen.palette().is_empty() && !screen.overlay_hidden {
        match (key.code, ctrl, alt) {
            (KeyCode::Up, false, false) => return Some(Command::PaletteMove(-1)),
            (KeyCode::Down, false, false) => return Some(Command::PaletteMove(1)),
            (KeyCode::Tab, false, false) => return Some(Command::PaletteComplete),
            (KeyCode::Esc, false, false) => return Some(Command::PaletteDismiss),
            (KeyCode::Enter, false, false)
                if screen.composer.text.trim() == "/"
                    || matches!(
                        crate::commands::parse(&screen.composer.text),
                        crate::commands::Parsed::Unknown(_)
                    ) =>
            {
                // A prefix like `/st`: run the highlighted command.
                return Some(Command::PaletteRun);
            }
            _ => {}
        }
    }
    if key.code == KeyCode::Tab && key.modifiers.is_empty() && screen.approval.is_none() {
        return Some(Command::SelectBlocks);
    }
    let text = || screen.composer.text.clone();
    // Local commands run at once, even while a turn runs: they never become
    // steering (a `/status` typed mid-turn must not reach the model).
    let command = !matches!(
        crate::commands::parse(&screen.composer.text),
        crate::commands::Parsed::Prompt
    );
    match (key.code, ctrl, alt) {
        (KeyCode::Char('j'), true, false) => Some(Command::Newline),
        (KeyCode::Home, false, false) | (KeyCode::Char('a'), true, false) => {
            Some(Command::Edit(E::Home))
        }
        (KeyCode::End, false, false) | (KeyCode::Char('e'), true, false) => {
            Some(Command::Edit(E::End))
        }
        (KeyCode::Delete, false, false) => Some(Command::Edit(E::Delete)),
        (KeyCode::Char('u'), true, false) => Some(Command::Edit(E::ClearLine)),
        (KeyCode::Char('k'), true, false) => Some(Command::Edit(E::KillEnd)),
        (KeyCode::Backspace, true, false)
        | (KeyCode::Backspace, false, true)
        | (KeyCode::Char('w'), false, true) => Some(Command::Edit(E::DeleteWord)),
        (KeyCode::Char('d'), false, true) | (KeyCode::Delete, _, true) => {
            Some(Command::Edit(E::DeleteWordRight))
        }
        (KeyCode::Left, true, false)
        | (KeyCode::Left, false, true)
        | (KeyCode::Char('b'), false, true) => Some(Command::Edit(E::WordLeft)),
        (KeyCode::Right, true, false)
        | (KeyCode::Right, false, true)
        | (KeyCode::Char('f'), false, true) => Some(Command::Edit(E::WordRight)),
        (KeyCode::Up, false, false) => Some(Command::Edit(E::Up)),
        (KeyCode::Down, false, false) => Some(Command::Edit(E::Down)),
        (KeyCode::Up, false, true) => Some(Command::ScrollLines(1)),
        (KeyCode::Down, false, true) => Some(Command::ScrollLines(-1)),
        (KeyCode::Home, true, false) => Some(Command::ScrollFirst),
        (KeyCode::End, true, false) => Some(Command::ScrollLast),
        (KeyCode::Enter, false, false) => {
            let text = text();
            if text.trim().is_empty() {
                None
            } else if busy(screen) && !command {
                Some(Command::QueueSteering(text))
            } else {
                Some(Command::Submit(text))
            }
        }
        (KeyCode::Enter, false, true) => {
            // SPEC §7: ⌥⏎ is `newline` idle, `queue follow-up` while working.
            if busy(screen) {
                let text = text();
                if command {
                    Some(Command::SubmitFollowUp(text))
                } else {
                    (!text.trim().is_empty()).then_some(Command::QueueFollowUp(text))
                }
            } else {
                Some(Command::Newline)
            }
        }
        // ^C: cancel a turn; at idle clear a draft first, quit only when empty.
        (KeyCode::Char('c'), true, false) if !busy(screen) && !screen.composer.text.is_empty() => {
            Some(Command::ClearDraft)
        }
        (KeyCode::Char('c'), true, false) => Some(Command::CancelOrQuit),
        (KeyCode::Char('o'), true, false) => Some(Command::OpenFold),
        (KeyCode::Char('r'), true, false) => Some(Command::ExpandReasoning),
        (KeyCode::Char('w'), true, false) => Some(Command::CyclePaneWidth),
        (KeyCode::Char('p'), true, false) => Some(Command::TogglePin),
        (KeyCode::Char('l'), true, false) => Some(Command::ToggleLedgerOverlay),
        (KeyCode::Char('g'), true, false) => Some(Command::EditGoal),
        (KeyCode::Char('y'), true, false) => Some(Command::Yank),
        // ^_ arrives as Ctrl+7 on legacy terminals.
        (KeyCode::Char('z' | '_' | '7'), true, false) => Some(Command::Undo),
        // Legacy terminals send ^H for Ctrl+Backspace, some for Backspace:
        // readline's meaning (delete one character) is safe for both.
        (KeyCode::Char('h'), true, false) => Some(Command::Backspace),
        (KeyCode::Tab, true, false) | (KeyCode::F(6), false, false) => Some(Command::CyclePaneMode),
        (KeyCode::PageUp, false, false) => Some(Command::ScrollUp),
        (KeyCode::PageDown, false, false) => Some(Command::ScrollDown),
        // Esc from the composer closes an OUTPUT pane, then drops a /find.
        (KeyCode::Esc, false, false)
            if screen.pane_mode == crate::state::PaneMode::Output || screen.search.is_some() =>
        {
            Some(Command::Dismiss)
        }
        // Alt chords are commands, never text (AltGr characters arrive without ALT).
        (KeyCode::Char(c), false, false) => Some(Command::Insert(c)),
        (KeyCode::Backspace, false, false) => Some(Command::Backspace),
        (KeyCode::Left, false, false) => Some(Command::Left),
        (KeyCode::Right, false, false) => Some(Command::Right),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::render::picker::{Picker, PickerGroup, PickerRow};

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn ctrl(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
    }

    #[test]
    fn enter_submits_idle_and_steers_while_working() {
        let mut s = Screen::default();
        s.composer.text = "fix it".into();
        assert_eq!(
            handle(&s, key(KeyCode::Enter)),
            Some(Command::Submit("fix it".into()))
        );
        s.working = Some(crate::state::Working {
            label: "shell".into(),
            started_ms: 0,
        });
        assert_eq!(
            handle(&s, key(KeyCode::Enter)),
            Some(Command::QueueSteering("fix it".into()))
        );
        // ⌥⏎ idle is a plain newline; while working it queues a follow-up.
        assert_eq!(
            handle(&s, KeyEvent::new(KeyCode::Enter, KeyModifiers::ALT)),
            Some(Command::QueueFollowUp("fix it".into()))
        );
        s.working = None;
        assert_eq!(
            handle(&s, KeyEvent::new(KeyCode::Enter, KeyModifiers::ALT)),
            Some(Command::Newline)
        );
        // An empty composer never sends.
        s.composer.text.clear();
        s.working = None;
        assert_eq!(handle(&s, key(KeyCode::Enter)), None);
    }

    #[test]
    fn a_pending_approval_captures_single_character_decisions() {
        let mut s = Screen {
            approval: Some(Approval::Permission(
                crate::render::permission::PermissionView {
                    tool: "shell".into(),
                    command: "rm -rf /".into(),
                    rows: vec![],
                    grantable: false,
                },
            )),
            approval_shown_ms: 1_000,
            approval_visible: true,
            now_ms: 1_500,
            busy: true,
            ..Default::default()
        };
        assert_eq!(
            handle(&s, key(KeyCode::Char('y'))),
            Some(Command::ApproveOnce)
        );
        assert_eq!(handle(&s, key(KeyCode::Char('n'))), Some(Command::Deny));
        // ^C cancels the turn, which denies the parked call.
        assert_eq!(handle(&s, ctrl('c')), Some(Command::CancelOrQuit));
        // Not grantable: `a` decides nothing; `p` never decides (no trust
        // store) — it says so instead of becoming text.
        assert_eq!(
            handle(&s, key(KeyCode::Char('a'))),
            Some(Command::Insert('a'))
        );
        assert_eq!(
            handle(&s, key(KeyCode::Char('p'))),
            Some(Command::ProjectUnavailable)
        );
        // Other letters are draft text, never decisions.
        assert_eq!(
            handle(&s, key(KeyCode::Char('w'))),
            Some(Command::Insert('w'))
        );
        // Typed mid-word or as the approval appears: draft text, not a decision.
        s.last_type_ms = Some(1_400);
        assert_eq!(
            handle(&s, key(KeyCode::Char('y'))),
            Some(Command::Insert('y'))
        );
        s.last_type_ms = None;
        s.now_ms = 1_200;
        assert_eq!(
            handle(&s, key(KeyCode::Char('n'))),
            Some(Command::Insert('n'))
        );
    }

    #[test]
    fn the_picker_layer_moves_accepts_and_dismisses() {
        let s = Screen {
            picker: Some(Picker {
                groups: vec![PickerGroup {
                    header: "G".into(),
                    rows: vec![PickerRow {
                        label: "a".into(),
                        value: String::new(),
                        available: true,
                    }],
                }],
                filter: String::new(),
                selected: 0,
            }),
            ..Default::default()
        };
        assert_eq!(handle(&s, key(KeyCode::Down)), Some(Command::PickerDown));
        assert_eq!(handle(&s, key(KeyCode::Enter)), Some(Command::PickerAccept));
        assert_eq!(handle(&s, key(KeyCode::Esc)), Some(Command::Dismiss));
    }

    use crate::state::Approval;
}
