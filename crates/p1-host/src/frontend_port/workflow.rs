//! Rendered observations behind the neutral port. Existing end/phase/log wording
//! comes from `FrontEnd::workflow_line`; the structured-only callbacks add lines.

use p1_contracts::frontend::{FrontEndPort, WorkflowEvent, WorkflowProgress};

use crate::frontend::{
    WorkflowRunEnded, WorkflowRunStarted, WorkflowStepEnded, WorkflowStepStarted,
};

pub(super) fn line(port: &dyn FrontEndPort, line: &str) {
    // HostWorkflowObserver always prefixes rendered lines with this run identity.
    if let Some((run, _)) = line
        .strip_prefix("workflow ")
        .and_then(|text| text.split_once(' '))
    {
        // Log lines use `workflow <id>: ...`, unlike phase/end lines.
        emit(port, run.trim_end_matches(':'), line.to_string(), None);
    }
}

pub(super) fn started(port: &dyn FrontEndPort, run: &WorkflowRunStarted) {
    port.workflow_event(WorkflowEvent::RunStarted {
        id: run.id.clone(),
        resumed_from: run.resumed_from.clone(),
    });
    let resumed = run
        .resumed_from
        .as_ref()
        .map(|id| format!(", resuming {id}"))
        .unwrap_or_default();
    emit(
        port,
        &run.id,
        format!("workflow {} started{resumed}", run.id),
        None,
    );
}

pub(super) fn step_started(port: &dyn FrontEndPort, step: &WorkflowStepStarted) {
    port.workflow_event(WorkflowEvent::StepStarted {
        run: step.run.clone(),
        ordinal: step.ordinal,
        call: step.call.clone(),
        label: step.label.clone(),
        phase: step.phase.clone(),
        role: step.role.clone(),
        model: step.model.clone(),
        worker_id: step.worker_id.clone(),
        attempt: step.attempt,
        prompt: step.prompt.clone(),
    });
    let name = step.label.as_deref().unwrap_or(&step.call);
    let worker = step
        .worker_id
        .as_ref()
        .map(|id| format!("; {id}"))
        .unwrap_or_default();
    emit(
        port,
        &step.run,
        format!(
            "workflow {} {name} ({} → {}{worker}) running",
            step.run, step.role, step.model
        ),
        None,
    );
}

pub(super) fn jobs_queued(port: &dyn FrontEndPort, run: &str, count: usize) {
    port.workflow_event(WorkflowEvent::JobsQueued {
        run: run.to_string(),
        count,
    });
    emit(
        port,
        run,
        format!("workflow {run}: {count} jobs queued"),
        None,
    );
}

pub(super) fn thunk_failed(port: &dyn FrontEndPort, run: &str, error: &str) {
    port.workflow_event(WorkflowEvent::ThunkFailed {
        run: run.to_string(),
        error: error.to_string(),
    });
    emit(
        port,
        run,
        format!("workflow {run} thunk failed: {error}"),
        None,
    );
}

pub(super) fn ended(port: &dyn FrontEndPort, run: &WorkflowRunEnded) {
    port.workflow_event(WorkflowEvent::RunEnded {
        id: run.id.clone(),
        outcome: run.outcome.clone(),
        error: run.error.clone(),
    });
    // The rendered run summary was already forwarded. Finish before the bridge
    // releases workers and the run's hold.
    emit(port, &run.id, String::new(), Some(run.outcome.clone()));
}

pub(super) fn step_ended(port: &dyn FrontEndPort, step: &WorkflowStepEnded) {
    port.workflow_event(WorkflowEvent::StepEnded {
        run: step.run.clone(),
        ordinal: step.ordinal,
        call: step.call.clone(),
        label: step.label.clone(),
        model: step.model.clone(),
        status: step.status.clone(),
        attempts: step.attempts,
        replayed: step.replayed,
        error: step.error.clone(),
        worker_id: step.worker_id.clone(),
    });
}

fn emit(port: &dyn FrontEndPort, run: &str, line: String, outcome: Option<String>) {
    port.workflow_progress(&WorkflowProgress {
        run: run.to_string(),
        line,
        outcome,
    });
}
