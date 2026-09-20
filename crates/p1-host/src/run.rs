//! The headless and interactive drivers, plus the delegation wiring.
//!
//! Headless runs one turn and then drains inbox turns, waiting for running
//! children so a parent that started a worker is woken by its completion.
//! Interactive prompts on stderr and drains inbox turns without waiting on
//! children. First Ctrl-C cancels the run (and, at exit, the children); second
//! Ctrl-C returns 130 immediately.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

#[cfg(feature = "delegation")]
use std::sync::OnceLock;
#[cfg(feature = "delegation")]
use std::sync::atomic::{AtomicUsize, Ordering};

#[cfg(feature = "delegation")]
use p1_assembly::Catalog;
use p1_assembly::{Substitutions, assemble, load_environment};
use p1_contracts::{
    BoxFuture, CancellationToken, CommitSink, ContextError, ContextInput, ContextPolicy, EventSink,
    JournalRecord, Prepared, TurnEnd,
};
use p1_core::{Agent, AgentParts, ResumeReport};
#[cfg(feature = "delegation")]
use p1_journal::MemoryJournal;

#[cfg(feature = "delegation")]
use crate::SharedWriter;
use crate::catalog::build_catalog;
use crate::cli::{self, Command, Options};
use crate::policy::HostPolicy;
use crate::render::Renderer;
use crate::session;
use crate::{HostDeps, InterruptSource};

#[cfg(feature = "delegation")]
use p1_workers::{AgentFactory, ChildAgent, ChildSpec, ChildStatus, InProcessWorkers};

