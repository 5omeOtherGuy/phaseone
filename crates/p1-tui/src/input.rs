//! Key input (SPEC §7): crossterm events in, commands out. This module only
//! DECIDES what a key means in the current screen state; the driver performs
//! the effects. Single-character decisions (`y`/`a`/`p`/`n`) stay single
//! characters — never a button row.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseEvent, MouseEventKind};

use crate::state::{Approval, PaneMode, PaneWidth, Screen};

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
    /// Composer editing.
    Insert(char),
    Newline,
    Backspace,
    Left,
    Right,
    /// `^C`: cancel the turn; quit when idle.
    CancelOrQuit,
    /// Approval decisions (SPEC §4.4/§4.5).
    ApproveOnce,
    ApproveSession,
    ApproveProject,
    Deny,
    /// Never produced any more: `^D` toggles the full review and `tab` pages its files
    /// (`ViewCommand`), and `^A` is removed (`y` allows every file of the call). Kept while
    /// p1-host matches on them.
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
    /// `^G` through `handle`: edit the goal (the driver prefills the composer).
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
    /// `Up` / `Down` with pane focus: scroll the pane.
    PaneUp,
    PaneDown,
    /// `y` on a pending worker stop: cancel that worker through the host service.
    StopWorker(String),
}

/// A key decision that only changes what the screen shows. The TUI performs these itself
/// (`Screen::apply_view`); the driver never needs to know about them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ViewCommand {
    /// `PgUp` / `PgDn`: the transcript by its rows minus two.
    PageUp,
    PageDown,
    /// `esc` while scrolled back.
    LiveTail,
    /// `/` in an empty composer.
    OpenCompletion,
    /// Typing and deleting while a menu is open.
    MenuInput(char),
    MenuBackspace,
    /// `tab` in command completion.
    Complete,
    /// `← →` on the focused `/model` row.
    Effort(isize),
    /// `^F`, and `esc` while the pane has focus.
    TogglePaneFocus,
    /// `⏎` / `a` on the focused WORKERS row (handoff §9.5).
    AttachWorker,
    /// `x` on the focused WORKERS row: ask before stopping it.
    AskStopWorker,
    /// `n` / `esc` on the pending worker stop: keep it running.
    KeepWorker,
    /// `esc` while a worker transcript is attached.
    DetachWorker,
    /// `^G`: the goal, prefilled, in the composer.
    EditGoal,
    /// `esc` during a goal edit: the previous composer text comes back.
    KeepComposer,
    // History key selection is adapted from
    // `iris-donor/src/ui/tui_loop.rs` (`prompt_history_key`) at
    // 5b04a1ad3412ad0bb663b6355f77a024aec0ddfa (MIT).
    /// `Up` / `Down` recall submitted prompts while idle.
    HistoryPrev,
    HistoryNext,
    /// Composer line editing, including while a turn is running.
    //
    // The key aliases are adapted from `iris-donor/src/ui/tui_loop.rs`
    // (`pi_editor_key_aliases_work` and `apply_editor_key`) at
    // 5b04a1ad3412ad0bb663b6355f77a024aec0ddfa (MIT).
    DeleteToLineStart,
    DeleteToLineEnd,
    LineStart,
    LineEnd,
    DeleteForward,
    /// `^D` on a diff decision.
    ToggleReview,
    /// `tab` / `⇧tab` in the full review.
    ReviewFile(isize),
    /// `PgUp` (+1) / `PgDn` (−1) in the full review.
    ReviewPage(isize),
    WheelTranscript(isize),
    WheelPane(isize),
    WheelReview(isize),
    ToggleMouse,
}

/// What a key means: work for the driver, or a view change the screen makes itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    Command(Command),
    View(ViewCommand),
}

/// Map one key event to commands, given the screen state — the driver's entry point until it
/// applies view commands. Keys whose only meaning is a view change map to the nearest command
/// the driver already performs, or to nothing.
pub fn handle(screen: &Screen, key: KeyEvent) -> Option<Command> {
    // `^F` cannot focus the pane before the driver applies view commands; until then an open
    // OUTPUT pane keeps scrolling on the arrows, as it always has.
    if screen.approval.is_none()
        && screen.picker.is_none()
        && screen.status.is_none()
        && screen.output.is_some()
        && key.modifiers.is_empty()
    {
        match key.code {
            KeyCode::Up => return Some(Command::PaneUp),
            KeyCode::Down => return Some(Command::PaneDown),
            _ => {}
        }
    }
    match decide(screen, key)? {
        Action::Command(command) => Some(command),
        Action::View(ViewCommand::PageUp) => Some(Command::ScrollUp),
        Action::View(ViewCommand::PageDown) => Some(Command::ScrollDown),
        Action::View(ViewCommand::EditGoal) => Some(Command::EditGoal),
        Action::View(ViewCommand::OpenCompletion) => Some(Command::Insert('/')),
        Action::View(_) => None,
    }
}

