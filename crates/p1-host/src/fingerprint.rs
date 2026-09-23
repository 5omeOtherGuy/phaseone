//! The workspace fingerprint (ADR-0055): what a command did to the workspace.
//!
//! A successful command that changes the workspace counts as progress for the §3c
//! stall guard and as a file change for the `finish` check, exactly as a
//! `WritesFiles` call does — a model that edits through shell heredocs is judged by
//! what it did, not by which tool it used.
//!
//! In a git workspace the fingerprint is a hash over `git status --porcelain=v1
//! -uall` (so `.gitignore` decides what counts: `target/` never does) together with
//! each listed path's status, size and mtime — a second edit of an already-modified
//! file is seen — and the HEAD sha, so a commit a command made is a change too.
//! Outside git it is a walk of the workspace, skipping `.git`, `target`,
//! `node_modules` and every dot-directory. NO file content is ever read: the cost is
//! metadata only, and past [`ENTRY_LIMIT`] entries the fingerprint refuses rather
//! than walking something that is not a code workspace.

use std::fmt;
use std::fs::{self, Metadata};
use std::path::{Component, Path, PathBuf};
use std::process::Command;
use std::time::UNIX_EPOCH;

/// Entries past this bound are not fingerprinted (ADR-0055 item 4).
pub const ENTRY_LIMIT: usize = 50_000;

/// Directories a non-git walk never descends into, besides any dot-directory.
const SKIPPED_DIRECTORIES: [&str; 2] = ["target", "node_modules"];

/// FNV-1a 64's offset basis and prime: a small stable hash, so a fingerprint never
/// depends on a crate or on `DefaultHasher`'s internals. Only equality of two
/// fingerprints matters, never the value itself.
const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

/// A hash of the workspace's listed paths and their metadata. Two fingerprints of
/// the same unchanged workspace are equal; that comparison is the only thing that
/// matters (ADR-0055 item 1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Fingerprint {
    hash: u64,
}

impl Fingerprint {
    pub fn hash(&self) -> u64 {
        self.hash
    }
}

/// Why a workspace could not be fingerprinted. The caller falls back to the
/// tool-declared rule and notes it once; nothing here ends a run (ADR-0055 item 4).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FingerprintError {
    /// The workspace holds more entries than a fingerprint is willing to walk.
    TooLarge { limit: usize },
    /// The workspace (or a directory in it) could not be read.
    Io { path: PathBuf, message: String },
}

impl fmt::Display for FingerprintError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooLarge { limit } => write!(
                f,
                "the workspace holds more than {limit} files to fingerprint"
            ),
            Self::Io { path, message } => {
                write!(f, "cannot read {}: {message}", path.display())
            }
        }
    }
}

impl std::error::Error for FingerprintError {}

/// The workspace's fingerprint now.
pub fn take(workspace: &Path) -> Result<Fingerprint, FingerprintError> {
    take_with(workspace, &[], ENTRY_LIMIT)
}

/// As [`take`], but never counting `ignored`: the absolute paths the HOST itself
/// appends to — its session journal and the worker journals beside it. They live in
/// the workspace when `--session` points there, and they grow on every record, so
/// counting them would call every command a workspace change.
pub fn take_ignoring(
    workspace: &Path,
    ignored: &[PathBuf],
) -> Result<Fingerprint, FingerprintError> {
    take_with(workspace, ignored, ENTRY_LIMIT)
}

/// The one implementation, with the bound open so a unit test can reach it without
/// building 50 000 files.
fn take_with(
    workspace: &Path,
    ignored: &[PathBuf],
    limit: usize,
) -> Result<Fingerprint, FingerprintError> {
    let ignored = Ignores::new(ignored);
    match git_root(workspace) {
        Some(root) => git_fingerprint(workspace, &root, &ignored, limit),
        None => walk_fingerprint(workspace, &ignored, limit),
    }
}

/// The repository root `workspace` is in, or `None` when it is not in a git
/// workspace at all.
fn git_root(workspace: &Path) -> Option<PathBuf> {
    let output = Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .current_dir(workspace)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if text.is_empty() {
        return None;
    }
    Some(absolute(Path::new(&text)))
}

