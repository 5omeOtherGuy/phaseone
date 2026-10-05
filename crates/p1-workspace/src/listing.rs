//! Bounded, descriptor-relative directory pages (ADR-0115).
use std::collections::BTreeSet;
use std::fs::File;
use std::os::fd::AsRawFd;
use std::path::{Component, Path};

use p1_contracts::CancellationToken;
use rustix::fs::{CWD, Mode, OFlags};

use crate::{FileKind, Workspace, WorkspaceError};

/// Names read per call, including ignored names and names before the cursor.
pub const LISTING_SCAN_CEILING: u64 = 100_000;

/// One entry, never its symlink target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListedEntry {
    pub path: String,
    pub kind: FileKind,
    pub size: u64,
    pub depth: u32,
}

/// A bounded page in component-wise bytewise depth-first order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListingPage {
    pub entries: Vec<ListedEntry>,
    pub next: Option<String>,
    pub scanned: u64,
    pub scan_capped: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum ListingError {
    #[error(transparent)]
    Workspace(#[from] WorkspaceError),
    #[error("{0}")]
    Invalid(String),
    #[error("listing cancelled")]
    Cancelled,
    #[error("{0}")]
    Io(String),
}

/// A cursor is an encoding, not authority: decoding is followed by confinement.
pub fn listing_cursor(path: &str) -> String {
    let mut cursor = String::from("ls1:");
    for byte in path.bytes() {
        use std::fmt::Write;
        write!(cursor, "{byte:02x}").expect("writing to a string");
    }
    cursor
}

fn decode_cursor(cursor: &str) -> Result<String, ListingError> {
    let invalid = || ListingError::Invalid("invalid listing continuation".into());
    let encoded = cursor.strip_prefix("ls1:").ok_or_else(invalid)?;
    if encoded.len() % 2 != 0 {
        return Err(invalid());
    }
    let bytes = encoded
        .as_bytes()
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| {
            let a = (pair[0] as char).to_digit(16).ok_or_else(invalid)?;
            let b = (pair[1] as char).to_digit(16).ok_or_else(invalid)?;
            Ok((a * 16 + b) as u8)
        })
        .collect::<Result<Vec<_>, ListingError>>()?;
    String::from_utf8(bytes).map_err(|_| invalid())
}

fn relative(path: &str) -> bool {
    !path.is_empty()
        && Path::new(path)
            .components()
            .all(|c| matches!(c, Component::Normal(_)))
        && !path.contains('\0')
        && !path
            .split('/')
            .any(|part| part.is_empty() || part == "." || part == "..")
}

