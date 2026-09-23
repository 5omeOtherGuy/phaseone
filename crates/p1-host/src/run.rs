//! The headless and interactive drivers, plus the delegation wiring.
//!
//! Headless runs one turn and then drains inbox turns, waiting for running
//! children so a parent that started a worker is woken by its completion.
//! Interactive prompts on stderr and drains inbox turns without waiting on
//! children. First Ctrl-C cancels the run (and, at exit, the children); second
//! Ctrl-C returns 130 immediately.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

#[cfg(feature = "delegation")]
use std::sync::OnceLock;
#[cfg(feature = "delegation")]
use std::sync::atomic::AtomicUsize;

use p1_assembly::Catalog;
#[cfg(feature = "delegation")]
use p1_assembly::ToolSpec;
use p1_assembly::{Assembled, EnvironmentFile, Substitutions, assemble, load_environment};
use p1_contracts::{
    AgentEvent, BoxFuture, CacheKeySupport, CancellationToken, CommitSink, ContextError,
    ContextInput, ContextPolicy, EventSink, JournalRecord, Prepared, ProviderErrorKind, Tool,
    TurnEnd,
};
use p1_core::{Agent, AgentParts, Reconfiguration, ResumeReport};
#[cfg(feature = "delegation")]
use p1_journal::MemoryJournal;

#[cfg(feature = "delegation")]
use crate::activity::WorkerReportTap;
use crate::activity::{ActivityLog, ActivityTee, Completion, CompletionHub};
use crate::catalog::build_catalog;
use crate::cli::{self, Command, Options};
use crate::frontend::{FrontEnd, LineFrontEnd};
use crate::render::Renderer;
use crate::session;
use crate::{HostDeps, InterruptSource};
use p1_tool_finish::{Accepted, CompletionPolicy};

#[cfg(feature = "delegation")]
use p1_workers::{
    AgentFactory, ChildAgent, ChildId, ChildSpec, ChildStatus, InProcessWorkers, Regrant,
    WorkerReport,
};

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
        // The model list (ADR-0049 stage 1): every environment × the profiles its
        // route binds. No catalog and no network — the credential column is the same
        // non-secret probe `env show` prints.
        Command::Models { search } => models_command(deps, &options, search.as_deref()),
        // The login surface (ADR-0044, spec §6): no catalog, no provider and no
        // network — the store is written and the "which source" report is printed.
        // The route quota ledger (ADR-0052): route metadata and p1-auth credential
        // references go to `p1-usage`; no catalog, no provider.
        Command::Usage(usage) => crate::usage::usage(deps, &usage).await,
        Command::Login { route } => crate::login::login(deps, &route).await,
        Command::LoginList => crate::login::list(deps),
        Command::Logout { route } => crate::login::logout(deps, &route).await,
        Command::Run { .. } => {
            if options.resume && options.session.is_none() {
                write_stderr(deps, "error: --resume requires --session\n");
                return EXIT_USAGE;
            }
            match run_agent(deps, &options).await {
                Ok(code) => code,
                Err(error) => {
                    write_stderr(deps, &format!("{}\n", error.message()));
                    error.code()
                }
            }
        }
        #[cfg(feature = "workflows")]
        Command::WorkflowRun(workflow) => match workflow_run(deps, &options, &workflow).await {
            Ok(code) => code,
            Err(error) => {
                write_stderr(deps, &format!("{}\n", error.message()));
                error.code()
            }
        },
        #[cfg(not(feature = "workflows"))]
        Command::WorkflowRun(_) => {
            write_stderr(deps, "error: this p1 was built without workflows\n");
            EXIT_USAGE
        }
    }
}

/// A run that stopped before it finished. The exit code says what has to be fixed:
/// the command line — a model reference that names no model, a scope pattern that
/// matches nothing, `settings.toml` — or the run itself (an environment, a provider,
/// a journal), which fails the way it always has.
#[derive(Debug)]
pub struct RunError {
    message: String,
    code: i32,
}

impl RunError {
    /// A command line the operator must fix.
    pub fn usage(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            code: EXIT_USAGE,
        }
    }

    pub fn message(&self) -> &str {
        &self.message
    }

    pub fn code(&self) -> i32 {
        self.code
    }
}

impl From<String> for RunError {
    fn from(message: String) -> Self {
        Self {
            message,
            code: EXIT_FAILURE,
        }
    }
}

impl std::fmt::Display for RunError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}

/// The model selection this command line asks for (ADR-0049 stage 1): `--model`,
/// else `--env`, else `settings.toml`'s `default_model`, else the default
/// environment. Reading `settings.toml` is the only reason a bare run touches the
/// p1 config directory.
fn selection(deps: &HostDeps, options: &Options) -> Result<crate::models::Choice, String> {
    // `--models` scopes what the session may cycle through (stage 3); a pattern
    // that matches nothing is a typo, so a run rejects it exactly as `p1 models` does.
    if let Some(patterns) = options.models.as_deref() {
        let models = crate::models::enumerate(&deps.environment_dirs)?;
        crate::models::check_scope(patterns, &models)?;
    }
    crate::models::choose(
        &deps.environment_dirs,
        &crate::auth::locations(deps),
        options.env_given.then_some(options.env.as_str()),
        options.model.as_deref(),
        options.effort,
    )
}

/// `p1 models [SEARCH]` (ADR-0049 stage 1, spec §2): one row per model, sorted by
/// environment then profile. Every failure is a usage error: the reference, the
/// scope or `settings.toml` is what the operator fixes.
fn models_command(deps: &HostDeps, options: &Options, search: Option<&str>) -> i32 {
    match model_table(deps, options.models.as_deref(), search) {
        Ok(table) => {
            write_stdout(deps, &table);
            EXIT_OK
        }
        Err(message) => {
            write_stderr(deps, &format!("{message}\n"));
            EXIT_USAGE
        }
    }
}

