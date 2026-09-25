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

/// The highest worker id any workflow run of `session` journalled, `0` when the
/// session has no workflow runs (issue #98).
///
/// A workflow step is a worker, but only the run's own journal names it: the parent
/// session holds no `worker_start` for it, and its `<session>.w<N>.jsonl` file may
/// already be gone. On resume that journal is therefore the ONLY witness of ids the
/// namespace must not hand out again — an unreadable or malformed journal is an
/// error, never evidence that no ids are used. Only a run directory (`wf*`) with
/// its `journal.jsonl` is read; anything else beside the session is not a run.
#[cfg(feature = "workflows")]
pub fn highest_workflow_worker_id(session: &Path) -> io::Result<usize> {
    let mut root = session.as_os_str().to_os_string();
    root.push(".workflows");
    let runs = match std::fs::read_dir(PathBuf::from(root)) {
        Ok(runs) => runs,
        // No workflow directory: this session never ran a workflow.
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(0),
        Err(error) => return Err(error),
    };
    let mut highest = 0;
    for run in runs {
        let run = run?;
        let name = run.file_name();
        // Run directories are `wf<N>` (the service's own allocation); anything else
        // in the run root is not this session's run journal.
        if !name.to_str().is_some_and(|name| name.starts_with("wf")) || !run.file_type()?.is_dir() {
            continue;
        }
        let journal = run.path().join("journal.jsonl");
        if !journal.is_file() {
            continue;
        }
        for record in p1_workflow::read_journal(&journal)
            .map_err(|message| io::Error::new(io::ErrorKind::InvalidData, message))?
        {
            if let p1_workflow::JournalRecord::Result { envelope, .. } = record
                && let Some(id) = envelope.worker.as_deref().and_then(workflow_worker_id)
            {
                highest = highest.max(id);
            }
        }
    }
    Ok(highest)
}

/// The `<N>` a step envelope's worker line names: `w7 (route/model)` names worker 7.
/// `None` for every other shape — a step cancelled before its worker exists names `-`,
/// and the id, not the description, is what must never be reused.
#[cfg(feature = "workflows")]
fn workflow_worker_id(worker: &str) -> Option<usize> {
    worker
        .split_whitespace()
        .next()?
        .strip_prefix('w')?
        .parse()
        .ok()
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
    use super::{highest_worker_id, worker_path};
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
        // The exact sibling name, built by the host's own path rule: highest_worker_id
        // ignores anything else, so a hand-made name would test nothing.
        fs::write(worker_path(&session, usize::MAX), "").unwrap();
        let error = highest_worker_id(&session).expect_err("the namespace is exhausted");
        assert!(error.to_string().contains("no generatable worker id"));
    }

    #[cfg(feature = "workflows")]
    mod workflow_journals {
        use crate::session::{highest_workflow_worker_id, worker_path};
        use p1_workflow::{CallId, JournalRecord, SchemaCheck, StepEnvelope, StepStatus};
        use std::fs;
        use tempfile::tempdir;

        /// A `Result` line as the workflow engine writes it: the only journal record
        /// that names the worker that ran the step (`"<id> (<route/model>)"`).
        fn result_line(worker: Option<&str>) -> String {
            let envelope = StepEnvelope {
                step: CallId("call".into()),
                label: None,
                status: StepStatus::Done,
                value: serde_json::Value::Null,
                schema: SchemaCheck::NotRequested,
                evidence: None,
                attempts: 1,
                worker: worker.map(str::to_string),
                needs: None,
                error: None,
                models: Vec::new(),
                worktree: None,
            };
            let record = JournalRecord::Result {
                call: CallId("call".into()),
                envelope,
            };
            let mut line = serde_json::to_string(&record).unwrap();
            line.push('\n');
            line
        }

        /// A run directory `wf<N>` with the given `journal.jsonl` content.
        fn run(session: &std::path::Path, name: &str, journal: &str) {
            let mut root = session.as_os_str().to_os_string();
            root.push(".workflows");
            let run_dir = std::path::PathBuf::from(root).join(name);
            fs::create_dir_all(&run_dir).unwrap();
            fs::write(run_dir.join("journal.jsonl"), journal).unwrap();
        }

        #[test]
        fn a_session_without_workflow_runs_reserves_nothing() {
            let directory = tempdir().unwrap();
            let session = directory.path().join("session.jsonl");
            assert_eq!(highest_workflow_worker_id(&session).unwrap(), 0);
        }

        #[test]
        fn a_workflow_journal_naming_w7_is_reserved_even_without_a_w7_file() {
            let directory = tempdir().unwrap();
            let session = directory.path().join("session.jsonl");
            // The step's own journal file is gone: the run journal is the only witness.
            assert!(!worker_path(&session, 7).exists());
            run(&session, "wf1", &result_line(Some("w7 (route/model)")));
            assert_eq!(highest_workflow_worker_id(&session).unwrap(), 7);
        }

        #[test]
        fn only_worker_ids_are_reserved_and_only_from_run_journals() {
            let directory = tempdir().unwrap();
            let session = directory.path().join("session.jsonl");
            // A cancelled step names no worker; a later run names w3.
            run(
                &session,
                "wf1",
                &format!(
                    "{}{}",
                    result_line(None),
                    result_line(Some("- (not started)"))
                ),
            );
            run(&session, "wf2", &result_line(Some("w3 (route/model)")));
            // Not a run directory: never read.
            run(
                &session,
                "not-a-run",
                &result_line(Some("w9 (route/model)")),
            );
            assert_eq!(highest_workflow_worker_id(&session).unwrap(), 3);
        }

        #[test]
        fn a_malformed_workflow_journal_is_an_error_never_zero() {
            let directory = tempdir().unwrap();
            let session = directory.path().join("session.jsonl");
            run(&session, "wf1", "{\"kind\":\"no-such-record\"}\n");
            let error = highest_workflow_worker_id(&session)
                .expect_err("a journal that cannot be parsed is a safety failure");
            assert!(
                error.to_string().contains("journal.jsonl"),
                "the message names the journal: {error}"
            );
        }
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
