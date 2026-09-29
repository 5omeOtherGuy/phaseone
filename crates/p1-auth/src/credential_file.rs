//! Every credential file is read, locked and replaced through a pinned directory
//! handle (issue #484).
//!
//! A credential lives in a directory: p1's own store directory (owner-only, 0700) or
//! another tool's login directory (`~/.claude`, `~/.codex`, …). Before anything is
//! read or written, that directory and every directory above it — along the path as
//! written and along the path it resolves to — must be owned by this user or by root
//! and writable by nobody else. A sticky directory such as `/tmp` is accepted, and so
//! is a directory writable by the user's own primary group, which distributions with
//! user-private groups create. The directory is then opened once and every later step
//! goes through that handle (`openat`, `renameat`), so a directory renamed on the path
//! afterwards cannot redirect a read or a write.
//!
//! - A file is opened without following a symlink, must be a regular file owned by this
//!   user, and is read with a size cap; a FIFO or a device is refused before it can
//!   block. In p1's own store it must also be private (0600) and have no other link.
//! - The lock is the directory handle itself plus the sibling `.lock` file older p1
//!   processes lock; the lock file must still be the file that was locked when it is
//!   taken. Replacing the lock file therefore never lets a second p1 rotation in: the
//!   directory lock still holds it out.
//! - A replacement is staged in a new file (`O_CREAT|O_EXCL`, 0600, never an existing
//!   leaf), reserved before any network request, synced, checked to still be the file
//!   written, renamed into place, and the directory is synced.
//! - Rotated credentials that could not be published are kept as `.<name>.p1-unsaved`
//!   and adopted by the next refresh under the lock, unless the file changed since.
//!
//! Linux-only, like the rest of the crate: `std::os::unix` and `rustix`.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::fd::OwnedFd;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use p1_provider_http::{LOCK_PATIENCE, lock_exclusive};
use rustix::fs::{AtFlags, CWD, FileType, Mode, OFlags, Stat};
use rustix::io::Errno;

/// The largest credential file p1 reads. Real logins are a few KiB; anything bigger
/// is not one, and reading it whole would let a hostile file exhaust memory.
pub(crate) const MAX_CREDENTIAL_FILE: u64 = 1 << 20;

/// Which rule the directory itself follows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DirKind {
    /// p1's own store directory: nobody but the owner may even enter it.
    Private,
    /// Another tool's login directory: its mode is the tool's, but nobody else may
    /// write into it.
    Borrowed,
}

/// Why a credential file operation failed. A message names a path at most, never
/// content.
#[derive(Debug)]
pub(crate) enum FileError {
    /// The file or its directory does not exist.
    Missing,
    /// It exists but must not be used; the message says why and what to do.
    Refused(String),
    /// Any other I/O failure; the caller words it.
    Io,
}

impl From<Errno> for FileError {
    fn from(errno: Errno) -> Self {
        if errno == Errno::NOENT {
            FileError::Missing
        } else {
            FileError::Io
        }
    }
}

impl From<std::io::Error> for FileError {
    fn from(error: std::io::Error) -> Self {
        if error.kind() == std::io::ErrorKind::NotFound {
            FileError::Missing
        } else {
            FileError::Io
        }
    }
}

/// A validated, opened credential directory.
pub(crate) struct CredentialDir {
    fd: OwnedFd,
    path: PathBuf,
    kind: DirKind,
}

