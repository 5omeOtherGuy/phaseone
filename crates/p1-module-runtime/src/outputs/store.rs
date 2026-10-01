//! The output store (ADR-0109 items 3, 4 and 7): one directory per run, one file per output,
//! written only by the host, and served only from what the store itself recorded.
//!
//! **Provenance.** A store serves a handle only when its own in-memory index holds it: the
//! index is filled by the store's writer when an output is complete, with the file's device,
//! inode, size and change time. `describe` and `page` open the file and compare those facts
//! with `fstat` of what they opened, so a file planted beside the outputs, one put in place of
//! an output (another inode) or one rewritten afterwards (another size or change time) is
//! `unknown-output`. Nothing on disk can make the store serve a file: the index is never read
//! back. So an output does not outlive the process that stored it; a `--resume` starts a new
//! run directory and an empty index, and only a directory a killed run left behind counts
//! against the session cap.
//!
//! **Directory.** Each store writes a directory of its own that it creates, with mode 0700 and
//! a random name, the first time a command prints: `FILE.outputs/run-<hex>/` beside a
//! `--session FILE` (the parent is created 0700 when missing), else `<tmp>/p1-outputs-<hex>/`;
//! either is removed when the run ends. Its path is known before it exists
//! ([`OutputStore::directory`]), so the host can exclude exactly it, and nothing else, from the
//! workspace fingerprint. Files are created 0600 with `create_new`.
//!
//! **Never in the command's way.** The process stream hands each masked chunk to a bounded
//! queue ([`QUEUE_CHUNKS`]) and a writer thread of its own writes it; the stream never waits
//! for the disk. When the queue is full (the disk stalls or is slower than the command), the
//! store stops storing that output: what it holds is exact up to there, and its capture is
//! `storage-incomplete`. `produced` waits a bounded time ([`SETTLE_WAIT`]) for an output whose
//! command ended; one whose writer has not finished by then is given up as `storage-failed` and
//! its file removed when the writer finishes.

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{BufWriter, Read, Seek, SeekFrom, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, SyncSender, TrySendError};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use p1_redact::SecretSet;

use super::redact::StreamRedactor;
use super::{Capture, OutputError, OutputInfo, OutputPage};

/// The most bytes one `page` answer carries, whatever limit a module asks for.
pub const MAX_PAGE_BYTES: u32 = 1024 * 1024;

/// The prefix of every handle.
const HANDLE_PREFIX: &str = "out-";
/// Random hex digits after a prefix: 128 bits.
const RANDOM_DIGITS: usize = 32;
/// Buffered bytes before a write reaches the file.
const WRITE_BUFFER_BYTES: usize = 64 * 1024;
/// Masked chunks queued for one output's writer before the store stops storing it. A chunk is
/// at most the redactor's hold-back plus one read, so this bounds the queue near 1.3 MiB.
pub(crate) const QUEUE_CHUNKS: usize = 16;
/// How long `produced` waits for the writer of an output whose command ended.
const SETTLE_WAIT: Duration = Duration::from_secs(2);

/// The disk bounds of a store (ADR-0109 item 7): host configuration, reported through
/// `stored-cap-reached` when reached. Reaching one stops storing, never the command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OutputCaps {
    /// The most bytes one output stores.
    pub per_output: u64,
    /// The most bytes all outputs of one session store together.
    pub per_session: u64,
}

impl OutputCaps {
    /// The defaults ADR-0109 item 7 set from measurement: the largest single output of the #510
    /// command set was 12.1 MiB (`git log -p -n 200`), so 16 MiB stores it whole; the largest of
    /// 13 replayed p1 coding sessions stored 1.5 MB (3.2 MB counting what the replay skipped at
    /// its upper bound, #523), so 256 MiB holds about 80 such sessions and 16 outputs at the cap.
    pub const DEFAULT: OutputCaps = OutputCaps {
        per_output: 16 * 1024 * 1024,
        per_session: 256 * 1024 * 1024,
    };
}

