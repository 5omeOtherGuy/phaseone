//! The TUI front end (issue #12): implements the host's [`FrontEnd`] seam —
//! event observation through `TuiSink`, authorization through `TuiPolicy`, and
//! the run loop below. The UI itself is `p1-tui`'s pure state machine; this
//! module is wiring: crossterm keys in, agent events in, authorization
//! questions parked on the screen, answers back.
//!
//! The seam hands `run` a `&mut Agent`, so turns are driven by a pinned future
//! inside the select loop (`pump`): the UI keeps reading keys and events while
//! a turn runs, and the turn future is never dropped mid-flight (the iris
//! harness-actor lesson, ADR-0060, adapted to a borrowed agent).

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Arc, Mutex};

use futures_util::StreamExt;
use p1_contracts::{
    AuthorizationPolicy, CancellationToken, Decision, EventSink, InboxKind, TurnEnd,
};
use p1_core::Agent;
use p1_tui::input::{self, Command};
use p1_tui::render::diff::DiffView;
use p1_tui::render::permission::PermissionView;
use p1_tui::runtime::{AuthRequest, TerminalGuard, TuiPolicy, TuiSink, UiEvent};
use p1_tui::state::{Approval, Screen};
use ratatui::backend::Backend;
use tokio::sync::mpsc;

use crate::HostDeps;
use crate::activity::Completion;
use crate::cli::Options;
use crate::frontend::{FrontEnd, WorkerService};
use crate::run::StallGuard;

/// What the host knows at construction time. The resolved route and model
/// arrive later, through [`FrontEnd::parent_assembled`].
pub struct TuiOptions {
    pub env: String,
    /// ADR-0038: full access is the default; prompts appear only under --ask.
    pub ask: bool,
    /// The workspace root: approval diff views read files from here.
    pub workspace: std::path::PathBuf,
    /// The shell sandbox posture, shown in §4.5 permission prompts.
    pub sandbox: String,
    /// The journal being resumed, if any: the transcript is painted from its
    /// records (what the operator saw), not from the model-visible history.
    pub resume_from: Option<std::path::PathBuf>,
    /// The session journal, named on exit with how to resume it.
    pub session: Option<std::path::PathBuf>,
}

/// The TUI front end. Created before the agent so its sink and policy install
/// into `AgentParts`; the run loop takes over in [`FrontEnd::run`].
pub struct TuiFrontEnd {
    options: TuiOptions,
    sink: Arc<TuiSink>,
    policy: Arc<TuiPolicy>,
    events: Mutex<Option<mpsc::UnboundedReceiver<UiEvent>>>,
    auth: Mutex<Option<mpsc::UnboundedReceiver<AuthRequest>>>,
    /// (route, model), announced by the host once assembly has happened.
    labels: Mutex<Option<(String, String)>>,
}

impl TuiFrontEnd {
    pub fn new(options: TuiOptions, cancel: CancellationToken) -> Self {
        let (sink, events) = TuiSink::new();
        let (policy, auth) = TuiPolicy::new(options.ask, cancel.clone());
        Self {
            options,
            sink: Arc::new(sink),
            policy: Arc::new(policy),
            events: Mutex::new(Some(events)),
            auth: Mutex::new(Some(auth)),
            labels: Mutex::new(None),
        }
    }
}

impl FrontEnd for TuiFrontEnd {
    fn event_sink(&self) -> Arc<dyn EventSink> {
        self.sink.clone()
    }

    fn child_event_sink(&self, worker_id: &str, _route: &str, _model: &str) -> Arc<dyn EventSink> {
        Arc::new(self.sink.child(worker_id))
    }

    fn child_started(&self, worker_id: &str) {
        self.sink.worker_started(worker_id);
    }

    fn authorization(&self) -> Arc<dyn AuthorizationPolicy> {
        self.policy.clone()
    }

    fn parent_assembled(&self, route: &str, model: &str, _completion: Option<Completion>) {
        *self.labels.lock().unwrap() = Some((route.to_string(), model.to_string()));
    }

    /// A TUI is interactive by definition: the host's §3c stall guard and the
    /// headless drivers never apply.
    fn is_headless(&self, _options: &Options) -> bool {
        false
    }

    fn run<'a>(
        &'a self,
        deps: &'a HostDeps,
        agent: &'a mut Agent,
        cancel: &'a CancellationToken,
        workers: Option<Arc<dyn WorkerService>>,
        _stall: Option<Arc<StallGuard>>,
    ) -> p1_contracts::BoxFuture<'a, i32> {
        let mut events = self.events.lock().unwrap().take();
        let mut auth_rx = self.auth.lock().unwrap().take();
        Box::pin(async move {
            let Ok(_guard) = TerminalGuard::enter() else {
                eprintln!("p1 --tui: could not enter the alternate screen");
                return 1;
            };
            // One write per frame (not 1 KiB line-writer chunks, and never a
            // clear on its own).
            let backend = ratatui::backend::CrosstermBackend::new(FrameOut::default());
            let mut terminal = match ratatui::Terminal::new(backend) {
                Ok(terminal) => terminal,
                Err(error) => {
                    eprintln!("p1 --tui: {error}");
                    return 1;
                }
            };

            let mut screen = Screen::new(std::env::var_os("P1_REDUCED_MOTION").is_some());
            let (route, model) = self.labels.lock().unwrap().clone().unwrap_or_default();
            screen.legacy_keyboard = !_guard.keyboard_enhanced;
            screen.color_mode = p1_tui::palette::ColorMode::from_env();
            screen.model = model.clone();
            if let Ok(Ok(output)) = tokio::time::timeout(
                std::time::Duration::from_secs(1),
                tokio::process::Command::new("git")
                    .args(["branch", "--show-current"])
                    .current_dir(&self.options.workspace)
                    .output(),
            )
            .await
                && output.status.success()
            {
                screen.branch = String::from_utf8_lossy(&output.stdout).trim().to_owned();
            }
            // The repository's name (its top level), the workspace folder otherwise.
            let toplevel = tokio::time::timeout(
                std::time::Duration::from_secs(1),
                tokio::process::Command::new("git")
                    .args(["rev-parse", "--show-toplevel"])
                    .current_dir(&self.options.workspace)
                    .output(),
            )
            .await
            .ok()
            .and_then(Result::ok)
            .filter(|o| o.status.success())
            .map(|o| std::path::PathBuf::from(String::from_utf8_lossy(&o.stdout).trim()));
            screen.asking = self.options.ask;
            screen.repo = toplevel
                .as_deref()
                .unwrap_or(&self.options.workspace)
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned();
            if let Ok(environment) =
                p1_assembly::load_environment(&self.options.env, &deps.environment_dirs)
            {
                screen.effort = environment
                    .options
                    .reasoning_effort
                    .or_else(|| environment.profile.as_ref().and_then(|p| p.default_effort))
                    .map(|e| {
                        match e {
                            p1_contracts::Effort::Low => "low",
                            p1_contracts::Effort::Medium => "medium",
                            p1_contracts::Effort::High => "high",
                            p1_contracts::Effort::ExtraHigh => "extra_high",
                            p1_contracts::Effort::Max => "max",
                        }
                        .to_owned()
                    })
                    .unwrap_or_default();
                screen.context_capacity = environment
                    .context
                    .as_ref()
                    .map(|c| (c.window_tokens, c.summarize_at_tokens))
                    .or_else(|| {
                        environment
                            .profile
                            .as_ref()
                            .and_then(|p| p.context_tokens)
                            // No context policy states a warn point: unknown (0).
                            .map(|w| (w, 0))
                    });
            }
            screen.env = self.options.env.clone();
            screen.route = route.clone();
            // A resumed session shows where it stands: painted from the journal
            // when it can be read, from the projected history otherwise.
            let records = if agent.history().is_empty() {
                None
            } else {
                self.options
                    .resume_from
                    .as_deref()
                    .and_then(|path| p1_journal::load(path).ok())
                    .map(|loaded| loaded.records)
            };
            match &records {
                Some(records) => {
                    let usage = screen.transcript.replay(records);
                    // Spend is rebuilt from the recorded responses; a response
                    // that reported no usage keeps its part unknown.
                    for u in &usage {
                        screen.spend.record(u.as_ref());
                    }
                    screen.input_history = records
                        .iter()
                        .filter_map(|r| match &r.body {
                            p1_contracts::RecordBody::UserInput { text } => Some(text.clone()),
                            p1_contracts::RecordBody::Inbox {
                                kind: InboxKind::Steering,
                                text,
                            } => Some(text.clone()),
                            _ => None,
                        })
                        .collect();
                }
                None => {
                    screen.transcript.paint_history(agent.history());
                    screen.input_history = agent
                        .history()
                        .iter()
                        .filter_map(|item| match item {
                            p1_contracts::Item::User { text } => Some(text.clone()),
                            _ => None,
                        })
                        .collect();
                    if !agent.history().is_empty() {
                        // Projected history has no usage records. Never show this
                        // process's partial spend as the resumed session's total.
                        screen.spend = p1_tui::state::Spend {
                            input: None,
                            output: None,
                            cached: None,
                            cost_micro_usd: None,
                            responses: 0,
                        };
                    }
                }
            }
            let worker_rows: Arc<Mutex<Vec<p1_tui::render::workers::WorkerRow>>> =
                Arc::new(Mutex::new(Vec::new()));
            #[cfg(feature = "delegation")]
            if let Some(service) = &workers {
                spawn_worker_refresher(service.clone(), worker_rows.clone(), cancel.child_token());
            }
            let mut driver = Driver {
                screen,
                env: self.options.env.clone(),
                route,
                model,
                workspace: self.options.workspace.clone(),
                sandbox: self.options.sandbox.clone(),
                policy: self.policy.clone(),
                pending_auth: VecDeque::new(),
                pinned_by_approval: false,
                follow_ups: VecDeque::new(),
                submit_pending: None,
                pending_calls: HashMap::new(),
                task_files: HashSet::new(),
                task_added: 0,
                task_removed: 0,
                exit: None,
                inbox: agent.inbox(),
                worker_rows,
                _workers: workers,
                now_ms: 0,
                last_cancel_ms: None,
                filter_before: String::new(),
                scroll_before: 0,
                unknown_confirm: None,
                term_out: Box::new(std::io::stdout()),
                ask: self.options.ask,
                pending_decision: None,
                last_turn_end_ms: None,
                last_decision_ms: None,
                inbox_hold: false,
                exit_after_turn: false,
                ui_tx: Some(self.sink.sender()),
                reviewed: HashMap::new(),
            };
            // Terminals that send several wheel events per notch.
            if std::env::var("TERM_PROGRAM").is_ok_and(|t| t.eq_ignore_ascii_case("ghostty")) {
                driver.screen.wheel_step = 1;
            }

            // TASK starts at a known zero; a resumed journal's own edits count.
            if let Some(records) = &records {
                let mut calls = HashMap::new();
                for record in records {
                    match &record.body {
                        p1_contracts::RecordBody::AssistantCompleted { item, .. } => {
                            for call in item.tool_calls() {
                                calls.insert(call.call_id.clone(), call.clone());
                            }
                        }
                        p1_contracts::RecordBody::ToolFinished { result } => {
                            if let Some(call) = calls.remove(&result.call_id) {
                                driver.track_task(&p1_contracts::AgentEvent::ToolStarted { call });
                                driver.track_task(&p1_contracts::AgentEvent::ToolFinished {
                                    result: result.clone(),
                                });
                            }
                        }
                        _ => {}
                    }
                }
            }
            if driver.screen.task_view.is_none()
                && (records.is_some() || agent.history().is_empty())
            {
                driver.screen.task_view = Some(p1_tui::render::ledger::Task {
                    id: None,
                    files: Some(driver.task_files.len() as u64),
                    diff: Some((driver.task_added, driver.task_removed)),
                    journal: Some(if records.is_some() { "resumed" } else { "new" }.into()),
                });
            } else if let Some(task) = &mut driver.screen.task_view {
                task.journal = Some("resumed".into());
            }
            let keys = Box::pin(crossterm::event::EventStream::new().filter_map(
                |event| async move {
                    use crossterm::event::{Event, KeyEventKind};
                    match event {
                        Ok(Event::Key(key)) if key.kind != KeyEventKind::Release => {
                            Some(UiInput::Key(key))
                        }
                        Ok(Event::Mouse(mouse)) => {
                            use crossterm::event::MouseEventKind;
                            match mouse.kind {
                                MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
                                    Some(UiInput::Wheel {
                                        up: mouse.kind == MouseEventKind::ScrollUp,
                                        column: mouse.column,
                                        row: mouse.row,
                                        events: 1,
                                    })
                                }
                                MouseEventKind::Down(_) => Some(UiInput::Mouse(mouse)),
                                // Motion, drags and releases: nothing reads them,
                                // so they cost no frame.
                                _ => None,
                            }
                        }
                        Ok(Event::Paste(text)) => Some(UiInput::Paste(text)),
                        Ok(Event::Resize(w, _)) => Some(UiInput::Resize(w)),
                        _ => None,
                    }
                },
            ));
            let code = drive_loop(
                &mut terminal,
                &mut driver,
                agent,
                keys,
                events.take().expect("run once"),
                auth_rx.take().expect("run once"),
                cancel,
                &self.sink,
            )
            .await;
            // `/mouse` turned alternate scroll off; the terminal default
            // (on, in the terminals that have it) comes back for less and man.
            if driver.screen.mouse_released {
                use std::io::Write;
                let _ = driver.term_out.write_all(b"\x1b[?1007h");
                let _ = driver.term_out.flush();
            }
            code
        })
    }

    fn finish(&self) {
        // Nothing to total up: the TUI showed spend live, and the terminal
        // was restored when the run loop's guard dropped. The normal screen
        // is otherwise empty: say where the session is and how to return.
        if let Some(path) = self.options.session.as_ref().filter(|p| p.exists()) {
            eprintln!("p1: session journal {}", path.display());
            match resume_command(std::env::args().collect(), path) {
                Some(command) => eprintln!("p1: resume with: {command}"),
                None => eprintln!("p1: run the same command again to resume"),
            }
        }
    }
}

