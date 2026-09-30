//! The output store (ADR-0109 items 3, 4 and 7): one directory per session, one file per
//! output, written only by the host.
//!
//! An output is written to `<handle>.part` while its command runs and renamed when it ends:
//! to `<handle>.out` when it holds everything (`complete`), to `<handle>.cap` when a cap
//! stopped it (`stored-cap-reached`). The capture state is the file name, so a store opened
//! again by `--resume` knows every output without an index, and a `.part` a crashed run left
//! is never served. A write, flush or rename that fails removes the file: the output is then
//! `storage-failed` and its handle names nothing.
//!
//! The directory is created on the first output only, with mode 0700, and each file with mode
//! 0600. A handle is `out-` and 32 random lowercase hex digits, checked character by character
//! before it is joined to the directory, so no module input names a file.

use std::fs::{File, OpenOptions};
use std::io::{BufWriter, Read, Seek, SeekFrom, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use p1_redact::SecretSet;

use super::redact::StreamRedactor;
use super::{Capture, OutputError, OutputInfo, OutputPage};

/// The most bytes one `page` answer carries, whatever limit a module asks for.
pub const MAX_PAGE_BYTES: u32 = 1024 * 1024;

/// The prefix of every handle.
const HANDLE_PREFIX: &str = "out-";
/// Random hex digits after the prefix: 128 bits.
const HANDLE_DIGITS: usize = 32;
/// The file of a complete output, of one a cap stopped, and of one still being written.
const COMPLETE: &str = "out";
const CAPPED: &str = "cap";
const PARTIAL: &str = "part";
/// Buffered bytes before a write reaches the file.
const WRITE_BUFFER_BYTES: usize = 64 * 1024;

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
    /// The defaults ADR-0109 item 7 set from the #510 measurement: the largest single output of
    /// the measured command set was 12.1 MiB (`git log -p -n 200`), so 16 MiB stores it whole;
    /// the per-session cap is not yet measured and holds about twenty such sets (follow-up #523).
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

/// Where a store's directory is.
enum Location {
    /// `FILE.outputs/` beside a `--session FILE`: it outlives the run.
    Session(PathBuf),
    /// A private temporary directory owned by the run, created on first use.
    Temporary(Mutex<Temporary>),
}

enum Temporary {
    NotCreated,
    Created(tempfile::TempDir),
    /// The run ended and removed it; nothing is stored any more.
    Removed,
}

/// The outputs of one session (see the module documentation).
pub struct OutputStore {
    location: Location,
    caps: OutputCaps,
    /// Bytes all outputs of the session hold, counted from the directory the first time an
    /// output starts (a resumed session's earlier outputs count too).
    used: Mutex<Option<u64>>,
}

impl std::fmt::Debug for OutputStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let location = match &self.location {
            Location::Session(dir) => dir.display().to_string(),
            Location::Temporary(_) => "<temporary>".to_owned(),
        };
        f.debug_struct("OutputStore")
            .field("location", &location)
            .field("caps", &self.caps)
            .finish()
    }
}

impl OutputStore {
    /// The store of a `--session` run: the directory `dir` (`FILE.outputs/`), created with
    /// the first output and kept after the run.
    pub fn in_directory(dir: impl Into<PathBuf>, caps: OutputCaps) -> Self {
        Self {
            location: Location::Session(dir.into()),
            caps,
            used: Mutex::new(None),
        }
    }

    /// The store of a run without `--session`: a private temporary directory, created with
    /// the first output and removed by [`OutputStore::remove_temporary`] or when the store
    /// is dropped.
    pub fn temporary(caps: OutputCaps) -> Self {
        Self {
            location: Location::Temporary(Mutex::new(Temporary::NotCreated)),
            caps,
            used: Mutex::new(None),
        }
    }

    /// The caps this store applies.
    pub fn caps(&self) -> OutputCaps {
        self.caps
    }

    /// Removes a temporary store's directory: the run that owned it ended. Handles stop
    /// resolving and later outputs are `storage-failed`. A session's store is left alone.
    pub fn remove_temporary(&self) {
        if let Location::Temporary(state) = &self.location {
            let mut state = state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if let Temporary::Created(dir) = std::mem::replace(&mut *state, Temporary::Removed) {
                let _ = dir.close();
            }
        }
    }

