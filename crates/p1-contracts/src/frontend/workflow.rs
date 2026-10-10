//! Structured observations behind the neutral front-end port. Data adapted from
//! `crates/p1-tui/src/workflow.rs` (ADR-0075); no renderer or protocol types.

/// A step's identity is run + ordinal, not call or worker id. A repeated start
/// updates that step (including a fallback's new worker). An end may have no start.
#[derive(Debug, Clone, PartialEq, Eq)]
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
        /// Script task text, not the worker's assembled prompt.
        prompt: String,
    },
    StepEnded {
        run: String,
        ordinal: u32,
        call: String,
        label: Option<String>,
        model: String,
        /// done, failed, blocked or cancelled.
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
        /// completed, completed_with_issues, failed or cancelled.
        outcome: String,
        error: Option<String>,
    },
}
