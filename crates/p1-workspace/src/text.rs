//! Atomic file replacement and model-visible output bounding.

use rustix::fs::{AtFlags, CWD, FileType, Mode, OFlags};
use std::fs;
use std::io::{self, Write};
use std::os::fd::OwnedFd;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// Makes sibling temp names unique without a random-number dependency. The
/// counter is process-wide, the pid keeps concurrent processes apart.
static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Atomically replace `path` with `contents`, creating missing parent
/// directories.
///
/// The bytes go to a uniquely named sibling temp file, which is fsynced and
/// then renamed over the target in one step, so a reader never sees a partial
/// file. The target's permission bits are copied onto the replacement. The temp
/// file is removed if any step before the rename fails, so a failed write is a
/// no-op that leaves the target byte-identical.
///
/// The temp file is a sibling of `path`, so callers that resolved `path`
/// through [`crate::Workspace::resolve`] also keep the temp file inside the
/// workspace.
pub fn write_atomic(path: &Path, contents: &[u8]) -> io::Result<()> {
    write_atomic_named(path, contents, temp_name)
}

/// [`write_atomic`] with the temporary's name chosen by `name`, so a regression can force the
/// `O_EXCL` collision `write_atomic` handles without racing the process-wide counter.
fn write_atomic_named(
    path: &Path,
    contents: &[u8],
    name: impl FnOnce(&std::ffi::OsStr) -> std::ffi::OsString,
) -> io::Result<()> {
    let parent = match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent.to_path_buf(),
        _ => PathBuf::from("."),
    };
    fs::create_dir_all(&parent)?;

    let file_name = path.file_name().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "atomic write target has no file name",
        )
    })?;

    let dir = rustix::fs::openat(
        CWD,
        &parent,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )?;
    let existing_permissions = rustix::fs::openat(
        &dir,
        file_name,
        OFlags::PATH | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .ok()
    .and_then(|fd| fs::File::from(fd).metadata().ok())
    .filter(|meta| meta.is_file())
    .map(|meta| meta.permissions());
    let temp = name(file_name);
    let fd = rustix::fs::openat(
        &dir,
        temp.as_os_str(),
        OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::from_raw_mode(
            existing_permissions
                .as_ref()
                .map_or(0o666, |permissions| permissions.mode()),
        ),
    )?;
    let mut file = fs::File::from(fd);
    let metadata = file.metadata()?;
    let mut guard = TempGuard::new(dir, temp, metadata);
    file.write_all(contents)?;
    file.sync_all()?;
    if let Some(permissions) = existing_permissions {
        file.set_permissions(permissions)?;
    }
    drop(file);
    if !guard.is_ours() {
        return Err(io::Error::other("staged temporary changed before rename"));
    }
    rustix::fs::renameat(&guard.dir, guard.temp.as_os_str(), &guard.dir, file_name)?;
    guard.disarm();
    let _ = rustix::fs::fsync(&guard.dir);
    Ok(())
}

