//! Read side of the `workspace` and `snapshot` capability interfaces.
//!
//! The migration's `workspace` and `snapshot` host imports call exactly these
//! methods, so they are the thinnest native surface over what the file tools
//! already do: confinement ([`Workspace::check_path`]), metadata
//! ([`Workspace::stat`], [`Workspace::list`]) and one whole-file read that
//! records the observation and hands back an immutable [`Snapshot`].
//!
//! The mutation side ([`crate::WriteGate`], [`crate::Mutation`],
//! [`crate::write_atomic`]) stays where it is; nothing here writes.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::observe::{ObservedFiles, hash_of};
use crate::{Workspace, WorkspaceError};

/// What kind of filesystem object a path or a directory entry is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileKind {
    /// A regular file.
    File,
    /// A directory.
    Directory,
    /// A symbolic link. [`Workspace::list`] never follows an entry, so every
    /// link in a listing is reported as a link. [`Workspace::stat`] reports a
    /// link as a link only where [`Workspace::resolve`] left the leaf
    /// unresolved (a link whose target is absent); a link whose target is
    /// inside the workspace arrives canonicalized, so its target's kind is
    /// reported instead.
    Symlink,
    /// Anything else the host filesystem has: a fifo, socket or device.
    Other,
}

/// A path that passed [`Workspace::check_path`]: the confined path to use, plus
/// the root-relative form to show the model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckedPath {
    path: PathBuf,
    display: String,
}

impl CheckedPath {
    /// The resolved path: inside the workspace root after symlink resolution.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The root-relative display form (see [`Workspace::display`]).
    pub fn display(&self) -> &str {
        &self.display
    }
}

/// What [`Workspace::stat`] reports about one path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Stat {
    /// The kind of the object the path names.
    pub kind: FileKind,
    /// Its size in bytes, as the host filesystem reports it.
    pub size: u64,
}

/// One entry of [`Workspace::list`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirEntry {
    /// The entry's name within its directory, never a path.
    pub name: String,
    /// The entry's kind, with a symlink reported as a symlink.
    pub kind: FileKind,
}

/// What a [`Snapshot`] knows about the file it was read from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnapshotMetadata {
    /// The root-relative display form of the path.
    pub path: String,
    /// The size in bytes of the snapshot's contents.
    pub size: u64,
    /// The content hash [`ObservedFiles`] stored for exactly these bytes, so
    /// two states of one file can be compared without re-reading either.
    pub content_hash: u64,
}

/// An immutable copy of one file's bytes, taken by [`Workspace::read`].
///
/// The bytes live in this snapshot and nowhere else: a later write to the file
/// changes the file, never the snapshot. The whole file is held in memory, with
/// no size bound of its own (see `Workspace::read`).
#[derive(Clone)]
pub struct Snapshot {
    path: String,
    bytes: Arc<[u8]>,
    content_hash: u64,
}

impl Snapshot {
    /// The path, size and content hash of the bytes this snapshot holds.
    pub fn metadata(&self) -> SnapshotMetadata {
        SnapshotMetadata {
            path: self.path.clone(),
            size: self.bytes.len() as u64,
            content_hash: self.content_hash,
        }
    }

    /// The bytes at `offset..offset + len` within this snapshot.
    ///
    /// A range that reaches past the end returns the bytes that exist, possibly
    /// an empty slice; an offset at or past the end, and a zero length, return
    /// an empty slice. Nothing here panics, whatever the caller passes.
    pub fn read(&self, offset: usize, len: usize) -> &[u8] {
        if offset >= self.bytes.len() {
            return &[];
        }
        let end = offset.saturating_add(len).min(self.bytes.len());
        &self.bytes[offset..end]
    }
}

impl std::fmt::Debug for Snapshot {
    /// Deliberately not the contents: a snapshot can hold a whole file, and a
    /// `{:?}` that dumped it would put a file into a log.
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Snapshot")
            .field("path", &self.path)
            .field("size", &self.bytes.len())
            .field("content_hash", &self.content_hash)
            .finish()
    }
}

impl Workspace {
    /// Confine `requested` exactly as [`Workspace::resolve`] does, returning the
    /// resolved path together with its display form.
    pub fn check_path(&self, requested: &str) -> Result<CheckedPath, WorkspaceError> {
        let path = self.resolve(requested)?;
        let display = self.display(&path);
        Ok(CheckedPath { path, display })
    }

    /// The kind and size in bytes of `requested`.
    ///
    /// The leaf is looked at as itself (not followed) — see [`FileKind::Symlink`].
    /// A path that does not exist is [`WorkspaceError::NotFound`].
    pub fn stat(&self, requested: &str) -> Result<Stat, WorkspaceError> {
        let path = self.resolve(requested)?;
        let metadata = std::fs::symlink_metadata(&path)
            .map_err(|error| missing_or_io(requested, &path, error))?;
        Ok(Stat {
            kind: kind_of(metadata.file_type()),
            size: metadata.len(),
        })
    }

