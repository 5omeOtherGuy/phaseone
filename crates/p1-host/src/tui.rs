//! The TUI driver (issue #12): owns the terminal, the input loop and the agent
//! task. The UI itself is `p1-tui`'s pure state machine; this module is the
//! wiring: crossterm keys in, agent events in, authorization questions parked
//! on the screen, answers back. Composition stays ordinary — `run_agent`
//! constructs the `Frontend`, installs its sink and policy into `AgentParts`,
//! then calls [`Frontend::run`].
//!
//! The agent lives on its own task (the iris harness-actor lesson, ADR-0060):
//! `run_turn` holds `&mut Agent` for the whole turn, so the input loop can
//! never call it directly without going deaf. Commands cross a channel.

use std::collections::VecDeque;
use std::sync::Arc;

use p1_contracts::{CancellationToken, Decision, EventSink, InboxKind, TurnEnd};
use p1_core::Agent;
use p1_tui::input::{self, Command};
use p1_tui::render::diff::DiffView;
use p1_tui::render::permission::PermissionView;
use p1_tui::runtime::{AuthRequest, Stamped, TerminalGuard, TuiPolicy, TuiSink};
use p1_tui::state::{Approval, Promotion, Screen};
use ratatui::backend::Backend;
use tokio::sync::{mpsc, oneshot};

/// What the branch point knows: how to label the session.
pub struct TuiOptions {
    pub env: String,
    pub route: String,
    pub model: String,
    /// ADR-0038: full access is the default; prompts appear only under --ask.
    pub ask: bool,
    /// The workspace root: approval diff views read files from here.
    pub workspace: std::path::PathBuf,
}

/// The TUI frontend. Created before the agent so its sink and policy install
/// into `AgentParts`; [`Frontend::run`] then takes the agent over.
pub struct Frontend {
    options: TuiOptions,
    sink: Arc<TuiSink>,
    policy: Arc<TuiPolicy>,
    events: Option<mpsc::UnboundedReceiver<Stamped>>,
    auth: Option<mpsc::UnboundedReceiver<AuthRequest>>,
}

impl Frontend {
    pub fn new(options: TuiOptions) -> Self {
        let (sink, events) = TuiSink::new();
        let (policy, auth) = TuiPolicy::new(options.ask, CancellationToken::new());
        Self {
            options,
            sink: Arc::new(sink),
            policy: Arc::new(policy),
            events: Some(events),
            auth: Some(auth),
        }
    }

    /// The observation end: install as `AgentParts.events` (behind the
    /// `ActivityTee`, exactly like the line renderer).
    pub fn event_sink(&self) -> Arc<dyn EventSink> {
        self.sink.clone()
    }

    /// The decision end: install as `AgentParts.authorization`.
    pub fn authorization(&self) -> Arc<dyn p1_contracts::AuthorizationPolicy> {
        self.policy.clone()
    }

    /// The TUI shares the run's cancellation: `^C` cancels the turn; a second
    /// `^C` (or one at idle) quits. Rebind the policy to the run's token.
    pub fn bind_cancel(self, cancel: CancellationToken) -> Self {
        let (policy, auth) = TuiPolicy::new(self.options.ask, cancel);
        Self {
            policy: Arc::new(policy),
            auth: Some(auth),
            ..self
        }
    }