impl CredentialDir {
    /// Open an existing directory after checking it and every directory above it.
    pub(crate) fn open(path: &Path, kind: DirKind) -> Result<Self, FileError> {
        let absolute = std::path::absolute(path)?;
        check_chain(&absolute)?;
        let canonical = std::fs::canonicalize(&absolute)?;
        check_chain(&canonical)?;
        let fd = rustix::fs::openat(
            CWD,
            &canonical,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(|errno| match errno {
            Errno::NOTDIR | Errno::LOOP => {
                FileError::Refused(format!("{} is not a directory", absolute.display()))
            }
            other => FileError::from(other),
        })?;
        let dir = Self {
            fd,
            path: absolute,
            kind,
        };
        dir.check_self()?;
        Ok(dir)
    }

    /// p1's store directory, created 0700 (with any missing parents) when it does not
    /// exist. An existing directory keeps the mode its owner chose and is refused when
    /// that mode lets anyone else in: it is never silently tightened (spec §6).
    pub(crate) fn create_private(path: &Path) -> Result<Self, FileError> {
        match std::fs::symlink_metadata(path) {
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
                if let Some(parent) = path.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                match std::fs::DirBuilder::new().mode(0o700).create(path) {
                    Ok(()) => {
                        // The umask may have taken bits away; it can never add any, but
                        // the directory this crate creates must be exactly 0700.
                        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))?;
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                    Err(error) => return Err(error.into()),
                }
            }
            Err(error) => return Err(error.into()),
        }
        Self::open(path, DirKind::Private)
    }

    fn display(&self, name: &str) -> String {
        self.path.join(name).display().to_string()
    }

    /// Check the opened directory itself: a directory, owned by this user, and private
    /// (p1's store) or writable by nobody else (a borrowed login directory).
    fn check_self(&self) -> Result<(), FileError> {
        let stat = rustix::fs::fstat(&self.fd)?;
        let path = self.path.display();
        if FileType::from_raw_mode(stat.st_mode) != FileType::Directory {
            return Err(FileError::Refused(format!("{path} is not a directory")));
        }
        if stat.st_uid != euid() {
            return Err(FileError::Refused(format!(
                "{path} is owned by another user"
            )));
        }
        let mode = stat.st_mode & 0o7777;
        match self.kind {
            DirKind::Private if mode & 0o077 != 0 => Err(FileError::Refused(format!(
                "{path} is group/world-accessible (mode {:o}); chmod 700 it",
                mode & 0o777
            ))),
            DirKind::Private => Ok(()),
            DirKind::Borrowed => writable_by_others(&self.path, mode, stat.st_gid),
        }
    }

    /// Read one file of this directory: no symlink, a regular file owned by this user
    /// (and private with one link in p1's store), at most [`MAX_CREDENTIAL_FILE`] bytes.
    pub(crate) fn read(&self, name: &str) -> Result<Vec<u8>, FileError> {
        Ok(self.read_with_stat(name)?.0)
    }

    fn read_with_stat(&self, name: &str) -> Result<(Vec<u8>, Stat), FileError> {
        let fd = rustix::fs::openat(
            &self.fd,
            name,
            OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::NOCTTY | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(|errno| self.open_error(name, errno))?;
        let stat = rustix::fs::fstat(&fd)?;
        self.check_leaf(name, &stat)?;
        let too_large = || {
            FileError::Refused(format!(
                "{} is larger than {} KiB and is not a credential file",
                self.display(name),
                MAX_CREDENTIAL_FILE / 1024
            ))
        };
        if u64::try_from(stat.st_size).unwrap_or(u64::MAX) > MAX_CREDENTIAL_FILE {
            return Err(too_large());
        }
        let mut bytes = Vec::new();
        File::from(fd)
            .take(MAX_CREDENTIAL_FILE + 1)
            .read_to_end(&mut bytes)?;
        if bytes.len() as u64 > MAX_CREDENTIAL_FILE {
            return Err(too_large());
        }
        Ok((bytes, stat))
    }

    fn open_error(&self, name: &str, errno: Errno) -> FileError {
        match errno {
            Errno::LOOP => FileError::Refused(format!(
                "{} is a symbolic link; a credential file must be a regular file",
                self.display(name)
            )),
            other => FileError::from(other),
        }
    }

    fn check_leaf(&self, name: &str, stat: &Stat) -> Result<(), FileError> {
        let path = self.display(name);
        if FileType::from_raw_mode(stat.st_mode) != FileType::RegularFile {
            return Err(FileError::Refused(format!("{path} is not a regular file")));
        }
        if stat.st_uid != euid() {
            return Err(FileError::Refused(format!(
                "{path} is owned by another user"
            )));
        }
        if self.kind == DirKind::Private {
            let mode = stat.st_mode & 0o777;
            if mode & 0o077 != 0 {
                return Err(FileError::Refused(format!(
                    "{path} is group/world-accessible (mode {mode:o}); chmod 600 it"
                )));
            }
            if stat.st_nlink != 1 {
                return Err(FileError::Refused(format!(
                    "{path} has another hard link; a credential file must have exactly one"
                )));
            }
        }
        Ok(())
    }

    /// Take the lock for this directory's credential file: the directory handle itself,
    /// then the sibling `lock_name` file older p1 processes lock. Both waits share one
    /// [`LOCK_PATIENCE`] and never block the thread.
    pub(crate) async fn lock(&self, lock_name: &str) -> Result<CredentialLock, FileError> {
        let give_up = tokio::time::Instant::now() + LOCK_PATIENCE;
        let remaining = || give_up.saturating_duration_since(tokio::time::Instant::now());
        let handle = rustix::fs::openat(
            &self.fd,
            ".",
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
            Mode::empty(),
        )?;
        let directory = lock_exclusive(File::from(handle), remaining())
            .await
            .map_err(|_| FileError::Io)?;
        for _ in 0..8 {
            let fd = rustix::fs::openat(
                &self.fd,
                lock_name,
                OFlags::RDWR
                    | OFlags::CREATE
                    | OFlags::NOFOLLOW
                    | OFlags::NONBLOCK
                    | OFlags::CLOEXEC,
                Mode::from_raw_mode(0o600),
            )
            .map_err(|errno| self.open_error(lock_name, errno))?;
            let stat = rustix::fs::fstat(&fd)?;
            if FileType::from_raw_mode(stat.st_mode) != FileType::RegularFile
                || stat.st_uid != euid()
            {
                return Err(FileError::Refused(format!(
                    "{} is not a regular file owned by this user",
                    self.display(lock_name)
                )));
            }
            let file = lock_exclusive(File::from(fd), remaining())
                .await
                .map_err(|_| FileError::Io)?;
            // A lock file removed or replaced while this process waited for it locks an
            // inode nobody else will lock again: take the one the path names now.
            if self.is_file(lock_name, &stat) {
                return Ok(CredentialLock {
                    _directory: directory,
                    _file: file,
                });
            }
        }
        Err(FileError::Refused(format!(
            "{} keeps being replaced while it is locked",
            self.display(lock_name)
        )))
    }

    /// Whether `name` is (still) exactly the file `stat` describes.
    fn is_file(&self, name: &str, stat: &Stat) -> bool {
        rustix::fs::statat(&self.fd, name, AtFlags::SYMLINK_NOFOLLOW)
            .is_ok_and(|now| same_file(&now, stat))
    }

    /// A new staging file for `target`: created here and nowhere else (`O_EXCL`, no
    /// symlink followed), 0600 from creation, with `reserve` bytes already written so
    /// a full disk shows up BEFORE a token is rotated.
    pub(crate) fn stage(&self, target: &str, reserve: usize) -> Result<Staging<'_>, FileError> {
        for _ in 0..16 {
            let name = staging_name(target);
            match rustix::fs::openat(
                &self.fd,
                name.as_str(),
                OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::from_raw_mode(0o600),
            ) {
                Ok(fd) => {
                    let identity = rustix::fs::fstat(&fd)?;
                    let mut staging = Staging {
                        dir: self,
                        target: target.to_string(),
                        name,
                        file: File::from(fd),
                        identity,
                        state: StagingState::Created,
                    };
                    staging.reserve(reserve)?;
                    return Ok(staging);
                }
                Err(Errno::EXIST) => continue,
                Err(errno) => return Err(errno.into()),
            }
        }
        Err(FileError::Refused(format!(
            "no staging file could be created beside {}",
            self.display(target)
        )))
    }

    /// Adopt a rotated login a previous refresh could not publish (see
    /// [`Staging::keep_for_recovery`]), when `valid` accepts it and `target` has not
    /// changed since it was kept. A stale copy is removed. Call it under the lock.
    pub(crate) fn recover(&self, target: &str, valid: impl Fn(&[u8]) -> bool) -> bool {
        let name = recovery_name(target);
        let Ok((bytes, kept)) = self.read_with_stat(&name) else {
            return false;
        };
        let target_is_older = match rustix::fs::statat(&self.fd, target, AtFlags::SYMLINK_NOFOLLOW)
        {
            Ok(current) => modified(&current) <= modified(&kept),
            Err(Errno::NOENT) => true,
            Err(_) => false,
        };
        if !(target_is_older && valid(&bytes)) {
            if self.is_file(&name, &kept) {
                let _ = rustix::fs::unlinkat(&self.fd, name.as_str(), AtFlags::empty());
            }
            return false;
        }
        if rustix::fs::renameat(&self.fd, name.as_str(), &self.fd, target).is_err() {
            return false;
        }
        let _ = rustix::fs::fsync(&self.fd);
        true
    }
}