impl Default for OutputCaps {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// Where the run directory is in its life.
enum Directory {
    NotCreated,
    /// Created by this store; its device and inode.
    Created,
    /// It could not be created: every output is `storage-failed`.
    Failed,
    /// The run ended and removed it (temporary stores only).
    Removed,
}

/// What the store recorded of one complete output: all it serves.
struct Stored {
    capture: Capture,
    device: u64,
    inode: u64,
    size: u64,
    changed: (i64, i64),
}

struct State {
    directory: Directory,
    /// Bytes all outputs of the session hold, counted from disk when the first output starts
    /// (what killed runs of the session left behind counts too).
    used: Option<u64>,
    index: HashMap<String, Stored>,
}

/// The outputs of one run (see the module documentation).
pub struct OutputStore {
    /// The run directory: the store's own, created on first use.
    dir: PathBuf,
    /// `FILE.outputs/` of a session store, whose earlier run directories count against the
    /// session cap; `None` for a temporary store.
    session_root: Option<PathBuf>,
    caps: OutputCaps,
    state: Mutex<State>,
    /// Test only: holds every writer before each write, as a stalled disk would.
    #[cfg(test)]
    pub(crate) stall: Arc<Gate>,
}

impl std::fmt::Debug for OutputStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OutputStore")
            .field("dir", &self.dir)
            .field("caps", &self.caps)
            .finish()
    }
}

impl OutputStore {
    /// The store of a `--session` run: a new run directory inside `root` (`FILE.outputs/`),
    /// created with the first output and removed by [`OutputStore::remove_run_directory`] or
    /// when the store is dropped.
    pub fn in_directory(root: impl Into<PathBuf>, caps: OutputCaps) -> Self {
        let root = root.into();
        let dir = root.join(format!("run-{}", random_hex()));
        Self::at(dir, Some(root), caps)
    }

    /// The store of a run without `--session`: a private directory under the system's
    /// temporary directory, created with the first output and removed by
    /// [`OutputStore::remove_run_directory`] or when the store is dropped.
    pub fn temporary(caps: OutputCaps) -> Self {
        let dir = std::env::temp_dir().join(format!("p1-outputs-{}", random_hex()));
        Self::at(dir, None, caps)
    }

    fn at(dir: PathBuf, session_root: Option<PathBuf>, caps: OutputCaps) -> Self {
        Self {
            dir,
            session_root,
            caps,
            state: Mutex::new(State {
                directory: Directory::NotCreated,
                used: None,
                index: HashMap::new(),
            }),
            #[cfg(test)]
            stall: Arc::new(Gate::open()),
        }
    }

    /// The caps this store applies.
    pub fn caps(&self) -> OutputCaps {
        self.caps
    }

    /// The run directory this store writes, whether it exists yet or not: the one path the
    /// host excludes from the workspace fingerprint for it.
    pub fn directory(&self) -> &Path {
        &self.dir
    }

    fn state(&self) -> MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Removes the run directory: the run that owned it ended, and no later run serves its
    /// outputs, so they would only hold disk and the session cap (#523). Handles stop
    /// resolving and later outputs are `storage-failed`. A session's `FILE.outputs/` goes too
    /// once it is empty; a directory a killed run left behind keeps it.
    pub fn remove_run_directory(&self) {
        let mut state = self.state();
        if matches!(state.directory, Directory::Created) {
            let _ = std::fs::remove_dir_all(&self.dir);
            if let Some(root) = &self.session_root {
                let _ = std::fs::remove_dir(root);
            }
        }
        state.directory = Directory::Removed;
        state.index.clear();
    }

    /// Creates the run directory on first use; `false` when the store cannot write.
    fn ensure_directory(&self, state: &mut State) -> bool {
        match state.directory {
            Directory::Created => return true,
            Directory::Failed | Directory::Removed => return false,
            Directory::NotCreated => {}
        }
        let created = self.create_directory();
        state.directory = if created.is_ok() {
            Directory::Created
        } else {
            Directory::Failed
        };
        if state.used.is_none() {
            state.used = Some(self.count_used());
        }
        created.is_ok()
    }

