//! The headless and interactive drivers, plus the delegation wiring.
//!
//! Headless runs one turn and then drains inbox turns, waiting for running
//! children so a parent that started a worker is woken by its completion.
//! Interactive prompts on stderr and drains inbox turns without waiting on
//! children. First Ctrl-C cancels the run (and, at exit, the children); second
//! Ctrl-C returns 130 immediately.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

#[cfg(feature = "delegation")]
use std::sync::OnceLock;
#[cfg(feature = "delegation")]
use std::sync::atomic::AtomicUsize;

#[cfg(feature = "delegation")]
use p1_assembly::Catalog;
use p1_assembly::{Assembled, Substitutions, assemble, load_environment};
use p1_contracts::{
    AgentEvent, BoxFuture, CacheKeySupport, CancellationToken, CommitSink, ContextError,
    ContextInput, ContextPolicy, EventSink, JournalRecord, Prepared, ProviderErrorKind, TurnEnd,
};
use p1_core::{Agent, AgentParts, ResumeReport};
#[cfg(feature = "delegation")]
use p1_journal::MemoryJournal;

#[cfg(feature = "delegation")]
use crate::SharedWriter;
use crate::activity::{ActivityLog, ActivityTee, Completion, CompletionHub};
use crate::catalog::build_catalog;
use crate::cli::{self, Command, Options};
use crate::policy::HostPolicy;
use crate::render::Renderer;
use crate::session;
use crate::{HostDeps, InterruptSource};
use p1_tool_finish::Accepted;

#[cfg(feature = "delegation")]
use p1_workers::{AgentFactory, ChildAgent, ChildSpec, ChildStatus, InProcessWorkers};

/// Exit codes (the process contract).
pub const EXIT_OK: i32 = 0;
pub const EXIT_FAILURE: i32 = 1;
pub const EXIT_USAGE: i32 = 2;
pub const EXIT_CANCELLED: i32 = 130;
/// The model called `finish` with `status: "blocked"`.
pub const EXIT_BLOCKED: i32 = 3;
/// The model kept stopping without finishing and the continuation budget ran out.
pub const EXIT_STALLED: i32 = 4;

/// The ONE message printed when the §3c stall guard fires. `<N>` is the configured
/// `--max-idle-summaries` bound.
pub fn stall_message(max_idle_summaries: usize) -> String {
    format!(
        "stalled: {max_idle_summaries} context summaries without a change to the workspace — the \
         task does not fit the configured context (see [context] in the environment), or it is too \
         large for one job"
    )
}

/// The ONE message the host sends after a premature stop in an unattended run.
/// Committed as a normal `UserInput` record, so the journal shows every
/// continuation.
pub const CONTINUATION_MESSAGE: &str = "You ended your turn without calling finish. You are running unattended: nobody will answer a question or confirm a plan, and this task authorizes you to continue on your own. Continue the work now. When it is complete and verified, call finish with status \"done\"; if something outside your control stops you, call finish with status \"blocked\".";

/// The ONE message the host sends after a transient provider failure in an
/// unattended run (completion.md §3b). Committed as a normal `UserInput` record,
/// so the journal shows every provider retry.
pub const PROVIDER_RETRY_MESSAGE: &str = "The connection to the model failed and the last response was lost; nothing else changed. Continue the work now.";

/// The default context policy: the history is sent unchanged.
#[derive(Clone, Copy, Default)]
pub struct DefaultContext;

impl ContextPolicy for DefaultContext {
    fn prepare<'a>(
        &'a self,
        _input: ContextInput<'a>,
    ) -> BoxFuture<'a, Result<Option<Prepared>, ContextError>> {
        Box::pin(async { Ok(None) })
    }
}

/// The context policy for an assembled agent (context.md §3): a
/// `SummarizingContext` when the environment opts in with `[context]`,
/// passthrough otherwise. The host is the composition root: `p1-assembly` only
/// carries the plain settings and the prompt override.
fn agent_context(assembled: &Assembled) -> Result<Arc<dyn ContextPolicy>, String> {
    let Some(settings) = &assembled.resolved.context else {
        return Ok(Arc::new(DefaultContext));
    };
    let config = p1_context::ContextConfig {
        window_tokens: settings.window_tokens,
        output_headroom_tokens: settings.output_headroom_tokens,
        summarize_at_tokens: settings.summarize_at_tokens,
        keep_recent_tokens: settings.keep_recent_tokens,
        user_verbatim_tokens: settings.user_verbatim_tokens,
        tool_result_excerpt_chars: settings.tool_result_excerpt_chars,
    };
    let prompt = assembled
        .resolved
        .summarize_prompt
        .clone()
        .unwrap_or_else(|| p1_context::DEFAULT_SUMMARIZER_PROMPT.to_string());
    let policy = p1_context::SummarizingContext::new(
        assembled.provider.clone(),
        assembled.options.clone(),
        config,
        prompt,
    )?
    .with_summary_output_tokens(settings.summary_output_tokens)?;
    Ok(Arc::new(policy))
}

/// A session store plus the records to resume from (when resuming).
type OpenedSession = (Arc<dyn CommitSink>, Option<Vec<JournalRecord>>);

/// Run one parsed command line.
pub async fn run(deps: &mut HostDeps, options: Options) -> i32 {
    match options.command.clone() {
        Command::Help => {
            write_stdout(deps, &(cli::usage() + "\n"));
            EXIT_OK
        }
        Command::Version => {
            write_stdout(deps, &(cli::version() + "\n"));
            EXIT_OK
        }
        Command::EnvShow { name } => env_show(deps, &options, &name),
        Command::Run { .. } => {
            if options.resume && options.session.is_none() {
                write_stderr(deps, "error: --resume requires --session\n");
                return EXIT_USAGE;
            }
            match run_agent(deps, &options).await {
                Ok(code) => code,
                Err(message) => {
                    write_stderr(deps, &format!("{message}\n"));
                    EXIT_FAILURE
                }
            }
        }
    }
}

