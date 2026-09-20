//! Session journal stores: in-memory and JSONL commit sinks.
//!
//! Contract: `docs/design/journal.md` and `p1_contracts::journal`. The core owns
//! record ordering and asks a [`CommitSink`] to make each record durable at a
//! meaningful boundary; a store only writes bytes and reads them back.
//!
//! Format: one `serde_json` [`JournalRecord`] per `\n`-terminated line, preceded by
//! the header line `{"p1_journal":1}`. Every commit is ONE `write_all` of a complete
//! line, so a crash leaves at most one partial last line — which [`load`] reports as
//! a [`TruncatedTail`] instead of guessing.
//!
//! Invariants enforced here (see the tests):
//! - a record is either completely in the file or not at all once `commit` returns
//!   `Ok` (single `write_all` plus, for `EveryRecord`, `sync_data`);
//! - [`load`] never drops a complete valid line and never repairs silently;
//! - the file lock is taken with `std::sync::Mutex` only inside the blocking task
//!   that `commit` runs on (`spawn_blocking`), never across an `.await` — the
//!   `Send` requirement of `CommitSink`'s boxed future makes a held `MutexGuard`
//!   across an await a compile error;
//! - new session files are created with mode 0600.

use std::fs::{File, OpenOptions, TryLockError};
use std::io::{ErrorKind, Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use p1_contracts::{BoxFuture, CommitError, CommitSink, JournalRecord};
use serde_json::Value;

/// The one header this format version writes.
const HEADER_LINE: &[u8] = b"{\"p1_journal\":1}\n";
/// The only accepted header version.
const JOURNAL_VERSION: u64 = 1;

/// How durable a store promises to be when `commit` returns.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncPolicy {
    /// `commit` returns after `write` + `fsync` of the file (and, on `create`, of
    /// the parent directory). Survives power loss, subject to the drive.
    EveryRecord,
    /// `commit` returns after `write`. Survives a process crash, not a power loss.
    OsBuffered,
}

/// Everything that can go wrong in a journal store.
///
/// `Io` carries the message rather than an `io::Error` so the error stays
/// `Clone + PartialEq` for callers and tests.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum JournalError {
    #[error("journal record out of order: expected seq {expected}, got {got}")]
    OutOfOrder { expected: u64, got: u64 },
    /// Physical 1-based line number; line 1 is the header.
    #[error("corrupt journal at line {line}")]
    Corrupt { line: u64 },
    /// The header names a format version this build does not understand.
    #[error("unknown journal version; refusing to guess")]
    UnknownVersion,
    /// `create` refuses to overwrite an existing session file.
    #[error("journal file already exists")]
    AlreadyExists,
    /// Another writer holds the advisory lock on this session file.
    #[error("journal file is locked by another writer")]
    Locked,
    /// The file no longer ends in the truncated tail the caller observed: someone
    /// wrote to it since. Repairing from a stale observation would cut off records.
    #[error("the journal changed since its truncated tail was observed; load it again")]
    StaleTail,
    #[error("journal io error: {0}")]
    Io(String),
}

impl From<std::io::Error> for JournalError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error.to_string())
    }
}

/// The tail of a file that `load` could not read as a complete record line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TruncatedTail {
    /// Byte offset where the incomplete line starts.
    pub byte_offset: u64,
    /// Bytes from `byte_offset` to end of file (including a terminating `\n` when
    /// the line was complete but not a valid record).
    pub bytes: u64,
}

/// What [`load`] found: every complete valid record, plus a truncated tail if the
/// file ended mid-line.
/// What [`JsonlJournal::resume`] found under the lock.
#[derive(Debug, Clone, PartialEq)]
pub struct Resumed {
    pub records: Vec<JournalRecord>,
    /// The incomplete last record that was cut off, if there was one.
    pub repaired_tail: Option<TruncatedTail>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Loaded {
    pub records: Vec<JournalRecord>,
    pub truncated_tail: Option<TruncatedTail>,
}

// ------------------------------------------------------------------ memory

#[derive(Debug, Default)]
struct MemoryInner {
    records: Vec<JournalRecord>,
    next_seq: u64,
}

/// In-memory commit sink. `Clone` shares one record list, so a handle can be used
/// both as the agent's sink and as the store's read side.
#[derive(Debug, Clone, Default)]
pub struct MemoryJournal {
    inner: Arc<Mutex<MemoryInner>>,
}

impl MemoryJournal {
    pub fn new() -> Self {
        Self::default()
    }

