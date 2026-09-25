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
    AuthorizationPolicy, CallDescription, CancellationToken, Decision, Effect, EventSink,
    InboxKind, Tool, ToolCall, TurnEnd,
};
use p1_core::Agent;
use p1_tui::input::{self, Command};
use p1_tui::palette::ColorMode;
use p1_tui::render::diff::DiffView;
use p1_tui::render::home::HomePrelude;
use p1_tui::render::ledger::{ContextView, SessionView};
use p1_tui::render::permission::PermissionView;
use p1_tui::runtime::{AuthRequest, TerminalGuard, TuiPolicy, TuiSink, UiEvent};
use p1_tui::state::{Approval, PaneMode, Screen};
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
    /// (window_tokens, summarize_at_tokens), from `FrontEnd::context_configured`
    /// (§10 `ctx`'s denominator); `None` fields when the environment has no
    /// `[context]` section.
    context: Mutex<(Option<u64>, Option<u64>)>,
    /// The parent's route label (ADR-0049 stage 3): a `/model`/`/effort` switch
    /// moves it, the same seam `LineFrontEnd` already uses — the assembled
    /// route fills it in, `route_label()` shares it with `crate::run::ModelSwitch`.
    route_label: Arc<Mutex<String>>,
    /// The assembled parent's tools, from `FrontEnd::parent_tools` (ADR-0057):
    /// the driver describes a call from the tool that owns it, never by name.
    tools: Mutex<Arc<Vec<Arc<dyn Tool>>>>,
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
            context: Mutex::new((None, None)),
            route_label: Arc::new(Mutex::new(String::new())),
            tools: Mutex::new(Arc::new(Vec::new())),
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
        *self.route_label.lock().unwrap() = route.to_string();
    }

    /// ADR-0057: keep the assembled tools so the driver can describe a call from
    /// the tool that owns it. Announced once, right after `parent_assembled`.
    fn parent_tools(&self, tools: &[Arc<dyn Tool>]) {
        *self.tools.lock().unwrap() = Arc::new(tools.to_vec());
    }

    fn context_configured(&self, window_tokens: Option<u64>, summarize_at_tokens: Option<u64>) {
        *self.context.lock().unwrap() = (window_tokens, summarize_at_tokens);
    }

    /// ADR-0049 stage 3: shared with `crate::run::ModelSwitch`, so a
    /// successful `/model`/`/effort` switch moves the label the driver reads
    /// back into `route` — the same mechanism `LineFrontEnd` uses.
    fn route_label(&self) -> Option<Arc<Mutex<String>>> {
        Some(self.route_label.clone())
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
                self.tools.lock().unwrap().clone(),
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
            screen.home = Some(home_prelude(
                &self.options.workspace,
                branch.lock().unwrap().clone(),
                &route,
                &model,
                self.options.ask,
            ));
            screen.session = Some(session_view(
                &model,
                self.options.effort.as_deref(),
                self.options.ask,
                &self.options.sandbox,
            ));
            let (window, summarize_at) = *self.context.lock().unwrap();
            screen.context = window
                .zip(summarize_at)
                .map(|(window, summarize_at)| ContextView {
                    used: None,
                    window,
                    summarize_at,
                    parts: vec![],
                });
            screen.env = self.options.env.clone();
            screen.route = route.clone();
            // A resumed session shows where it stands (issue #12, seam note).
            screen.transcript.paint_history(agent.history());
            let worker_rows: Arc<Mutex<Vec<p1_tui::render::workers::WorkerBlock>>> =
                Arc::new(Mutex::new(Vec::new()));
            let worker_details = Arc::new(Mutex::new(HashMap::new()));
            #[cfg(feature = "delegation")]
            if let Some(service) = &workers {
                spawn_worker_refresher(
                    service.clone(),
                    worker_rows.clone(),
                    worker_details.clone(),
                    cancel.child_token(),
                );
            }
            #[cfg(feature = "delegation")]
            let worker_stops = workers
                .as_ref()
                .map(|service| spawn_worker_stopper(service.clone(), cancel.child_token()));
            #[cfg(not(feature = "delegation"))]
            let worker_stops: Option<mpsc::UnboundedSender<String>> = None;
            let mut driver = Driver {
                screen,
                env: self.options.env.clone(),
                route,
                model,
                workspace: self.options.workspace.clone(),
                sandbox: self.options.sandbox.clone(),
                ask: self.options.ask,
                policy: self.policy.clone(),
                pending_auth: VecDeque::new(),
                pinned_by_approval: false,
                follow_ups: VecDeque::new(),
                submit_pending: None,
                pending_calls: HashMap::new(),
                task_files: HashSet::new(),
                exit: None,
                inbox: agent.inbox(),
                worker_rows,
                worker_stops,
                worker_usage: HashMap::new(),
                branch,
                tools: self.tools.lock().unwrap().clone(),
                // Read once above: two `lock()` temporaries in this literal both
                // live to the end of the statement, and the second deadlocked
                // every TUI start on its own guard.
                context_window: window,
                context_warn_at: summarize_at,
                pending_worker_starts: HashMap::new(),
                worker_details,
                // ADR-0049 stage 3: `deps.model_switch` is set (in `run.rs`, this
                // crate) right before `FrontEnd::run` is called — the same
                // instant the line mode's `run_interactive` starts reading it.
                model_switch: deps.model_switch.clone(),
                environment_dirs: deps.environment_dirs.clone(),
                route_label: self.route_label.clone(),
                pending_switch: None,
                _workers: workers,
            };

            let keys = Box::pin(crossterm::event::EventStream::new().filter_map(
                |event| async move {
                    match event {
                        Ok(crossterm::event::Event::Key(key))
                            if key.kind == crossterm::event::KeyEventKind::Press =>
                        {
                            Some(Input::Key(key))
                        }
                        // A resize is a frame input too: the frame on screen is
                        // stale at the new size (`Terminal::draw` resizes its
                        // own buffer, issue #141).
                        Ok(crossterm::event::Event::Resize(..)) => Some(Input::Resize),
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
                Redraws::new(DrawCounter::default()),
            )
            .await
        })
    }

    fn finish(&self) {
        // Nothing to total up: the TUI showed spend live, and the terminal
        // was restored when the run loop's guard dropped.
    }
}

