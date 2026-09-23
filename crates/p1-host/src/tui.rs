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
use p1_tui::face::{TargetKind, ToolDescriber};
use p1_tui::input::{self, Command};
use p1_tui::palette::ColorMode;
use p1_tui::render::diff::DiffView;
use p1_tui::render::permission::PermissionView;
use p1_tui::runtime::{AuthRequest, TerminalGuard, TuiPolicy, TuiSink, UiEvent};
use p1_tui::state::{Approval, Screen};
use p1_tui::transcript::Transcript;
use ratatui::backend::Backend;
use tokio::sync::mpsc;

use crate::HostDeps;
use crate::activity::Completion;
use crate::cli::Options;
use crate::frontend::{FrontEnd, WorkerService};
use crate::run::StallGuard;

mod describer;
mod status;

use describer::HostDescriber;

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
    /// §10 `effort`: an explicit `--effort`, already resolved at CLI parse time.
    /// `None` is the adapter default, which the statusline already renders as
    /// `default` (§10's "known default" rule) — never re-resolved from the
    /// assembled environment, which is not known this early (`run_agent`
    /// builds this before `assemble`).
    pub effort: Option<String>,
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

    /// A worker's end as ONE transcript note (ADR-0050 item 6), through the mechanism
    /// the TUI already has for provider notices: a parent-tagged `ProviderNotice` the
    /// driver renders `· …`. Display only — no history, no journal, nothing the model
    /// sees — so a worker that lacked a tool is visible without the parent's help.
    #[cfg(feature = "delegation")]
    fn worker_ended(&self, worker_id: &str, description: &str, report: &p1_workers::WorkerReport) {
        self.sink.emit(p1_contracts::AgentEvent::ProviderNotice {
            text: crate::render::worker_end_note(worker_id, description, report),
        });
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
            // §2: the process environment decides once; every frame degrades to it
            // (`draw` below) — truecolor stays a no-op.
            let color_mode = ColorMode::detect(&std::env::vars().collect());

            let mut screen = Screen::new(std::env::var_os("P1_REDUCED_MOTION").is_some());
            // §7.1/§7.3: the ONE describer, installed once (and shared with the
            // driver below — `approval_view` reuses its classification instead of
            // a second tool-name list). `p1-tui` never sees a tool name from here on.
            let describer = Arc::new(HostDescriber::new(
                self.options.workspace.clone(),
                self.options.sandbox.clone(),
            ));
            screen.transcript = Transcript::with_describer(describer.clone());
            let (route, model) = self.labels.lock().unwrap().clone().unwrap_or_default();
            // §10 statusline: the chip and the static fields the driver never
            // recomputes (workers/ctx/spend/clock move every frame instead).
            screen.statusbar.model = Some(format!("{route}/{model}"));
            screen.statusbar.effort = self.options.effort.clone();
            screen.statusbar.repo = status::repo_name(&self.options.workspace);
            // Bounded and off the render loop: a `git` that hangs must not hang
            // the first frame, and a missing/slow `git` just omits `branch`.
            let workspace = self.options.workspace.clone();
            let initial_branch = tokio::time::timeout(
                std::time::Duration::from_millis(200),
                tokio::task::spawn_blocking(move || status::git_branch(&workspace)),
            )
            .await
            .ok()
            .and_then(Result::ok)
            .flatten();
            let branch = Arc::new(Mutex::new(initial_branch));
            screen.statusbar.branch = branch.lock().unwrap().clone();
            // §4.1 idle prelude: version line, one sentence of state, the
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
                        format!("  /env        {route} · {model}"),
                        format!(
                            "  /access     {}",
                            if self.options.ask {
                                "ask · prompts on"
                            } else {
                                "full · --ask to confirm"
                            }
                        ),
                        "  /goal       set the session objective".into(),
                    ],
                });
            screen.env = self.options.env.clone();
            screen.route = route.clone();
            // A resumed session shows where it stands (issue #12, seam note).
            screen.transcript.paint_history(agent.history());
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
                branch,
                describer,
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
                color_mode,
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
    /// §10 `branch`: refreshed off the render loop (`spawn_branch_refresh`,
    /// called from the async loop, never from a `Driver` method a plain
    /// `#[test]` calls directly) at start and after every `TurnFinished`.
    branch: Arc<Mutex<Option<String>>>,
    /// The same describer installed on `screen.transcript` (§7.1): the one
    /// other host-side place that used to know a tool name by matching it
    /// (`approval_view`'s diff-vs-permission choice) reuses its classification
    /// instead of a second list.
    describer: Arc<HostDescriber>,
    _workers: Option<Arc<dyn WorkerService>>,
}

