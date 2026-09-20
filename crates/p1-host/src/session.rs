//! Journal selection and resume support.
//!
//! No `--session` uses an in-memory store. `--session FILE` creates a JSONL store
//! (refusing to overwrite). `--resume` takes the file over under its writer lock:
//! load, cut off a truncated tail, continue appending.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use p1_contracts::CommitSink;
use p1_journal::{JsonlJournal, MemoryJournal, Resumed, SyncPolicy};

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

/// Take over an existing session file for `--resume`: lock, load, cut off a
/// truncated tail and continue — one step under the writer's lock, so a second
/// `p1` on the same file fails here without having touched it.
pub fn resume(path: &Path) -> Result<(Arc<JsonlJournal>, Resumed), SessionError> {
    let (store, resumed) = JsonlJournal::resume(path, SyncPolicy::EveryRecord)?;
    Ok((Arc::new(store), resumed))
}

/// The `CommitSink` behind a JSONL store, as the agent wants it.
pub fn sink(store: &Arc<JsonlJournal>) -> Arc<dyn CommitSink> {
    store.clone()
}

/// The path of worker `<id>`'s own session file: a sibling of the parent
/// `--session` file. `session.jsonl` gives `session.jsonl.w1.jsonl`.
pub fn worker_path(session: &Path, id: usize) -> PathBuf {
    let mut path = session.as_os_str().to_os_string();
    path.push(format!(".w{id}.jsonl"));
    PathBuf::from(path)
}

/// Create the NEW JSONL journal for worker `<id>` next to its parent session.
/// Same store and durability as [`create`], and the same refusal to overwrite: a
/// worker's file is never truncated or appended to. It bypasses [`create`]'s
/// `--resume` hint, which would be wrong advice for a worker's own file.
pub fn worker(session: &Path, id: usize) -> Result<Arc<dyn CommitSink>, SessionError> {
    let store = Arc::new(JsonlJournal::create(
        &worker_path(session, id),
        SyncPolicy::EveryRecord,
    )?);
    Ok(sink(&store))
}