/// `p1 env show NAME`: assemble with the real catalog and print the resolved
/// environment as JSON. Credentials are never read and the network is never used
/// because provider construction is lazy.
fn env_show(deps: &HostDeps, options: &Options, name: &str) -> i32 {
    // Showing an environment starts no worker, but a delegating environment must still
    // assemble: bind the worker tools to a service that can never start one.
    // `env show` only prints the resolved environment; the completion state it
    // issues is dropped with the catalog.
    let completion = Arc::new(CompletionHub::new());
    #[cfg(feature = "delegation")]
    let catalog = {
        let inert: p1_workers::AgentFactory =
            Arc::new(|_| Err("`p1 env show` does not start workers".to_string()));
        let service: Arc<dyn p1_workers::WorkerService> = InProcessWorkers::new(inert, 1);
        crate::catalog::build_catalog_with_workers(
            deps,
            Some(service),
            options.sandbox,
            &options.sandbox_write,
            &options.sandbox_read,
            &options.env_pass,
            &completion,
        )
    };
    #[cfg(not(feature = "delegation"))]
    let catalog = build_catalog(
        deps,
        options.sandbox,
        &options.sandbox_write,
        &options.sandbox_read,
        &options.env_pass,
        &completion,
    );
    // A route file that collides with a whole-provider key fails here, before any
    // environment is loaded or any provider is built.
    let catalog = match catalog {
        Ok(catalog) => catalog,
        Err(message) => {
            write_stderr(deps, &format!("{message}\n"));
            return EXIT_FAILURE;
        }
    };
    let mut environment = match load_environment(name, &deps.environment_dirs) {
        Ok(environment) => environment,
        Err(error) => {
            write_stderr(deps, &format!("{error}\n"));
            return EXIT_FAILURE;
        }
    };
    // Resolve the route binding before assembling: the wire model and the route's
    // own output ceiling come from the route file (spec §2).
    if let Err(message) =
        crate::catalog::resolve_environment(&mut environment, &deps.environment_dirs)
    {
        write_stderr(deps, &format!("{message}\n"));
        return EXIT_FAILURE;
    }
    let workspace = match resolve_workspace(options) {
        Ok(workspace) => workspace,
        Err(message) => {
            write_stderr(deps, &format!("{message}\n"));
            return EXIT_FAILURE;
        }
    };
    let substitutions = substitutions(deps, &workspace);
    match assemble(&catalog, &environment, &workspace, &substitutions) {
        Ok(assembled) => match serde_json::to_string_pretty(&assembled.resolved) {
            Ok(json) => {
                write_stdout(deps, &(json + "\n"));
                EXIT_OK
            }
            Err(error) => {
                write_stderr(
                    deps,
                    &format!("could not render the environment: {error}\n"),
                );
                EXIT_FAILURE
            }
        },
        Err(error) => {
            write_stderr(deps, &format!("{error}\n"));
            EXIT_FAILURE
        }
    }
}