impl Driver {
    fn on_key(&mut self, key: crossterm::event::KeyEvent) {
        use crossterm::event::KeyCode;
        // `input::handle`'s own pre-check, replicated: until `^F` is applied
        // through `apply_view`, an open OUTPUT pane with no modal on screen
        // keeps scrolling on the bare arrows (this driver now calls `decide`
        // directly, so it owns the check `handle` used to make for it).
        if self.screen.approval.is_none()
            && self.screen.picker.is_none()
            && self.screen.status.is_none()
            && self.screen.output.is_some()
            && key.modifiers.is_empty()
        {
            match key.code {
                KeyCode::Up => return self.dispatch(Command::PaneUp),
                KeyCode::Down => return self.dispatch(Command::PaneDown),
                _ => {}
            }
        }
        let Some(action) = input::decide(&self.screen, key) else {
            return;
        };
        match action {
            input::Action::Command(command) => self.dispatch(command),
            // These keep `input::handle`'s exact historical mapping: opening
            // the command-completion picker isn't wired to submit on `Enter`
            // anywhere yet, so routing it through `apply_view` would silently
            // swallow a typed slash command's submission instead of running
            // it (§6.10's completion flow is a `Enter`-to-complete-then-
            // `Enter`-to-submit two-step, which no test or caller here uses).
            input::Action::View(input::ViewCommand::PageUp) => self.dispatch(Command::ScrollUp),
            input::Action::View(input::ViewCommand::PageDown) => self.dispatch(Command::ScrollDown),
            input::Action::View(input::ViewCommand::OpenCompletion) => {
                self.dispatch(Command::Insert('/'))
            }
            input::Action::View(input::ViewCommand::EditGoal) => self.screen.edit_goal(),
            // Every other view key (§12) the screen applies to itself: full
            // review paging, menu filtering/effort-stepping, pane focus, the
            // goal editor's `esc` — none of which `input::handle` ever reached
            // (it dropped them; that is the wiring gap this task closes).
            input::Action::View(other) => self.screen.apply_view(other),
        }
    }