/// `git status` lists paths relative to the REPOSITORY root, which may be an
/// ancestor of the workspace, so every listed path is resolved against it.
fn git_fingerprint(
    workspace: &Path,
    root: &Path,
    ignored: &Ignores,
    limit: usize,
) -> Result<Fingerprint, FingerprintError> {
    // The HEAD sha: a commit a command made is a change even when the working tree
    // is clean afterwards (ADR-0055 item 1). A repository with no commit yet has
    // none, which is a state like any other.
    let head = git_text(workspace, &["rev-parse", "HEAD"]).unwrap_or_default();
    let status = git_text(workspace, &["status", "--porcelain=v1", "-uall", "-z"])?;
    let mut keys = Vec::new();
    for (status, path) in porcelain_z(status.as_bytes()) {
        let absolute = root.join(&path);
        if ignored.matches(&absolute) {
            continue;
        }
        if keys.len() >= limit {
            return Err(FingerprintError::TooLarge { limit });
        }
        let (size, mtime_ns) = metadata_of(&absolute);
        keys.push(entry_key(&path, &status, size, mtime_ns));
    }
    keys.sort();
    Ok(digest(head.trim(), &keys))
}

/// A walk of a workspace that is not in a git repository. Only metadata is read.
fn walk_fingerprint(
    workspace: &Path,
    ignored: &Ignores,
    limit: usize,
) -> Result<Fingerprint, FingerprintError> {
    let root = absolute(workspace);
    let mut keys = Vec::new();
    let mut pending = vec![(root.clone(), PathBuf::new())];
    while let Some((directory, relative)) = pending.pop() {
        let listing = fs::read_dir(&directory).map_err(|error| FingerprintError::Io {
            path: directory.clone(),
            message: error.to_string(),
        })?;
        for item in listing {
            let item = item.map_err(|error| FingerprintError::Io {
                path: directory.clone(),
                message: error.to_string(),
            })?;
            let name = item.file_name().to_string_lossy().to_string();
            let path = relative.join(&name);
            // A symlink is neither directory nor file: it is not followed, so a
            // link back into the tree can never make the walk loop.
            let Ok(kind) = item.file_type() else { continue };
            if kind.is_dir() {
                if name.starts_with('.') || SKIPPED_DIRECTORIES.contains(&name.as_str()) {
                    continue;
                }
                pending.push((item.path(), path));
            } else if kind.is_file() {
                if ignored.matches(&root.join(&path)) {
                    continue;
                }
                if keys.len() >= limit {
                    return Err(FingerprintError::TooLarge { limit });
                }
                let metadata = item.metadata().map_err(|error| FingerprintError::Io {
                    path: item.path(),
                    message: error.to_string(),
                })?;
                let (size, mtime_ns) = (Some(metadata.len()), mtime_ns(&metadata));
                keys.push(entry_key(&path.to_string_lossy(), "", size, mtime_ns));
            }
        }
    }
    keys.sort();
    Ok(digest("", &keys))
}

/// The entries of `git status --porcelain=v1 -z`: `XY PATH` records separated by
/// NUL, where a rename or copy carries its original path in the following field.
fn porcelain_z(raw: &[u8]) -> Vec<(String, String)> {
    let text = String::from_utf8_lossy(raw);
    let mut fields = text.split('\0');
    let mut entries = Vec::new();
    while let Some(field) = fields.next() {
        // `XY PATH`: the status is always two ASCII bytes, and `get` never panics on
        // a field that is not one.
        let (Some(status), Some(rest)) = (field.get(..2), field.get(2..)) else {
            continue;
        };
        let path = rest.strip_prefix(' ').unwrap_or(rest);
        if status.starts_with('R') || status.starts_with('C') {
            // The original path is a field of its own and is not an entry.
            let _original = fields.next();
        }
        entries.push((status.to_string(), path.to_string()));
    }
    entries
}

/// One listed path as the hashed line: the path, git's status, and the metadata a
/// change is seen in. `-` means "no metadata", which is a state of its own (a path
/// git lists as deleted).
fn entry_key(path: &str, status: &str, size: Option<u64>, mtime_ns: Option<u64>) -> String {
    let number = |value: Option<u64>| value.map_or_else(|| "-".to_string(), |v| v.to_string());
    format!("{path}\0{status}\0{}\0{}", number(size), number(mtime_ns))
}

fn metadata_of(path: &Path) -> (Option<u64>, Option<u64>) {
    match fs::metadata(path) {
        Ok(metadata) => (Some(metadata.len()), mtime_ns(&metadata)),
        Err(_) => (None, None),
    }
}

fn mtime_ns(metadata: &Metadata) -> Option<u64> {
    metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map(|since| since.as_nanos() as u64)
}

/// FNV-1a 64 over the HEAD sha and the sorted entry keys.
fn digest(head: &str, keys: &[String]) -> Fingerprint {
    let mut hash = FNV_OFFSET;
    let mut feed = |bytes: &[u8]| {
        for byte in bytes {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(FNV_PRIME);
        }
    };
    feed(head.as_bytes());
    for key in keys {
        feed(b"\0");
        feed(key.as_bytes());
    }
    Fingerprint { hash }
}