/// Usage assembled from one worker's own response events.
struct WorkerUsage {
    model: String,
    tokens: Option<u64>,
    cost_micro_usd: Option<u64>,
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
    /// ADR-0038: full access is the default; prompts appear only under --ask.
    /// §6.9's `/status`/`/access` read it (the policy itself carries no
    /// public getter — `p1-tui::runtime` is not an owned path here).
    ask: bool,
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
    /// Task stats for the LEDGER's WORKSPACE section: call inputs arrive on
    /// `ToolStarted`; a successful write-shaped call is tracked from the tool
    /// that owns it on `ToolFinished` (ADR-0057).
    pending_calls: HashMap<String, p1_contracts::ToolCall>,
    task_files: HashSet<String>,
    exit: Option<i32>,
    /// A submitted prompt waiting for the loop to start the turn (the agent
    /// borrow lives in the loop, not in the driver).
    submit_pending: Option<String>,
    /// The worker snapshot the refresher task maintains (delegation only).
    worker_rows: Arc<Mutex<Vec<p1_tui::render::workers::WorkerBlock>>>,
    /// Confirmed worker stops; the task owns the service calls off the UI loop.
    worker_stops: Option<mpsc::UnboundedSender<String>>,
    /// Usage reported by each worker's own responses; never merged into the
    /// parent's context or spend.
    worker_usage: HashMap<String, WorkerUsage>,
    inbox: p1_core::Inbox,
    /// §10 `branch`: refreshed off the render loop (`spawn_branch_refresh`,
    /// called from the async loop, never from a `Driver` method a plain
    /// `#[test]` calls directly) at start and after every `TurnFinished`.
    branch: Arc<Mutex<Option<String>>>,
    /// The assembled parent's tools, from `FrontEnd::parent_tools` (ADR-0057):
    /// `track_task` and `approval_view` describe a call from the tool that owns
    /// it, so no code here matches a tool name or an argument key.
    tools: Arc<Vec<Arc<dyn Tool>>>,
    /// §10 `ctx`'s denominator (`FrontEnd::context_configured`): the
    /// assembled `[context]` window and its summarize threshold, in tokens.
    /// Both `None` when the environment has no `[context]` section.
    context_window: Option<u64>,
    context_warn_at: Option<u64>,
    pending_worker_starts: HashMap<String, String>,
    worker_details: Arc<Mutex<HashMap<String, (String, String)>>>,
    /// ADR-0049 stage 3: `/model`/`/effort`'s switch context, already plumbed
    /// onto `HostDeps` for the line mode (`deps.model_switch`) — `None` only
    /// when the run never reached that point (never true once `run` starts).
    model_switch: Option<Arc<crate::run::ModelSwitch>>,
    /// For `/models`' listing: `crate::models::enumerate` needs only the
    /// search path, not the whole `HostDeps`.
    environment_dirs: Vec<std::path::PathBuf>,
    /// Shared with `crate::run::ModelSwitch` (`FrontEnd::route_label`): a
    /// successful switch moves it; the driver reads it back into `route`.
    route_label: Arc<Mutex<String>>,
    /// A `/model`/`/effort` typed while a turn is running: applied at the
    /// next boundary (§11: "while a turn runs the switch applies at the next
    /// boundary"), since `agent` is exclusively borrowed by the turn future
    /// until then.
    pending_switch: Option<PendingSwitch>,
    _workers: Option<Arc<dyn WorkerService>>,
}

/// A `/model`/`/effort` switch queued while a turn was running.
enum PendingSwitch {
    Model(String),
    Effort(String),
}

impl Driver {
    fn on_key(&mut self, key: crossterm::event::KeyEvent, agent: Option<&mut Agent>) {
        use crossterm::event::KeyCode;
        // `input::handle`'s own pre-check, replicated: until `^F` is applied
        // through `apply_view`, an open OUTPUT pane with no modal on screen
        // keeps scrolling on the bare arrows (this driver now calls `decide`
        // directly, so it owns the check `handle` used to make for it).
        if self.screen.approval.is_none()
            && self.screen.picker.is_none()
            && self.screen.output.is_some()
            && self.screen.pane_mode == PaneMode::Output
            && key.modifiers.is_empty()
        {
            match key.code {
                KeyCode::Up => return self.dispatch(Command::PaneUp, None),
                KeyCode::Down => return self.dispatch(Command::PaneDown, None),
                _ => {}
            }
        }
        let Some(action) = input::decide(&self.screen, key) else {
            return;
        };
        match action {
            input::Action::Command(command) => self.dispatch(command, agent),
            // These keep `input::handle`'s exact historical mapping: opening
            // the command-completion picker isn't wired to submit on `Enter`
            // anywhere yet, so routing it through `apply_view` would silently
            // swallow a typed slash command's submission instead of running
            // it (§6.10's completion flow is a `Enter`-to-complete-then-
            // `Enter`-to-submit two-step, which no test or caller here uses).
            input::Action::View(input::ViewCommand::PageUp) => {
                self.dispatch(Command::ScrollUp, None)
            }
            input::Action::View(input::ViewCommand::PageDown) => {
                self.dispatch(Command::ScrollDown, None)
            }
            input::Action::View(input::ViewCommand::OpenCompletion) => {
                self.dispatch(Command::Insert('/'), None)
            }
            input::Action::View(input::ViewCommand::EditGoal) => self.screen.edit_goal(),
            // Every other view key (§12) the screen applies to itself: full
            // review paging, menu filtering/effort-stepping, pane focus, the
            // goal editor's `esc` — none of which `input::handle` ever reached
            // (it dropped them; that is the wiring gap this task closes).
            input::Action::View(other) => self.screen.apply_view(other),
        }
    }