async fn run_agent(deps: &mut HostDeps, options: &Options) -> Result<i32, String> {
    let workspace = resolve_workspace(options)?;
    let headless = options.is_headless();

    let cancel = CancellationToken::new();
    let policy: Arc<HostPolicy> = Arc::new(HostPolicy::new(
        options.ask,
        headless,
        deps.lines.clone(),
        deps.stderr.clone(),
        cancel.clone(),
    ));

    // The delegation service must exist before the catalog so the `worker_*`
    // tools can be registered; the child factory reaches the catalog lazily,
    // breaking the cycle (children never assemble delegation tools).
    let completion_hub = Arc::new(CompletionHub::new());
    #[cfg(feature = "delegation")]
    let catalog_slot: Arc<OnceLock<Arc<Catalog>>> = Arc::new(OnceLock::new());
    #[cfg(feature = "delegation")]
    let child_counter = Arc::new(AtomicUsize::new(0));
    // One owner for the worker usage aggregate: the host creates it and every
    // child renderer feeds it, so the exit line is a plain read at the end.
    #[cfg(feature = "delegation")]
    let worker_usage = Arc::new(crate::render::WorkerUsage::new());
    #[cfg(feature = "delegation")]
    let service: Option<Arc<InProcessWorkers>> = {
        let factory = make_child_factory(
            deps,
            &workspace,
            policy.clone(),
            catalog_slot.clone(),
            child_counter.clone(),
            completion_hub.clone(),
            WorkerJournals {
                session: options.session.clone(),
                usage: worker_usage.clone(),
            },
        );
        let service = InProcessWorkers::new(factory, 2);
        deps.worker_service = Some(service.clone());
        Some(service)
    };

    let catalog = Arc::new(build_catalog(
        deps,
        options.sandbox,
        &options.sandbox_write,
        &options.sandbox_read,
        &options.env_pass,
        &completion_hub,
    )?);
    #[cfg(feature = "delegation")]
    {
        let _ = catalog_slot.set(catalog.clone());
    }

    let mut environment = load_environment(&options.env, &deps.environment_dirs)
        .map_err(|error| error.to_string())?;
    crate::catalog::resolve_environment(&mut environment, &deps.environment_dirs)?;
    let substitutions = substitutions(deps, &workspace);
    let assembled = assemble_with_cache_key(&catalog, &environment, &workspace, &substitutions)?;
    // The `finish` factory issued this agent's completion state during `assemble`.
    // `None` when the environment does not assemble `finish`.
    let completion = completion_hub.take();
    let context = agent_context(&assembled)?;
    let route = assembled.resolved.route.origin.route.clone();
    let model = assembled.resolved.route.origin.model.clone();

    let (journal, records): OpenedSession = open_session(deps, options)?;
    // On resume the journal holds the earlier turns; rebuild this agent's activity
    // from them so a verification run before the restart still counts and a file
    // change before it still invalidates (completion.md §3).
    if let (Some(completion), Some(records)) = (&completion, &records) {
        completion.log.replay(&assembled.tools, records);
    }

    let renderer = Arc::new(Renderer::new(
        deps.stdout.clone(),
        deps.stderr.clone(),
        deps.stdout_is_tty,
        route,
        model,
        Arc::new(Mutex::new(String::new())),
    ));
    // The activity tee forwards every event to the renderer unchanged and feeds
    // this agent's log the effects and exit codes a later `finish` reads. It is
    // installed even without `finish`: the headless stall guard (§3c) reads the
    // same log for workspace mutations. Without a `finish` tool the hub issued no
    // log, so the host makes one.
    let log = match &completion {
        Some(completion) => completion.log.clone(),
        None => Arc::new(ActivityLog::default()),
    };
    let events: Arc<dyn EventSink> = Arc::new(ActivityTee::new(
        renderer.clone(),
        log.clone(),
        &assembled.tools,
    ));
    // The guard is headless-only (completion.md §3c); an interactive user sees the
    // summaries and decides.
    let mut stall: Option<Arc<StallGuard>> = None;
    let events: Arc<dyn EventSink> = if headless {
        let guard = Arc::new(StallGuard::new(
            log,
            options.max_idle_summaries,
            cancel.clone(),
        ));
        stall = Some(guard.clone());
        Arc::new(StallWatcher {
            inner: events,
            guard,
        })
    } else {
        events
    };

    let parts = AgentParts {
        provider: assembled.provider,
        tools: assembled.tools,
        system_prompt: assembled.system_prompt,
        options: assembled.options,
        context,
        authorization: policy,
        journal,
        events,
    };

    let (mut agent, report): (Agent, Option<ResumeReport>) = match records {
        Some(records) => {
            let (agent, report) = Agent::resume(parts, &records).map_err(|e| e.to_string())?;
            #[cfg(feature = "delegation")]
            if let Some(service) = &service {
                announce_lost_workers(deps, &agent, service, &child_counter, &records);
            }
            (agent, Some(report))
        }
        None => (Agent::new(parts).map_err(|e| e.to_string())?, None),
    };
    if let Some(report) = &report {
        print_resume_report(deps, report);
    }

    #[cfg(feature = "delegation")]
    if let Some(service) = &service {
        service.set_parent_inbox(agent.inbox());
    }

    let code = if headless {
        let stall = stall.expect("a headless run always installs the stall guard");
        run_headless(
            deps, &mut agent, options, &cancel, completion, &stall, &renderer,
        )
        .await
    } else {
        run_interactive(deps, &mut agent, &cancel).await
    };

    #[cfg(feature = "delegation")]
    if let Some(service) = &service {
        service.shutdown().await;
    }

    renderer.finish();
    // After the parent's own total, and only when a worker actually ran.
    #[cfg(feature = "delegation")]
    if let Some(line) = worker_usage.line() {
        write_stderr(deps, &format!("{line}\n"));
    }
    Ok(code)
}

fn open_session(deps: &HostDeps, options: &Options) -> Result<OpenedSession, String> {
    match &options.session {
        None => Ok((session::memory(), None)),
        Some(path) => {
            if options.resume {
                let (store, resumed) = session::resume(path).map_err(|e| e.to_string())?;
                if let Some(tail) = &resumed.repaired_tail {
                    write_stderr(
                        deps,
                        &format!(
                            "session file had an incomplete last record ({} bytes); it was cut off\n",
                            tail.bytes
                        ),
                    );
                }
                Ok((session::sink(&store), Some(resumed.records)))
            } else {
                let store = session::create(path).map_err(|e| e.to_string())?;
                Ok((session::sink(&store), None))
            }
        }
    }
}