/// The `p1 models` table for one scope flag and one optional search — the ONE
/// computation, shared by the command and by the line mode's bare `/model` line.
fn model_table(
    deps: &HostDeps,
    scope_flag: Option<&str>,
    search: Option<&str>,
) -> Result<String, String> {
    let locations = crate::auth::locations(deps);
    let settings = crate::models::load_settings(&locations)?;
    let all = crate::models::enumerate(&deps.environment_dirs)?;
    let scope = crate::models::scope(scope_flag, &settings, &all)?;
    let default = crate::models::default_model(&settings, &deps.environment_dirs, &all)?;
    let rows: Vec<crate::models::Model> = crate::models::search(&all, search)
        .into_iter()
        .cloned()
        .collect();
    crate::models::table(&rows, &scope, default.as_deref(), |route| {
        crate::catalog::credential_line_for_route(route, &deps.environment_dirs, &locations)
    })
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
    // The same for the workflow tools: they assemble, and no run can start.
    #[cfg(feature = "workflows")]
    let catalog = catalog.map(|mut catalog| {
        if deps.workflow_service.is_none() {
            crate::catalog::register_workflow_tools(
                &mut catalog,
                Some(Arc::new(crate::workflow::RefusingWorkflows)),
            );
        }
        catalog
    });
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
    with_worker_tools(&mut environment);
    // Resolve the route binding before assembling: the wire model and the route's
    // own output ceiling come from the route file (spec §2).
    if let Err(message) =
        crate::catalog::resolve_environment(&mut environment, &deps.environment_dirs)
    {
        write_stderr(deps, &format!("{message}\n"));
        return EXIT_FAILURE;
    }
    // Which source this route's credential comes from — never a value (spec §4).
    match crate::catalog::credential_line(
        &environment,
        &deps.environment_dirs,
        &crate::auth::locations(deps),
    ) {
        Ok(Some(line)) => write_stdout(deps, &format!("credential  {line}\n")),
        Ok(None) => {}
        Err(message) => {
            write_stderr(deps, &format!("{message}\n"));
            return EXIT_FAILURE;
        }
    }
    // The model this environment resolves to (ADR-0049 stage 1, spec §2): `E/P`,
    // with the effort its `[options]` carries.
    if let Some(profile) = &environment.profile {
        let effort = environment
            .options
            .reasoning_effort
            .map(|effort| format!(":{}", crate::models::effort_name(effort)))
            .unwrap_or_default();
        write_stdout(
            deps,
            &format!("model  {}/{}{effort}\n", environment.name, profile.id),
        );
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

/// The default `Run` path: build the line front end and hand the run over. The
/// single branch point below is where a session that owns its own event sink and
/// run loop (the TUI, `--tui`) would construct its front end — or it can call
/// [`run_with_front_end`] directly, leaving `run.rs` untouched.
async fn run_agent(deps: &mut HostDeps, options: &Options) -> Result<i32, RunError> {
    let cancel = CancellationToken::new();
    // The ONE branch point: the TUI (issue #12) owns the terminal when --tui.
    let front_end: Arc<dyn FrontEnd> = if options.tui {
        let workspace = options
            .workspace
            .clone()
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
        Arc::new(crate::tui::TuiFrontEnd::new(
            crate::tui::TuiOptions {
                // The label the TUI shows is the environment the selection chose,
                // not the `--env` name it was asked for.
                env: selection(deps, options)
                    .map_err(RunError::usage)?
                    .environment,
                ask: options.ask,
                workspace,
                sandbox: format!("{:?}", options.sandbox).to_lowercase(),
                // §10 `effort`: only an explicit `--effort` (already parsed by
                // `cli.rs`); `None` is the adapter default, which the statusline
                // already renders as `default`.
                effort: options
                    .effort
                    .map(|effort| crate::models::effort_name(effort).to_string()),
            },
            cancel.clone(),
        ))
    } else {
        Arc::new(LineFrontEnd::new(deps, options, cancel.clone()))
    };
    run_with_front_end(deps, options, cancel, front_end).await
}

/// Assemble the parent agent and drive it through `front_end`. `run_agent` calls
/// this with a [`LineFrontEnd`]; a custom front end (issue #12) calls it with
/// its own. The composition below is unchanged: only the event sink, the
/// authorization policy and the child sinks come from the front end, and the run
/// loop is handed to it.
pub async fn run_with_front_end(
    deps: &mut HostDeps,
    options: &Options,
    cancel: CancellationToken,
    front_end: Arc<dyn FrontEnd>,
) -> Result<i32, RunError> {
    let workspace = resolve_workspace(options)?;
    // The §3c stall guard is host policy and applies only to unattended runs; the
    // front end decides what "headless" means (the line front end uses the CLI
    // rule, a terminal UI is interactive by definition).
    let headless = front_end.is_headless(options);

    // The delegation service must exist before the catalog so the `worker_*`
    // tools can be registered; the child factory reaches the catalog lazily,
    // breaking the cycle (children never assemble delegation tools).
    let completion_hub = Arc::new(CompletionHub::new());
    #[cfg(feature = "delegation")]
    let catalog_slot: Arc<OnceLock<Arc<Catalog>>> = Arc::new(OnceLock::new());
    #[cfg(feature = "delegation")]
    let child_counter = Arc::new(AtomicUsize::new(0));
    #[cfg(feature = "delegation")]
    let agent_ordinals = Arc::new(AtomicUsize::new(1));
    // The child factory's §3c guard stops a child's turn through the service, which
    // does not exist yet — the factory is its argument. Same slot pattern.
    #[cfg(feature = "delegation")]
    let service_slot: Arc<OnceLock<Arc<InProcessWorkers>>> = Arc::new(OnceLock::new());
    #[cfg(feature = "delegation")]
    let child_builder = Arc::new(ChildBuilder::new(
        deps,
        &workspace,
        front_end.clone(),
        catalog_slot.clone(),
        child_counter.clone(),
        agent_ordinals,
        completion_hub.clone(),
        options.session.clone(),
        options.max_idle_summaries,
        service_slot.clone(),
    ));
    #[cfg(feature = "delegation")]
    let service: Option<Arc<InProcessWorkers>> = {
        let factory = make_child_factory(child_builder.clone());
        let service = InProcessWorkers::new(factory, 2);
        let _ = service_slot.set(service.clone());
        deps.worker_service = Some(service.clone());
        Some(service)
    };
    // Steps are workers of the SAME service built by the SAME builder, so the service
    // and the slots must exist first; the catalog below then registers the tools.
    #[cfg(feature = "workflows")]
    let workflows = match &service {
        Some(service) => Some(
            crate::workflow::compose(
                deps,
                child_builder.clone(),
                service.clone(),
                crate::workflow::run_root(deps, options.session.as_deref()),
            )
            .map_err(RunError::usage)?,
        ),
        None => None,
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

    // The chosen model (ADR-0049 stage 1): the environment the reference or
    // `default_model` named, with the selected profile applied on top of it. The
    // resolution and assembly path below is unchanged — `resolve_environment` still
    // turns the profile into the route's wire model.
    let choice = selection(deps, options).map_err(RunError::usage)?;
    let mut environment = load_environment(&choice.environment, &deps.environment_dirs)
        .map_err(|error| error.to_string())?;
    with_worker_tools(&mut environment);
    crate::models::apply(&mut environment, &choice, &deps.environment_dirs)
        .map_err(RunError::usage)?;
    crate::catalog::resolve_environment(&mut environment, &deps.environment_dirs)?;
    let substitutions = substitutions(deps, &workspace);
    let assembled = assemble_with_cache_key(
        &catalog,
        &environment,
        &workspace,
        &substitutions,
        PARENT_ORDINAL,
    )?;
    // The `finish` factory issued this agent's completion state during `assemble`.
    // `None` when the environment does not assemble `finish`.
    let completion = completion_hub.take();
    let context = agent_context(&assembled)?;
    let route = assembled.resolved.route.origin.route.clone();
    let model = assembled.resolved.route.origin.model.clone();
    // The session's model, for a later switch (ADR-0049 stage 3): the environment it
    // runs, the profile it selected and the `finish` tool it keeps.
    let session_environment = assembled.resolved.environment.clone();
    let session_finish = finish_tool(&assembled);

    // Announce the assembled parent before the agent is built: the front end
    // builds its parent renderer from this.
    front_end.parent_assembled(&route, &model, completion.clone());
    // §10 `ctx`'s denominator: unknown (no `[context]` section) stays `None`,
    // never a guessed window.
    front_end.context_configured(
        assembled.resolved.context.as_ref().map(|c| c.window_tokens),
        assembled
            .resolved
            .context
            .as_ref()
            .map(|c| c.summarize_at_tokens),
    );

    let (journal, records): OpenedSession = open_session(deps, options)?;
    // On resume the journal holds the earlier turns; rebuild this agent's activity
    // from them so a verification run before the restart still counts and a file
    // change before it still invalidates (completion.md §3).
    if let (Some(completion), Some(records)) = (&completion, &records) {
        completion.log.replay(&assembled.tools, records);
    }

    // The activity tee forwards every event to the front end's sink unchanged and
    // feeds this agent's log the effects and exit codes a later `finish` reads.
    // It is installed even without `finish`: the headless stall guard (§3c) reads
    // the same log for workspace mutations. Without a `finish` tool the hub issued
    // no log, so the host makes one.
    let log = match &completion {
        Some(completion) => completion.log.clone(),
        None => Arc::new(ActivityLog::default()),
    };
    // ADR-0055: this agent's commands are measured in `workspace`, and the host's own
    // session journals inside it are not workspace content. Set AFTER the replay
    // above, so a replayed call is never fingerprinted: the journal does not carry
    // what a past command did.
    log.watch_workspace(&workspace, &session_journals(options.session.as_deref()));
    let activity = Arc::new(ParentActivity::new(
        front_end.event_sink(),
        log.clone(),
        &assembled.tools,
    ));
    let events: Arc<dyn EventSink> = activity.clone();
    // The guard is headless-only (completion.md §3c); an interactive user sees the
    // summaries and decides.
    let mut stall: Option<Arc<StallGuard>> = None;
    let events: Arc<dyn EventSink> = if headless {
        let guard = Arc::new(StallGuard::new(
            activity.clone(),
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
        authorization: front_end.authorization(),
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
            #[cfg(feature = "workflows")]
            if workflows.is_some() {
                announce_lost_runs(deps, options);
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
    #[cfg(feature = "workflows")]
    if let Some(workflows) = &workflows {
        workflows.observer.set_parent_inbox(agent.inbox());
    }

    #[cfg(feature = "delegation")]
    let workers: Option<Arc<dyn crate::frontend::WorkerService>> = service
        .as_ref()
        .map(|service| service.clone() as Arc<dyn crate::frontend::WorkerService>);
    #[cfg(not(feature = "delegation"))]
    let workers: Option<Arc<dyn crate::frontend::WorkerService>> = None;

    // The model-switch context (ADR-0049 stage 3): the SAME catalog, cache-key
    // policy and completion plumbing the start path used, plus the session's own
    // `finish` tool. The line mode uses it between turns; the TUI's run loop will.
    deps.model_switch = Some(Arc::new(ModelSwitch {
        catalog: catalog.clone(),
        completion: completion_hub.clone(),
        activity: activity.clone(),
        environment_dirs: deps.environment_dirs.clone(),
        workspace: workspace.clone(),
        substitutions: substitutions.clone(),
        ignored: session_journals(options.session.as_deref()),
        scope: options.models.clone(),
        route_label: front_end.route_label(),
        session: Mutex::new(SessionModel {
            environment: session_environment,
            profile: choice.profile.clone(),
            finish: session_finish,
        }),
    }));

    let code = front_end
        .run(deps, &mut agent, &cancel, workers, stall)
        .await;

    // ADR-0055 item 4: a workspace fingerprint that could not be taken means the run
    // fell back to the tool-declared rule. Say so once, so a silent downgrade is never
    // invisible to the operator or the run report.
    if let Some(error) = log.fingerprint_error() {
        write_stderr(
            deps,
            &format!("note: workspace fingerprinting is off: {error}\n"),
        );
    }

    // Runs first: a run cancelled here cancels its step workers through the worker
    // service, which must still be up to do it and to let the journal get `Ended`.
    #[cfg(feature = "workflows")]
    if let Some(workflows) = &workflows {
        workflows.service.shutdown().await;
    }
    #[cfg(feature = "delegation")]
    if let Some(service) = &service {
        service.shutdown().await;
    }

    front_end.finish();
    Ok(code)
}

/// `p1 workflow run` (ADR-0053): the composition of a run — catalog, worker service,
/// workflow service, line front end — with NO parent agent. The run's lines go to
/// stderr as they come; its report, rendered as `workflow_result` renders it, goes to
/// stdout; the exit code is the outcome.
#[cfg(feature = "workflows")]
async fn workflow_run(
    deps: &mut HostDeps,
    options: &Options,
    workflow: &cli::WorkflowRunOptions,
) -> Result<i32, RunError> {
    use p1_workflow::WorkflowService as _;
    let script = std::fs::read_to_string(&workflow.file).map_err(|error| {
        RunError::usage(format!(
            "cannot read the workflow script {}: {error}",
            workflow.file.display()
        ))
    })?;
    let args = workflow_args(workflow).map_err(RunError::usage)?;
    let workspace = resolve_workspace(options)?;
    let cancel = CancellationToken::new();
    let front_end: Arc<dyn FrontEnd> = Arc::new(LineFrontEnd::new(deps, options, cancel.clone()));

    // The same slots and builder `run_with_front_end` composes: a step worker is built
    // exactly as a direct worker is.
    let completion_hub = Arc::new(CompletionHub::new());
    let catalog_slot: Arc<OnceLock<Arc<Catalog>>> = Arc::new(OnceLock::new());
    let service_slot: Arc<OnceLock<Arc<InProcessWorkers>>> = Arc::new(OnceLock::new());
    let child_builder = Arc::new(ChildBuilder::new(
        deps,
        &workspace,
        front_end.clone(),
        catalog_slot.clone(),
        Arc::new(AtomicUsize::new(0)),
        Arc::new(AtomicUsize::new(1)),
        completion_hub.clone(),
        options.session.clone(),
        options.max_idle_summaries,
        service_slot.clone(),
    ));
    let service = InProcessWorkers::new(
        make_child_factory(child_builder.clone()),
        workflow.max_workers,
    );
    let _ = service_slot.set(service.clone());
    deps.worker_service = Some(service.clone());
    let run_root = match &workflow.out {
        Some(out) => out.clone(),
        None => crate::workflow::run_root(deps, options.session.as_deref()),
    };
    let workflows = crate::workflow::compose(deps, child_builder, service.clone(), run_root)
        .map_err(RunError::usage)?;
    let catalog = Arc::new(build_catalog(
        deps,
        options.sandbox,
        &options.sandbox_write,
        &options.sandbox_read,
        &options.env_pass,
        &completion_hub,
    )?);
    let _ = catalog_slot.set(catalog);

    let request = p1_workflow::StartRequest {
        script,
        args,
        resume_from: workflow.resume_from.clone().map(p1_workflow::RunId),
        role_models: workflow.roles.iter().cloned().collect(),
        workspace: Some(workspace),
    };
    let code = match workflows.service.start(request).await {
        Err(error) => {
            write_stderr(deps, &format!("{error}\n"));
            EXIT_FAILURE
        }
        Ok(id) => {
            let second = Arc::new(tokio::sync::Notify::new());
            spawn_interrupt(deps.interrupt.clone(), cancel.clone(), second);
            let mut status = workflows.service.wait(&id, cancel.clone()).await;
            if matches!(status, Ok(p1_workflow::RunStatus::Running(_))) {
                // Ctrl-C: cancel the run and wait for its `Ended`, which the engine
                // journals once the in-flight steps have been cancelled.
                let _ = workflows.service.cancel(&id).await;
                status = workflows.service.wait(&id, CancellationToken::new()).await;
            }
            // The end line is the observer's; let it out before the report.
            workflows.observer.settled(&id).await;
            match status {
                Ok(p1_workflow::RunStatus::Ended(report)) => {
                    write_stdout(deps, &(workflow_report(&workflows, &id).await + "\n"));
                    match report.outcome {
                        p1_workflow::RunOutcome::Completed => EXIT_OK,
                        p1_workflow::RunOutcome::CompletedWithIssues => EXIT_USAGE,
                        p1_workflow::RunOutcome::Failed => EXIT_FAILURE,
                        p1_workflow::RunOutcome::Cancelled => EXIT_CANCELLED,
                    }
                }
                Ok(p1_workflow::RunStatus::Running(_)) => EXIT_CANCELLED,
                Err(error) => {
                    write_stderr(deps, &format!("{error}\n"));
                    EXIT_FAILURE
                }
            }
        }
    };

    workflows.service.shutdown().await;
    service.shutdown().await;
    front_end.finish();
    Ok(code)
}

/// `--args FILE` (a JSON object) with every `--arg k=v` laid over it, in order. A value
/// that parses as JSON is that JSON (`n=3`, `items=[…]`); anything else is a string.
#[cfg(feature = "workflows")]
fn workflow_args(workflow: &cli::WorkflowRunOptions) -> Result<serde_json::Value, String> {
    let mut args = match &workflow.args_file {
        None => serde_json::Map::new(),
        Some(path) => {
            let text = std::fs::read_to_string(path)
                .map_err(|error| format!("cannot read --args {}: {error}", path.display()))?;
            match serde_json::from_str::<serde_json::Value>(&text) {
                Ok(serde_json::Value::Object(map)) => map,
                Ok(_) => return Err(format!("--args {} is not a JSON object", path.display())),
                Err(error) => {
                    return Err(format!("--args {} is not JSON: {error}", path.display()));
                }
            }
        }
    };
    for (key, value) in &workflow.args {
        let value = serde_json::from_str(value)
            .unwrap_or_else(|_| serde_json::Value::String(value.clone()));
        args.insert(key.clone(), value);
    }
    Ok(serde_json::Value::Object(args))
}

/// The report exactly as the `workflow_result` tool renders it for a model.
#[cfg(feature = "workflows")]
async fn workflow_report(
    workflows: &crate::workflow::Workflows,
    id: &p1_workflow::RunId,
) -> String {
    use p1_contracts::{Tool, ToolCall, ToolContext, ToolInput};
    let tool = p1_tool_workflow::WorkflowResultTool::new(workflows.service.clone());
    let call = ToolCall {
        call_id: "workflow-run".to_string(),
        name: "workflow_result".to_string(),
        input: ToolInput::Json(serde_json::json!({ "id": id.0 }).to_string()),
    };
    let context = ToolContext {
        cancel: CancellationToken::new(),
    };
    tool.execute(&call, context).await.content
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

pub(crate) async fn run_headless(
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
        // A workflow run's end also arrives on the inbox, as ONE notification.
        #[cfg(feature = "workflows")]
        let runs = crate::workflow::running_workflows(_deps);
        #[cfg(not(feature = "workflows"))]
        let runs = 0;
        if runs > 0 || running_children(_deps).await > 0 {
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

/// The interactive line loop: one turn per line, `/exit` to stop, and — ADR-0049
/// stage 3 — `/model` and `/effort` to switch the model of the running session.
/// The loop only ever runs between turns, so a switch never lands inside a tool
/// loop. Every other `/…` line stays what it is today: a prompt for the model.
pub(crate) async fn run_interactive(
    deps: &HostDeps,
    agent: &mut Agent,
    cancel: &CancellationToken,
) -> i32 {
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
        if let Some(switch) = &deps.model_switch
            && let Some(reference) = argument(text, "/model")
        {
            if reference.is_empty() {
                match model_table(deps, switch.scope_flag(), None) {
                    Ok(table) => write_stderr(deps, &table),
                    Err(reason) => write_stderr(deps, &format!("· {reason}\n")),
                }
            } else {
                report_model(
                    deps,
                    switch_model(switch, agent, SwitchRequest::Model(reference)),
                );
            }
            continue;
        }
        if let Some(switch) = &deps.model_switch
            && let Some(level) = argument(text, "/effort")
        {
            report_model(
                deps,
                switch_model(switch, agent, SwitchRequest::Effort(level)),
            );
            continue;
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

/// The argument of a `/name` line: `Some("")` for the bare command, `Some(rest)`
/// when whitespace follows it, and `None` for anything else — so `/models` is not
/// `/model` and stays a prompt for the model.
fn argument<'a>(text: &'a str, name: &str) -> Option<&'a str> {
    let rest = text.strip_prefix(name)?;
    if rest.is_empty() || rest.starts_with(char::is_whitespace) {
        Some(rest.trim())
    } else {
        None
    }
}

/// What a `/model` or `/effort` line did, on the host's own channel.
fn report_model(deps: &HostDeps, outcome: Result<String, String>) {
    match outcome {
        Ok(model) => write_stderr(deps, &format!("· model: {model}\n")),
        Err(reason) => write_stderr(deps, &format!("· model not changed: {reason}\n")),
    }
}

// ------------------------------------------ the model switch (ADR-0049 stage 3)
//
// A switch moves the parent renderer's route label (the same way the session's
// environment moves) so the per-response line names the route that produced THAT
// response: `ResponseCompleted` carries the response item's own model, and the
// renderer's label is the route the session is assembled on. A switch that fails
// changes nothing, the label included.

/// The catalog key of the `finish` tool (`catalog.rs`). Its activity log and its
/// outcome are SESSION state — fed from the event stream and read by the run — so a
/// switch keeps the session's instance of it.
const FINISH_MODULE: &str = "finish";

/// The four worker tools, in the order the host appends them to a main agent.
#[cfg(feature = "delegation")]
const WORKER_MODULES: [&str; 4] = [
    "worker_start",
    "worker_result",
    "worker_continue",
    "worker_cancel",
];

/// The four workflow tools, appended after the worker tools.
#[cfg(feature = "workflows")]
const WORKFLOW_MODULES: [&str; 4] = [
    "workflow_start",
    "workflow_status",
    "workflow_result",
    "workflow_cancel",
];

/// Give every MAIN agent the worker tools (ADR-0050 item 1). Appends a default-face
/// [`ToolSpec`] for each worker module the environment does not already list, in
/// `worker_start`, `worker_result`, `worker_continue`, `worker_cancel` order; an
/// environment that lists one keeps its own entry (which carries a face). Called only
/// at the three main-agent assembly sites — never in the child factory, so a worker
/// never gets the worker tools. A no-op when the `delegation` feature is not compiled.
/// With `workflows` the four `workflow_*` tools follow the same way (ADR-0053 item 7).
#[cfg(feature = "delegation")]
fn with_worker_tools(environment: &mut EnvironmentFile) {
    #[cfg(feature = "workflows")]
    let modules = WORKER_MODULES.iter().chain(WORKFLOW_MODULES.iter());
    #[cfg(not(feature = "workflows"))]
    let modules = WORKER_MODULES.iter();
    for &module in modules {
        if environment.tools.iter().any(|tool| tool.module == module) {
            continue;
        }
        environment.tools.push(ToolSpec {
            module: module.to_string(),
            name: None,
            description: None,
            variant: None,
        });
    }
}

#[cfg(not(feature = "delegation"))]
fn with_worker_tools(_environment: &mut EnvironmentFile) {}

/// Where the `finish` tool sits in an assembly: `resolved.tools` and `tools` are
/// built from the same environment list, in order.
fn finish_index(assembled: &Assembled) -> Option<usize> {
    assembled
        .resolved
        .tools
        .iter()
        .position(|tool| tool.module == FINISH_MODULE)
}

/// The session's `finish` tool in an assembly, when the environment declares one.
fn finish_tool(assembled: &Assembled) -> Option<Arc<dyn Tool>> {
    finish_index(assembled).map(|index| assembled.tools[index].clone())
}

/// The shell tool's identity implementation: the one identity the host's completion
/// policy reads (ADR-0052 item 1) to decide whether a child can verify anything
/// itself. Never a model-facing name, which an environment's face may change.
#[cfg(feature = "delegation")]
const SHELL_IMPLEMENTATION: &str = "p1-tool-shell";

/// The completion policy a CHILD's assembled tools call for (ADR-0052 item 1): a
/// worker whose tools include no tool that records command runs cannot verify
/// anything itself, so it reports to its parent instead of naming a command it cannot
/// run. The check is on the tools' IDENTITIES — the technique `WorkerReportTap` uses
/// to find `finish` — never on grant names, so a face cannot hide the shell tool.
///
/// MAIN agents never come through here: their `finish` keeps the strict rule.
#[cfg(feature = "delegation")]
fn completion_policy(tools: &[Arc<dyn Tool>]) -> CompletionPolicy {
    let can_run_commands = tools
        .iter()
        .any(|tool| tool.identity().implementation == SHELL_IMPLEMENTATION);
    if can_run_commands {
        CompletionPolicy::RecordedCommands
    } else {
        CompletionPolicy::ReportToParent
    }
}

/// Apply the child's policy to the `finish` tool the catalog assembled (ADR-0052 item
/// 1). The catalog builds the tool before the host knows the assembled tools, and the
/// policy follows from THEM, so it is applied here: the tool keeps the child's own
/// activity log and outcome cell — its whole history, which a freshly assembled
/// `finish` would not see — and its model-facing name and variant.
#[cfg(feature = "delegation")]
fn apply_completion_policy(
    assembled: &mut Assembled,
    completion: &Completion,
    contract: Option<p1_tool_finish::OutputContract>,
) {
    let Some(index) = finish_index(assembled) else {
        return;
    };
    let policy = completion_policy(&assembled.tools);
    let finish = finish_under_policy(
        &assembled.tools[index],
        completion.log.clone(),
        completion.outcome.clone(),
        policy,
        contract,
    );
    // `resolved` is what the host journals and prints: keep the declaration in step
    // with the tool the model is actually given.
    assembled.resolved.tools[index].declaration = finish.declaration().clone();
    assembled.tools[index] = finish;
}

/// The same `finish` tool under `policy`, on the given activity log and outcome cell.
/// The description follows the policy — it is what tells the model which completion
/// rule applies to it — while the name and variant stay the ones the agent was
/// assembled with. A `contract` (a workflow step's schema) is set before the face is
/// taken, so the description carries the contract paragraph: a face pins the text it
/// is given and would never gain it afterwards.
#[cfg(feature = "delegation")]
fn finish_under_policy(
    finish: &Arc<dyn Tool>,
    log: Arc<ActivityLog>,
    outcome: p1_tool_finish::FinishOutcome,
    policy: CompletionPolicy,
    contract: Option<p1_tool_finish::OutputContract>,
) -> Arc<dyn Tool> {
    let name = finish.declaration().name.clone();
    let variant = finish.identity().variant.clone();
    let mut tool = p1_tool_finish::FinishTool::new(log, outcome).with_policy(policy);
    if let Some(contract) = contract {
        tool = tool.with_output_contract(contract);
    }
    let face = p1_tool_finish::ToolFace::new(name, tool.declaration().description.clone());
    Arc::new(tool.with_face(face, &variant))
}

/// The session's model (ADR-0049 stage 3): what a `/model` or `/effort` line
/// changes, plus the `finish` tool the session keeps.
struct SessionModel {
    /// The environment the session runs now: §1 rule 2's "current environment".
    environment: String,
    /// The profile the session selected (`None` keeps the environment's own).
    profile: Option<String>,
    /// The `finish` tool the session keeps, when its environment assembles one.
    finish: Option<Arc<dyn Tool>>,
}

/// Everything a model switch needs of the host (ADR-0049 stage 3, spec §4). `run`
/// builds it once the catalog and the parent's activity plumbing exist and stores it
/// on [`HostDeps`], so the line mode switches now and the TUI's run loop can call
/// [`switch_model`] with it.
pub(crate) struct ModelSwitch {
    /// The session's catalog: a switch assembles exactly as the start path did.
    catalog: Arc<Catalog>,
    /// The hub the catalog's `finish` factory issues into.
    completion: Arc<CompletionHub>,
    /// The parent's activity plumbing, re-pointed when the switched `finish` is not
    /// the session's own.
    activity: Arc<ParentActivity>,
    /// The parent's environment search path, workspace and substitutions.
    environment_dirs: Vec<PathBuf>,
    workspace: PathBuf,
    substitutions: Substitutions,
    /// The host's own session journals inside the workspace (ADR-0055): a switch
    /// assembles a new log, which must ignore them exactly as the first one did.
    ignored: Vec<PathBuf>,
    /// The run's `--models` scope, for the bare `/model` table.
    scope: Option<String>,
    /// The parent renderer's route label, when the front end has one: a successful
    /// switch moves it to the new assembly's route label.
    route_label: Option<Arc<Mutex<String>>>,
    session: Mutex<SessionModel>,
}

impl ModelSwitch {
    /// The `--models` value this run was given, if any.
    fn scope_flag(&self) -> Option<&str> {
        self.scope.as_deref()
    }
}

/// What a `/model` or `/effort` line asks for (spec §4).
pub(crate) enum SwitchRequest<'a> {
    /// `/model REF`: a model reference — `E/P`, a bare `P`, optionally `:effort`.
    Model(&'a str),
    /// `/effort LEVEL`: the model the session runs now, with a new effort only.
    Effort(&'a str),
}

/// The ONE model-switch entry point (ADR-0049 stage 3, spec §1 and §4): resolve the
/// reference against the CURRENT session environment, load the environment, apply
/// the selection, resolve the route binding and assemble EXACTLY as the start path
/// does — the same catalog, the same cache-key policy with the parent's ordinal —
/// then hand the result to `Agent::reconfigure`, which validates it against the
/// current history.
///
/// On success the new `E/P[:effort]` is returned and the session's model state is
/// updated. On any failure the reason is returned and NOTHING changes: the agent
/// keeps its model.
pub(crate) fn switch_model(
    switch: &ModelSwitch,
    agent: &mut Agent,
    request: SwitchRequest<'_>,
) -> Result<String, String> {
    let mut session = switch.session.lock().unwrap();
    let choice = match request {
        SwitchRequest::Model(reference) => {
            let models = crate::models::enumerate(&switch.environment_dirs)?;
            let resolved = crate::models::resolve(reference, &session.environment, &models)?;
            crate::models::Choice {
                environment: resolved.environment,
                profile: Some(resolved.profile),
                effort: resolved.effort,
            }
        }
        // `/effort LEVEL` keeps the model and replaces only the effort.
        SwitchRequest::Effort(level) => crate::models::Choice {
            environment: session.environment.clone(),
            profile: session.profile.clone(),
            effort: Some(crate::models::parse_effort(level)?),
        },
    };
    let mut environment = load_environment(&choice.environment, &switch.environment_dirs)
        .map_err(|error| error.to_string())?;
    with_worker_tools(&mut environment);
    crate::models::apply(&mut environment, &choice, &switch.environment_dirs)?;
    crate::catalog::resolve_environment(&mut environment, &switch.environment_dirs)?;
    let assembled = assemble_with_cache_key(
        &switch.catalog,
        &environment,
        &switch.workspace,
        &switch.substitutions,
        PARENT_ORDINAL,
    )?;
    // The catalog's `finish` factory issued this assembly its own completion. Take
    // it, so the hub cannot hand a stale one to a later worker assembly, and so it
    // is there for the switched tool set's own `finish` (below).
    let issued = switch.completion.take();
    let finish_at = finish_index(&assembled);
    // The label the renderer names after this switch, exactly as the start path
    // named it (`Origin.route`, `<adapter>/<account>`).
    let route = assembled.resolved.route.origin.route.clone();
    let context = agent_context(&assembled)?;
    let mut tools = assembled.tools;
    // The switched tool set's `finish` must reach the completion the run reads. The
    // session keeps ITS `finish` — the whole session's activity is in that tool's
    // log — when the environment declares it under the same model-facing name;
    // otherwise the switched tool set's own is the session's from now on, and the
    // plumbing follows the completion the catalog just issued it (which the `finish`
    // factory always does).
    let adopted = match (&session.finish, finish_at) {
        (Some(kept), Some(index)) if kept.declaration().name == tools[index].declaration().name => {
            tools[index] = kept.clone();
            None
        }
        (_, Some(_)) => issued,
        _ => None,
    };
    // `reconfigure` validates against the CURRENT history and, on failure, changes
    // nothing at all — so the session state below is only updated once it is `Ok`.
    agent
        .reconfigure(Reconfiguration {
            provider: assembled.provider,
            tools: tools.clone(),
            system_prompt: assembled.system_prompt,
            options: assembled.options,
            context,
        })
        .map_err(|error| error.to_string())?;
    if let Some(completion) = adopted {
        // The switched `finish` writes the completion the catalog issued: point the
        // parent's activity plumbing (and the §3c guard, which reads it) at it, so
        // file changes and summaries reach the log that tool reads. The new log
        // measures the same workspace, minus the host's own journals (ADR-0055).
        completion
            .log
            .watch_workspace(&switch.workspace, &switch.ignored);
        switch.activity.repoint(completion.log.clone(), &tools);
        session.finish = finish_at.map(|index| tools[index].clone());
    }
    session.environment = environment.name.clone();
    session.profile = environment
        .profile
        .as_ref()
        .map(|profile| profile.id.clone());
    // The session's route label moves only now: a failed switch changed nothing.
    if let Some(label) = &switch.route_label {
        *label.lock().unwrap() = route;
    }
    Ok(model_name(&environment))
}

/// The model a loaded environment runs, as the operator writes it: `E/P`, with the
/// effort its `[options]` carry when they carry one.
fn model_name(environment: &p1_assembly::EnvironmentFile) -> String {
    let model = match &environment.profile {
        Some(profile) => format!("{}/{}", environment.name, profile.id),
        None => environment.name.clone(),
    };
    match environment.options.reasoning_effort {
        Some(effort) => format!("{model}:{}", crate::models::effort_name(effort)),
        None => model,
    }
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

// ------------------------------------------------------ the parent's activity

/// The parent's activity plumbing (ADR-0049 stage 3): every event goes to the front
/// end unchanged and is recorded into the log of the assembly that is CURRENT, and
/// the §3c guard counts that log's replacements. A model switch re-points it at the
/// completion the switched tool set's `finish` writes, so the activity log and the
/// `finish` outcome stay the session's, not the switched-to model's.
struct ParentActivity {
    /// The front end's sink, to build the next tee with.
    front: Arc<dyn EventSink>,
    current: Mutex<CurrentActivity>,
}

struct CurrentActivity {
    /// The log the current `finish` tool writes; the guard reads it.
    log: Arc<ActivityLog>,
    /// Records into that log (its own clone of it) and forwards to the front end.
    tee: ActivityTee,
}

impl ParentActivity {
    fn new(front: Arc<dyn EventSink>, log: Arc<ActivityLog>, tools: &[Arc<dyn Tool>]) -> Self {
        let tee = ActivityTee::new(front.clone(), log.clone(), tools);
        Self {
            front,
            current: Mutex::new(CurrentActivity { log, tee }),
        }
    }

    /// Follow another assembly's completion: its log and its tools (the effect of a
    /// call comes from the tool that will actually run it).
    fn repoint(&self, log: Arc<ActivityLog>, tools: &[Arc<dyn Tool>]) {
        let tee = ActivityTee::new(self.front.clone(), log.clone(), tools);
        *self.current.lock().unwrap() = CurrentActivity { log, tee };
    }

    /// One committed context replacement, in the CURRENT log (completion.md §3c).
    fn record_replacement(&self) {
        self.current.lock().unwrap().log.record_replacement();
    }

    fn consecutive_replacements(&self) -> u64 {
        self.current.lock().unwrap().log.consecutive_replacements()
    }
}

impl EventSink for ParentActivity {
    fn emit(&self, event: AgentEvent) {
        self.current.lock().unwrap().tee.emit(event);
    }
}

// ------------------------------------------------------ stall guard (completion.md §3c)

/// The headless §3c guard: count consecutive context replacements and cancel the
/// turn when they reach `--max-idle-summaries`. Progress — a workspace mutation or
/// a `finish` call of any status — resets the count through [`ActivityLog`].
///
/// Opaque to a front end: it is handed to [`crate::frontend::FrontEnd::run`] so the
/// line drivers can report a stall, and a custom front end may ignore it.
pub struct StallGuard {
    activity: Arc<ParentActivity>,
    max: usize,
    cancel: CancellationToken,
    stalled: AtomicBool,
}

impl StallGuard {
    fn new(activity: Arc<ParentActivity>, max: usize, cancel: CancellationToken) -> Self {
        Self {
            activity,
            max,
            cancel,
            stalled: AtomicBool::new(false),
        }
    }

    /// One committed `ContextReplaced` was observed. On reaching the bound the
    /// guard latches and cancels the turn; the driver then reports the stall.
    fn on_context_replaced(&self) {
        self.activity.record_replacement();
        if self.max > 0 && self.activity.consecutive_replacements() >= self.max as u64 {
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

// -------------------------------------------------- the same guard for a child

/// The per-child §3c guard. A delegated worker is always unattended — nobody
/// reads its summaries and decides — so the parent's `--max-idle-summaries` bound
/// applies to EVERY child, whatever the parent's own mode is (0 disables, as for
/// the parent).
///
/// It counts the CHILD's committed replacements in the child's own
/// [`ActivityLog`], the same log [`ActivityTee`] already feeds for that child, and
/// progress (a workspace mutation or a `finish` call) resets the count exactly as
/// it does for the parent. At the bound it stops that child's running turn through
/// the worker service: the child's turn token lives there and this factory cannot
/// reach it. The child then ends [`ChildStatus::Failed`] with the parent's own
/// sentence — the parent model and the operator read the same words, and the
/// worker is not merely "cancelled". Nothing here touches the run's cancellation:
/// one child's stall leaves the parent and every other child running.
#[cfg(feature = "delegation")]
struct ChildStallWatcher {
    inner: Arc<dyn EventSink>,
    log: Arc<ActivityLog>,
    max: usize,
    message: String,
    service: Arc<OnceLock<Arc<InProcessWorkers>>>,
    worker_id: String,
}

#[cfg(feature = "delegation")]
impl EventSink for ChildStallWatcher {
    fn emit(&self, event: AgentEvent) {
        if matches!(event, AgentEvent::ContextReplaced { .. }) {
            self.log.record_replacement();
            if self.log.consecutive_replacements() >= self.max as u64 {
                // The service is built AFTER this factory (the factory is its
                // argument), so the slot is how the guard reaches the child's turn.
                if let Some(service) = self.service.get() {
                    let _ =
                        service.stall_child(&ChildId(self.worker_id.clone()), self.message.clone());
                }
            }
        }
        self.inner.emit(event);
    }
}

/// The last turn end of ONE workflow step worker (ADR-0054 item 3). A step's fallback
/// chain turns on the ROUTE failing, and `ChildStatus::Failed`'s message cannot say
/// whether the failure was the route's or the worker's own work; this cell can.
#[cfg(feature = "delegation")]
pub(crate) type TurnEndCell = Arc<Mutex<Option<TurnEnd>>>;

/// Records each finished turn's end into a step's cell and forwards the event
/// unchanged. It sits OUTSIDE the worker report tap, so it sees every turn of the
/// worker, a repair included.
#[cfg(feature = "delegation")]
struct TurnEndTap {
    inner: Arc<dyn EventSink>,
    cell: TurnEndCell,
}

#[cfg(feature = "delegation")]
impl EventSink for TurnEndTap {
    fn emit(&self, event: AgentEvent) {
        if let AgentEvent::TurnFinished { end } = &event {
            *self.cell.lock().unwrap() = Some(end.clone());
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

/// Workflow runs, like workers, live in the process that started them: the new
/// service knows none of the earlier runs. Their journals stay on disk, which is what
/// `resume_from` replays, so the user is told once where they are.
#[cfg(feature = "workflows")]
fn announce_lost_runs(deps: &HostDeps, options: &Options) {
    let root = crate::workflow::run_root(deps, options.session.as_deref());
    let runs = crate::workflow::lost_runs(&root);
    if runs.is_empty() {
        return;
    }
    write_stderr(
        deps,
        &format!(
            "resume: workflow run(s) {} belonged to the earlier process and are not restored; \
             their journals stay in {} for resume_from\n",
            runs.join(", "),
            root.display()
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

/// The paths the host itself appends to inside the workspace (ADR-0055): the
/// `--session` journal, which a run writes a record to for every event, and — by the
/// name rule the fingerprint applies — the `FILE.w{n}.jsonl` journals beside it.
/// They are the host's bookkeeping, not workspace content: counting them would call
/// every command a workspace change.
fn session_journals(session: Option<&Path>) -> Vec<PathBuf> {
    session
        .map(|session| vec![session.to_path_buf()])
        .unwrap_or_default()
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

/// Assemble one child: load its environment, give it EXACTLY the tool modules the
/// parent granted plus `finish`, resolve its route binding and assemble it under the
/// host's cache-key policy at the child's own ordinal.
///
/// The START path and a re-grant (`worker_continue` with `add_tools`, ADR-0050 item
/// 6) both go through this, so both build the tool list identically — and a re-grant
/// passes the child's original ordinal, so its provider-side prompt cache survives
/// where the route takes a key.
#[cfg(feature = "delegation")]
#[allow(clippy::too_many_arguments)]
fn assemble_child(
    environment_dirs: &[PathBuf],
    catalog: &Catalog,
    environment_name: &str,
    choice: Option<&crate::models::Choice>,
    grant: &[String],
    workspace: &Path,
    substitutions: &Substitutions,
    ordinal: u64,
) -> Result<Assembled, String> {
    let mut environment =
        load_environment(environment_name, environment_dirs).map_err(|error| error.to_string())?;
    environment.tools = child_tools(&environment, grant)?;
    // A selected profile must be in place before the route binding resolves it to
    // the wire model, exactly as the parent's selection is applied.
    if let Some(choice) = choice {
        crate::models::apply(&mut environment, choice, environment_dirs)?;
    }
    crate::catalog::resolve_environment(&mut environment, environment_dirs)?;
    assemble_with_cache_key(catalog, &environment, workspace, substitutions, ordinal)
}

/// The tool list of a child: the granted modules in the parent's order, each with the
/// environment's own `ToolSpec` when it has one (an entry there only supplies the face
/// the module is presented under) and the module's default face otherwise, then
/// `finish` LAST — every worker gets it, because it is how a worker reports done or
/// blocked. The environment's own `[[tools]]` list neither limits nor extends the
/// grant, so a grant is never silently dropped.
#[cfg(feature = "delegation")]
fn child_tools(environment: &EnvironmentFile, grant: &[String]) -> Result<Vec<ToolSpec>, String> {
    let mut granted = Vec::with_capacity(grant.len() + 1);
    for module in grant {
        // A worker can never start workers: the worker tools are not grantable, but a
        // direct [`ChildSpec`] — or a service call — could still name one. Refuse
        // plainly rather than assemble a delegating child.
        if module.starts_with("worker_") {
            return Err(format!(
                "a worker cannot be granted the worker tool `{module}`"
            ));
        }
        // Nor can it run workflows: a step that orchestrated would escape the run's
        // caps and step budget.
        if module.starts_with("workflow_") {
            return Err(format!(
                "a worker cannot be granted the workflow tool `{module}`"
            ));
        }
        let own = environment
            .tools
            .iter()
            .find(|tool| &tool.module == module)
            .cloned();
        granted.push(own.unwrap_or_else(|| ToolSpec {
            module: module.clone(),
            name: None,
            description: None,
            variant: None,
        }));
    }
    let finish = environment
        .tools
        .iter()
        .find(|tool| tool.module == FINISH_MODULE)
        .cloned()
        .unwrap_or_else(|| ToolSpec {
            module: FINISH_MODULE.to_string(),
            name: None,
            description: None,
            variant: None,
        });
    granted.push(finish);
    Ok(granted)
}

/// What every child build shares: the composition seams `run_with_front_end` owns (the
/// front end, the catalog and service slots that break the factory/service cycle, the
/// counter that keeps ids in step, the per-run cache-key ordinal counter, the
/// completion hub, the session, the parent's §3c bound). The direct worker
/// factory and the workflow step runner both build through
/// [`ChildBuilder::build_child`], so a step worker is assembled exactly as a direct one.
#[cfg(feature = "delegation")]
pub(crate) struct ChildBuilder {
    pub(crate) environment_dirs: Vec<PathBuf>,
    date: String,
    pub(crate) parent_workspace: PathBuf,
    pub(crate) front_end: Arc<dyn FrontEnd>,
    catalog_slot: Arc<OnceLock<Arc<Catalog>>>,
    counter: Arc<AtomicUsize>,
    agent_ordinals: Arc<AtomicUsize>,
    completion_hub: Arc<CompletionHub>,
    session: Option<PathBuf>,
    max_idle_summaries: usize,
    service_slot: Arc<OnceLock<Arc<InProcessWorkers>>>,
}

#[cfg(feature = "delegation")]
impl ChildBuilder {
    /// Every argument is a separate composition seam, so the list is long by nature.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        deps: &HostDeps,
        parent_workspace: &Path,
        front_end: Arc<dyn FrontEnd>,
        catalog_slot: Arc<OnceLock<Arc<Catalog>>>,
        counter: Arc<AtomicUsize>,
        agent_ordinals: Arc<AtomicUsize>,
        completion_hub: Arc<CompletionHub>,
        session: Option<PathBuf>,
        max_idle_summaries: usize,
        service_slot: Arc<OnceLock<Arc<InProcessWorkers>>>,
    ) -> Self {
        Self {
            environment_dirs: deps.environment_dirs.clone(),
            date: deps.date.clone(),
            parent_workspace: parent_workspace.to_path_buf(),
            front_end,
            catalog_slot,
            counter,
            agent_ordinals,
            completion_hub,
            session,
            max_idle_summaries,
            service_slot,
        }
    }

    /// Build the child `Agent` through the SAME load + assemble path the top-level
    /// agent uses. The child gets its own fresh `ToolServices` (inside `assemble`),
    /// `workspace`, the front end's shared authorization policy, its own session
    /// journal, and the front end's labelled sink for `worker_id`.
    ///
    /// `choice` selects a profile/effort on top of the environment (a workflow role's
    /// model); `contract` is the output contract the child's `finish` checks; with
    /// `silent_end` the front end is not told the turn ended (a workflow step, whose
    /// end the workflow observer reports); `turn_end` is a workflow step's cell for the
    /// worker's last turn end, which is how a step tells a route failure from a failure
    /// of its own work (ADR-0054 item 3). Returns the child's `finish` outcome cell
    /// too: a step runner reads the structured result from it.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn build_child(
        &self,
        environment: &str,
        choice: Option<&crate::models::Choice>,
        grant: &[String],
        workspace: &Path,
        worker_id: &str,
        contract: Option<p1_tool_finish::OutputContract>,
        silent_end: bool,
        turn_end: Option<TurnEndCell>,
    ) -> Result<(ChildAgent, p1_tool_finish::FinishOutcome), String> {
        let front_end = &self.front_end;
        let completion_hub = &self.completion_hub;
        let environment_dirs = &self.environment_dirs;
        let max_idle_summaries = self.max_idle_summaries;
        let catalog = self
            .catalog_slot
            .get()
            .ok_or_else(|| "the host catalog is not ready".to_string())?
            .clone();
        let workspace = workspace.to_path_buf();
        let substitutions = Substitutions {
            workspace: workspace.display().to_string(),
            date: self.date.clone(),
            os: std::env::consts::OS.to_string(),
        };
        // This child's own cache-key ordinal, kept for its whole life: a re-grant
        // assembles at the SAME ordinal, never a new one.
        let ordinal = next_agent_ordinal(&self.agent_ordinals);
        let mut assembled = assemble_child(
            environment_dirs,
            &catalog,
            environment,
            choice,
            grant,
            &workspace,
            &substitutions,
            ordinal,
        )?;
        // The session file is numbered like the id the service hands out.
        let id: usize = worker_id
            .strip_prefix('w')
            .and_then(|number| number.parse().ok())
            .ok_or_else(|| format!("`{worker_id}` is not a worker id"))?;
        let worker_id = worker_id.to_string();
        // The child gets its OWN activity log and outcome, issued by the shared
        // catalog for this assembly. The worker service does not read the outcome:
        // a child's turn end is its completion, the parent verifies.
        let child_completion = completion_hub.take();
        let outcome = child_completion
            .as_ref()
            .map(|completion| completion.outcome.clone())
            .unwrap_or_default();
        // ADR-0052 item 1: the policy follows the assembled tools' identities, so it
        // is applied here, after assembly, to the `finish` tool the catalog built.
        if let Some(completion) = &child_completion {
            apply_completion_policy(&mut assembled, completion, contract.clone());
        }
        let context = agent_context(&assembled)?;
        let route = assembled.resolved.route.origin.route.clone();
        let model = assembled.resolved.route.origin.model.clone();
        let description = format!("{route}/{model}");

        // The front end builds the labelled child sink; under delegation it also
        // feeds the run's worker-usage aggregate.
        let renderer = front_end.child_event_sink(&worker_id, &route, &model);
        // Every child gets its OWN activity log, whether or not its environment
        // assembles `finish`: the child's §3c guard reads that log for its
        // replacements and its progress, exactly as the parent's guard reads the
        // parent's.
        let log = match &child_completion {
            Some(completion) => completion.log.clone(),
            None => Arc::new(ActivityLog::default()),
        };
        // ADR-0055: the child's commands are measured in ITS workspace, with the
        // host's own session journals (the parent's and every worker's, which sit
        // beside it) excluded.
        log.watch_workspace(&workspace, &session_journals(self.session.as_deref()));
        // The typed handle is kept too: a re-grant re-points the tee at the new tool
        // set, so the effect of a re-granted tool is read from that tool.
        let tee = Arc::new(ActivityTee::new(renderer, log.clone(), &assembled.tools));
        let events: Arc<dyn EventSink> = tee.clone();
        let events: Arc<dyn EventSink> = if max_idle_summaries > 0 {
            Arc::new(ChildStallWatcher {
                inner: events,
                log: log.clone(),
                max: max_idle_summaries,
                message: stall_message(max_idle_summaries),
                service: self.service_slot.clone(),
                worker_id: worker_id.clone(),
            })
        } else {
            events
        };
        // The worker's report (ADR-0050 item 6): the tap is the OUTERMOST sink, so it
        // sees the whole turn — the child's own rendering and the stall guard have
        // had their say before the operator is told the worker's end. The service
        // reads the same cell through `ChildAgent::report`.
        let report = Arc::new(Mutex::new(WorkerReport::new(
            assembled
                .tools
                .iter()
                .map(|tool| tool.declaration().name.clone())
                .collect(),
        )));
        // The typed handle is kept too: a re-grant re-points the tap at the new tool
        // set (below).
        let tap = Arc::new(WorkerReportTap::new(
            events,
            report.clone(),
            &assembled.tools,
            outcome.clone(),
            front_end.clone(),
            worker_id.clone(),
            description.clone(),
            silent_end,
        ));
        let events: Arc<dyn EventSink> = tap.clone();
        // A workflow step keeps its worker's last turn end (ADR-0054 item 3): a turn
        // that ended on a provider failure is a ROUTE failure, which the step runner
        // must tell from a failure of the worker's own work. A direct worker passes no
        // cell and the event goes straight on.
        let events: Arc<dyn EventSink> = match turn_end {
            Some(cell) => Arc::new(TurnEndTap {
                inner: events,
                cell,
            }),
            None => events,
        };
        // Re-assembly for a repair (ADR-0050 item 6): `worker_continue` with
        // `add_tools` hands over the child's FULL new grant, and this rebuilds exactly
        // what the start built — the same environment, the same assembly path, the
        // same cache-key ordinal — with that grant. The service applies it through
        // `Agent::reconfigure` BEFORE the new turn, so the worker keeps its context.
        let regrant: Regrant = {
            let catalog = catalog.clone();
            let environment_dirs = environment_dirs.clone();
            let environment_name = environment.to_string();
            let choice = choice.cloned();
            let contract = contract.clone();
            let workspace = workspace.clone();
            let substitutions = substitutions.clone();
            let completion_hub = completion_hub.clone();
            let tap = tap.clone();
            let tee = tee.clone();
            let log = log.clone();
            let outcome = outcome.clone();
            // The worker's OWN `finish` tool survives every re-grant: its activity
            // log is the worker's whole history, which `finish` reads to verify a
            // claim, and a freshly assembled one would see an empty session.
            let finish = finish_tool(&assembled);
            Arc::new(move |grant: &[String]| -> Result<Reconfiguration, String> {
                let assembled = assemble_child(
                    &environment_dirs,
                    &catalog,
                    &environment_name,
                    choice.as_ref(),
                    grant,
                    &workspace,
                    &substitutions,
                    ordinal,
                )?;
                // The catalog's `finish` factory issued THIS assembly its own
                // completion: take it, so the hub cannot hand a stale one to a later
                // worker assembly.
                let _issued = completion_hub.take();
                let context = agent_context(&assembled)?;
                let finish_at = finish_index(&assembled);
                // A re-grant is a new tool set, so the policy is chosen again from it
                // (ADR-0052 item 1): `add_tools: ["shell"]` puts the worker back on the
                // strict rule for every later turn.
                let policy = completion_policy(&assembled.tools);
                let mut tools = assembled.tools;
                if let (Some(finish), Some(index)) = (&finish, finish_at) {
                    tools[index] = finish_under_policy(
                        finish,
                        log.clone(),
                        outcome.clone(),
                        policy,
                        contract.clone(),
                    );
                }
                // The report's `tools` becomes the new assembly's names, its `finish`
                // tool is found again by identity, and the child's activity records
                // the effect of a re-granted tool from that tool itself.
                tap.retool(&tools);
                tee.retool(&tools);
                Ok(Reconfiguration {
                    provider: assembled.provider,
                    tools,
                    system_prompt: assembled.system_prompt,
                    options: assembled.options,
                    context,
                })
            })
        };
        // With `--session`, worker `w{n}` gets its OWN new JSONL file next to the
        // parent's (`FILE.w{n}.jsonl`). Without one it stays in memory like before.
        // Created last among the fallible steps so a later failure cannot leave a
        // stray file behind — and if `Agent::new` still fails, remove what we made.
        let session = &self.session;
        let created_file = session
            .as_ref()
            .map(|session| crate::session::worker_path(session, id));
        let journal: Arc<dyn CommitSink> = match session {
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
            authorization: front_end.authorization(),
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
        // Both ways of starting a worker advance the counter on success, so the
        // direct factory's predicted id stays the service's next id.
        self.counter.fetch_add(1, Ordering::SeqCst);
        front_end.child_started(&worker_id);
        // The service snapshots this when a child's turn ends; it reads the SAME cell
        // the tap just filled and the front end was told about.
        let report: Arc<dyn Fn() -> WorkerReport + Send + Sync> =
            Arc::new(move || report.lock().unwrap().clone());
        Ok((
            ChildAgent {
                agent,
                description,
                report,
                regrant: Some(regrant),
            },
            outcome,
        ))
    }
}

/// The direct worker factory (`worker_start`): builds through
/// [`ChildBuilder::build_child`] with the spec's environment and grant, no contract.
#[cfg(feature = "delegation")]
fn make_child_factory(builder: Arc<ChildBuilder>) -> AgentFactory {
    Arc::new(move |spec: &ChildSpec| -> Result<ChildAgent, String> {
        let workspace = spec
            .workspace
            .clone()
            .unwrap_or_else(|| builder.parent_workspace.clone());
        // `InProcessWorkers` assigns `w{n}` after a SUCCESSFUL factory call and
        // factory calls are serialised, so this is the id the service will hand
        // out. The counter is only advanced after a successful build: a start that
        // fails (bad environment, an existing worker session file, a failed build)
        // must not desynchronise it from the service's own numbering.
        let worker_id = format!("w{}", builder.counter.load(Ordering::SeqCst) + 1);
        builder
            .build_child(
                &spec.environment,
                None,
                &spec.tools,
                &workspace,
                &worker_id,
                None,
                false,
                None,
            )
            .map(|(child, _outcome)| child)
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
/// `agent_ordinal` is the agent's position in the cache-key scheme: 0 for the
/// parent, the worker's own ordinal otherwise.
fn assemble_with_cache_key(
    catalog: &Catalog,
    environment: &p1_assembly::EnvironmentFile,
    workspace: &std::path::Path,
    substitutions: &Substitutions,
    agent_ordinal: u64,
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
                options.cache_key = Some(generated_cache_key(workspace, &name, agent_ordinal));
            }
            options
        },
    )
    .map_err(|error| error.to_string())
}

/// A STABLE provider-side prompt-cache key for one agent: a pure function of the
/// workspace, the environment name and the agent's ordinal — no process id and
/// no clock. Stability is the point: a resume and a re-run in the same workspace
/// keep their provider-side cache routing, and the journalled environment no
/// longer changes on resume. Ordinal 0 is the parent agent ([`PARENT_ORDINAL`]);
/// workers get 1, 2, … in start order ([`next_agent_ordinal`]). Without a key the
/// Codex route served 0 cached tokens across a whole task (measured 2026-09-20);
/// routes without such a key never see it.
fn generated_cache_key(
    workspace: &std::path::Path,
    environment: &str,
    agent_ordinal: u64,
) -> String {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    workspace.hash(&mut hasher);
    environment.hash(&mut hasher);
    agent_ordinal.hash(&mut hasher);
    format!("p1-{:016x}", hasher.finish())
}

/// The parent agent's ordinal. It is passed literally at the parent's assembly
/// call site, never taken from the shared counter, so no worker that assembled
/// earlier can shift it off 0.
const PARENT_ORDINAL: u64 = 0;

/// The next worker ordinal: 1, 2, … in start order, so each worker gets its own
/// key while every worker of a given start order keeps it across processes.
#[cfg(feature = "delegation")]
fn next_agent_ordinal(ordinals: &AtomicUsize) -> u64 {
    ordinals.fetch_add(1, Ordering::Relaxed) as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The key is a pure function of its three inputs: same inputs, same key —
    /// in another process too, because nothing process-local (a pid, a clock, a
    /// counter) enters it. Each input on its own still changes the key.
    #[test]
    fn the_generated_cache_key_is_pure_and_depends_on_every_input() {
        let workspace = Path::new("/tmp/one-workspace");
        let key = generated_cache_key(workspace, "plain", PARENT_ORDINAL);
        assert_eq!(key, generated_cache_key(workspace, "plain", PARENT_ORDINAL));
        assert!(key.starts_with("p1-"), "{key}");
        assert_eq!(key.len(), "p1-".len() + 16, "{key}");
        assert_ne!(
            key,
            generated_cache_key(Path::new("/tmp/other"), "plain", 0)
        );
        assert_ne!(key, generated_cache_key(workspace, "other", 0));
        assert_ne!(key, generated_cache_key(workspace, "plain", 1));
    }

    /// The parent's ordinal is the constant 0 — not a draw from the counter — and
    /// the counter never hands 0 out, so a worker cannot collide with its parent.
    #[test]
    fn the_parent_ordinal_is_zero_and_worker_ordinals_start_at_one() {
        assert_eq!(PARENT_ORDINAL, 0);
        let ordinals = AtomicUsize::new(1);
        let first = next_agent_ordinal(&ordinals);
        let second = next_agent_ordinal(&ordinals);
        assert_eq!(first, 1, "a worker never gets the parent's ordinal");
        assert_eq!(second, first + 1);
        assert_ne!(
            generated_cache_key(Path::new("/tmp/ws"), "plain", PARENT_ORDINAL),
            generated_cache_key(Path::new("/tmp/ws"), "plain", first)
        );
    }
}