/// The command that resumes this session: the command line it was started
/// with (every environment, workspace, approval and sandbox flag kept) plus
/// `--resume` and the journal. `None` when this was not the `p1 --tui` CLI (a
/// test fixture): running it again resumes.
fn resume_command(args: Vec<String>, journal: &std::path::Path) -> Option<String> {
    if !args.iter().any(|a| a == "--tui") {
        return None;
    }
    let mut out: Vec<String> = vec![];
    let mut has_session = false;
    let mut args = args.into_iter();
    out.extend(args.next());
    for arg in args {
        if arg == "--resume" {
            continue;
        }
        has_session |= arg == "--session" || arg.starts_with("--session=");
        out.push(arg);
    }
    out.insert(1.min(out.len()), "--resume".into());
    if !has_session {
        out.push("--session".into());
        out.push(journal.display().to_string());
    }
    let quote = |a: &String| {
        if !a.is_empty()
            && a.chars()
                .all(|c| c.is_ascii_alphanumeric() || "-_./=:,+@%".contains(c))
        {
            a.clone()
        } else {
            format!("'{}'", a.replace('\'', r"'\''"))
        }
    };
    Some(out.iter().map(quote).collect::<Vec<_>>().join(" "))
}

/// The UI state plus the wiring the loop needs. No terminal in here: keys and
/// events enter through methods, so the whole driver is channel-testable.
pub(crate) struct Driver {
    screen: Screen,
    env: String,
    route: String,
    model: String,
    workspace: std::path::PathBuf,
    sandbox: String,
    policy: Arc<TuiPolicy>,
    /// Parked authorizations; the front one is on screen. Two workers can
    /// park at once (they share this policy) — a second request must QUEUE,
    /// never replace the one the operator is reading (its dropped answer
    /// would be a denial the operator never chose).
    pending_auth: VecDeque<AuthRequest>,
    /// Set when an approval pinned the pane, so deciding it releases only
    /// the pin it took — an operator's own `^P` survives the decision.
    pinned_by_approval: bool,
    /// Follow-ups fire only when the agent would otherwise stop (SPEC §7).
    follow_ups: VecDeque<String>,
    /// Task stats for the LEDGER's TASK section: call inputs arrive on
    /// `ToolStarted`; the counts settle on `ToolFinished`.
    pending_calls: HashMap<String, p1_contracts::ToolCall>,
    task_files: HashSet<String>,
    task_added: u64,
    task_removed: u64,
    exit: Option<i32>,
    /// A submitted prompt waiting for the loop to start the turn (the agent
    /// borrow lives in the loop, not in the driver).
    submit_pending: Option<String>,
    /// The worker snapshot the refresher task maintains (delegation only).
    worker_rows: Arc<Mutex<Vec<p1_tui::render::workers::WorkerRow>>>,
    inbox: p1_core::Inbox,
    _workers: Option<Arc<dyn WorkerService>>,
    /// The frontend clock at the latest input or event.
    now_ms: u64,
    /// When the last turn was cancelled: a ^C right after it does not quit.
    last_cancel_ms: Option<u64>,
    /// The filter and scroll to restore when an output filter edit is cancelled.
    filter_before: String,
    scroll_before: usize,
    /// An unknown `/word` noted once; the same Enter again sends it.
    unknown_confirm: Option<String>,
    /// Where terminal side effects go (OSC 52, mouse capture): stdout in the
    /// app, a sink in tests.
    term_out: Box<dyn std::io::Write + Send>,
    /// `--ask`: writes and commands wait for a decision.
    ask: bool,
    /// A decision key waiting a beat: a key right after it means the operator
    /// is typing a word, and both go to the draft instead (see `decide`). It
    /// names the call it answers, so it never decides a later one.
    pending_decision: Option<(char, u64, String)>,
    /// When a turn last ended: a reflexive ^C right after it does not quit.
    last_turn_end_ms: Option<u64>,
    /// When the operator last answered an approval: a lone `y` typed for the
    /// next one before it shows is not sent to the model.
    last_decision_ms: Option<u64>,
    /// After a cancel, worker notifications wait for the operator's next turn
    /// instead of starting one by themselves.
    inbox_hold: bool,
    /// `/exit` during a turn: cancel it, then leave.
    exit_after_turn: bool,
    /// Background work (a clipboard copy) reports back through the UI channel.
    ui_tx: Option<mpsc::UnboundedSender<UiEvent>>,
    /// Diff rows shown at approval time, by call id: the settled block and the
    /// TASK counts use the same rows (the file has changed once it ran).
    reviewed: HashMap<String, Vec<p1_tui::render::diff::DiffRow>>,
}

enum UiInput {
    Key(crossterm::event::KeyEvent),
    Paste(String),
    Mouse(crossterm::event::MouseEvent),
    /// Consecutive wheel events in one direction over one spot, coalesced.
    Wheel {
        up: bool,
        column: u16,
        row: u16,
        events: usize,
    },
    /// The terminal's new width (a width change re-wraps; heights do not).
    Resize(u16),
}
impl From<crossterm::event::KeyEvent> for UiInput {
    fn from(key: crossterm::event::KeyEvent) -> Self {
        Self::Key(key)
    }
}
impl UiInput {
    fn is_cancel(&self) -> bool {
        matches!(self,Self::Key(key) if is_cancel(key))
    }
}

impl Driver {
    fn on_input(&mut self, input: UiInput) -> bool {
        let changed = self.route_input(input);
        self.screen.sync_palette();
        // An unknown `/word` is confirmed by Enter on the same text, now —
        // not by the same word typed again much later.
        if self
            .unknown_confirm
            .as_deref()
            .is_some_and(|u| u != self.screen.composer.text)
        {
            self.unknown_confirm = None;
        }
        changed
    }

    fn route_input(&mut self, input: UiInput) -> bool {
        // Every input stamps the screen clock (a notice from a click lasts as
        // long as one from a key).
        self.screen.now_ms = self.now_ms;
        match input {
            UiInput::Key(key) => {
                self.on_key(key);
                true
            }
            UiInput::Mouse(mouse) => self.screen.on_mouse(mouse),
            UiInput::Wheel {
                up,
                column,
                row,
                events,
            } => {
                // One notch may arrive as several events (Ghostty sends three):
                // a burst scrolls like one notch, more events scroll further.
                let rows = self.screen.wheel_rows(events);
                self.screen.wheel(column, row, up, rows)
            }
            UiInput::Paste(text) => {
                self.paste(text);
                true
            }
            UiInput::Resize(_) => true,
        }
    }

    /// Paste is data: it is never dropped and never decides anything. It lands
    /// in the output filter while one is typed, otherwise in the draft (also
    /// the hidden draft under an approval).
    fn paste(&mut self, text: String) {
        let text = text.replace("\r\n", "\n").replace('\r', "\n");
        if let Some(picker) = &mut self.screen.picker {
            picker.filter.push_str(&text.replace('\n', " "));
            picker.selected = 0;
            return;
        }
        if self.screen.output_focus && self.screen.output_search {
            self.screen
                .output_filter
                .push_str(&p1_tui::editor::sanitize(&text).replace('\n', " "));
            self.screen.refilter_output();
            return;
        }
        self.screen.status = None;
        self.screen.output_focus = false;
        self.screen.selected = None;
        self.screen.quit_armed = false;
        // A paste right after a held decision key means it was typed text.
        if let Some((held, _, _)) = self.pending_decision.take() {
            self.screen.composer.insert(held);
            self.screen.flash = None;
        }
        // Typing for the approval guard: a decision key right after is not one.
        self.screen.last_type_ms = Some(self.now_ms);
        self.screen.composer.insert_text(&text);
    }