/// `git` output as text; a failure is an I/O error the caller falls back on.
fn git_text(workspace: &Path, args: &[&str]) -> Result<String, FingerprintError> {
    let output = Command::new("git")
        .args(args)
        .current_dir(workspace)
        .output()
        .map_err(|error| FingerprintError::Io {
            path: workspace.to_path_buf(),
            message: error.to_string(),
        })?;
    if !output.status.success() {
        return Err(FingerprintError::Io {
            path: workspace.to_path_buf(),
            message: format!(
                "`git {}` failed: {}",
                args.join(" "),
                String::from_utf8_lossy(&output.stderr).trim()
            ),
        });
    }
    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

/// A path as absolute as it can be made: `git` and the walk yield absolute paths, so
/// an ignore has to be absolute too. Symlinks are resolved when the path exists; a
/// relative path that does not exist yet is resolved against the process directory
/// and normalised lexically.
fn absolute(path: &Path) -> PathBuf {
    if let Ok(resolved) = fs::canonicalize(path) {
        return resolved;
    }
    if path.is_absolute() {
        return normalize(path);
    }
    match std::env::current_dir() {
        Ok(current) => normalize(&current.join(path)),
        Err(_) => path.to_path_buf(),
    }
}

/// Lexical `a/./b` → `a/b`, `a/../b` → `b`: only for a path that does not exist, so
/// no symlink can make a lexical `..` wrong.
fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other),
        }
    }
    out
}

/// The paths the host writes itself, never counted as workspace content.
struct Ignores {
    paths: Vec<PathBuf>,
}

impl Ignores {
    fn new(paths: &[PathBuf]) -> Self {
        Self {
            paths: paths.iter().map(|path| absolute(path)).collect(),
        }
    }

    fn matches(&self, path: &Path) -> bool {
        self.paths
            .iter()
            .any(|ignored| path == ignored || is_worker_journal(path, ignored))
    }
}

