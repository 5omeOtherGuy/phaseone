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
use p1_tui::state::{Approval, Promotion, Screen};
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
        _deps: &'a HostDeps,
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
            let backend = ratatui::backend::CrosstermBackend::new(std::io::stdout());
            let mut terminal = match ratatui::Terminal::new(backend) {
                Ok(terminal) => terminal,
                Err(error) => {
                    eprintln!("p1 --tui: {error}");
                    return 1;
                }
            };

            let mut screen = Screen::new(std::env::var_os("P1_REDUCED_MOTION").is_some());
            // The §4.1 idle prelude: version line, one sentence of state, the
            // four affordances — then the transcript takes over.
            screen
                .transcript
                .blocks
                .push(p1_tui::transcript::Block::Info {
                    lines: vec![
                        format!(
                            "p1 {}   {}",
                            env!("CARGO_PKG_VERSION"),
                            self.options.workspace.display()
                        ),
                        String::new(),
                        "  /resume     reopen a previous session".into(),
                        format!("  /env        {}", self.options.env),
                        format!(
                            "  /access     {}",
                            if self.options.ask { "ask" } else { "full" }
                        ),
                        "  /goal       set the session objective".into(),
                    ],
                });
            // A resumed session shows where it stands (issue #12, seam note).
            screen.transcript.paint_history(agent.history());
            let (route, model) = self.labels.lock().unwrap().clone().unwrap_or_default();
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
                policy: self.policy.clone(),
                pending_auth: None,
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
            };

            let keys = Box::pin(crossterm::event::EventStream::new().filter_map(
                |event| async move {
                    match event {
                        Ok(crossterm::event::Event::Key(key))
                            if key.kind == crossterm::event::KeyEventKind::Press =>
                        {
                            Some(key)
                        }
                        _ => None,
                    }
                },
            ));
            drive_loop(
                &mut terminal,
                &mut driver,
                agent,
                keys,
                events.take().expect("run once"),
                auth_rx.take().expect("run once"),
                cancel,
                &self.sink,
            )
            .await
        })
    }

    fn finish(&self) {
        // Nothing to total up: the TUI showed spend live, and the terminal
        // was restored when the run loop's guard dropped.
    }
}

/// The UI state plus the wiring the loop needs. No terminal in here: keys and
/// events enter through methods, so the whole driver is channel-testable.
pub(crate) struct Driver {
    screen: Screen,
    env: String,
    route: String,
    model: String,
    workspace: std::path::PathBuf,
    policy: Arc<TuiPolicy>,
    /// The parked authorization being shown; answered by the decision keys.
    pending_auth: Option<AuthRequest>,
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
}