async fn run_headless(
    deps: &HostDeps,
    agent: &mut Agent,
    options: &Options,
    cancel: &CancellationToken,
    completion: Option<Completion>,
    stall: &Arc<StallGuard>,
    renderer: &Renderer,
) -> i32 {
    let second = Arc::new(tokio::sync::Notify::new());
    spawn_interrupt(deps.interrupt.clone(), cancel.clone(), second.clone());

    let prompt = match &options.command {
        Command::Run {
            prompt: Some(prompt),
        } => prompt.clone(),
        _ => String::new(),
    };

    // No `finish` in the assembled environment: the run behaves exactly as before
    // except that the §3c stall guard is still headless policy.
    let Some(completion) = completion else {
        return run_headless_plain(deps, agent, cancel, &second, stall, renderer, options).await;
    };

    let log = completion.log.clone();
    let outcome = completion.outcome.clone();
    let max_continuations = options.max_continuations;

    outcome.clear();
    let mut end = match prompt_turn(deps, agent, renderer, cancel, &second, options, prompt).await {
        TurnOutcome::End(end) => end,
        TurnOutcome::Cancelled => return stalled_exit(deps, stall).unwrap_or(EXIT_CANCELLED),
    };

    let mut continuations = 0usize;
    let mut last_marker: Option<u64> = None;
    let mut stops = 0usize;

    loop {
        if cancel.is_cancelled() {
            return stalled_exit(deps, stall).unwrap_or(EXIT_CANCELLED);
        }
        // A turn that did not complete (cancelled, provider failure, …) is
        // handled exactly as before, except that a stall-guard cancel is a stall.
        if !matches!(end, TurnEnd::Completed { .. }) {
            if let Some(code) = stalled_exit(deps, stall) {
                return code;
            }
            return end_code(&end);
        }
        match outcome.get() {
            Some(Accepted::Done { .. }) => return EXIT_OK,
            Some(Accepted::Blocked { needs, tried, .. }) => {
                write_stderr(deps, &format!("blocked: {needs}\n"));
                if !tried.is_empty() {
                    write_stderr(deps, &format!("tried: {}\n", tried.join("; ")));
                }
                return EXIT_BLOCKED;
            }
            None => {}
        }
        // FIRST the things a turn end legitimately waits for: a pending inbox or a
        // running worker. A parent that stopped while its worker runs is WAITING,
        // not stopping (completion.md §3, must-pass a0).
        outcome.clear();
        match wait_for_work(deps, agent, cancel, &second, renderer, options).await {
            WaitOutcome::Turn(next) => {
                end = next;
                continue;
            }
            WaitOutcome::Cancelled => return stalled_exit(deps, stall).unwrap_or(EXIT_CANCELLED),
            WaitOutcome::Idle => {}
        }
        // Premature stop. It is allowed while BOTH bounds hold: the whole-run
        // count and at least one non-finish call finished since the last
        // continuation.
        stops += 1;
        let progress = log.non_finish_finishes();
        let allowed =
            continuations < max_continuations && last_marker.is_none_or(|marker| progress > marker);
        if !allowed {
            write_stderr(
                deps,
                &format!("stalled: the agent stopped {stops} times without finishing\n"),
            );
            return EXIT_STALLED;
        }
        continuations += 1;
        last_marker = Some(progress);
        outcome.clear();
        end = match prompt_turn(
            deps,
            agent,
            renderer,
            cancel,
            &second,
            options,
            CONTINUATION_MESSAGE.to_string(),
        )
        .await
        {
            TurnOutcome::End(end) => end,
            TurnOutcome::Cancelled => return stalled_exit(deps, stall).unwrap_or(EXIT_CANCELLED),
        };
    }
}

/// The pre-completion headless driver: one turn, then inbox turns and a wait for
/// running children. Used verbatim when the environment does not assemble `finish`.
async fn run_headless_plain(
    deps: &HostDeps,
    agent: &mut Agent,
    cancel: &CancellationToken,
    second: &Arc<tokio::sync::Notify>,
    stall: &Arc<StallGuard>,
    renderer: &Renderer,
    options: &Options,
) -> i32 {
    let prompt = match &options.command {
        Command::Run {
            prompt: Some(prompt),
        } => prompt.clone(),
        _ => String::new(),
    };
    let mut code = match prompt_turn(deps, agent, renderer, cancel, second, options, prompt).await {
        TurnOutcome::End(end) => end_code(&end),
        TurnOutcome::Cancelled => return stalled_exit(deps, stall).unwrap_or(EXIT_CANCELLED),
    };

    loop {
        if cancel.is_cancelled() {
            return stalled_exit(deps, stall).unwrap_or(EXIT_CANCELLED);
        }
        match wait_for_work(deps, agent, cancel, second, renderer, options).await {
            WaitOutcome::Turn(end) => {
                code = end_code(&end);
                if code == EXIT_CANCELLED {
                    return stalled_exit(deps, stall).unwrap_or(EXIT_CANCELLED);
                }
            }
            WaitOutcome::Idle => break,
            WaitOutcome::Cancelled => return stalled_exit(deps, stall).unwrap_or(EXIT_CANCELLED),
        }
    }
    code
}

/// What the host found when a turn ended and it looked for legitimate work.
enum WaitOutcome {
    /// An inbox turn ran and ended with this end; judge it from the top.
    Turn(TurnEnd),
    /// The inbox is empty and no worker is running: there is nothing to wait for.
    Idle,
    /// The run was cancelled while waiting.
    Cancelled,
}

/// The ONE place a turn end waits instead of stopping: a pending inbox message,
/// or a running worker whose completion notification will arrive on the inbox.
/// Shared by the plain and the `finish`-aware headless drivers so waiting has one
/// meaning in both. `_deps` is only read under the delegation feature.
async fn wait_for_work(
    _deps: &HostDeps,
    agent: &mut Agent,
    cancel: &CancellationToken,
    second: &Arc<tokio::sync::Notify>,
    renderer: &Renderer,
    options: &Options,
) -> WaitOutcome {
    if agent.has_pending_inbox() {
        return inbox_turn(_deps, agent, cancel, second, renderer, options).await;
    }
    #[cfg(feature = "delegation")]
    {
        if running_children(_deps).await > 0 {
            tokio::select! {
                biased;
                _ = second.notified() => return WaitOutcome::Cancelled,
                _ = cancel.cancelled() => return WaitOutcome::Cancelled,
                _ = agent.inbox_ready() => {}
            }
            // `inbox_ready` only resolves with a message pending, so this turn is
            // the worker's completion notification.
            return inbox_turn(_deps, agent, cancel, second, renderer, options).await;
        }
    }
    WaitOutcome::Idle
}

