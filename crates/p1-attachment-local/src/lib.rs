//! The local [`AttachmentStore`] (ADR-0180): a host OS service, never a module.
//!
//! Layout: `<root>/objects/<sha256>` holds each attachment once, named by the lowercase
//! hex SHA-256 of its stored bytes, mode 0400, never deleted. `<root>` is the host's state
//! directory joined with `attachments` (ADR-0171); nothing is created until the first save.
//!
//! Writes go through a pinned directory handle (ADR-0108): the root and `objects` are
//! opened once per operation with `O_NOFOLLOW`, refused when another user can write them,
//! and every file is opened relative to that handle. A save stages a fresh exclusive file,
//! syncs it and links it under its address with `RENAME_NOREPLACE` (ADR-0111), so an
//! existing object is never replaced; finding one, the store re-verifies it instead. Every
//! read re-hashes the object against its address.

mod image;
mod sha256;

use std::ffi::OsString;
use std::fs::File;
use std::io::{Read, Write};
use std::os::fd::OwnedFd;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use p1_contracts::{
    AttachmentError, AttachmentId, AttachmentStore, BoxFuture, FileRef, ImageLimits, ImageRef,
    ImageTarget,
};
use p1_redact::SecretSet;
use rustix::fs::{AtFlags, Mode, OFlags, RenameFlags};
use rustix::io::Errno;

/// The largest file the store takes, so one attachment cannot fill the disk unbounded.
pub const MAX_FILE_BYTES: u64 = 64 * 1024 * 1024;

const OBJECTS: &str = "objects";

/// Content-addressed attachments under one root directory.
#[derive(Debug, Clone)]
pub struct LocalAttachmentStore {
    root: PathBuf,
    limits: ImageLimits,
    secrets: SecretSet,
}

impl LocalAttachmentStore {
    /// A store at `root`, with the default image limits. Creates nothing.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            limits: ImageLimits::default(),
            secrets: SecretSet::new(),
        }
    }

    /// Other image limits than the default.
    pub fn with_limits(mut self, limits: ImageLimits) -> Self {
        self.limits = limits;
        self
    }

    /// The host's credential values: a file name that quotes one is masked before it is
    /// stored in a reference (ADR-0108).
    pub fn with_secrets(mut self, secrets: SecretSet) -> Self {
        self.secrets = secrets;
        self
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn object_path(&self, id: &AttachmentId) -> PathBuf {
        self.root.join(OBJECTS).join(id.as_str())
    }
}

fn io(context: &str, error: impl std::fmt::Display) -> AttachmentError {
    AttachmentError::Io(format!("{context}: {error}"))
}

/// Run blocking store I/O off the executor.
async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> Result<T, AttachmentError> + Send + 'static,
) -> Result<T, AttachmentError> {
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|error| io("the store task failed", error))?
}

/// Open `path` as a directory without following a final symlink, creating it 0700 when
/// `create` and it is missing, and refuse it when a group or other user can write it.
fn open_dir(
    parent: Option<&OwnedFd>,
    path: &Path,
    create: bool,
) -> Result<OwnedFd, AttachmentError> {
    let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
    let open = || match parent {
        Some(dir) => rustix::fs::openat(dir, path, flags, Mode::empty()),
        None => rustix::fs::open(path, flags, Mode::empty()),
    };
    let fd = match open() {
        Err(Errno::NOENT) if create => {
            let made = match parent {
                Some(dir) => rustix::fs::mkdirat(dir, path, Mode::RWXU),
                None => std::fs::create_dir_all(path)
                    .map_err(|error| Errno::from_io_error(&error).unwrap_or(Errno::IO)),
            };
            match made {
                Ok(()) | Err(Errno::EXIST) => {}
                Err(error) => return Err(io(&format!("cannot create {}", path.display()), error)),
            }
            if parent.is_none() {
                // `create_dir_all` honours the umask; the root holds the user's files.
                rustix::fs::chmod(path, Mode::RWXU)
                    .map_err(|error| io(&format!("cannot restrict {}", path.display()), error))?;
            }
            open().map_err(|error| io(&format!("cannot open {}", path.display()), error))?
        }
        result => result.map_err(|error| io(&format!("cannot open {}", path.display()), error))?,
    };
    let stat = rustix::fs::fstat(&fd).map_err(|error| io("cannot inspect a directory", error))?;
    if stat.st_mode & 0o022 != 0 {
        return Err(AttachmentError::Io(format!(
            "{} is writable by other users; the store refuses it",
            path.display()
        )));
    }
    if stat.st_uid != rustix::process::geteuid().as_raw() {
        return Err(AttachmentError::Io(format!(
            "{} belongs to another user; the store refuses it",
            path.display()
        )));
    }
    Ok(fd)
}

