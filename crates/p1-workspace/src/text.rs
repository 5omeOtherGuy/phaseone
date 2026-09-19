//! Atomic file replacement and model-visible output bounding.

use std::fs::{self, OpenOptions};
use std::io::{self, Write};
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

    // Read the destination's permissions up front: the temp file is created
    // with them (so it is never briefly world-readable under the umask) and set
    // again exactly after the rename.
    let existing_permissions = fs::metadata(path).ok().map(|meta| meta.permissions());

    let temp_path = parent.join(temp_name(file_name));
    let mut guard = TempGuard::new(temp_path.clone());

    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
        if let Some(permissions) = &existing_permissions {
            options.mode(permissions.mode());
        }
    }

    let mut file = options.open(&temp_path)?;
    file.write_all(contents)?;
    file.sync_all()?;
    drop(file);

    if let Some(permissions) = existing_permissions {
        fs::set_permissions(&temp_path, permissions)?;
    }

    fs::rename(&temp_path, path)?;
    guard.disarm();

    // Best effort: fsync the directory so the rename itself survives a crash.
    // Not every filesystem supports directory fsync, so ignore the error.
    if let Ok(directory) = fs::File::open(&parent) {
        let _ = directory.sync_all();
    }
    Ok(())
}

fn temp_name(file_name: &std::ffi::OsStr) -> std::ffi::OsString {
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
    path: Option<PathBuf>,
}

impl TempGuard {
    fn new(path: PathBuf) -> Self {
        Self { path: Some(path) }
    }

    fn disarm(&mut self) {
        self.path = None;
    }
}

impl Drop for TempGuard {
    fn drop(&mut self) {
        if let Some(path) = self.path.take() {
            let _ = fs::remove_file(path);
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
    use super::{bound_output, write_atomic};
    use std::fs;

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