/// One inbox turn, with the same transient-provider policy as any other turn
/// (completion.md §3b).
async fn inbox_turn(
    deps: &HostDeps,
    agent: &mut Agent,
    cancel: &CancellationToken,
    second: &Arc<tokio::sync::Notify>,
    renderer: &Renderer,
    options: &Options,
) -> WaitOutcome {
    match race_inbox(agent.run_inbox_turn(cancel.clone()), second).await {
        Some(Some(end)) => {
            match wait_out_transient(deps, agent, renderer, cancel, second, options, end).await {
                TurnOutcome::End(end) => WaitOutcome::Turn(end),
                TurnOutcome::Cancelled => WaitOutcome::Cancelled,
            }
        }
        Some(None) => WaitOutcome::Idle,
        None => WaitOutcome::Cancelled,
    }
}

async fn run_interactive(deps: &HostDeps, agent: &mut Agent, cancel: &CancellationToken) -> i32 {
    let second = Arc::new(tokio::sync::Notify::new());
    spawn_interrupt(deps.interrupt.clone(), cancel.clone(), second.clone());

    loop {
        write_stderr(deps, "p1> ");
        // Waiting for the user is also waiting for the inbox: a worker finishing
        // while the prompt is idle must reach the agent now, not at the next
        // keystroke. The pending line read is dropped for the inbox turn (the
        // line source is cancel-safe) so that turn's authorization questions can
        // have the terminal; then the prompt is shown again.
        let line = loop {
            let woken = tokio::select! {
                biased;
                _ = second.notified() => return EXIT_CANCELLED,
                _ = cancel.cancelled() => return EXIT_CANCELLED,
                line = deps.lines.next_line() => Some(line),
                _ = agent.inbox_ready() => None,
            };
            match woken {
                Some(line) => break line,
                None => {
                    write_stderr(deps, "\n");
                    if !drain_inbox(agent, cancel, &second).await {
                        return EXIT_CANCELLED;
                    }
                    write_stderr(deps, "p1> ");
                }
            }
        };
        let Some(line) = line else {
            break;
        };
        let text = line.trim();
        if text.is_empty() {
            continue;
        }
        if text == "/exit" {
            break;
        }
        let end = match race_turn(agent.run_turn(text.to_string(), cancel.clone()), &second).await {
            Some(end) => end,
            None => return EXIT_CANCELLED,
        };
        if matches!(end, TurnEnd::Cancelled) {
            return EXIT_CANCELLED;
        }
        if !drain_inbox(agent, cancel, &second).await {
            return EXIT_CANCELLED;
        }
        #[cfg(feature = "delegation")]
        {
            let running = running_children(deps).await;
            if running > 0 {
                write_stderr(deps, &format!("({running} worker(s) still running)\n"));
            }
        }
    }
    EXIT_OK
}

/// Run inbox turns until the inbox is empty, without blocking on running
/// children. `false` means the run was cancelled.
async fn drain_inbox(
    agent: &mut Agent,
    cancel: &CancellationToken,
    second: &Arc<tokio::sync::Notify>,
) -> bool {
    while agent.has_pending_inbox() {
        if cancel.is_cancelled() {
            return false;
        }
        match race_inbox(agent.run_inbox_turn(cancel.clone()), second).await {
            Some(Some(TurnEnd::Cancelled)) | None => return false,
            Some(_) => {}
        }
    }
    !cancel.is_cancelled()
}

/// Spawn the Ctrl-C pump. The first interrupt cancels the run; the second
/// notifies `second` so the driver can return 130 without waiting for the turn.
fn spawn_interrupt(
    interrupt: Arc<dyn InterruptSource>,
    cancel: CancellationToken,
    second: Arc<tokio::sync::Notify>,
) {
    tokio::spawn(async move {
        loop {
            interrupt.recv().await;
            if cancel.is_cancelled() {
                second.notify_one();
                return;
            }
            cancel.cancel();
        }
    });
}

async fn race_turn<F>(future: F, second: &tokio::sync::Notify) -> Option<TurnEnd>
where
    F: std::future::Future<Output = TurnEnd>,
{
    tokio::select! {
        biased;
        _ = second.notified() => None,
        end = future => Some(end),
    }
}

async fn race_inbox<F>(future: F, second: &tokio::sync::Notify) -> Option<Option<TurnEnd>>
where
    F: std::future::Future<Output = Option<TurnEnd>>,
{
    tokio::select! {
        biased;
        _ = second.notified() => None,
        end = future => Some(end),
    }
}

// ------------------------------------------------------ stall guard (completion.md §3c)

/// The headless §3c guard: count consecutive context replacements and cancel the
/// turn when they reach `--max-idle-summaries`. Progress — a workspace mutation or
/// a `finish` call of any status — resets the count through [`ActivityLog`].
struct StallGuard {
    log: Arc<ActivityLog>,
    max: usize,
    cancel: CancellationToken,
    stalled: AtomicBool,
}

impl StallGuard {
    fn new(log: Arc<ActivityLog>, max: usize, cancel: CancellationToken) -> Self {
        Self {
            log,
            max,
            cancel,
            stalled: AtomicBool::new(false),
        }
    }

    /// One committed `ContextReplaced` was observed. On reaching the bound the
    /// guard latches and cancels the turn; the driver then reports the stall.
    fn on_context_replaced(&self) {
        self.log.record_replacement();
        if self.max > 0 && self.log.consecutive_replacements() >= self.max as u64 {
            self.stalled.store(true, Ordering::SeqCst);
            self.cancel.cancel();
        }
    }

    fn stalled(&self) -> bool {
        self.stalled.load(Ordering::SeqCst)
    }
}

/// Wraps the event sink for a headless run so the guard sees every committed
/// `ContextReplaced`; it forwards the event unchanged.
struct StallWatcher {
    inner: Arc<dyn EventSink>,
    guard: Arc<StallGuard>,
}

impl EventSink for StallWatcher {
    fn emit(&self, event: AgentEvent) {
        if matches!(event, AgentEvent::ContextReplaced { .. }) {
            self.guard.on_context_replaced();
        }
        self.inner.emit(event);
    }
}

