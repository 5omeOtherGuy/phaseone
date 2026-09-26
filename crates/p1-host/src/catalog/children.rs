//! The child assembly: how the host composes the worker service and builds each
//! worker (a direct `worker_start` child or a workflow step) as an agent of its own.
//! Kept apart from `run.rs` so the delegation family can later be built as a module
//! without touching the run drivers; the parent's assembly path stays in `run.rs`.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use p1_assembly::{Assembled, Catalog, EnvironmentFile, Substitutions, ToolSpec, load_environment};
use p1_contracts::{AgentEvent, CommitSink, EventSink, Tool, TurnEnd};
use p1_core::{Agent, AgentParts, Reconfiguration};
use p1_journal::MemoryJournal;
use p1_model_profile::ModelProfile;
use p1_redact::{MaskCounter, redacted};
use p1_tool_finish::CompletionPolicy;
use p1_workers::{
    AgentFactory, ChildAgent, ChildId, ChildSpec, ChildStatus, InProcessWorkers, Regrant,
    WorkerReport,
};

use crate::HostDeps;
use crate::activity::{ActivityLog, ActivityTee, Completion, CompletionHub, WorkerReportTap};
use crate::cli::Options;
use crate::frontend::FrontEnd;
use crate::run::{
    FINISH_MODULE, MaskNoticeSink, agent_context, assemble_with_cache_key, config_for_route,
    finish_index, finish_tool, session_journals, stall_message, write_stderr,
};
#[cfg(feature = "shadow-hook")]
use crate::run::{ShadowJournal, ShadowOrigin};
use crate::session;

/// What `compose_children` hands back: the completion hub, the catalog slot, the
/// child builder, the worker service and the direct-child id counter.
#[cfg(feature = "delegation")]
type ComposedChildren = (
    Arc<CompletionHub>,
    Arc<OnceLock<Arc<Catalog>>>,
    Arc<ChildBuilder>,
    Arc<InProcessWorkers>,
    Arc<AtomicUsize>,
);

/// The ids already taken beside the session file, from every source that can hold
/// them: the `<session>.w<N>.jsonl` worker journals on disk and — when workflows are
/// composed in — the worker ids the session's run journals
/// (`<session>.workflows/wf*/journal.jsonl`) name. One function, because a direct
/// `worker_start` and a workflow step draw from one id namespace and must not
/// disagree about its first free id (issue #98). A source that cannot be read is a
/// failure, never evidence that no ids are reserved.
#[cfg(feature = "delegation")]
fn reserved_worker_ids(session: Option<&Path>) -> Result<usize, String> {
    let Some(session) = session else {
        return Ok(0);
    };
    let siblings = session::highest_worker_id(session).map_err(|error| {
        format!(
            "cannot reserve worker ids beside {}: {error}",
            session.display()
        )
    })?;
    #[cfg(feature = "workflows")]
    let runs = session::highest_workflow_worker_id(session).map_err(|error| {
        format!(
            "cannot reserve worker ids from the workflow runs of {}: {error}",
            session.display()
        )
    })?;
    #[cfg(not(feature = "workflows"))]
    let runs = 0;
    Ok(siblings.max(runs))
}

/// Compose the child factory and worker service from one initial id reservation.
/// Both direct workers and workflow steps consume this same service, so neither
/// path may begin with an unexamined `w1` journal.
#[cfg(feature = "delegation")]
pub(crate) fn compose_children(
    deps: &mut HostDeps,
    workspace: &Path,
    front_end: Arc<dyn FrontEnd>,
    options: &Options,
    max_workers: usize,
) -> Result<ComposedChildren, String> {
    let catalog_slot: Arc<OnceLock<Arc<Catalog>>> = Arc::new(OnceLock::new());
    let service_slot: Arc<OnceLock<Arc<InProcessWorkers>>> = Arc::new(OnceLock::new());
    let reserved = reserved_worker_ids(options.session.as_deref())?;
    if reserved >= usize::MAX - 1 {
        return Err("worker id namespace is exhausted: no id can be allocated".into());
    }
    let child_counter = Arc::new(AtomicUsize::new(reserved));
    let child_completion_hub = Arc::new(CompletionHub::new());
    let child_builder = Arc::new(ChildBuilder::new(
        deps,
        workspace,
        front_end,
        catalog_slot.clone(),
        child_counter.clone(),
        Arc::new(AtomicUsize::new(1)),
        child_completion_hub.clone(),
        options.session.clone(),
        options.max_idle_summaries,
        service_slot.clone(),
    ));
    let service = InProcessWorkers::new(make_child_factory(child_builder.clone()), max_workers);
    service.reserve_ids(reserved);
    let _ = service_slot.set(service.clone());
    deps.worker_service = Some(service.clone());
    Ok((
        child_completion_hub,
        catalog_slot,
        child_builder,
        service,
        child_counter,
    ))
}