    /// Run the TUI to the end of the session. Returns the process exit code.
    pub async fn run(mut self, agent: Agent, cancel: CancellationToken) -> i32 {
        let Ok(_guard) = TerminalGuard::enter() else {
            eprintln!("p1 --tui: could not enter the alternate screen");
            return 1;
        };
        let backend = ratatui::backend::CrosstermBackend::new(std::io::stdout());
        let mut terminal = match ratatui::Terminal::new(backend) {
            Ok(terminal) => terminal,
            Err(error) => {
                eprintln!("p1 --tui: {error}");
                return 1;
            }
        };

        let mut screen = Screen::new(std::env::var_os("P1_REDUCED_MOTION").is_some());
        // A resumed session shows where it stands (issue #12, seam note).
        screen.transcript.paint_history(agent.history());
        let inbox = agent.inbox();
        let (agent_tx, agent_rx) = mpsc::unbounded_channel();
        tokio::spawn(agent_task(agent, agent_rx));

        let mut driver = Driver {
            screen,
            options: self.options,
            agent_tx,
            turn: None,
            inbox,
            policy: self.policy,
            pending_auth: None,
            follow_ups: VecDeque::new(),
            pending_calls: Default::default(),
            task_files: Default::default(),
            task_added: 0,
            task_removed: 0,
            exit: None,
        };

        use futures_util::StreamExt;
        let keys = Box::pin(
            crossterm::event::EventStream::new().filter_map(|event| async move {
                match event {
                    Ok(crossterm::event::Event::Key(key))
                        if key.kind == crossterm::event::KeyEventKind::Press =>
                    {
                        Some(key)
                    }
                    _ => None,
                }
            }),
        );
        drive_loop(
            &mut terminal,
            &mut driver,
            keys,
            self.events.take().expect("run once"),
            self.auth.take().expect("run once"),
            cancel,
        )
        .await
    }
}

/// One turn request to the agent task: the prompt, the turn's cancellation
/// child token, and where the end goes.
struct AgentCmd {
    text: String,
    cancel: CancellationToken,
    done: oneshot::Sender<TurnEnd>,
}

/// The agent task: owns the `Agent`, runs turns and drains the inbox after
/// each one (the interactive loop's drain rule, unchanged).
async fn agent_task(mut agent: Agent, mut rx: mpsc::UnboundedReceiver<AgentCmd>) {
    while let Some(cmd) = rx.recv().await {
        let mut end = agent.run_turn(cmd.text, cmd.cancel.clone()).await;
        while !cmd.cancel.is_cancelled() && agent.has_pending_inbox() {
            match agent.run_inbox_turn(cmd.cancel.clone()).await {
                Some(next) => end = next,
                None => break,
            }
        }
        if cmd.done.send(end).is_err() {
            return; // the UI is gone
        }
    }
}

/// The testable core: screen state plus the channels, no terminal. Keys and
/// events come in through methods; `drive_loop` is thin wiring over it.
pub(crate) struct Driver {
    screen: Screen,
    options: TuiOptions,
    agent_tx: mpsc::UnboundedSender<AgentCmd>,
    /// The running turn: its cancellation token and completion channel.
    turn: Option<(CancellationToken, oneshot::Receiver<TurnEnd>)>,
    inbox: p1_core::Inbox,
    policy: Arc<TuiPolicy>,
    /// The parked authorization being shown; answered by the decision keys.
    pending_auth: Option<AuthRequest>,
    /// Follow-ups fire only when the agent would otherwise stop (SPEC §7).
    follow_ups: VecDeque<String>,
    /// Task stats for the LEDGER's TASK section: files touched and lines
    /// added/removed by successful edit-shaped calls. Call inputs arrive on
    /// `ToolStarted`; the counts settle on `ToolFinished`.
    pending_calls: std::collections::HashMap<String, p1_contracts::ToolCall>,
    task_files: std::collections::HashSet<String>,
    task_added: u64,
    task_removed: u64,
    exit: Option<i32>,
}