impl Driver {
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
        // Composer editing is not modal: ordinary keys always edit.
        if self.screen.approval.is_none()
            && self.screen.picker.is_none()
            && self.screen.status.is_none()
        {
            use crossterm::event::KeyCode;
            match (key.code, plain) {
                (KeyCode::Char(c), true) => {
                    self.screen.composer.insert(c);
                    return;
                }
                (KeyCode::Backspace, _) => {
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
            // ^C is handled by the loop: cancel the turn, quit at idle.
            Command::CancelOrQuit => {}
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
            Command::NextFile => {}
            Command::OpenFold => {
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
            Command::ExpandReasoning => self.screen.transcript.toggle_reasoning(),
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
    }

    /// A submitted line: a slash command, or a prompt for the agent. The
    /// prompt waits in `submit_pending` for the loop (the agent borrow lives
    /// there, not here).
    fn submit(&mut self, text: String) {
        if let Some(command) = text.strip_prefix('/') {
            self.slash(command);
            return;
        }
        self.screen.transcript.operator(text.clone());
        self.submit_pending = Some(text);
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
                self.screen.status = Some(status_groups(self));
            }
            other => {
                self.screen.transcript.operator(format!("/{other}"));
                self.screen.transcript.note(&format!(
                    "· /{other} is not a TUI command yet — try /status, /focus, /goal, /exit"
                ));
            }
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

    /// Pull the refresher's snapshot into the screen (SPEC §5 promotion).
    fn sync_workers(&mut self) {
        let rows = self.worker_rows.lock().unwrap().clone();
        self.screen.sync_workers(rows);
    }

    /// One UI event: an agent event (parent or a tagged worker) or a marker.
    fn on_ui_event(&mut self, ui: UiEvent) {
        match ui {
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
    /// §4.5): edit-shaped calls review as diffs, commands as §4.5 rows.
    fn on_auth(&mut self, request: AuthRequest) {
        let view = approval_view(&request, &self.workspace);
        self.screen.approval = Some(view);
        // An approval self-pins (SPEC §5): nothing may swap it away.
        self.screen.pinned = true;
        self.screen.promotion = Promotion::Blocked;
        self.pending_auth = Some(request);
    }

    /// The turn ended. On cancellation every queued input is dropped — a
    /// cancelled turn means "stop", not "continue with the queue".
    fn note_turn_end(&mut self, end: &TurnEnd) {
        self.screen.queued.retain(|q| q.follow_up);
        if matches!(end, TurnEnd::Cancelled) {
            self.follow_ups.clear();
            self.screen.queued.clear();
        }
    }

    /// The oldest queued follow-up, fired only when the agent would stop.
    fn take_follow_up(&mut self) -> Option<String> {
        let next = self.follow_ups.pop_front()?;
        self.screen.queued.pop_front();
        self.screen.transcript.operator(next.clone());
        Some(next)
    }
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
    B: Backend,
    K: futures_util::Stream<Item = crossterm::event::KeyEvent> + Unpin,
{
    let mut tick = tokio::time::interval(std::time::Duration::from_millis(50));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut prompt: Option<String> = None;
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
            let child = cancel.child_token();
            let end = pump(
                terminal,
                driver,
                &mut keys,
                &mut events,
                &mut auth,
                sink,
                &child,
                Box::pin(agent.run_turn(text, child.clone())),
            )
            .await;
            driver.note_turn_end(&end);
            while !child.is_cancelled() && agent.has_pending_inbox() {
                let end = pump(
                    terminal,
                    driver,
                    &mut keys,
                    &mut events,
                    &mut auth,
                    sink,
                    &child,
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
            if matches!(end, TurnEnd::Cancelled) {
                continue;
            }
            if let Some(follow_up) = driver.take_follow_up() {
                prompt = Some(follow_up);
            }
            continue;
        }
        // Idle: draw, then wait for anything.
        draw(terminal, &driver.screen, sink.now_ms());
        tokio::select! {
            biased;
            _ = cancel.cancelled() => return 130,
            key = keys.next() => {
                let Some(key) = key else { return 0 };
                if is_cancel(&key) {
                    driver.exit = Some(0);
                    continue;
                }
                driver.on_key(key);
                if let Some(text) = driver.submit_pending.take() {
                    prompt = Some(text);
                }
            }
            ui = events.recv() => {
                match ui {
                    Some(ui) => driver.on_ui_event(ui),
                    None => return 0,
                }
            }
            request = auth.recv() => {
                if let Some(request) = request {
                    driver.on_auth(request);
                }
            }
            _ = tick.tick() => { driver.sync_workers(); }
            _ = agent.inbox_ready() => {
                // A worker's completion arrived at idle: run it as a turn so
                // the same pump handles keys and approvals.
                prompt = Some(String::new());
            }
        }
    }
}

/// Poll one turn-shaped future to completion while the UI stays live: keys,
/// events and parked authorizations are handled on every wakeup, and the
/// future is re-polled — never dropped — until it resolves.
#[allow(clippy::too_many_arguments)]
async fn pump<B, K, F>(
    terminal: &mut ratatui::Terminal<B>,
    driver: &mut Driver,
    keys: &mut K,
    events: &mut mpsc::UnboundedReceiver<UiEvent>,
    auth: &mut mpsc::UnboundedReceiver<AuthRequest>,
    sink: &TuiSink,
    turn_cancel: &CancellationToken,
    mut turn: std::pin::Pin<Box<F>>,
) -> TurnEnd
where
    B: Backend,
    K: futures_util::Stream<Item = crossterm::event::KeyEvent> + Unpin,
    F: std::future::Future<Output = TurnEnd>,
{
    let mut tick = tokio::time::interval(std::time::Duration::from_millis(50));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        draw(terminal, &driver.screen, sink.now_ms());
        tokio::select! {
            biased;
            end = &mut turn => return end,
            key = keys.next() => {
                let Some(key) = key else { continue };
                if is_cancel(&key) {
                    // ^C during a turn cancels the TURN; quitting is idle-only.
                    turn_cancel.cancel();
                    continue;
                }
                driver.on_key(key);
            }
            ui = events.recv() => {
                if let Some(ui) = ui {
                    driver.on_ui_event(ui);
                }
            }
            request = auth.recv() => {
                if let Some(request) = request {
                    driver.on_auth(request);
                }
            }
            _ = tick.tick() => { driver.sync_workers(); }
        }
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

/// `^C`: cancel during a turn, quit at idle.
fn is_cancel(key: &crossterm::event::KeyEvent) -> bool {
    key.code == crossterm::event::KeyCode::Char('c')
        && key
            .modifiers
            .contains(crossterm::event::KeyModifiers::CONTROL)
}

/// Draw one frame.
fn draw<B: Backend>(terminal: &mut ratatui::Terminal<B>, screen: &Screen, now_ms: u64) {
    terminal
        .draw(|frame| {
            p1_tui::render::screen::draw(screen, frame.area(), frame.buffer_mut(), now_ms)
        })
        .ok();
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
                ("cwd".into(), workspace.display().to_string()),
                ("tool".into(), request.call.name.clone()),
            ],
            grantable: true,
        }),
    }
}

/// The `/status` overlay from live state (SPEC §4.6 shape).
fn status_groups(driver: &Driver) -> Vec<p1_tui::render::status::StatusGroup> {
    use p1_tui::render::status::{StatusGroup, StatusRow};
    let row = |label: &str, value: String| StatusRow {
        label: label.into(),
        value,
        available: true,
    };
    let spend = &driver.screen.spend;
    let or_unknown = |v: Option<u64>| v.map(|n| n.to_string()).unwrap_or("—".into());
    vec![
        StatusGroup {
            header: "ENVIRONMENT".into(),
            rows: vec![
                row("environment", driver.env.clone()),
                row("route", driver.route.clone()),
                row("profile", driver.model.clone()),
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
