//! Delegation on a main agent: the worker and workflow tools every main agent is
//! given, and what a resumed session says about workers and runs that belonged to
//! the earlier process.

#[cfg(feature = "delegation")]
use std::path::Path;
#[cfg(feature = "delegation")]
use std::sync::atomic::{AtomicUsize, Ordering};

use p1_assembly::EnvironmentFile;
#[cfg(feature = "delegation")]
use p1_assembly::ToolSpec;
#[cfg(feature = "delegation")]
use p1_core::Agent;
#[cfg(feature = "delegation")]
use p1_workers::InProcessWorkers;

#[cfg(feature = "delegation")]
use crate::HostDeps;
#[cfg(feature = "delegation")]
use crate::child_assembly::reserved_worker_ids;
#[cfg(feature = "workflows")]
use crate::cli::Options;
#[cfg(feature = "delegation")]
use crate::run::write_stderr;

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
pub(crate) fn with_worker_tools(environment: &mut EnvironmentFile) {
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
pub(crate) fn with_worker_tools(_environment: &mut EnvironmentFile) {}

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

/// Workflow runs, like workers, live in the process that started them: the new
/// service knows none of the earlier runs. Their journals stay on disk, which is what
/// `resume_from` replays, so the user is told once where they are.
#[cfg(feature = "workflows")]
pub(crate) fn announce_lost_runs(deps: &HostDeps, options: &Options) {
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