/// §3c: a cancellation caused by the stall guard is reported as a stall, not as an
/// ordinary Ctrl-C.
fn stalled_exit(deps: &HostDeps, stall: &StallGuard) -> Option<i32> {
    if stall.stalled() {
        write_stderr(deps, &format!("{}\n", stall_message(stall.max)));
        Some(EXIT_STALLED)
    } else {
        None
    }
}

// ------------------------------------------------------ transient provider ends

/// The fixed waits for a dropped connection, in order (completion.md §3b).
const TRANSPORT_RETRY_WAITS: [Duration; 3] = [
    Duration::from_secs(5),
    Duration::from_secs(30),
    Duration::from_secs(120),
];

/// The fixed waits for a rate limit, in order (completion.md §3b).
const RATE_LIMITED_RETRY_WAITS: [Duration; 3] = [
    Duration::from_secs(60),
    Duration::from_secs(300),
    Duration::from_secs(900),
];

/// The transient kind of a turn end, if it is one (completion.md §3b, amended by
/// §3c): a dropped transport, a rate limit, or a malformed response are worth
/// waiting out — the model's or the route's one-off. Everything else
/// (`InvalidRequest`, `Authentication`, `ContextWindowExceeded`) ends the run as it
/// always did.
fn transient_kind(end: &TurnEnd) -> Option<ProviderErrorKind> {
    match end {
        TurnEnd::ProviderFailed { error } => match error.kind {
            kind @ (ProviderErrorKind::Transport
            | ProviderErrorKind::RateLimited
            | ProviderErrorKind::Protocol) => Some(kind),
            _ => None,
        },
        _ => None,
    }
}

/// The fixed wait schedule of a transient kind (completion.md §3b): a dropped
/// connection is retried quickly, a quota window slowly, and a malformed response
/// on the transport schedule (§3c). The schedules are not computed.
fn retry_schedule(kind: ProviderErrorKind) -> &'static [Duration] {
    match kind {
        ProviderErrorKind::Transport | ProviderErrorKind::Protocol => &TRANSPORT_RETRY_WAITS,
        ProviderErrorKind::RateLimited => &RATE_LIMITED_RETRY_WAITS,
        _ => &[],
    }
}

/// The wait before retry `retry` (1-based) of a transient failure. The last entry
/// repeats when `--provider-retries` outlasts the schedule.
fn retry_wait(kind: ProviderErrorKind, retry: usize) -> Duration {
    let schedule = retry_schedule(kind);
    schedule
        .get(retry - 1)
        .or_else(|| schedule.last())
        .copied()
        .unwrap_or(Duration::ZERO)
}

/// What a turn — and the retries it needed — ended with.
enum TurnOutcome {
    /// The turn ended for a reason other than a transient provider failure.
    End(TurnEnd),
    /// The run was cancelled while a turn ran or while waiting to retry.
    Cancelled,
}

/// Run one prompt turn and wait out its transient provider failures.
async fn prompt_turn(
    deps: &HostDeps,
    agent: &mut Agent,
    renderer: &Renderer,
    cancel: &CancellationToken,
    second: &Arc<tokio::sync::Notify>,
    options: &Options,
    prompt: String,
) -> TurnOutcome {
    let end = match race_turn(agent.run_turn(prompt, cancel.clone()), second).await {
        Some(end) => end,
        None => return TurnOutcome::Cancelled,
    };
    wait_out_transient(deps, agent, renderer, cancel, second, options, end).await
}

/// completion.md §3b: a turn that ended `ProviderFailed` transiently is not the
/// end of the run. The host WAITS on the fixed schedule, then continues with ONE
/// [`PROVIDER_RETRY_MESSAGE`] — while `--provider-retries` CONSECUTIVE transient
/// ends allow it. The count resets in a turn where at least one provider response
/// completed. Cancel wins during a wait, and the wait itself is injected through
/// [`HostDeps::wait`], so no test sleeps.
async fn wait_out_transient(
    deps: &HostDeps,
    agent: &mut Agent,
    renderer: &Renderer,
    cancel: &CancellationToken,
    second: &Arc<tokio::sync::Notify>,
    options: &Options,
    end: TurnEnd,
) -> TurnOutcome {
    let mut end = end;
    let mut consecutive = 0usize;
    loop {
        let Some(kind) = transient_kind(&end) else {
            return TurnOutcome::End(end);
        };
        if consecutive >= options.provider_retries {
            // Exhausted: the run ends as it does today, with the last error
            // already printed by the renderer.
            return TurnOutcome::End(end);
        }
        consecutive += 1;
        let wait = retry_wait(kind, consecutive);
        renderer.provider_retry(kind, consecutive, options.provider_retries, wait);
        if wait_for_retry(deps, cancel, second, wait).await == WaitEnd::Cancelled {
            return TurnOutcome::Cancelled;
        }
        let completed_before = renderer.responses_completed();
        end = match race_turn(
            agent.run_turn(PROVIDER_RETRY_MESSAGE.to_string(), cancel.clone()),
            second,
        )
        .await
        {
            Some(end) => end,
            None => return TurnOutcome::Cancelled,
        };
        if renderer.responses_completed() > completed_before {
            consecutive = 0;
        }
    }
}

#[derive(PartialEq, Eq)]
enum WaitEnd {
    Ready,
    Cancelled,
}

