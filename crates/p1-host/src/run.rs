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

use p1_assembly::Catalog;
use p1_assembly::{Assembled, Substitutions, assemble, load_environment};
use p1_contracts::{
    AgentEvent, AuthorizationPolicy, BoxFuture, CacheKeySupport, CancellationToken, CommitSink,
    Compaction, ContextError, ContextInput, ContextPolicy, Effort, EventSink, JournalRecord,
    ModelOptions, Prepared, Provider, ProviderErrorKind, Tool, TurnEnd,
};
use p1_core::{Agent, AgentParts, Reconfiguration, ReconfigureError, ResumeReport};
use p1_model_profile::ModelProfile;
use p1_redact::{MaskCounter, redacted};

use crate::activity::{ActivityLog, ActivityTee, Completion, CompletionHub};
use crate::catalog::build_catalog;
#[cfg(feature = "delegation")]
use crate::catalog::children::{announce_lost_workers, compose_children, running_children};
use crate::catalog::delegation::with_worker_tools;
use crate::cli::{self, Command, Options};
use crate::frontend::{FrontEnd, LineFrontEnd};
use crate::render::Renderer;
use crate::session;
use crate::{HostDeps, InterruptSource};
use p1_tool_finish::Accepted;

/// Observe the record only after the underlying journal has accepted it. The
/// wrapper also covers TUI prompts, which bypass the line-mode turn driver.
#[cfg(feature = "shadow-hook")]
pub(crate) struct ShadowJournal {
    pub(crate) inner: Arc<dyn CommitSink>,
    pub(crate) hook: Arc<p1_hook_shadow::ShadowHook>,
    pub(crate) workspace: PathBuf,
    pub(crate) journal: PathBuf,
    pub(crate) cache_key: Mutex<Option<String>>,
    pub(crate) origin: ShadowOrigin,
}

#[cfg(feature = "shadow-hook")]
pub(crate) enum ShadowOrigin {
    Parent,
    Child { family: String, provider: String },
}

#[cfg(feature = "shadow-hook")]
impl CommitSink for ShadowJournal {
    fn commit<'a>(
        &'a self,
        record: &'a JournalRecord,
    ) -> BoxFuture<'a, Result<(), p1_contracts::CommitError>> {
        Box::pin(async move {
            self.inner.commit(record).await?;
            if let p1_contracts::RecordBody::Environment { options, .. } = &record.body
                && let Ok(mut key) = self.cache_key.lock()
            {
                *key = options.cache_key.clone();
            }
            if let p1_contracts::RecordBody::UserInput { text } = &record.body {
                let origin = match &self.origin {
                    ShadowOrigin::Parent
                        if text == CONTINUATION_MESSAGE || text == PROVIDER_RETRY_MESSAGE =>
                    {
                        return Ok(());
                    }
                    ShadowOrigin::Parent => p1_hook_shadow::Origin::UserInput,
                    // Only the initial brief is a dispatch; subsequent user_input
                    // records in a child are repair/continuation messages.
                    ShadowOrigin::Child { .. } if record.seq != 1 => return Ok(()),
                    ShadowOrigin::Child { family, provider } => p1_hook_shadow::Origin::Dispatch {
                        family: family.clone(),
                        provider: provider.clone(),
                    },
                };
                self.hook.observe(p1_hook_shadow::ShadowEvent {
                    text: text.clone(),
                    workspace: Some(self.workspace.clone()),
                    journal: self.journal.clone(),
                    cache_key: self.cache_key.lock().ok().and_then(|key| key.clone()),
                    origin,
                    source_ref: match self.origin {
                        ShadowOrigin::Child { .. } if self.journal.is_file() => {
                            Some(format!("{}:{}", self.journal.display(), record.seq))
                        }
                        _ => None,
                    },
                });
            }
            Ok(())
        })
    }
}

#[cfg(feature = "delegation")]
use p1_workers::InProcessWorkers;

// The child assembly moved to `catalog/children.rs`; the workflow step runner still
// names these two at their old path.
#[cfg(feature = "workflows")]
pub(crate) use crate::catalog::children::{ChildBuilder, TurnEndCell};

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
/// carries the plain settings and the prompt override. `profile` is the model
/// profile the environment selected (the whole-provider form has none): its own
/// capacity narrows the environment's table, and its effort floor is what the
/// summarization request runs at (#125).
pub(crate) fn agent_context(
    assembled: &Assembled,
    profile: Option<&ModelProfile>,
) -> Result<Arc<dyn ContextPolicy>, String> {
    let Some(settings) = &assembled.resolved.context else {
        return Ok(Arc::new(DefaultContext));
    };
    let (config, summary_output_tokens) = effective_context(settings, profile)?;
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
    .with_summary_output_tokens(summary_output_tokens)?
    .with_summary_effort(summary_effort(profile));
    Ok(Arc::new(policy))
}

/// The smallest summary-output cap worth sending: a cap below this cannot produce a summary
/// that says anything, so a table that leaves no such cap is a configuration error rather
/// than an agent that fails on its first long turn.
const MIN_SUMMARY_OUTPUT_TOKENS: u64 = 1_000;

/// The effective table PLUS the summary-output cap that belongs to it (#125 review round 3):
/// the cap is a budget of the SAME window as the table, so it is clamped against the effective
/// wall and not the environment's — a profile that narrows the window must narrow the cap with
/// it, or a valid narrow profile makes the agent unstartable. Half the wall is the ceiling: the
/// summarization request carries the rendered transcript as well as its own answer, so a cap
/// that claimed more than half of what can be sent would leave the transcript no room. A table
/// that cannot host even [`MIN_SUMMARY_OUTPUT_TOKENS`] fails here, naming the profile and the
/// window it serves, instead of failing later with a number no operator wrote.
fn effective_context(
    settings: &p1_assembly::ContextSettings,
    profile: Option<&ModelProfile>,
) -> Result<(p1_context::ContextConfig, u64), String> {
    let config = config_for_route(settings, profile);
    let wall = config.window_tokens - config.output_headroom_tokens;
    let cap = settings.summary_output_tokens.min(wall / 2);
    if cap < MIN_SUMMARY_OUTPUT_TOKENS {
        let who = match profile {
            Some(profile) => format!("profile `{}`", profile.id),
            None => "this environment".to_string(),
        };
        return Err(format!(
            "the declared summary-output cap ({}) does not fit the context table of {who}: its window \
             is {} tokens with {} reserved, leaving {wall} to send, and a summary request needs at \
             least {MIN_SUMMARY_OUTPUT_TOKENS} of them for its own answer",
            settings.summary_output_tokens, config.window_tokens, config.output_headroom_tokens
        ));
    }
    Ok((config, cap))
}

/// The `ContextConfig` one assembled agent actually gets, with the selected profile's own
/// capacity folded in (#125 review): an environment states the window of its ROUTE, but a
/// profile states what THIS model serves, and p1 lets a narrower profile be selected on the
/// same environment. A profile that names a smaller window or a smaller output ceiling lowers
/// both the effective window and the reserve, and the useful point is pulled into the result —
/// so a task that selects MiMo (200k) on `zen` compacts at MiMo's size instead of failing a
/// request against Space Bunny's 1M window.
pub(crate) fn config_for_route(
    settings: &p1_assembly::ContextSettings,
    profile: Option<&ModelProfile>,
) -> p1_context::ContextConfig {
    let window = profile
        .and_then(|profile| profile.context_tokens)
        .map_or(settings.window_tokens, |model_window| {
            settings.window_tokens.min(model_window)
        });
    // The reserve is the next response; a profile's output ceiling bounds it, and it stays
    // strictly below the effective window, because a reserve as large as the window would leave
    // no room at all for the request that carries the next response.
    let headroom = profile
        .and_then(|profile| profile.max_output_tokens)
        .map_or(settings.output_headroom_tokens, |ceiling| {
            settings.output_headroom_tokens.min(u64::from(ceiling))
        })
        .min(window.saturating_sub(1));
    let wall = window.saturating_sub(headroom);
    // The useful point: never later than the environment asked and always below the wall the
    // request has to fit under. Only a profile that NARROWS the window also caps it at 60% of
    // the narrower window; an environment's own threshold is a decision about its route (GPT's
    // 220,000 of 272,000, docs/design/context-windows.md) and a profile that does not narrow
    // the window leaves it alone.
    let asked = if window < settings.window_tokens {
        settings
            .summarize_at_tokens
            .min(window.saturating_mul(60) / 100)
    } else {
        settings.summarize_at_tokens
    };
    let useful = asked.min(wall.saturating_sub(1));
    // The verbatim budgets are copied from the environment, but they are budgets of THIS window:
    // a kept tail larger than what a request can carry would keep the whole history verbatim and
    // leave the next request over the wall, so both are clamped below it (the verbatim budget is
    // a share of the kept tail, so it can never exceed it).
    let keep_recent = settings.keep_recent_tokens.min(wall.saturating_sub(1));
    let user_verbatim = settings.user_verbatim_tokens.min(keep_recent);
    p1_context::ContextConfig {
        window_tokens: window,
        output_headroom_tokens: headroom,
        summarize_at_tokens: useful,
        keep_recent_tokens: keep_recent,
        user_verbatim_tokens: user_verbatim,
        tool_result_excerpt_chars: settings.tool_result_excerpt_chars,
    }
}