pub(crate) fn temp_name(file_name: &std::ffi::OsStr) -> std::ffi::OsString {
    // Built from OsString so a non-UTF-8 file name is preserved exactly.
    let mut name = std::ffi::OsString::from(".");
    name.push(file_name);
    name.push(format!(
        ".p1-tmp-{}-{}",
        std::process::id(),
        TEMP_COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    name
}

/// Deletes the temp file on drop unless the rename succeeded.
struct TempGuard {
    dir: OwnedFd,
    temp: std::ffi::OsString,
    identity: (u64, u64),
    armed: bool,
}

impl TempGuard {
    fn new(dir: OwnedFd, temp: std::ffi::OsString, metadata: fs::Metadata) -> Self {
        use std::os::unix::fs::MetadataExt;
        Self {
            dir,
            temp,
            identity: (metadata.dev(), metadata.ino()),
            armed: true,
        }
    }

    fn is_ours(&self) -> bool {
        rustix::fs::statat(&self.dir, self.temp.as_os_str(), AtFlags::SYMLINK_NOFOLLOW).is_ok_and(
            |stat| {
                (stat.st_dev, stat.st_ino) == self.identity
                    && FileType::from_raw_mode(stat.st_mode) == FileType::RegularFile
            },
        )
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for TempGuard {
    fn drop(&mut self) {
        if self.armed && self.is_ours() {
            let _ = rustix::fs::unlinkat(&self.dir, self.temp.as_os_str(), AtFlags::empty());
        }
    }
}

/// Bound text shown to the model to `max_bytes` bytes and `max_lines` lines,
/// cutting on a char boundary. When anything was cut, a trailer reports the
/// bytes actually shown against the original total.
pub fn bound_output(text: &str, max_bytes: usize, max_lines: usize) -> String {
    let total_bytes = text.len();
    let mut end = total_bytes;
    let mut truncated = false;

    // Keep the first `max_lines` lines. A trailing newline does not create an
    // extra line, matching how the file tools count lines.
    if max_lines > 0 {
        let mut newlines = 0usize;
        for (index, character) in text.char_indices() {
            if character == '\n' {
                newlines += 1;
                if newlines == max_lines {
                    let next_line = index + 1;
                    if next_line < total_bytes {
                        end = next_line;
                        truncated = true;
                    }
                    break;
                }
            }
        }
    }

    if max_bytes < end {
        let mut cut = max_bytes;
        while cut > 0 && !text.is_char_boundary(cut) {
            cut -= 1;
        }
        end = cut;
        truncated = true;
    }

    if !truncated {
        return text.to_string();
    }

    let mut out = String::with_capacity(end + 64);
    out.push_str(&text[..end]);
    // Put the trailer on its own line without emitting a blank line when the
    // kept slice already ends with a newline (a line-cap cut does).
    if !out.ends_with('\n') {
        out.push('\n');
    }
    out.push_str(&format!(
        "[output truncated: showing {end} of {total_bytes} bytes]"
    ));
    out
}

#[cfg(test)]
mod tests {
    use super::{TEMP_COUNTER, bound_output, write_atomic};
    use std::fs;
    use std::sync::atomic::Ordering;

    #[test]
    fn write_atomic_creates_file_and_missing_parents() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("nested/dir/file.txt");

        write_atomic(&target, b"hello").unwrap();

        assert_eq!(fs::read(&target).unwrap(), b"hello");
    }

    #[test]
    fn write_atomic_overwrites_existing_contents() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("file.txt");
        fs::write(&target, b"old contents").unwrap();

        write_atomic(&target, b"new").unwrap();

        assert_eq!(fs::read(&target).unwrap(), b"new");
    }

    #[cfg(unix)]
    #[test]
    fn write_atomic_preserves_permission_bits() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("script.sh");
        fs::write(&target, b"#!/bin/sh\n").unwrap();
        fs::set_permissions(&target, fs::Permissions::from_mode(0o755)).unwrap();

        write_atomic(&target, b"#!/bin/sh\necho hi\n").unwrap();

        let mode = fs::metadata(&target).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o755);
    }

    #[test]
    fn write_atomic_leaves_no_temp_file_on_success() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("file.txt");

        write_atomic(&target, b"data").unwrap();

        let mut entries: Vec<String> = fs::read_dir(dir.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        entries.sort();
        assert_eq!(entries, vec!["file.txt".to_string()]);
    }

    #[test]
    fn write_atomic_removes_temp_file_and_target_is_untouched_on_failure() {
        let dir = tempfile::tempdir().unwrap();
        // A directory target makes the final rename fail after the temp file
        // exists, exercising the cleanup path.
        let target = dir.path().join("subdir");
        fs::create_dir(&target).unwrap();

        write_atomic(&target, b"data").unwrap_err();

        assert!(target.is_dir(), "the target must be untouched");
        let leftovers: Vec<String> = fs::read_dir(dir.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|name| name.contains("p1-tmp"))
            .collect();
        assert!(leftovers.is_empty(), "leftover temp files: {leftovers:?}");
    }

    #[cfg(unix)]
    #[test]
    fn write_atomic_creates_its_temp_file_in_the_target_directory() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let locked = dir.path().join("locked");
        fs::create_dir(&locked).unwrap();
        let target = locked.join("file.txt");
        // A read-only target directory makes creating a sibling temp file fail.
        // If the temp were created elsewhere (e.g. /tmp) the write would
        // succeed, so this pins the temp file to the target's own directory.
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o555)).unwrap();

        let result = write_atomic(&target, b"data");

        fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(result.is_err());
        assert!(!target.exists());
    }

    #[test]
    fn substituted_temporary_is_neither_renamed_nor_cleaned_up() {
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let sentinel = outside.path().join("sentinel");
        fs::write(&sentinel, b"outside").unwrap();
        let fd = rustix::fs::openat(
            rustix::fs::CWD,
            dir.path(),
            rustix::fs::OFlags::RDONLY | rustix::fs::OFlags::DIRECTORY,
            rustix::fs::Mode::empty(),
        )
        .unwrap();
        let temp = std::ffi::OsString::from(".file.tmp");
        let path = dir.path().join(&temp);
        fs::write(&path, b"staged").unwrap();
        let guard = super::TempGuard::new(fd, temp, fs::metadata(&path).unwrap());
        fs::rename(&path, dir.path().join("moved")).unwrap();
        std::os::unix::fs::symlink(&sentinel, &path).unwrap();
        assert!(!guard.is_ours());
        drop(guard);
        assert!(path.is_symlink());
        assert_eq!(fs::read(&sentinel).unwrap(), b"outside");
    }

    #[test]
    fn a_colliding_temporary_name_is_refused_and_left_alone() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("file.txt");
        let collision = dir.path().join(".file.txt.p1-tmp-collision");
        fs::write(&collision, b"sentinel").unwrap();
        // A deterministic name makes the collision real instead of racing the shared counter:
        // the `O_EXCL` open must refuse and the pre-existing file must be left byte-identical.
        let result = super::write_atomic_named(&target, b"new", |_| {
            std::ffi::OsString::from(".file.txt.p1-tmp-collision")
        });
        assert_eq!(
            result.unwrap_err().kind(),
            std::io::ErrorKind::AlreadyExists
        );
        assert_eq!(fs::read(&collision).unwrap(), b"sentinel");
        assert!(!target.exists());
    }

    #[test]
    fn colliding_temporary_is_not_removed() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("file.txt");
        let counter = TEMP_COUNTER.load(Ordering::Relaxed);
        // Other tests may allocate names concurrently; reserve a span of candidates.
        let collision = dir
            .path()
            .join(format!(".file.txt.p1-tmp-{}-{counter}", std::process::id()));
        fs::write(&collision, b"sentinel").unwrap();
        let _ = write_atomic(&target, b"new");
        assert_eq!(fs::read(&collision).unwrap(), b"sentinel");
    }

    #[test]
    fn bound_output_leaves_short_text_unchanged() {
        assert_eq!(bound_output("a\nb\n", 50_000, 2_000), "a\nb\n");
    }

    #[test]
    fn bound_output_keeps_exactly_max_lines() {
        // Three lines with a trailing newline: the cap is not exceeded.
        assert_eq!(bound_output("a\nb\nc\n", 50_000, 3), "a\nb\nc\n");
    }

    #[test]
    fn bound_output_cuts_at_the_line_cap_and_reports_byte_totals() {
        let out = bound_output("1\n2\n3\n4\n", 50_000, 2);
        assert_eq!(out, "1\n2\n[output truncated: showing 4 of 8 bytes]");
    }

    #[test]
    fn bound_output_cuts_on_a_char_boundary() {
        // "aé" is three bytes; a two-byte cap cannot split the two-byte `é`.
        let out = bound_output("aé", 2, 100);
        assert_eq!(out, "a\n[output truncated: showing 1 of 3 bytes]");
    }
}