    fn on_key(&mut self, key: crossterm::event::KeyEvent) {
        use crossterm::event::KeyModifiers;
        let plain = key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT;
        // The picker filters as you type (SPEC §4.6).
        if let Some(picker) = &mut self.screen.picker {
            match (key.code, plain) {
                (crossterm::event::KeyCode::Char(c), true) => {
                    picker.filter.push(c);
                    picker.selected = 0;
                    return;
                }
                (crossterm::event::KeyCode::Backspace, _) => {
                    picker.filter.pop();
                    picker.selected = 0;
                    return;
                }
                _ => {}
            }
        }
        self.screen.now_ms = self.now_ms;
        // A decision key waiting its beat: another key now means a word was
        // being typed — the held key and this one are draft text. Enter
        // confirms it instead (`y⏎`, the [y/N] habit: a lone letter never
        // becomes steering), and Esc takes it back.
        if let Some((held, _, call)) = self.pending_decision.take() {
            self.screen.flash = None;
            let enter = key.code == crossterm::event::KeyCode::Enter
                && (key.modifiers.is_empty() || key.modifiers == KeyModifiers::ALT);
            match key.code {
                // Only over an empty draft: finishing a word (`retr` + `y⏎`)
                // is text, and Enter then sends it as usual.
                _ if enter && self.screen.composer.text.trim().is_empty() => {
                    self.pending_decision = Some((held, 0, call));
                    self.commit_decision();
                    return;
                }
                crossterm::event::KeyCode::Esc => return,
                // ^C decides on its own (deny, or cancel the turn): the held
                // letter is dropped, never typed.
                crossterm::event::KeyCode::Char('c')
                    if key.modifiers.contains(KeyModifiers::CONTROL) => {}
                _ => self.screen.composer.insert(held),
            }
        }
        let command = input::handle(&self.screen, key);
        if matches!(
            key.code,
            crossterm::event::KeyCode::Char(_) | crossterm::event::KeyCode::Backspace
        ) && plain
        {
            self.screen.last_type_ms = Some(self.now_ms);
        }
        let Some(command) = command else {
            return;
        };
        if !matches!(command, Command::CancelOrQuit | Command::ClearDraft) {
            self.screen.quit_armed = false;
        }
        match command {
            Command::Submit(text) | Command::QueueSteering(text) | Command::QueueFollowUp(text)
                if self.lone_decision(&text) => {}
            Command::QueueSteering(text) | Command::QueueFollowUp(text) if text.trim() == "/" => {
                self.screen
                    .flash_text("type a command name — /help lists them");
            }
            Command::Submit(text) => self.submit(text),
            Command::SubmitFollowUp(text) => self.submit_as(text, true),
            Command::QueueSteering(text) => self.steer(text),
            Command::QueueFollowUp(text) => {
                self.screen.composer.take();
                self.screen.remember(&text);
                let text = p1_tui::commands::prompt_text(&text);
                self.screen.queue(true, text.clone());
                self.follow_ups.push_back(text);
            }
            Command::EditGoal => {
                // Never destroy a draft: it goes to history (Up brings it back)
                // and the goal is prefilled so it can be edited.
                let draft = self.screen.composer.text.clone();
                // A goal already being written stays as it is.
                if !draft.starts_with("/goal") {
                    if !draft.trim().is_empty() {
                        self.screen.remember(&draft);
                    }
                    let goal = self.screen.goal.clone().unwrap_or_default();
                    self.screen.composer.set_text(format!("/goal {goal}"));
                }
            }
            Command::Edit(action) => self.screen.edit(action),
            Command::OutputSearch => {
                self.screen.output_search = true;
                self.filter_before = self.screen.output_filter.clone();
                self.scroll_before = self.screen.output.as_ref().map_or(0, |o| o.scroll);
            }
            Command::OutputFilter(c) => {
                self.screen.output_filter.push(c);
                self.screen.refilter_output();
            }
            Command::OutputFilterBackspace => {
                self.screen.output_filter.pop();
                self.screen.refilter_output();
            }
            Command::OutputFilterDone => self.screen.output_search = false,
            Command::OutputFilterCancel => {
                self.screen.output_search = false;
                self.screen.output_filter = std::mem::take(&mut self.filter_before);
                self.screen.refilter_output();
                if let Some(o) = &mut self.screen.output {
                    o.scroll = self.scroll_before;
                }
            }
            Command::ScrollFirst => self.screen.scroll_top = Some(0),
            Command::ScrollLast => self.screen.scroll_top = None,
            Command::ScrollLines(n) => self.screen.scroll_by(n),
            Command::PaneLeft => self.screen.pan_output(-8),
            Command::PaneRight => self.screen.pan_output(8),
            Command::PaneFirst => {
                if let Some(o) = &mut self.screen.output {
                    o.scroll = 0;
                }
            }
            Command::PaneLast => self.screen.scroll_output_by(isize::MAX / 2),
            Command::Newline => self.screen.composer.insert('\n'),
            Command::Insert(c) => self.screen.composer.insert(c),
            Command::Backspace => self.screen.composer.backspace(),
            Command::Left => self.screen.composer.left(),
            Command::Right => self.screen.composer.right(),
            Command::Yank => self.screen.composer.yank(),
            Command::Undo => {
                self.screen.composer.undo();
            }
            // Right after a turn ends, a reflexive ^C must not wipe what the
            // cancel just returned: it only arms the quit.
            Command::ClearDraft if self.just_ended() && !self.screen.quit_armed => {
                self.screen.quit_armed = true;
            }
            Command::ClearDraft => self.screen.clear_draft(),
            Command::CopyOutput => self.copy("output"),
            Command::PaletteMove(delta) => {
                let n = self.screen.palette().len();
                if n > 0 {
                    self.screen.palette_selected = (self.screen.palette_selected as isize + delta)
                        .rem_euclid(n as isize)
                        as usize;
                }
            }
            Command::PaletteComplete => {
                // Completed to `/find `: the argument it wants stays named.
                if let Some(command) = self.screen.palette_complete() {
                    match command.args {
                        p1_tui::commands::Args::Required(a) => self.screen.flash_text(format!(
                            "/{} <{a}> — {}",
                            command.name, command.description
                        )),
                        p1_tui::commands::Args::Optional(a) => self.screen.flash_text(format!(
                            "/{} [{a}] — {}",
                            command.name, command.description
                        )),
                        p1_tui::commands::Args::None => {}
                    }
                }
            }
            Command::PaletteRun => {
                // Run it unless it still needs an argument.
                // A prefix runs only harmless no-argument commands; anything
                // that quits or wants an argument is completed for a second ⏎.
                if let Some(command) = self.screen.palette_complete()
                    && command.args == p1_tui::commands::Args::None
                    && !matches!(command.name, "exit" | "quit")
                {
                    let text = self.screen.composer.text.trim_end().to_owned();
                    self.submit(text);
                }
            }
            Command::PaletteDismiss => self.screen.palette_dismissed = true,
            Command::SelectBlocks => {
                if !self.screen.select_first() {
                    self.screen.flash_text("nothing to select yet");
                }
            }
            Command::SelectStep(delta) => self.screen.select_step(delta),
            Command::ToggleSelected => self.screen.toggle_selected(),
            Command::OpenSelected => match self.screen.selected_output() {
                Some(id) => {
                    self.screen.selected = None;
                    self.screen.open_fold(&id);
                }
                None => self.screen.flash_text("this block has no retained output"),
            },
            Command::CopySelected => match self.screen.selected_output() {
                Some(id) => self.copy(&id.0),
                None => self.screen.flash_text("this block has no retained output"),
            },
            Command::EndSelection(key) => {
                self.screen.selected = None;
                if let Some(key) = key {
                    self.on_key(key);
                }
            }
            Command::ReturnToComposer(key) => {
                self.screen.output_focus = false;
                self.screen.output_search = false;
                self.on_key(key);
            }
            Command::FocusComposer => {
                self.screen.output_focus = false;
                self.screen.output_search = false;
            }
            // ^C at idle with nothing to clear: quit, unless a turn was just
            // cancelled — a second, deliberate press is then needed.
            Command::CancelOrQuit => {
                let recent = self.just_ended();
                if self.screen.working.is_some() || self.screen.busy {
                    // The loop owns turn cancellation.
                } else if recent && !self.screen.quit_armed {
                    self.screen.quit_armed = true;
                } else {
                    self.exit = Some(0);
                }
            }
            // A decision commits after a short beat without another key (see
            // `commit_decision`); a word typed across the approval never decides.
            Command::ApproveOnce | Command::ApproveSession | Command::Deny
                if matches!(key.code, crossterm::event::KeyCode::Char(_))
                    && !key.modifiers.contains(KeyModifiers::CONTROL) =>
            {
                let crossterm::event::KeyCode::Char(c) = key.code else {
                    unreachable!()
                };
                let call = self
                    .pending_auth
                    .front()
                    .map(|p| p.call.call_id.clone())
                    .unwrap_or_default();
                self.pending_decision = Some((c, self.now_ms + DECISION_BEAT_MS, call));
                // A held key is visible: what it will do, and how to undo it.
                let what = match c.to_ascii_lowercase() {
                    'y' => "allow once",
                    'a' => "allow for this session",
                    _ => "deny",
                };
                self.screen
                    .flash_text(format!("{c} · {what} in a moment — ⏎ now · esc undoes"));
            }
            Command::DecideFirst => self.decide_first(),
            Command::ProjectUnavailable => self.screen.flash_text(
                "p project: not available yet (no trust store) — y once · a session · n deny",
            ),
            Command::ApproveOnce => self.answer(Decision::Permit, false),
            Command::ApproveSession | Command::ApproveProject | Command::AllFiles => {
                // Project grants share the in-memory set until a trust store
                // exists (issue #12). AllFiles grants the tool under review.
                self.answer(Decision::Permit, true);
            }
            Command::Deny => self.answer(
                Decision::Deny {
                    reason: p1_tui::runtime::USER_DENY.to_string(),
                },
                false,
            ),
            Command::CloseOverlayAndType(key) => {
                self.screen.status = None;
                self.screen.ledger_overlay = false;
                self.screen.overlay_scroll = 0;
                self.on_key(key);
            }
            Command::OverlayScroll(delta) => {
                self.screen.overlay_scroll =
                    self.screen.overlay_scroll.saturating_add_signed(delta);
            }
            // The filter goes, the reader stays on the line they had found.
            Command::OutputFilterClear => {
                let at = match (&self.screen.output_matches, &self.screen.output) {
                    (Some(matches), Some(view)) => matches.get(view.scroll).copied(),
                    _ => None,
                };
                self.screen.output_filter.clear();
                self.screen.refilter_output();
                if let Some(at) = at {
                    self.screen.scroll_output_by(at as isize);
                }
            }
            Command::NextFile => {}
            Command::OpenFold => self.open_fold_here(),
            Command::ExpandReasoning => self.screen.toggle_reasoning(),
            Command::CyclePaneMode => self.screen.cycle_mode(),
            Command::CyclePaneWidth => self.screen.cycle_width(),
            Command::TogglePin => self.screen.toggle_pin(),
            Command::ToggleLedgerOverlay => {
                self.screen.ledger_overlay = !self.screen.ledger_overlay;
                if self.screen.ledger_overlay {
                    // The ledger takes the pane: an open OUTPUT closes as Esc
                    // would (its focus, and the width it took, go with it).
                    if self.screen.pane_mode == p1_tui::state::PaneMode::Output {
                        self.close_output();
                        self.screen.ledger_overlay = true;
                    }
                    self.screen.pane_mode = p1_tui::state::PaneMode::Ledger;
                    self.screen.output_focus = false;
                    self.screen.output_search = false;
                }
            }
            // Esc closes the topmost layer only.
            Command::Dismiss => {
                if self.screen.picker.is_some() || self.screen.status.is_some() {
                    self.screen.picker = None;
                    self.screen.status = None;
                    self.screen.overlay_scroll = 0;
                } else if self.screen.ledger_overlay {
                    self.screen.ledger_overlay = false;
                } else if self.screen.output_focus
                    || self.screen.output_saved.is_some()
                    || (self.screen.output.is_some()
                        && self.screen.pane_mode == p1_tui::state::PaneMode::Output)
                {
                    // Also an OUTPUT pane reached by F6 or /pane: the hint
                    // says esc closes it, so it does.
                    self.close_output();
                } else {
                    // Nothing else is open: esc drops the /find highlight.
                    self.screen.search = None;
                    self.screen.find_output = None;
                }
            }
            Command::PickerUp => {
                if let Some(picker) = &mut self.screen.picker {
                    picker.move_selection(-1);
                }
            }
            Command::PickerDown => {
                if let Some(picker) = &mut self.screen.picker {
                    picker.move_selection(1);
                }
            }
            Command::PickerAccept => {
                let chosen = self
                    .screen
                    .picker
                    .as_ref()
                    .and_then(|p| p.selected_row())
                    .map(|row| row.value.clone());
                self.screen.picker = None;
                // Output picker rows carry their handle first.
                if let Some(value) = chosen
                    && let Some(id) = value.split_whitespace().next()
                {
                    self.screen.open_fold(&p1_tui::fold::FoldId(id.to_owned()));
                }
            }
            Command::ScrollUp if self.screen.output_focus => {
                let page = self.screen.output_page();
                self.screen.scroll_output_by(-page)
            }
            Command::ScrollDown if self.screen.output_focus => {
                let page = self.screen.output_page();
                self.screen.scroll_output_by(page)
            }
            Command::ScrollUp => self
                .screen
                .scroll_by(self.screen.last_rendered.1.saturating_sub(2).max(1) as isize),
            Command::ScrollDown => self
                .screen
                .scroll_by(-(self.screen.last_rendered.1.saturating_sub(2).max(1) as isize)),
            Command::PaneUp => self.screen.scroll_output_by(-1),
            Command::PaneDown => self.screen.scroll_output_by(1),
        }
    }