/// A worker's journal is its parent's file plus `.w{n}.jsonl` (`session.jsonl` →
/// `session.jsonl.w1.jsonl`), beside it: the same bookkeeping, the same ignore.
fn is_worker_journal(path: &Path, session: &Path) -> bool {
    let (Some(parent), Some(session_parent)) = (path.parent(), session.parent()) else {
        return false;
    };
    if parent != session_parent {
        return false;
    }
    let (Some(name), Some(session_name)) = (path.file_name(), session.file_name()) else {
        return false;
    };
    let mut prefix = session_name.to_string_lossy().into_owned();
    prefix.push_str(".w");
    name.to_string_lossy().starts_with(&prefix)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    /// A real repository: the git path is the one that respects `.gitignore`, which
    /// is the whole point of the fingerprint (ADR-0055 item 1).
    fn git_workspace() -> tempfile::TempDir {
        let workspace = tempfile::tempdir().unwrap();
        std::fs::write(workspace.path().join("README.md"), "one\n").unwrap();
        std::fs::write(workspace.path().join(".gitignore"), "target/\n").unwrap();
        git(workspace.path(), &["init", "-q", "."]);
        git(workspace.path(), &["add", "README.md", ".gitignore"]);
        commit(workspace.path(), "one");
        workspace
    }

    fn git(workspace: &Path, args: &[&str]) {
        let status = Command::new("git")
            .args(args)
            .current_dir(workspace)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .status()
            .expect("git runs");
        assert!(status.success(), "git {args:?} failed");
    }

    fn commit(workspace: &Path, message: &str) {
        // `--allow-empty`: the commit test moves HEAD alone, with no tree change to
        // hide behind.
        git(
            workspace,
            &[
                "-c",
                "user.name=p1",
                "-c",
                "user.email=p1@example.invalid",
                "-c",
                "commit.gpgsign=false",
                "commit",
                "-q",
                "--allow-empty",
                "-m",
                message,
            ],
        );
    }

    fn take_in(workspace: &Path) -> Fingerprint {
        take(workspace).unwrap()
    }

    #[test]
    fn an_ignored_path_never_changes_the_fingerprint() {
        let workspace = git_workspace();
        let before = take_in(workspace.path());
        std::fs::create_dir_all(workspace.path().join("target")).unwrap();
        std::fs::write(workspace.path().join("target/x"), "build output\n").unwrap();
        std::fs::write(workspace.path().join("target/more"), "more output\n").unwrap();
        assert_eq!(
            before,
            take_in(workspace.path()),
            "`target/` is ignored, so build output is not a workspace change"
        );
    }

    #[test]
    fn an_untracked_new_file_changes_the_fingerprint() {
        let workspace = git_workspace();
        let before = take_in(workspace.path());
        std::fs::write(workspace.path().join("out.txt"), "hi\n").unwrap();
        assert_ne!(
            before,
            take_in(workspace.path()),
            "an untracked non-ignored file is a change (a heredoc write is exactly this)"
        );
    }

    #[test]
    fn a_second_edit_of_the_same_file_changes_the_fingerprint() {
        let workspace = git_workspace();
        let tracked = workspace.path().join("README.md");
        std::fs::write(&tracked, "two\n").unwrap();
        let once = take_in(workspace.path());
        // A different SIZE, so the change is seen without relying on a clock: no
        // test sleeps (AGENTS.md).
        std::fs::write(&tracked, "two, and longer\n").unwrap();
        assert_ne!(
            once,
            take_in(workspace.path()),
            "the path is listed in both, so only its size and mtime can tell them apart"
        );
    }

    #[test]
    fn a_commit_changes_the_fingerprint() {
        let workspace = git_workspace();
        let before = take_in(workspace.path());
        // The commit is made with the tree unchanged: only HEAD moves.
        commit(workspace.path(), "empty of content, new of sha");
        assert_ne!(
            before,
            take_in(workspace.path()),
            "the HEAD sha is part of the fingerprint, so a commit is a change"
        );
    }

    #[test]
    fn the_non_git_walk_skips_build_output_and_dot_directories() {
        let workspace = tempfile::tempdir().unwrap();
        std::fs::write(workspace.path().join("note.txt"), "one\n").unwrap();
        let before = take_in(workspace.path());

        std::fs::create_dir_all(workspace.path().join("target/deep")).unwrap();
        std::fs::write(workspace.path().join("target/deep/x"), "output\n").unwrap();
        std::fs::create_dir_all(workspace.path().join("node_modules")).unwrap();
        std::fs::write(workspace.path().join("node_modules/x"), "package\n").unwrap();
        std::fs::create_dir_all(workspace.path().join(".git")).unwrap();
        std::fs::write(workspace.path().join(".git/x"), "git state\n").unwrap();
        assert_eq!(
            before,
            take_in(workspace.path()),
            "target, node_modules and dot-directories are not workspace content"
        );

        std::fs::write(workspace.path().join("new.txt"), "two\n").unwrap();
        assert_ne!(
            before,
            take_in(workspace.path()),
            "a plain new file in the walk IS a change"
        );
    }

    #[test]
    fn too_many_entries_refuse_rather_than_walk() {
        let workspace = tempfile::tempdir().unwrap();
        for index in 0..5 {
            std::fs::write(workspace.path().join(format!("f{index}")), "x\n").unwrap();
        }
        // A small bound, so the test does not have to build 50 000 files.
        assert_eq!(
            take_with(workspace.path(), &[], 4),
            Err(FingerprintError::TooLarge { limit: 4 })
        );
        assert!(take_with(workspace.path(), &[], 5).is_ok());
    }

    #[test]
    fn a_session_journal_in_the_workspace_is_not_a_change() {
        let workspace = tempfile::tempdir().unwrap();
        let session = workspace.path().join("session.jsonl");
        std::fs::write(&session, "record\n").unwrap();
        let ignore = std::slice::from_ref(&session);
        let before = take_ignoring(workspace.path(), ignore).unwrap();

        // The host appends a record, and the worker journals beside it grow too.
        std::fs::write(&session, "record\nrecord\n").unwrap();
        std::fs::write(workspace.path().join("session.jsonl.w1.jsonl"), "record\n").unwrap();
        assert_eq!(
            before,
            take_ignoring(workspace.path(), ignore).unwrap(),
            "the host's own journals are not workspace content"
        );

        std::fs::write(workspace.path().join("out.txt"), "hi\n").unwrap();
        assert_ne!(
            before,
            take_ignoring(workspace.path(), ignore).unwrap(),
            "a file the model wrote beside them still counts"
        );
    }

    #[test]
    fn a_missing_workspace_is_an_io_error() {
        let workspace = tempfile::tempdir().unwrap();
        let missing = workspace.path().join("gone");
        assert!(matches!(take(&missing), Err(FingerprintError::Io { .. })));
    }

    #[test]
    fn the_status_parser_reads_a_rename_as_one_entry() {
        let raw = b"R  b.txt\0a.txt\0?? c.txt\0";
        assert_eq!(
            porcelain_z(raw),
            vec![
                ("R ".to_string(), "b.txt".to_string()),
                ("??".to_string(), "c.txt".to_string()),
            ]
        );
    }
}
