//! Targeted cancellation replies for the negotiated p1.dev v1 fallback (#682).

use serde::Serialize;

pub(crate) const RUN_CAPABILITY: &str = "run_cancel";
pub(crate) const WORKER_CAPABILITY: &str = "worker_stop";

#[derive(Serialize)]
pub(crate) struct RunCancelResult {
    pub cancelled: bool,
}

#[derive(Serialize)]
pub(crate) struct WorkerStopResult {
    pub stopped: bool,
}