    /// `^O`: an open but unfocused OUTPUT pane takes focus back as it was;
    /// otherwise open the fold the reader is looking at (the newest one on
    /// screen while scrolled, the newest of all while following).
    fn open_fold_here(&mut self) {
        // A /find that pointed into an output ("^O opens it"), while its note
        // is still on screen: open it there.
        let noted = self.screen.find_output.take().filter(|id| {
            self.screen
                .flash
                .as_ref()
                .is_some_and(|(t, _)| t.contains(&id.0))
        });
        if let Some(id) = noted {
            self.screen.flash = None;
            let needle = self
                .screen
                .search
                .as_ref()
                .map(|s| s.query.clone())
                .unwrap_or_default();
            self.screen.open_fold_at(&id, &needle);
            return;
        }
        // The fold row on screen says "^O open in pane": it wins, following
        // or not.
        let fold_row = self
            .screen
            .tool_hits
            .iter()
            .rev()
            .find_map(|(_, hit)| match hit {
                p1_tui::render::block::Hit::Fold(id) => Some(id.clone()),
                _ => None,
            });
        if let Some(action) = self.screen.pane_ctrl_o() {
            match action {
                p1_tui::state::CtrlO::Open(id) => {
                    self.screen.open_fold(&id);
                }
                p1_tui::state::CtrlO::Close => self.close_output(),
                p1_tui::state::CtrlO::Focus => self.screen.output_focus = true,
            }
            return;
        }
        // Else a call on screen whose output is not all shown (folded, or cut
        // at the right edge): the output being read.
        let width = self.screen.transcript_width;
        let visible = fold_row.or_else(|| {
            self.screen
                .tool_hits
                .iter()
                .rev()
                .find_map(|(_, hit)| match hit {
                    p1_tui::render::block::Hit::Header(i) => {
                        match &self.screen.transcript.blocks[*i] {
                            p1_tui::transcript::Block::Call(row)
                                if p1_tui::render::block::hides_content(row, width) =>
                            {
                                row.output_id.clone()
                            }
                            _ => None,
                        }
                    }
                    _ => None,
                })
        });
        // Else the newest folded output, else the newest output at all — and
        // then the pane says which one opened, since it is not on screen.
        let newest = || {
            self.screen
                .transcript
                .blocks
                .iter()
                .rev()
                .find_map(|block| match block {
                    p1_tui::transcript::Block::Call(row) => row.output_id.clone(),
                    _ => None,
                })
        };
        let on_screen = visible.is_some();
        match visible
            .or_else(|| self.screen.transcript.latest_fold.clone())
            .or_else(newest)
        {
            Some(id) => {
                self.screen.open_fold(&id);
                if !on_screen {
                    self.screen
                        .flash_text(format!("opened {id} — the newest output (not on screen)"));
                }
            }
            None => self.screen.flash_text("no retained outputs yet"),
        }
    }

    /// A submitted line: a local command, or a prompt for the agent. The
    /// prompt waits in `submit_pending` for the loop (the agent borrow lives
    /// there, not here).
    fn submit(&mut self, text: String) {
        self.submit_as(text, false);
    }

    /// `submit`, with a mid-turn prompt queued as a follow-up (`⌥⏎`) rather
    /// than as steering.
    fn submit_as(&mut self, text: String, follow_up: bool) {
        use p1_tui::commands::{Parsed, parse, prompt_text};
        // A bare `/` is the palette's start, never a prompt.
        if text.trim() == "/" {
            self.screen
                .flash_text("type a command name — /help lists them");
            return;
        }
        match parse(&text) {
            Parsed::Command(command, arg) => {
                let (name, arg) = (command.name, arg.to_owned());
                self.screen.composer.take();
                // History only: a local command leaves the reader where they are.
                self.screen.remember(&text);
                self.unknown_confirm = None;
                self.slash(name, &arg);
            }
            // An unknown `/word` stays in the composer with a note; Enter again
            // sends it as a prompt after all.
            Parsed::Unknown(name) if self.unknown_confirm.as_deref() != Some(text.as_str()) => {
                let key = if follow_up { "⌥⏎" } else { "⏎" };
                self.screen.flash_text(format!(
                    "/{name} is not a command · {key} again sends it · /help"
                ));
                self.unknown_confirm = Some(text);
            }
            // During a turn a prompt (a confirmed unknown /word) is steering,
            // or a follow-up by ⌥⏎: delivered at the next boundary, drawn when
            // it is delivered.
            _ if self.screen.working.is_some() || self.screen.busy => {
                self.unknown_confirm = None;
                self.screen.flash = None;
                if follow_up {
                    self.screen.composer.take();
                    self.screen.remember(&text);
                    let text = prompt_text(&text);
                    self.screen.queue(true, text.clone());
                    self.follow_ups.push_back(text);
                } else {
                    self.steer(text);
                }
            }
            _ => {
                self.unknown_confirm = None;
                self.screen.flash = None;
                self.screen.composer.take();
                self.screen.remember_input(&text);
                let prompt = prompt_text(&text);
                self.screen.transcript.operator(prompt.clone());
                self.submit_pending = Some(prompt);
                self.inbox_hold = false;
            }
        }
    }

    /// Queue steering for the running turn (Enter while working).
    fn steer(&mut self, text: String) {
        self.screen.composer.take();
        self.screen.remember(&text);
        let text = p1_tui::commands::prompt_text(&text);
        self.screen.queue(false, text.clone());
        self.inbox.send(InboxKind::Steering, text);
    }

    /// A held decision key whose beat passed without another key: decide the
    /// call it was pressed for, if that call still waits.
    fn commit_decision(&mut self) {
        let Some((key, due, call)) = &self.pending_decision else {
            return;
        };
        let key = &key.to_ascii_lowercase();
        if self.now_ms < *due {
            return;
        }
        let (key, call) = (*key, call.clone());
        self.pending_decision = None;
        // The "in a moment" note is done either way.
        self.screen.flash = None;
        if self
            .pending_auth
            .front()
            .is_none_or(|p| p.call.call_id != call)
        {
            return;
        }
        match key {
            'y' => self.answer(Decision::Permit, false),
            'a' => self.answer(Decision::Permit, true),
            _ => self.answer(
                Decision::Deny {
                    reason: p1_tui::runtime::USER_DENY.to_string(),
                },
                false,
            ),
        }
    }

    /// A turn ended (or was cancelled) moments ago.
    fn just_ended(&self) -> bool {
        self.last_cancel_ms
            .into_iter()
            .chain(self.last_turn_end_ms)
            .any(|at| self.now_ms.saturating_sub(at) < 1_500)
    }

    /// When the held decision is due, if one is held.
    fn decision_due(&self) -> Option<u64> {
        self.pending_decision.as_ref().map(|(_, due, _)| *due)
    }

    /// A draft that is only `y`, `n` or `a` while an approval waits (or just
    /// after one was answered) is a decision, never a message to the model:
    /// an armed approval takes it; otherwise it stays in the draft with a note.
    /// True when the text was handled here.
    fn lone_decision(&mut self, text: &str) -> bool {
        // `y`, or the same letter pressed again (`yy`, a retried reflex).
        let letters: Vec<char> = text
            .chars()
            .filter(|c| !c.is_whitespace())
            .map(|c| c.to_ascii_lowercase())
            .collect();
        let letter = match letters.first() {
            Some(&first) if letters.iter().all(|&c| c == first) && letters.len() <= 3 => first,
            _ => return false,
        };
        if !matches!(letter, 'y' | 'n' | 'a') {
            return false;
        }
        let recent = self
            .last_decision_ms
            .is_some_and(|at| self.now_ms.saturating_sub(at) < 2_000);
        if self.screen.approval.is_none() && !recent {
            return false;
        }
        let grantable = match &self.screen.approval {
            Some(Approval::Diff(view)) => view.grantable,
            Some(Approval::Permission(view)) => view.grantable,
            None => false,
        };
        // Only an approval the operator can see, and `a` only where a grant
        // is allowed.
        let armed = self.screen.approval.is_some()
            && self.screen.approval_visible
            && (letter != 'a' || (grantable && self.screen.approval_grant_visible))
            && self.now_ms >= self.screen.approval_shown_ms + input::APPROVAL_ARM_MS;
        if armed {
            self.screen.composer.take();
            match letter {
                'y' => self.answer(Decision::Permit, false),
                'a' => self.answer(Decision::Permit, true),
                _ => self.answer(
                    Decision::Deny {
                        reason: p1_tui::runtime::USER_DENY.to_string(),
                    },
                    false,
                ),
            }
        } else {
            self.screen.flash_text(format!(
                "'{letter}' not sent — alone it answers an approval; type more to steer"
            ));
        }
        true
    }

    /// A layer asked for while an approval waits: nothing may cover it.
    fn decide_first(&mut self) {
        self.screen
            .flash_text("answer the approval first — y allow · n deny · ^C cancels");
    }

    fn close_output(&mut self) {
        self.screen.close_output();
    }

    /// `/copy` (the last reply), `/copy output` (the open output as shown,
    /// filtered or whole), `/copy h-xxxx`, and the OUTPUT pane's `y`.
    fn copy(&mut self, arg: &str) {
        use p1_tui::transcript::Block;
        let text = match arg {
            "" | "last" => self
                .screen
                .transcript
                .blocks
                .iter()
                .rev()
                .find_map(|b| match b {
                    Block::Prose { lines } => Some(lines.join("\n").trim().to_owned()),
                    _ => None,
                }),
            "output" => match &self.screen.output {
                Some(view) => Some(match &self.screen.output_matches {
                    Some(matches) if !self.screen.output_filter.is_empty() => matches
                        .iter()
                        .map(|i| view.lines[*i].as_str())
                        .collect::<Vec<_>>()
                        .join("\n"),
                    _ => view.lines.join("\n"),
                }),
                None => self
                    .screen
                    .transcript
                    .latest_fold
                    .as_ref()
                    .and_then(|id| self.screen.transcript.output(id))
                    .map(str::to_owned),
            },
            id => self
                .screen
                .transcript
                .output(&p1_tui::fold::FoldId(
                    id.trim_matches(['[', ']']).to_owned(),
                ))
                .map(str::to_owned),
        };
        // Every path copies the same way: escapes out, tabs and lines kept.
        let Some(text) = text
            .map(|t| p1_tui::render::block::strip_escapes(strip_exit_trailer(&t)))
            .filter(|t| !t.is_empty())
        else {
            self.screen.flash_text("nothing to copy");
            return;
        };
        let (lines, bytes) = (text.lines().count(), text.len());
        match self.ui_tx.clone() {
            // A clipboard tool can take a moment: never on the UI thread.
            Some(tx) => {
                self.screen.flash_text(format!(
                    "copying {} …",
                    p1_tui::render::block::plural(lines, "line")
                ));
                std::thread::spawn(move || {
                    let mut osc52 = vec![];
                    let result = clipboard::copy(&text, &mut osc52);
                    if !osc52.is_empty() {
                        let _ = tx.send(UiEvent::Terminal(osc52));
                    }
                    let _ = tx.send(UiEvent::Notice(clipboard::notice(lines, bytes, &result)));
                });
            }
            None => {
                let result = clipboard::copy(&text, &mut *self.term_out);
                self.screen
                    .flash_text(clipboard::notice(lines, bytes, &result));
            }
        }
    }

