//! `_p1/workflow_update` v1. Data adapted from the read-only donor
//! `crates/p1-tui/src/workflow.rs` (ADR-0075), not its tree model or renderer.

use p1_contracts::frontend::WorkflowEvent as Observation;
use serde::{Deserialize, Serialize};

pub const CAPABILITY: &str = "workflow_update";
pub const METHOD: &str = "_p1/workflow_update";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
pub enum WorkflowEvent {
    RunStarted {
        id: String,
        resumed_from: Option<String>,
    },
    Phase {
        run: String,
        name: String,
    },
    Log {
        run: String,
        text: String,
    },
    JobsQueued {
        run: String,
        count: usize,
    },
    StepStarted {
        run: String,
        ordinal: u32,
        call: String,
        label: Option<String>,
        phase: Option<String>,
        role: String,
        model: String,
        worker_id: Option<String>,
        attempt: u32,
        prompt: String,
    },
    StepEnded {
        run: String,
        ordinal: u32,
        call: String,
        label: Option<String>,
        model: String,
        status: String,
        attempts: u32,
        replayed: bool,
        error: Option<String>,
        worker_id: Option<String>,
    },
    ThunkFailed {
        run: String,
        error: String,
    },
    RunEnded {
        id: String,
        outcome: String,
        error: Option<String>,
    },
}

impl From<Observation> for WorkflowEvent {
    fn from(event: Observation) -> Self {
        match event {
            Observation::RunStarted { id, resumed_from } => Self::RunStarted { id, resumed_from },
            Observation::Phase { run, name } => Self::Phase { run, name },
            Observation::Log { run, text } => Self::Log { run, text },
            Observation::JobsQueued { run, count } => Self::JobsQueued { run, count },
            Observation::StepStarted {
                run,
                ordinal,
                call,
                label,
                phase,
                role,
                model,
                worker_id,
                attempt,
                prompt,
            } => Self::StepStarted {
                run,
                ordinal,
                call,
                label,
                phase,
                role,
                model,
                worker_id,
                attempt,
                prompt,
            },
            Observation::StepEnded {
                run,
                ordinal,
                call,
                label,
                model,
                status,
                attempts,
                replayed,
                error,
                worker_id,
            } => Self::StepEnded {
                run,
                ordinal,
                call,
                label,
                model,
                status,
                attempts,
                replayed,
                error,
                worker_id,
            },
            Observation::ThunkFailed { run, error } => Self::ThunkFailed { run, error },
            Observation::RunEnded { id, outcome, error } => Self::RunEnded { id, outcome, error },
        }
    }
}