    /// `agent` is `Some` only when no turn is running (the loop's idle key
    /// arm): a `/model`/`/effort` switch needs it; mid-turn (`pump`'s key
    /// arm) it is exclusively borrowed by the turn future, so callers pass
    /// `None` and a switch request queues instead (§11).
    fn dispatch(&mut self, command: Command, agent: Option<&mut Agent>) {
        match command {
            Command::Submit(text) => {
                self.screen.composer.take();
                self.submit(text, agent);
            }
            Command::QueueSteering(text) => {
                self.screen.composer.take();
                self.screen.queue(false, text.clone());
                // §8.2: delivery (`InboxDelivered`) renders the queued text as a
                // tagged `OperatorTurn` — the transcript keeps its own copy so it
                // can drain exactly what the inbox actually delivered.
                self.screen.transcript.queue_steering(text.clone());
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
            Command::StopWorker(id) => {
                self.screen.stop_pending = None;
                if self
                    .worker_stops
                    .as_ref()
                    .is_some_and(|tx| tx.send(id.clone()).is_ok())
                {
                    self.screen
                        .transcript
                        .note(&format!("↳ {id} stop requested"));
                } else {
                    self.screen
                        .transcript
                        .note(&format!("↳ {id} cannot be stopped here"));
                }
            }
            Command::PaneUp => self.screen.pane_step(-1),
            Command::PaneDown => self.screen.pane_step(1),
        }
    }

    /// A submitted line: a slash command, or a prompt for the agent. The
    /// prompt waits in `submit_pending` for the loop (the agent borrow lives
    /// there, not here).
    fn submit(&mut self, text: String, agent: Option<&mut Agent>) {
        if let Some(command) = text.strip_prefix('/') {
            self.slash(command, agent);
            return;
        }
        self.screen.transcript.operator(text.clone());
        self.submit_pending = Some(text);
    }

    fn slash(&mut self, command: &str, agent: Option<&mut Agent>) {
        let (name, arg) = command.split_once(' ').unwrap_or((command, ""));
        let arg = arg.trim();
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
            // §11: `/env` is an alias of `/model` (open question §15.3, resolved
            // — the idle prelude already advertises `/env`).
            "model" | "env" => self.slash_model(arg, agent),
            "effort" => self.slash_effort(arg, agent),
            "models" => self.slash_models((!arg.is_empty()).then_some(arg)),
            "status" => {
                let output = status_command_output(self);
                self.screen.transcript.command_output(output);
            }
            "access" => {
                let output = access_command_output(self);
                self.screen.transcript.command_output(output);
            }
            "help" => self.screen.transcript.command_output(help_command_output()),
            other => {
                self.screen
                    .transcript
                    .note(&format!("· /{other} is not a command · /help"));
            }
        }
    }

    /// `/model` bare lists every model (the same listing `/models` shows);
    /// `/model REF` switches.
    fn slash_model(&mut self, reference: &str, agent: Option<&mut Agent>) {
        if reference.is_empty() {
            self.slash_models(None);
            return;
        }
        self.request_switch(PendingSwitch::Model(reference.to_string()), agent);
    }

    fn slash_effort(&mut self, level: &str, agent: Option<&mut Agent>) {
        if level.is_empty() {
            self.screen
                .transcript
                .note("· /effort needs a level · low medium high max");
            return;
        }
        self.request_switch(PendingSwitch::Effort(level.to_string()), agent);
    }

    fn slash_models(&mut self, search: Option<&str>) {
        match models_command_output(&self.environment_dirs, search) {
            Ok(output) => self.screen.transcript.command_output(output),
            Err(reason) => self.screen.transcript.note(&format!("· /models: {reason}")),
        }
    }

    /// §11: while a turn runs the switch applies at the next boundary —
    /// `agent` is `None` exactly then (mid-`pump`, borrowed by the turn).
    fn request_switch(&mut self, request: PendingSwitch, agent: Option<&mut Agent>) {
        let Some(agent) = agent else {
            self.screen
                .transcript
                .note("· model switch queued · applies at the next boundary");
            self.pending_switch = Some(request);
            return;
        };
        self.apply_switch(request, agent);
    }

    fn apply_switch(&mut self, request: PendingSwitch, agent: &mut Agent) {
        let Some(switch) = self.model_switch.clone() else {
            self.screen
                .transcript
                .note("· switch refused · no model switch is available this run");
            return;
        };
        let before = self.model.clone();
        let outcome = match &request {
            PendingSwitch::Model(reference) => crate::run::switch_model(
                &switch,
                agent,
                crate::run::SwitchRequest::Model(reference),
            ),
            PendingSwitch::Effort(level) => {
                crate::run::switch_model(&switch, agent, crate::run::SwitchRequest::Effort(level))
            }
        };
        self.report_switch(before, outcome);
    }