    /// Seed a store from already-loaded records so a session can continue in memory.
    /// Rejects a sequence that is not dense from 0, like [`project`] does.
    pub fn from_records(records: Vec<JournalRecord>) -> Result<Self, JournalError> {
        for (index, record) in records.iter().enumerate() {
            if record.seq != index as u64 {
                return Err(JournalError::OutOfOrder {
                    expected: index as u64,
                    got: record.seq,
                });
            }
        }
        let next_seq = records.len() as u64;
        Ok(Self {
            inner: Arc::new(Mutex::new(MemoryInner { records, next_seq })),
        })
    }

    /// A snapshot of every record committed so far, in order.
    pub fn records(&self) -> Vec<JournalRecord> {
        self.inner.lock().unwrap().records.clone()
    }
}

impl CommitSink for MemoryJournal {
    fn commit<'a>(&'a self, record: &'a JournalRecord) -> BoxFuture<'a, Result<(), CommitError>> {
        Box::pin(async move {
            self.append(record.clone())
                .map_err(|error| CommitError(error.to_string()))
        })
    }
}

impl MemoryJournal {
    fn append(&self, record: JournalRecord) -> Result<(), JournalError> {
        let mut inner = self.inner.lock().unwrap();
        if record.seq != inner.next_seq {
            return Err(JournalError::OutOfOrder {
                expected: inner.next_seq,
                got: record.seq,
            });
        }
        inner.records.push(record);
        inner.next_seq += 1;
        Ok(())
    }
}

// ------------------------------------------------------------------ jsonl

#[derive(Debug)]
struct JsonlInner {
    // The open handle IS the advisory lock: `try_lock` is held until this file is
    // dropped with the store. `append(true)` sends every write to end of file.
    file: File,
    path: PathBuf,
    sync: SyncPolicy,
    next_seq: u64,
}

/// JSONL commit sink over one session file.
#[derive(Debug)]
pub struct JsonlJournal {
    inner: Arc<Mutex<JsonlInner>>,
}

impl JsonlJournal {
    /// Create a NEW session file. Fails with [`JournalError::AlreadyExists`] if the
    /// path exists, so a session is never silently truncated.
    pub fn create(path: &Path, sync: SyncPolicy) -> Result<Self, JournalError> {
        let file = OpenOptions::new()
            .append(true)
            .create_new(true)
            .mode(0o600)
            .open(path)
            .map_err(|error| match error.kind() {
                ErrorKind::AlreadyExists => JournalError::AlreadyExists,
                _ => JournalError::from(error),
            })?;
        lock_or_err(&file)?;
        let mut inner = JsonlInner {
            file,
            path: path.to_path_buf(),
            sync,
            next_seq: 0,
        };
        write_header(&mut inner)?;
        Ok(Self {
            inner: Arc::new(Mutex::new(inner)),
        })
    }

    /// Take over an existing session file: lock it FIRST, then read, validate, cut
    /// off a truncated tail and derive the next sequence number — all under that
    /// lock, which is held for the life of the returned writer. This is the resume
    /// path; nothing in it acts on an observation made before ownership.
    pub fn resume(path: &Path, sync: SyncPolicy) -> Result<(Self, Resumed), JournalError> {
        let mut file = OpenOptions::new()
            .read(true)
            .append(true)
            .open(path)
            .map_err(JournalError::from)?;
        lock_or_err(&file)?;
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes).map_err(JournalError::from)?;
        let loaded = parse_records(&bytes)?;
        if let Some(tail) = &loaded.truncated_tail {
            file.set_len(tail.byte_offset).map_err(JournalError::from)?;
            file.sync_all().map_err(JournalError::from)?;
        }
        let header_lost = loaded
            .truncated_tail
            .is_some_and(|tail| tail.byte_offset == 0);
        let mut inner = JsonlInner {
            file,
            path: path.to_path_buf(),
            sync,
            next_seq: loaded.records.len() as u64,
        };
        if header_lost {
            write_header(&mut inner)?;
        }
        Ok((
            Self {
                inner: Arc::new(Mutex::new(inner)),
            },
            Resumed {
                records: loaded.records,
                repaired_tail: loaded.truncated_tail,
            },
        ))
    }