    fn slash(&mut self, name: &str, arg: &str) {
        let row = |label: &str, value: String| p1_tui::render::status::StatusRow {
            label: label.into(),
            value,
            available: true,
        };
        // A pending approval is never covered: layers wait until it is
        // answered (the command stays in the draft for afterwards).
        if self.screen.approval.is_some()
            && matches!(
                name,
                "open" | "outputs" | "filter" | "help" | "status" | "pane" | "find"
            )
        {
            let text = if arg.is_empty() {
                format!("/{name}")
            } else {
                format!("/{name} {arg}")
            };
            self.screen.composer.set_text(text);
            self.decide_first();
            return;
        }
        match name {
            "open" => {
                if arg.is_empty() {
                    self.open_fold_here();
                } else {
                    let id = p1_tui::fold::FoldId(arg.trim_matches(['[', ']']).to_string());
                    if !self.screen.open_fold(&id) {
                        self.screen
                            .flash_text("unknown output handle — /outputs lists them");
                    }
                }
            }
            "close" => self.close_output(),
            "find" if arg.is_empty() => self.screen.flash_text("usage: /find text"),
            "find" => {
                if !self.screen.find_next(arg) {
                    self.screen.flash_text(format!("no match for \"{arg}\""));
                }
            }
            "filter" => {
                let shown = self.screen.output.is_some()
                    && self.screen.pane_mode == p1_tui::state::PaneMode::Output;
                if !shown {
                    self.open_fold_here();
                }
                if self.screen.output.is_some() {
                    self.screen.output_focus = true;
                    self.screen.output_search = false;
                    self.screen.output_filter = arg.to_owned();
                    self.screen.refilter_output();
                }
            }
            "outputs" => {
                let picker = self.screen.outputs_picker();
                if picker.groups.iter().all(|g| g.rows.is_empty()) {
                    self.screen.flash_text("no retained outputs yet");
                } else {
                    self.screen.picker = Some(picker);
                }
            }
            "help" => {
                let mut rows = vec![
                    row("⏎ / ⇧⏎ or ^J", "send / newline".into()),
                    row("⌥⏎", "newline idle · follow-up while working".into()),
                    row("↑ ↓", "move by row · recall history at the edges".into()),
                    row("^A ^E · ⌥← ⌥→", "line start/end · word left/right".into()),
                    row(
                        "^U ^K ⌥⌫ ⌥D",
                        "erase to start/end · word back/forward".into(),
                    ),
                    row("^Y · ^Z", "yank the last erase · undo".into()),
                    row("^C", "cancel turn · clear draft · quit when empty".into()),
                    row("^O · esc", "open the output you are reading · close".into()),
                    row(
                        "PgUp PgDn · ⌥↑ ⌥↓",
                        "scroll transcript by page / line".into(),
                    ),
                    row("^Home ^End", "oldest event · follow live".into()),
                    row("click header", "expand / fold that tool's output".into()),
                    row("click fold row", "open that output in the pane".into()),
                    row("wheel", "scroll the pane under the pointer".into()),
                    row("^R · ^G", "reasoning · edit goal".into()),
                    row("Tab", "select blocks: ↑↓ ⏎ toggle, o open, y copy".into()),
                    row(
                        "y a n",
                        "approval: allow once · session · deny (alone, empty draft; ⏎ now, esc undoes)".into(),
                    ),
                    row(
                        "pane: ←→ / y",
                        "pan · filter (esc clears) · copy what it shows".into(),
                    ),
                    row("//x", "send /x to the model as text".into()),
                    row("^L", "ledger overlay (narrow screens)".into()),
                    row(
                        "^W · ^Tab or F6 · ^P",
                        "pane width · pane mode · pin".into(),
                    ),
                    row("shift+drag", "terminal selection (or /mouse)".into()),
                ];
                for command in p1_tui::commands::COMMANDS {
                    let args = match command.args {
                        p1_tui::commands::Args::None => String::new(),
                        p1_tui::commands::Args::Optional(a) => format!(" [{a}]"),
                        p1_tui::commands::Args::Required(a) => format!(" <{a}>"),
                    };
                    rows.push(row(
                        &format!("/{}{args}", command.name),
                        command.description.into(),
                    ));
                }
                self.screen.status = Some(vec![p1_tui::render::status::StatusGroup {
                    header: "KEYS & COMMANDS".into(),
                    rows,
                }]);
            }
            // During a turn: stop it first, then leave (the loop cancels it).
            "exit" | "quit" if self.screen.working.is_some() || self.screen.busy => {
                self.exit_after_turn = true;
                self.screen.flash_text("stopping the turn to quit …");
            }
            "exit" | "quit" => self.exit = Some(0),
            // `/focus` toggles; `/focus on|off` are deterministic, and `off`
            // returns to the automatic short-terminal policy (SPEC §4.3a).
            "focus" => {
                self.screen.focus_explicit = match arg {
                    "on" => Some(true),
                    "off" => Some(false),
                    _ => Some(!self.screen.focus),
                };
                let on = self.screen.focus_explicit == Some(true);
                self.screen
                    .flash_text(if on { "focus on" } else { "focus off" });
            }
            "goal" => {
                self.screen.goal = (!arg.is_empty()).then(|| arg.to_string());
                self.screen.flash_text(match &self.screen.goal {
                    Some(goal) => format!("goal set — {goal}"),
                    None => "goal cleared".into(),
                });
            }
            "status" => {
                self.screen.status = Some(status_groups(self));
            }
            "pane" => {
                use p1_tui::state::PaneMode;
                let mode = match arg {
                    "ledger" => Some(PaneMode::Ledger),
                    "output" => Some(PaneMode::Output),
                    "diff" => Some(PaneMode::Diff),
                    "workers" => Some(PaneMode::Workers),
                    "" => None,
                    _ => {
                        self.screen
                            .flash_text("usage: /pane ledger|output|diff|workers");
                        return;
                    }
                };
                match mode {
                    Some(mode) => self.screen.pane_mode = mode,
                    None => self.screen.cycle_mode(),
                }
                if self.screen.pane_width == p1_tui::state::PaneWidth::Off {
                    self.screen.pane_width = p1_tui::state::PaneWidth::Ch40;
                }
            }
            "copy" => self.copy(arg),
            "mouse" => {
                self.screen.mouse_released = !self.screen.mouse_released;
                // ?1007: with the mouse released, a terminal may turn the wheel
                // into arrow keys (alternate scroll), which would walk history.
                use std::io::Write;
                let _ = if self.screen.mouse_released {
                    crossterm::execute!(self.term_out, crossterm::event::DisableMouseCapture)
                        .and_then(|()| self.term_out.write_all(b"\x1b[?1007l"))
                } else {
                    crossterm::execute!(self.term_out, crossterm::event::EnableMouseCapture)
                        .and_then(|()| self.term_out.write_all(b"\x1b[?1007h"))
                };
                let _ = self.term_out.flush();
                self.screen.flash_text(if !self.screen.mouse_released {
                    "mouse on — wheel and clicks go to p1"
                } else {
                    "mouse off — drag selects text; /mouse takes it back"
                });
            }
            _ => {}
        }
    }

    fn answer(&mut self, decision: Decision, grant: bool) {
        let Some(pending) = self.pending_auth.pop_front() else {
            return;
        };
        self.last_decision_ms = Some(self.now_ms);
        // Waiting for the operator is not the model being silent.
        self.screen.phase_since_ms = self.now_ms;
        // The pane stays away a moment: approvals in a row do not flip the
        // layout back and forth between them.
        self.screen.pane_hold_until = self.now_ms + 1_000;
        // Notes about this approval are done with it.
        if self.screen.flash.as_ref().is_some_and(|(text, _)| {
            text.starts_with("answer the approval first") || text.starts_with('\'')
        }) {
            self.screen.flash = None;
        }
        if grant {
            self.policy.grant_session(&pending.call, &pending.identity);
            // A grant is visible where it was made: it outlives this call.
            self.screen.transcript.note(&format!(
                "· {} allowed for the rest of this session",
                pending.call.name
            ));
        }
        // A denial shows at once; the core's own result updates the same row.
        if let Decision::Deny { reason } = &decision {
            self.screen.transcript.apply(
                &p1_contracts::AgentEvent::ToolFinished {
                    result: p1_contracts::ToolResultItem {
                        call_id: pending.call.call_id.clone(),
                        name: pending.call.name.clone(),
                        status: p1_contracts::ToolStatus::Denied,
                        content: reason.clone(),
                    },
                },
                None,
            );
        }
        // An allowed call is what the turn is doing now (until its start
        // event), not a wait for the model.
        if matches!(decision, Decision::Permit)
            && let Some(working) = &mut self.screen.working
        {
            working.label = pending.call.name.clone();
        }
        pending.answer(decision);
        self.screen.approval = None;
        if self.pinned_by_approval {
            self.screen.pinned = false;
            self.pinned_by_approval = false;
        }
        // The next queued approval takes the screen immediately.
        self.show_next_auth();
    }

    /// A cancelled turn abandons its parked approvals: dropping a request
    /// denies it (the policy's drop semantics), and the screen comes back.
    fn drop_pending_auth(&mut self) {
        self.pending_auth.clear();
        if self.pending_decision.take().is_some() {
            self.screen.flash = None;
        }
        self.screen.approval = None;
        self.screen.approvals_waiting = 0;
        if self.pinned_by_approval {
            self.screen.pinned = false;
            self.pinned_by_approval = false;
        }
    }

    /// Pull the refresher's snapshot into the screen (SPEC §5 promotion).
    fn sync_workers(&mut self) -> bool {
        let rows = self.worker_rows.lock().unwrap();
        if self.screen.workers == *rows {
            return false;
        }
        self.screen.sync_workers(rows.clone());
        true
    }

    /// One UI event: an agent event (parent or a tagged worker) or a marker.
    fn on_ui_event(&mut self, ui: UiEvent) {
        // Notices are stamped with the clock of their arrival, not of the
        // last key (a copy can finish seconds later).
        self.screen.now_ms = self.now_ms;
        match ui {
            UiEvent::Notice(text) => self.screen.flash_text(text),
            UiEvent::Terminal(bytes) => {
                use std::io::Write;
                let _ = self.term_out.write_all(&bytes);
                let _ = self.term_out.flush();
            }
            UiEvent::WorkerStarted(id) => {
                self.screen.transcript.note(&format!("↳ {id} started"));
            }
            UiEvent::Agent(stamped) => {
                if stamped.worker.is_some() {
                    // Worker streams stay OUT of the parent's transcript (the
                    // WORKERS pane is a later milestone); a finished worker is
                    // worth one quiet line.
                    if let p1_contracts::AgentEvent::TurnFinished { end } = &stamped.event {
                        let state = match end {
                            TurnEnd::Completed { .. } => "finished",
                            TurnEnd::Cancelled => "cancelled",
                            _ => "failed",
                        };
                        let id = stamped.worker.clone().unwrap_or_default();
                        self.screen.transcript.note(&format!("↳ {id} {state}"));
                    }
                    return;
                }
                // Delivered steering is the operator's own words: it enters the
                // transcript as operator input where the model received it (the
                // same shape a resumed session paints); only worker notices are
                // summarised.
                if let p1_contracts::AgentEvent::InboxDelivered { count } = &stamped.event {
                    let mut delivered = 0;
                    while delivered < *count
                        && let Some(at) = self.screen.queued.iter().position(|q| !q.follow_up)
                    {
                        let steering = self.screen.queued.remove(at).expect("present");
                        self.screen.transcript.operator(steering.text);
                        delivered += 1;
                    }
                    if *count > delivered {
                        let n = count - delivered;
                        self.screen.transcript.note(&format!(
                            "· {n} worker notification{} delivered",
                            if n == 1 { "" } else { "s" }
                        ));
                    }
                    return;
                }
                self.track_task(&stamped.event);
                self.screen.apply(&stamped.event, stamped.at_ms);
            }
        }
    }

