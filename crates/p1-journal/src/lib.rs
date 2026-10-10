//! Session journal stores: in-memory and JSONL commit sinks.
//!
//! Contract: `docs/design/journal.md` and `p1_contracts::journal`. The core owns
//! record ordering and asks a [`CommitSink`] to make each record durable at a
//! meaningful boundary; a store only writes bytes and reads them back.
//!
//! Format: one `serde_json` [`JournalRecord`] per `\n`-terminated line, preceded by
//! the header line `{"p1_journal":3}` (new files), `{"p1_journal":2}` or
//! `{"p1_journal":1}` (files written by earlier builds, still read and appended to
//! in their own format). Version 3 stamps each record with `at_ms`; versions 2 and 3
//! permit assembly identity lines `{"assembly":{...}}` between records. Every commit
//! is ONE `write_all` of a complete line, so a crash leaves at most one partial last
//! line — which [`load`] reports as a [`TruncatedTail`] instead of guessing.
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

use p1_contracts::{BoxFuture, Clock, CommitError, CommitSink, JournalRecord, SystemClock};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The header every new file gets.
const HEADER_LINE: &[u8] = b"{\"p1_journal\":3}\n";
/// The version new files are written in; the only one that carries `at_ms` on its
/// records.
pub const JOURNAL_VERSION: u64 = 3;
/// Version 3 (ADR-0121): records carry `at_ms`, and the core may commit
/// `RequestTiming`. Supersedes version 2's `{"p1_journal":2}` as what `create`
/// writes; version 2 and version 1 files stay read and appended to in their own
/// format.
/// The version that first carried assembly identity lines.
pub const JOURNAL_VERSION_2: u64 = 2;
/// The format of the released binaries. Still read, and appended to without
/// rewriting its header, so an old session stays readable by the old binaries.
pub const JOURNAL_VERSION_1: u64 = 1;

/// What executed the records that follow it: the environment, the host binary and
/// every assembled module, plus prompt-data provenance. New optional provenance
/// fields default empty when loading older journals.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AssemblyIdentity {
    pub environment: String,
    pub host: HostIdentity,
    pub modules: Vec<ModuleIdentity>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub instructions: Vec<InstructionIdentity>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub skills: Vec<SkillIdentity>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InstructionIdentity {
    pub path: PathBuf,
    /// SHA-256 of the exact loaded UTF-8 text, including a truncated prefix.
    pub sha256: String,
    pub bytes: u64,
    pub loaded_bytes: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SkillIdentity {
    pub name: String,
    pub path: PathBuf,
}

/// The p1 binary that assembled the session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HostIdentity {
    pub version: String,
    pub commit: String,
}

/// One assembled module.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModuleIdentity {
    pub name: String,
    pub kind: ModuleKind,
    pub package: String,
    pub version: String,
    /// sha256 hex of the package bytes the loader verified; `None` for a native
    /// module, whose code is identified by the host's commit instead.
    pub digest: Option<String>,
    pub abi: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModuleKind {
    Tool,
    Provider,
    ContextPolicy,
    AuthorizationPolicy,
}

/// An assembly identity as found in a journal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AssemblyEntry {
    /// The seq of the first record after the line; equal to the record count when
    /// the line follows the last record.
    pub from_seq: u64,
    pub identity: AssemblyIdentity,
}

/// The on-disk shape of an assembly line: the one key tells it apart from a
/// record, and `deny_unknown_fields` keeps anything else from passing as one.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AssemblyLine {
    assembly: AssemblyIdentity,
}

#[derive(Serialize)]
struct AssemblyLineRef<'a> {
    assembly: &'a AssemblyIdentity,
}

/// A version-3 record line: the record's own fields, plus `at_ms` beside them. The
/// `JournalRecord` type itself is unchanged, so every record literal and reader
/// stays as it is; a reader that wants the time reads `at_ms` off the line.
#[derive(Serialize)]
struct TimedRecord<'a> {
    #[serde(flatten)]
    record: &'a JournalRecord,
    at_ms: u64,
}

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
    /// A version-1 file holds an assembly identity line (physical 1-based line).
    /// Version 1 has no such line kind, so the file was not written by a p1 that
    /// kept it at version 1: refuse rather than guess which format it is.
    #[error(
        "assembly identity line at line {line} in a version-1 journal; only version 2 carries assembly lines"
    )]
    AssemblyInVersion1 { line: u64 },
    /// `record_assembly` on a version-1 file: writing the line would make the file
    /// unreadable as version 1, and its header is never rewritten.
    #[error(
        "cannot record an assembly identity in a version-1 journal; only version 2 carries assembly lines"
    )]
    AssemblyNeedsVersion2,
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
    /// Hash of observed tail bytes; equal-length replacements are stale.
    pub fingerprint: u64,
}