/// Exit codes (the process contract).
pub const EXIT_OK: i32 = 0;
pub const EXIT_FAILURE: i32 = 1;
pub const EXIT_USAGE: i32 = 2;
pub const EXIT_CANCELLED: i32 = 130;

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
    #[cfg(feature = "delegation")]
    let catalog = {
        let inert: p1_workers::AgentFactory =
            Arc::new(|_| Err("`p1 env show` does not start workers".to_string()));
        let service: Arc<dyn p1_workers::WorkerService> = InProcessWorkers::new(inert, 1);
        crate::catalog::build_catalog_with_workers(deps, Some(service))
    };
    #[cfg(not(feature = "delegation"))]
    let catalog = build_catalog(deps);
    let environment = match load_environment(name, &deps.environment_dirs) {
        Ok(environment) => environment,
        Err(error) => {
            write_stderr(deps, &format!("{error}\n"));
            return EXIT_FAILURE;
        }
    };
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
        options.yes,
        headless,
        deps.lines.clone(),
        deps.stderr.clone(),
        cancel.clone(),
    ));

    // The delegation service must exist before the catalog so the `worker_*`
    // tools can be registered; the child factory reaches the catalog lazily,
    // breaking the cycle (children never assemble delegation tools).
    #[cfg(feature = "delegation")]
    let catalog_slot: Arc<OnceLock<Arc<Catalog>>> = Arc::new(OnceLock::new());
    #[cfg(feature = "delegation")]
    let child_counter = Arc::new(AtomicUsize::new(0));
    #[cfg(feature = "delegation")]
    let service: Option<Arc<InProcessWorkers>> = {
        let factory = make_child_factory(
            deps,
            &workspace,
            policy.clone(),
            catalog_slot.clone(),
            child_counter.clone(),
        );
        let service = InProcessWorkers::new(factory, 2);
        deps.worker_service = Some(service.clone());
        Some(service)
    };

    let catalog = Arc::new(build_catalog(deps));
    #[cfg(feature = "delegation")]
    {
        let _ = catalog_slot.set(catalog.clone());
    }

    let mut environment = load_environment(&options.env, &deps.environment_dirs)
        .map_err(|error| error.to_string())?;
    ensure_cache_key(&mut environment, &workspace);
    let substitutions = substitutions(deps, &workspace);
    let assembled =
        assemble(&catalog, &environment, &workspace, &substitutions).map_err(|e| e.to_string())?;
    let route = assembled.resolved.route.origin.route.clone();
    let model = assembled.resolved.route.origin.model.clone();

    let (journal, records): OpenedSession = open_session(deps, options)?;

    let renderer = Arc::new(Renderer::new(
        deps.stdout.clone(),
        deps.stderr.clone(),
        deps.stdout_is_tty,
        route,
        model,
        Arc::new(Mutex::new(String::new())),
    ));
    let events: Arc<dyn EventSink> = renderer.clone();

    let parts = AgentParts {
        provider: assembled.provider,
        tools: assembled.tools,
        system_prompt: assembled.system_prompt,
        options: assembled.options,
        context: Arc::new(DefaultContext),
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
        run_headless(deps, &mut agent, options, &cancel).await
    } else {
        run_interactive(deps, &mut agent, &cancel).await
    };

    #[cfg(feature = "delegation")]
    if let Some(service) = &service {
        service.shutdown().await;
    }

    renderer.finish();
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
) -> i32 {
    let second = Arc::new(tokio::sync::Notify::new());
    spawn_interrupt(deps.interrupt.clone(), cancel.clone(), second.clone());

    let prompt = match &options.command {
        Command::Run {
            prompt: Some(prompt),
        } => prompt.clone(),
        _ => String::new(),
    };

    let mut code = match race_turn(agent.run_turn(prompt, cancel.clone()), &second).await {
        Some(end) => end_code(&end),
        None => return EXIT_CANCELLED,
    };

    loop {
        if cancel.is_cancelled() {
            return EXIT_CANCELLED;
        }
        if agent.has_pending_inbox() {
            match race_inbox(agent.run_inbox_turn(cancel.clone()), &second).await {
                Some(Some(end)) => {
                    code = end_code(&end);
                    if code == EXIT_CANCELLED {
                        return code;
                    }
                }
                Some(None) => {}
                None => return EXIT_CANCELLED,
            }
            continue;
        }
        #[cfg(feature = "delegation")]
        {
            if running_children(deps).await > 0 {
                tokio::select! {
                    biased;
                    _ = second.notified() => return EXIT_CANCELLED,
                    _ = cancel.cancelled() => return EXIT_CANCELLED,
                    _ = agent.inbox_ready() => continue,
                }
            }
        }
        break;
    }
    code
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

/// Build the child `Agent` through the SAME load + assemble path the top-level
/// agent uses. The child gets its own fresh `ToolServices` (inside `assemble`),
/// the parent's workspace unless the spec overrides it, the parent's
/// authorization policy, a memory journal, and a prefixed renderer.
#[cfg(feature = "delegation")]
fn make_child_factory(
    deps: &HostDeps,
    parent_workspace: &Path,
    policy: Arc<HostPolicy>,
    catalog_slot: Arc<OnceLock<Arc<Catalog>>>,
    counter: Arc<AtomicUsize>,
) -> AgentFactory {
    let environment_dirs = deps.environment_dirs.clone();
    let date = deps.date.clone();
    let stdout: SharedWriter = deps.stdout.clone();
    let stderr: SharedWriter = deps.stderr.clone();
    let tty = deps.stdout_is_tty;
    let parent_workspace = parent_workspace.to_path_buf();

    Arc::new(move |spec: &ChildSpec| -> Result<ChildAgent, String> {
        let environment = load_environment(&spec.environment, &environment_dirs)
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
        let mut environment = environment;
        ensure_cache_key(&mut environment, &workspace);
        let assembled = assemble(&catalog, &environment, &workspace, &substitutions)
            .map_err(|e| e.to_string())?;
        let route = assembled.resolved.route.origin.route.clone();
        let model = assembled.resolved.route.origin.model.clone();
        let description = format!("{route}/{model}");

        let label = Arc::new(Mutex::new(String::new()));
        let renderer: Arc<dyn EventSink> = Arc::new(Renderer::new(
            stdout.clone(),
            stderr.clone(),
            tty,
            route,
            model,
            label.clone(),
        ));
        let parts = AgentParts {
            provider: assembled.provider,
            tools: assembled.tools,
            system_prompt: assembled.system_prompt,
            options: assembled.options,
            context: Arc::new(DefaultContext),
            authorization: policy.clone(),
            journal: Arc::new(MemoryJournal::new()),
            events: renderer,
        };
        let agent = Agent::new(parts).map_err(|error| error.to_string())?;
        // `InProcessWorkers` assigns `w{n}` after a successful factory call, and
        // factory calls are serialised, so this counter stays aligned with it.
        let id = counter.fetch_add(1, Ordering::SeqCst) + 1;
        *label.lock().unwrap() = format!("[w{id}] ");
        Ok(ChildAgent { agent, description })
    })
}

/// Give the agent a stable provider-side prompt-cache key for its lifetime when the
/// environment sets none. Without one the Codex route served 0 cached tokens across a
/// whole task (measured 2026-09-20); routes without such a key ignore it.
fn ensure_cache_key(environment: &mut p1_assembly::EnvironmentFile, workspace: &std::path::Path) {
    use std::hash::{Hash, Hasher};
    if environment.options.cache_key.is_some() {
        return;
    }
    static AGENTS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    workspace.hash(&mut hasher);
    environment.name.hash(&mut hasher);
    std::process::id().hash(&mut hasher);
    AGENTS
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        .hash(&mut hasher);
    if let Ok(now) = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH) {
        now.as_nanos().hash(&mut hasher);
    }
    environment.options.cache_key = Some(format!("p1-{:016x}", hasher.finish()));
}