/// The reasoning effort the summarization request runs at (#125): the LOWEST level the model
/// profile supports, so the summary-output cap buys summary text and not the agent's own
/// reasoning. Without a profile there is no list to read a floor from, and the agent's effort
/// must still not leak into the summary: the whole-provider form summarizes at `Low`, the
/// lowest level every route's effort scale starts at.
fn summary_effort(profile: Option<&ModelProfile>) -> Option<Effort> {
    match profile {
        Some(profile) => profile.efforts.iter().copied().min(),
        None => Some(Effort::Low),
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
        // The model list (ADR-0049 stage 1): every environment × the profiles its
        // route binds. No catalog and no network — the credential column is the same
        // non-secret probe `env show` prints.
        Command::Models { search } => models_command(deps, &options, search.as_deref()),
        // The installed module set (ADR-0079, freeze item 6). No catalog, no provider and
        // no network; `verify` reads the module set and nothing else at all.
        Command::Modules(modules) => crate::modules_cli::modules(deps, &modules),
        // The login surface (ADR-0044, spec §6): no catalog, no provider and no
        // network — the store is written and the "which source" report is printed.
        // The route quota ledger (ADR-0052): route metadata and p1-auth credential
        // references go to `p1-usage`; no catalog, no provider.
        Command::Usage(usage) => crate::usage::usage(deps, &usage).await,
        Command::Login { route } => crate::login::login(deps, &route).await,
        Command::LoginFromClaudeCode { route, dir } => {
            crate::login::from_claude_code(deps, &route, dir.as_deref()).await
        }
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
                compact: options.compact,
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
    // Standing instructions and the skill index belong to the top-level agent only
    // (issue #129): a child's brief carries what it needs.
    let instructions = crate::instructions::prompt_section(&options.instructions, &options.skills)
        .map_err(RunError::usage)?;
    // The §3c stall guard is host policy and applies only to unattended runs; the
    // front end decides what "headless" means (the line front end uses the CLI
    // rule, a terminal UI is interactive by definition).
    let headless = front_end.is_headless(options);

    // The delegation service must exist before the catalog so the `worker_*`
    // tools can be registered; the child factory reaches the catalog lazily,
    // breaking the cycle (children never assemble delegation tools).
    // The child service and the direct factory share one initial reservation. This
    // must happen before workflows are composed, since a step worker is built by the
    // same service and receives the same id namespace.
    #[cfg(feature = "delegation")]
    let (completion_hub, generations, child_builder, service, child_counter) =
        compose_children(deps, &workspace, front_end.clone(), options, 2)?;
    #[cfg(feature = "delegation")]
    let service = Some(service);
    #[cfg(not(feature = "delegation"))]
    let completion_hub = Arc::new(CompletionHub::new());
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
        // Generation 0: the start's catalog and policy. The child builder already
        // shares `generations`, so a child started from now on pins this one until
        // a reload replaces it (ADR-0084 §3).
        generations.install(catalog.clone(), front_end.authorization());
    }
    // Without delegation nothing else reads the generations; the session still has
    // generation 0 so a `/modules reload` has a current one to replace.
    #[cfg(not(feature = "delegation"))]
    let generations = Arc::new(Generations::new(catalog.clone(), front_end.authorization()));

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
    // Issue #142: one counter per agent, shared by the tools assembled below and by
    // the notice sink the turn boundary reports through.
    let mask = Arc::new(MaskCounter::new());
    let assembled = assemble_with_cache_key(
        &catalog,
        &environment,
        &workspace,
        &substitutions,
        PARENT_ORDINAL,
        &mask,
    )?;
    // The `finish` factory issued this agent's completion state during `assemble`.
    // `None` when the environment does not assemble `finish`.
    let completion = completion_hub.take();
    let context = agent_context(&assembled, environment.profile.as_deref())?;
    let route = assembled.resolved.route.origin.route.clone();
    let model = assembled.resolved.route.origin.model.clone();
    // The session's model, for a later switch (ADR-0049 stage 3): the environment it
    // runs, the profile it selected and the `finish` tool it keeps.
    let session_environment = assembled.resolved.environment.clone();
    let session_finish = finish_tool(&assembled);
    let session_effort = environment.options.reasoning_effort;

    // Announce the assembled parent before the agent is built: the front end
    // builds its parent renderer from this.
    front_end.parent_assembled(&route, &model, completion.clone());
    // ADR-0057: the same announcement carries the assembled tools, so a front end
    // can describe a call from the tool that owns it instead of matching a name.
    front_end.parent_tools(&assembled.tools);
    // §10 `ctx`'s denominator: unknown (no `[context]` section) stays `None`,
    // never a guessed window. The numbers are the EFFECTIVE ones — the selected
    // profile's own capacity folded in — so the display shows what the
    // summarizer acts on, not a window this model cannot serve.
    let context_numbers = assembled
        .resolved
        .context
        .as_ref()
        .map(|settings| config_for_route(settings, environment.profile.as_deref()));
    front_end.context_configured(
        context_numbers.as_ref().map(|config| config.window_tokens),
        context_numbers
            .as_ref()
            .map(|config| config.summarize_at_tokens),
    );

    let (journal, records): OpenedSession = open_session(deps, options)?;
    #[cfg(feature = "shadow-hook")]
    let journal: Arc<dyn CommitSink> = match &deps.shadow {
        Some(hook) => Arc::new(ShadowJournal {
            inner: journal,
            hook: hook.clone(),
            workspace: workspace.clone(),
            journal: options
                .session
                .clone()
                .unwrap_or_else(|| workspace.join("p1-memory")),
            cache_key: Mutex::new(assembled.options.cache_key.clone()),
            origin: ShadowOrigin::Parent,
        }),
        None => journal,
    };
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
    } else if options.tui && options.max_idle_summaries > 0 {
        Arc::new(InteractiveStallWatcher {
            inner: events,
            activity: activity.clone(),
            max: options.max_idle_summaries,
            warned: AtomicBool::new(false),
        })
    } else {
        events
    };
    // Issue #142: report the count of masked credential-shaped values once per turn,
    // through the same display-only notice path a provider notice uses; never a value.
    let events: Arc<dyn EventSink> = Arc::new(MaskNoticeSink::new(events, mask.clone()));

    let parts = AgentParts {
        provider: assembled.provider,
        tools: assembled.tools,
        system_prompt: assembled.system_prompt + instructions.as_str(),
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
                announce_lost_workers(
                    deps,
                    &agent,
                    service,
                    &child_counter,
                    options.session.as_deref(),
                    &records,
                )?;
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
    // ADR-0076: `--compact` (only with `--resume`, `cli::parse` refuses it
    // otherwise) summarizes the resumed history once, before the first turn and
    // its first provider request. The TUI queues it as a `/compact` instead, so
    // its line lands in the transcript; every other front end does it here.
    if options.compact && !options.tui {
        let result = agent.compact_now(&cancel).await;
        write_stderr(deps, &format!("{}\n", compaction_line(&result)));
        // The run was asked to start compacted; running on the old history
        // would hide that it did not.
        if let Err(error) = result {
            return Err(format!("--compact: {error}").into());
        }
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
    // ADR-0084 §3: the start's catalog and policy are generation 0; a
    // `/modules reload` rebuilds both from a copy of the catalog's dependencies.
    let reload_deps = catalog_deps(deps);
    let reload_policy = front_end.clone();
    deps.model_switch = Some(Arc::new(ModelSwitch {
        generations: generations.clone(),
        reload: ReloadInputs {
            deps: reload_deps,
            sandbox: options.sandbox,
            sandbox_write: options.sandbox_write.clone(),
            sandbox_read: options.sandbox_read.clone(),
            env_pass: options.env_pass.clone(),
            policy: Box::new(move || reload_policy.authorization()),
            queue: ReloadQueue::default(),
        },
        completion: completion_hub.clone(),
        activity: activity.clone(),
        environment_dirs: deps.environment_dirs.clone(),
        workspace: workspace.clone(),
        substitutions: substitutions.clone(),
        ignored: session_journals(options.session.as_deref()),
        scope: options.models.clone(),
        route_label: front_end.route_label(),
        instructions,
        mask: mask.clone(),
        session: Mutex::new(SessionModel {
            environment: session_environment,
            profile: choice.profile.clone(),
            effort: session_effort,
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

    // B-S6-9, D068: the main agent's assembly is dropped here, at teardown, so its
    // generation of worker-member scopes is retired: every child id those scopes held
    // becomes `unknown-child` through them. Retiring forgets ids only, so a running child
    // still completes and still notifies. A model switch or a re-grant keeps the
    // generation; this is the one place it ends.
    #[cfg(feature = "delegation")]
    if let Some(scopes) = &deps.member_scopes {
        scopes
            .registry()
            .retire_generation(scopes.generation())
            .await;
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

    // The same child composition as an interactive run, including the sibling
    // reservation. A standalone workflow can be invoked repeatedly with one session.
    let (completion_hub, generations, child_builder, service, _child_counter) = compose_children(
        deps,
        &workspace,
        front_end.clone(),
        options,
        workflow.max_workers,
    )?;
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
    // Generation 0 of this standalone run: the child builder shares it, so every
    // step worker pins the catalog the run loaded (ADR-0084 §3).
    generations.install(catalog, front_end.authorization());

    // The run's base commit (ADR-0073): what its steps' new worktrees branch from.
    let base = crate::worktree::run_base_async(workspace.clone()).await;
    let request = p1_workflow::StartRequest {
        script,
        args,
        resume_from: workflow.resume_from.clone().map(p1_workflow::RunId),
        role_models: workflow.roles.iter().cloned().collect(),
        workspace: Some(workspace),
        base,
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
                    switch_model(switch, agent, SwitchRequest::Model(reference)).await,
                );
            }
            continue;
        }
        if let Some(switch) = &deps.model_switch
            && let Some(level) = argument(text, "/effort")
        {
            report_model(
                deps,
                switch_model(switch, agent, SwitchRequest::Effort(level)).await,
            );
            continue;
        }
        // ADR-0084 §3: this loop reads a line only between complete turns, so a
        // `/modules reload` is never pending here; it applies at once.
        if let Some(switch) = &deps.model_switch
            && argument(text, "/modules").and_then(p1_tui::input::modules_command)
                == Some(p1_tui::input::ModulesCommand::Reload)
        {
            report_reload(deps, reload_modules(switch, agent).await);
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
pub(crate) const FINISH_MODULE: &str = "finish";

/// Where the `finish` tool sits in an assembly: `resolved.tools` and `tools` are
/// built from the same environment list, in order.
pub(crate) fn finish_index(assembled: &Assembled) -> Option<usize> {
    assembled
        .resolved
        .tools
        .iter()
        .position(|tool| tool.module == FINISH_MODULE)
}

/// The session's `finish` tool in an assembly, when the environment declares one.
pub(crate) fn finish_tool(assembled: &Assembled) -> Option<Arc<dyn Tool>> {
    finish_index(assembled).map(|index| assembled.tools[index].clone())
}

/// The session's model (ADR-0049 stage 3): what a `/model` or `/effort` line
/// changes, plus the `finish` tool the session keeps.
struct SessionModel {
    /// The environment the session runs now: §1 rule 2's "current environment".
    environment: String,
    /// The profile the session selected (`None` keeps the environment's own).
    profile: Option<String>,
    /// The effort the session runs at, so a module reload assembles the same model.
    effort: Option<Effort>,
    /// The `finish` tool the session keeps, when its environment assembles one.
    finish: Option<Arc<dyn Tool>>,
}

/// Everything a model switch needs of the host (ADR-0049 stage 3, spec §4). `run`
/// builds it once the catalog and the parent's activity plumbing exist and stores it
/// on [`HostDeps`], so the line mode switches now and the TUI's run loop can call
/// [`switch_model`] with it.
pub(crate) struct ModelSwitch {
    /// The session's assembly generations (ADR-0084 §3): a switch assembles on the
    /// current generation's catalog exactly as the start path did, and a
    /// `/modules reload` installs the next one. The child builder shares THIS cell,
    /// so a child or workflow step pinning it pins what the reload replaced.
    generations: Arc<Generations>,
    /// What a `/modules reload` builds its candidate from.
    reload: ReloadInputs,
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
    /// The top-level agent's standing instructions and skill index (issue #129),
    /// re-appended to every switched assembly.
    instructions: String,
    /// Issue #142: the top-level agent's mask counter. A switched assembly's tools
    /// feed the SAME counter the parent's notice sink reads.
    mask: Arc<MaskCounter>,
    session: Mutex<SessionModel>,
}

impl ModelSwitch {
    /// The `--models` value this run was given, if any.
    fn scope_flag(&self) -> Option<&str> {
        self.scope.as_deref()
    }

    /// The session's `/modules reload` queue: a front end asks it whether a request
    /// waits for the next boundary.
    pub(crate) fn reload_queue(&self) -> &ReloadQueue {
        &self.reload.queue
    }

    /// A snapshot of the session's model rather than the guard: a std guard must not
    /// live across the commit's await, and `&mut Agent` already serializes changes.
    fn session_snapshot(&self) -> SessionSnapshot {
        let session = self.session.lock().unwrap();
        SessionSnapshot {
            environment: session.environment.clone(),
            profile: session.profile.clone(),
            effort: session.effort,
            finish: session.finish.clone(),
        }
    }
}

/// A WORKING [`ModelSwitch`] for the driver's tests (S5.7, issue #331): the session
/// runs `environment` from `deps.environment_dirs`, generation 0 is a catalog built
/// exactly as a run builds one, and the front end's own policy answers. The TUI's
/// `request_reload` and `apply_reload` are driven through it, so the pending note and
/// the boundary application are tested through the front end the way a run wires them.
#[cfg(all(test, feature = "delegation"))]
pub(crate) fn model_switch_for_test(
    deps: &mut HostDeps,
    front_end: Arc<dyn FrontEnd>,
    environment: &str,
    workspace: PathBuf,
) -> Result<ModelSwitch, String> {
    // The session's MAIN environment carries the `worker_*` tools (and, with
    // workflows, `workflow_*`); a run's service registers them, so a stub stands in.
    let stub: p1_workers::AgentFactory = Arc::new(|_spec| Err("no workers in this test".into()));
    deps.worker_service = Some(p1_workers::InProcessWorkers::new(stub, 1));
    // The workflow tools are only registered by a run's `workflow_service`. A driver
    // test has none, so stand-ins go in through the same catalog hook `reload_modules`
    // builds ITS catalog with — both the session's and the reload's catalogs must
    // have them, or the main environment will not assemble.
    #[cfg(feature = "workflows")]
    {
        let inner = deps.catalog_hook.take();
        deps.catalog_hook = Some(Box::new(move |catalog: &mut Catalog| {
            for key in crate::catalog::workflow::WORKFLOW_MODULES {
                if !catalog
                    .tool_keys()
                    .iter()
                    .any(|registered| registered == key)
                {
                    catalog.tool(
                        key,
                        Box::new(
                            |spec: &p1_assembly::ToolSpec, _: &p1_assembly::ToolServices| {
                                Ok(Arc::new(p1_testkit::FakeTool::new(&spec.module))
                                    as Arc<dyn Tool>)
                            },
                        ),
                    );
                }
            }
            if let Some(inner) = &inner {
                inner(catalog);
            }
        }));
    }
    let reload_deps = catalog_deps(deps);
    let completion = Arc::new(CompletionHub::new());
    let catalog = build_catalog(
        &reload_deps,
        cli::SandboxMode::Off,
        &[],
        &[],
        &[],
        &completion,
    )?;
    let catalog = Arc::new(catalog);
    let generations = Arc::new(Generations::new(catalog, front_end.authorization()));
    // A stand-in sink: a driver test's session environment has no `finish`, so the
    // activity plumbing is never re-pointed, and the front end's renderer is only
    // built once a turn announces itself.
    let activity = Arc::new(ParentActivity::new(
        Arc::new(p1_testkit::RecordingEvents::new()),
        Arc::new(ActivityLog::default()),
        &[],
    ));
    let substitutions = substitutions(&reload_deps, &workspace);
    Ok(ModelSwitch {
        generations,
        reload: ReloadInputs {
            deps: reload_deps,
            sandbox: cli::SandboxMode::Off,
            sandbox_write: Vec::new(),
            sandbox_read: Vec::new(),
            env_pass: Vec::new(),
            policy: {
                let front_end = front_end.clone();
                Box::new(move || front_end.authorization())
            },
            queue: ReloadQueue::default(),
        },
        completion,
        activity,
        environment_dirs: deps.environment_dirs.clone(),
        workspace,
        substitutions,
        ignored: session_journals(None),
        scope: None,
        route_label: front_end.route_label(),
        instructions: String::new(),
        mask: Arc::new(MaskCounter::new()),
        session: Mutex::new(SessionModel {
            environment: environment.to_string(),
            profile: None,
            effort: None,
            finish: None,
        }),
    })
}

/// [`SessionModel`] read out of its lock.
struct SessionSnapshot {
    environment: String,
    profile: Option<String>,
    effort: Option<Effort>,
    finish: Option<Arc<dyn Tool>>,
}

#[cfg(test)]
impl ModelSwitch {
    /// A real switch over a test's own catalog and scratch environment tree, for the
    /// TUI's idle `/model` case (`tui::tests`): it runs the production
    /// [`switch_model`] + `Agent::reconfigure` path through `drive_loop`.
    ///
    /// ADR-0084 §3 (S5.7) moved the switch's catalog into a generation, so `catalog`
    /// becomes generation 0 and the reload inputs — which a `/model` never reads —
    /// are a minimal stand-in over the same environment tree.
    pub(crate) fn new_for_test(
        catalog: Arc<Catalog>,
        front: Arc<dyn EventSink>,
        environment_dirs: Vec<PathBuf>,
        workspace: PathBuf,
        environment: String,
        profile: Option<String>,
    ) -> Self {
        let substitutions = Substitutions {
            workspace: workspace.display().to_string(),
            date: "2026-01-02".to_string(),
            os: std::env::consts::OS.to_string(),
        };
        // `/model` keeps the agent's policy (`authorization: None`), so the
        // generation only carries one; a permissive stand-in is enough.
        let authorization: Arc<dyn AuthorizationPolicy> =
            Arc::new(p1_testkit::ScriptedAuthorization::permit_all());
        let policy = authorization.clone();
        let writer = || -> crate::SharedWriter { Arc::new(Mutex::new(Box::new(std::io::sink()))) };
        let reload_deps = HostDeps::new(
            writer(),
            writer(),
            Arc::new(crate::StdinLines::new()),
            Arc::new(p1_provider_http::testing::ScriptedTransport::new(Vec::new())),
            "2026-01-02".to_string(),
            Arc::new(crate::SignalInterrupt),
            environment_dirs.clone(),
            false,
        );
        Self {
            generations: Arc::new(Generations::new(catalog, authorization)),
            reload: ReloadInputs {
                deps: reload_deps,
                sandbox: cli::SandboxMode::Off,
                sandbox_write: Vec::new(),
                sandbox_read: Vec::new(),
                env_pass: Vec::new(),
                policy: Box::new(move || policy.clone()),
                queue: ReloadQueue::default(),
            },
            completion: Arc::new(CompletionHub::new()),
            activity: Arc::new(ParentActivity::new(
                front,
                Arc::new(ActivityLog::default()),
                &[],
            )),
            environment_dirs,
            workspace,
            substitutions,
            ignored: Vec::new(),
            scope: None,
            route_label: None,
            instructions: String::new(),
            mask: Arc::new(MaskCounter::new()),
            session: Mutex::new(SessionModel {
                environment,
                profile,
                effort: None,
                finish: None,
            }),
        }
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
pub(crate) async fn switch_model(
    switch: &ModelSwitch,
    agent: &mut Agent,
    request: SwitchRequest<'_>,
) -> Result<String, String> {
    let current = switch.session_snapshot();
    let choice = match request {
        SwitchRequest::Model(reference) => {
            let models = crate::models::enumerate(&switch.environment_dirs)?;
            let resolved = crate::models::resolve(reference, &current.environment, &models)?;
            crate::models::Choice {
                environment: resolved.environment,
                profile: Some(resolved.profile),
                effort: resolved.effort,
            }
        }
        // `/effort LEVEL` keeps the model and replaces only the effort.
        SwitchRequest::Effort(level) => crate::models::Choice {
            environment: current.environment,
            profile: current.profile,
            effort: Some(crate::models::parse_effort(level)?),
        },
    };
    let generation = switch.generations.current();
    let candidate = session_candidate(switch, generation.catalog(), &choice, &current.finish)?;
    // `reconfigure` validates against the CURRENT history and commits the new
    // `Environment` before it installs; on either failure it changes nothing at all,
    // so the session state below is only updated once it is `Ok`.
    agent
        .reconfigure(Reconfiguration {
            provider: candidate.parts.provider.clone(),
            tools: candidate.parts.tools.clone(),
            system_prompt: candidate.parts.system_prompt.clone(),
            options: candidate.parts.options.clone(),
            context: candidate.parts.context.clone(),
            authorization: None,
        })
        .await
        .map_err(|error| error.to_string())?;
    Ok(adopt_candidate(switch, candidate))
}

/// The session's assembly as the start path builds it: its parts, plus what the
/// session state takes over once the agent installed them.
struct SessionCandidate {
    parts: CandidateParts,
    environment: p1_assembly::EnvironmentFile,
    route: String,
    /// The completion the switched `finish` writes, when the session adopts it.
    adopted: Option<Completion>,
    finish_at: Option<usize>,
}

/// Load `choice`, apply the selection, resolve the route binding and assemble on
/// `catalog` EXACTLY as the start path does — the same cache-key policy with the
/// parent's ordinal, the same standing instructions. Nothing is installed.
fn session_candidate(
    switch: &ModelSwitch,
    catalog: &Catalog,
    choice: &crate::models::Choice,
    current_finish: &Option<Arc<dyn Tool>>,
) -> Result<SessionCandidate, String> {
    let mut environment = load_environment(&choice.environment, &switch.environment_dirs)
        .map_err(|error| error.to_string())?;
    with_worker_tools(&mut environment);
    crate::models::apply(&mut environment, choice, &switch.environment_dirs)?;
    crate::catalog::resolve_environment(&mut environment, &switch.environment_dirs)?;
    let assembled = assemble_with_cache_key(
        catalog,
        &environment,
        &switch.workspace,
        &switch.substitutions,
        PARENT_ORDINAL,
        &switch.mask,
    )?;
    // The catalog's `finish` factory issued this assembly its own completion. Take
    // it, so the hub cannot hand a stale one to a later worker assembly, and so it
    // is there for the switched tool set's own `finish` (below).
    let issued = switch.completion.take();
    let finish_at = finish_index(&assembled);
    // The label the renderer names after this switch, exactly as the start path
    // named it (`Origin.route`, `<adapter>/<account>`).
    let route = assembled.resolved.route.origin.route.clone();
    let context = agent_context(&assembled, environment.profile.as_deref())?;
    let mut tools = assembled.tools;
    // The switched tool set's `finish` must reach the completion the run reads. The
    // session keeps ITS `finish` — the whole session's activity is in that tool's
    // log — when the environment declares it under the same model-facing name;
    // otherwise the switched tool set's own is the session's from now on, and the
    // plumbing follows the completion the catalog just issued it (which the `finish`
    // factory always does).
    let adopted = match (current_finish, finish_at) {
        (Some(kept), Some(index)) if kept.declaration().name == tools[index].declaration().name => {
            tools[index] = kept.clone();
            None
        }
        (_, Some(_)) => issued,
        _ => None,
    };
    Ok(SessionCandidate {
        parts: CandidateParts {
            provider: assembled.provider,
            tools,
            system_prompt: assembled.system_prompt + switch.instructions.as_str(),
            options: assembled.options,
            context,
        },
        environment,
        route,
        adopted,
        finish_at,
    })
}

/// The session state follows an installed candidate: its `finish` plumbing, its
/// model and the route label. Called only once the agent installed it, so a failed
/// switch or reload changed nothing. Returns the new `E/P[:effort]`.
fn adopt_candidate(switch: &ModelSwitch, candidate: SessionCandidate) -> String {
    let SessionCandidate {
        parts,
        environment,
        route,
        adopted,
        finish_at,
    } = candidate;
    let tools = parts.tools;
    let mut session = switch.session.lock().unwrap();
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
    session.effort = environment.options.reasoning_effort;
    // The session's route label moves only now: a failed switch changed nothing.
    if let Some(label) = &switch.route_label {
        *label.lock().unwrap() = route;
    }
    model_name(&environment)
}

// ------------------------------------------ the module reload (ADR-0084 §3)
//
// `/modules reload` is a model switch to the model the session runs now, on a
// catalog loaded again from the release (S1.4's loader through `build_catalog`)
// and with the policy the session selects, installed through the same
// `Agent::reconfigure`. It happens only between complete turns: `&mut Agent` is
// free only once the turn and its tool calls settled, and a front end that is busy
// queues the request on [`ReloadQueue`] and applies it at its next boundary.

/// One assembly generation of the session: the catalog a start or a reload loaded
/// from the release, and the authorization policy that decides with it.
pub struct Generation {
    number: u64,
    catalog: Arc<Catalog>,
    authorization: Arc<dyn AuthorizationPolicy>,
}

impl std::fmt::Debug for Generation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Generation")
            .field("number", &self.number)
            .finish_non_exhaustive()
    }
}

impl Generation {
    /// 0 for the start's generation, one more for every installed reload.
    pub fn number(&self) -> u64 {
        self.number
    }

    /// The catalog every assembly of this generation is built on.
    pub fn catalog(&self) -> &Arc<Catalog> {
        &self.catalog
    }

    /// The policy that decides this generation's calls.
    pub fn authorization(&self) -> Arc<dyn AuthorizationPolicy> {
        self.authorization.clone()
    }
}

/// The session's current generation. Whatever starts an assembly — the parent's
/// switch, a child, a workflow step — pins [`Generations::current`] when it starts
/// and keeps it until it ends: an installed reload replaces the current generation
/// and never a pinned one, and a generation is dropped with its last pin.
pub struct Generations {
    /// `None` until the start's catalog exists: the composition reads the
    /// configuration that names the delegation service before the catalog is
    /// built, and the child builder is created in between.
    current: Mutex<Option<Arc<Generation>>>,
}

impl Generations {
    /// No generation yet. The child builder is created before the catalog (the
    /// catalog registers the `worker_*` tools, so the service must exist first);
    /// [`Generations::install`] installs generation 0 once the catalog is built.
    pub fn empty() -> Self {
        Self {
            current: Mutex::new(None),
        }
    }

    /// Generation 0: what the session started with.
    pub fn new(catalog: Arc<Catalog>, authorization: Arc<dyn AuthorizationPolicy>) -> Self {
        let generations = Self::empty();
        generations.install(catalog, authorization);
        generations
    }

    /// Install the NEXT generation: the start path's catalog and policy once the
    /// catalog exists, and a `/modules reload`'s candidate after its agent took it
    /// ([`install_candidate`]). The swap is here — one lock, no await — so a child or
    /// workflow step starting after it pins the new generation, and one already
    /// running keeps the old.
    pub fn install(
        &self,
        catalog: Arc<Catalog>,
        authorization: Arc<dyn AuthorizationPolicy>,
    ) -> Arc<Generation> {
        let mut current = self.current.lock().unwrap();
        let generation = Arc::new(Generation {
            number: current
                .as_ref()
                .map_or(0, |generation| generation.number + 1),
            catalog,
            authorization,
        });
        *current = Some(generation.clone());
        generation
    }

    /// The generation a new assembly pins.
    pub fn current(&self) -> Arc<Generation> {
        self.try_current()
            .expect("the session's assembly generation is installed")
    }

    /// The generation a new assembly pins, when the start has installed one: a
    /// child build asked for before the catalog exists is refused, not a panic.
    pub fn try_current(&self) -> Option<Arc<Generation>> {
        self.current.lock().unwrap().clone()
    }
}

/// The agent-facing parts of a candidate assembly.
pub struct CandidateParts {
    pub provider: Arc<dyn Provider>,
    pub tools: Vec<Arc<dyn Tool>>,
    pub system_prompt: String,
    pub options: ModelOptions,
    pub context: Arc<dyn ContextPolicy>,
}

/// A complete reload candidate: the catalog loaded again, the policy, and the
/// session's assembly built on that catalog.
pub struct Candidate {
    pub catalog: Arc<Catalog>,
    pub authorization: Arc<dyn AuthorizationPolicy>,
    pub parts: CandidateParts,
}

/// Validate `candidate` against the agent's current history, commit it as ONE
/// `Environment` record and install it — policy included — through
/// `Agent::reconfigure`, then make it the current generation. Nothing awaits
/// between the commit and either installation: the agent's is inside
/// `reconfigure`, and the generation is swapped in the same poll `reconfigure`
/// returns in. On any failure nothing changes: the agent keeps its assembly and
/// its policy, and the current generation stays.
pub async fn install_candidate(
    generations: &Generations,
    agent: &mut Agent,
    candidate: Candidate,
) -> Result<Arc<Generation>, ReconfigureError> {
    let Candidate {
        catalog,
        authorization,
        parts,
    } = candidate;
    agent
        .reconfigure(Reconfiguration {
            provider: parts.provider,
            tools: parts.tools,
            system_prompt: parts.system_prompt,
            options: parts.options,
            context: parts.context,
            authorization: Some(authorization.clone()),
        })
        .await?;
    Ok(generations.install(catalog, authorization))
}

/// What a `/modules reload` request did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReloadRequested {
    /// The session is idle: the front end applies it now.
    Now,
    /// The session is busy: it waits for the next boundary between complete turns.
    Pending,
}

/// The session's queued `/modules reload`: requested from a key handler or a line,
/// applied by the loop that owns the agent, at a boundary.
#[derive(Default)]
pub struct ReloadQueue {
    pending: AtomicBool,
}

impl ReloadQueue {
    /// Queue a reload. `busy` is whether a turn (and so its tool calls) is still
    /// running; the answer says whether it waits for that turn's end.
    pub fn request(&self, busy: bool) -> ReloadRequested {
        self.pending.store(true, Ordering::SeqCst);
        if busy {
            ReloadRequested::Pending
        } else {
            ReloadRequested::Now
        }
    }

    /// Take the queued request, at a boundary: `true` once per request.
    pub fn take(&self) -> bool {
        self.pending.swap(false, Ordering::SeqCst)
    }
}

/// What the host's reload rebuilds a candidate from: its own copy of the
/// dependencies the catalog is built from, the run's sandbox selection, and the
/// session's policy.
pub(crate) struct ReloadInputs {
    deps: HostDeps,
    sandbox: cli::SandboxMode,
    sandbox_write: Vec<PathBuf>,
    sandbox_read: Vec<PathBuf>,
    env_pass: Vec<String>,
    /// The policy the session selects. The host's policies are still the native
    /// twins of `p1/policy/*` (`policy.rs`), owned by the front end that asks
    /// through them, so this hands back the front end's; a loaded policy package
    /// is built here once the host loads them.
    policy: Box<dyn Fn() -> Arc<dyn AuthorizationPolicy> + Send + Sync>,
    queue: ReloadQueue,
}

/// A copy of what [`build_catalog`] reads of `deps`, for a reload that runs where
/// `deps` is not reachable (the TUI's loop). The test catalog hook moves behind an
/// `Arc` both copies call.
fn catalog_deps(deps: &mut HostDeps) -> HostDeps {
    let hook: Option<Arc<crate::catalog::CatalogHook>> = deps.catalog_hook.take().map(Arc::new);
    let forward = |hook: Arc<crate::catalog::CatalogHook>| -> crate::catalog::CatalogHook {
        Box::new(move |catalog: &mut Catalog| hook(catalog))
    };
    deps.catalog_hook = hook.clone().map(forward);
    HostDeps {
        stdout: deps.stdout.clone(),
        stderr: deps.stderr.clone(),
        lines: deps.lines.clone(),
        transport: deps.transport.clone(),
        date: deps.date.clone(),
        interrupt: deps.interrupt.clone(),
        environment_dirs: deps.environment_dirs.clone(),
        stdout_is_tty: deps.stdout_is_tty,
        home: deps.home.clone(),
        runtime_dir: deps.runtime_dir.clone(),
        shell_env: deps.shell_env.clone(),
        #[cfg(feature = "shadow-hook")]
        shadow: deps.shadow.clone(),
        catalog_hook: hook.map(forward),
        wait: deps.wait.clone(),
        #[cfg(feature = "delegation")]
        worker_service: deps.worker_service.clone(),
        #[cfg(feature = "workflows")]
        workflow_service: deps.workflow_service.clone(),
        #[cfg(feature = "workflows")]
        workflow_observer: deps.workflow_observer.clone(),
        model_switch: None,
    }
}

/// The ONE `/modules reload` entry point: load the release again through the
/// catalog build (S1.4's `load_locked_modules` and `register_modules`), assemble the
/// model the session runs now on it exactly as the start path does, take the
/// session's policy, and install the whole candidate with [`install_candidate`].
/// Callable only between turns (`&mut Agent`).
///
/// On success the new generation and model are returned; on any failure (load,
/// verification, assembly, validation or commit) the reason is returned and the
/// current assembly keeps answering.
pub(crate) async fn reload_modules(
    switch: &ModelSwitch,
    agent: &mut Agent,
) -> Result<String, String> {
    let inputs = &switch.reload;
    let catalog = Arc::new(build_catalog(
        &inputs.deps,
        inputs.sandbox,
        &inputs.sandbox_write,
        &inputs.sandbox_read,
        &inputs.env_pass,
        &switch.completion,
    )?);
    let current = switch.session_snapshot();
    let choice = crate::models::Choice {
        environment: current.environment,
        // An effort belongs to a profile; without one the environment keeps its own.
        effort: current.profile.as_ref().and(current.effort),
        profile: current.profile,
    };
    let candidate = session_candidate(switch, &catalog, &choice, &current.finish)?;
    let generation = install_candidate(
        &switch.generations,
        agent,
        Candidate {
            catalog,
            authorization: (inputs.policy)(),
            parts: CandidateParts {
                provider: candidate.parts.provider.clone(),
                tools: candidate.parts.tools.clone(),
                system_prompt: candidate.parts.system_prompt.clone(),
                options: candidate.parts.options.clone(),
                context: candidate.parts.context.clone(),
            },
        },
    )
    .await
    .map_err(|error| error.to_string())?;
    let model = adopt_candidate(switch, candidate);
    Ok(format!("generation {} · {model}", generation.number()))
}

/// What a `/modules reload` did, on the host's own channel.
fn report_reload(deps: &HostDeps, outcome: Result<String, String>) {
    match outcome {
        Ok(reloaded) => write_stderr(deps, &format!("· modules reloaded: {reloaded}\n")),
        Err(reason) => write_stderr(deps, &format!("· modules not reloaded: {reason}\n")),
    }
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

/// The interactive counterpart to the headless guard: it reports the activity
/// log's consecutive-summary count to the TUI but never cancels the operator's run.
struct InteractiveStallWatcher {
    inner: Arc<dyn EventSink>,
    activity: Arc<ParentActivity>,
    max: usize,
    warned: AtomicBool,
}

impl EventSink for InteractiveStallWatcher {
    fn emit(&self, event: AgentEvent) {
        if matches!(event, AgentEvent::ContextReplaced { .. }) {
            self.activity.record_replacement();
        }
        self.inner.emit(event);

        let count = self.activity.consecutive_replacements() as usize;
        let show = count >= self.max;
        let was_shown = self.warned.swap(show, Ordering::SeqCst);
        if show != was_shown || (show && count == self.max) {
            // Private host-to-TUI control notice. The TUI turns it into (or removes)
            // the single warning meta row; it is never exposed to the model.
            self.inner.emit(AgentEvent::ProviderNotice {
                text: format!("\0p1-idle-summary-count:{count}"),
            });
        }
    }
}

/// Issue #142: report how many credential-shaped values were masked during one
/// turn, once per turn boundary, through the host's EXISTING display-only notice
/// path (`AgentEvent::ProviderNotice`, the channel a provider notice and a
/// worker-end note already use). Only the count is ever emitted; the values live
/// only in [`p1_redact`]'s replacement and never reach an event, a journal record or
/// a request.
pub(crate) struct MaskNoticeSink {
    inner: Arc<dyn EventSink>,
    counter: Arc<MaskCounter>,
}

impl MaskNoticeSink {
    pub(crate) fn new(inner: Arc<dyn EventSink>, counter: Arc<MaskCounter>) -> Self {
        Self { inner, counter }
    }
}

impl EventSink for MaskNoticeSink {
    fn emit(&self, event: AgentEvent) {
        let turn_finished = matches!(event, AgentEvent::TurnFinished { .. });
        self.inner.emit(event);
        if !turn_finished {
            return;
        }
        let masked = self.counter.take();
        if masked > 0 {
            self.inner.emit(AgentEvent::ProviderNotice {
                text: format!("masked {masked} credential-shaped value(s) in tool output"),
            });
        }
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
/// (`InvalidRequest`, `Authentication`, `ContextWindowExceeded`,
/// `UsageLimitExhausted`) ends the run as it always did.
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

/// The ONE line a manual compaction reports (ADR-0076): the TUI's `/compact`
/// and `--compact` on resume print the same text.
pub(crate) fn compaction_line(result: &Result<Compaction, ContextError>) -> String {
    match result {
        Ok(Compaction::Replaced {
            tokens_before,
            tokens_after,
            ..
        }) => format!("compacted: {tokens_before} → {tokens_after} tokens"),
        Ok(Compaction::Unchanged { tokens }) => format!("nothing to compact: {tokens} tokens"),
        Err(error) => format!("compact failed: {error}"),
    }
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
pub(crate) fn session_journals(session: Option<&Path>) -> Vec<PathBuf> {
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

/// Print `text` on the host's stdout. `pub(crate)`: the module CLI (ADR-0079) prints
/// through the same writer the other read-only commands use.
pub(crate) fn write_stdout(deps: &HostDeps, text: &str) {
    let mut writer = deps.stdout.lock().unwrap();
    let _ = writer.write_all(text.as_bytes());
    let _ = writer.flush();
}

pub(crate) fn write_stderr(deps: &HostDeps, text: &str) {
    let mut writer = deps.stderr.lock().unwrap();
    let _ = writer.write_all(text.as_bytes());
    let _ = writer.flush();
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
pub(crate) fn assemble_with_cache_key(
    catalog: &Catalog,
    environment: &p1_assembly::EnvironmentFile,
    workspace: &std::path::Path,
    substitutions: &Substitutions,
    agent_ordinal: u64,
    mask: &Arc<MaskCounter>,
) -> Result<p1_assembly::Assembled, String> {
    let name = environment.name.clone();
    let configured = environment.options.clone();
    // B-S6-9, D068: a MAIN agent's tools learn which parent they serve, so the worker and
    // workflow members' scopes are per parent. The parent's ordinal names it: each main
    // agent has its own catalog, worker service and scope generation, so the ordinal is
    // unique among the agents that share one scope registry. Workers get none.
    let agent = (agent_ordinal == PARENT_ORDINAL).then(|| agent_ordinal.to_string());
    let mut assembled = p1_assembly::assemble_for_agent(
        catalog,
        environment,
        workspace,
        substitutions,
        mask,
        agent.as_deref(),
        |route| {
            let mut options = configured.clone();
            if options.cache_key.is_none() && route.cache_key == CacheKeySupport::Optional {
                options.cache_key = Some(generated_cache_key(workspace, &name, agent_ordinal));
            }
            options
        },
    )
    .map_err(|error| error.to_string())?;
    // Issue #142: every ASSEMBLED tool is wrapped here, at the one host assembly
    // path, so a tool's result text is masked before p1-core turns it into a
    // `ToolResultItem` — history, journal and every later request only ever see the
    // masked form. Declaration and identity are forwarded unchanged, so dispatch and
    // the journalled identity do not move. `mask` is also the counter in the shared
    // `ToolServices`, so a module tool `wasm_tool` already wrapped counts into THIS
    // counter; masking is idempotent, so this second wrapper adds nothing for it.
    for tool in &mut assembled.tools {
        *tool = redacted(tool.clone(), mask);
    }
    Ok(assembled)
}

/// A STABLE provider-side prompt-cache key for one agent: a pure function of the
/// workspace, the environment name and the agent's ordinal — no process id and
/// no clock. Stability is the point: a resume and a re-run in the same workspace
/// keep their provider-side cache routing, and the journalled environment no
/// longer changes on resume. Ordinal 0 is the parent agent ([`PARENT_ORDINAL`]);
/// workers get 1, 2, … in start order ([`next_agent_ordinal`](crate::catalog::children::next_agent_ordinal)). Without a key the
/// Codex route served 0 cached tokens across a whole task (measured 2026-09-20);
/// routes without such a key never see it.
pub(crate) fn generated_cache_key(
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
pub(crate) const PARENT_ORDINAL: u64 = 0;

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[derive(Default)]
    struct CapturedEvents(Mutex<Vec<AgentEvent>>);

    impl EventSink for CapturedEvents {
        fn emit(&self, event: AgentEvent) {
            self.0.lock().unwrap().push(event);
        }
    }

    #[test]
    fn an_exhausted_usage_limit_is_not_a_transient_host_failure() {
        let end = TurnEnd::ProviderFailed {
            error: p1_contracts::ProviderError::new(
                ProviderErrorKind::UsageLimitExhausted,
                "the account's usage allowance is used up",
            ),
        };
        assert_eq!(transient_kind(&end), None);
    }

    #[test]
    fn interactive_idle_summary_warning_is_display_only_and_clears_on_progress() {
        let captured = Arc::new(CapturedEvents::default());
        // Share the same activity counter for both observing and forwarding.
        let workspace = tempfile::tempdir().unwrap();
        let log = Arc::new(ActivityLog::default());
        log.watch_workspace(workspace.path(), &[]);
        let activity = Arc::new(ParentActivity::new(captured.clone(), log.clone(), &[]));
        let watcher = InteractiveStallWatcher {
            inner: activity.clone(),
            activity,
            max: 2,
            warned: AtomicBool::new(false),
        };
        for _ in 0..2 {
            watcher.emit(AgentEvent::ContextReplaced {
                items_before: 100,
                items_after: 10,
                usage: None,
            });
        }
        let call = p1_contracts::ToolCall {
            call_id: "w1".into(),
            name: "write".into(),
            input: p1_contracts::ToolInput::Json("{}".into()),
        };
        log.record_started(&call, p1_contracts::Effect::WritesFiles);
        std::fs::write(workspace.path().join("progress.txt"), "worked").unwrap();
        watcher.emit(AgentEvent::ToolFinished {
            result: p1_contracts::ToolResultItem {
                call_id: "w1".into(),
                name: "write".into(),
                status: p1_contracts::ToolStatus::Ok,
                content: "wrote".into(),
            },
        });

        let notices: Vec<_> = captured
            .0
            .lock()
            .unwrap()
            .iter()
            .filter_map(|event| match event {
                AgentEvent::ProviderNotice { text } => Some(text.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(
            notices,
            ["\0p1-idle-summary-count:2", "\0p1-idle-summary-count:0"]
        );
    }

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

    // ---------------------------- #125 review: the effective context table

    /// The shipped `environments/` directory, as the host searches it.
    fn shipped_environments() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../environments")
    }

    /// The `[context]` table the shipped environment declares.
    fn shipped_settings(environment: &str) -> p1_assembly::ContextSettings {
        load_environment(environment, &[shipped_environments()])
            .expect("the shipped environment loads")
            .context
            .expect("the shipped environment opts in with [context]")
    }

    /// The shipped profile, parsed through the same entry point the loader uses.
    fn shipped_profile(stem: &str) -> ModelProfile {
        let path = shipped_environments()
            .join("../profiles")
            .join(format!("{stem}.toml"));
        let text = std::fs::read_to_string(&path)
            .unwrap_or_else(|error| panic!("{} is unreadable: {error}", path.display()));
        ModelProfile::from_toml(stem, &text).expect("the shipped profile parses")
    }

    /// The reserve the effective table leaves for the next response.
    fn wall_of(config: &p1_context::ContextConfig) -> u64 {
        config.window_tokens - config.output_headroom_tokens
    }

    /// A profile that is not a shipped file: the folding rule must hold for any capacity a
    /// profile can state, including ones no shipped binding has.
    fn synthetic_profile(
        context_tokens: Option<u64>,
        max_output_tokens: Option<u32>,
    ) -> ModelProfile {
        ModelProfile {
            id: "synthetic".into(),
            revision: 1,
            model_id: "synthetic-model".into(),
            family: "synthetic".into(),
            thinking: p1_model_profile::ThinkingPolicy::Enabled,
            efforts: vec![Effort::Low, Effort::High],
            default_effort: Some(Effort::High),
            thinking_budgets: Default::default(),
            context_tokens,
            max_output_tokens,
        }
    }

    /// #125 review: a profile that states a SMALLER window than the environment narrows the
    /// effective table, so selecting MiMo (200k) on `zen` compacts at MiMo's size instead of
    /// sending requests the binding cannot serve.
    #[test]
    fn a_narrower_profile_narrows_the_effective_context_table() {
        let settings = shipped_settings("zen");
        let mimo = shipped_profile("mimo-v2.6-flash-free");
        assert_eq!(
            mimo.context_tokens,
            Some(200_000),
            "the shipped profile states the narrow window"
        );
        let config = config_for_route(&settings, Some(&mimo));
        assert_eq!(config.window_tokens, 200_000, "the profile's window wins");
        assert_eq!(
            config.output_headroom_tokens, 32_000,
            "the reserve is the profile's own output ceiling, below zen's 524,288"
        );
        assert!(
            config.summarize_at_tokens <= 120_000,
            "threshold {} must stay at or below 60% of the effective window",
            config.summarize_at_tokens
        );
        assert!(
            config.summarize_at_tokens < wall_of(&config),
            "threshold {} must stay below the wall {}",
            config.summarize_at_tokens,
            wall_of(&config)
        );
        // The effective table must be one the policy accepts.
        p1_context::ContextConfig::validate(&config).expect("the effective table validates");
    }

    /// A profile that states MORE than the environment cannot widen it: the environment's table
    /// is what its route serves, and the profile is only allowed to narrow.
    #[test]
    fn a_profile_stating_more_capacity_than_the_environment_cannot_widen_the_table() {
        let settings = shipped_settings("zen");
        let roomy = synthetic_profile(Some(4_000_000), Some(900_000));
        let config = config_for_route(&settings, Some(&roomy));
        assert_eq!(
            config.window_tokens, settings.window_tokens,
            "the environment's window still caps the table"
        );
        assert_eq!(
            config.output_headroom_tokens, settings.output_headroom_tokens,
            "the environment's reserve still caps the reserve"
        );
        assert_eq!(config.summarize_at_tokens, settings.summarize_at_tokens);
        assert_eq!(config.keep_recent_tokens, settings.keep_recent_tokens);
        assert_eq!(
            config,
            config_for_route(&settings, None),
            "a roomier profile changes nothing"
        );
        p1_context::ContextConfig::validate(&config).expect("the effective table validates");
    }

    /// A profile far narrower than the environment (40,000 tokens on `zen`) clamps the copied
    /// budgets to the effective window: the kept tail and the verbatim share are budgets of THIS
    /// window, and a tail larger than what a request can carry would keep the whole history
    /// verbatim and send the next request over the wall.
    #[test]
    fn a_very_narrow_profile_clamps_the_copied_verbatim_budgets() {
        let settings = shipped_settings("zen");
        assert!(
            settings.keep_recent_tokens > 40_000,
            "the environment keeps a tail larger than the narrow profile's window"
        );
        let narrow = synthetic_profile(Some(40_000), Some(32_000));
        let config = config_for_route(&settings, Some(&narrow));
        assert_eq!(config.window_tokens, 40_000);
        assert_eq!(config.output_headroom_tokens, 32_000);
        assert!(
            config.summarize_at_tokens < wall_of(&config),
            "threshold {} must stay below the wall {}",
            config.summarize_at_tokens,
            wall_of(&config)
        );
        assert!(
            config.keep_recent_tokens < wall_of(&config),
            "the kept tail ({}) must stay below the wall ({})",
            config.keep_recent_tokens,
            wall_of(&config)
        );
        assert!(
            config.user_verbatim_tokens <= config.keep_recent_tokens,
            "the verbatim share ({}) must stay inside the kept tail ({})",
            config.user_verbatim_tokens,
            config.keep_recent_tokens
        );
        p1_context::ContextConfig::validate(&config).expect("the effective table validates");

        // A profile whose own output ceiling exceeds its own window: the reserve is clamped
        // below the window, so the table still describes a sendable request. What is left is too
        // little to send a summary under, and that is a configuration error that NAMES the profile
        // and the window it serves — not a bare number check the operator never wrote.
        let odd = synthetic_profile(Some(20_000), Some(32_000));
        let config = config_for_route(&settings, Some(&odd));
        assert_eq!(config.window_tokens, 20_000);
        assert!(
            config.output_headroom_tokens < config.window_tokens,
            "the reserve ({}) stays below the window ({})",
            config.output_headroom_tokens,
            config.window_tokens
        );
        assert!(config.keep_recent_tokens < wall_of(&config));
        p1_context::ContextConfig::validate(&config).expect("the effective table validates");

        let (assembled, _) = assembled_for_test(Some(settings.clone()), Effort::High);
        let error = match agent_context(&assembled, Some(&odd)) {
            Ok(_) => panic!("a table with no room for a summary must not build an agent"),
            Err(error) => error,
        };
        assert!(error.contains("synthetic"), "{error}");
        assert!(error.contains("20000"), "{error}");
        assert!(
            error.contains("summary-output cap"),
            "the error names the budget that does not fit: {error}"
        );
    }

    /// #125 review round 3: the summary-output cap is a budget of the SAME effective window, so a
    /// profile that narrows the window narrows the cap with it. Under a 40,000-token profile on the
    /// `zen` table (wall 8,000) the environment's 12,000 cap cannot be sent; the agent must still
    /// start, with the cap clamped below the wall, and the request it sends must carry that cap.
    #[tokio::test(start_paused = true)]
    async fn a_narrow_profile_clamps_the_summary_output_cap_and_the_agent_still_starts() {
        // The zen table's own window, reserve and summary cap, with a threshold a two-item
        // history crosses.
        let settings = p1_assembly::ContextSettings {
            window_tokens: 1_048_576,
            output_headroom_tokens: 524_288,
            summarize_at_tokens: 100,
            summary_output_tokens: 12_000,
            ..summarizer_table()
        };
        let narrow = synthetic_profile(Some(40_000), Some(32_000));
        let (assembled, provider) = assembled_for_test(Some(settings), Effort::High);
        let policy = agent_context(&assembled, Some(&narrow))
            .expect("a valid narrow profile must not make the agent unstartable");
        let history = summarizer_history();
        let cancel = CancellationToken::new();
        let prepared = policy
            .prepare(ContextInput {
                history: &history,
                last_usage: None,
                cancel: &cancel,
            })
            .await
            .expect("preparing a summary succeeds")
            .expect("the history crosses the threshold");
        assert_eq!(
            provider.requests()[0].options.max_output_tokens,
            Some(4_000),
            "the cap is clamped to half of the effective wall (8,000), not sent at the \
             environment's 12,000"
        );
        assert_eq!(prepared.usage, None);
    }

    /// The other two zen bindings: Space Bunny is the environment's own table, Muse's output
    /// ceiling lowers only the reserve (its window is the environment's window).
    #[test]
    fn a_profile_bounds_the_reserve_and_keeps_the_window_it_does_not_narrow() {
        let settings = shipped_settings("zen");

        let space_bunny = shipped_profile("space-bunny-free");
        let config = config_for_route(&settings, Some(&space_bunny));
        assert_eq!(config.window_tokens, settings.window_tokens);
        assert_eq!(
            config.output_headroom_tokens,
            settings.output_headroom_tokens
        );
        assert_eq!(config.summarize_at_tokens, settings.summarize_at_tokens);

        let muse = shipped_profile("muse-spark-1.3-contributor-free");
        let config = config_for_route(&settings, Some(&muse));
        assert_eq!(
            config.window_tokens, 1_048_576,
            "the window is not narrowed"
        );
        assert_eq!(
            config.output_headroom_tokens, 131_072,
            "the profile's output ceiling bounds the reserve"
        );
        assert_eq!(config.summarize_at_tokens, 500_000);
        p1_context::ContextConfig::validate(&config).expect("the effective table validates");
    }

    /// A profile that states no capacity at all (the one the `deepseek` environment binds) leaves
    /// the environment's table exactly as it is — and so does the whole-provider form, which names
    /// no profile.
    #[test]
    fn a_profile_that_states_nothing_leaves_the_environment_table_alone() {
        let settings = shipped_settings("deepseek");
        let config = config_for_route(&settings, None);
        assert_eq!(config.window_tokens, settings.window_tokens);
        assert_eq!(
            config.output_headroom_tokens,
            settings.output_headroom_tokens
        );
        assert_eq!(config.summarize_at_tokens, settings.summarize_at_tokens);
        assert_eq!(config.keep_recent_tokens, settings.keep_recent_tokens);

        // The environment's own binding, not a compiled model id: route data stays out of run.rs
        // (`tests/route_files.rs`).
        let deepseek = load_environment("deepseek", &[shipped_environments()])
            .expect("the shipped environment loads")
            .profile
            .expect("the deepseek environment binds a profile");
        let deepseek = deepseek.as_ref().clone();
        assert_eq!(
            deepseek.context_tokens, None,
            "the profile states no window"
        );
        assert_eq!(deepseek.max_output_tokens, None, "and no output ceiling");
        assert_eq!(config_for_route(&settings, Some(&deepseek)), config);
        assert_eq!(config.window_tokens, 1_000_000);
        assert_eq!(config.summarize_at_tokens, 300_000);
    }

    /// Review 146 r4: an environment's own threshold survives profile folding when the profile
    /// does not narrow the window — the shipped `gpt` environment binds a profile and compacts at
    /// its decided point, not at 60% of its window.
    #[test]
    fn a_profile_that_does_not_narrow_the_window_keeps_the_environment_threshold() {
        let settings = shipped_settings("gpt");
        let profile = load_environment("gpt", &[shipped_environments()])
            .expect("the shipped environment loads")
            .profile
            .expect("the gpt environment binds a profile");
        let config = config_for_route(&settings, Some(profile.as_ref()));
        assert_eq!(config.window_tokens, settings.window_tokens);
        assert_eq!(config.summarize_at_tokens, settings.summarize_at_tokens);
        assert!(
            config.summarize_at_tokens > settings.window_tokens * 60 / 100,
            "the decided threshold is above 60% of the window, so the rule must not apply"
        );
        p1_context::ContextConfig::validate(&config).expect("the effective table validates");
    }

    // ---------------------------- #125 review: the summary's own effort

    /// The floor the host passes: the profile's lowest level when it states one, no override for
    /// a present profile with an empty effort list, and `Low` when there is no profile to read
    /// one from (the whole-provider form).
    #[test]
    fn the_summary_effort_is_the_profiles_lowest_level_or_low_without_a_profile() {
        let mut profile = synthetic_profile(None, None);
        profile.efforts = vec![Effort::Low, Effort::High];
        assert_eq!(
            summary_effort(Some(&profile)),
            Some(Effort::Low),
            "a present profile uses its lowest listed level"
        );

        let mut empty = synthetic_profile(None, None);
        empty.efforts.clear();
        assert_eq!(
            summary_effort(Some(&empty)),
            None,
            "a present profile with no effort list carries no override"
        );
        assert_eq!(
            summary_effort(None),
            Some(Effort::Low),
            "no profile must not mean the agent's own effort"
        );
    }

    /// An assembled agent for the request-level tests: a scripted provider, one context table
    /// and the agent's own effort. The settings are tiny so a two-item history crosses the
    /// threshold (the same shape `crates/p1-context/tests/impl_internals.rs` uses).
    fn assembled_for_test(
        context: Option<p1_assembly::ContextSettings>,
        effort: Effort,
    ) -> (Assembled, Arc<p1_testkit::ScriptedProvider>) {
        let provider = Arc::new(p1_testkit::ScriptedProvider::new(vec![
            p1_testkit::text_response("s"),
        ]));
        let options = p1_contracts::ModelOptions {
            reasoning_effort: Some(effort),
            ..p1_contracts::ModelOptions::default()
        };
        let assembled = Assembled {
            resolved: p1_assembly::ResolvedEnvironment {
                environment: "test".into(),
                family: "test".into(),
                route: p1_contracts::RouteDescription {
                    origin: p1_contracts::Origin {
                        route: "fake".into(),
                        model: "fake-model".into(),
                    },
                    supports_freeform_tools: false,
                    mandatory_prompt_prefix: None,
                    reports_cost: false,
                    cache_key: CacheKeySupport::Optional,
                },
                system_prompt: "sys".into(),
                tools: Vec::new(),
                options: options.clone(),
                context,
                summarize_prompt: None,
            },
            provider: provider.clone(),
            tools: Vec::new(),
            system_prompt: "sys".into(),
            options,
        };
        (assembled, provider)
    }

    fn summarizer_history() -> Vec<p1_contracts::Item> {
        vec![
            p1_contracts::Item::Assistant(p1_contracts::AssistantItem {
                origin: p1_testkit::origin(),
                blocks: vec![p1_contracts::AssistantBlock::Text {
                    text: "x".repeat(500),
                }],
            }),
            p1_contracts::Item::Assistant(p1_contracts::AssistantItem {
                origin: p1_testkit::origin(),
                blocks: vec![p1_contracts::AssistantBlock::Text {
                    text: "tail".into(),
                }],
            }),
        ]
    }

    fn summarizer_table() -> p1_assembly::ContextSettings {
        p1_assembly::ContextSettings {
            window_tokens: 10_000,
            output_headroom_tokens: 1_000,
            summarize_at_tokens: 100,
            keep_recent_tokens: 80,
            user_verbatim_tokens: 100,
            tool_result_excerpt_chars: 2_000,
            summary_output_tokens: 4_000,
        }
    }

    /// #125 review: the request the host's policy actually sends carries the lowered effort,
    /// whatever the assembled agent's own options name.
    #[tokio::test(start_paused = true)]
    async fn the_host_summarizes_at_the_profiles_lowest_effort_whatever_the_agent_runs_at() {
        for agent_effort in [Effort::High, Effort::ExtraHigh, Effort::Max] {
            let (assembled, provider) = assembled_for_test(Some(summarizer_table()), agent_effort);
            let policy = agent_context(&assembled, Some(&shipped_profile("space-bunny-free")))
                .expect("the environment builds a summarizer");
            let history = summarizer_history();
            let cancel = CancellationToken::new();
            let prepared = policy
                .prepare(ContextInput {
                    history: &history,
                    last_usage: None,
                    cancel: &cancel,
                })
                .await
                .expect("preparing a summary succeeds");
            assert!(prepared.is_some(), "the history crosses the threshold");
            assert_eq!(
                provider.requests()[0].options.reasoning_effort,
                Some(Effort::Low),
                "an agent at {agent_effort:?} still summarizes at the profile's lowest level"
            );
        }
    }

    /// #125 review: with NO profile there is nothing to read a floor from, so the summary runs
    /// at `Low` — never at the agent's own level.
    #[tokio::test(start_paused = true)]
    async fn the_host_summarizes_at_low_when_the_environment_names_no_profile() {
        for agent_effort in [Effort::High, Effort::Max] {
            let (assembled, provider) = assembled_for_test(Some(summarizer_table()), agent_effort);
            let policy =
                agent_context(&assembled, None).expect("the environment builds a summarizer");
            let history = summarizer_history();
            let cancel = CancellationToken::new();
            let prepared = policy
                .prepare(ContextInput {
                    history: &history,
                    last_usage: None,
                    cancel: &cancel,
                })
                .await
                .expect("preparing a summary succeeds");
            assert!(prepared.is_some(), "the history crosses the threshold");
            assert_eq!(
                provider.requests()[0].options.reasoning_effort,
                Some(Effort::Low),
                "an agent at {agent_effort:?} with no profile"
            );
            assert_ne!(
                provider.requests()[0].options.reasoning_effort,
                Some(agent_effort),
                "the agent's own effort must not leak into the summary"
            );
        }
    }
}