/// Wait out one retry delay, raced against cancellation: cancel wins immediately
/// during a wait (completion.md §3b).
async fn wait_for_retry(
    deps: &HostDeps,
    cancel: &CancellationToken,
    second: &Arc<tokio::sync::Notify>,
    wait: Duration,
) -> WaitEnd {
    let sleep = (deps.wait)(wait);
    tokio::select! {
        biased;
        _ = second.notified() => WaitEnd::Cancelled,
        _ = cancel.cancelled() => WaitEnd::Cancelled,
        _ = sleep => WaitEnd::Ready,
    }
}

fn end_code(end: &TurnEnd) -> i32 {
    match end {
        TurnEnd::Completed { .. } => EXIT_OK,
        TurnEnd::Cancelled => EXIT_CANCELLED,
        TurnEnd::ProviderFailed { .. }
        | TurnEnd::CommitFailed { .. }
        | TurnEnd::ContextFailed { .. } => EXIT_FAILURE,
    }
}

/// Workers live in the process that started them: their sessions are in memory and
/// are NOT restored with the parent's (ADR-0034). A resumed history that mentions
/// workers is therefore talking about agents that no longer exist. Say so — to the
/// user and, through the inbox, to the model — and keep their ids from being reused.
#[cfg(feature = "delegation")]
fn announce_lost_workers(
    deps: &HostDeps,
    agent: &Agent,
    service: &InProcessWorkers,
    child_counter: &AtomicUsize,
    records: &[p1_contracts::JournalRecord],
) {
    let earlier = p1_tool_delegate::workers_started_in(records);
    if earlier.is_empty() {
        return;
    }
    let used = earlier
        .iter()
        .filter_map(|id| id.strip_prefix('w')?.parse::<usize>().ok())
        .max()
        .unwrap_or(earlier.len());
    service.reserve_ids(used);
    child_counter.store(used, Ordering::SeqCst);
    let names = earlier.join(", ");
    write_stderr(
        deps,
        &format!(
            "resume: worker(s) {names} belonged to the earlier process and are not restored\n"
        ),
    );
    agent.inbox().send(
        p1_contracts::InboxKind::Notification,
        format!(
            "This session was resumed in a new process. Workers started before the resume \
             ({names}) no longer exist: they cannot be continued or asked for results, and \
             work they had not finished was not saved. Check the files for what they left \
             behind before relying on it, and start a new worker if the work is still needed."
        ),
    );
}

fn print_resume_report(deps: &HostDeps, report: &ResumeReport) {
    let mut lines: Vec<String> = Vec::new();
    if !report.unresolved_calls.is_empty() {
        lines.push(format!(
            "resume: {} unresolved tool call(s) reconciled",
            report.unresolved_calls.len()
        ));
    }
    for name in &report.changed_tools {
        lines.push(format!(
            "resume: tool `{name}` changed implementation; nothing old is dispatched"
        ));
    }
    for name in &report.missing_tools {
        lines.push(format!("resume: tool `{name}` is no longer available"));
    }
    if report.environment_changed {
        lines.push("resume: environment changed; it is re-committed at the next turn".to_string());
    }
    for line in lines {
        write_stderr(deps, &format!("{line}\n"));
    }
}

fn resolve_workspace(options: &Options) -> Result<PathBuf, String> {
    match &options.workspace {
        Some(path) => Ok(path.clone()),
        None => std::env::current_dir()
            .map_err(|error| format!("cannot determine the current directory: {error}")),
    }
}

fn substitutions(deps: &HostDeps, workspace: &Path) -> Substitutions {
    Substitutions {
        workspace: workspace.display().to_string(),
        date: deps.date.clone(),
        os: std::env::consts::OS.to_string(),
    }
}

fn write_stdout(deps: &HostDeps, text: &str) {
    let mut writer = deps.stdout.lock().unwrap();
    let _ = writer.write_all(text.as_bytes());
    let _ = writer.flush();
}

fn write_stderr(deps: &HostDeps, text: &str) {
    let mut writer = deps.stderr.lock().unwrap();
    let _ = writer.write_all(text.as_bytes());
    let _ = writer.flush();
}

#[cfg(feature = "delegation")]
async fn running_children(deps: &HostDeps) -> usize {
    match &deps.worker_service {
        Some(service) => service
            .list()
            .await
            .iter()
            .filter(|(_, status)| matches!(status, ChildStatus::Running))
            .count(),
        None => 0,
    }
}

/// What a child factory needs beyond the catalog: where worker journals go and
/// the run's shared usage aggregate. (Kept as one argument so the factory's
/// signature stays legible.)
#[cfg(feature = "delegation")]
struct WorkerJournals {
    /// The parent's `--session FILE`; worker `w<N>` writes `FILE.w<N>.jsonl`.
    /// `None` keeps a child's journal in memory.
    session: Option<PathBuf>,
    usage: Arc<crate::render::WorkerUsage>,
}

