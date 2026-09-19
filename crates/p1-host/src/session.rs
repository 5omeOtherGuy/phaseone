//! Journal selection and resume support.
//!
//! No `--session` uses an in-memory store. `--session FILE` creates a JSONL store
//! (refusing to overwrite). `--resume` loads, repairs a truncated tail, and opens
//! the file for appending.

use std::path::Path;
use std::sync::Arc;

use p1_contracts::CommitSink;
use p1_journal::{JsonlJournal, Loaded, MemoryJournal, SyncPolicy, TruncatedTail};

/// A session store failure. The `Exists` text is the CLI contract.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionError {
    /// `--session` names a file that already exists and `--resume` was not given.
    Exists,
    /// Any journal store error, already formatted.
    Journal(String),
}

impl std::fmt::Display for SessionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Exists => write!(f, "session file exists; pass --resume to continue it"),
            Self::Journal(message) => write!(f, "{message}"),
        }
    }
}

impl std::error::Error for SessionError {}

impl From<p1_journal::JournalError> for SessionError {
    fn from(error: p1_journal::JournalError) -> Self {
        Self::Journal(error.to_string())
    }
}

/// A fresh in-memory session.
pub fn memory() -> Arc<MemoryJournal> {
    Arc::new(MemoryJournal::new())
}

/// Create a NEW JSONL session file. Refuses an existing path so a session is
/// never silently truncated.
pub fn create(path: &Path) -> Result<Arc<JsonlJournal>, SessionError> {
    if path.exists() {
        return Err(SessionError::Exists);
    }
    Ok(Arc::new(JsonlJournal::create(
        path,
        SyncPolicy::EveryRecord,
    )?))
}

/// Load every complete record, plus a truncated tail if the file ended mid-line.
pub fn load(path: &Path) -> Result<Loaded, SessionError> {
    Ok(p1_journal::load(path)?)
}

/// Cut a truncated tail off the file.
pub fn repair(path: &Path, tail: &TruncatedTail) -> Result<(), SessionError> {
    Ok(p1_journal::repair_truncated_tail(path, tail)?)
}

/// Open an existing, validated session file for appending at `next_seq`.
pub fn append(path: &Path, next_seq: u64) -> Result<Arc<JsonlJournal>, SessionError> {
    Ok(Arc::new(JsonlJournal::open_for_append(
        path,
        SyncPolicy::EveryRecord,
        next_seq,
    )?))
}

/// The `CommitSink` behind a JSONL store, as the agent wants it.
pub fn sink(store: &Arc<JsonlJournal>) -> Arc<dyn CommitSink> {
    store.clone()
}