    /// Open an existing session file for appending after [`load`] has validated it.
    /// Refuses (`Corrupt`) a file that still holds a truncated tail.
    ///
    /// A zero-byte file with `next_seq == 0` is accepted and gets a fresh header:
    /// that is what a `repair_truncated_tail` to offset 0 leaves behind when the cut
    /// fell inside the header line.
    pub fn open_for_append(
        path: &Path,
        sync: SyncPolicy,
        next_seq: u64,
    ) -> Result<Self, JournalError> {
        // Own the file BEFORE reading it: what is validated below must be what gets
        // appended to, and a second writer must fail here, not after a stale read.
        let mut file = OpenOptions::new()
            .read(true)
            .append(true)
            .open(path)
            .map_err(JournalError::from)?;
        lock_or_err(&file)?;
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes).map_err(JournalError::from)?;
        if bytes.is_empty() {
            if next_seq != 0 {
                return Err(JournalError::Corrupt { line: 1 });
            }
        } else {
            let loaded = parse_records(&bytes)?;
            if let Some(tail) = &loaded.truncated_tail {
                let line = complete_lines_before(&bytes, tail.byte_offset as usize) + 1;
                return Err(JournalError::Corrupt { line });
            }
            // The caller's number comes from an earlier, unlocked load. If the file
            // grew since, appending at that number would duplicate a sequence.
            let expected = loaded.records.len() as u64;
            if next_seq != expected {
                return Err(JournalError::OutOfOrder {
                    expected,
                    got: next_seq,
                });
            }
        }
        let mut inner = JsonlInner {
            file,
            path: path.to_path_buf(),
            sync,
            next_seq,
        };
        if bytes.is_empty() {
            write_header(&mut inner)?;
        }
        Ok(Self {
            inner: Arc::new(Mutex::new(inner)),
        })
    }
}

impl CommitSink for JsonlJournal {
    fn commit<'a>(&'a self, record: &'a JournalRecord) -> BoxFuture<'a, Result<(), CommitError>> {
        // Clone the shared handle and the record so the blocking task owns them;
        // the `std::sync::Mutex` is locked and released inside that task, never by
        // the async future (invariant 8c).
        let inner = self.inner.clone();
        let record = record.clone();
        Box::pin(async move {
            match tokio::task::spawn_blocking(move || append_blocking(&inner, &record)).await {
                Ok(Ok(())) => Ok(()),
                Ok(Err(error)) => Err(CommitError(error.to_string())),
                Err(join) => Err(CommitError(format!("journal writer task failed: {join}"))),
            }
        })
    }
}

fn append_blocking(inner: &Mutex<JsonlInner>, record: &JournalRecord) -> Result<(), JournalError> {
    let mut inner = inner.lock().unwrap();
    if record.seq != inner.next_seq {
        return Err(JournalError::OutOfOrder {
            expected: inner.next_seq,
            got: record.seq,
        });
    }
    let mut line =
        serde_json::to_vec(record).map_err(|error| JournalError::Io(error.to_string()))?;
    line.push(b'\n');
    // Invariant 8a: ONE `write_all` of the complete line. A crash can therefore
    // leave at most a partial last line, which `load` reports, never misreads.
    inner.file.write_all(&line).map_err(JournalError::from)?;
    if inner.sync == SyncPolicy::EveryRecord {
        inner.file.sync_data().map_err(JournalError::from)?;
    }
    inner.next_seq += 1;
    Ok(())
}

fn write_header(inner: &mut JsonlInner) -> Result<(), JournalError> {
    inner
        .file
        .write_all(HEADER_LINE)
        .map_err(JournalError::from)?;
    if inner.sync == SyncPolicy::EveryRecord {
        inner.file.sync_data().map_err(JournalError::from)?;
        // The directory entry has to be durable too, or the file can vanish.
        let parent = parent_dir(&inner.path);
        File::open(parent)
            .and_then(|dir| dir.sync_all())
            .map_err(JournalError::from)?;
    }
    Ok(())
}

fn parent_dir(path: &Path) -> &Path {
    match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => Path::new("."),
    }
}

fn lock_or_err(file: &File) -> Result<(), JournalError> {
    match file.try_lock() {
        Ok(()) => Ok(()),
        Err(TryLockError::WouldBlock) => Err(JournalError::Locked),
        Err(TryLockError::Error(error)) => Err(JournalError::from(error)),
    }
}

// ------------------------------------------------------------------ loading

enum Line {
    /// `start`..`end` excludes the terminating `\n`.
    Complete { start: usize, end: usize },
    /// Bytes from `start` to EOF, with no `\n`.
    Partial { start: usize },
    /// No bytes left.
    None,
}