    /// The directory, when it exists; `create` makes it (0700) when it does not yet.
    fn directory(&self, create: bool) -> std::io::Result<Option<PathBuf>> {
        match &self.location {
            Location::Session(dir) => {
                match std::fs::symlink_metadata(dir) {
                    Ok(metadata) if metadata.is_dir() => return Ok(Some(dir.clone())),
                    Ok(_) => {
                        return Err(std::io::Error::other(format!(
                            "{} exists and is not a directory",
                            dir.display()
                        )));
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(error) => return Err(error),
                }
                if !create {
                    return Ok(None);
                }
                std::fs::DirBuilder::new().mode(0o700).create(dir)?;
                Ok(Some(dir.clone()))
            }
            Location::Temporary(state) => {
                let mut state = state
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                match &*state {
                    Temporary::Created(dir) => Ok(Some(dir.path().to_path_buf())),
                    Temporary::Removed => {
                        Err(std::io::Error::other("the run's output store was removed"))
                    }
                    Temporary::NotCreated if !create => Ok(None),
                    Temporary::NotCreated => {
                        let dir = tempfile::Builder::new().prefix("p1-outputs-").tempdir()?;
                        let path = dir.path().to_path_buf();
                        *state = Temporary::Created(dir);
                        Ok(Some(path))
                    }
                }
            }
        }
    }

    /// Starts one output, masked with `secrets`. A store that cannot write gives a recorder
    /// that is `storage-failed` from the start.
    pub(crate) fn start(self: &Arc<Self>, secrets: SecretSet) -> OutputRecorder {
        let handle = new_handle();
        let entry = Arc::new(Mutex::new(OutputInfo {
            handle: handle.clone(),
            stored_bytes: 0,
            capture: Capture::Complete,
        }));
        let mut recorder = OutputRecorder {
            store: self.clone(),
            entry,
            redactor: StreamRedactor::new(secrets),
            file: None,
            partial: None,
            state: Writing::Open,
        };
        match self.open_partial(&handle) {
            Ok((file, partial)) => {
                recorder.file = Some(BufWriter::with_capacity(WRITE_BUFFER_BYTES, file));
                recorder.partial = Some(partial);
            }
            Err(_) => recorder.fail(),
        }
        recorder
    }

    fn open_partial(&self, handle: &str) -> std::io::Result<(File, PathBuf)> {
        let dir = self
            .directory(true)?
            .ok_or_else(|| std::io::Error::other("no output directory"))?;
        self.count_used(&dir);
        let partial = dir.join(format!("{handle}.{PARTIAL}"));
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&partial)?;
        Ok((file, partial))
    }

    /// Counts what the directory already holds, once.
    fn count_used(&self, dir: &Path) {
        let mut used = self
            .used
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if used.is_some() {
            return;
        }
        let mut total = 0;
        if let Ok(entries) = std::fs::read_dir(dir) {
            for entry in entries.flatten() {
                let name = entry.file_name();
                let stored = name
                    .to_str()
                    .and_then(|name| name.rsplit_once('.'))
                    .is_some_and(|(_, extension)| extension == COMPLETE || extension == CAPPED);
                if stored && let Ok(metadata) = entry.metadata() {
                    total += metadata.len();
                }
            }
        }
        *used = Some(total);
    }

    /// Reserves up to `wanted` bytes of the session cap; returns how many were granted.
    fn reserve(&self, wanted: u64) -> u64 {
        let mut used = self
            .used
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let current = used.unwrap_or(0);
        let granted = wanted.min(self.caps.per_session.saturating_sub(current));
        *used = Some(current + granted);
        granted
    }

    /// Gives back bytes reserved and not stored.
    fn release(&self, bytes: u64) {
        let mut used = self
            .used
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(current) = used.as_mut() {
            *current = current.saturating_sub(bytes);
        }
    }

    /// The stored file of `handle` and its capture state; `unknown-output` for anything else.
    fn stored(&self, handle: &str) -> Result<(PathBuf, Capture), OutputError> {
        if !is_handle(handle) {
            return Err(OutputError::UnknownOutput);
        }
        let dir = match self.directory(false) {
            Ok(Some(dir)) => dir,
            _ => return Err(OutputError::UnknownOutput),
        };
        for (extension, capture) in [
            (COMPLETE, Capture::Complete),
            (CAPPED, Capture::StoredCapReached),
        ] {
            let path = dir.join(format!("{handle}.{extension}"));
            match std::fs::symlink_metadata(&path) {
                Ok(metadata) if metadata.is_file() => return Ok((path, capture)),
                _ => continue,
            }
        }
        Err(OutputError::UnknownOutput)
    }

    /// What the store holds under `handle`.
    pub fn describe(&self, handle: &str) -> Result<OutputInfo, OutputError> {
        let (path, capture) = self.stored(handle)?;
        let stored_bytes = std::fs::metadata(&path)
            .map_err(|error| OutputError::ReadFailed(error.to_string()))?
            .len();
        Ok(OutputInfo {
            handle: handle.to_owned(),
            stored_bytes,
            capture,
        })
    }

    /// One page of the output under `handle` (see `tool-outputs.page`).
    pub fn page(&self, handle: &str, offset: u64, limit: u32) -> Result<OutputPage, OutputError> {
        let (path, _) = self.stored(handle)?;
        let read_failed = |error: std::io::Error| OutputError::ReadFailed(error.to_string());
        let mut file = File::open(&path).map_err(read_failed)?;
        let length = file.metadata().map_err(read_failed)?.len();
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
        // The store writes only masked UTF-8; a file changed behind its back is shown lossily
        // rather than refused, its offsets still counting the bytes on disk.
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
        self.remove_temporary();
    }
}

fn is_continuation(byte: u8) -> bool {
    byte & 0b1100_0000 == 0b1000_0000
}

