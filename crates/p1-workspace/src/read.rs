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

use std::fs::File;
use std::io::Read;
use std::os::fd::{AsRawFd, OwnedFd};
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use rustix::fs::{CWD, Mode, OFlags};
use rustix::io::Errno;

use crate::observe::{ObservedFiles, hash_of};
use crate::{
    CredentialPolicy, ProtectedIndex, Workspace, WorkspaceError, credential_refusal,
    xdg_credentials,
};

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
/// bounded by [`crate::commit::MAX_FILE_BYTES`] (see `Workspace::read`).
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

    /// Resolve `requested`, then open its regular file without following links.
    pub fn open_file(&self, requested: &str) -> Result<File, WorkspaceError> {
        let resolved = self.resolve(requested)?;
        self.open_file_at(&resolved)
    }

    /// Open a resolved path beneath the workspace root without following links,
    /// then verify the opened handle's type. NONBLOCK ensures a swapped FIFO
    /// cannot park a host thread between path validation and open.
    pub fn open_file_at(&self, requested: &Path) -> Result<File, WorkspaceError> {
        self.open_file_at_with_path(requested)
            .map(|(file, _path)| file)
    }

    fn open_file_at_with_path(&self, requested: &Path) -> Result<(File, PathBuf), WorkspaceError> {
        let requested_display = requested.to_string_lossy();
        let path = std::fs::canonicalize(requested)
            .map_err(|source| missing_or_io(&requested_display, requested, source))?;
        if !path.starts_with(&self.root) {
            return Err(WorkspaceError::OutsideWorkspace {
                requested: requested_display.into_owned(),
            });
        }
        let relative =
            path.strip_prefix(&self.root)
                .map_err(|_| WorkspaceError::OutsideWorkspace {
                    requested: requested_display.to_string(),
                })?;
        let mut directory = rustix::fs::openat(
            CWD,
            &self.root,
            OFlags::PATH | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(|error| io_at(&path, error))?;
        let mut components = relative.components().peekable();
        while let Some(component) = components.next() {
            let Component::Normal(name) = component else {
                return Err(WorkspaceError::NotADirectory(path));
            };
            if components.peek().is_some() {
                directory = rustix::fs::openat(
                    &directory,
                    name,
                    OFlags::PATH | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                    Mode::empty(),
                )
                .map_err(|error| io_at(&path, error))?;
            } else {
                let fd = rustix::fs::openat(
                    &directory,
                    name,
                    OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
                    Mode::empty(),
                )
                .map_err(|error| match error {
                    Errno::LOOP | Errno::NOTDIR => WorkspaceError::NotADirectory(path.clone()),
                    other => io_at(&path, other),
                })?;
                let file = File::from(fd);
                if !file
                    .metadata()
                    .map_err(|error| WorkspaceError::Io {
                        path: path.clone(),
                        source: error,
                    })?
                    .is_file()
                {
                    return Err(WorkspaceError::NotADirectory(path));
                }
                return Ok((file, path));
            }
        }
        Err(WorkspaceError::NotADirectory(path))
    }

    /// The kind and size in bytes of `requested`.
    ///
    /// The leaf is looked at as itself (not followed) — see [`FileKind::Symlink`].
    /// A path that does not exist is [`WorkspaceError::NotFound`].
    pub fn stat(&self, requested: &str) -> Result<Stat, WorkspaceError> {
        let path = self.resolve(requested)?;
        let fd = open_checked_path(self, requested, &path, OFlags::PATH)?;
        let metadata = File::from(fd)
            .metadata()
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
        let fd = open_checked_path(self, requested, &path, OFlags::RDONLY | OFlags::DIRECTORY)?;
        let directory = File::from(fd);
        if !directory
            .metadata()
            .map_err(|error| missing_or_io(requested, &path, error))?
            .is_dir()
        {
            return Err(WorkspaceError::NotADirectory(path));
        }
        // The fd is the opened directory; /proc/self/fd (or /dev/fd) duplicates it,
        // never resolving the original workspace spelling after validation.
        #[cfg(target_os = "linux")]
        let descriptor = format!("/proc/self/fd/{}", directory.as_raw_fd());
        #[cfg(not(target_os = "linux"))]
        let descriptor = format!("/dev/fd/{}", directory.as_raw_fd());
        let reader = std::fs::read_dir(descriptor)
            .map_err(|error| missing_or_io(requested, &path, error))?;
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

    /// Build a bounded snapshot from a descriptor already checked by the host's
    /// credential policy. The extra byte detects growth past the budget before
    /// allocating more than `max_bytes + 1`, even if stat raced a writer.
    pub fn snapshot_from_open_file(
        &self,
        path: &Path,
        mut file: File,
        max_bytes: u64,
    ) -> Result<Snapshot, WorkspaceError> {
        let mut bytes = Vec::new();
        file.by_ref()
            .take(max_bytes.saturating_add(1))
            .read_to_end(&mut bytes)
            .map_err(|source| WorkspaceError::Io {
                path: path.to_path_buf(),
                source,
            })?;
        if bytes.len() as u64 > max_bytes {
            return Err(WorkspaceError::Io {
                path: path.to_path_buf(),
                source: std::io::Error::other("file exceeds the component read budget"),
            });
        }
        Ok(Snapshot {
            path: self.display(path),
            content_hash: hash_of(&bytes),
            bytes: Arc::from(bytes),
        })
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
    /// Snapshots larger than the mutation file-size limit are refused before
    /// allocating the complete file; the limit is also enforced during the read.
    /// [`read_within`](Self::read_within) is the bounded form a caller that must
    /// cap host memory further uses.
    pub fn read(
        &self,
        requested: &str,
        observed: &ObservedFiles,
    ) -> Result<Snapshot, WorkspaceError> {
        self.read_within(requested, observed, usize::MAX)
    }

    /// Read the file `requested` once as [`read`](Self::read), but refuse a file
    /// whose bytes exceed `limit`, itself capped at the mutation file-size limit.
    ///
    /// The bound is applied to the handle this call opens, not to a separate
    /// [`stat`](Self::stat) of the path: a writer that grows or replaces the file
    /// between such a check and this read cannot make it load more than `limit`
    /// bytes. A file of exactly `limit` bytes is accepted; one byte more is
    /// refused with a [`WorkspaceError::Io`] whose message names the bound.
    pub fn read_within(
        &self,
        requested: &str,
        observed: &ObservedFiles,
        limit: usize,
    ) -> Result<Snapshot, WorkspaceError> {
        let limit = limit.min(usize::try_from(crate::commit::MAX_FILE_BYTES).unwrap_or(usize::MAX));
        let resolved = self.resolve(requested)?;
        let (mut file, path) =
            self.open_file_at_with_path(&resolved)
                .map_err(|error| match error {
                    WorkspaceError::Io { path, source } => missing_or_io(requested, &path, source),
                    other => other,
                })?;
        let display = self.display(&path);
        // Ask the open handle, not the path: its answer is about the bytes this
        // call would read, so a concurrent replacement cannot change it.
        let size = file
            .metadata()
            .map_err(|error| missing_or_io(requested, &path, error))?
            .len();
        if size > u64::try_from(limit).unwrap_or(u64::MAX) {
            return Err(WorkspaceError::Io {
                path,
                source: std::io::Error::other(format!(
                    "file exceeds the file transfer limit of {limit} bytes"
                )),
            });
        }
        let mut bytes = Vec::new();
        // Read one byte past the limit: exactly `limit` bytes fit, and the extra
        // byte is what makes a file that grows after the metadata check visible
        // without reading it all.
        let bound = u64::try_from(limit.saturating_add(1)).unwrap_or(u64::MAX);
        let mut reader = std::io::Read::take(&mut file, bound);
        std::io::Read::read_to_end(&mut reader, &mut bytes)
            .map_err(|error| missing_or_io(requested, &path, error))?;
        if bytes.len() > limit {
            return Err(WorkspaceError::Io {
                path,
                source: std::io::Error::other(format!(
                    "file exceeds the file transfer limit of {limit} bytes"
                )),
            });
        }

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

    /// [`Workspace::read_unobserved`], proving the very handle the bytes come from is not
    /// a credential. `resolve` refuses a credential by path, but an ungated writer can swap
    /// the leaf for a credential alias between that resolution and the open, and a mutating
    /// tool's planning read must not materialize those bytes (the hunk match would become a
    /// content oracle). The credential identity check therefore runs on the opened handle,
    /// as the search capability does, and the bytes are read from that same handle.
    pub fn read_unobserved_checked(
        &self,
        requested: &str,
        cancel: &p1_contracts::CancellationToken,
    ) -> Result<Snapshot, WorkspaceError> {
        let refusal = |message: String| WorkspaceError::Io {
            path: self.spelling(requested),
            source: std::io::Error::new(std::io::ErrorKind::PermissionDenied, message),
        };
        let policy = CredentialPolicy::new(self.credential_home.as_deref(), &xdg_credentials());
        if let Err(message) = policy.refuse(self, requested) {
            return Err(refusal(message));
        }
        let resolved = self.resolve(requested)?;
        if policy.refuses(&resolved) {
            return Err(refusal(credential_refusal(&self.display(&resolved))));
        }
        let (file, path) = self.open_file_at_with_path(&resolved)?;
        let index = ProtectedIndex::build(&policy, cancel).map_err(|_| WorkspaceError::Io {
            path: path.clone(),
            source: std::io::Error::other("cancelled"),
        })?;
        let metadata = file.metadata().map_err(|source| WorkspaceError::Io {
            path: path.clone(),
            source,
        })?;
        if policy.refuses_opened(&path, &file)
            || index.refuses_metadata(&metadata)
            || index.refuses_current_exact(&policy, &metadata)
        {
            return Err(refusal(credential_refusal(&self.display(&path))));
        }
        self.snapshot_from_file(requested, file, path)
    }

    /// Read one already-opened regular file into a [`Snapshot`], bounded by the mutation
    /// file-size limit. The opened handle is the one the caller validated, so no path is
    /// resolved a second time between the check and the bytes.
    fn snapshot_from_file(
        &self,
        requested: &str,
        file: File,
        path: PathBuf,
    ) -> Result<Snapshot, WorkspaceError> {
        let display = self.display(&path);
        let limit = crate::commit::MAX_FILE_BYTES;
        if file
            .metadata()
            .map_err(|error| missing_or_io(requested, &path, error))?
            .len()
            > limit
        {
            return Err(size_error(&path, limit));
        }
        let mut bytes = Vec::new();
        std::io::Read::read_to_end(&mut file.take(limit + 1), &mut bytes)
            .map_err(|error| missing_or_io(requested, &path, error))?;
        if bytes.len() as u64 > limit {
            return Err(size_error(&path, limit));
        }
        let content_hash = hash_of(&bytes);
        Ok(Snapshot {
            path: display,
            bytes: Arc::from(bytes),
            content_hash,
        })
    }
}

/// The path the `O_NOFOLLOW` walk below opens. [`Workspace::resolve`] returns a lexical
/// path when the leaf does not exist, so an in-workspace directory symlink can remain in
/// an intermediate component and the walk, which refuses a symlink on the way, would
/// report it as `NotADirectory`. Canonicalize the deepest existing directory ancestor
/// (the one `resolve` verified stays inside) and re-join the rest as names to open: a
/// dangling leaf under such a link is then reached and reported as the link it is.
fn canonical_walk_path(workspace: &Workspace, path: &Path) -> Result<PathBuf, WorkspaceError> {
    let root = workspace.root();
    if path == root {
        return Ok(root.to_path_buf());
    }
    let mut ancestor = path.parent().unwrap_or(root);
    while ancestor != root && !std::fs::metadata(ancestor).is_ok_and(|metadata| metadata.is_dir()) {
        ancestor = ancestor.parent().unwrap_or(root);
    }
    let canonical = ancestor
        .canonicalize()
        .map_err(|source| WorkspaceError::Io {
            path: path.to_path_buf(),
            source,
        })?;
    if !canonical.starts_with(root) {
        return Err(WorkspaceError::OutsideWorkspace {
            requested: path.display().to_string(),
        });
    }
    let suffix = path
        .strip_prefix(ancestor)
        .unwrap_or_else(|_| Path::new(""));
    Ok(canonical.join(suffix))
}

/// Walk a resolved path from the canonical root, holding every directory fd
/// and refusing a swapped symlink in any component. The final link is opened as
/// a link for stat; list requires a directory and therefore rejects it.
fn open_checked_path(
    workspace: &Workspace,
    requested: &str,
    path: &Path,
    final_flags: OFlags,
) -> Result<OwnedFd, WorkspaceError> {
    let walk = canonical_walk_path(workspace, path)?;
    let relative =
        walk.strip_prefix(workspace.root())
            .map_err(|_| WorkspaceError::OutsideWorkspace {
                requested: path.display().to_string(),
            })?;
    let mut directory = rustix::fs::openat(
        CWD,
        workspace.root(),
        OFlags::PATH | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(|error| io_at(path, error))?;
    let mut parts = relative.components().peekable();
    while let Some(component) = parts.next() {
        let Component::Normal(name) = component else {
            return Err(WorkspaceError::NotADirectory(path.to_path_buf()));
        };
        let flags = if parts.peek().is_some() {
            OFlags::PATH | OFlags::DIRECTORY
        } else {
            final_flags
        } | OFlags::NOFOLLOW
            | OFlags::CLOEXEC;
        directory =
            rustix::fs::openat(&directory, name, flags, Mode::empty()).map_err(|error| {
                if error == Errno::NOTDIR {
                    WorkspaceError::NotADirectory(path.to_path_buf())
                } else {
                    missing_or_io(requested, path, error.into())
                }
            })?;
    }
    // O_PATH|NOFOLLOW can open a symlink itself. A resolved, previously
    // existing path that was swapped for a live symlink must not be stat'd as
    // the new link (a genuinely dangling leaf still reports Symlink).
    if final_flags == OFlags::PATH
        && rustix::fs::FileType::from_raw_mode(
            rustix::fs::fstat(&directory)
                .map_err(|error| io_at(path, error))?
                .st_mode,
        ) == rustix::fs::FileType::Symlink
        && path.exists()
    {
        return Err(WorkspaceError::NotADirectory(path.to_path_buf()));
    }
    Ok(directory)
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
fn io_at(path: &Path, error: Errno) -> WorkspaceError {
    WorkspaceError::Io {
        path: path.to_path_buf(),
        source: error.into(),
    }
}

fn size_error(path: &Path, limit: u64) -> WorkspaceError {
    WorkspaceError::Io {
        path: path.to_path_buf(),
        source: std::io::Error::other(format!("file exceeds {limit} byte snapshot limit")),
    }
}

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ObservedFiles;
    use rustix::fs::{CWD, Mode};
    use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};
    use std::sync::mpsc;
    use std::time::Duration;

    #[test]
    fn in_workspace_symlinks_read_and_outside_symlink_is_refused() {
        let directory = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::create_dir(directory.path().join("documentation")).unwrap();
        std::fs::write(directory.path().join("documentation/x.md"), b"inside\n").unwrap();
        std::fs::write(outside.path().join("secret.md"), b"outside\n").unwrap();
        symlink(
            directory.path().join("documentation"),
            directory.path().join("docs"),
        )
        .unwrap();
        symlink(
            directory.path().join("documentation/x.md"),
            directory.path().join("shortcut.md"),
        )
        .unwrap();
        symlink(
            outside.path().join("secret.md"),
            directory.path().join("external.md"),
        )
        .unwrap();
        let workspace = Workspace::new(directory.path()).unwrap();

        assert_eq!(
            workspace
                .read("docs/x.md", &ObservedFiles::new())
                .unwrap()
                .read(0, 100),
            b"inside\n"
        );
        assert_eq!(
            workspace
                .read("shortcut.md", &ObservedFiles::new())
                .unwrap()
                .read(0, 100),
            b"inside\n"
        );
        assert!(matches!(
            workspace.read("external.md", &ObservedFiles::new()),
            Err(WorkspaceError::OutsideWorkspace { .. })
        ));
    }

    #[test]
    fn reading_file_in_search_only_directory_succeeds() {
        let directory = tempfile::tempdir().unwrap();
        if directory.path().metadata().unwrap().uid() == 0 {
            return;
        }
        let subdirectory = directory.path().join("sub");
        std::fs::create_dir(&subdirectory).unwrap();
        std::fs::write(subdirectory.join("file"), b"readable\n").unwrap();
        std::fs::set_permissions(&subdirectory, std::fs::Permissions::from_mode(0o111)).unwrap();
        let workspace = Workspace::new(directory.path()).unwrap();

        let result = workspace.read("sub/file", &ObservedFiles::new());

        std::fs::set_permissions(&subdirectory, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(result.unwrap().read(0, 100), b"readable\n");
    }

    #[test]
    fn swapped_directory_cannot_be_opened_for_stat_or_listing() {
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("sub")).unwrap();
        std::fs::write(outside.path().join("sentinel"), b"secret").unwrap();
        let workspace = Workspace::new(dir.path()).unwrap();
        let checked = workspace.resolve("sub").unwrap();
        std::fs::rename(dir.path().join("sub"), dir.path().join("saved")).unwrap();
        symlink(outside.path(), dir.path().join("sub")).unwrap();
        for flags in [OFlags::PATH, OFlags::RDONLY | OFlags::DIRECTORY] {
            assert!(super::open_checked_path(&workspace, "sub", &checked, flags).is_err());
        }
        assert!(
            !workspace
                .list("sub")
                .is_ok_and(|entries| entries.iter().any(|entry| entry.name == "sentinel"))
        );
    }

    #[test]
    fn a_dangling_leaf_under_a_directory_symlink_reports_the_link() {
        use std::os::unix::fs::symlink;
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("real")).unwrap();
        symlink(dir.path().join("real"), dir.path().join("alias")).unwrap();
        symlink("missing", dir.path().join("real/leaf")).unwrap();
        let workspace = Workspace::new(dir.path()).unwrap();

        // `resolve` verifies `alias` but leaves the lexical path (the leaf does not
        // exist); the walk must follow the verified ancestor and reach the leaf as the
        // dangling link it is, not refuse the intermediate symlink.
        assert_eq!(
            workspace.stat("alias/leaf").unwrap().kind,
            FileKind::Symlink
        );
    }

    #[test]
    fn sparse_file_above_snapshot_limit_is_refused() {
        let directory = tempfile::tempdir().unwrap();
        let file = File::create(directory.path().join("large")).unwrap();
        file.set_len(crate::commit::MAX_FILE_BYTES + 1).unwrap();
        let workspace = Workspace::new(directory.path()).unwrap();
        assert!(matches!(
            workspace.read("large", &ObservedFiles::new()),
            Err(WorkspaceError::Io { .. })
        ));
    }

    #[test]
    fn reading_fifo_returns_without_blocking() {
        let directory = tempfile::tempdir().unwrap();
        let fifo = directory.path().join("pipe");
        rustix::fs::mkfifoat(CWD, &fifo, Mode::from_raw_mode(0o600)).unwrap();
        let workspace = Workspace::new(directory.path()).unwrap();
        let (sender, receiver) = mpsc::channel();
        std::thread::spawn(move || {
            let result = workspace.read("pipe", &ObservedFiles::new());
            sender
                .send(matches!(result, Err(WorkspaceError::NotADirectory(_))))
                .unwrap();
        });

        assert!(
            receiver.recv_timeout(Duration::from_secs(2)).unwrap(),
            "reading a FIFO should return the existing wrong-kind error"
        );
    }

    /// A planning read must refuse a hard link to a credential even though the plain
    /// unchecked read would return its bytes: the credential identity check runs on the
    /// opened handle, so a swap after the path refusal cannot leak the alias's contents.
    #[test]
    fn a_checked_read_refuses_a_hard_link_to_a_credential() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(directory.path().join(".config/keys")).unwrap();
        std::fs::write(
            directory.path().join(".config/keys/secret.key"),
            b"private-marker",
        )
        .unwrap();
        std::fs::hard_link(
            directory.path().join(".config/keys/secret.key"),
            directory.path().join("alias"),
        )
        .unwrap();
        let workspace = Workspace::new(directory.path())
            .unwrap()
            .with_credential_home(Some(directory.path().to_path_buf()));
        let cancel = p1_contracts::CancellationToken::new();

        assert!(workspace.read_unobserved("alias").is_ok());
        let error = workspace
            .read_unobserved_checked("alias", &cancel)
            .unwrap_err();
        assert!(
            format!("{error}").contains("refuses credential files"),
            "{error}"
        );
    }

    #[test]
    fn bounded_read_refuses_a_file_beyond_its_limit() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(directory.path().join("exact.txt"), b"0123456789").unwrap();
        std::fs::write(directory.path().join("over.txt"), b"0123456789").unwrap();
        let workspace = Workspace::new(directory.path()).unwrap();

        // Exactly the limit is read whole.
        let exact = workspace
            .read_within("exact.txt", &ObservedFiles::new(), 10)
            .unwrap();
        assert_eq!(exact.read(0, usize::MAX), b"0123456789");

        // One byte over is refused, and the refusal names the reached bound.
        let error = match workspace.read_within("over.txt", &ObservedFiles::new(), 9) {
            Err(error) => error,
            Ok(_) => panic!("a ten-byte file is over a nine-byte limit"),
        };
        assert!(
            error.to_string().contains("file transfer limit"),
            "the refusal names the bound: {error}"
        );
    }
}