/// The pinned `objects` directory under `root`.
fn objects(root: &Path, create: bool) -> Result<OwnedFd, AttachmentError> {
    let root_fd = open_dir(None, root, create)?;
    open_dir(Some(&root_fd), Path::new(OBJECTS), create)
}

/// A staging name no other save of this process or another uses at the same time; the
/// exclusive create refuses a collision anyway.
fn staging_name() -> OsString {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos())
        .unwrap_or(0);
    OsString::from(format!(
        ".staging-{}-{nanos}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ))
}

/// Read the object `id` through the pinned directory, refusing a symlink, a non-file and
/// one larger than `cap`, and verify it against its address.
fn read_object(dir: &OwnedFd, id: &AttachmentId, cap: u64) -> Result<Vec<u8>, AttachmentError> {
    let fd = match rustix::fs::openat(
        dir,
        id.as_str(),
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
        Mode::empty(),
    ) {
        Ok(fd) => fd,
        Err(Errno::NOENT) => return Err(AttachmentError::NotFound(id.clone())),
        Err(error) => return Err(io("cannot open an attachment", error)),
    };
    let stat = rustix::fs::fstat(&fd).map_err(|error| io("cannot inspect an attachment", error))?;
    if rustix::fs::FileType::from_raw_mode(stat.st_mode) != rustix::fs::FileType::RegularFile
        || stat.st_size as u64 > cap
    {
        return Err(AttachmentError::Corrupt(id.clone()));
    }
    let mut bytes = Vec::with_capacity(stat.st_size as usize);
    File::from(fd)
        .take(cap + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| io("cannot read an attachment", error))?;
    if sha256::sha256_hex(&bytes) != id.as_str() {
        return Err(AttachmentError::Corrupt(id.clone()));
    }
    Ok(bytes)
}

/// Store `bytes` under their address, unless an object with that address exists already
/// (and verifies). Returns the address.
fn put(root: &Path, bytes: &[u8]) -> Result<AttachmentId, AttachmentError> {
    let id = AttachmentId::new(sha256::sha256_hex(bytes))?;
    let dir = objects(root, true)?;
    if let Ok(existing) = read_object(&dir, &id, bytes.len() as u64) {
        debug_assert_eq!(existing, bytes);
        return Ok(id);
    }
    let staging = staging_name();
    let fd = rustix::fs::openat(
        &dir,
        &staging,
        OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        // The creating descriptor may write it; nobody can open it for writing again.
        Mode::RUSR,
    )
    .map_err(|error| io("cannot stage an attachment", error))?;
    let mut file = File::from(fd);
    let written = file
        .write_all(bytes)
        .and_then(|()| file.sync_all())
        .map_err(|error| io("cannot write an attachment", error));
    drop(file);
    let linked = written.and_then(|()| link_no_replace(&dir, &staging, id.as_str()));
    // The staging name goes either way: after a link it is a second name of the object.
    let _ = rustix::fs::unlinkat(&dir, &staging, AtFlags::empty());
    match linked {
        Ok(()) => {}
        // Another save linked the same content first: verify that one instead.
        Err(AttachmentError::Io(_)) if read_object(&dir, &id, bytes.len() as u64).is_ok() => {}
        Err(error) => return Err(error),
    }
    rustix::fs::fsync(&dir).map_err(|error| io("cannot sync the attachment directory", error))?;
    Ok(id)
}

/// Give `staging` the name `target` without ever replacing an existing `target`:
/// `RENAME_NOREPLACE`, or a hard link where the filesystem has no `renameat2`.
fn link_no_replace(dir: &OwnedFd, staging: &OsString, target: &str) -> Result<(), AttachmentError> {
    match rustix::fs::renameat_with(dir, staging, dir, target, RenameFlags::NOREPLACE) {
        Ok(()) => Ok(()),
        Err(Errno::INVAL | Errno::NOSYS) => {
            rustix::fs::linkat(dir, staging, dir, target, AtFlags::empty())
                .map_err(|error| io("cannot link an attachment", error))
        }
        Err(error) => Err(io("cannot link an attachment", error)),
    }
}

/// A display name with no path, no control characters and no credential, at most 255
/// characters: it is stored in history and shown to the model.
fn display_name(name: &str, secrets: &SecretSet) -> String {
    let base = name.rsplit(['/', '\\']).next().unwrap_or_default();
    let clean: String = base.chars().filter(|c| !c.is_control()).collect();
    // Mask before cutting, so a credential across the cut cannot leave its prefix.
    let masked: String = p1_redact::redact_with(&clean, secrets)
        .text
        .chars()
        .take(255)
        .collect();
    if masked.trim().is_empty() {
        "attachment".to_owned()
    } else {
        masked
    }
}

impl AttachmentStore for LocalAttachmentStore {
    fn limits(&self) -> ImageLimits {
        self.limits
    }

    fn save_image(
        &self,
        bytes: Vec<u8>,
        _name: Option<String>,
    ) -> BoxFuture<'_, Result<ImageRef, AttachmentError>> {
        let root = self.root.clone();
        let limits = self.limits;
        Box::pin(blocking(move || {
            let validated = image::validate(bytes, limits)?;
            let id = put(&root, &validated.bytes)?;
            Ok(ImageRef {
                id,
                media_type: validated.media_type,
                bytes: validated.bytes.len() as u64,
                width: validated.width,
                height: validated.height,
            })
        }))
    }

    fn read_image<'a>(
        &'a self,
        image: &'a ImageRef,
        target: Option<ImageTarget>,
    ) -> BoxFuture<'a, Result<Vec<u8>, AttachmentError>> {
        let root = self.root.clone();
        let image = image.clone();
        Box::pin(blocking(move || {
            if let Some(target) = target {
                // No re-encoding here: an image above the route's size is refused, not
                // shrunk (ADR-0180, a later slice adds per-route variants).
                if image.bytes > target.max_bytes {
                    return Err(AttachmentError::TooLarge {
                        actual: image.bytes,
                        limit: target.max_bytes,
                    });
                }
                if image.width > target.max_dimension || image.height > target.max_dimension {
                    return Err(AttachmentError::TooManyPixels {
                        width: image.width,
                        height: image.height,
                        limit: target.max_dimension,
                    });
                }
            }
            let dir = match objects(&root, false) {
                Ok(dir) => dir,
                // A store that never saved anything holds no object at all.
                Err(_) if !root.join(OBJECTS).exists() => {
                    return Err(AttachmentError::NotFound(image.id.clone()));
                }
                Err(error) => return Err(error),
            };
            let bytes = read_object(&dir, &image.id, image.bytes)?;
            // The reference must describe this object: its size and its format.
            if bytes.len() as u64 != image.bytes || image::sniff(&bytes) != Some(image.media_type) {
                return Err(AttachmentError::Corrupt(image.id.clone()));
            }
            Ok(bytes)
        }))
    }

    fn save_file(
        &self,
        bytes: Vec<u8>,
        name: String,
    ) -> BoxFuture<'_, Result<FileRef, AttachmentError>> {
        let root = self.root.clone();
        let name = display_name(&name, &self.secrets);
        Box::pin(blocking(move || {
            let len = bytes.len() as u64;
            if len > MAX_FILE_BYTES {
                return Err(AttachmentError::TooLarge {
                    actual: len,
                    limit: MAX_FILE_BYTES,
                });
            }
            let id = put(&root, &bytes)?;
            Ok(FileRef {
                id,
                name,
                bytes: len,
            })
        }))
    }

    fn file_path(&self, file: &FileRef) -> PathBuf {
        self.object_path(&file.id)
    }
}

#[cfg(test)]
mod tests {
    use super::display_name;
    use p1_redact::SecretSet;

    #[test]
    fn a_display_name_loses_its_path_control_characters_and_credentials() {
        let secrets = SecretSet::new();
        secrets.register("hunter2-registered-value");
        assert_eq!(display_name("../../etc/passwd", &secrets), "passwd");
        assert_eq!(display_name("C:\\x\\report.pdf", &secrets), "report.pdf");
        assert_eq!(display_name("a\nb\u{7}.txt", &secrets), "ab.txt");
        assert_eq!(display_name("", &secrets), "attachment");
        assert!(!display_name("hunter2-registered-value.txt", &secrets).contains("hunter2"));
    }
}
