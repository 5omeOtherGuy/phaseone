//! Which files one agent has seen, and in what state.
//!
//! Shared by the read/edit/write tools of a single agent (cloned handles share
//! one store). Staleness is decided by content, not mtime, so a benign touch
//! does not invalidate an observation and a changed file does.
//!
//! Contents are compared by a non-cryptographic `std` hash. This guards against
//! accidental stale writes, not against a hostile filesystem, so a collision is
//! acceptable and no hashing dependency is needed.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};

/// What [`ObservedFiles::check_unchanged`] found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Observation {
    /// This agent has never read or written this path.
    NeverObserved,
    /// The contents still match the last observed state.
    Unchanged,
    /// The contents changed on disk since this agent last saw them.
    ChangedSinceObserved,
}

/// A clonable, `Send + Sync` registry of observed file contents.
#[derive(Clone, Default)]
pub struct ObservedFiles {
    seen: Arc<Mutex<HashMap<PathBuf, u64>>>,
}

impl ObservedFiles {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record `path`'s current `contents`, after a successful read or mutation.
    pub fn record(&self, path: &Path, contents: &[u8]) {
        let hash = hash_of(contents);
        self.lock().insert(key(path), hash);
    }

    /// Compare `current` against the last observation of `path`.
    pub fn check_unchanged(&self, path: &Path, current: &[u8]) -> Observation {
        let hash = hash_of(current);
        match self.lock().get(&key(path)) {
            None => Observation::NeverObserved,
            Some(observed) if *observed == hash => Observation::Unchanged,
            Some(_) => Observation::ChangedSinceObserved,
        }
    }

    /// Like [`Self::record`], but the content hash was built incrementally
    /// with [`StreamingHash`] instead of from a fully materialized byte
    /// slice, for a caller that must not hold an entire file in memory just
    /// to observe it (e.g. a windowed read of a huge file).
    pub fn record_streamed(&self, path: &Path, hash: StreamingHash) {
        self.lock().insert(key(path), hash.finish());
    }

    /// Recover the map from a poisoned lock rather than panicking: a panic in
    /// another thread must not turn a stale-file check into a process abort.
    fn lock(&self) -> MutexGuard<'_, HashMap<PathBuf, u64>> {
        self.seen
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

fn key(path: &Path) -> PathBuf {
    // Canonicalize so a read and a later mutation agree on the key even when
    // one arrived through a symlink; fall back for a path that does not exist.
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

fn hash_of(contents: &[u8]) -> u64 {
    let mut hash = StreamingHash::new();
    hash.update(contents);
    hash.finish()
}

/// A content hash built incrementally, chunk by chunk, for a caller that must not
/// hold a whole file in memory just to observe it. FNV-1a over the bytes, then the
/// length: a byte-at-a-time fold is independent of how the input is chunked BY
/// CONSTRUCTION — `std`'s `Hasher` does not promise that `write(a); write(b)` equals
/// `write(ab)`, and a silent difference would make every streamed read look stale.
pub struct StreamingHash {
    state: u64,
    len: u64,
}

impl StreamingHash {
    pub fn new() -> Self {
        Self {
            state: 0xcbf2_9ce4_8422_2325,
            len: 0,
        }
    }

    /// Feed the next chunk of bytes, in file order.
    pub fn update(&mut self, chunk: &[u8]) {
        for byte in chunk {
            self.state = (self.state ^ u64::from(*byte)).wrapping_mul(0x0000_0100_0000_01b3);
        }
        self.len += chunk.len() as u64;
    }

    fn finish(self) -> u64 {
        (self.state ^ self.len).wrapping_mul(0x0000_0100_0000_01b3)
    }
}

impl Default for StreamingHash {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::{Observation, ObservedFiles};
    use std::fs;

    #[test]
    fn check_unchanged_tracks_observed_contents() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("file.txt");
        fs::write(&path, b"hello").unwrap();
        let observed = ObservedFiles::new();

        assert_eq!(
            observed.check_unchanged(&path, b"hello"),
            Observation::NeverObserved
        );

        observed.record(&path, b"hello");
        assert_eq!(
            observed.check_unchanged(&path, b"hello"),
            Observation::Unchanged
        );
        assert_eq!(
            observed.check_unchanged(&path, b"hello!"),
            Observation::ChangedSinceObserved
        );
    }

    #[test]
    fn record_overwrites_a_previous_observation() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("file.txt");
        fs::write(&path, b"one").unwrap();
        let observed = ObservedFiles::new();

        observed.record(&path, b"one");
        observed.record(&path, b"two");

        assert_eq!(
            observed.check_unchanged(&path, b"one"),
            Observation::ChangedSinceObserved
        );
        assert_eq!(
            observed.check_unchanged(&path, b"two"),
            Observation::Unchanged
        );
    }

    #[test]
    fn clones_share_one_observation_store() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("file.txt");
        fs::write(&path, b"contents").unwrap();
        let first = ObservedFiles::new();
        let second = first.clone();

        first.record(&path, b"contents");

        assert_eq!(
            second.check_unchanged(&path, b"contents"),
            Observation::Unchanged
        );
    }

    #[test]
    fn observed_files_is_send_and_sync() {
        fn assert_send_and_sync<T: Send + Sync>() {}
        assert_send_and_sync::<ObservedFiles>();
    }

    #[test]
    fn streamed_hash_matches_a_hash_of_the_whole_slice_in_any_chunking() {
        use super::StreamingHash;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("file.txt");
        let contents = b"alpha\nbeta\ngamma\n".repeat(37);
        fs::write(&path, &contents).unwrap();

        let whole = ObservedFiles::new();
        whole.record(&path, &contents);

        let mut hash = StreamingHash::new();
        for chunk in contents.chunks(7) {
            hash.update(chunk);
        }
        let streamed = ObservedFiles::new();
        streamed.record_streamed(&path, hash);

        // Both stores must agree that `contents` is unchanged: the streamed
        // hash is bit-for-bit the same as the one `record` computes from the
        // whole slice, regardless of how the bytes were chunked.
        assert_eq!(
            streamed.check_unchanged(&path, &contents),
            Observation::Unchanged
        );
        assert_eq!(
            whole.check_unchanged(&path, &contents),
            streamed.check_unchanged(&path, &contents)
        );
    }
}