    /// Successful edit-shaped calls move the TASK section. Reads only the
    /// call's own input; a denied or failed call counts nothing.
    fn track_task(&mut self, event: &p1_contracts::AgentEvent) {
        match event {
            p1_contracts::AgentEvent::ToolStarted { call } => {
                self.pending_calls
                    .insert(call.call_id.clone(), call.clone());
            }
            p1_contracts::AgentEvent::ToolFinished { result } => {
                // The reviewed rows are needed once, for this result, whatever it is.
                let reviewed = self.reviewed.remove(&result.call_id);
                let Some(call) = self.pending_calls.remove(&result.call_id) else {
                    return;
                };
                if result.status != p1_contracts::ToolStatus::Ok {
                    return;
                }
                if !matches!(
                    call.name.as_str(),
                    "edit" | "patch" | "apply_patch" | "write"
                ) {
                    return;
                }
                let json: serde_json::Value =
                    serde_json::from_str(call.input.raw()).unwrap_or_default();
                let get = |key: &str| json.get(key).and_then(|v| v.as_str());
                if let Some(path) = get("file_path") {
                    self.task_files.insert(path.to_string());
                }
                if call.name.contains("patch") {
                    let patch = get("patch").unwrap_or(call.input.raw());
                    for file in p1_tui::render::diff::parse_patch(patch).files {
                        self.task_files.insert(file.path);
                    }
                }
                let (added, removed) = match reviewed {
                    // The diff reviewed before it ran: exact counts.
                    Some(rows) => {
                        use p1_tui::render::diff::DiffRow;
                        (
                            rows.iter()
                                .filter(|r| matches!(r, DiffRow::Add { .. }))
                                .count(),
                            rows.iter()
                                .filter(|r| matches!(r, DiffRow::Del { .. }))
                                .count(),
                        )
                    }
                    None if call.name.contains("patch") => {
                        let patch = get("patch").unwrap_or(call.input.raw());
                        let view = p1_tui::render::diff::DiffView::from_patch(&call.name, patch);
                        view.counts()
                    }
                    None => (
                        get("new_string")
                            .or_else(|| get("content"))
                            .map_or(0, |s| s.lines().count()),
                        get("old_string").map_or(0, |s| s.lines().count()),
                    ),
                };
                self.task_added += added as u64;
                self.task_removed += removed as u64;
                let journal = self
                    .screen
                    .task_view
                    .as_ref()
                    .and_then(|t| t.journal.clone());
                self.screen.task_view = Some(p1_tui::render::ledger::Task {
                    id: None,
                    files: Some(self.task_files.len() as u64),
                    diff: Some((self.task_added, self.task_removed)),
                    journal,
                });
            }
            _ => {}
        }
    }

    /// A parked authorization queues; the front one becomes the blocking
    /// approval view (SPEC §4.4 / §4.5): edit-shaped calls review as diffs,
    /// commands as §4.5 rows.
    fn on_auth(&mut self, request: AuthRequest) {
        // A denied call still gets its row, showing what it would have done.
        self.screen.transcript.announce(&request.call);
        self.pending_auth.push_back(request);
        self.screen.approvals_waiting = self.pending_auth.len().saturating_sub(1);
        self.show_next_auth();
    }

    /// Show the queue's front request, if none is on screen.
    fn show_next_auth(&mut self) {
        if self.screen.approval.is_some() {
            return;
        }
        let Some(request) = self.pending_auth.front() else {
            return;
        };
        let view = approval_view(request, &self.workspace, &self.sandbox);
        if let Approval::Diff(diff) = &view {
            self.reviewed
                .insert(request.call.call_id.clone(), diff.rows.clone());
            self.screen
                .transcript
                .attach_diff(&request.call.call_id, diff.rows.clone());
        }
        self.screen.approval = Some(view);
        self.screen.approval_shown_ms = self.now_ms;
        // Until a frame says it had no room for it.
        self.screen.approval_visible = true;
        self.screen.approval_grant_visible = true;
        self.screen.approvals_waiting = self.pending_auth.len() - 1;
        // A decision the operator is asked for is always in view and never
        // under another layer: overlays, pane focus and selection close.
        self.screen.picker = None;
        self.screen.status = None;
        self.screen.ledger_overlay = false;
        self.screen.output_focus = false;
        self.screen.output_search = false;
        self.screen.selected = None;
        self.screen.scroll_top = None;
        self.screen.approval_reveal = true;
        // An approval self-pins (SPEC §5): nothing may swap it away.
        if !self.screen.pinned {
            self.screen.pinned = true;
            self.pinned_by_approval = true;
        }
    }

    /// The turn ended. A cancel means stop: queued steering is taken back from
    /// the agent (ADR-0047) and, with queued follow-ups and an unsent prompt,
    /// returned to the composer — nothing typed is lost, nothing runs.
    fn note_turn_end(&mut self, end: &TurnEnd) {
        self.last_turn_end_ms = Some(self.now_ms);
        // Steering not yet delivered stays queued (and shown): the next inbox
        // turn delivers it. Only a cancel takes it back.
        if matches!(end, TurnEnd::Cancelled) {
            self.inbox_hold = true;
            // Back in the order they were typed (the queue rows' order), each
            // its own paragraph so their boundaries survive.
            let mut withdrawn = self.inbox.withdraw(InboxKind::Steering);
            let mut follow_ups = std::mem::take(&mut self.follow_ups);
            let mut returned: Vec<String> = vec![];
            for queued in std::mem::take(&mut self.screen.queued) {
                if queued.follow_up {
                    returned.extend(follow_ups.pop_front());
                } else if let Some(at) = withdrawn.iter().position(|w| *w == queued.text) {
                    returned.push(withdrawn.remove(at));
                }
            }
            returned.splice(0..0, withdrawn);
            returned.extend(follow_ups);
            returned.extend(self.submit_pending.take());
            self.drop_pending_auth();
            self.last_cancel_ms = Some(self.now_ms);
            if !returned.is_empty() {
                let n = returned.len();
                // While browsing history the draft is the stash, not the
                // recalled entry on screen.
                if self.screen.history_position.is_some() {
                    self.screen.clear_draft();
                }
                if !self.screen.composer.text.trim().is_empty() {
                    returned.push(self.screen.composer.text.clone());
                }
                self.screen.composer.set_text(returned.join("\n\n"));
                let returned = format!(
                    "{} returned to the composer",
                    p1_tui::render::block::plural(n, "queued message")
                );
                // One line for one event: `· cancelled after 3.2s · 1 queued …`.
                if !self.screen.transcript.extend_note("· cancelled", &returned) {
                    self.screen.transcript.note(&format!("· {returned}"));
                }
            }
        }
    }

    /// The oldest queued follow-up, fired only when the agent would stop.
    fn take_follow_up(&mut self) -> Option<String> {
        let next = self.follow_ups.pop_front()?;
        if let Some(at) = self.screen.queued.iter().position(|q| q.follow_up) {
            self.screen.queued.remove(at);
        }
        self.screen.transcript.operator(next.clone());
        Some(next)
    }
}

/// Shortest gap between two frames: bursts of events coalesce into one draw.
const FRAME_MS: u64 = 16;
/// How long a decision key waits for a following key before it decides: a
/// word typed across an approval (`also …`), even slowly, is text; a lone `a`
/// is a grant. Enter confirms sooner.
const DECISION_BEAT_MS: u64 = 400;

/// Output without the shell tool's `[exit code: N]` footer (the block states
/// the exit; a copy or a count of the output should not include it).
fn strip_exit_trailer(text: &str) -> &str {
    use p1_tui::render::block::is_trailer;
    let trimmed = text.trim_end_matches('\n');
    match trimmed.rsplit_once('\n') {
        Some((head, last)) if is_trailer(last) => head,
        None if is_trailer(trimmed) => "",
        _ => text,
    }
}
/// The working LEDs' frame step: 12.5 fps keeps the 1.1 s pulse smooth enough
/// and costs little; no frames at all while no LED is on screen.
const ANIMATION_MS: u64 = 80;
/// How often changes nobody can see yet are drawn (the `↓ N lines below`
/// count while the reader is scrolled back from a stream).
const OFFSCREEN_MS: u64 = 300;
/// A width change re-wraps prose: a resize drag lands as one frame once the
/// terminal settles (Iris RESIZE_REDRAW_DEBOUNCE).
const RESIZE_SETTLE_MS: u64 = 50;

/// Demand-driven redraw (after Iris's RenderScheduler): producers mark the
/// frame dirty only when something visible changed; a draw happens at most once
/// per [`FRAME_MS`], and not at all while a resize settles.
#[derive(Default)]
struct Frames {
    dirty: bool,
    /// Changes the reader cannot see (a reply streaming below a scrolled-back
    /// view): drawn at most every [`OFFSCREEN_MS`], for the `↓ N` count.
    offscreen: bool,
    last_draw: Option<u64>,
    hold_until: Option<u64>,
    width: Option<u16>,
}

impl Frames {
    fn pending(&self, now: u64) -> bool {
        self.dirty
            || (self.offscreen && self.last_draw.is_none_or(|last| now >= last + OFFSCREEN_MS))
    }

    fn due(&self, now: u64) -> bool {
        self.pending(now)
            && self.hold_until.is_none_or(|hold| now >= hold)
            && self.last_draw.is_none_or(|last| now >= last + FRAME_MS)
    }

    /// How long until a pending frame may be drawn.
    fn wait(&self, now: u64) -> std::time::Duration {
        let pace = if self.dirty { FRAME_MS } else { OFFSCREEN_MS };
        let at = self
            .hold_until
            .unwrap_or(0)
            .max(self.last_draw.map_or(0, |last| last + pace));
        std::time::Duration::from_millis(at.saturating_sub(now).max(1))
    }

    /// Something changed; `visible` says whether the reader can see it.
    fn changed(&mut self, visible: bool) {
        if visible {
            self.dirty = true;
        } else {
            self.offscreen = true;
        }
    }

    fn drawn(&mut self, now: u64) {
        self.dirty = false;
        self.offscreen = false;
        self.last_draw = Some(now);
        self.hold_until = None;
    }

    fn resized(&mut self, width: u16, now: u64) {
        if self.width.is_some_and(|w| w != width) {
            self.hold_until = Some(now + RESIZE_SETTLE_MS);
        }
        self.width = Some(width);
        self.dirty = true;
    }
}

/// Take one input plus everything already waiting behind it, coalescing wheel
/// events over one spot. The stream is polled with the task's own waker: a
/// no-op waker would lose the wake-up for the next key.
async fn batch<K>(first: UiInput, keys: &mut K) -> Vec<UiInput>
where
    K: futures_util::Stream + Unpin,
    K::Item: Into<UiInput>,
{
    use futures_util::StreamExt;
    use std::task::Poll;
    let mut out: Vec<UiInput> = vec![];
    let push = |input: UiInput, out: &mut Vec<UiInput>| {
        if let (
            UiInput::Wheel {
                up,
                column,
                row,
                events,
            },
            Some(UiInput::Wheel {
                up: last_up,
                column: last_column,
                row: last_row,
                events: last_events,
            }),
        ) = (&input, out.last_mut())
            && *up == *last_up
            && *column == *last_column
            && *row == *last_row
        {
            *last_events += events;
            return;
        }
        out.push(input);
    };
    push(first, &mut out);
    for _ in 0..256 {
        let ready = std::future::poll_fn(|cx| match keys.poll_next_unpin(cx) {
            Poll::Ready(item) => Poll::Ready(item),
            Poll::Pending => Poll::Ready(None),
        })
        .await;
        match ready {
            Some(next) => push(next.into(), &mut out),
            None => break,
        }
    }
    out
}

