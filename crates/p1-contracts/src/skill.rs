//! Prompt-data sources, independent of filesystem formats and tool implementations.
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillSummary {
    pub name: String,
    pub description: String,
    pub path: PathBuf,
    pub bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoadedSkill {
    pub body: String,
    pub directory: PathBuf,
    pub truncated: bool,
}

#[derive(Debug, Clone, Default)]
pub struct SkillListing {
    pub skills: Vec<SkillSummary>,
    pub warnings: Vec<String>,
}

#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct SkillError(pub String);

/// A source owns its snapshot/reload policy. Both methods are synchronous reads;
/// implementations must be safe to share across the agent's read-only calls.
pub trait SkillSource: Send + Sync {
    fn list(&self) -> SkillListing;
    fn load(&self, name: &str) -> Result<LoadedSkill, SkillError>;
}