/// Map one key event to an action (handoff §12), given the screen state. Modal layers win in
/// order: a decision on screen → a menu or overlay → the composer.
pub fn decide(screen: &Screen, key: KeyEvent) -> Option<Action> {
    use Action::{Command as C, View as V};
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let alt = key.modifiers.contains(KeyModifiers::ALT);

    if ctrl && key.code == KeyCode::Char('t') {
        return Some(V(ViewCommand::ToggleMouse));
    }

    if let Some(approval) = &screen.approval {
        let diff = matches!(approval, Approval::Diff(_));
        let grantable = match approval {
            Approval::Diff(view) => view.grantable,
            Approval::Permission(view) => view.grantable,
        };
        let full = diff && screen.review.open;
        // Adapted from `iris-donor/src/ui/tui_loop.rs` `approval_key` at
        // 5b04a1ad3412ad0bb663b6355f77a024aec0ddfa (MIT): unavailable grants
        // must not become commands while the approval remains modal.
        return match (key.code, ctrl) {
            (KeyCode::Char('y'), false) => Some(C(Command::ApproveOnce)),
            (KeyCode::Char('a'), false) if grantable => Some(C(Command::ApproveSession)),
            (KeyCode::Char('p'), false) if grantable => Some(C(Command::ApproveProject)),
            (KeyCode::Char('n'), false) => Some(C(Command::Deny)),
            (KeyCode::Char('d'), true) if diff => Some(V(ViewCommand::ToggleReview)),
            (KeyCode::Char('c'), true) => Some(C(Command::CancelOrQuit)),
            (KeyCode::Tab, false) if full => Some(V(ViewCommand::ReviewFile(1))),
            (KeyCode::BackTab, _) if full => Some(V(ViewCommand::ReviewFile(-1))),
            (KeyCode::PageUp, false) if full => Some(V(ViewCommand::ReviewPage(1))),
            (KeyCode::PageDown, false) if full => Some(V(ViewCommand::ReviewPage(-1))),
            (KeyCode::PageUp, false) => Some(V(ViewCommand::PageUp)),
            (KeyCode::PageDown, false) => Some(V(ViewCommand::PageDown)),
            _ => None,
        };
    }
    if screen.picker.is_some() || screen.status.is_some() {
        let menu = screen.picker.is_some();
        return match (key.code, ctrl) {
            (KeyCode::Up, false) => Some(C(Command::PickerUp)),
            (KeyCode::Down, false) => Some(C(Command::PickerDown)),
            // Enter accepts a picker's selection; on a status overlay there is
            // nothing to accept, so it dismisses (like esc).
            (KeyCode::Enter, false) if menu => Some(C(Command::PickerAccept)),
            (KeyCode::Enter, false) => Some(C(Command::Dismiss)),
            (KeyCode::Esc, false) => Some(C(Command::Dismiss)),
            (KeyCode::Char('c'), true) => Some(C(Command::CancelOrQuit)),
            (KeyCode::Tab, false) if menu => Some(V(ViewCommand::Complete)),
            (KeyCode::Left, false) if menu => Some(V(ViewCommand::Effort(-1))),
            (KeyCode::Right, false) if menu => Some(V(ViewCommand::Effort(1))),
            (KeyCode::Backspace, false) if menu => Some(V(ViewCommand::MenuBackspace)),
            (KeyCode::Char(c), false) if menu && !alt => Some(V(ViewCommand::MenuInput(c))),
            _ => None,
        };
    }
    if let Some(id) = &screen.stop_pending {
        return match (key.code, ctrl) {
            (KeyCode::Char('y'), false) => Some(C(Command::StopWorker(id.clone()))),
            (KeyCode::Char('n'), false) | (KeyCode::Esc, false) => Some(V(ViewCommand::KeepWorker)),
            (KeyCode::Char('c'), true) => Some(C(Command::CancelOrQuit)),
            _ => None,
        };
    }
    let pane_shown =
        !screen.focus && (screen.pane_width != PaneWidth::Off || screen.ledger_overlay);
    match (key.code, ctrl, alt) {
        (KeyCode::Enter, false, false) | (KeyCode::Char('a'), false, false)
            if key.modifiers.is_empty()
                && screen.pane_focused
                && screen.pane_mode == PaneMode::Workers
                && screen.workers.focused.is_some() =>
        {
            Some(V(ViewCommand::AttachWorker))
        }
        (KeyCode::Char('x'), false, false)
            if key.modifiers.is_empty()
                && screen.pane_focused
                && screen.pane_mode == PaneMode::Workers
                && (screen.attached.is_some() || screen.workers.focused.is_some()) =>
        {
            Some(V(ViewCommand::AskStopWorker))
        }
        (KeyCode::Enter, false, false) => {
            let text = screen.composer.text.clone();
            if text.trim().is_empty() {
                None
            } else if screen.working.is_some() {
                Some(C(Command::QueueSteering(text)))
            } else {
                Some(C(Command::Submit(text)))
            }
        }
        (KeyCode::Enter, false, true) => {
            // SPEC §7: ⌥⏎ is `newline` idle, `queue follow-up` while working.
            if screen.working.is_some() {
                let text = screen.composer.text.clone();
                (!text.trim().is_empty()).then_some(C(Command::QueueFollowUp(text)))
            } else {
                Some(C(Command::Newline))
            }
        }
        (KeyCode::Char('c'), true, false) => Some(C(Command::CancelOrQuit)),
        (KeyCode::Char('o'), true, false) => Some(C(Command::OpenFold)),
        (KeyCode::Char('r'), true, false) => Some(C(Command::ExpandReasoning)),
        (KeyCode::Char('w'), true, false) => Some(C(Command::CyclePaneWidth)),
        (KeyCode::Char('p'), true, false) => Some(C(Command::TogglePin)),
        (KeyCode::Char('l'), true, false) => Some(C(Command::ToggleLedgerOverlay)),
        (KeyCode::Char('g'), true, false) => Some(V(ViewCommand::EditGoal)),
        (KeyCode::Char('u'), true, false) if !screen.pane_focused => {
            Some(V(ViewCommand::DeleteToLineStart))
        }
        (KeyCode::Char('k'), true, false) if !screen.pane_focused => {
            Some(V(ViewCommand::DeleteToLineEnd))
        }
        (KeyCode::Home, false, false) if !screen.pane_focused => Some(V(ViewCommand::LineStart)),
        (KeyCode::End, false, false) if !screen.pane_focused => Some(V(ViewCommand::LineEnd)),
        (KeyCode::Delete, false, false) if !screen.pane_focused => {
            Some(V(ViewCommand::DeleteForward))
        }
        (KeyCode::Char('f'), true, false) if pane_shown => Some(V(ViewCommand::TogglePaneFocus)),
        // Most terminals send plain `Tab` for `^Tab`; `^N` always arrives.
        (KeyCode::Tab, true, false) | (KeyCode::Char('n'), true, false) => {
            Some(C(Command::CyclePaneMode))
        }
        (KeyCode::PageUp, false, false) => Some(V(ViewCommand::PageUp)),
        (KeyCode::PageDown, false, false) => Some(V(ViewCommand::PageDown)),
        // esc order (menus and overlays were handled above): goal edit → ledger overlay →
        // attached worker → pane focus → scrolled view.
        (KeyCode::Esc, false, false) => {
            if screen.composer.editing_goal() {
                Some(V(ViewCommand::KeepComposer))
            } else if screen.ledger_overlay {
                Some(C(Command::ToggleLedgerOverlay))
            } else if screen.attached.is_some() {
                Some(V(ViewCommand::DetachWorker))
            } else if screen.pane_focused {
                Some(V(ViewCommand::TogglePaneFocus))
            } else if screen.scroll_top.is_some() {
                Some(V(ViewCommand::LiveTail))
            } else {
                None
            }
        }
        (KeyCode::Up, false, false) if screen.pane_focused => Some(C(Command::PaneUp)),
        (KeyCode::Down, false, false) if screen.pane_focused => Some(C(Command::PaneDown)),
        (KeyCode::Up, false, false)
            if !screen.composer.history.is_empty()
                && screen.working.is_none()
                && (screen.composer.text.is_empty() || screen.composer.browsing_history()) =>
        {
            Some(V(ViewCommand::HistoryPrev))
        }
        (KeyCode::Down, false, false)
            if screen.working.is_none() && screen.composer.browsing_history() =>
        {
            Some(V(ViewCommand::HistoryNext))
        }
        (KeyCode::Char('/'), false, false) if screen.composer.text.is_empty() => {
            Some(V(ViewCommand::OpenCompletion))
        }
        (KeyCode::Char(c), false, _) => Some(C(Command::Insert(c))),
        (KeyCode::Backspace, false, false) => Some(C(Command::Backspace)),
        (KeyCode::Left, false, false) => Some(C(Command::Left)),
        (KeyCode::Right, false, false) => Some(C(Command::Right)),
        _ => None,
    }
}