/// A process signal that ends the session as if the operator quit (the
/// terminal guard restores the screen on the way out).
async fn termination() -> i32 {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        // In raw mode ^C is a key; SIGINT only comes from outside (kill -INT)
        // and ends the session as cleanly as the others.
        let (Ok(mut term), Ok(mut hup), Ok(mut int)) = (
            signal(SignalKind::terminate()),
            signal(SignalKind::hangup()),
            signal(SignalKind::interrupt()),
        ) else {
            return std::future::pending().await;
        };
        tokio::select! {
            _ = term.recv() => 143,
            _ = hup.recv() => 129,
            _ = int.recv() => 130,
        }
    }
    #[cfg(not(unix))]
    std::future::pending().await
}

/// The render/input loop over a borrowed agent. Turns are pinned futures
/// inside this function: polled every wakeup, never dropped mid-flight.
#[allow(clippy::too_many_arguments)]
async fn drive_loop<B, K>(
    terminal: &mut ratatui::Terminal<B>,
    driver: &mut Driver,
    agent: &mut Agent,
    mut keys: K,
    mut events: mpsc::UnboundedReceiver<UiEvent>,
    mut auth: mpsc::UnboundedReceiver<AuthRequest>,
    cancel: &CancellationToken,
    sink: &TuiSink,
) -> i32
where
    B: Backend + FrameCommit,
    K: futures_util::Stream + Unpin,
    K::Item: Into<UiInput>,
{
    let mut tick = tokio::time::interval(std::time::Duration::from_millis(250));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut prompt: Option<String> = None;
    // Input that arrived in the same burst as the Enter that took a prompt:
    // the turn's own rules apply to it (a ^C there cancels that turn).
    let mut backlog: Vec<UiInput> = vec![];
    let mut frames = Frames {
        dirty: true,
        ..Frames::default()
    };
    let mut clock_minute = 0;
    let signal = termination();
    tokio::pin!(signal);
    loop {
        if let Some(code) = driver.exit {
            return code;
        }
        if cancel.is_cancelled() {
            return 130;
        }
        // Start the next turn (submitted prompt, or a queued follow-up from
        // the last turn's end), then drain the inbox after it — the
        // interactive loop's drain rule, unchanged.
        if let Some(text) = prompt.take() {
            driver.screen.busy = true;
            let child = cancel.child_token();
            driver.policy.set_turn(Some(child.clone()));
            let mut end = pump(
                terminal,
                driver,
                &mut keys,
                &mut events,
                &mut auth,
                sink,
                &child,
                &mut signal,
                std::mem::take(&mut backlog),
                Box::pin(agent.run_turn(text, child.clone())),
            )
            .await;
            driver.note_turn_end(&end);
            while !child.is_cancelled() && driver.exit.is_none() && agent.has_pending_inbox() {
                end = pump(
                    terminal,
                    driver,
                    &mut keys,
                    &mut events,
                    &mut auth,
                    sink,
                    &child,
                    &mut signal,
                    vec![],
                    Box::pin(async {
                        agent
                            .run_inbox_turn(child.clone())
                            .await
                            .unwrap_or(TurnEnd::Completed {
                                stop: p1_contracts::StopReason::EndTurn,
                            })
                    }),
                )
                .await;
                driver.note_turn_end(&end);
            }
            driver.policy.set_turn(None);
            driver.screen.busy = false;
            frames.dirty = true;
            if std::mem::take(&mut driver.exit_after_turn) {
                driver.exit = Some(0);
                continue;
            }
            if matches!(end, TurnEnd::Cancelled) {
                continue;
            }
            // A follow-up the operator queued, or a prompt submitted while the
            // turn was running (the pump's key handler routes by state).
            prompt = driver
                .take_follow_up()
                .or_else(|| driver.submit_pending.take());
            continue;
        }
        // Idle: draw when due, then wait for anything.
        let now = sink.now_ms();
        if frames.due(now) {
            draw(terminal, &mut driver.screen, now);
            frames.drawn(now);
        }
        let wait = frames.wait(now);
        tokio::select! {
            biased;
            _ = cancel.cancelled() => return 130,
            code = &mut signal => return code,
            key = keys.next() => {
                let Some(key) = key else { return 0 };
                driver.now_ms = sink.now_ms();
                let mut inputs = batch(key.into(), &mut keys).await.into_iter();
                for input in inputs.by_ref() {
                    if let UiInput::Resize(w) = input {
                        frames.resized(w, driver.now_ms);
                    }
                    frames.dirty |= driver.on_input(input);
                    // A prompt taken: everything after it in the same burst
                    // is typed while that turn runs (a second Enter steers,
                    // ^C cancels it, /exit stops it).
                    if let Some(text) = driver.submit_pending.take() {
                        prompt = Some(text);
                        driver.screen.busy = true;
                        break;
                    }
                }
                backlog.extend(inputs);
            }
            ui = events.recv() => {
                match ui {
                    Some(ui) => {
                        driver.now_ms = sink.now_ms();
                        driver.on_ui_event(ui);
                        frames.dirty = true;
                    },
                    None => return 0,
                }
            }
            request = auth.recv() => {
                if let Some(request) = request {
                    driver.now_ms = sink.now_ms();
                    driver.on_auth(request);
                    frames.dirty = true;
                }
            }
            _ = tokio::time::sleep(wait), if frames.dirty || frames.offscreen => {}
            _ = until(driver.decision_due(), sink.now_ms()), if driver.decision_due().is_some() => {
                driver.now_ms = sink.now_ms();
                driver.commit_decision();
                frames.dirty = true;
            }
            _ = tick.tick() => {
                frames.dirty |= driver.sync_workers();
                let now = sink.now_ms();
                let minute = now / 60_000;
                frames.dirty |= minute != clock_minute;
                clock_minute = minute;
                let before = (driver.screen.promotion.clone(), driver.screen.flash.is_some());
                driver.screen.tick(now);
                frames.dirty |= before != (driver.screen.promotion.clone(), driver.screen.flash.is_some());
            }
            // After a cancel, worker notifications wait for the next prompt.
            _ = agent.inbox_ready(), if !driver.inbox_hold => {
                // A worker's completion arrived at idle: drain it through the
                // inbox path — never as a phantom empty user turn. Each inbox
                // turn gets its own token; a cancel stops the drain.
                driver.screen.busy = true;
                while agent.has_pending_inbox() && driver.exit.is_none() {
                    let child = cancel.child_token();
                    driver.policy.set_turn(Some(child.clone()));
                    let end = pump(
                        terminal,
                        driver,
                        &mut keys,
                        &mut events,
                        &mut auth,
                        sink,
                        &child,
                        &mut signal,
                        vec![],
                        Box::pin(async {
                            agent
                                .run_inbox_turn(child.clone())
                                .await
                                .unwrap_or(TurnEnd::Completed {
                                    stop: p1_contracts::StopReason::EndTurn,
                                })
                        }),
                    )
                    .await;
                    driver.note_turn_end(&end);
                    if matches!(end, TurnEnd::Cancelled) {
                        break;
                    }
                }
                driver.policy.set_turn(None);
                driver.screen.busy = false;
                frames.dirty = true;
                if std::mem::take(&mut driver.exit_after_turn) {
                    driver.exit = Some(0);
                }
                // Queued follow-ups and a pending prompt start now, as after
                // any other turn.
                prompt = driver
                    .take_follow_up()
                    .or_else(|| driver.submit_pending.take());
            }
        }
    }
}

/// Sleep until the frontend clock reads `due` (now if it has passed).
async fn until(due: Option<u64>, now: u64) {
    let wait = due.map_or(0, |d| d.saturating_sub(now));
    tokio::time::sleep(std::time::Duration::from_millis(wait)).await;
}

/// Poll one turn-shaped future to completion while the UI stays live: keys,
/// events and parked authorizations are handled on every wakeup, and the
/// future is re-polled — never dropped — until it resolves.
#[allow(clippy::too_many_arguments)]
async fn pump<B, K, F, S>(
    terminal: &mut ratatui::Terminal<B>,
    driver: &mut Driver,
    keys: &mut K,
    events: &mut mpsc::UnboundedReceiver<UiEvent>,
    auth: &mut mpsc::UnboundedReceiver<AuthRequest>,
    sink: &TuiSink,
    turn_cancel: &CancellationToken,
    signal: &mut std::pin::Pin<&mut S>,
    backlog: Vec<UiInput>,
    mut turn: std::pin::Pin<Box<F>>,
) -> TurnEnd
where
    B: Backend + FrameCommit,
    K: futures_util::Stream + Unpin,
    K::Item: Into<UiInput>,
    F: std::future::Future<Output = TurnEnd>,
    S: std::future::Future<Output = i32>,
{
    let mut animation = tokio::time::interval(std::time::Duration::from_millis(ANIMATION_MS));
    animation.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut workers = tokio::time::interval(std::time::Duration::from_millis(250));
    workers.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut keys_done = false;
    let mut signalled = false;
    let mut shown_second = 0;
    let mut frames = Frames {
        dirty: true,
        width: terminal.size().ok().map(|s| s.width),
        ..Frames::default()
    };
    for input in backlog {
        turn_input(driver, &mut frames, turn_cancel, input);
    }
    loop {
        let now = sink.now_ms();
        if frames.due(now) {
            draw(terminal, &mut driver.screen, now);
            frames.drawn(now);
        }
        let wait = frames.wait(now);
        // The LEDs animate only while one is on screen and motion is allowed.
        let animating = driver.screen.animating && !driver.screen.reduced_motion;
        tokio::select! {
            biased;
            end = &mut turn => {
                // Completion may win select before the last emitted events are
                // read. No frame here: the loop draws once it knows whether a
                // follow-up starts at once (no in-between frame that jiggles).
                while let Ok(ui) = events.try_recv() {
                    driver.on_ui_event(ui);
                }
                return end;
            },
            code = signal.as_mut(), if !signalled => {
                // Stop the work, let the turn resolve, then leave.
                signalled = true;
                turn_cancel.cancel();
                driver.exit = Some(code);
            }
            key = keys.next(), if !keys_done => {
                let Some(key) = key else { keys_done = true; continue };
                driver.now_ms = sink.now_ms();
                for input in batch(key.into(), keys).await {
                    turn_input(driver, &mut frames, turn_cancel, input);
                }
            }
            ui = events.recv() => {
                if let Some(ui) = ui {
                    driver.now_ms = sink.now_ms();
                    driver.on_ui_event(ui);
                    for _ in 0..255 {
                        match events.try_recv() {
                            Ok(ui) => driver.on_ui_event(ui),
                            Err(_) => break,
                        }
                    }
                    frames.changed(!driver.screen.tail_offscreen());
                }
            }
            request = auth.recv() => {
                if let Some(request) = request {
                    driver.now_ms = sink.now_ms();
                    driver.on_auth(request);
                    frames.dirty = true;
                }
            }
            _ = tokio::time::sleep(wait), if frames.dirty || frames.offscreen => {}
            _ = animation.tick(), if animating => frames.dirty = true,
            _ = until(driver.decision_due(), sink.now_ms()), if driver.decision_due().is_some() => {
                driver.now_ms = sink.now_ms();
                driver.commit_decision();
                frames.dirty = true;
            }
            _ = workers.tick() => {
                frames.dirty |= driver.sync_workers();
                // The working row counts silent seconds; one frame a second.
                let second = sink.now_ms() / 1_000;
                frames.dirty |= driver.screen.counting && second != shown_second;
                shown_second = second;
                let before = (driver.screen.promotion.clone(), driver.screen.flash.is_some());
                driver.screen.tick(sink.now_ms());
                frames.dirty |= before != (driver.screen.promotion.clone(), driver.screen.flash.is_some());
            }
        }
    }
}