/// The completion policy a CHILD's assembled tools call for (ADR-0052 item 1): a
/// worker whose tools include no tool that records command runs cannot verify
/// anything itself, so it reports to its parent instead of naming a command it cannot
/// run. The check is the `records-command-evidence` capability of the tools' verified
/// IDENTITIES (`catalog/capabilities.rs`) — never a grant name or an implementation
/// name, so a face cannot hide the shell tool.
///
/// MAIN agents never come through here: their `finish` keeps the strict rule.
#[cfg(feature = "delegation")]
fn completion_policy(tools: &[Arc<dyn Tool>]) -> CompletionPolicy {
    let can_run_commands = tools.iter().any(|tool| {
        super::capabilities::carries(
            tool.as_ref(),
            super::capabilities::SemanticCapability::RecordsCommandEvidence,
        )
    });
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
    mask: &Arc<MaskCounter>,
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
        mask,
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
    mask: &Arc<MaskCounter>,
) -> Arc<dyn Tool> {
    let name = finish.declaration().name.clone();
    let variant = finish.identity().variant.clone();
    let mut tool = p1_tool_finish::FinishTool::new(log, outcome).with_policy(policy);
    if let Some(contract) = contract {
        tool = tool.with_output_contract(contract);
    }
    let face = p1_tool_finish::ToolFace::new(name, tool.declaration().description.clone());
    // Issue #142: this `finish` is assembled after the general wrapping pass, so it
    // is wrapped here too; the marker keeps the face and identity above.
    redacted(
        Arc::new(tool.with_face(face, &variant)) as Arc<dyn Tool>,
        mask,
    )
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

/// Workers live in the process that started them: their sessions are in memory and
/// are NOT restored with the parent's (ADR-0034). A resumed history that mentions
/// workers is therefore talking about agents that no longer exist. Say so — to the
/// user and, through the inbox, to the model — and keep their ids from being reused.
#[cfg(feature = "delegation")]
pub(crate) fn announce_lost_workers(
    deps: &HostDeps,
    agent: &Agent,
    service: &InProcessWorkers,
    child_counter: &AtomicUsize,
    session_file: Option<&Path>,
    records: &[p1_contracts::JournalRecord],
) -> Result<(), String> {
    let earlier = p1_tool_delegate::workers_started_in(records);
    // `workers_started_in` reads only the delegate tool's own results, so workers a
    // WORKFLOW started are missing from it. Their run journals name them, and the
    // step's own `<session>.w<N>.jsonl` file may be gone or still there; every source
    // is bound below BEFORE the message can return early (issue #98).
    let reserved = reserved_worker_ids(session_file)?;
    let used = earlier
        .iter()
        .filter_map(|id| id.strip_prefix('w')?.parse::<usize>().ok())
        .max()
        .unwrap_or(earlier.len())
        .max(reserved);
    if used >= usize::MAX - 1 {
        return Err("worker id namespace is exhausted: no id can be allocated".into());
    }
    service.reserve_ids(used);
    child_counter.store(used, Ordering::SeqCst);
    if earlier.is_empty() {
        return Ok(());
    }
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
    Ok(())
}

#[cfg(feature = "delegation")]
pub(crate) async fn running_children(deps: &HostDeps) -> usize {
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
/// where the route takes a key. The child's profile comes back with the assembly: it
/// is not otherwise reachable from an `Assembled`, and the child's summarizer needs
/// its effort floor (#125).
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
    mask: &Arc<MaskCounter>,
) -> Result<(Assembled, Option<Arc<ModelProfile>>), String> {
    let mut environment =
        load_environment(environment_name, environment_dirs).map_err(|error| error.to_string())?;
    environment.tools = child_tools(&environment, grant)?;
    // A selected profile must be in place before the route binding resolves it to
    // the wire model, exactly as the parent's selection is applied.
    if let Some(choice) = choice {
        crate::models::apply(&mut environment, choice, environment_dirs)?;
    }
    crate::catalog::resolve_environment(&mut environment, environment_dirs)?;
    let profile = environment.profile.clone();
    let assembled = assemble_with_cache_key(
        catalog,
        &environment,
        workspace,
        substitutions,
        ordinal,
        mask,
    )?;
    Ok((assembled, profile))
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
    #[cfg(feature = "shadow-hook")]
    shadow: Option<Arc<p1_hook_shadow::ShadowHook>>,
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
            #[cfg(feature = "shadow-hook")]
            shadow: deps.shadow.clone(),
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
        // Issue #142: the child's own mask counter, shared by its assembled tools and
        // by its notice sink below (a child is its own agent).
        let mask = Arc::new(MaskCounter::new());
        let (mut assembled, child_profile) = assemble_child(
            environment_dirs,
            &catalog,
            environment,
            choice,
            grant,
            &workspace,
            &substitutions,
            ordinal,
            &mask,
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
            apply_completion_policy(&mut assembled, completion, contract.clone(), &mask);
        }
        let context = agent_context(&assembled, child_profile.as_deref())?;
        let route = assembled.resolved.route.origin.route.clone();
        let model = assembled.resolved.route.origin.model.clone();
        let description = format!("{route}/{model}");

        // The front end builds the labelled child sink; under delegation it also
        // feeds the run's worker-usage aggregate.
        let renderer = front_end.child_event_sink(&worker_id, &route, &model);
        let worker_window = assembled
            .resolved
            .context
            .as_ref()
            .map(|settings| config_for_route(settings, child_profile.as_deref()).window_tokens);
        front_end.worker_context_configured(&worker_id, worker_window);
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
        // Issue #142: a worker's masked values are reported once per turn through the
        // same display-only notice path, tagged with its own id by its renderer.
        let events: Arc<dyn EventSink> = Arc::new(MaskNoticeSink::new(events, mask.clone()));
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
            let mask = mask.clone();
            // The worker's OWN `finish` tool survives every re-grant: its activity
            // log is the worker's whole history, which `finish` reads to verify a
            // claim, and a freshly assembled one would see an empty session.
            let finish = finish_tool(&assembled);
            Arc::new(move |grant: &[String]| -> Result<Reconfiguration, String> {
                let (assembled, child_profile) = assemble_child(
                    &environment_dirs,
                    &catalog,
                    &environment_name,
                    choice.as_ref(),
                    grant,
                    &workspace,
                    &substitutions,
                    ordinal,
                    &mask,
                )?;
                // The catalog's `finish` factory issued THIS assembly its own
                // completion: take it, so the hub cannot hand a stale one to a later
                // worker assembly.
                let _issued = completion_hub.take();
                let context = agent_context(&assembled, child_profile.as_deref())?;
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
                        &mask,
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
                    authorization: None,
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
        #[cfg(feature = "shadow-hook")]
        let journal: Arc<dyn CommitSink> = match &self.shadow {
            Some(hook) => Arc::new(ShadowJournal {
                inner: journal,
                hook: hook.clone(),
                workspace: workspace.clone(),
                journal: created_file
                    .clone()
                    .unwrap_or_else(|| workspace.join(format!("p1-memory-{worker_id}"))),
                cache_key: Mutex::new(assembled.options.cache_key.clone()),
                origin: ShadowOrigin::Child {
                    family: "worker".to_string(),
                    provider: choice
                        .and_then(|choice| choice.profile.clone())
                        .or_else(|| {
                            load_environment(environment, environment_dirs)
                                .ok()
                                .and_then(|env| env.profile.map(|profile| profile.id.clone()))
                        })
                        .unwrap_or_else(|| "p1".to_string()),
                },
            }),
            None => journal,
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
        self.counter
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |current| {
                current.checked_add(1)
            })
            .map_err(|_| "worker id namespace is exhausted: no id can be allocated")?;
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
        let next = builder
            .counter
            .load(Ordering::SeqCst)
            .checked_add(1)
            .ok_or_else(|| {
                "worker id namespace is exhausted: no id can be allocated".to_string()
            })?;
        let worker_id = format!("w{next}");
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

/// The next worker ordinal: 1, 2, … in start order, so each worker gets its own
/// key while every worker of a given start order keeps it across processes.
#[cfg(feature = "delegation")]
fn next_agent_ordinal(ordinals: &AtomicUsize) -> u64 {
    ordinals.fetch_add(1, Ordering::Relaxed) as u64
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::run::{PARENT_ORDINAL, generated_cache_key};

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