/// What [`load`] found: every complete valid record, plus a truncated tail if the
/// file ended mid-line.
/// What [`JsonlJournal::resume`] found under the lock.
#[derive(Debug, Clone, PartialEq)]
pub struct Resumed {
    pub records: Vec<JournalRecord>,
    /// The incomplete last record that was cut off, if there was one.
    pub repaired_tail: Option<TruncatedTail>,
    /// The header version the writer continues in (see [`Loaded::version`]).
    pub version: u64,
    pub assemblies: Vec<AssemblyEntry>,
    /// Parallel to `records`: the `at_ms` each record line carried, `None` for a
    /// line written before version 3 (ADR-0121).
    pub at_ms: Vec<Option<u64>>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Loaded {
    pub records: Vec<JournalRecord>,
    pub truncated_tail: Option<TruncatedTail>,
    /// The header's version. A header torn by a crash reports [`JOURNAL_VERSION`],
    /// the version the file is re-headed with once the tail is repaired.
    pub version: u64,
    /// Every assembly identity line, in file order.
    pub assemblies: Vec<AssemblyEntry>,
    /// Parallel to `records`: the `at_ms` each record line carried, `None` for a
    /// line written before version 3 (ADR-0121).
    pub at_ms: Vec<Option<u64>>,
}

// ------------------------------------------------------------------ memory

#[derive(Debug, Default)]
struct MemoryInner {
    records: Vec<JournalRecord>,
    assemblies: Vec<AssemblyEntry>,
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
            inner: Arc::new(Mutex::new(MemoryInner {
                records,
                assemblies: Vec::new(),
                next_seq,
            })),
        })
    }

    /// A snapshot of every record committed so far, in order.
    pub fn records(&self) -> Vec<JournalRecord> {
        self.inner.lock().unwrap().records.clone()
    }

    /// Record the assembly that executes the records committed after this call,
    /// exactly as [`JsonlJournal::record_assembly`] does for a version-2 file.
    pub fn record_assembly(&self, identity: &AssemblyIdentity) -> Result<(), JournalError> {
        let mut inner = self.inner.lock().unwrap();
        let from_seq = inner.next_seq;
        inner.assemblies.push(AssemblyEntry {
            from_seq,
            identity: identity.clone(),
        });
        Ok(())
    }

    /// A snapshot of every assembly identity recorded so far, in order.
    pub fn assemblies(&self) -> Vec<AssemblyEntry> {
        self.inner.lock().unwrap().assemblies.clone()
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
    /// The file's header version; decides whether assembly lines and `at_ms` may be
    /// written.
    version: u64,
    /// Stamps `at_ms` into every version-3 record this store commits. System by
    /// default; a fake one in tests ([`JsonlJournal::set_clock`]).
    clock: Arc<dyn Clock>,
    poisoned: bool,
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
            version: JOURNAL_VERSION,
            clock: Arc::new(SystemClock),
            poisoned: false,
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
        read_bounded(&mut file, &mut bytes)?;
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
            version: loaded.version,
            clock: Arc::new(SystemClock),
            poisoned: false,
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
                version: loaded.version,
                assemblies: loaded.assemblies,
                at_ms: loaded.at_ms,
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
        read_bounded(&mut file, &mut bytes)?;
        let mut version = JOURNAL_VERSION;
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
            version = loaded.version;
        }
        let mut inner = JsonlInner {
            file,
            path: path.to_path_buf(),
            sync,
            next_seq,
            version,
            clock: Arc::new(SystemClock),
            poisoned: false,
        };
        if bytes.is_empty() {
            write_header(&mut inner)?;
        }
        Ok(Self {
            inner: Arc::new(Mutex::new(inner)),
        })
    }

    /// Record the assembly that executes the records committed after this call.
    /// Durable like a record commit under the store's [`SyncPolicy`]. Refused
    /// ([`JournalError::AssemblyNeedsVersion2`]) on a version-1 file, whose header
    /// is never rewritten; allowed on a version-2 or version-3 file.
    pub fn record_assembly(&self, identity: &AssemblyIdentity) -> Result<(), JournalError> {
        let mut inner = self.inner.lock().unwrap();
        if inner.version == JOURNAL_VERSION_1 {
            return Err(JournalError::AssemblyNeedsVersion2);
        }
        let line = serde_json::to_vec(&AssemblyLineRef { assembly: identity })
            .map_err(|error| JournalError::Io(error.to_string()))?;
        write_line(&mut inner, line)
    }

    /// Stamp every version-3 record this store commits from now on with `at_ms`
    /// from `clock`. The store starts with the [`SystemClock`]; a test supplies a
    /// fake one. Inert on a version-1 or version-2 file, which carries no `at_ms`.
    pub fn set_clock(&self, clock: Arc<dyn Clock>) {
        self.inner.lock().unwrap().clock = clock;
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

    /// Only a version-3 file carries `RequestTiming` (ADR-0121): a version-1 or
    /// version-2 file keeps its format, so the core commits none there.
    fn accepts_request_timing(&self) -> bool {
        self.inner.lock().unwrap().version == JOURNAL_VERSION
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
    // Version 3 stamps `at_ms` beside the record; versions 1 and 2 keep their
    // format, so appending to them writes no `at_ms`.
    let line = if inner.version == JOURNAL_VERSION {
        let at_ms = inner.clock.now_ms();
        serde_json::to_vec(&TimedRecord { record, at_ms })
    } else {
        serde_json::to_vec(record)
    }
    .map_err(|error| JournalError::Io(error.to_string()))?;
    write_line(&mut inner, line)?;
    inner.next_seq += 1;
    Ok(())
}

fn write_line(inner: &mut JsonlInner, mut line: Vec<u8>) -> Result<(), JournalError> {
    if inner.poisoned {
        return Err(JournalError::Io(
            "journal writer needs recovery after failed write".into(),
        ));
    }
    line.push(b'\n');
    // Invariant 8a: ONE `write_all` of the complete line. A crash can therefore
    // leave at most a partial last line, which `load` reports, never misreads.
    let result = inner.file.write_all(&line).and_then(|()| {
        if inner.sync == SyncPolicy::EveryRecord {
            inner.file.sync_data()
        } else {
            Ok(())
        }
    });
    if result.is_err() {
        inner.poisoned = true;
    }
    result.map_err(JournalError::from)
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

/// Bound admission before allocation, including sparse files and files grown after stat.
const MAX_JOURNAL_BYTES: u64 = 256 * 1024 * 1024;
fn read_bounded(file: &mut File, bytes: &mut Vec<u8>) -> Result<(), JournalError> {
    if file.metadata().map_err(JournalError::from)?.len() > MAX_JOURNAL_BYTES {
        return Err(JournalError::Io(
            "session journal exceeds size limit".into(),
        ));
    }
    let count = file
        .take(MAX_JOURNAL_BYTES + 1)
        .read_to_end(bytes)
        .map_err(JournalError::from)?;
    if count as u64 > MAX_JOURNAL_BYTES {
        return Err(JournalError::Io(
            "session journal exceeds size limit".into(),
        ));
    }
    Ok(())
}

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
    let fingerprint = bytes[start..]
        .iter()
        .fold(0xcbf2_9ce4_8422_2325u64, |hash, byte| {
            (hash ^ u64::from(*byte)).wrapping_mul(0x0000_0100_0000_01b3)
        });
    TruncatedTail {
        byte_offset: start as u64,
        bytes: (bytes.len() - start) as u64,
        fingerprint,
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
/// - a final line without `\n`, without valid JSON, or without a valid record or
///   assembly line → `truncated_tail` (`byte_offset` = where that line starts);
/// - an invalid line that is not the final one → `Corrupt{line}` (1-based);
/// - records whose `seq` is not dense from 0 → `Corrupt{line}`; assembly lines
///   carry no seq and do not count;
/// - an assembly line in a version-1 file → `AssemblyInVersion1{line}`;
/// - a zero-byte file or an invalid header → `Corrupt{line: 1}`;
/// - a header naming a version other than 1, 2 or 3 → `UnknownVersion`.
pub fn load(path: &Path) -> Result<Loaded, JournalError> {
    let mut file = File::open(path).map_err(JournalError::from)?;
    let mut bytes = Vec::new();
    read_bounded(&mut file, &mut bytes)?;
    parse_records(&bytes)
}

fn parse_records(bytes: &[u8]) -> Result<Loaded, JournalError> {
    if bytes.is_empty() {
        return Err(JournalError::Corrupt { line: 1 });
    }
    let mut loaded = Loaded {
        records: Vec::new(),
        truncated_tail: None,
        version: JOURNAL_VERSION,
        assemblies: Vec::new(),
        at_ms: Vec::new(),
    };
    let mut pos = match next_line(bytes, 0) {
        Line::Complete { start, end } => {
            loaded.version = parse_header(&bytes[start..end])?;
            end + 1
        }
        // A header torn by a crash is not a missing header: it is the tail.
        Line::Partial { start } => {
            loaded.truncated_tail = Some(tail_from(bytes, start));
            return Ok(loaded);
        }
        Line::None => unreachable!("a non-empty file always yields a first line"),
    };
    let mut line = 1u64;
    loop {
        match next_line(bytes, pos) {
            Line::None => break,
            Line::Partial { start } => {
                loaded.truncated_tail = Some(tail_from(bytes, start));
                return Ok(loaded);
            }
            Line::Complete { start, end } => {
                line += 1;
                let is_last = end + 1 == bytes.len();
                match parse_line(&bytes[start..end]) {
                    Some(Parsed::Record(record, at_ms)) => {
                        if record.seq != loaded.records.len() as u64 {
                            return Err(JournalError::Corrupt { line });
                        }
                        loaded.records.push(record);
                        loaded.at_ms.push(at_ms);
                    }
                    Some(Parsed::Assembly(identity)) => {
                        // A valid line of a kind version 1 lacks is not a torn tail:
                        // it is refused wherever it stands.
                        if loaded.version == JOURNAL_VERSION_1 {
                            return Err(JournalError::AssemblyInVersion1 { line });
                        }
                        loaded.assemblies.push(AssemblyEntry {
                            from_seq: loaded.records.len() as u64,
                            identity,
                        });
                    }
                    // A complete final line that is not a valid record is the tail,
                    // exactly like a torn one; only a non-final bad line is corrupt.
                    None if is_last => {
                        loaded.truncated_tail = Some(tail_from(bytes, start));
                        return Ok(loaded);
                    }
                    None => return Err(JournalError::Corrupt { line }),
                }
                pos = end + 1;
            }
        }
    }
    Ok(loaded)
}

enum Parsed {
    /// The record, and the `at_ms` its line carried (`None` for a line written
    /// before version 3).
    Record(JournalRecord, Option<u64>),
    Assembly(AssemblyIdentity),
}

fn parse_line(line: &[u8]) -> Option<Parsed> {
    let value: Value = serde_json::from_slice(line).ok()?;
    // A record line carries `seq` and a `record` tag; an assembly line carries only
    // `assembly`. Try the record first, as the file store always has, then read the
    // version-3 `at_ms` off the same value (a version-1/2 record line has none).
    if let Ok(record) = serde_json::from_value::<JournalRecord>(value.clone()) {
        let at_ms = value.get("at_ms").and_then(Value::as_u64);
        return Some(Parsed::Record(record, at_ms));
    }
    serde_json::from_value::<AssemblyLine>(value)
        .ok()
        .map(|parsed| Parsed::Assembly(parsed.assembly))
}

fn parse_header(line: &[u8]) -> Result<u64, JournalError> {
    let value: Value =
        serde_json::from_slice(line).map_err(|_| JournalError::Corrupt { line: 1 })?;
    match value.get("p1_journal").map(Value::as_u64) {
        None => Err(JournalError::Corrupt { line: 1 }),
        Some(Some(version))
            if version == JOURNAL_VERSION_1
                || version == JOURNAL_VERSION_2
                || version == JOURNAL_VERSION =>
        {
            Ok(version)
        }
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
    read_bounded(&mut file, &mut bytes)?;
    if parse_records(&bytes)?.truncated_tail.as_ref() != Some(tail) {
        return Err(JournalError::StaleTail);
    }
    file.set_len(tail.byte_offset).map_err(JournalError::from)?;
    file.sync_all().map_err(JournalError::from)?;
    Ok(())
}

#[cfg(all(test, target_os = "linux"))]
mod failure_tests {
    use super::*;

    #[test]
    fn failed_write_poison_blocks_later_sequences_and_assembly() {
        let file = OpenOptions::new().write(true).open("/dev/full").unwrap();
        let mut inner = JsonlInner {
            file,
            path: PathBuf::from("/dev/full"),
            sync: SyncPolicy::OsBuffered,
            next_seq: 0,
            version: JOURNAL_VERSION,
            clock: Arc::new(SystemClock),
            poisoned: false,
        };
        assert!(write_line(&mut inner, b"first".to_vec()).is_err());
        assert!(inner.poisoned);
        let error = write_line(&mut inner, b"second".to_vec()).unwrap_err();
        assert!(error.to_string().contains("needs recovery"));
        assert_eq!(inner.next_seq, 0);
    }
}