    /// The entries of the directory `requested`, sorted by name, each with its
    /// name and kind.
    ///
    /// An entry is never followed, so a symlink is reported as
    /// [`FileKind::Symlink`]: a link out of the workspace stays visible as a
    /// link instead of leaking its target or failing the whole listing. A path
    /// that does not exist is [`WorkspaceError::NotFound`], one that is not a
    /// directory is [`WorkspaceError::NotADirectory`].
    pub fn list(&self, requested: &str) -> Result<Vec<DirEntry>, WorkspaceError> {
        let path = self.resolve(requested)?;
        let metadata = std::fs::symlink_metadata(&path)
            .map_err(|error| missing_or_io(requested, &path, error))?;
        if !metadata.is_dir() {
            return Err(WorkspaceError::NotADirectory(path));
        }

        let reader =
            std::fs::read_dir(&path).map_err(|error| missing_or_io(requested, &path, error))?;
        let mut entries = Vec::new();
        for entry in reader {
            let entry = entry.map_err(|source| WorkspaceError::Io {
                path: path.clone(),
                source,
            })?;
            let file_type = entry.file_type().map_err(|source| WorkspaceError::Io {
                path: entry.path(),
                source,
            })?;
            entries.push(DirEntry {
                // A name crosses the module boundary as a UTF-8 string, so a name
                // the host cannot represent is shown lossily rather than dropped.
                name: entry.file_name().to_string_lossy().into_owned(),
                kind: kind_of(file_type),
            });
        }
        // `sort_by_key` would clone every name to hand back a key; compare in place.
        entries.sort_by(|left, right| left.name.cmp(&right.name));
        Ok(entries)
    }

    /// Read the file `requested` once, record the observation in `observed` and
    /// return an immutable [`Snapshot`] of exactly those bytes.
    ///
    /// The recorded hash equals what the `read` tool records today: `read`
    /// streams the whole file through [`crate::StreamingHash`] and keeps only a
    /// window of lines, and `observe.rs` shows that a streamed hash is the hash
    /// of the same bytes as a whole slice, which is the value
    /// [`ObservedFiles::record`] stores. An edit after this call is therefore
    /// judged exactly as after a `read` tool call.
    ///
    /// The snapshot holds the whole file in memory and this method adds no size
    /// bound; the read tool's window is what bounds model-visible output today,
    /// and a bound for very large files is a question for the slice that moves
    /// the read tool onto this API.
    pub fn read(
        &self,
        requested: &str,
        observed: &ObservedFiles,
    ) -> Result<Snapshot, WorkspaceError> {
        let path = self.resolve(requested)?;
        let display = self.display(&path);
        // Mirror `list`: decide the kind from metadata instead of leaving `read` of a
        // directory to surface as an untyped `Io` ("Is a directory"). The host import
        // needs the typed variant to map a wrong kind without parsing an io message.
        let metadata = std::fs::symlink_metadata(&path)
            .map_err(|error| missing_or_io(requested, &path, error))?;
        if metadata.is_dir() {
            return Err(WorkspaceError::NotADirectory(path));
        }
        let bytes = std::fs::read(&path).map_err(|error| missing_or_io(requested, &path, error))?;

        // `record` stores `hash_of(contents)`: computing the same value here with
        // the same function makes the snapshot's hash and the stored observation
        // equal by construction, not by a second algorithm agreeing.
        let content_hash = hash_of(&bytes);
        observed.record(&path, &bytes);

        Ok(Snapshot {
            path: display,
            bytes: Arc::from(bytes),
            content_hash,
        })
    }
}

/// The kind of `file_type`, which a caller already looked at without following.
fn kind_of(file_type: std::fs::FileType) -> FileKind {
    if file_type.is_file() {
        FileKind::File
    } else if file_type.is_dir() {
        FileKind::Directory
    } else if file_type.is_symlink() {
        FileKind::Symlink
    } else {
        FileKind::Other
    }
}

/// A filesystem failure on `requested`. A missing path is its own variant, so a
/// host import can tell "absent" from "unreadable" without parsing a message.
fn missing_or_io(requested: &str, path: &Path, source: std::io::Error) -> WorkspaceError {
    if source.kind() == std::io::ErrorKind::NotFound {
        WorkspaceError::NotFound {
            requested: requested.to_string(),
        }
    } else {
        WorkspaceError::Io {
            path: path.to_path_buf(),
            source,
        }
    }
}