/// One input during a turn. ^C cancels the TURN (quitting is idle-only) and
/// drops a held decision key with it; `/exit` stops the turn, and the loop
/// leaves when it ends.
fn turn_input(
    driver: &mut Driver,
    frames: &mut Frames,
    turn_cancel: &CancellationToken,
    input: UiInput,
) {
    if input.is_cancel() {
        if driver.pending_decision.take().is_some() {
            driver.screen.flash = None;
        }
        turn_cancel.cancel();
        return;
    }
    if let UiInput::Resize(w) = input {
        frames.resized(w, driver.now_ms);
    }
    frames.dirty |= driver.on_input(input);
    if driver.exit_after_turn {
        turn_cancel.cancel();
    }
}

/// Poll the worker service into the shared snapshot the driver draws from.
/// Workers carry no usage tap yet, so cost renders `—` (issue #12).
#[cfg(feature = "delegation")]
fn spawn_worker_refresher(
    service: Arc<dyn WorkerService>,
    rows: Arc<Mutex<Vec<p1_tui::render::workers::WorkerRow>>>,
    cancel: CancellationToken,
) {
    use p1_tui::render::workers::{WorkerRow, WorkerState};
    use p1_workers::ChildStatus;
    tokio::spawn(async move {
        let mut started: HashMap<String, std::time::Instant> = HashMap::new();
        let mut interval = tokio::time::interval(std::time::Duration::from_millis(500));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            interval.tick().await;
            if cancel.is_cancelled() {
                return;
            }
            let list = service.list().await;
            let mut next = Vec::with_capacity(list.len());
            for (id, status) in list {
                let description = service.describe(&id).await.unwrap_or_default();
                let elapsed = match &status {
                    ChildStatus::Running => {
                        let at = started
                            .entry(id.0.clone())
                            .or_insert_with(std::time::Instant::now);
                        let secs = at.elapsed().as_secs();
                        Some(format!("{}m{:02}s", secs / 60, secs % 60))
                    }
                    _ => None,
                };
                let (state, details) = match &status {
                    ChildStatus::Running => (WorkerState::Running, Vec::new()),
                    ChildStatus::Finished(_) => (WorkerState::Done, Vec::new()),
                    ChildStatus::Cancelled => (WorkerState::Done, vec!["cancelled".into()]),
                    ChildStatus::Failed(e) => (WorkerState::Done, vec![format!("failed: {e}")]),
                };
                next.push(WorkerRow {
                    id: id.0.clone(),
                    summary: id.0.clone(),
                    route: description,
                    state,
                    elapsed,
                    cost_micro_usd: None,
                    details,
                });
            }
            *rows.lock().unwrap() = next;
        }
    });
}

/// `^C` during a turn cancels it.
fn is_cancel(key: &crossterm::event::KeyEvent) -> bool {
    key.code == crossterm::event::KeyCode::Char('c')
        && key
            .modifiers
            .contains(crossterm::event::KeyModifiers::CONTROL)
}

/// Draw one frame, presented atomically: inside a synchronized update the
/// terminal shows the whole frame or none of it (unsupported terminals ignore
/// the markers).
/// The terminal's output, held until a frame is complete. ratatui clears the
/// screen on a resize and flushes at once; a terminal without synchronized
/// output (tmux) would show it blank for the whole render. Held, the clear and
/// the new frame reach the terminal in one write.
#[derive(Default)]
struct FrameOut {
    held: Vec<u8>,
}

/// Set only while a finished frame is being committed: every other flush
/// (ratatui's own, after a clear or a cursor move) keeps the bytes held.
static FRAME_COMMIT: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

impl std::io::Write for FrameOut {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.held.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        if !FRAME_COMMIT.load(std::sync::atomic::Ordering::Relaxed) {
            return Ok(());
        }
        let mut out = std::io::stdout().lock();
        // A frame that could not be written is dropped, not resent (the next
        // one is complete by itself); the buffer never grows.
        let written = out.write_all(&self.held);
        self.held.clear();
        written.and_then(|()| out.flush())
    }
}

/// A backend whose frame is written in one piece, inside a synchronized update.
trait FrameCommit {
    fn frame_begin(&mut self);
    fn frame_commit(&mut self);
}

impl FrameCommit for ratatui::backend::CrosstermBackend<FrameOut> {
    fn frame_begin(&mut self) {
        let _ = crossterm::queue!(self, crossterm::terminal::BeginSynchronizedUpdate);
    }

    fn frame_commit(&mut self) {
        use std::sync::atomic::Ordering;
        let _ = crossterm::queue!(self, crossterm::terminal::EndSynchronizedUpdate);
        FRAME_COMMIT.store(true, Ordering::Relaxed);
        let _ = std::io::Write::flush(self);
        FRAME_COMMIT.store(false, Ordering::Relaxed);
    }
}

fn draw<B: Backend + FrameCommit>(
    terminal: &mut ratatui::Terminal<B>,
    screen: &mut Screen,
    now_ms: u64,
) {
    screen.tick(now_ms);
    terminal.backend_mut().frame_begin();
    terminal
        .draw(|frame| {
            // Focus mode: explicit (/focus) wins; otherwise automatic at 12
            // rows or fewer (SPEC §4.3a).
            screen.focus = screen.focus_explicit.unwrap_or(frame.area().height <= 12);
            p1_tui::render::screen::draw(screen, frame.area(), frame.buffer_mut(), now_ms);
            if let Some(position) = screen.cursor_position {
                frame.set_cursor_position(position);
            }
        })
        .ok();
    terminal.backend_mut().frame_commit();
}

/// Build the blocking approval view for a parked request. Every row states a
/// fact the host actually knows; nothing is filled in to look complete.
fn approval_view(request: &AuthRequest, workspace: &std::path::Path, sandbox: &str) -> Approval {
    let raw = request.call.input.raw();
    let json: Option<serde_json::Value> = serde_json::from_str(raw).ok();
    let get = |key: &str| json.as_ref()?.get(key)?.as_str().map(str::to_string);
    let name = request.call.name.as_str();
    let current = |path: &str| std::fs::read_to_string(workspace.join(path)).ok();
    match name {
        // The edit-shaped tools review as diffs (SPEC §4.4).
        "edit" => {
            let path = get("file_path").unwrap_or_default();
            let old = get("old_string").unwrap_or_default();
            let new = get("new_string").unwrap_or_default();
            let all = json
                .as_ref()
                .and_then(|j| j.get("replace_all"))
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            // `replace_all` changes every occurrence: review them all, as the
            // whole file after the edit against the file now.
            if all
                && !old.is_empty()
                && let Some(text) = current(&path)
                && text.matches(old.as_str()).count() > 1
            {
                let sites = text.matches(old.as_str()).count();
                let mut view =
                    DiffView::from_write(name, &path, &text.replace(&old, &new), Some(&text));
                view.summary = format!("replace exact string · all {sites}");
                return Approval::Diff(view);
            }
            let text = current(&path);
            let mut view = DiffView::from_edit(name, &path, &old, &new, text.as_deref(), (1, 1));
            // The review says when the edit cannot apply as asked: the
            // operator decides with that fact in view.
            // The edit tool matches across CRLF and LF line ends alike.
            let lf = |s: &str| s.replace("\r\n", "\n");
            view.note = match text
                .as_deref()
                .map(|t| lf(t).matches(lf(&old).as_str()).count())
            {
                None => Some(format!("{path} does not exist — this edit will fail")),
                Some(0) if !old.is_empty() => Some(format!(
                    "the old text is not in {path} — this edit will fail"
                )),
                Some(n) if n > 1 && !all => Some(format!(
                    "the old text matches {n} places — the edit tool will refuse it"
                )),
                _ => None,
            };
            Approval::Diff(view)
        }
        "write" => {
            let path = get("file_path").unwrap_or_default();
            let content = get("content").unwrap_or_default();
            Approval::Diff(DiffView::from_write(
                name,
                &path,
                &content,
                current(&path).as_deref(),
            ))
        }
        "patch" | "apply_patch" => {
            // `{"patch": "..."}`, or the freeform patch text itself.
            let patch = get("patch").unwrap_or_else(|| raw.to_owned());
            Approval::Diff(DiffView::from_patch(name, &patch))
        }
        // Commands prompt per SPEC §4.5: cwd, sandbox, network, reason.
        _ => {
            let home = std::env::var("HOME").unwrap_or_default();
            let cwd = workspace.display().to_string();
            let cwd = match cwd.strip_prefix(&home) {
                Some(rest) if !home.is_empty() => format!("~{rest}"),
                _ => cwd,
            };
            let mut rows = vec![("cwd".into(), cwd), ("sandbox".into(), sandbox.to_string())];
            // Neither sandbox mode isolates the network (the workspace sandbox
            // unshares the pid namespace only), so the honest value is "on".
            if name == "shell" {
                rows.push(("network".into(), "on".into()));
            }
            if let Some(task) = get("task") {
                rows.push(("task".into(), task));
            }
            rows.push((
                "reason".into(),
                match request.effect {
                    p1_contracts::Effect::Executes => "runs a process".into(),
                    p1_contracts::Effect::WritesFiles => "writes files".into(),
                    p1_contracts::Effect::Delegates => "starts an agent".into(),
                    p1_contracts::Effect::ReadOnly => "read".into(),
                },
            ));
            Approval::Permission(PermissionView {
                tool: name.to_owned(),
                command: get("command")
                    .unwrap_or_else(|| p1_tui::transcript::summarize_call(name, raw)),
                rows,
                grantable: true,
            })
        }
    }
}

/// The `/status` overlay from live state (SPEC §4.6 shape). Spend comes from
/// the same view the ledger renders: unknown is `—`, cost is dollars.
fn status_groups(driver: &Driver) -> Vec<p1_tui::render::status::StatusGroup> {
    use p1_tui::render::status::{StatusGroup, StatusRow};
    let row = |label: &str, value: String| StatusRow {
        label: label.into(),
        value,
        available: true,
    };
    let spend = driver.screen.ledger().spend;
    let known = spend.responses > 0;
    let or_unknown = |v: Option<u64>, f: fn(u64) -> String| {
        v.filter(|_| known)
            .map(f)
            .unwrap_or_else(|| p1_tui::render::UNKNOWN.into())
    };
    vec![
        StatusGroup {
            header: "ENVIRONMENT".into(),
            rows: vec![
                row("environment", driver.env.clone()),
                row("route", driver.route.clone()),
                row("profile", driver.model.clone()),
                row(
                    "access",
                    if driver.ask {
                        "ask — writes and commands wait for y/a/n".into()
                    } else {
                        "full — nothing asks (--ask to confirm)".into()
                    },
                ),
                row("sandbox", driver.sandbox.clone()),
            ],
        },
        StatusGroup {
            header: "SPEND".into(),
            rows: vec![
                row("responses", spend.responses.to_string()),
                row("in", or_unknown(spend.input, p1_tui::render::tokens)),
                row("out", or_unknown(spend.output, p1_tui::render::tokens)),
                row(
                    "cost",
                    or_unknown(spend.cost_micro_usd, |micro| {
                        format!("${}.{:04}", micro / 1_000_000, (micro % 1_000_000) / 100)
                    }),
                ),
            ],
        },
    ]
}

mod clipboard;

#[cfg(test)]
mod tests;