/// The held lock of one credential directory. Dropping it releases both locks.
pub(crate) struct CredentialLock {
    _directory: File,
    _file: File,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StagingState {
    /// Created by this process; removed on drop.
    Created,
    /// Holds the complete, synced new contents.
    Written,
    /// Renamed into place or kept for recovery: nothing to clean up.
    Done,
    /// No longer the file this process created: never touched again.
    Foreign,
}

/// One staged replacement of a credential file.
pub(crate) struct Staging<'d> {
    dir: &'d CredentialDir,
    target: String,
    name: String,
    file: File,
    identity: Stat,
    state: StagingState,
}

/// Why a staged replacement was not published.
#[derive(Debug)]
pub(crate) enum PublishError {
    /// Nothing was published; the message (if any) says why the directory or the
    /// staging file may no longer be trusted.
    NotPublished(Option<String>),
    /// The new file is in place, but the directory could not be synced to disk.
    NotDurable,
}

impl Staging<'_> {
    fn reserve(&mut self, bytes: usize) -> Result<(), FileError> {
        const CHUNK: [u8; 4096] = [b' '; 4096];
        let mut left = bytes;
        while left > 0 {
            let step = left.min(CHUNK.len());
            self.file.write_all(&CHUNK[..step])?;
            left -= step;
        }
        Ok(())
    }

    /// Write, sync and rename `contents` over the target, while the directory is still
    /// what it was checked to be and the staging file still the one written.
    pub(crate) fn publish(&mut self, contents: &[u8]) -> Result<(), PublishError> {
        let written = (|| -> std::io::Result<()> {
            self.file.seek(SeekFrom::Start(0))?;
            self.file.write_all(contents)?;
            self.file.set_len(contents.len() as u64)?;
            self.file.sync_all()
        })();
        if written.is_err() {
            return Err(PublishError::NotPublished(None));
        }
        self.state = StagingState::Written;
        if let Err(FileError::Refused(reason)) = self.dir.check_self() {
            return Err(PublishError::NotPublished(Some(reason)));
        }
        if !self.dir.is_file(&self.name, &self.identity) {
            self.state = StagingState::Foreign;
            return Err(PublishError::NotPublished(Some(format!(
                "the staging file beside {} was replaced before it was published",
                self.dir.display(&self.target)
            ))));
        }
        if rustix::fs::renameat(
            &self.dir.fd,
            self.name.as_str(),
            &self.dir.fd,
            self.target.as_str(),
        )
        .is_err()
        {
            return Err(PublishError::NotPublished(None));
        }
        self.state = StagingState::Done;
        rustix::fs::fsync(&self.dir.fd).map_err(|_| PublishError::NotDurable)
    }

    /// Keep a written but unpublished replacement (rotated tokens the server already
    /// issued) as `.<target>.p1-unsaved` for [`CredentialDir::recover`]. Returns
    /// whether it was kept.
    pub(crate) fn keep_for_recovery(&mut self) -> bool {
        if self.state != StagingState::Written || !self.dir.is_file(&self.name, &self.identity) {
            return false;
        }
        let kept = rustix::fs::renameat(
            &self.dir.fd,
            self.name.as_str(),
            &self.dir.fd,
            recovery_name(&self.target).as_str(),
        )
        .is_ok();
        if kept {
            self.state = StagingState::Done;
            let _ = rustix::fs::fsync(&self.dir.fd);
        }
        kept
    }
}

