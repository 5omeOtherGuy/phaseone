//! Negotiated worker transcripts, sharing the parent's ordered output queue.

use serde::Serialize;

pub const CAPABILITY: &str = "worker_transcripts";
pub const METHOD: &str = "_p1/worker_update";

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Worker {
    pub id: String,
    pub route: String,
    pub model: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct WorkerUpdate {
    pub worker: String,
    pub state: &'static str,
    pub description: Option<String>,
    pub note: Option<String>,
}