impl Driver {
    fn on_key(&mut self, key: crossterm::event::KeyEvent, now_ms: u64) {
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
        // Composer editing is not modal: ordinary keys always edit.
        if self.screen.approval.is_none()
            && self.screen.picker.is_none()
            && self.screen.status.is_none()
        {
            use crossterm::event::KeyCode;
            match (key.code, key.modifiers.is_empty()) {
                (KeyCode::Char(c), true) => {
                    self.screen.composer.insert(c);
                    return;
                }
                (KeyCode::Backspace, true) => {
                    self.screen.composer.backspace();
                    return;
                }
                (KeyCode::Left, true) => {
                    self.screen.composer.left();
                    return;
                }
                (KeyCode::Right, true) => {
                    self.screen.composer.right();
                    return;
                }
                _ => {}
            }
        }
        let Some(command) = input::handle(&self.screen, key) else {
            return;
        };
        match command {
            Command::Submit(text) => {
                self.screen.composer.take();
                self.submit(text);
            }
            Command::QueueSteering(text) => {
                self.screen.composer.take();
                self.screen.queue(false, text.clone());
                self.inbox.send(InboxKind::Steering, text);
            }
            Command::QueueFollowUp(text) => {
                self.screen.composer.take();
                self.screen.queue(true, text.clone());
                self.follow_ups.push_back(text);
            }
            Command::Newline => self.screen.composer.insert('\n'),
            Command::Insert(c) => self.screen.composer.insert(c),
            Command::Backspace => self.screen.composer.backspace(),
            Command::Left => self.screen.composer.left(),
            Command::Right => self.screen.composer.right(),
            Command::CancelOrQuit => self.cancel_or_quit(),
            Command::ApproveOnce => self.answer(Decision::Permit, false),
            Command::ApproveSession | Command::ApproveProject | Command::AllFiles => {
                // Project grants share the in-memory set until a trust store
                // exists (issue #12). AllFiles is the session grant for the
                // tool under review.
                self.answer(Decision::Permit, true);
            }
            Command::Deny => self.answer(
                Decision::Deny {
                    reason: p1_tui::runtime::USER_DENY.to_string(),
                },
                false,
            ),
            Command::NextFile => {}
            Command::OpenFold => {
                // Open the most recent fold in the OUTPUT pane (SPEC §4.3).
                if let Some(id) = self.screen.transcript.latest_fold.clone()
                    && let Some(content) = self.screen.transcript.output(&id)
                {
                    self.screen.open_output(p1_tui::render::output::OutputView {
                        id,
                        lines: content.lines().map(str::to_string).collect(),
                        scroll: 0,
                    });
                }
            }
            Command::ExpandReasoning => {}
            Command::CyclePaneMode => self.screen.cycle_mode(),
            Command::CyclePaneWidth => self.screen.cycle_width(),
            Command::TogglePin => self.screen.toggle_pin(),
            Command::ToggleLedgerOverlay => {
                self.screen.ledger_overlay = !self.screen.ledger_overlay;
            }
            Command::Dismiss => {
                self.screen.picker = None;
                self.screen.status = None;
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
                self.screen.picker = None;
            }
            Command::ScrollUp => self.screen.scroll_by(10),
            Command::ScrollDown => self.screen.scroll_by(-10),
            Command::PaneUp => self.screen.scroll_output_by(-1),
            Command::PaneDown => self.screen.scroll_output_by(1),
        }
        let _ = now_ms;
    }

    /// `⏎` idle: a slash command or a prompt for the agent.
    fn submit(&mut self, text: String) {
        if let Some(command) = text.strip_prefix('/') {
            self.slash(command);
            return;
        }
        self.screen.transcript.operator(text.clone());
        self.start_turn(text);
    }

    fn slash(&mut self, command: &str) {
        let (name, arg) = command.split_once(' ').unwrap_or((command, ""));
        match name {
            "exit" | "quit" => self.exit = Some(0),
            "focus" => self.screen.focus = !self.screen.focus,
            "goal" => {
                self.screen.goal = (!arg.is_empty()).then(|| arg.to_string());
            }
            "status" => {
                self.screen.status = Some(status_groups(&self.screen, &self.options));
            }
            other => {
                self.screen.transcript.operator(format!("/{other}"));
                self.screen.transcript.note(&format!(
                    "· /{other} is not a TUI command yet — try /status, /focus, /goal, /exit"
                ));
            }
        }
    }

    /// Start a turn unless one is running; the prompt becomes a follow-up
    /// otherwise (cannot happen through `Submit`, which checks `working`, but
    /// follow-ups funnel through here).
    fn start_turn(&mut self, text: String) {
        let (done_tx, done_rx) = oneshot::channel();
        let turn_cancel = CancellationToken::new();
        if self
            .agent_tx
            .send(AgentCmd {
                text,
                cancel: turn_cancel.clone(),
                done: done_tx,
            })
            .is_ok()
        {
            self.turn = Some((turn_cancel, done_rx));
        }
    }