impl Drop for Staging<'_> {
    /// Remove the staging file only while it is still the one this process created.
    fn drop(&mut self) {
        if matches!(self.state, StagingState::Created | StagingState::Written)
            && self.dir.is_file(&self.name, &self.identity)
        {
            let _ = rustix::fs::unlinkat(&self.dir.fd, self.name.as_str(), AtFlags::empty());
        }
    }
}

/// `.<target>.p1-<pid>-<nanos>-<counter>.tmp`: beside the target (a rename stays in
/// one directory), hidden, and named after it so the workspace policy refuses it with
/// the credential file's family.
fn staging_name(target: &str) -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let counter = COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.subsec_nanos())
        .unwrap_or(0);
    format!(
        ".{target}.p1-{}-{nanos:09}-{counter}.tmp",
        std::process::id()
    )
}

fn recovery_name(target: &str) -> String {
    format!(".{target}.p1-unsaved")
}

fn same_file(a: &Stat, b: &Stat) -> bool {
    a.st_dev == b.st_dev && a.st_ino == b.st_ino
}

fn modified(stat: &Stat) -> (i64, i64) {
    (stat.st_mtime as i64, stat.st_mtime_nsec as i64)
}

fn euid() -> u32 {
    rustix::process::geteuid().as_raw()
}

