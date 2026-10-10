//! Session-owned, agent-authored progress, independent of any front end.
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlanEntry {
    pub content: String,
    pub status: PlanStatus,
    pub priority: PlanPriority,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlanStatus {
    Pending,
    InProgress,
    Completed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlanPriority {
    Low,
    Medium,
    High,
}

/// A projection of committed session records, independent of tool presentation.
pub trait PlanSource: Send + Sync {
    /// Only committed records may enter; returns a replacement when one was observed.
    fn observe(&self, record: &crate::JournalRecord) -> Option<Vec<PlanEntry>>;
    /// None means no authored list yet; Some(empty) means explicitly cleared.
    fn snapshot(&self) -> Option<Vec<PlanEntry>>;
}