    fn cancel_or_quit(&mut self) {
        if let Some((turn_cancel, _)) = &self.turn {
            turn_cancel.cancel();
        } else {
            self.exit = Some(0);
        }
    }

    fn answer(&mut self, decision: Decision, grant: bool) {
        let Some(pending) = self.pending_auth.take() else {
            return;
        };
        if grant {
            self.policy.grant_session(&pending.call, &pending.identity);
        }
        pending.answer(decision);
        self.screen.approval = None;
        self.screen.pinned = false;
        self.screen.promotion = Promotion::None;
    }

    /// One observed agent event, stamped on the sink's clock.
    fn on_event(&mut self, stamped: Stamped) {
        // A delivered inbox message clears the queued steering display.
        if matches!(
            stamped.event,
            p1_contracts::AgentEvent::InboxDelivered { .. }
        ) {
            self.screen.queued.retain(|q| q.follow_up);
        }
        self.track_task(&stamped.event);
        self.screen.apply(&stamped.event, stamped.at_ms);
    }

    /// Successful edit-shaped calls move the TASK section. Reads only the
    /// call's own input (the presentation adapter's data, not tool internals);
    /// a denied or failed call counts nothing.
    fn track_task(&mut self, event: &p1_contracts::AgentEvent) {
        match event {
            p1_contracts::AgentEvent::ToolStarted { call } => {
                self.pending_calls
                    .insert(call.call_id.clone(), call.clone());
            }
            p1_contracts::AgentEvent::ToolFinished { result } => {
                let Some(call) = self.pending_calls.remove(&result.call_id) else {
                    return;
                };
                if result.status != p1_contracts::ToolStatus::Ok {
                    return;
                }
                if !matches!(call.name.as_str(), "edit" | "patch" | "write") {
                    return;
                }
                let Ok(json) = serde_json::from_str::<serde_json::Value>(call.input.raw()) else {
                    return;
                };
                let get = |key: &str| json.get(key).and_then(|v| v.as_str());
                if let Some(path) = get("file_path") {
                    self.task_files.insert(path.to_string());
                }
                self.task_removed += get("old_string").map_or(0, |s| s.lines().count() as u64);
                self.task_added += get("new_string")
                    .or_else(|| get("content"))
                    .map_or(0, |s| s.lines().count() as u64);
                self.screen.task_view = Some(p1_tui::render::ledger::Task {
                    id: None,
                    files: Some(self.task_files.len() as u64),
                    diff: Some((self.task_added, self.task_removed)),
                    journal: None,
                });
            }
            _ => {}
        }
    }

    /// A parked authorization becomes the blocking approval view (SPEC §4.4 /
    /// §4.5). The edit-shaped calls render as diffs; commands as §4.5 rows.
    fn on_auth(&mut self, request: AuthRequest) {
        let view = approval_view(&request, &self.options.workspace);
        self.screen.approval = Some(view);
        // An approval self-pins (SPEC §5): nothing may swap it away.
        self.screen.pinned = true;
        self.screen.promotion = Promotion::Blocked;
        self.pending_auth = Some(request);
    }

    /// The turn ended: run the oldest queued follow-up, or go idle.
    fn on_turn_end(&mut self, end: TurnEnd) {
        self.turn = None;
        self.screen.queued.retain(|q| q.follow_up);
        if matches!(end, TurnEnd::Cancelled) {
            self.follow_ups.clear();
            self.screen.queued.clear();
            return;
        }
        if let Some(next) = self.follow_ups.pop_front() {
            self.screen.queued.pop_front();
            self.screen.transcript.operator(next.clone());
            self.start_turn(next);
        }
    }
}

