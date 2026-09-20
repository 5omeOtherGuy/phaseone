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
    /// `⌥⏎`: queue a follow-up, injected only when the agent would stop.
    QueueFollowUp(String),
    /// `^C`: cancel the turn; quit when idle.
    CancelOrQuit,
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
    /// `esc`: dismiss the topmost overlay.
    Dismiss,
    /// Picker movement and choice.
    PickerUp,
    PickerDown,
    PickerAccept,
    /// `PageUp` / `PageDown`: scroll the transcript.
    ScrollUp,
    ScrollDown,
}

/// Map one key event to commands, given the screen state. Modal layers win in
/// order: approval (blocking) → picker/status overlay → the composer.
pub fn handle(screen: &Screen, key: KeyEvent) -> Option<Command> {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let alt = key.modifiers.contains(KeyModifiers::ALT);

    if screen.approval.is_some() {
        return match (key.code, ctrl) {
            (KeyCode::Char('y'), false) => Some(Command::ApproveOnce),
            (KeyCode::Char('a'), false) => Some(Command::ApproveSession),
            (KeyCode::Char('p'), false) => Some(Command::ApproveProject),
            (KeyCode::Char('n'), false) => Some(Command::Deny),
            (KeyCode::Char('d'), true) => Some(Command::NextFile),
            (KeyCode::Char('a'), true) => Some(Command::AllFiles),
            (KeyCode::Char('c'), true) => Some(Command::CancelOrQuit),
            _ => None,
        };
    }
    if screen.picker.is_some() || screen.status.is_some() {
        return match (key.code, ctrl) {
            (KeyCode::Up, false) => Some(Command::PickerUp),
            (KeyCode::Down, false) => Some(Command::PickerDown),
            (KeyCode::Enter, false) => Some(Command::PickerAccept),
            (KeyCode::Esc, false) => Some(Command::Dismiss),
            (KeyCode::Char('c'), true) => Some(Command::CancelOrQuit),
            _ => None,
        };
    }
    match (key.code, ctrl, alt) {
        (KeyCode::Enter, false, false) => {
            let text = screen.composer.text.clone();
            if text.trim().is_empty() {
                None
            } else if screen.working.is_some() {
                Some(Command::QueueSteering(text))
            } else {
                Some(Command::Submit(text))
            }
        }
        (KeyCode::Enter, false, true) => {
            let text = screen.composer.text.clone();
            (!text.trim().is_empty()).then_some(Command::QueueFollowUp(text))
        }
        (KeyCode::Char('c'), true, false) => Some(Command::CancelOrQuit),
        (KeyCode::Char('o'), true, false) => Some(Command::OpenFold),
        (KeyCode::Char('r'), true, false) => Some(Command::ExpandReasoning),
        (KeyCode::Char('w'), true, false) => Some(Command::CyclePaneWidth),
        (KeyCode::Char('p'), true, false) => Some(Command::TogglePin),
        (KeyCode::Char('l'), true, false) => Some(Command::ToggleLedgerOverlay),
        (KeyCode::Tab, true, false) => Some(Command::CyclePaneMode),
        (KeyCode::PageUp, false, false) => Some(Command::ScrollUp),
        (KeyCode::PageDown, false, false) => Some(Command::ScrollDown),
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
        assert_eq!(handle(&s, key(KeyCode::Enter)), Some(Command::Submit("fix it".into())));
        s.working = Some(crate::state::Working { label: "shell".into(), started_ms: 0 });
        assert_eq!(
            handle(&s, key(KeyCode::Enter)),
            Some(Command::QueueSteering("fix it".into()))
        );
        // Alt+Enter queues a follow-up in either state.
        assert_eq!(
            handle(&s, KeyEvent::new(KeyCode::Enter, KeyModifiers::ALT)),
            Some(Command::QueueFollowUp("fix it".into()))
        );
        // An empty composer never sends.
        s.composer.text.clear();
        s.working = None;
        assert_eq!(handle(&s, key(KeyCode::Enter)), None);
    }

    #[test]
    fn a_pending_approval_captures_single_character_decisions() {
        let mut s = Screen::default();
        s.approval = Some(Approval::Permission(crate::render::permission::PermissionView {
            command: "rm -rf /".into(),
            rows: vec![],
            grantable: false,
        }));
        assert_eq!(handle(&s, key(KeyCode::Char('y'))), Some(Command::ApproveOnce));
        assert_eq!(handle(&s, key(KeyCode::Char('n'))), Some(Command::Deny));
        assert_eq!(handle(&s, ctrl('d')), Some(Command::NextFile));
        // Composer keys do not leak through the modal layer.
        assert_eq!(handle(&s, key(KeyCode::Char('w'))), None);
    }

    #[test]
    fn the_picker_layer_moves_accepts_and_dismisses() {
        let mut s = Screen::default();
        s.picker = Some(Picker {
            groups: vec![PickerGroup {
                header: "G".into(),
                rows: vec![PickerRow { label: "a".into(), value: String::new(), available: true }],
            }],
            filter: String::new(),
            selected: 0,
        });
        assert_eq!(handle(&s, key(KeyCode::Down)), Some(Command::PickerDown));
        assert_eq!(handle(&s, key(KeyCode::Enter)), Some(Command::PickerAccept));
        assert_eq!(handle(&s, key(KeyCode::Esc)), Some(Command::Dismiss));
    }

    use crate::state::Approval;
}