// Mouse routing is adapted from `iris-donor/src/ui/tui_loop.rs`
// (`pager_wheel`) at 5b04a1ad3412ad0bb663b6355f77a024aec0ddfa (MIT).
/// Decide a wheel event against the regions recorded by the last frame.
pub fn decide_mouse(screen: &Screen, event: MouseEvent) -> Option<Action> {
    if screen.mouse_off {
        return None;
    }
    let ticks = match event.kind {
        MouseEventKind::ScrollUp => 1,
        MouseEventKind::ScrollDown => -1,
        _ => return None,
    };
    let command = if screen.hits.review {
        ViewCommand::WheelReview(ticks)
    } else if screen
        .hits
        .pane
        .contains(ratatui::layout::Position::new(event.column, event.row))
    {
        match screen.hits.pane_mode {
            PaneMode::Output => ViewCommand::WheelPane(ticks),
            PaneMode::Workers
                if screen.pane_focused
                    && screen.approval.is_none()
                    && screen.picker.is_none()
                    && screen.status.is_none()
                    && screen.stop_pending.is_none() =>
            {
                ViewCommand::WheelPane(ticks)
            }
            PaneMode::Workers | PaneMode::Diff | PaneMode::Ledger => return None,
        }
    } else {
        ViewCommand::WheelTranscript(ticks)
    };
    Some(Action::View(command))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::render::diff::DiffView;
    use crate::render::permission::PermissionView;
    use crate::render::picker::{Picker, PickerGroup, PickerRow};

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn ctrl(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
    }

    fn view(command: ViewCommand) -> Option<Action> {
        Some(Action::View(command))
    }

    fn command(command: Command) -> Option<Action> {
        Some(Action::Command(command))
    }

    fn working() -> Option<crate::state::Working> {
        Some(crate::state::Working {
            label: "shell".into(),
            started_ms: 0,
        })
    }

    fn diff_approval() -> Option<Approval> {
        Some(Approval::Diff(DiffView {
            tool: "edit".into(),
            file: "src/x.rs".into(),
            summary: "replace exact string · once".into(),
            position: (1, 1),
            rows: vec![],
            grantable: true,
        }))
    }

    fn permission_approval() -> Option<Approval> {
        Some(Approval::Permission(PermissionView {
            command: "rm -rf /".into(),
            rows: vec![],
            grantable: true,
        }))
    }

    fn menu() -> Option<Picker> {
        Some(Picker {
            groups: vec![PickerGroup {
                rows: vec![PickerRow {
                    label: "a".into(),
                    ..PickerRow::default()
                }],
                ..PickerGroup::default()
            }],
            ..Picker::default()
        })
    }

    #[test]
    fn enter_sends_idle_and_queues_steering_while_working() {
        let mut s = Screen::default();
        s.composer.text = "fix it".into();
        assert_eq!(
            handle(&s, key(KeyCode::Enter)),
            Some(Command::Submit("fix it".into()))
        );
        s.working = working();
        assert_eq!(
            handle(&s, key(KeyCode::Enter)),
            Some(Command::QueueSteering("fix it".into()))
        );
        // An empty composer never sends.
        s.composer.text.clear();
        assert_eq!(handle(&s, key(KeyCode::Enter)), None);
    }

    #[test]
    fn alt_enter_is_a_newline_idle_and_a_follow_up_while_working() {
        let mut s = Screen::default();
        s.composer.text = "fix it".into();
        let alt_enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::ALT);
        assert_eq!(handle(&s, alt_enter), Some(Command::Newline));
        s.working = working();
        assert_eq!(
            handle(&s, alt_enter),
            Some(Command::QueueFollowUp("fix it".into()))
        );
    }

    #[test]
    fn ctrl_c_cancels_in_every_layer() {
        let mut s = Screen::default();
        assert_eq!(decide(&s, ctrl('c')), command(Command::CancelOrQuit));
        s.picker = menu();
        assert_eq!(decide(&s, ctrl('c')), command(Command::CancelOrQuit));
        s.picker = None;
        s.approval = diff_approval();
        assert_eq!(decide(&s, ctrl('c')), command(Command::CancelOrQuit));
    }

    #[test]
    fn slash_in_an_empty_composer_opens_completion_and_otherwise_types() {
        let mut s = Screen::default();
        assert_eq!(
            decide(&s, key(KeyCode::Char('/'))),
            view(ViewCommand::OpenCompletion)
        );
        // Until the driver applies view commands the slash still types.
        assert_eq!(
            handle(&s, key(KeyCode::Char('/'))),
            Some(Command::Insert('/'))
        );
        s.composer.text = "a".into();
        assert_eq!(
            decide(&s, key(KeyCode::Char('/'))),
            command(Command::Insert('/'))
        );
    }

    #[test]
    fn the_menu_layer_moves_completes_steps_effort_filters_and_dismisses() {
        let s = Screen {
            picker: menu(),
            ..Default::default()
        };
        assert_eq!(handle(&s, key(KeyCode::Down)), Some(Command::PickerDown));
        assert_eq!(handle(&s, key(KeyCode::Up)), Some(Command::PickerUp));
        assert_eq!(handle(&s, key(KeyCode::Enter)), Some(Command::PickerAccept));
        assert_eq!(handle(&s, key(KeyCode::Esc)), Some(Command::Dismiss));
        assert_eq!(decide(&s, key(KeyCode::Tab)), view(ViewCommand::Complete));
        assert_eq!(
            decide(&s, key(KeyCode::Left)),
            view(ViewCommand::Effort(-1))
        );
        assert_eq!(
            decide(&s, key(KeyCode::Right)),
            view(ViewCommand::Effort(1))
        );
        assert_eq!(
            decide(&s, key(KeyCode::Char('m'))),
            view(ViewCommand::MenuInput('m'))
        );
        assert_eq!(
            decide(&s, key(KeyCode::Backspace)),
            view(ViewCommand::MenuBackspace)
        );
    }

    #[test]
    fn non_grantable_permission_ignores_session_and_project_grants() {
        let mut s = Screen {
            approval: permission_approval(),
            ..Default::default()
        };
        let Some(Approval::Permission(permission_view)) = &mut s.approval else {
            unreachable!()
        };
        permission_view.grantable = false;
        s.composer.text = "draft".into();

        assert_eq!(decide(&s, key(KeyCode::Char('a'))), None);
        assert_eq!(decide(&s, key(KeyCode::Char('p'))), None);
        assert_eq!(
            decide(&s, key(KeyCode::Char('y'))),
            command(Command::ApproveOnce)
        );
        assert_eq!(decide(&s, key(KeyCode::Char('n'))), command(Command::Deny));

        // An ignored decision stays modal and cannot leak into the composer.
        let before = s.composer.text.clone();
        assert_eq!(handle(&s, key(KeyCode::Char('a'))), None);
        assert_eq!(handle(&s, key(KeyCode::Char('p'))), None);
        assert_eq!(s.composer.text, before);
    }

    #[test]
    fn non_grantable_diff_ignores_grants_but_keeps_review_keys() {
        let mut s = Screen {
            approval: diff_approval(),
            ..Default::default()
        };
        let Some(Approval::Diff(diff_view)) = &mut s.approval else {
            unreachable!()
        };
        diff_view.grantable = false;

        assert_eq!(decide(&s, key(KeyCode::Char('a'))), None);
        assert_eq!(decide(&s, key(KeyCode::Char('p'))), None);
        assert_eq!(decide(&s, ctrl('d')), view(ViewCommand::ToggleReview));
        s.review.open = true;
        assert_eq!(
            decide(&s, key(KeyCode::Tab)),
            view(ViewCommand::ReviewFile(1))
        );
    }

    #[test]
    fn decision_keys_only_while_a_decision_is_shown() {
        let mut s = Screen::default();
        assert_eq!(
            decide(&s, key(KeyCode::Char('y'))),
            command(Command::Insert('y'))
        );
        s.approval = permission_approval();
        for (c, expected) in [
            ('y', Command::ApproveOnce),
            ('a', Command::ApproveSession),
            ('p', Command::ApproveProject),
            ('n', Command::Deny),
        ] {
            assert_eq!(decide(&s, key(KeyCode::Char(c))), command(expected));
        }
        // Composer keys do not leak through the decision.
        assert_eq!(decide(&s, key(KeyCode::Char('w'))), None);
        // ^D reviews diffs only; ^A is removed.
        assert_eq!(decide(&s, ctrl('d')), None);
        assert_eq!(decide(&s, ctrl('a')), None);
        s.approval = diff_approval();
        assert_eq!(decide(&s, ctrl('d')), view(ViewCommand::ToggleReview));
        assert_eq!(decide(&s, ctrl('a')), None);
    }

    #[test]
    fn the_full_review_pages_files_and_scrolls_its_body() {
        let mut s = Screen {
            approval: diff_approval(),
            ..Default::default()
        };
        // Inline: tab does nothing, PgUp scrolls the transcript.
        assert_eq!(decide(&s, key(KeyCode::Tab)), None);
        assert_eq!(decide(&s, key(KeyCode::PageUp)), view(ViewCommand::PageUp));
        s.review.open = true;
        assert_eq!(
            decide(&s, key(KeyCode::Tab)),
            view(ViewCommand::ReviewFile(1))
        );
        assert_eq!(
            decide(&s, KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT)),
            view(ViewCommand::ReviewFile(-1))
        );
        assert_eq!(
            decide(&s, key(KeyCode::PageUp)),
            view(ViewCommand::ReviewPage(1))
        );
        assert_eq!(
            decide(&s, key(KeyCode::PageDown)),
            view(ViewCommand::ReviewPage(-1))
        );
        assert_eq!(decide(&s, ctrl('d')), view(ViewCommand::ToggleReview));
    }

    #[test]
    fn history_arrows_recall_only_an_empty_idle_composer() {
        let mut s = Screen::default();
        assert_eq!(decide(&s, key(KeyCode::Up)), None);
        s.composer.text = "one".into();
        s.composer.take();
        assert_eq!(decide(&s, key(KeyCode::Down)), None);
        assert_eq!(decide(&s, key(KeyCode::Up)), view(ViewCommand::HistoryPrev));
        s.apply_view(ViewCommand::HistoryPrev);
        assert_eq!(decide(&s, key(KeyCode::Up)), view(ViewCommand::HistoryPrev));
        assert_eq!(
            decide(&s, key(KeyCode::Down)),
            view(ViewCommand::HistoryNext)
        );

        s.composer.text = "draft".into();
        s.composer.take();
        s.composer.text = "draft".into();
        assert_eq!(decide(&s, key(KeyCode::Up)), None);
        s.composer.text.clear();
        s.working = working();
        assert_eq!(decide(&s, key(KeyCode::Up)), None);
        assert_eq!(decide(&s, key(KeyCode::Down)), None);
    }

    #[test]
    fn history_recall_keeps_pane_picker_and_approval_precedence() {
        let mut s = Screen {
            pane_focused: true,
            ..Screen::default()
        };
        s.composer.text = "one".into();
        s.composer.take();
        assert_eq!(decide(&s, key(KeyCode::Up)), command(Command::PaneUp));
        assert_eq!(decide(&s, key(KeyCode::Down)), command(Command::PaneDown));

        s.picker = menu();
        assert_eq!(decide(&s, key(KeyCode::Up)), command(Command::PickerUp));
        assert_eq!(decide(&s, key(KeyCode::Down)), command(Command::PickerDown));
        s.picker = None;
        s.approval = permission_approval();
        assert_eq!(
            decide(&s, key(KeyCode::Up)),
            None,
            "the approval remains modal"
        );
        assert_eq!(
            decide(&s, key(KeyCode::Char('y'))),
            command(Command::ApproveOnce)
        );
    }

    #[test]
    fn page_keys_scroll_the_transcript() {
        let s = Screen::default();
        assert_eq!(decide(&s, key(KeyCode::PageUp)), view(ViewCommand::PageUp));
        assert_eq!(
            decide(&s, key(KeyCode::PageDown)),
            view(ViewCommand::PageDown)
        );
        assert_eq!(handle(&s, key(KeyCode::PageUp)), Some(Command::ScrollUp));
    }

    #[test]
    fn esc_unwinds_menu_goal_edit_overlay_pane_focus_then_scroll() {
        let mut s = Screen {
            picker: menu(),
            ledger_overlay: true,
            pane_focused: true,
            scroll_top: Some(0),
            ..Default::default()
        };
        s.composer.begin_goal_edit(None);
        let esc = key(KeyCode::Esc);
        assert_eq!(decide(&s, esc), command(Command::Dismiss));
        s.picker = None;
        assert_eq!(decide(&s, esc), view(ViewCommand::KeepComposer));
        s.composer.keep();
        assert_eq!(decide(&s, esc), command(Command::ToggleLedgerOverlay));
        s.ledger_overlay = false;
        assert_eq!(decide(&s, esc), view(ViewCommand::TogglePaneFocus));
        s.pane_focused = false;
        assert_eq!(decide(&s, esc), view(ViewCommand::LiveTail));
        s.scroll_top = None;
        assert_eq!(decide(&s, esc), None);
    }

    #[test]
    fn ctrl_n_is_the_always_working_alias_of_ctrl_tab() {
        let s = Screen::default();
        assert_eq!(decide(&s, ctrl('n')), command(Command::CyclePaneMode));
        assert_eq!(
            decide(&s, KeyEvent::new(KeyCode::Tab, KeyModifiers::CONTROL)),
            command(Command::CyclePaneMode)
        );
    }

    #[test]
    fn ctrl_f_focuses_a_shown_pane_and_the_arrows_follow_it() {
        let mut s = Screen::default();
        assert_eq!(decide(&s, ctrl('f')), view(ViewCommand::TogglePaneFocus));
        assert_eq!(decide(&s, key(KeyCode::Up)), None);
        s.pane_focused = true;
        assert_eq!(decide(&s, key(KeyCode::Up)), command(Command::PaneUp));
        assert_eq!(decide(&s, key(KeyCode::Down)), command(Command::PaneDown));
        // No pane in focus mode: nothing to focus.
        s.focus = true;
        assert_eq!(decide(&s, ctrl('f')), None);
    }

    #[test]
    fn composer_line_keys_are_view_commands_idle_and_while_working() {
        let s = Screen::default();
        assert_eq!(decide(&s, ctrl('u')), view(ViewCommand::DeleteToLineStart));
        assert_eq!(decide(&s, ctrl('k')), view(ViewCommand::DeleteToLineEnd));
        assert_eq!(decide(&s, key(KeyCode::Home)), view(ViewCommand::LineStart));
        assert_eq!(decide(&s, key(KeyCode::End)), view(ViewCommand::LineEnd));
        assert_eq!(
            decide(&s, key(KeyCode::Delete)),
            view(ViewCommand::DeleteForward)
        );

        let working = Screen {
            working: working(),
            ..Screen::default()
        };
        assert_eq!(
            decide(&working, ctrl('u')),
            view(ViewCommand::DeleteToLineStart)
        );
        assert_eq!(
            decide(&working, ctrl('k')),
            view(ViewCommand::DeleteToLineEnd)
        );
        assert_eq!(
            decide(&working, key(KeyCode::Home)),
            view(ViewCommand::LineStart)
        );
        assert_eq!(
            decide(&working, key(KeyCode::End)),
            view(ViewCommand::LineEnd)
        );
        assert_eq!(
            decide(&working, key(KeyCode::Delete)),
            view(ViewCommand::DeleteForward)
        );
        let mut goal = Screen::default();
        goal.composer.begin_goal_edit(None);
        assert_eq!(
            decide(&goal, ctrl('u')),
            view(ViewCommand::DeleteToLineStart)
        );
        assert_eq!(decide(&goal, ctrl('k')), view(ViewCommand::DeleteToLineEnd));
        assert_eq!(
            decide(&goal, key(KeyCode::Home)),
            view(ViewCommand::LineStart)
        );
        assert_eq!(decide(&goal, key(KeyCode::End)), view(ViewCommand::LineEnd));
        assert_eq!(
            decide(&goal, key(KeyCode::Delete)),
            view(ViewCommand::DeleteForward)
        );
    }

    #[test]
    fn composer_line_keys_keep_modal_layers_and_non_subset_keys_unchanged() {
        let approval = Screen {
            approval: permission_approval(),
            ..Screen::default()
        };
        let picker = Screen {
            picker: menu(),
            ..Screen::default()
        };
        let focused = Screen {
            pane_focused: true,
            ..Screen::default()
        };
        for screen in [&approval, &picker, &focused] {
            assert_eq!(decide(screen, ctrl('u')), None);
            assert_eq!(decide(screen, ctrl('k')), None);
            assert_eq!(decide(screen, key(KeyCode::Home)), None);
            assert_eq!(decide(screen, key(KeyCode::End)), None);
            assert_eq!(decide(screen, key(KeyCode::Delete)), None);
        }

        let s = Screen::default();
        assert_eq!(decide(&s, ctrl('w')), command(Command::CyclePaneWidth));
        assert_eq!(decide(&s, ctrl('r')), command(Command::ExpandReasoning));
        assert_eq!(decide(&s, ctrl('o')), command(Command::OpenFold));
        assert_eq!(decide(&s, ctrl('p')), command(Command::TogglePin));
        assert_eq!(decide(&s, ctrl('g')), view(ViewCommand::EditGoal));
        assert_eq!(decide(&s, ctrl('f')), view(ViewCommand::TogglePaneFocus));
        assert_eq!(decide(&s, ctrl('a')), None);
        assert_eq!(decide(&s, ctrl('d')), None);
    }

    #[test]
    fn ctrl_g_edits_the_goal_prefilled() {
        let mut s = Screen {
            goal: Some("fix the stall".into()),
            ..Default::default()
        };
        s.composer.text = "draft".into();
        assert_eq!(decide(&s, ctrl('g')), view(ViewCommand::EditGoal));
        assert_eq!(handle(&s, ctrl('g')), Some(Command::EditGoal));
        s.apply_view(ViewCommand::EditGoal);
        assert_eq!(s.composer.text, "/goal fix the stall");
        assert_eq!(s.composer.cursor, s.composer.text.chars().count());
        s.apply_view(ViewCommand::KeepComposer);
        assert_eq!(s.composer.text, "draft");
    }

    #[test]
    fn the_existing_commands_keep_their_keys() {
        let s = Screen::default();
        for (c, expected) in [
            ('o', Command::OpenFold),
            ('r', Command::ExpandReasoning),
            ('w', Command::CyclePaneWidth),
            ('p', Command::TogglePin),
            ('l', Command::ToggleLedgerOverlay),
        ] {
            assert_eq!(handle(&s, ctrl(c)), Some(expected));
        }
        assert_eq!(
            handle(&s, key(KeyCode::Backspace)),
            Some(Command::Backspace)
        );
        assert_eq!(handle(&s, key(KeyCode::Left)), Some(Command::Left));
        assert_eq!(handle(&s, key(KeyCode::Right)), Some(Command::Right));
        // Until the driver applies `^F`, an open OUTPUT pane keeps scrolling on the arrows.
        let s = Screen {
            output: Some(crate::render::output::OutputView {
                id: crate::fold::FoldId("h-1".into()),
                lines: vec![],
                scroll: 0,
            }),
            ..Default::default()
        };
        assert_eq!(handle(&s, key(KeyCode::Down)), Some(Command::PaneDown));
    }

    #[test]
    fn worker_focus_attaches_with_enter_or_a_but_unfocused_composer_keys_are_unchanged() {
        let mut s = Screen {
            pane_focused: true,
            pane_mode: PaneMode::Workers,
            ..Default::default()
        };
        s.workers.focused = Some("w2".into());

        assert_eq!(
            decide(&s, key(KeyCode::Enter)),
            view(ViewCommand::AttachWorker)
        );
        assert_eq!(
            decide(&s, key(KeyCode::Char('a'))),
            view(ViewCommand::AttachWorker)
        );

        s.pane_focused = false;
        s.composer.text = "hi".into();
        assert_eq!(
            decide(&s, key(KeyCode::Enter)),
            command(Command::Submit("hi".into()))
        );
        assert_eq!(
            decide(&s, key(KeyCode::Char('a'))),
            command(Command::Insert('a'))
        );
    }

    #[test]
    fn worker_focus_asks_before_stopping_and_unfocused_x_types() {
        let mut s = Screen {
            pane_focused: true,
            pane_mode: PaneMode::Workers,
            ..Default::default()
        };
        s.workers.focused = Some("w2".into());

        assert_eq!(
            decide(&s, key(KeyCode::Char('x'))),
            view(ViewCommand::AskStopWorker)
        );
        s.pane_focused = false;
        assert_eq!(
            decide(&s, key(KeyCode::Char('x'))),
            command(Command::Insert('x'))
        );
    }

    #[test]
    fn the_pending_worker_stop_is_modal() {
        let s = Screen {
            stop_pending: Some("w2".into()),
            ..Default::default()
        };
        assert_eq!(
            decide(&s, key(KeyCode::Char('y'))),
            command(Command::StopWorker("w2".into()))
        );
        for code in [KeyCode::Char('n'), KeyCode::Esc] {
            assert_eq!(decide(&s, key(code)), view(ViewCommand::KeepWorker));
        }
        assert_eq!(decide(&s, key(KeyCode::Char('a'))), None);
        assert_eq!(decide(&s, ctrl('c')), command(Command::CancelOrQuit));
    }

    #[test]
    fn an_approval_wins_over_a_pending_worker_stop() {
        let s = Screen {
            approval: permission_approval(),
            stop_pending: Some("w2".into()),
            ..Default::default()
        };
        assert_eq!(
            decide(&s, key(KeyCode::Char('y'))),
            command(Command::ApproveOnce)
        );
    }

    #[test]
    fn esc_detaches_after_the_ledger_overlay_and_before_pane_focus() {
        let mut s = Screen {
            attached: Some(crate::state::AttachedWorker {
                id: "w2".into(),
                route: "route".into(),
                state: crate::render::workers::BlockState::Running,
                transcript: crate::transcript::Transcript::new(),
            }),
            ledger_overlay: true,
            pane_focused: true,
            ..Default::default()
        };

        assert_eq!(
            decide(&s, key(KeyCode::Esc)),
            command(Command::ToggleLedgerOverlay)
        );
        s.ledger_overlay = false;
        assert_eq!(
            decide(&s, key(KeyCode::Esc)),
            view(ViewCommand::DetachWorker)
        );
        s.attached = None;
        assert_eq!(
            decide(&s, key(KeyCode::Esc)),
            view(ViewCommand::TogglePaneFocus)
        );
    }
}