fn egid() -> u32 {
    rustix::process::getegid().as_raw()
}

/// Every directory from the root down to `path` (inclusive), as the path is spelled:
/// a directory or a symlink owned by this user or root, and no directory writable by
/// anyone else.
fn check_chain(path: &Path) -> Result<(), FileError> {
    let me = euid();
    let mut prefixes: Vec<&Path> = path.ancestors().collect();
    prefixes.reverse();
    for prefix in prefixes {
        let metadata = std::fs::symlink_metadata(prefix)?;
        let owner = metadata.uid();
        if owner != me && owner != 0 {
            return Err(FileError::Refused(format!(
                "{} is owned by another user",
                prefix.display()
            )));
        }
        let kind = metadata.file_type();
        if kind.is_symlink() {
            continue;
        }
        if !kind.is_dir() {
            return Err(FileError::Refused(format!(
                "{} is not a directory",
                prefix.display()
            )));
        }
        writable_by_others(prefix, metadata.mode() & 0o7777, metadata.gid())?;
    }
    Ok(())
}

/// A directory anyone but its owner can write to lets them replace a credential file:
/// refused, unless it is sticky (only an entry's owner may rename it) or its group is
/// this user's own primary group.
fn writable_by_others(path: &Path, mode: u32, gid: u32) -> Result<(), FileError> {
    let sticky = mode & 0o1000 != 0;
    if mode & 0o002 != 0 && !sticky {
        return Err(FileError::Refused(format!(
            "{} is writable by every user; chmod o-w it",
            path.display()
        )));
    }
    if mode & 0o020 != 0 && !sticky && gid != egid() {
        return Err(FileError::Refused(format!(
            "{} is writable by its group; chmod g-w it",
            path.display()
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::future::Future;
    use std::os::unix::fs::PermissionsExt;

    fn mode(path: &Path) -> u32 {
        std::fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    fn set_mode(path: &Path, mode: u32) {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
    }

    fn private_dir() -> (tempfile::TempDir, CredentialDir) {
        let scratch = tempfile::tempdir().unwrap();
        let dir = CredentialDir::create_private(&scratch.path().join("p1")).unwrap();
        (scratch, dir)
    }

    fn names(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    #[test]
    fn a_created_store_directory_is_0700_and_an_existing_wider_one_is_refused() {
        let scratch = tempfile::tempdir().unwrap();
        let path = scratch.path().join("config/p1");
        CredentialDir::create_private(&path).unwrap();
        assert_eq!(mode(&path), 0o700);

        set_mode(&path, 0o750);
        match CredentialDir::create_private(&path) {
            Err(FileError::Refused(reason)) => assert!(reason.contains("chmod 700"), "{reason}"),
            other => panic!("expected a refusal, got {:?}", other.err()),
        }
        assert_eq!(mode(&path), 0o750, "never silently tightened");
    }

    #[test]
    fn a_symlinked_leaf_is_refused_and_its_target_left_alone() {
        let (scratch, dir) = private_dir();
        let victim = scratch.path().join("victim");
        std::fs::write(&victim, "victim").unwrap();
        std::os::unix::fs::symlink(&victim, scratch.path().join("p1/auth.json")).unwrap();

        assert!(matches!(dir.read("auth.json"), Err(FileError::Refused(_))));
        assert_eq!(std::fs::read_to_string(&victim).unwrap(), "victim");
    }

    #[test]
    fn a_fifo_is_refused_without_blocking() {
        let (scratch, dir) = private_dir();
        rustix::fs::mkfifoat(
            CWD,
            &scratch.path().join("p1/auth.json"),
            Mode::from_raw_mode(0o600),
        )
        .unwrap();
        assert!(matches!(dir.read("auth.json"), Err(FileError::Refused(_))));
    }

    #[test]
    fn an_oversized_file_is_refused() {
        let (scratch, dir) = private_dir();
        let path = scratch.path().join("p1/auth.json");
        let file = std::fs::File::create(&path).unwrap();
        file.set_len(MAX_CREDENTIAL_FILE + 1).unwrap();
        set_mode(&path, 0o600);
        assert!(matches!(dir.read("auth.json"), Err(FileError::Refused(_))));
    }

    #[test]
    fn a_hard_linked_store_file_is_refused() {
        let (scratch, dir) = private_dir();
        let path = scratch.path().join("p1/auth.json");
        std::fs::write(&path, "{}").unwrap();
        set_mode(&path, 0o600);
        std::fs::hard_link(&path, scratch.path().join("elsewhere")).unwrap();
        assert!(matches!(dir.read("auth.json"), Err(FileError::Refused(_))));
    }

    #[test]
    fn a_directory_writable_by_every_user_is_refused_as_an_ancestor() {
        let scratch = tempfile::tempdir().unwrap();
        let open = scratch.path().join("open");
        std::fs::create_dir(&open).unwrap();
        std::fs::create_dir(open.join("login")).unwrap();
        set_mode(&open, 0o777);
        assert!(matches!(
            CredentialDir::open(&open.join("login"), DirKind::Borrowed),
            Err(FileError::Refused(_))
        ));
        // A sticky one (like /tmp) is fine.
        set_mode(&open, 0o1777);
        assert!(CredentialDir::open(&open.join("login"), DirKind::Borrowed).is_ok());
    }

    #[test]
    fn a_renamed_directory_cannot_redirect_a_publication() {
        let scratch = tempfile::tempdir().unwrap();
        let path = scratch.path().join("p1");
        let dir = CredentialDir::create_private(&path).unwrap();
        let mut staging = dir.stage("auth.json", 64).unwrap();
        // The directory moves and another one takes its name.
        std::fs::rename(&path, scratch.path().join("moved")).unwrap();
        std::fs::create_dir(&path).unwrap();

        staging.publish(b"{}\n").unwrap();
        assert_eq!(
            std::fs::read_to_string(scratch.path().join("moved/auth.json")).unwrap(),
            "{}\n"
        );
        assert!(names(&path).is_empty(), "nothing lands in the replacement");
    }

    #[test]
    fn a_publication_is_private_synced_and_leaves_no_staging_file() {
        let (scratch, dir) = private_dir();
        let mut staging = dir.stage("auth.json", 4096).unwrap();
        staging.publish(b"{\"a\":1}\n").unwrap();
        drop(staging);
        let path = scratch.path().join("p1/auth.json");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "{\"a\":1}\n");
        assert_eq!(mode(&path), 0o600);
        assert_eq!(names(&scratch.path().join("p1")), vec!["auth.json"]);
    }

    #[test]
    fn an_abandoned_staging_file_is_removed_but_a_foreign_one_is_not() {
        let (scratch, dir) = private_dir();
        let staging = dir.stage("auth.json", 16).unwrap();
        drop(staging);
        assert!(names(&scratch.path().join("p1")).is_empty());

        // Someone replaces the staging file: this process never removes the replacement.
        let staging = dir.stage("auth.json", 16).unwrap();
        let staged = scratch.path().join("p1").join(&staging.name);
        std::fs::remove_file(&staged).unwrap();
        std::fs::write(&staged, "theirs").unwrap();
        drop(staging);
        assert_eq!(std::fs::read_to_string(&staged).unwrap(), "theirs");
    }

    #[test]
    fn a_replaced_staging_file_is_never_published() {
        let (scratch, dir) = private_dir();
        let target = scratch.path().join("p1/auth.json");
        std::fs::write(&target, "old").unwrap();
        let mut staging = dir.stage("auth.json", 16).unwrap();
        let staged = scratch.path().join("p1").join(&staging.name);
        std::fs::rename(&staged, scratch.path().join("away")).unwrap();
        std::fs::write(&staged, "attacker").unwrap();

        assert!(matches!(
            staging.publish(b"new"),
            Err(PublishError::NotPublished(Some(_)))
        ));
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "old");
    }

    /// A staged replacement written and then kept, as a failed publication leaves it.
    fn keep(dir: &CredentialDir, contents: &[u8]) {
        let mut staging = dir.stage("auth.json", 16).unwrap();
        staging.file.seek(SeekFrom::Start(0)).unwrap();
        staging.file.set_len(0).unwrap();
        staging.file.write_all(contents).unwrap();
        staging.state = StagingState::Written;
        assert!(staging.keep_for_recovery());
    }

    fn age(path: &Path) {
        let old = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1);
        std::fs::File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_modified(old)
            .unwrap();
    }

    #[test]
    fn a_kept_replacement_is_adopted_when_valid_and_newer_and_dropped_otherwise() {
        let (scratch, dir) = private_dir();
        let target = scratch.path().join("p1/auth.json");
        let kept = scratch.path().join("p1/.auth.json.p1-unsaved");
        std::fs::write(&target, "old").unwrap();
        set_mode(&target, 0o600);
        age(&target);

        keep(&dir, b"new");
        assert!(!dir.recover("auth.json", |bytes| bytes == b"other"));
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "old");
        assert!(!kept.exists(), "an invalid copy is dropped");

        keep(&dir, b"new");
        assert!(dir.recover("auth.json", |bytes| bytes == b"new"));
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "new");
        assert!(!kept.exists());

        // The login changed after the copy was kept: the copy is stale.
        keep(&dir, b"stale");
        age(&kept);
        std::fs::write(&target, "relogged").unwrap();
        assert!(!dir.recover("auth.json", |_| true));
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "relogged");
        assert!(!kept.exists());
    }

    #[tokio::test(start_paused = true)]
    async fn the_directory_lock_serializes_even_when_the_lock_file_is_replaced() {
        let (scratch, dir) = private_dir();
        let _held = dir.lock("auth.json.lock").await.unwrap();
        let path = scratch.path().join("p1/auth.json.lock");
        std::fs::remove_file(&path).unwrap();

        let second = CredentialDir::open(&scratch.path().join("p1"), DirKind::Private).unwrap();
        let waiting = second.lock("auth.json.lock");
        tokio::pin!(waiting);
        let mut context = std::task::Context::from_waker(std::task::Waker::noop());
        assert!(
            waiting.as_mut().poll(&mut context).is_pending(),
            "the directory lock is still held"
        );
    }
}
