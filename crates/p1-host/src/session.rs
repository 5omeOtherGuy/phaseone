//! Journal selection and resume support.
//!
//! No `--session` uses an in-memory store. `--session FILE` creates a JSONL store
//! (refusing to overwrite). `--resume` takes the file over under its writer lock:
//! load, cut off a truncated tail, continue appending.

use std::ffi::OsStr;
use std::io;
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

/// The highest `<N>` among the worker journals already beside `session`, `0` when
/// there are none. A workflow step is a worker too, so its `FILE.w<N>.jsonl` exists
/// on disk without any delegation-tool record naming it: on resume this is what keeps
/// a new worker from being handed an id whose journal file is already there (issue
/// #98). Only an exact sibling name counts — anything else is ignored.
///
/// Failing to enumerate the directory is a safety failure, not evidence that no ids
/// are reserved. `usize::MAX` is rejected because the service would have no next id
/// to allocate. Both conditions must stop startup rather than weaken the guarantee.
pub fn highest_worker_id(session: &Path) -> io::Result<usize> {
    let directory = session
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let Some(name) = session.file_name() else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "session path has no file name",
        ));
    };
    let mut highest = 0;
    for entry in std::fs::read_dir(directory)? {
        let Some(id) = worker_id_in(name, &entry?.file_name())? else {
            continue;
        };
        if id >= usize::MAX - 1 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "worker journal leaves no generatable worker id; cannot continue",
            ));
        }
        highest = highest.max(id);
    }
    Ok(highest)
}

/// The `<N>` of a sibling named exactly `<session file name>.w<N>.jsonl`, `None` for
/// every other name. `<N>` is one or more ASCII digits and nothing else. Values
/// larger than `usize` cannot collide with a generatable id and are ignored.
fn worker_id_in(session_name: &OsStr, sibling: &OsStr) -> io::Result<Option<usize>> {
    #[cfg(unix)]
    fn parse_id(session_name: &OsStr, sibling: &OsStr) -> Option<usize> {
        use std::os::unix::ffi::OsStrExt;

        let sibling = sibling.as_bytes();
        let suffix = sibling
            .strip_prefix(session_name.as_bytes())?
            .strip_prefix(b".w")?
            .strip_suffix(b".jsonl")?;
        if suffix.is_empty() || !suffix.iter().all(u8::is_ascii_digit) {
            return None;
        }
        std::str::from_utf8(suffix).ok()?.parse().ok()
    }

    #[cfg(not(unix))]
    fn parse_id(session_name: &OsStr, sibling: &OsStr) -> Option<usize> {
        let digits = sibling
            .to_str()?
            .strip_prefix(session_name.to_str()?)?
            .strip_prefix(".w")?
            .strip_suffix(".jsonl")?;
        if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
            return None;
        }
        digits.parse().ok()
    }

    Ok(parse_id(session_name, sibling))
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

#[cfg(test)]
mod tests {
    use super::highest_worker_id;
    use std::ffi::OsString;
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn a_scan_failure_is_not_treated_as_no_reserved_ids() {
        let error = highest_worker_id(std::path::Path::new("/dev/null/session.jsonl"))
            .expect_err("a file cannot be enumerated as a session directory");
        assert_eq!(error.kind(), std::io::ErrorKind::NotADirectory);
    }

    #[test]
    fn a_usize_max_journal_is_a_clear_exhaustion_error() {
        let directory = tempdir().unwrap();
        let session = directory.path().join("session.jsonl");
        fs::write(format!("{}w{}.jsonl", session.display(), usize::MAX), "").unwrap();
        let error = highest_worker_id(&session).expect_err("the namespace is exhausted");
        assert!(error.to_string().contains("no generatable worker id"));
    }

    #[cfg(unix)]
    #[test]
    fn a_non_utf8_session_basename_is_still_discovered() {
        use std::os::unix::ffi::OsStringExt;

        let directory = tempdir().unwrap();
        let mut name = OsString::from("session");
        name.push(OsString::from_vec(vec![0xff]));
        let session = directory.path().join(name);
        let mut sibling = session.as_os_str().to_os_string();
        sibling.push(".w4.jsonl");
        fs::write(directory.path().join(sibling), "").unwrap();
        assert_eq!(highest_worker_id(&session).unwrap(), 4);
    }
}