    fn create_directory(&self) -> std::io::Result<()> {
        let mut builder = std::fs::DirBuilder::new();
        builder.mode(0o700);
        if let Some(root) = &self.session_root {
            match builder.create(root) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                    if !std::fs::symlink_metadata(root)?.is_dir() {
                        return Err(std::io::Error::other(format!(
                            "{} exists and is not a directory",
                            root.display()
                        )));
                    }
                }
                Err(error) => return Err(error),
            }
        }
        // `create` refuses an existing path: the directory is this store's own.
        builder.create(&self.dir)
    }

    /// What the session's earlier run directories already hold.
    fn count_used(&self) -> u64 {
        let Some(root) = &self.session_root else {
            return 0;
        };
        let mut total = 0;
        let Ok(runs) = std::fs::read_dir(root) else {
            return 0;
        };
        for run in runs.flatten() {
            if !run.file_type().is_ok_and(|kind| kind.is_dir()) {
                continue;
            }
            let Ok(files) = std::fs::read_dir(run.path()) else {
                continue;
            };
            for file in files.flatten() {
                if let Ok(metadata) = file.metadata()
                    && metadata.is_file()
                {
                    total += metadata.len();
                }
            }
        }
        total
    }

    /// Reserves up to `wanted` bytes of the session cap; returns how many were granted.
    fn reserve(&self, wanted: u64) -> u64 {
        let mut state = self.state();
        let current = state.used.unwrap_or(0);
        let granted = wanted.min(self.caps.per_session.saturating_sub(current));
        state.used = Some(current + granted);
        granted
    }

    /// Gives back bytes reserved and not stored.
    fn release(&self, bytes: u64) {
        let mut state = self.state();
        if let Some(current) = state.used.as_mut() {
            *current = current.saturating_sub(bytes);
        }
    }

    /// Starts one output, masked with `secrets`. A store that cannot write gives a recorder
    /// that is `storage-failed` from the start.
    pub(crate) fn start(self: &Arc<Self>, secrets: SecretSet) -> OutputRecorder {
        let handle = format!("{HANDLE_PREFIX}{}", random_hex());
        let entry = Arc::new(Entry::new(handle.clone()));
        let mut recorder = OutputRecorder {
            store: self.clone(),
            entry: entry.clone(),
            redactor: StreamRedactor::new(secrets),
            queue: None,
            accepted: 0,
            stopped: None,
            finished: false,
        };
        let opened = {
            let mut state = self.state();
            if self.ensure_directory(&mut state) {
                OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .mode(0o600)
                    .open(self.dir.join(&handle))
                    .ok()
            } else {
                None
            }
        };
        let Some(file) = opened else {
            entry.settle(|info| {
                info.capture = Capture::StorageFailed;
                info.stored_bytes = 0;
            });
            recorder.finished = true;
            return recorder;
        };
        let (sender, receiver) = std::sync::mpsc::sync_channel(QUEUE_CHUNKS);
        let writer = Writer {
            store: self.clone(),
            entry,
            path: self.dir.join(&handle),
            handle,
        };
        let spawned = std::thread::Builder::new()
            .name("p1-output-store".to_owned())
            .spawn(move || writer.run(file, receiver));
        match spawned {
            Ok(_) => recorder.queue = Some(sender),
            Err(_) => {
                let _ = std::fs::remove_file(self.dir.join(&recorder.entry.handle));
                recorder.entry.settle(|info| {
                    info.capture = Capture::StorageFailed;
                    info.stored_bytes = 0;
                });
                recorder.finished = true;
            }
        }
        recorder
    }

    /// The file and state of a handle this store recorded, checked against what is on disk;
    /// `unknown-output` for anything else.
    fn open(&self, handle: &str) -> Result<(File, Capture, u64), OutputError> {
        let (capture, expected) = {
            let state = self.state();
            let Some(stored) = state.index.get(handle) else {
                return Err(OutputError::UnknownOutput);
            };
            (
                stored.capture,
                (stored.device, stored.inode, stored.size, stored.changed),
            )
        };
        let file = File::open(self.dir.join(handle)).map_err(|_| OutputError::UnknownOutput)?;
        let metadata = file.metadata().map_err(|_| OutputError::UnknownOutput)?;
        let found = (
            metadata.dev(),
            metadata.ino(),
            metadata.len(),
            (metadata.ctime(), metadata.ctime_nsec()),
        );
        if !metadata.is_file() || found != expected {
            return Err(OutputError::UnknownOutput);
        }
        Ok((file, capture, metadata.len()))
    }

    /// What the store holds under `handle`.
    pub fn describe(&self, handle: &str) -> Result<OutputInfo, OutputError> {
        let (_, capture, stored_bytes) = self.open(handle)?;
        Ok(OutputInfo {
            handle: handle.to_owned(),
            stored_bytes,
            capture,
        })
    }

    /// One page of the output under `handle` (see `tool-outputs.page`).
    pub fn page(&self, handle: &str, offset: u64, limit: u32) -> Result<OutputPage, OutputError> {
        let (mut file, _, length) = self.open(handle)?;
        let read_failed = |error: std::io::Error| OutputError::ReadFailed(error.to_string());
        if offset > length {
            return Err(OutputError::OffsetPastEnd(length));
        }
        if offset == length {
            return Ok(OutputPage {
                text: String::new(),
                next_offset: offset,
                at_end: true,
            });
        }
        let wanted = u64::from(limit.min(MAX_PAGE_BYTES)).min(length - offset);
        file.seek(SeekFrom::Start(offset)).map_err(read_failed)?;
        // One byte past the page shows whether the page ends on a character boundary.
        let mut bytes = Vec::with_capacity(wanted as usize + 1);
        file.take(wanted + 1)
            .read_to_end(&mut bytes)
            .map_err(read_failed)?;
        if bytes.first().is_some_and(|&byte| is_continuation(byte)) {
            return Err(OutputError::OffsetInsideCharacter);
        }
        let mut end = (wanted as usize).min(bytes.len());
        while end > 0 && bytes.get(end).is_some_and(|&byte| is_continuation(byte)) {
            end -= 1;
        }
        if end == 0 {
            return Err(OutputError::LimitTooSmall);
        }
        bytes.truncate(end);
        let next_offset = offset + end as u64;
        let text = match String::from_utf8(bytes) {
            Ok(text) => text,
            Err(error) => String::from_utf8_lossy(error.as_bytes()).into_owned(),
        };
        Ok(OutputPage {
            text,
            next_offset,
            at_end: next_offset == length,
        })
    }
}