    fn dispatch(&mut self, command: Command) {
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
            // Never produced by `decide` (`^G` maps to `ViewCommand::EditGoal`,
            // handled directly in `on_key`); kept so the match stays
            // exhaustive over every `Command` variant, and correct — not the
            // blank `/goal ` this used to insert — if it ever is.
            Command::EditGoal => self.screen.edit_goal(),
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
            // `/focus` toggles; `/focus on|off` are deterministic, and `off`
            // returns to the automatic short-terminal policy (SPEC §4.3a).
            "focus" => {
                self.screen.focus_explicit = match arg {
                    "on" => Some(true),
                    "off" => Some(false),
                    _ => {
                        let on = !self.screen.focus;
                        Some(on)
                    }
                };
            }
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
        let Some(pending) = self.pending_auth.pop_front() else {
            return;
        };
        if grant {
            self.policy.grant_session(&pending.call, &pending.identity);
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
        self.screen.approval = None;
        if self.pinned_by_approval {
            self.screen.pinned = false;
            self.pinned_by_approval = false;
        }
    }

    /// Pull the refresher's snapshot into the screen (SPEC §5 promotion) and
    /// the statusline's §10 `▪ N workers` count.
    fn sync_workers(&mut self) {
        let rows = self.worker_rows.lock().unwrap().clone();
        self.screen.statusbar.workers = status::running_workers(&rows);
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
                // ADR-0048: a provider notice is display-only — one quiet
                // transcript note, like every other note the driver adds.
                if let p1_contracts::AgentEvent::ProviderNotice { text } = &stamped.event {
                    self.screen.transcript.note(&format!("· {text}"));
                    return;
                }
                if let p1_contracts::AgentEvent::ResponseCompleted { usage, .. } = &stamped.event {
                    // §10 `ctx`: no seam threads the configured context window to
                    // the TUI yet (`FrontEnd::parent_assembled` carries only
                    // route/model — frontend.rs is not an owned path here), so
                    // this always resolves to `None`/`—`; the math itself is
                    // proven directly in `status::ctx_status`'s own tests.
                    let (ctx, warn) =
                        status::ctx_status(status::usage_input_total(usage.as_ref()), None, None);
                    self.screen.statusbar.ctx = ctx;
                    self.screen.statusbar.ctx_warn = warn;
                }
                self.track_task(&stamped.event);
                self.screen.apply(&stamped.event, stamped.at_ms);
                // §10 `spend`: `Screen::apply` above just updated `screen.spend`
                // (`ResponseCompleted`/every event); mirror it into the
                // statusline's compact `$0.00` (or `—`, e.g. a subscription
                // route, which never reports a cost).
                self.screen.statusbar.spend =
                    status::spend_string(self.screen.spend.cost_micro_usd);
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

    /// A parked authorization queues; the front one becomes the blocking
    /// approval view (SPEC §4.4 / §4.5): edit-shaped calls review as diffs,
    /// commands as §4.5 rows.
    fn on_auth(&mut self, request: AuthRequest) {
        self.pending_auth.push_back(request);
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
        self.screen.approval = Some(approval_view(
            request,
            &self.describer,
            &self.workspace,
            &self.sandbox,
        ));
        // An approval self-pins (SPEC §5): nothing may swap it away.
        if !self.screen.pinned {
            self.screen.pinned = true;
            self.pinned_by_approval = true;
        }
    }

    /// The turn ended. On cancellation every queued input is dropped — a
    /// cancelled turn means "stop", not "continue with the queue".
    fn note_turn_end(&mut self, end: &TurnEnd) {
        self.screen.queued.retain(|q| q.follow_up);
        if matches!(end, TurnEnd::Cancelled) {
            self.follow_ups.clear();
            self.screen.queued.clear();
            self.drop_pending_auth();
        }
    }

    /// The oldest queued follow-up, fired only when the agent would stop.
    fn take_follow_up(&mut self) -> Option<String> {
        let next = self.follow_ups.pop_front()?;
        self.screen.queued.pop_front();
        self.screen.transcript.operator(next.clone());
        Some(next)
    }

    /// §10 fields that move every frame regardless of what event caused it:
    /// the clock (from the session-relative `now_ms` every draw already
    /// carries) and the branch (a background task keeps `self.branch`
    /// current; this just republishes its latest value).
    fn sync_status(&mut self, now_ms: u64) {
        self.screen.statusbar.clock = Some(status::clock(now_ms));
        self.screen.statusbar.branch = self.branch.lock().unwrap().clone();
    }
}

/// §10 `branch`: re-read `git` in the background and publish it once done —
/// called from the async loop only (at start, and after every `TurnFinished`),
/// never from a `Driver` method a plain `#[test]` calls directly (a bare
/// `#[test]` has no tokio runtime for `tokio::spawn` to run on).
fn spawn_branch_refresh(workspace: std::path::PathBuf, branch: Arc<Mutex<Option<String>>>) {
    tokio::spawn(async move {
        let next = tokio::task::spawn_blocking(move || status::git_branch(&workspace))
            .await
            .unwrap_or(None);
        *branch.lock().unwrap() = next;
    });
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
    color_mode: ColorMode,
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
            driver.policy.set_turn(Some(child.clone()));
            // Bind mutably: the drain loop below can run more than one
            // inbox sub-turn, and the end that decides whether to stop is
            // the LAST one. A `let` inside the loop would shadow this and
            // the check would read the first turn's end instead.
            let mut end = pump(
                terminal,
                driver,
                &mut keys,
                &mut events,
                &mut auth,
                sink,
                &child,
                color_mode,
                Box::pin(agent.run_turn(text, child.clone())),
            )
            .await;
            driver.note_turn_end(&end);
            spawn_branch_refresh(driver.workspace.clone(), driver.branch.clone());
            while !child.is_cancelled() && agent.has_pending_inbox() {
                end = pump(
                    terminal,
                    driver,
                    &mut keys,
                    &mut events,
                    &mut auth,
                    sink,
                    &child,
                    color_mode,
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
                spawn_branch_refresh(driver.workspace.clone(), driver.branch.clone());
            }
            driver.policy.set_turn(None);
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
        // Idle: draw, then wait for anything.
        driver.sync_status(sink.now_ms());
        draw(terminal, &mut driver.screen, sink.now_ms(), color_mode);
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
                // A worker's completion arrived at idle: drain it through the
                // inbox path — never as a phantom empty user turn.
                let child = cancel.child_token();
                driver.policy.set_turn(Some(child.clone()));
                while agent.has_pending_inbox() {
                    let end = pump(
                        terminal,
                        driver,
                        &mut keys,
                        &mut events,
                        &mut auth,
                        sink,
                        &child,
                        color_mode,
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
                    spawn_branch_refresh(driver.workspace.clone(), driver.branch.clone());
                }
                driver.policy.set_turn(None);
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
    color_mode: ColorMode,
    mut turn: std::pin::Pin<Box<F>>,
) -> TurnEnd
where
    B: Backend,
    K: futures_util::Stream<Item = crossterm::event::KeyEvent> + Unpin,
    F: std::future::Future<Output = TurnEnd>,
{
    let mut tick = tokio::time::interval(std::time::Duration::from_millis(50));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut keys_done = false;
    loop {
        driver.sync_status(sink.now_ms());
        draw(terminal, &mut driver.screen, sink.now_ms(), color_mode);
        tokio::select! {
            biased;
            end = &mut turn => return end,
            key = keys.next(), if !keys_done => {
                let Some(key) = key else { keys_done = true; continue };
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

/// Draw one frame, then degrade the whole buffer to the process's colour mode
/// (§2): truecolor needs no pass (`palette::degrade`'s RGB→indexed path is
/// for 256/no-colour only), so it is skipped rather than called as a no-op.
fn draw<B: Backend>(
    terminal: &mut ratatui::Terminal<B>,
    screen: &mut Screen,
    now_ms: u64,
    color_mode: ColorMode,
) {
    terminal
        .draw(|frame| {
            // Focus mode: explicit (/focus) wins; otherwise automatic at 12
            // rows or fewer (SPEC §4.3a).
            screen.focus = screen.focus_explicit.unwrap_or(frame.area().height <= 12);
            p1_tui::render::screen::draw(screen, frame.area(), frame.buffer_mut(), now_ms);
            if color_mode != ColorMode::TrueColor {
                p1_tui::palette::degrade(frame.buffer_mut(), color_mode);
            }
            // The text cursor lives in the composer, unless a modal owns keys.
            if screen.approval.is_none() && screen.picker.is_none() && screen.status.is_none() {
                let area = frame.area();
                let queued = screen.queued.len();
                let (col, row) = p1_tui::render::composer::cursor_cell(
                    &screen.composer,
                    queued,
                    area.width as usize,
                );
                let composer_rows = 1 + queued + 1; // input line(s) + hints
                let y = area.height.saturating_sub(composer_rows as u16) + row as u16;
                frame.set_cursor_position((col.min(area.width as usize - 1) as u16, y));
            }
        })
        .ok();
}

/// Build the blocking approval view for a parked request. §7.1: the diff-vs-
/// permission choice reuses the describer's own classification (a path
/// target with an old/new pair to show) instead of a second tool-name list
/// — `edit`/`write` supply both; `apply_patch`'s multi-file patch text has
/// neither key, so it still takes the permission form (unchanged from
/// before: a full patch-diff review is a separate, unbuilt seam, §7.5).
fn approval_view(
    request: &AuthRequest,
    describer: &HostDescriber,
    workspace: &std::path::Path,
    sandbox: &str,
) -> Approval {
    let raw = request.call.input.raw();
    let json: Option<serde_json::Value> = serde_json::from_str(raw).ok();
    let get = |key: &str| json.as_ref()?.get(key)?.as_str().map(str::to_string);
    let is_path_target = describer.call(&request.call).kind == TargetKind::Path;
    let old = get("old_string");
    let new = get("new_string").or_else(|| get("content"));
    if is_path_target && (old.is_some() || new.is_some()) {
        let path = get("file_path").unwrap_or_default();
        let old = old.unwrap_or_default();
        let new = new.unwrap_or_default();
        let current = std::fs::read_to_string(workspace.join(&path)).ok();
        return Approval::Diff(DiffView::from_edit(
            &request.call.name,
            &path,
            &old,
            &new,
            current.as_deref(),
            (1, 1),
        ));
    }
    // Commands prompt per SPEC §4.5: cwd, sandbox, network, reason.
    Approval::Permission(PermissionView {
        command: get("command").unwrap_or_else(|| p1_tui::transcript::summarize_input(raw)),
        rows: vec![
            ("cwd".into(), workspace.display().to_string()),
            ("sandbox".into(), sandbox.to_string()),
            ("network".into(), "off".into()),
            (
                "reason".into(),
                match request.effect {
                    p1_contracts::Effect::Executes => "runs a process".into(),
                    p1_contracts::Effect::WritesFiles => "writes files".into(),
                    p1_contracts::Effect::Delegates => "starts an agent".into(),
                    p1_contracts::Effect::ReadOnly => "read".into(),
                },
            ),
        ],
        grantable: true,
    })
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
    let or_unknown = |v: Option<u64>| {
        v.map(p1_tui::render::tokens)
            .unwrap_or_else(|| p1_tui::render::UNKNOWN.into())
    };
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

/// The worker-end wiring (ADR-0050 item 6): the sentence the line front end prints
/// reaches the TUI as the note the driver already renders for a provider notice.
#[cfg(all(test, feature = "delegation"))]
mod worker_end_tests {
    use super::*;
    use p1_workers::{FinishReport, WorkerReport};

    #[test]
    fn a_workers_end_becomes_one_parent_tagged_provider_notice() {
        let front_end = TuiFrontEnd::new(
            TuiOptions {
                env: "claude".into(),
                ask: false,
                workspace: std::path::PathBuf::from("/workspace"),
                sandbox: "off".into(),
                effort: None,
            },
            CancellationToken::new(),
        );
        let mut events = front_end
            .events
            .lock()
            .unwrap()
            .take()
            .expect("the UI loop has not started");
        front_end.worker_ended(
            "w1",
            "claude/sonnet",
            &WorkerReport {
                tools: vec!["read".into(), "finish".into()],
                finish: Some(FinishReport {
                    status: "blocked".into(),
                    needs: Some("edit".into()),
                    summary: Some("cannot write".into()),
                }),
                missing_tool_calls: vec![("edit".into(), 2)],
            },
        );

        let Some(UiEvent::Agent(stamped)) = events.try_recv().ok() else {
            panic!("the note must reach the UI loop");
        };
        assert!(
            stamped.worker.is_none(),
            "the note is the parent's, so the driver renders it in the transcript"
        );
        assert_eq!(
            stamped.event,
            p1_contracts::AgentEvent::ProviderNotice {
                text: "worker w1 (claude/sonnet; read, finish) blocked: needs edit — tried \
                       edit x2"
                    .into()
            }
        );
    }
}