fn open_directory(workspace: &Workspace, path: &str) -> Result<File, ListingError> {
    let mut fd = rustix::fs::openat(
        CWD,
        workspace.root(),
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(|e| ListingError::Io(e.to_string()))?;
    if path != "." {
        for part in Path::new(path).components() {
            let Component::Normal(name) = part else {
                return Err(WorkspaceError::OutsideWorkspace {
                    requested: path.into(),
                }
                .into());
            };
            fd = rustix::fs::openat(
                &fd,
                name,
                OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::empty(),
            )
            .map_err(|e| match e {
                rustix::io::Errno::NOENT => ListingError::Workspace(WorkspaceError::NotFound {
                    requested: path.into(),
                }),
                rustix::io::Errno::LOOP | rustix::io::Errno::NOTDIR => ListingError::Workspace(
                    WorkspaceError::NotADirectory(workspace.root().join(path)),
                ),
                _ => ListingError::Io(e.to_string()),
            })?;
        }
    }
    Ok(File::from(fd))
}

impl Workspace {
    /// The host provides glob/policy exclusion; the walk owns confinement and ordering.
    #[allow(clippy::too_many_arguments)]
    pub fn list_directory(
        &self,
        path: &str,
        depth: u32,
        limit: u32,
        continuation: Option<&str>,
        cancel: &CancellationToken,
        excluded: impl Fn(&str, &File) -> Result<bool, ListingError>,
    ) -> Result<ListingPage, ListingError> {
        self.list_directory_with_ceiling(
            path,
            depth,
            limit,
            continuation,
            cancel,
            excluded,
            LISTING_SCAN_CEILING,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn list_directory_with_ceiling(
        &self,
        path: &str,
        depth: u32,
        limit: u32,
        continuation: Option<&str>,
        cancel: &CancellationToken,
        excluded: impl Fn(&str, &File) -> Result<bool, ListingError>,
        ceiling: u64,
    ) -> Result<ListingPage, ListingError> {
        if cancel.is_cancelled() {
            return Err(ListingError::Cancelled);
        }
        if depth == 0 || !(1..=500).contains(&limit) {
            return Err(ListingError::Invalid(
                "depth must be >= 1 and limit must be 1..500".into(),
            ));
        }
        if path != "." && !relative(path) {
            return Err(WorkspaceError::OutsideWorkspace {
                requested: path.into(),
            }
            .into());
        }
        let cursor = continuation.map(decode_cursor).transpose()?;
        if let Some(cursor) = &cursor {
            if !relative(cursor) || (path != "." && !Path::new(cursor).starts_with(path)) {
                return Err(WorkspaceError::OutsideWorkspace {
                    requested: cursor.clone(),
                }
                .into());
            }
            // Re-confine the parent, not the leaf: a listed symlink may point outside,
            // and a deleted leaf remains a valid position in the ordering.
            if let Some(parent) = Path::new(cursor).parent() {
                let parent = parent.to_str().unwrap_or("");
                if !parent.is_empty() {
                    self.resolve(parent)?;
                }
            }
        }
        let directory = open_directory(self, path)?;
        let mut walk = Walk {
            depth,
            limit: limit as usize,
            cursor: cursor.as_deref(),
            cancel,
            excluded: &excluded,
            ceiling,
            page: ListingPage {
                entries: Vec::new(),
                next: None,
                scanned: 0,
                scan_capped: false,
            },
        };
        let more = walk.directory(directory, if path == "." { "" } else { path }, 1)?;
        if more {
            walk.page.next = walk.page.entries.last().map(|e| listing_cursor(&e.path));
            if walk.page.next.is_none() {
                return Err(ListingError::Io(
                    "listing cannot advance within the scan ceiling; list a narrower path".into(),
                ));
            }
        }
        Ok(walk.page)
    }
}

struct Walk<'a, F> {
    depth: u32,
    limit: usize,
    cursor: Option<&'a str>,
    cancel: &'a CancellationToken,
    excluded: &'a F,
    ceiling: u64,
    page: ListingPage,
}

impl<F: Fn(&str, &File) -> Result<bool, ListingError>> Walk<'_, F> {
    fn directory(
        &mut self,
        directory: File,
        prefix: &str,
        depth: u32,
    ) -> Result<bool, ListingError> {
        if self.page.scanned == self.ceiling {
            self.page.scan_capped = true;
            return Ok(true);
        }
        let descriptor = format!("/proc/self/fd/{}", directory.as_raw_fd());
        let reader = std::fs::read_dir(descriptor).map_err(|e| ListingError::Io(e.to_string()))?;
        let mut names = BTreeSet::new();
        let capacity = (self.limit - self.page.entries.len()).max(1);
        let mut eligible = 0usize;
        let mut ancestor_name = None;
        for entry in reader {
            if self.cancel.is_cancelled() {
                return Err(ListingError::Cancelled);
            }
            let entry = entry.map_err(|e| ListingError::Io(e.to_string()))?;
            if self.page.scanned == self.ceiling {
                return Err(ListingError::Io(format!(
                    "{} has more than {} entries; list it with a glob",
                    if prefix.is_empty() { "." } else { prefix },
                    self.ceiling
                )));
            }
            self.page.scanned += 1;
            // Lossy names can collide and cannot be reopened faithfully.
            let name = entry
                .file_name()
                .into_string()
                .map_err(|_| ListingError::Io("directory contains a non-UTF-8 name".into()))?;
            let path = if prefix.is_empty() {
                name.clone()
            } else {
                format!("{prefix}/{name}")
            };
            let ancestor = self
                .cursor
                .is_some_and(|c| c == path || c.starts_with(&format!("{path}/")));
            let before = self
                .cursor
                .is_some_and(|c| path.split('/').cmp(c.split('/')).is_lt());
            if before && !ancestor {
                continue;
            }
            // The policy checks the exact no-follow object, not a pathname that
            // could have changed since the descriptor-relative enumeration.
            let fd = rustix::fs::openat(
                &directory,
                &name,
                OFlags::PATH | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::empty(),
            )
            .map_err(|e| ListingError::Io(e.to_string()))?;
            if (self.excluded)(&path, &File::from(fd))? {
                continue;
            }
            if ancestor {
                ancestor_name = Some(name);
                continue;
            }
            eligible += 1;
            if names.len() < capacity {
                names.insert(name);
            } else if names.last().is_some_and(|last| name < *last) {
                names.pop_last();
                names.insert(name);
            }
        }
        let unselected = eligible > names.len();
        for name in ancestor_name.into_iter().chain(names) {
            let path = if prefix.is_empty() {
                name.clone()
            } else {
                format!("{prefix}/{name}")
            };
            let fd = rustix::fs::openat(
                &directory,
                &name,
                OFlags::PATH | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::empty(),
            )
            .map_err(|e| ListingError::Io(e.to_string()))?;
            let file = File::from(fd);
            if (self.excluded)(&path, &file)? {
                continue;
            }
            let stat = rustix::fs::fstat(&file).map_err(|e| ListingError::Io(e.to_string()))?;
            let kind = match rustix::fs::FileType::from_raw_mode(stat.st_mode) {
                rustix::fs::FileType::RegularFile => FileKind::File,
                rustix::fs::FileType::Directory => FileKind::Directory,
                rustix::fs::FileType::Symlink => FileKind::Symlink,
                _ => FileKind::Other,
            };
            let ancestor = self
                .cursor
                .is_some_and(|c| c == path || c.starts_with(&format!("{path}/")));
            if !ancestor {
                if self.page.entries.len() == self.limit {
                    return Ok(true);
                }
                self.page.entries.push(ListedEntry {
                    path: path.clone(),
                    kind,
                    size: if kind == FileKind::File {
                        stat.st_size as u64
                    } else {
                        0
                    },
                    depth,
                });
            }
            if kind == FileKind::Directory && depth < self.depth {
                let fd = rustix::fs::openat(
                    &directory,
                    &name,
                    OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                    Mode::empty(),
                )
                .map_err(|e| ListingError::Io(e.to_string()))?;
                if self.directory(File::from(fd), &path, depth + 1)? {
                    return Ok(true);
                }
            }
        }
        Ok(unselected)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn wide_union_and_removed_entry() {
        let dir = tempfile::tempdir().unwrap();
        for i in 0..50_000 {
            File::create(dir.path().join(format!("f{i:05}"))).unwrap();
        }
        let ws = Workspace::new(dir.path()).unwrap();
        let cancel = CancellationToken::new();
        for remove in [false, true] {
            let mut cursor = None;
            let mut seen = vec![false; 50_000];
            let mut count = 0;
            loop {
                let page = ws
                    .list_directory(".", 1, 500, cursor.as_deref(), &cancel, |_, _| Ok(false))
                    .unwrap();
                assert!(page.entries.len() <= 500);
                for e in page.entries {
                    let index: usize = e.path.strip_prefix('f').unwrap().parse().unwrap();
                    assert!(!seen[index], "repeated {}", e.path);
                    seen[index] = true;
                    count += 1;
                }
                if remove && count == 500 {
                    std::fs::remove_file(dir.path().join("f00500")).unwrap();
                }
                cursor = page.next;
                if cursor.is_none() {
                    break;
                }
            }
            assert_eq!(count, if remove { 49_999 } else { 50_000 });
            for (i, present) in seen.into_iter().enumerate() {
                assert_eq!(present, !(remove && i == 500));
            }
        }
    }
    #[test]
    fn bytewise_depth_first_hidden_symlinks_and_removed_cursor() {
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("a")).unwrap();
        for name in [".hidden", "a/child", "a-", "z"] {
            File::create(dir.path().join(name)).unwrap();
        }
        for (name, target) in [
            ("file-link", dir.path().join("z")),
            ("dir-link", outside.path().to_path_buf()),
            ("loop", dir.path().join("loop")),
        ] {
            std::os::unix::fs::symlink(target, dir.path().join(name)).unwrap();
        }
        let ws = Workspace::new(dir.path()).unwrap();
        let cancel = CancellationToken::new();
        let mut cursor = None;
        let mut entries = Vec::new();
        loop {
            let page = ws
                .list_directory(".", 3, 1, cursor.as_deref(), &cancel, |_, _| Ok(false))
                .unwrap();
            entries.extend(page.entries);
            cursor = page.next;
            if cursor.is_none() {
                break;
            }
        }
        let paths: Vec<_> = entries.iter().map(|e| e.path.as_str()).collect();
        assert_eq!(
            paths,
            [
                ".hidden",
                "a",
                "a/child",
                "a-",
                "dir-link",
                "file-link",
                "loop",
                "z"
            ]
        );
        for entry in entries
            .iter()
            .filter(|e| ["dir-link", "file-link", "loop"].contains(&e.path.as_str()))
        {
            assert_eq!((entry.kind, entry.size), (FileKind::Symlink, 0));
        }
        std::fs::remove_file(dir.path().join("a/child")).unwrap();
        let page = ws
            .list_directory(
                ".",
                3,
                500,
                Some(&listing_cursor("a/child")),
                &cancel,
                |_, _| Ok(false),
            )
            .unwrap();
        assert_eq!(page.entries.first().unwrap().path, "a-");
    }

    #[test]
    fn confinement_cursor_validation_and_cancel() {
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(outside.path(), dir.path().join("escape")).unwrap();
        let ws = Workspace::new(dir.path()).unwrap();
        let cancel = CancellationToken::new();
        for path in ["..", "/", "escape", "escape/child", "a/../b"] {
            assert!(
                ws.list_directory(path, 2, 500, None, &cancel, |_, _| Ok(false))
                    .is_err(),
                "{path}"
            );
        }
        for path in ["../outside", "/outside", "escape/child"] {
            assert!(
                ws.list_directory(
                    ".",
                    2,
                    500,
                    Some(&listing_cursor(path)),
                    &cancel,
                    |_, _| Ok(false)
                )
                .is_err(),
                "{path}"
            );
        }
        assert!(
            ws.list_directory(".", 2, 500, Some("ls1:zz"), &cancel, |_, _| Ok(false))
                .is_err()
        );
        File::create(dir.path().join("file")).unwrap();
        cancel.cancel();
        assert!(matches!(
            ws.list_directory(".", 1, 500, None, &cancel, |_, _| Ok(false)),
            Err(ListingError::Cancelled)
        ));
    }

    #[test]
    fn ceiling_between_directories_marks_lower_bounds() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("a")).unwrap();
        std::fs::create_dir(dir.path().join("b")).unwrap();
        let ws = Workspace::new(dir.path()).unwrap();
        let page = ws
            .list_directory_with_ceiling(
                ".",
                2,
                500,
                None,
                &CancellationToken::new(),
                |_, _| Ok(false),
                2,
            )
            .unwrap();
        assert!(page.scan_capped);
        assert_eq!(page.scanned, 2);
        assert_eq!(page.entries.len(), 1);
        assert_eq!(page.next, Some(listing_cursor("a")));
    }

    #[test]
    fn ceiling_refuses_unsorted_partial_directory() {
        let dir = tempfile::tempdir().unwrap();
        for name in ["z", "b", "a"] {
            File::create(dir.path().join(name)).unwrap();
        }
        let ws = Workspace::new(dir.path()).unwrap();
        let error = ws
            .list_directory_with_ceiling(
                ".",
                1,
                1,
                None,
                &CancellationToken::new(),
                |_, _| Ok(false),
                2,
            )
            .unwrap_err();
        assert_eq!(
            error.to_string(),
            ". has more than 2 entries; list it with a glob"
        );
    }
}