/// A new random handle.
fn new_handle() -> String {
    let bytes = crate::capabilities::random_bytes(HANDLE_DIGITS as u32 / 2).unwrap_or_default();
    let mut handle = String::with_capacity(HANDLE_PREFIX.len() + HANDLE_DIGITS);
    handle.push_str(HANDLE_PREFIX);
    for byte in bytes {
        handle.push_str(&format!("{byte:02x}"));
    }
    handle
}

/// Whether `handle` has the one shape a handle has; checked before it is joined to a path.
fn is_handle(handle: &str) -> bool {
    handle.strip_prefix(HANDLE_PREFIX).is_some_and(|digits| {
        digits.len() == HANDLE_DIGITS
            && digits
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    })
}

/// Where a recorder is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Writing {
    /// Storing what arrives.
    Open,
    /// A cap stopped it; what is stored stays.
    Capped,
    /// Storage failed; nothing is kept.
    Failed,
    /// Finished and renamed (or failed): nothing more happens.
    Finished,
}

/// One output being written: the tee a process stream writes each chunk through
/// (`crate::process::ProcessStream`). Dropping it finishes it.
pub(crate) struct OutputRecorder {
    store: Arc<OutputStore>,
    /// What `produced` reports for it, updated as it is written.
    entry: Arc<Mutex<OutputInfo>>,
    redactor: StreamRedactor,
    file: Option<BufWriter<File>>,
    partial: Option<PathBuf>,
    state: Writing,
}

impl OutputRecorder {
    /// The entry `produced` reports for this output.
    pub(crate) fn entry(&self) -> Arc<Mutex<OutputInfo>> {
        self.entry.clone()
    }

    /// Takes one chunk of the process's output in.
    pub(crate) fn write(&mut self, chunk: &[u8]) {
        if self.state != Writing::Open {
            return;
        }
        let text = self.redactor.push(chunk);
        self.store_text(&text);
    }

    /// Masks what is still held, stores it and gives the output its final name.
    pub(crate) fn finish(&mut self) {
        if self.state == Writing::Finished {
            return;
        }
        if self.state == Writing::Open {
            let text = self.redactor.finish();
            self.store_text(&text);
        }
        if self.state == Writing::Failed {
            self.state = Writing::Finished;
            return;
        }
        let capped = self.state == Writing::Capped;
        let renamed = self.close(capped);
        match renamed {
            Ok(()) => {
                if capped {
                    self.set(|info| info.capture = Capture::StoredCapReached);
                }
                self.state = Writing::Finished;
            }
            Err(_) => {
                self.fail();
                self.state = Writing::Finished;
            }
        }
    }

    fn close(&mut self, capped: bool) -> std::io::Result<()> {
        let mut file = self
            .file
            .take()
            .ok_or_else(|| std::io::Error::other("no output file"))?;
        file.flush()?;
        drop(file);
        let partial = self
            .partial
            .as_ref()
            .ok_or_else(|| std::io::Error::other("no output file"))?;
        let extension = if capped { CAPPED } else { COMPLETE };
        std::fs::rename(partial, partial.with_extension(extension))?;
        self.partial = None;
        Ok(())
    }

    fn store_text(&mut self, text: &str) {
        if self.state != Writing::Open || text.is_empty() {
            return;
        }
        let stored = self.stored();
        let room = self.store.caps.per_output.saturating_sub(stored);
        let wanted = (text.len() as u64).min(room);
        let granted = self.store.reserve(wanted);
        let mut take = granted as usize;
        while !text.is_char_boundary(take) {
            take -= 1;
        }
        self.store.release(granted - take as u64);
        if take > 0 {
            let written = match self.file.as_mut() {
                Some(file) => file.write_all(&text.as_bytes()[..take]),
                None => Err(std::io::Error::other("no output file")),
            };
            if written.is_err() {
                self.store.release(take as u64);
                self.fail();
                return;
            }
            self.set(|info| info.stored_bytes += take as u64);
        }
        if take < text.len() {
            self.state = Writing::Capped;
            self.set(|info| info.capture = Capture::StoredCapReached);
        }
    }

    fn stored(&self) -> u64 {
        self.entry
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .stored_bytes
    }

    fn set(&self, change: impl FnOnce(&mut OutputInfo)) {
        change(
            &mut self
                .entry
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()),
        );
    }

    /// Storage failed: remove what was written and keep nothing.
    fn fail(&mut self) {
        self.file = None;
        if let Some(partial) = self.partial.take() {
            let _ = std::fs::remove_file(partial);
        }
        let stored = self.stored();
        self.store.release(stored);
        self.set(|info| {
            info.stored_bytes = 0;
            info.capture = Capture::StorageFailed;
        });
        self.state = Writing::Failed;
    }

    /// Bytes this recorder holds in memory: the redactor's held text and the write buffer.
    #[cfg(test)]
    pub(crate) fn held(&self) -> usize {
        self.redactor.held() + self.file.as_ref().map_or(0, |file| file.buffer().len())
    }
}

impl Drop for OutputRecorder {
    fn drop(&mut self) {
        self.finish();
    }
}