/// Build the child `Agent` through the SAME load + assemble path the top-level
/// agent uses. The child gets its own fresh `ToolServices` (inside `assemble`),
/// the parent's workspace unless the spec overrides it, the parent's
/// authorization policy, its own session journal, and a prefixed renderer.
#[cfg(feature = "delegation")]
fn make_child_factory(
    deps: &HostDeps,
    parent_workspace: &Path,
    policy: Arc<HostPolicy>,
    catalog_slot: Arc<OnceLock<Arc<Catalog>>>,
    counter: Arc<AtomicUsize>,
    completion_hub: Arc<CompletionHub>,
    journals: WorkerJournals,
) -> AgentFactory {
    let environment_dirs = deps.environment_dirs.clone();
    let date = deps.date.clone();
    let stdout: SharedWriter = deps.stdout.clone();
    let stderr: SharedWriter = deps.stderr.clone();
    let tty = deps.stdout_is_tty;
    let parent_workspace = parent_workspace.to_path_buf();

    Arc::new(move |spec: &ChildSpec| -> Result<ChildAgent, String> {
        let mut environment = load_environment(&spec.environment, &environment_dirs)
            .map_err(|error| error.to_string())?;
        if environment
            .tools
            .iter()
            .any(|tool| tool.module.starts_with("worker_"))
        {
            return Err("delegation inside a worker is not supported".to_string());
        }
        let catalog = catalog_slot
            .get()
            .ok_or_else(|| "the host catalog is not ready".to_string())?
            .clone();
        let workspace = spec
            .workspace
            .clone()
            .unwrap_or_else(|| parent_workspace.clone());
        let substitutions = Substitutions {
            workspace: workspace.display().to_string(),
            date: date.clone(),
            os: std::env::consts::OS.to_string(),
        };
        crate::catalog::resolve_environment(&mut environment, &environment_dirs)?;
        let assembled =
            assemble_with_cache_key(&catalog, &environment, &workspace, &substitutions)?;
        // `InProcessWorkers` assigns `w{n}` after a SUCCESSFUL factory call and
        // factory calls are serialised, so this is the id the service will hand
        // out. The counter is only advanced at the very end: a start that fails
        // (bad environment, an existing worker session file, a failed build) must
        // not desynchronise it from the service's own numbering.
        let id = counter.load(Ordering::SeqCst) + 1;
        // The child gets its OWN activity log and outcome, issued by the shared
        // catalog for this assembly. The worker service does not read the
        // outcome: a child's turn end is its completion, the parent verifies.
        let child_completion = completion_hub.take();
        let context = agent_context(&assembled)?;
        let route = assembled.resolved.route.origin.route.clone();
        let model = assembled.resolved.route.origin.model.clone();
        let description = format!("{route}/{model}");

        let label = Arc::new(Mutex::new(String::new()));
        let renderer: Arc<dyn EventSink> = Arc::new(
            Renderer::new(
                stdout.clone(),
                stderr.clone(),
                tty,
                route,
                model,
                label.clone(),
            )
            .with_worker_usage(journals.usage.clone()),
        );
        let events: Arc<dyn EventSink> = match &child_completion {
            Some(completion) => Arc::new(ActivityTee::new(
                renderer.clone(),
                completion.log.clone(),
                &assembled.tools,
            )),
            None => renderer,
        };
        // With `--session`, worker `w{n}` gets its OWN new JSONL file next to the
        // parent's (`FILE.w{n}.jsonl`). Without one it stays in memory like before.
        // Created last among the fallible steps so a later failure cannot leave a
        // stray file behind — and if `Agent::new` still fails, remove what we made.
        let created_file = journals
            .session
            .as_ref()
            .map(|session| crate::session::worker_path(session, id));
        let journal: Arc<dyn CommitSink> = match &journals.session {
            Some(session) => crate::session::worker(session, id).map_err(|error| {
                format!(
                    "cannot create worker session file {}: {error}",
                    crate::session::worker_path(session, id).display()
                )
            })?,
            None => Arc::new(MemoryJournal::new()),
        };
        let parts = AgentParts {
            provider: assembled.provider,
            tools: assembled.tools,
            system_prompt: assembled.system_prompt,
            options: assembled.options,
            context,
            authorization: policy.clone(),
            journal,
            events,
        };
        let agent = match Agent::new(parts) {
            Ok(agent) => agent,
            Err(error) => {
                if let Some(path) = &created_file {
                    let _ = std::fs::remove_file(path);
                }
                return Err(error.to_string());
            }
        };
        counter.fetch_add(1, Ordering::SeqCst);
        *label.lock().unwrap() = format!("[w{id}] ");
        journals.usage.worker_started();
        Ok(ChildAgent { agent, description })
    })
}

/// Assemble one agent under the host's explicit cache-key policy (ADR-0039): the
/// RESOLVED route decides. A key is generated only when the environment sets
/// none and the resolved provider reports [`CacheKeySupport::Optional`]; a key
/// the environment file sets EXPLICITLY is passed through untouched, and a route
/// that cannot carry it fails assembly, as any explicit option it cannot carry
/// does. Exactly ONE assembly runs — the provider and the tools are each built
/// once — and an assembly error is reported as it is: there is no second attempt
/// to drop a key, because the description already said whether one is taken.
fn assemble_with_cache_key(
    catalog: &Catalog,
    environment: &p1_assembly::EnvironmentFile,
    workspace: &std::path::Path,
    substitutions: &Substitutions,
) -> Result<p1_assembly::Assembled, String> {
    let name = environment.name.clone();
    let configured = environment.options.clone();
    p1_assembly::assemble_with_route_options(
        catalog,
        environment,
        workspace,
        substitutions,
        |route| {
            let mut options = configured.clone();
            if options.cache_key.is_none() && route.cache_key == CacheKeySupport::Optional {
                options.cache_key = Some(generated_cache_key(&name, workspace));
            }
            options
        },
    )
    .map_err(|error| error.to_string())
}

/// A fresh provider-side prompt-cache key for one agent. Without one the Codex
/// route served 0 cached tokens across a whole task (measured 2026-09-20);
/// routes without such a key never see it.
fn generated_cache_key(environment: &str, workspace: &std::path::Path) -> String {
    use std::hash::{Hash, Hasher};
    static AGENTS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    workspace.hash(&mut hasher);
    environment.hash(&mut hasher);
    std::process::id().hash(&mut hasher);
    AGENTS
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        .hash(&mut hasher);
    if let Ok(now) = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH) {
        now.as_nanos().hash(&mut hasher);
    }
    format!("p1-{:016x}", hasher.finish())
}