impl Drop for OutputStore {
    fn drop(&mut self) {
        self.remove_run_directory();
    }
}

fn is_continuation(byte: u8) -> bool {
    byte & 0b1100_0000 == 0b1000_0000
}

/// 32 random lowercase hex digits.
fn random_hex() -> String {
    let bytes = crate::capabilities::random_bytes(RANDOM_DIGITS as u32 / 2).unwrap_or_default();
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// A gate a test closes to stall every writer before its next write.
#[cfg(test)]
pub(crate) struct Gate {
    open: Mutex<bool>,
    changed: Condvar,
}

#[cfg(test)]
impl Gate {
    fn open() -> Self {
        Self {
            open: Mutex::new(true),
            changed: Condvar::new(),
        }
    }

    pub(crate) fn set(&self, open: bool) {
        *self.open.lock().unwrap() = open;
        self.changed.notify_all();
    }

    fn pass(&self) {
        let mut open = self.open.lock().unwrap();
        while !*open {
            open = self.changed.wait(open).unwrap();
        }
    }
}

/// What `produced` reports of one output, shared by its recorder and its writer.
pub(crate) struct Entry {
    handle: String,
    state: Mutex<EntryState>,
    changed: Condvar,
}

struct EntryState {
    info: OutputInfo,
    /// The recorder is done: the command ended.
    finished: bool,
    /// The writer is done: `info` is final.
    settled: bool,
    /// `produced` gave up waiting: the writer removes the file instead of recording it.
    abandoned: bool,
    /// Why the recorder stopped storing, when it did.
    stopped: Option<Capture>,
}

impl Entry {
    fn new(handle: String) -> Self {
        Self {
            state: Mutex::new(EntryState {
                info: OutputInfo {
                    handle: handle.clone(),
                    stored_bytes: 0,
                    capture: Capture::Complete,
                },
                finished: false,
                settled: false,
                abandoned: false,
                stopped: None,
            }),
            handle,
            changed: Condvar::new(),
        }
    }

    fn lock(&self) -> MutexGuard<'_, EntryState> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn settle(&self, change: impl FnOnce(&mut OutputInfo)) {
        let mut state = self.lock();
        change(&mut state.info);
        state.finished = true;
        state.settled = true;
        self.changed.notify_all();
    }

    /// What `produced` reports: the final state of an output whose command ended (waiting
    /// at most [`SETTLE_WAIT`] for its writer), else what is stored so far.
    pub(crate) fn report(&self) -> OutputInfo {
        let deadline = Instant::now() + SETTLE_WAIT;
        let mut state = self.lock();
        while state.finished && !state.settled {
            let now = Instant::now();
            if now >= deadline {
                state.abandoned = true;
                state.settled = true;
                state.info.capture = Capture::StorageFailed;
                state.info.stored_bytes = 0;
                break;
            }
            state = self
                .changed
                .wait_timeout(state, deadline - now)
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .0;
        }
        let mut info = state.info.clone();
        if !state.settled
            && let Some(stopped) = state.stopped
        {
            info.capture = stopped;
        }
        info
    }
}

/// One output's writer thread: writes what the queue brings, then records the output.
struct Writer {
    store: Arc<OutputStore>,
    entry: Arc<Entry>,
    path: PathBuf,
    handle: String,
}