fn next_line(bytes: &[u8], pos: usize) -> Line {
    if pos >= bytes.len() {
        return Line::None;
    }
    match bytes[pos..].iter().position(|byte| *byte == b'\n') {
        Some(rel) => Line::Complete {
            start: pos,
            end: pos + rel,
        },
        None => Line::Partial { start: pos },
    }
}

fn tail_from(bytes: &[u8], start: usize) -> TruncatedTail {
    TruncatedTail {
        byte_offset: start as u64,
        bytes: (bytes.len() - start) as u64,
    }
}

fn complete_lines_before(bytes: &[u8], offset: usize) -> u64 {
    bytes[..offset.min(bytes.len())]
        .iter()
        .filter(|byte| **byte == b'\n')
        .count() as u64
}

/// Read every complete valid record from `path`.
///
/// - a final line without `\n`, without valid JSON, or without a valid record →
///   `truncated_tail` (`byte_offset` = where that line starts);
/// - an invalid line that is not the final one → `Corrupt{line}` (1-based);
/// - records whose `seq` is not dense from 0 → `Corrupt{line}`;
/// - a zero-byte file or an invalid header → `Corrupt{line: 1}`;
/// - a header naming another version → `UnknownVersion`.
pub fn load(path: &Path) -> Result<Loaded, JournalError> {
    let bytes = std::fs::read(path).map_err(JournalError::from)?;
    parse_records(&bytes)
}

fn parse_records(bytes: &[u8]) -> Result<Loaded, JournalError> {
    if bytes.is_empty() {
        return Err(JournalError::Corrupt { line: 1 });
    }
    let mut pos = match next_line(bytes, 0) {
        Line::Complete { start, end } => {
            parse_header(&bytes[start..end])?;
            end + 1
        }
        // A header torn by a crash is not a missing header: it is the tail.
        Line::Partial { start } => {
            return Ok(Loaded {
                records: Vec::new(),
                truncated_tail: Some(tail_from(bytes, start)),
            });
        }
        Line::None => unreachable!("a non-empty file always yields a first line"),
    };
    let mut records = Vec::new();
    let mut line = 1u64;
    loop {
        match next_line(bytes, pos) {
            Line::None => break,
            Line::Partial { start } => {
                return Ok(Loaded {
                    records,
                    truncated_tail: Some(tail_from(bytes, start)),
                });
            }
            Line::Complete { start, end } => {
                line += 1;
                let is_last = end + 1 == bytes.len();
                match serde_json::from_slice::<JournalRecord>(&bytes[start..end]) {
                    Ok(record) => {
                        if record.seq != records.len() as u64 {
                            return Err(JournalError::Corrupt { line });
                        }
                        records.push(record);
                    }
                    // A complete final line that is not a valid record is the tail,
                    // exactly like a torn one; only a non-final bad line is corrupt.
                    Err(_) if is_last => {
                        return Ok(Loaded {
                            records,
                            truncated_tail: Some(tail_from(bytes, start)),
                        });
                    }
                    Err(_) => return Err(JournalError::Corrupt { line }),
                }
                pos = end + 1;
            }
        }
    }
    Ok(Loaded {
        records,
        truncated_tail: None,
    })
}

fn parse_header(line: &[u8]) -> Result<(), JournalError> {
    let value: Value =
        serde_json::from_slice(line).map_err(|_| JournalError::Corrupt { line: 1 })?;
    match value.get("p1_journal") {
        None => Err(JournalError::Corrupt { line: 1 }),
        Some(version) if version.as_u64() == Some(JOURNAL_VERSION) => Ok(()),
        Some(_) => Err(JournalError::UnknownVersion),
    }
}

/// Cut the file back to `tail.byte_offset` and fsync it.
///
/// This is the caller's explicit decision to continue from a truncated session; it
/// never runs implicitly. If the cut lands at offset 0 (the whole file was the
/// incomplete header) the result is a zero-byte file, which
/// [`JsonlJournal::open_for_append`] can re-open and re-head.
pub fn repair_truncated_tail(path: &Path, tail: &TruncatedTail) -> Result<(), JournalError> {
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .map_err(JournalError::from)?;
    // Never cut a file an active writer owns, and never cut from an observation
    // that is no longer true: re-read under the lock and require the same tail.
    lock_or_err(&file)?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes).map_err(JournalError::from)?;
    if parse_records(&bytes)?.truncated_tail.as_ref() != Some(tail) {
        return Err(JournalError::StaleTail);
    }
    file.set_len(tail.byte_offset).map_err(JournalError::from)?;
    file.sync_all().map_err(JournalError::from)?;
    Ok(())
}
