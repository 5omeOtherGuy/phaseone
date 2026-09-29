//! The call read record: the digest of every whole file one tool read through the
//! `workspace` read side, keyed by its resolved path.
//!
//! A mutating component reads and computes outside the write gate, so another agent can
//! change the file between its read and its gated write. The mutation rechecks every
//! change under the gate against this record
//! (`docs/design/modules/workspace-mutation.md`, step 3), so a change computed from
//! contents that are no longer there is refused as stale instead of overwriting the
//! other writer. `p1-tool-patch`'s guest-parity harness implements the same check from
//! the same inputs.
//!
//! The record is deliberately not the agent's observation: a read through the `workspace`
//! side of an edit, write or patch component must not record what the agent has seen
//! (ADR-0025: only the read tool and a successful mutation do), and a search's reads must
//! not grant edit permission at all. The record is one tool instance's own state, so no
//! other tool and no other agent can obtain the identity from it.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};

use crate::observe::{hash_of, key};

/// A clonable, `Send + Sync` registry of what one tool read.
#[derive(Clone, Default)]
pub struct ReadRecord {
    reads: Arc<Mutex<Reads>>,
}

#[derive(Default)]
struct Reads {
    digests: HashMap<PathBuf, u64>,
    /// The file each requested spelling resolved to when it was read, so a mutation of
    /// the same spelling can refuse a symlink retargeted since (see [`ReadRecord::read_as`]).
    resolved: HashMap<PathBuf, PathBuf>,
}

impl ReadRecord {
    /// An empty record.
    pub fn new() -> Self {
        Self::default()
    }

    /// Record `path`'s whole-file `contents` as what this tool read.
    pub fn record(&self, path: &Path, contents: &[u8]) {
        self.record_hash(path, hash_of(contents));
    }

    /// Like [`Self::record`], but for a caller that already holds the content digest of
    /// the whole file (a snapshot's `content_hash`), so the record names the very same
    /// value the read returned instead of a second hash of the same bytes.
    pub fn record_hash(&self, path: &Path, hash: u64) {
        self.lock().digests.insert(key(path), hash);
    }

    /// Like [`Self::record_hash`], and remember that `spelling` (a request's
    /// [`Workspace::spelling`](crate::Workspace::spelling)) resolved to `path`.
    pub fn record_read(&self, spelling: &Path, path: &Path, hash: u64) {
        let mut reads = self.lock();
        let path = key(path);
        reads.resolved.insert(spelling.to_path_buf(), path.clone());
        reads.digests.insert(path, hash);
    }

    /// Forget a source removed or renamed by this same tool call.
    pub fn forget(&self, path: &Path) {
        let path = key(path);
        let mut reads = self.lock();
        reads.digests.remove(&path);
        reads.resolved.retain(|_, resolved| *resolved != path);
    }

    /// The digest recorded for `path`, `None` when this tool has not read it.
    pub fn recorded(&self, path: &Path) -> Option<u64> {
        self.lock().digests.get(&key(path)).copied()
    }

    /// The file `spelling` resolved to when this tool last read it, `None` when it has
    /// not read that spelling.
    pub fn read_as(&self, spelling: &Path) -> Option<PathBuf> {
        self.lock().resolved.get(spelling).cloned()
    }

    /// Recover from a poisoned lock: a panic elsewhere must not turn a read identity
    /// into a process abort.
    fn lock(&self) -> MutexGuard<'_, Reads> {
        self.reads
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

#[cfg(test)]
mod tests {
    use super::ReadRecord;
    use std::fs;

    #[test]
    fn the_latest_read_of_a_path_wins_and_an_unread_path_stays_unread() {
        let dir = tempfile::tempdir().unwrap();
        let read = dir.path().join("a.txt");
        let unread = dir.path().join("b.txt");
        fs::write(&read, b"one\n").unwrap();
        let record = ReadRecord::new();

        assert_eq!(record.recorded(&read), None, "nothing is read yet");
        assert_eq!(record.recorded(&unread), None);

        record.record(&read, b"one\n");
        let one = record.recorded(&read).expect("the read is recorded");

        record.record(&read, b"two\n");
        let two = record.recorded(&read).expect("the second read is recorded");
        assert_ne!(one, two, "other contents are another identity");

        record.record(&read, b"one\n");
        assert_eq!(
            record.recorded(&read),
            Some(one),
            "the same contents are the same identity again"
        );
        assert_eq!(
            record.recorded(&unread),
            None,
            "an unread path stays unread"
        );
    }

    #[test]
    fn clones_share_one_record() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.txt");
        let record = ReadRecord::new();
        let clone = record.clone();

        record.record(&path, b"contents");

        assert!(clone.recorded(&path).is_some(), "the clone sees the read");
        assert_eq!(clone.recorded(&path), record.recorded(&path));
    }

    #[test]
    fn read_record_is_send_and_sync() {
        fn assert_send_and_sync<T: Send + Sync>() {}
        assert_send_and_sync::<ReadRecord>();
    }
}