impl Writer {
    fn run(self, file: File, queue: Receiver<String>) {
        let mut file = BufWriter::with_capacity(WRITE_BUFFER_BYTES, file);
        let mut received: u64 = 0;
        let mut failed = false;
        while let Ok(text) = queue.recv() {
            received += text.len() as u64;
            if failed {
                continue;
            }
            #[cfg(test)]
            self.store.stall.pass();
            if file.write_all(text.as_bytes()).is_err() {
                failed = true;
                continue;
            }
            self.entry.lock().info.stored_bytes += text.len() as u64;
        }
        // The path must still name the file written: a directory removed or a file put in its
        // place mid-output is a storage failure, not an output.
        let recorded = if failed {
            None
        } else {
            file.flush()
                .ok()
                .and_then(|()| file.get_ref().metadata().ok())
                .filter(|written| {
                    std::fs::symlink_metadata(&self.path).is_ok_and(|named| {
                        named.dev() == written.dev() && named.ino() == written.ino()
                    })
                })
        };
        drop(file);
        let mut state = self.entry.lock();
        let capture = state.stopped.unwrap_or(Capture::Complete);
        match recorded {
            Some(metadata) if !state.abandoned && metadata.len() == received => {
                self.store.state().index.insert(
                    self.handle.clone(),
                    Stored {
                        capture,
                        device: metadata.dev(),
                        inode: metadata.ino(),
                        size: metadata.len(),
                        changed: (metadata.ctime(), metadata.ctime_nsec()),
                    },
                );
                state.info.capture = capture;
                state.info.stored_bytes = received;
            }
            _ => {
                let _ = std::fs::remove_file(&self.path);
                self.store.release(received);
                if !state.abandoned {
                    state.info.capture = Capture::StorageFailed;
                    state.info.stored_bytes = 0;
                }
            }
        }
        state.settled = true;
        self.entry.changed.notify_all();
    }
}

/// One output being written: the tee a process stream writes each chunk through
/// (`crate::process::ProcessStream`). It masks and queues on the caller's thread and never
/// waits for the disk. Dropping it finishes it.
pub(crate) struct OutputRecorder {
    store: Arc<OutputStore>,
    entry: Arc<Entry>,
    redactor: StreamRedactor,
    /// The writer's queue; `None` once finished or stopped.
    queue: Option<SyncSender<String>>,
    /// Bytes queued so far.
    accepted: u64,
    /// Why storing stopped, once it did.
    stopped: Option<Capture>,
    finished: bool,
}

impl OutputRecorder {
    /// The entry `produced` reports for this output.
    pub(crate) fn entry(&self) -> Arc<Entry> {
        self.entry.clone()
    }

    /// Takes one chunk of the process's output in.
    pub(crate) fn write(&mut self, chunk: &[u8]) {
        if self.queue.is_none() {
            return;
        }
        let text = self.redactor.push(chunk);
        self.store_text(&text);
    }

    /// Masks what is still held, queues it and lets the writer record the output. Never waits.
    pub(crate) fn finish(&mut self) {
        if self.finished {
            return;
        }
        self.finished = true;
        if self.queue.is_some() {
            let text = self.redactor.finish();
            self.store_text(&text);
        }
        {
            let mut state = self.entry.lock();
            state.stopped = self.stopped;
            state.finished = true;
        }
        // Closing the queue is the writer's signal to record the output.
        self.queue = None;
    }

    fn stop(&mut self, why: Capture) {
        self.stopped = Some(why);
        self.entry.lock().stopped = Some(why);
        self.queue = None;
    }

    fn store_text(&mut self, text: &str) {
        let Some(queue) = &self.queue else {
            return;
        };
        if text.is_empty() {
            return;
        }
        let room = self.store.caps.per_output.saturating_sub(self.accepted);
        let wanted = (text.len() as u64).min(room);
        let granted = self.store.reserve(wanted);
        let mut take = granted as usize;
        while !text.is_char_boundary(take) {
            take -= 1;
        }
        self.store.release(granted - take as u64);
        if take > 0 {
            match queue.try_send(text[..take].to_owned()) {
                Ok(()) => self.accepted += take as u64,
                Err(TrySendError::Full(_)) => {
                    self.store.release(take as u64);
                    self.stop(Capture::StorageIncomplete);
                    return;
                }
                Err(TrySendError::Disconnected(_)) => {
                    self.store.release(take as u64);
                    self.stop(Capture::StorageFailed);
                    return;
                }
            }
        }
        if take < text.len() {
            self.stop(Capture::StoredCapReached);
        }
    }

    /// Bytes this recorder holds in memory on the stream's side: the redactor's held text.
    #[cfg(test)]
    pub(crate) fn held(&self) -> usize {
        self.redactor.held()
    }
}

impl Drop for OutputRecorder {
    fn drop(&mut self) {
        self.finish();
    }
}