/// The render/input loop. Generic over the backend and the key stream so the
/// whole thing is drivable from tests without a TTY.
async fn drive_loop<B: Backend>(
    terminal: &mut ratatui::Terminal<B>,
    driver: &mut Driver,
    mut keys: impl futures_util::Stream<Item = crossterm::event::KeyEvent> + Unpin,
    mut events: mpsc::UnboundedReceiver<Stamped>,
    mut auth: mpsc::UnboundedReceiver<AuthRequest>,
    cancel: CancellationToken,
) -> i32 {
    use futures_util::StreamExt;
    let sink_epoch = std::time::Instant::now();
    let mut tick = tokio::time::interval(std::time::Duration::from_millis(50));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        if let Some(code) = driver.exit {
            return code;
        }
        if cancel.is_cancelled() {
            return 130;
        }
        // Draw first: the initial frame must not wait for input.
        let now_ms = sink_epoch.elapsed().as_millis() as u64;
        terminal
            .draw(|frame| {
                p1_tui::render::screen::draw(
                    &driver.screen,
                    frame.area(),
                    frame.buffer_mut(),
                    now_ms,
                )
            })
            .ok();
        tokio::select! {
            biased;
            _ = cancel.cancelled() => return 130,
            key = keys.next() => {
                match key {
                    Some(key) => driver.on_key(key, now_ms),
                    None => return 0, // input stream ended
                }
            }
            event = events.recv() => {
                match event {
                    Some(stamped) => driver.on_event(stamped),
                    None => return 0, // the agent is gone
                }
            }
            request = auth.recv() => {
                if let Some(request) = request {
                    driver.on_auth(request);
                }
            }
            end = async {
                match &mut driver.turn {
                    Some((_, done)) => done.await.unwrap_or(TurnEnd::Cancelled),
                    None => std::future::pending().await,
                }
            } => driver.on_turn_end(end),
            _ = tick.tick() => {}
        }
    }
}

/// Build the blocking approval view for a parked request.
fn approval_view(request: &AuthRequest, workspace: &std::path::Path) -> Approval {
    let raw = request.call.input.raw();
    let json: Option<serde_json::Value> = serde_json::from_str(raw).ok();
    let get = |key: &str| json.as_ref()?.get(key)?.as_str().map(str::to_string);
    match request.call.name.as_str() {
        // The edit-shaped tools review as diffs (SPEC §4.4).
        name @ ("edit" | "patch" | "write") => {
            let path = get("file_path").unwrap_or_default();
            let old = get("old_string").unwrap_or_default();
            let new = get("new_string")
                .or_else(|| get("content"))
                .unwrap_or_default();
            let current = std::fs::read_to_string(workspace.join(&path)).ok();
            Approval::Diff(DiffView::from_edit(
                name,
                &path,
                &old,
                &new,
                current.as_deref(),
                (1, 1),
            ))
        }
        // Commands prompt per SPEC §4.5.
        _ => Approval::Permission(PermissionView {
            command: get("command").unwrap_or_else(|| p1_tui::transcript::summarize_input(raw)),
            rows: vec![
                (
                    "cwd".into(),
                    std::env::current_dir()
                        .map(|p| p.display().to_string())
                        .unwrap_or_default(),
                ),
                ("tool".into(), request.call.name.clone()),
            ],
            grantable: true,
        }),
    }
}

/// The `/status` overlay from live state (SPEC §4.6 shape).
fn status_groups(
    screen: &Screen,
    options: &TuiOptions,
) -> Vec<p1_tui::render::status::StatusGroup> {
    use p1_tui::render::status::{StatusGroup, StatusRow};
    let row = |label: &str, value: String| StatusRow {
        label: label.into(),
        value,
        available: true,
    };
    let spend = &screen.spend;
    let or_unknown = |v: Option<u64>| v.map(|n| n.to_string()).unwrap_or("—".into());
    vec![
        StatusGroup {
            header: "ENVIRONMENT".into(),
            rows: vec![
                row("environment", options.env.clone()),
                row("route", options.route.clone()),
                row("profile", options.model.clone()),
            ],
        },
        StatusGroup {
            header: "SPEND".into(),
            rows: vec![
                row("in", or_unknown(spend.input)),
                row("out", or_unknown(spend.output)),
                row("cost", or_unknown(spend.cost_micro_usd)),
            ],
        },
    ]
}

#[cfg(test)]
mod tests;