    /// §11: `switch_model` result → `MetaRow · model a → b · from the next
    /// turn`, or `✗ switch refused · <reason> · kept still on a`.
    fn report_switch(&mut self, before: String, outcome: Result<String, String>) {
        match outcome {
            Ok(after) => {
                self.screen
                    .transcript
                    .note(&format!("· model {before} → {after} · from the next turn"));
                if let Some(env) = after.split('/').next() {
                    self.env = env.to_string();
                    self.screen.env = self.env.clone();
                }
                self.screen.statusbar.effort = after.split(':').nth(1).map(str::to_string);
                self.model = after;
                self.screen.session = Some(session_view(
                    &self.model,
                    self.screen.statusbar.effort.as_deref(),
                    self.ask,
                    &self.sandbox,
                ));
                // `switch_model` moved the shared route label; the chip and
                // `/status`'s `route` row follow it.
                self.route = self.route_label.lock().unwrap().clone();
                self.screen.statusbar.model = Some(format!("{}/{}", self.route, self.model));
            }
            Err(reason) => {
                self.screen.transcript.note(&format!(
                    "✗ switch refused · {reason} · kept still on {before}"
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
        let mut rows = self.worker_rows.lock().unwrap().clone();
        self.screen.statusbar.workers = status::running_workers(&rows);
        for row in &mut rows {
            if let Some(usage) = self.worker_usage.get(&row.id) {
                row.model = Some(usage.model.clone());
                row.tokens = usage.tokens;
                row.cost_micro_usd = usage.cost_micro_usd;
            }
        }
        self.screen.sync_workers(rows);
    }

    /// One UI event: an agent event (parent or a tagged worker) or a marker.
    fn on_ui_event(&mut self, ui: UiEvent) {
        match ui {
            UiEvent::WorkerStarted(id) => {
                self.screen.transcript.note(&format!("↳ {id} started"));
            }
            UiEvent::Agent(stamped) => {
                if let Some(id) = &stamped.worker {
                    // Worker streams stay OUT of the parent's transcript and
                    // usage; a finished worker is worth one quiet line.
                    if let p1_contracts::AgentEvent::ResponseCompleted { model, usage, .. } =
                        &stamped.event
                    {
                        let cost = usage.and_then(|usage| usage.cost_micro_usd);
                        let worker_usage =
                            self.worker_usage
                                .entry(id.clone())
                                .or_insert_with(|| WorkerUsage {
                                    model: String::new(),
                                    tokens: None,
                                    cost_micro_usd: Some(0),
                                });
                        worker_usage.model = model.clone();
                        worker_usage.tokens = status::usage_input_total(usage.as_ref());
                        worker_usage.cost_micro_usd = match (worker_usage.cost_micro_usd, cost) {
                            (Some(total), Some(cost)) => Some(total + cost),
                            _ => None,
                        };
                    }
                    if let p1_contracts::AgentEvent::TurnFinished { end } = &stamped.event {
                        let state = match end {
                            TurnEnd::Completed { .. } => "finished",
                            TurnEnd::Cancelled => "cancelled",
                            _ => "failed",
                        };
                        self.screen.transcript.note(&format!("↳ {id} {state}"));
                    }
                    self.screen.apply_worker(id, &stamped.event, stamped.at_ms);
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
                    if let Some(count) = text.strip_prefix("\0p1-idle-summary-count:")
                        && let Ok(count) = count.parse::<usize>()
                    {
                        let warning = "context summaries since the last change";
                        self.screen.transcript.blocks.retain(|block| {
                            !matches!(
                                block,
                                p1_tui::transcript::Block::Meta { text }
                                    if text.contains(warning)
                            )
                        });
                        if count > 0 {
                            self.screen.transcript.note(&format!(
                                "· {count} context summaries since the last change"
                            ));
                        }
                        return;
                    }
                    self.screen.transcript.note(&format!("· {text}"));
                    return;
                }
                if let p1_contracts::AgentEvent::ResponseCompleted { usage, .. } = &stamped.event {
                    // §10 `ctx`: `FrontEnd::context_configured` carries the
                    // assembled `[context]` window and threshold; unknown (no
                    // `[context]` section) stays `None`, never a guessed `0`.
                    let used = status::usage_input_total(usage.as_ref());
                    self.screen.context = self.context_window.zip(self.context_warn_at).map(
                        |(window, summarize_at)| ContextView {
                            used,
                            window,
                            summarize_at,
                            parts: vec![],
                        },
                    );
                    let (ctx, warn) =
                        status::ctx_status(used, self.context_window, self.context_warn_at);
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

    /// Successful write-shaped calls move the WORKSPACE section. The tool that
    /// owns the call says what it touches (`effect` + `describe`, ADR-0057); a
    /// denied or failed call counts nothing, and a call whose tool is no longer
    /// assembled is not guessed at.
    fn track_task(&mut self, event: &p1_contracts::AgentEvent) {
        match event {
            p1_contracts::AgentEvent::ToolStarted { call } => {
                self.pending_calls
                    .insert(call.call_id.clone(), call.clone());
                #[cfg(feature = "delegation")]
                if call.name == "worker_start"
                    && let Ok(input) = serde_json::from_str::<serde_json::Value>(call.input.raw())
                {
                    let task = input.get("task").and_then(|v| v.as_str()).unwrap_or("");
                    self.pending_worker_starts.insert(
                        call.call_id.clone(),
                        task.lines().next().unwrap_or("").to_string(),
                    );
                }
            }
            p1_contracts::AgentEvent::ToolFinished { result } => {
                #[cfg(feature = "delegation")]
                if let Some(task) = self.pending_worker_starts.remove(&result.call_id)
                    && result.status == p1_contracts::ToolStatus::Ok
                    && let Some((id, grants)) = worker_start_result(&result.content)
                {
                    self.worker_details
                        .lock()
                        .unwrap()
                        .insert(id, (task, grants));
                }
                let Some(call) = self.pending_calls.remove(&result.call_id) else {
                    return;
                };
                if result.status != p1_contracts::ToolStatus::Ok {
                    return;
                }
                let Some(target) = tracked_target(&self.tools, &call) else {
                    return;
                };
                self.task_files.insert(target);
                self.screen.workspace = Some(p1_tui::render::ledger::WorkspaceView {
                    files: Some(self.task_files.len() as u64),
                    // The line counts are the tool's own knowledge and are not part
                    // of `describe`; the diff seam that counts every workspace change
                    // is unbuilt (§10, §14.4), so it stays unknown, never guessed.
                    diff: None,
                    journal: None,
                });
            }
            _ => {}
        }
    }

    /// The assembled tool that owns a call, by its model-facing name. `None` when
    /// the name is not assembled (a re-grant or a model switch may have replaced
    /// the set); such a call is never described by a guess.
    fn tool_for(&self, call: &ToolCall) -> Option<&Arc<dyn Tool>> {
        self.tools
            .iter()
            .find(|tool| tool.declaration().name == call.name)
    }

    /// The call's own description (`Tool::describe`, ADR-0057). A call whose tool
    /// is not assembled falls back to the trait's default: the name in `target`.
    fn describe_call(&self, call: &ToolCall) -> CallDescription {
        match self.tool_for(call) {
            Some(tool) => tool.describe(call),
            None => CallDescription {
                verb: "call",
                target: Some(call.name.clone()),
                edit: None,
                destructive: false,
            },
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
        // The front request is the one on screen; the rest wait behind it (`N of M pending`).
        self.screen.approvals_waiting = self.pending_auth.len().saturating_sub(1);
        if self.screen.approval.is_some() {
            return;
        }
        let Some(request) = self.pending_auth.front() else {
            return;
        };
        self.screen.approval_tool = request.call.name.clone();
        let description = self.describe_call(&request.call);
        self.screen.approval = Some(approval_view(
            &request.call,
            request.effect,
            &description,
            &self.workspace,
            &self.sandbox,
        ));
        self.screen.detach_worker();
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
fn home_prelude(
    workspace: &std::path::Path,
    branch: Option<String>,
    route: &str,
    model: &str,
    ask: bool,
) -> HomePrelude {
    let path = display_workspace_path(workspace, std::env::var_os("HOME").as_deref());
    HomePrelude {
        version: env!("CARGO_PKG_VERSION").into(),
        path,
        branch,
        state: vec![],
        items: vec![
            ("/resume".into(), "reopen a previous session".into()),
            ("/env".into(), format!("{route} · {model}")),
            (
                "/access".into(),
                if ask {
                    "ask · prompts on"
                } else {
                    "full · --ask to confirm"
                }
                .into(),
            ),
            ("/goal".into(), "set the session objective".into()),
        ],
    }
}

fn display_workspace_path(workspace: &std::path::Path, home: Option<&std::ffi::OsStr>) -> String {
    home.and_then(|home| workspace.strip_prefix(std::path::Path::new(home)).ok())
        .map(|tail| {
            let tail = tail.to_string_lossy();
            if tail.is_empty() {
                "~".to_string()
            } else {
                format!("~/{}", tail.trim_start_matches('/'))
            }
        })
        .unwrap_or_else(|| workspace.to_string_lossy().into_owned())
}

fn session_view(model: &str, effort: Option<&str>, ask: bool, sandbox: &str) -> SessionView {
    SessionView {
        model: model.to_string(),
        effort: effort.unwrap_or("default").to_string(),
        access: if ask { "ask" } else { "full" }.into(),
        sandbox: sandbox.to_string(),
    }
}

#[cfg(feature = "delegation")]
fn worker_start_result(content: &str) -> Option<(String, String)> {
    let rest = content.strip_prefix("Started worker ")?;
    let (identity, remainder) = rest.split_once(" on ")?;
    let id = identity.trim();
    let (_, tools) = remainder.split_once(" with tools: ")?;
    let grants = tools.split(". You will be notified").next()?.trim();
    (!id.is_empty() && !grants.is_empty()).then(|| (id.to_string(), grants.to_string()))
}

fn spawn_branch_refresh(workspace: std::path::PathBuf, branch: Arc<Mutex<Option<String>>>) {
    tokio::spawn(async move {
        let next = tokio::task::spawn_blocking(move || status::git_branch(&workspace))
            .await
            .unwrap_or(None);
        *branch.lock().unwrap() = next;
    });
}

/// Worker rows are re-read at 4 Hz while idle (issue #141). The refresher task
/// publishes them every 500 ms, and a frame follows only when they changed, so
/// the poll itself costs a lock and a comparison — never a render.
const WORKER_POLL: std::time::Duration = std::time::Duration::from_millis(250);

/// The idle heartbeat (issue #141): the fastest an idle TUI needs to look at
/// the fields that move on their own — the §10 clock (minute granularity), the
/// branch, the worker count. A frame still follows only when their text moved,
/// so an idle screen draws at most once a minute.
const IDLE_HEARTBEAT: std::time::Duration = std::time::Duration::from_millis(1000);

/// While a turn runs the `▪▪▪` pulse (SPEC §2) is the one frame input with no
/// text to compare: colour alone moves, so the heartbeat itself must draw it.
/// Its cell cycle is 1.1 s with a 180 ms stagger, so 5 Hz is the spinner's
/// whole need and anything faster is frames nobody can see.
const SPINNER_HEARTBEAT: std::time::Duration = std::time::Duration::from_millis(200);

/// One terminal input the loop acts on. A resize carries nothing: it only says
/// the frame on screen is stale (`Terminal::draw` resizes its own buffer).
#[derive(Debug, Clone, Copy)]
enum Input {
    Key(crossterm::event::KeyEvent),
    Resize,
}

/// The frames one loop drew. Production ignores it; the idle-CPU tests (issue
/// #141) hold a clone and count frames under fake time — a number taken from
/// the draw path itself, never a timing assertion.
#[derive(Clone, Default)]
pub(crate) struct DrawCounter(Arc<std::sync::atomic::AtomicUsize>);

impl DrawCounter {
    /// Frames drawn so far — the tests' read side.
    #[cfg(test)]
    fn count(&self) -> usize {
        self.0.load(std::sync::atomic::Ordering::Relaxed)
    }

    fn bump(&self) {
        self.0.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }
}

/// The loop's draw decision (issue #141). A frame follows a key, a UI/agent
/// event, an authorization, a resize, worker rows that differ from the last
/// drawn ones, or a statusline whose text moved — and, besides those frame
/// inputs, only the heartbeat while a `▪▪▪` pulse is on screen.
struct Redraws {
    /// A frame input changed; the next iteration draws.
    dirty: bool,
    /// The heartbeat ticked while a `▪▪▪` pulse was on screen.
    pulse: bool,
    /// Nothing has been drawn yet: the first frame is unconditional.
    first: bool,
    /// The worker rows of the last drawn frame.
    rows: Vec<p1_tui::render::workers::WorkerBlock>,
    /// The statusline fields of the last drawn frame.
    status: p1_tui::render::statusbar::StatusBar,
    /// Frames drawn, for the tests.
    draws: DrawCounter,
}

impl Redraws {
    fn new(draws: DrawCounter) -> Self {
        Self {
            dirty: false,
            pulse: false,
            first: true,
            rows: Vec::new(),
            status: Default::default(),
            draws,
        }
    }

    /// Whether this iteration draws a frame. Every time-based field except the
    /// pulse changes its TEXT (the clock, an elapsed time, the worker count), so
    /// the two comparisons below catch it and the heartbeat needs no rate check
    /// of its own.
    fn due(&self, screen: &Screen, pulsing: bool) -> bool {
        self.first
            || self.dirty
            || (self.pulse && pulsing)
            || self.rows != screen.workers.workers
            || self.status != screen.statusbar
    }

    /// Remember what was just drawn.
    fn drawn(&mut self, screen: &Screen) {
        self.dirty = false;
        self.pulse = false;
        self.first = false;
        self.rows = screen.workers.workers.clone();
        self.status = screen.statusbar.clone();
    }

    /// The heartbeat to wait at until the next check: the spinner's while a
    /// `▪▪▪` pulse is on screen (only its colour moves), the idle rate
    /// otherwise, where the wake merely re-reads the clock's text.
    fn heartbeat(&self, pulsing: bool) -> std::time::Duration {
        if pulsing {
            SPINNER_HEARTBEAT
        } else {
            IDLE_HEARTBEAT
        }
    }
}

/// Whether a `▪▪▪` pulse is on screen this frame (SPEC §2): a running call row,
/// or the turn working row that stands in for a live turn with nothing running.
/// Reduced motion freezes the pulse and a parked approval replaces the working
/// row — neither leaves anything to animate.
fn pulsing(screen: &Screen, now_ms: u64) -> bool {
    if screen.reduced_motion {
        return false;
    }
    match &screen.attached {
        // A worker's own transcript is on screen: its pulse, not the parent's.
        Some(worker) => {
            worker.transcript.call_running() || worker.transcript.turn_working(now_ms).is_some()
        }
        None => {
            screen.transcript.call_running()
                || (screen.working.is_some() && screen.approval.is_none())
        }
    }
}

/// The loop's heartbeat timer. It wakes at the rate the last frame asked for,
/// so a rate change restarts the wait instead of firing a catch-up tick.
struct Heartbeat {
    interval: tokio::time::Interval,
    period: std::time::Duration,
}

impl Heartbeat {
    fn new() -> Self {
        Self {
            interval: Self::every(IDLE_HEARTBEAT),
            period: IDLE_HEARTBEAT,
        }
    }

    /// One interval that first ticks a whole period from now — the heartbeat
    /// never fires at the instant the rate changes.
    fn every(period: std::time::Duration) -> tokio::time::Interval {
        let mut interval = tokio::time::interval_at(tokio::time::Instant::now() + period, period);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        interval
    }

    /// Wait `period` for the next tick; a period change takes effect at once.
    fn set(&mut self, period: std::time::Duration) {
        if self.period != period {
            self.period = period;
            self.interval = Self::every(period);
        }
    }

    async fn tick(&mut self) {
        self.interval.tick().await;
    }
}

/// One iteration's frame decision (issue #141): republish the fields that move
/// without an event (worker rows, the §10 clock and branch), draw a frame only
/// when a frame input changed, and say which heartbeat to wait at next.
fn frame_if_due<B: Backend>(
    redraws: &mut Redraws,
    terminal: &mut ratatui::Terminal<B>,
    driver: &mut Driver,
    now_ms: u64,
    color_mode: ColorMode,
) -> std::time::Duration {
    driver.sync_workers();
    driver.sync_status(now_ms);
    let pulsing = pulsing(&driver.screen, now_ms);
    if redraws.due(&driver.screen, pulsing) {
        draw(
            terminal,
            &mut driver.screen,
            now_ms,
            color_mode,
            &redraws.draws,
        );
        redraws.drawn(&driver.screen);
    }
    redraws.heartbeat(pulsing)
}

/// The render/input loop over a borrowed agent. Turns are pinned futures
/// inside this function: polled every wakeup, never dropped mid-flight.
///
/// Idle (issue #141): a frame follows a frame input — a key, a UI/agent event,
/// an authorization, a resize, worker rows or statusline text that changed — or
/// the heartbeat's pulse. Nothing else draws, and the worker poll never does.
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
    mut redraws: Redraws,
) -> i32
where
    B: Backend,
    K: futures_util::Stream<Item = Input> + Unpin,
{
    let mut workers =
        tokio::time::interval_at(tokio::time::Instant::now() + WORKER_POLL, WORKER_POLL);
    workers.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut heartbeat = Heartbeat::new();
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
            // A new turn is a frame input of its own: the operator line, the
            // working row. The pump's first iteration draws it.
            redraws.dirty = true;
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
                &mut redraws,
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
                    &mut redraws,
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
            // §11: a `/model`/`/effort` typed mid-turn applies here, at the
            // first boundary `agent` is free again.
            if let Some(pending) = driver.pending_switch.take() {
                driver.apply_switch(pending, agent);
            }
            // The turn's end moved the screen too: the working row leaves, a
            // cancelled turn drops its queue, a switch renames the chip.
            redraws.dirty = true;
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
        // Idle: republish what moves by itself, draw only when a frame input
        // changed, then wait for anything at all.
        heartbeat.set(frame_if_due(
            &mut redraws,
            terminal,
            driver,
            sink.now_ms(),
            color_mode,
        ));
        tokio::select! {
            biased;
            _ = cancel.cancelled() => return 130,
            input = keys.next() => {
                let Some(input) = input else { return 0 };
                redraws.dirty = true;
                match input {
                    // The frame on screen is stale at the new size; the next
                    // iteration draws it.
                    Input::Resize => {}
                    Input::Key(key) if is_cancel(&key) => driver.exit = Some(0),
                    Input::Key(key) => {
                        driver.on_key(key, Some(agent));
                        if let Some(text) = driver.submit_pending.take() {
                            prompt = Some(text);
                        }
                    }
                }
            }
            ui = events.recv() => {
                match ui {
                    Some(ui) => {
                        redraws.dirty = true;
                        driver.on_ui_event(ui);
                    }
                    None => return 0,
                }
            }
            request = auth.recv() => {
                if let Some(request) = request {
                    redraws.dirty = true;
                    driver.on_auth(request);
                }
            }
            // Paces re-reading the worker rows; the frame still follows only
            // when `frame_if_due` above sees them differ.
            _ = workers.tick() => {}
            _ = heartbeat.tick() => redraws.pulse = true,
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
                        &mut redraws,
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
                if let Some(pending) = driver.pending_switch.take() {
                    driver.apply_switch(pending, agent);
                }
                redraws.dirty = true;
            }
        }
    }
}

/// Poll one turn-shaped future to completion while the UI stays live: keys,
/// events and parked authorizations are handled on every wakeup, and the
/// future is re-polled — never dropped — until it resolves.
///
/// Frames follow the idle rule (issue #141), except that a `▪▪▪` pulse is on
/// screen while the turn runs: the heartbeat then draws it at the spinner's
/// rate instead of once a second.
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
    redraws: &mut Redraws,
    mut turn: std::pin::Pin<Box<F>>,
) -> TurnEnd
where
    B: Backend,
    K: futures_util::Stream<Item = Input> + Unpin,
    F: std::future::Future<Output = TurnEnd>,
{
    let mut workers =
        tokio::time::interval_at(tokio::time::Instant::now() + WORKER_POLL, WORKER_POLL);
    workers.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut heartbeat = Heartbeat::new();
    let mut keys_done = false;
    loop {
        heartbeat.set(frame_if_due(
            redraws,
            terminal,
            driver,
            sink.now_ms(),
            color_mode,
        ));
        tokio::select! {
            biased;
            end = &mut turn => return end,
            input = keys.next(), if !keys_done => {
                let Some(input) = input else { keys_done = true; continue };
                redraws.dirty = true;
                match input {
                    Input::Resize => {}
                    Input::Key(key) if is_cancel(&key) => {
                        // ^C during a turn cancels the TURN; quitting is idle-only.
                        turn_cancel.cancel();
                    }
                    // A turn's future owns `agent`; a `/model`/`/effort`
                    // mid-turn queues instead (§11).
                    Input::Key(key) => driver.on_key(key, None),
                }
            }
            ui = events.recv() => {
                if let Some(ui) = ui {
                    redraws.dirty = true;
                    driver.on_ui_event(ui);
                }
            }
            request = auth.recv() => {
                if let Some(request) = request {
                    redraws.dirty = true;
                    driver.on_auth(request);
                }
            }
            _ = workers.tick() => {}
            _ = heartbeat.tick() => redraws.pulse = true,
        }
    }
}

/// Worker run clocks, reset when a continued worker starts a new run.
#[derive(Default)]
struct WorkerClocks {
    started: HashMap<String, std::time::Instant>,
    frozen: HashMap<String, std::time::Duration>,
}

impl WorkerClocks {
    fn observe(&mut self, id: &str, running: bool, now: std::time::Instant) -> Option<String> {
        if running {
            // worker_continue starts a new run under the same id, so reset its clock.
            self.frozen.remove(id);
            let since = *self.started.entry(id.to_string()).or_insert(now);
            Some(clock_text(now.saturating_duration_since(since)))
        } else {
            if let Some(since) = self.started.remove(id) {
                self.frozen
                    .insert(id.to_string(), now.saturating_duration_since(since));
            }
            self.frozen.get(id).map(|elapsed| clock_text(*elapsed))
        }
    }
}

fn clock_text(elapsed: std::time::Duration) -> String {
    let secs = elapsed.as_secs();
    format!("{}m{:02}s", secs / 60, secs % 60)
}

/// Poll the worker service into the shared snapshot the driver draws from.
#[cfg(feature = "delegation")]
fn spawn_worker_refresher(
    service: Arc<dyn WorkerService>,
    rows: Arc<Mutex<Vec<p1_tui::render::workers::WorkerBlock>>>,
    details: Arc<Mutex<HashMap<String, (String, String)>>>,
    cancel: CancellationToken,
) {
    use p1_tui::render::workers::{BlockState, WorkerBlock};
    use p1_workers::ChildStatus;
    tokio::spawn(async move {
        let mut clocks = WorkerClocks::default();
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
                let elapsed = clocks.observe(
                    &id.0,
                    matches!(&status, ChildStatus::Running),
                    std::time::Instant::now(),
                );
                let (state, activity) = match &status {
                    ChildStatus::Running => (BlockState::Running, String::new()),
                    ChildStatus::Finished(_) => (BlockState::Done, String::new()),
                    ChildStatus::Cancelled => (BlockState::Cancelled, String::new()),
                    ChildStatus::Failed(e) => (BlockState::Failed, format!("failed: {e}")),
                };
                let (task, grants) = details
                    .lock()
                    .unwrap()
                    .get(&id.0)
                    .cloned()
                    .unwrap_or_default();
                next.push(WorkerBlock {
                    id: id.0.clone(),
                    task,
                    route: description,
                    model: None,
                    state,
                    elapsed,
                    cost_micro_usd: None,
                    tokens: None,
                    // The child's configured window is not known to the host.
                    context_window: None,
                    grants,
                    activity,
                });
            }
            *rows.lock().unwrap() = next;
        }
    });
}

/// Apply confirmed worker stops outside the UI loop; the refresher publishes the new state.
#[cfg(feature = "delegation")]
fn spawn_worker_stopper(
    service: Arc<dyn WorkerService>,
    cancel: CancellationToken,
) -> mpsc::UnboundedSender<String> {
    let (tx, mut rx) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = cancel.cancelled() => return,
                received = rx.recv() => match received {
                    Some(id) => {
                        let _ = service.cancel(&p1_workers::ChildId(id)).await;
                    }
                    None => return,
                },
            }
        }
    });
    tx
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
///
/// Every frame goes through here and bumps `draws` (issue #141), so the loop's
/// tests count frames on the draw path itself rather than by timing.
fn draw<B: Backend>(
    terminal: &mut ratatui::Terminal<B>,
    screen: &mut Screen,
    now_ms: u64,
    color_mode: ColorMode,
    draws: &DrawCounter,
) {
    draws.bump();
    terminal
        .draw(|frame| {
            // Focus mode: explicit (/focus) wins; otherwise automatic at 12
            // rows or fewer (SPEC §4.3a).
            screen.focus = screen.focus_explicit.unwrap_or(frame.area().height <= 12);
            p1_tui::render::screen::draw(screen, frame.area(), frame.buffer_mut(), now_ms);
            if color_mode != ColorMode::TrueColor {
                p1_tui::palette::degrade(frame.buffer_mut(), color_mode);
            }
            // The hardware cursor sits on the composer's text cell (§8.1), when it is editable.
            if let Some(cursor) = screen.cursor {
                frame.set_cursor_position(cursor);
            }
        })
        .ok();
}

/// The file a successful call writes, from the tool that owns it (ADR-0057):
/// `effect()` says the call changes files, `describe()` says which one. `None` for
/// a call that does not write, whose tool is not assembled, or whose tool has no
/// target — never a guess from another tool's argument keys. The LEDGER's
/// WORKSPACE tracking is exactly this, so it is one testable function.
pub fn tracked_target(tools: &[Arc<dyn Tool>], call: &ToolCall) -> Option<String> {
    let tool = tools
        .iter()
        .find(|tool| tool.declaration().name == call.name)?;
    if tool.effect(call) != Effect::WritesFiles {
        return None;
    }
    tool.describe(call).target
}

/// Build the blocking approval view for a parked request. §7.1 reuses the tool's
/// own parsed description: edit tools supply their diff preview, run tools supply
/// their command, and everything else takes the permission form. `apply_patch`
/// intentionally has no single-file preview (§7.5), so it takes the permission
/// form without the host interpreting its freeform patch text.
fn approval_view(
    call: &ToolCall,
    effect: Effect,
    description: &CallDescription,
    workspace: &std::path::Path,
    sandbox: &str,
) -> Approval {
    let raw = call.input.raw();
    if description.verb == "edit"
        && let Some(edit) = &description.edit
    {
        let current = std::fs::read_to_string(workspace.join(&edit.path)).ok();
        let mut view = DiffView::from_edit(
            &call.name,
            &edit.path,
            &edit.old,
            &edit.new,
            current.as_deref(),
            (1, 1),
        );
        view.grantable = !description.destructive;
        return Approval::Diff(view);
    }
    // Commands prompt per SPEC §4.5: cwd, sandbox, network, reason. The command is
    // the tool's own description of the call; a tool that is not a `run` shows the
    // generic input summary.
    let command = if description.verb == "run" {
        description.target.clone().unwrap_or_default()
    } else {
        p1_tui::transcript::summarize_input(raw)
    };
    Approval::Permission(PermissionView {
        command,
        rows: vec![
            ("cwd".into(), workspace.display().to_string()),
            ("sandbox".into(), sandbox.to_string()),
            ("network".into(), "off".into()),
            (
                "reason".into(),
                format!(
                    "{}{}",
                    match effect {
                        p1_contracts::Effect::Executes => "runs a process",
                        p1_contracts::Effect::WritesFiles => "writes files",
                        p1_contracts::Effect::Delegates => "starts an agent",
                        p1_contracts::Effect::ReadOnly => "read",
                    },
                    if description.destructive {
                        " · destructive"
                    } else {
                        ""
                    }
                ),
            ),
        ],
        grantable: !description.destructive,
    })
}

/// §6.9/§11: `/status` is a settled `CommandOutput`, not the retired overlay
/// — the facts today's overlay showed (environment, route, model, effort,
/// access, sandbox, spend), one row each.
fn status_command_output(driver: &Driver) -> p1_tui::transcript::CommandOutput {
    use p1_tui::transcript::{CommandOutput, CommandRow};
    let entry = |key: &str, text: String| CommandRow::Entry {
        key: key.into(),
        text,
    };
    let spend = status::spend_string(driver.screen.spend.cost_micro_usd)
        .unwrap_or_else(|| p1_tui::render::UNKNOWN.into());
    CommandOutput {
        command: "/status".into(),
        argument: String::new(),
        facts: String::new(),
        body: vec![
            entry("environment", driver.env.clone()),
            entry("route", driver.route.clone()),
            entry("model", driver.model.clone()),
            entry(
                "effort",
                driver
                    .screen
                    .statusbar
                    .effort
                    .clone()
                    .unwrap_or_else(|| "default".into()),
            ),
            entry(
                "access",
                if driver.ask {
                    "ask · prompts on".into()
                } else {
                    "full · --ask to confirm".into()
                },
            ),
            entry("sandbox", driver.sandbox.clone()),
            entry("spend", spend),
        ],
    }
}

/// §11: `/access` — policy facts; `--ask` restarts to change (ADR-0038: the
/// access mode is fixed per process, never switched live).
fn access_command_output(driver: &Driver) -> p1_tui::transcript::CommandOutput {
    use p1_tui::transcript::{CommandOutput, CommandRow};
    CommandOutput {
        command: "/access".into(),
        argument: String::new(),
        facts: String::new(),
        body: vec![
            CommandRow::Entry {
                key: "mode".into(),
                text: if driver.ask {
                    "ask · every tool prompts".into()
                } else {
                    "full · every tool runs".into()
                },
            },
            CommandRow::Entry {
                key: "sandbox".into(),
                text: driver.sandbox.clone(),
            },
            CommandRow::Entry {
                key: "change".into(),
                text: "--ask restarts the session to change it".into(),
            },
        ],
    }
}

/// §11: `/help` — the command list with descriptions. `/resume` is left off:
/// the TUI does not implement it yet (a listed-but-inert command would be
/// worse than an incomplete list).
fn help_command_output() -> p1_tui::transcript::CommandOutput {
    use p1_tui::transcript::{CommandOutput, CommandRow};
    const COMMANDS: &[(&str, &str)] = &[
        ("/model [REF]", "switch model or effort · alias /env"),
        ("/effort LEVEL", "low medium high max"),
        (
            "/goal [TEXT]",
            "set or clear the session objective · ^G edits",
        ),
        ("/focus [on|off]", "transcript only"),
        ("/status", "session facts"),
        ("/access", "access and sandbox · fixed per process"),
        ("/models [SEARCH]", "every model p1 can run"),
        ("/exit", "quit"),
    ];
    let mut body = vec![CommandRow::Head("COMMANDS".into())];
    body.extend(COMMANDS.iter().map(|(key, text)| CommandRow::Entry {
        key: (*key).into(),
        text: (*text).into(),
    }));
    CommandOutput {
        command: "/help".into(),
        argument: String::new(),
        // `/help` itself is the one command not listed as a row.
        facts: format!("{} commands", COMMANDS.len() + 1),
        body,
    }
}

/// §11: `/models` — the `p1 models` listing, without the credential column
/// (that probe reads the auth store; §7.3's fact rule applies just the same
/// — a name it cannot derive here is omitted, not guessed).
fn models_command_output(
    environment_dirs: &[std::path::PathBuf],
    search: Option<&str>,
) -> Result<p1_tui::transcript::CommandOutput, String> {
    use p1_tui::transcript::{CommandOutput, CommandRow};
    let all = crate::models::enumerate(environment_dirs)?;
    let rows = crate::models::search(&all, search);
    let mut body = vec![CommandRow::Head("MODELS".into())];
    body.extend(rows.iter().map(|model| CommandRow::Entry {
        key: model.id(),
        text: model.efforts_line(),
    }));
    Ok(CommandOutput {
        command: "/models".into(),
        argument: search.unwrap_or_default().to_string(),
        facts: format!("{} models", rows.len()),
        body,
    })
}

#[cfg(test)]
mod tests;

/// The worker-end wiring (ADR-0050 item 6): the sentence the line front end prints
/// reaches the TUI as the note the driver already renders for a provider notice, so
/// the TUI shows exactly the host's own line — evidence included (ADR-0051 item 3).
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
                    evidence: None,
                }),
                missing_tool_calls: vec![("edit".into(), 2)],
            },
        );
        // A `done` the worker could not verify: the note carries the same sentence the
        // host's `worker_end_note` prints.
        front_end.worker_ended(
            "w2",
            "deepseek2",
            &WorkerReport {
                tools: vec!["read".into(), "edit".into(), "finish".into()],
                finish: Some(FinishReport {
                    status: "done".into(),
                    needs: None,
                    summary: Some("edited the file".into()),
                    evidence: Some("not verified; parent verification required".into()),
                }),
                missing_tool_calls: Vec::new(),
            },
        );

        let mut notices = Vec::new();
        while let Ok(UiEvent::Agent(stamped)) = events.try_recv() {
            assert!(
                stamped.worker.is_none(),
                "the note is the parent's, so the driver renders it in the transcript"
            );
            notices.push(stamped.event);
        }
        assert_eq!(
            notices,
            vec![
                p1_contracts::AgentEvent::ProviderNotice {
                    text: "worker w1 (claude/sonnet; read, finish) blocked: needs edit — tried \
                           edit x2"
                        .into()
                },
                p1_contracts::AgentEvent::ProviderNotice {
                    text: "worker w2 (deepseek2; read, edit, finish) done — not verified; parent \
                           verification required"
                        .into()
                },
            ]
        );
    }
}
