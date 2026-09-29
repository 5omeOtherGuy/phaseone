//! The file policy that is not confinement: the credential files every file tool refuses
//! BEFORE confinement (issue #142), and the model-facing texts a refusal and a failed read
//! carry.
//!
//! The policy lives here, in the crate the native tools and the capability services a module
//! is linked with both build on, so a component can never reach what a native tool would not:
//! the refusal is compared on the canonicalised form, so a symlink or a relative path cannot
//! slip past, and it comes before any confinement error, so the model is told why and never
//! sees the file's bytes. `p1-read-guest` keeps its own copy of the two texts for the guest
//! side, which cannot depend on this crate (it builds for `wasm32-unknown-unknown`); the
//! crates that see both sides pin them equal
//! (`crates/p1-tool-read/src/lib.rs`, `the_native_texts_are_the_guests`).

#[cfg(unix)]
use std::collections::HashSet;
use std::path::{Component, Path, PathBuf};

use p1_contracts::CancellationToken;

use crate::Workspace;

/// Credential policy with fixed paths canonicalized once per request.
#[derive(Clone, Debug)]
pub struct CredentialPolicy {
    lexical_exact_paths: Vec<PathBuf>,
    lexical_directories: Vec<PathBuf>,
    exact_paths: Vec<PathBuf>,
    directories: Vec<PathBuf>,
}

/// Descriptor identities of protected regular files, captured once for a request.
#[derive(Debug)]
pub struct ProtectedIndex {
    #[cfg(unix)]
    files: HashSet<(u64, u64)>,
    incomplete: bool,
    protected_directories: Vec<PathBuf>,
    protected_exact_paths: Vec<PathBuf>,
    #[cfg(unix)]
    directories_seen: Vec<SeenDirectory>,
}

#[cfg(unix)]
#[derive(Debug, PartialEq, Eq)]
struct DirectoryStamp {
    dev: u64,
    ino: u64,
    mtime: (i64, i64),
    ctime: (i64, i64),
}

#[cfg(unix)]
impl DirectoryStamp {
    fn of(metadata: &std::fs::Metadata) -> Self {
        use std::os::unix::fs::MetadataExt;
        Self {
            dev: metadata.dev(),
            ino: metadata.ino(),
            mtime: (metadata.mtime(), metadata.mtime_nsec()),
            ctime: (metadata.ctime(), metadata.ctime_nsec()),
        }
    }
}

/// One protected directory the index traversed: the stamp taken from it and the wall-clock
/// instant recorded immediately before its contents were read.
#[cfg(unix)]
#[derive(Debug)]
struct SeenDirectory {
    path: PathBuf,
    stamp: Option<DirectoryStamp>,
    read_at: std::time::SystemTime,
}

#[cfg(unix)]
impl SeenDirectory {
    /// Whether the directory's current metadata still matches the captured stamp.
    fn matches_current(&self) -> bool {
        let current = std::fs::metadata(&self.path).ok();
        match (&self.stamp, current) {
            (Some(expected), Some(metadata)) if metadata.is_dir() => {
                *expected == DirectoryStamp::of(&metadata)
            }
            (None, None) => cleanly_missing(&self.path),
            _ => false,
        }
    }

    /// Whether the captured stamp is old enough that a same-tick change cannot hide: both
    /// mtime and ctime are strictly older than the read by the safety margin. A directory
    /// recorded as cleanly missing carries no stamp and is judged by equality alone.
    fn settled(&self) -> bool {
        let Some(stamp) = &self.stamp else {
            return true;
        };
        stamp_predates_read_by_margin(stamp.mtime, self.read_at)
            && stamp_predates_read_by_margin(stamp.ctime, self.read_at)
    }
}

/// On a coarse filesystem clock (say 1 s, as on the Depot runners) a file created in the same
/// tick as the index read leaves mtime and ctime unchanged; a stamp must therefore be older
/// than the read by this margin before it can prove the directory unchanged (issue #481).
#[cfg(unix)]
const DIRECTORY_STAMP_SAFETY_MARGIN: std::time::Duration = std::time::Duration::from_secs(2);

/// Whether a `(seconds, nanoseconds)` stamp is strictly older than `read_at` by the margin.
#[cfg(unix)]
fn stamp_predates_read_by_margin(
    (seconds, nanoseconds): (i64, i64),
    read_at: std::time::SystemTime,
) -> bool {
    let stamp = i128::from(seconds) * 1_000_000_000 + i128::from(nanoseconds);
    let read = match read_at.duration_since(std::time::SystemTime::UNIX_EPOCH) {
        Ok(elapsed) => elapsed.as_nanos() as i128,
        Err(before) => -(before.duration().as_nanos() as i128),
    };
    stamp + (DIRECTORY_STAMP_SAFETY_MARGIN.as_nanos() as i128) < read
}

/// Index construction stopped by the request's cancellation token.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IndexCancelled;

impl ProtectedIndex {
    /// Follow symlinked subdirectories while preventing directory cycles. An unreadable
    /// protected directory forces conservative treatment of every multiply linked file.
    pub fn build(
        policy: &CredentialPolicy,
        cancel: &CancellationToken,
    ) -> Result<Self, IndexCancelled> {
        Self::build_with_clock(policy, cancel, std::time::SystemTime::now)
    }

    /// [`build`] with the wall clock supplied by the caller. The clock is read immediately
    /// before each directory is enumerated, and the recorded instant decides the coarse-clock
    /// margin [`still_current`] applies; tests inject it to make that margin deterministic.
    pub fn build_with_clock(
        policy: &CredentialPolicy,
        cancel: &CancellationToken,
        now: impl Fn() -> std::time::SystemTime,
    ) -> Result<Self, IndexCancelled> {
        #[cfg(not(unix))]
        let _ = now;
        let mut index = Self {
            #[cfg(unix)]
            files: HashSet::new(),
            incomplete: false,
            protected_directories: policy.directories.clone(),
            protected_exact_paths: policy.exact_paths.clone(),
            #[cfg(unix)]
            directories_seen: Vec::new(),
        };
        for path in &policy.exact_paths {
            if cancel.is_cancelled() {
                return Err(IndexCancelled);
            }
            #[cfg(unix)]
            let opened = rustix::fs::openat(
                rustix::fs::CWD,
                path,
                rustix::fs::OFlags::PATH
                    | rustix::fs::OFlags::NONBLOCK
                    | rustix::fs::OFlags::CLOEXEC,
                rustix::fs::Mode::empty(),
            )
            .map(std::fs::File::from)
            .map_err(std::io::Error::from);
            #[cfg(not(unix))]
            let opened = std::fs::File::open(path);
            match opened.and_then(|file| file.metadata()) {
                Ok(metadata) => index.insert(&metadata),
                Err(error)
                    if error.kind() == std::io::ErrorKind::NotFound && cleanly_missing(path) => {}
                Err(_) => index.incomplete = true,
            }
        }
        let mut pending: Vec<(PathBuf, Option<std::fs::File>)> = policy
            .directories
            .iter()
            .cloned()
            .map(|path| (path, None))
            .collect();
        #[cfg(unix)]
        let mut visited = HashSet::new();
        while let Some((directory, pinned)) = pending.pop() {
            if cancel.is_cancelled() {
                return Err(IndexCancelled);
            }
            // Keep the directory open through enumeration: re-pointing an XDG or
            // `.config` symlink cannot redirect the walk after this open.
            #[cfg(unix)]
            let opened = pinned.map(Ok).unwrap_or_else(|| {
                rustix::fs::openat(
                    rustix::fs::CWD,
                    &directory,
                    rustix::fs::OFlags::RDONLY
                        | rustix::fs::OFlags::DIRECTORY
                        | rustix::fs::OFlags::NONBLOCK
                        | rustix::fs::OFlags::CLOEXEC,
                    rustix::fs::Mode::empty(),
                )
                .map(std::fs::File::from)
                .map_err(std::io::Error::from)
            });
            #[cfg(not(unix))]
            let opened = pinned
                .map(Ok)
                .unwrap_or_else(|| std::fs::File::open(&directory));
            let opened_metadata = match &opened {
                Ok(file) => file.metadata(),
                Err(error) => Err(std::io::Error::new(error.kind(), error.to_string())),
            };
            let metadata = match opened_metadata {
                Ok(metadata) if metadata.is_dir() => metadata,
                Ok(_) => {
                    index.incomplete = true;
                    continue;
                }
                Err(error)
                    if error.kind() == std::io::ErrorKind::NotFound
                        && cleanly_missing(&directory) =>
                {
                    #[cfg(unix)]
                    index.directories_seen.push(SeenDirectory {
                        path: directory,
                        stamp: None,
                        read_at: now(),
                    });
                    continue;
                }
                Err(_) => {
                    index.incomplete = true;
                    continue;
                }
            };
            #[cfg(unix)]
            {
                let stamp = DirectoryStamp::of(&metadata);
                let identity = (stamp.dev, stamp.ino);
                // The read clock is taken here, before the directory's contents are read, so a
                // write during the read leaves a stamp no older than this instant.
                let read_at = now();
                index.directories_seen.push(SeenDirectory {
                    path: directory.clone(),
                    stamp: Some(stamp),
                    read_at,
                });
                if !visited.insert(identity) {
                    continue;
                }
            }
            #[cfg(not(unix))]
            let _ = metadata;
            let entries =
                match pinned_read_dir(opened.as_ref().expect("directory opened"), &directory) {
                    Ok(entries) => entries,
                    Err(_) => {
                        index.incomplete = true;
                        continue;
                    }
                };
            for entry in entries {
                if cancel.is_cancelled() {
                    return Err(IndexCancelled);
                }
                let entry = match entry {
                    Ok(entry) => entry,
                    Err(_) => {
                        index.incomplete = true;
                        continue;
                    }
                };
                match std::fs::metadata(entry.path()) {
                    Ok(metadata) if metadata.is_dir() => {
                        let child = directory.join(entry.file_name());
                        #[cfg(unix)]
                        let handle = rustix::fs::openat(
                            rustix::fs::CWD,
                            entry.path(),
                            rustix::fs::OFlags::RDONLY
                                | rustix::fs::OFlags::DIRECTORY
                                | rustix::fs::OFlags::NONBLOCK
                                | rustix::fs::OFlags::CLOEXEC,
                            rustix::fs::Mode::empty(),
                        )
                        .map(std::fs::File::from);
                        #[cfg(not(unix))]
                        let handle = std::fs::File::open(entry.path());
                        match handle {
                            Ok(handle) => pending.push((child, Some(handle))),
                            Err(_) => index.incomplete = true,
                        }
                    }
                    Ok(metadata) => index.insert(&metadata),
                    Err(_) => index.incomplete = true,
                }
            }
        }
        if cancel.is_cancelled() {
            return Err(IndexCancelled);
        }
        Ok(index)
    }

    /// A cached index belongs to the canonical roots and exact stores used to build it.
    pub fn matches_policy(&self, policy: &CredentialPolicy) -> bool {
        self.protected_directories == policy.directories
            && self.protected_exact_paths == policy.exact_paths
    }

    /// A cached directory index is safe to reuse only while every traversed directory is
    /// unchanged *and* settled: a stamp whose mtime or ctime sits inside
    /// `DIRECTORY_STAMP_SAFETY_MARGIN` of the recorded read time cannot prove a same-tick
    /// write did not happen, so reuse is refused and the caller rebuilds (issue #481).
    /// Incomplete walks are rebuilt; cancellations never reuse a stale snapshot.
    pub fn still_current(&self, cancel: &CancellationToken) -> Result<bool, IndexCancelled> {
        #[cfg(unix)]
        {
            if self.incomplete {
                return Ok(false);
            }
            for seen in &self.directories_seen {
                if cancel.is_cancelled() {
                    return Err(IndexCancelled);
                }
                if !seen.matches_current() || !seen.settled() {
                    return Ok(false);
                }
            }
        }
        if cancel.is_cancelled() {
            return Err(IndexCancelled);
        }
        #[cfg(not(unix))]
        {
            Ok(false)
        }
        #[cfg(unix)]
        {
            Ok(true)
        }
    }

    /// Whether every traversed directory's stamp is still equal to the one captured at build.
    /// A walk uses this to notice a change *during* one request; it ignores the coarse-clock
    /// margin, which only the cache's reuse decision ([`still_current`]) needs.
    pub fn stamps_unchanged(&self, cancel: &CancellationToken) -> Result<bool, IndexCancelled> {
        #[cfg(unix)]
        {
            if self.incomplete {
                return Ok(false);
            }
            for seen in &self.directories_seen {
                if cancel.is_cancelled() {
                    return Err(IndexCancelled);
                }
                if !seen.matches_current() {
                    return Ok(false);
                }
            }
        }
        if cancel.is_cancelled() {
            return Err(IndexCancelled);
        }
        #[cfg(not(unix))]
        {
            Ok(false)
        }
        #[cfg(unix)]
        {
            Ok(true)
        }
    }

    fn insert(&mut self, metadata: &std::fs::Metadata) {
        #[cfg(unix)]
        if metadata.is_file() {
            use std::os::unix::fs::MetadataExt;
            self.files.insert((metadata.dev(), metadata.ino()));
        }
        #[cfg(not(unix))]
        let _ = metadata;
    }

    /// Metadata must come from the opened handle for reads and stat.
    pub fn refuses_metadata(&self, metadata: &std::fs::Metadata) -> bool {
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            self.files.contains(&(metadata.dev(), metadata.ino()))
                || (self.incomplete && metadata.nlink() > 1)
        }
        #[cfg(not(unix))]
        {
            let _ = metadata;
            self.incomplete
        }
    }

    /// Recheck exact credential paths at the time of opening: an atomic rename may have
    /// replaced one after this index was captured.
    pub fn refuses_current_exact(
        &self,
        policy: &CredentialPolicy,
        metadata: &std::fs::Metadata,
    ) -> bool {
        if self.refuses_metadata(metadata) {
            return true;
        }
        for path in &policy.exact_paths {
            match std::fs::metadata(path) {
                Ok(credential) if credential.is_file() && same_identity(&credential, metadata) => {
                    return true;
                }
                Err(error)
                    if error.kind() == std::io::ErrorKind::NotFound && cleanly_missing(path) => {}
                Err(_) => return multiple_links(metadata),
                _ => {}
            }
        }
        false
    }

    pub fn refuses_path(&self, path: &Path) -> bool {
        std::fs::metadata(path).is_ok_and(|metadata| self.refuses_metadata(&metadata))
    }
}

fn pinned_read_dir(opened: &std::fs::File, path: &Path) -> std::io::Result<std::fs::ReadDir> {
    #[cfg(unix)]
    {
        use std::os::fd::AsRawFd;
        #[cfg(target_os = "linux")]
        let prefix = "/proc/self/fd";
        #[cfg(not(target_os = "linux"))]
        let prefix = "/dev/fd";
        let _ = path;
        std::fs::read_dir(format!("{prefix}/{}", opened.as_raw_fd()))
    }
    #[cfg(not(unix))]
    {
        let _ = opened;
        std::fs::read_dir(path)
    }
}

#[cfg(unix)]
fn same_identity(a: &std::fs::Metadata, b: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;
    a.dev() == b.dev() && a.ino() == b.ino()
}

#[cfg(not(unix))]
fn same_identity(_a: &std::fs::Metadata, _b: &std::fs::Metadata) -> bool {
    false
}

#[cfg(unix)]
fn multiple_links(metadata: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;
    metadata.nlink() > 1
}

#[cfg(not(unix))]
fn multiple_links(_metadata: &std::fs::Metadata) -> bool {
    true
}

/// A missing exact store is harmless only when every ancestor lookup succeeds up to
/// an existing directory. Permission failures and dangling symlinks are not absence.
fn cleanly_missing(path: &Path) -> bool {
    let mut candidate = path;
    loop {
        match std::fs::symlink_metadata(candidate) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let Some(parent) = candidate.parent() else {
                    return false;
                };
                candidate = parent;
            }
            Ok(metadata) => {
                return candidate != path
                    && (metadata.is_dir()
                        || std::fs::metadata(candidate).is_ok_and(|metadata| metadata.is_dir()));
            }
            Err(_) => return false,
        }
    }
}

impl CredentialPolicy {
    /// Builds the policy for an agent home and its XDG-named credential files.
    pub fn new(home: Option<&Path>, xdg_credentials: &[PathBuf]) -> Self {
        let mut lexical_exact_paths = xdg_credentials
            .iter()
            .map(|path| lexical_absolute(path))
            .collect::<Vec<_>>();
        let lexical_directories = home.map_or_else(Vec::new, |home| {
            let home = lexical_absolute(home);
            lexical_exact_paths.extend([
                home.join(".config/p1/auth.json"),
                home.join(".codex/auth.json"),
                home.join(".claude/.credentials.json"),
                home.join(".local/share/opencode/auth.json"),
                home.join(".pi/agent/auth.json"),
            ]);
            vec![home.join(".config/keys")]
        });
        let mut exact_paths = xdg_credentials
            .iter()
            .map(|path| canonical_best_effort(path))
            .collect::<Vec<_>>();
        let directories = home.map_or_else(Vec::new, |home| {
            let home = canonical_best_effort(home);
            let home_credentials = [
                home.join(".config/p1/auth.json"),
                home.join(".codex/auth.json"),
                home.join(".claude/.credentials.json"),
                home.join(".local/share/opencode/auth.json"),
                home.join(".pi/agent/auth.json"),
            ];
            exact_paths.extend(
                home_credentials
                    .iter()
                    .map(|path| canonical_best_effort(path)),
            );
            [".config/keys"]
                .iter()
                .map(|relative| canonical_best_effort(&home.join(relative)))
                .collect()
        });
        Self {
            lexical_exact_paths,
            lexical_directories,
            exact_paths,
            directories,
        }
    }

    /// Whether `candidate` is one of the fixed credential files or lies under a credential
    /// directory. Both lexical spelling and canonical target are checked for each candidate.
    pub fn refuses(&self, candidate: &Path) -> bool {
        let lexical_candidate = lexical_absolute(candidate);
        let lexical_match = self.lexical_exact_paths.contains(&lexical_candidate)
            || self.lexical_directories.iter().any(|directory| {
                lexical_candidate == *directory || lexical_candidate.starts_with(directory)
            });
        let candidate = canonical_best_effort(candidate);
        lexical_match
            || self.exact_paths.contains(&candidate)
            || self
                .directories
                .iter()
                .any(|directory| candidate == *directory || candidate.starts_with(directory))
    }

    /// Check the object already opened, including hard links to credential files.
    /// File identities come from the descriptor, not a second lookup of the candidate.
    pub fn refuses_opened(&self, candidate: &Path, file: &std::fs::File) -> bool {
        if self.refuses(candidate) {
            return true;
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            let Ok(opened) = file.metadata() else {
                return true;
            };
            if opened.nlink() == 1 {
                return false;
            }
            let same_file = |target: &Path| {
                std::fs::metadata(target).is_ok_and(|credential| {
                    credential.is_file()
                        && credential.dev() == opened.dev()
                        && credential.ino() == opened.ino()
                })
            };
            if self.exact_paths.iter().any(|target| same_file(target)) {
                return true;
            }
            let mut pending = self.directories.clone();
            while let Some(directory) = pending.pop() {
                let Ok(entries) = std::fs::read_dir(directory) else {
                    continue;
                };
                for entry in entries.flatten() {
                    let Ok(kind) = entry.file_type() else {
                        continue;
                    };
                    if kind.is_dir() {
                        pending.push(entry.path());
                    } else if kind.is_file() && same_file(&entry.path()) {
                        return true;
                    }
                }
            }
            false
        }
        #[cfg(not(unix))]
        {
            let _ = file;
            false
        }
    }

    /// The credential refusal for a request, before workspace confinement.
    pub fn refuse(&self, workspace: &Workspace, requested: &str) -> Result<(), String> {
        let candidate = if Path::new(requested).is_absolute() {
            PathBuf::from(requested)
        } else {
            workspace.root().join(requested)
        };
        if self.refuses(&candidate) {
            return Err(credential_refusal(&workspace.display(&candidate)));
        }
        Ok(())
    }
}

/// Whether `candidate` is one of the credential files refused by this file policy.
/// An empty `home` refuses only the XDG-named stores.
pub fn refuses_credentials(
    candidate: &Path,
    home: Option<&Path>,
    xdg_credentials: &[PathBuf],
) -> bool {
    CredentialPolicy::new(home, xdg_credentials).refuses(candidate)
}

/// The refusal of a credential file, before confinement, as the model is told it: the path
/// as the workspace displays it, and why. The component and the native tool show this exact
/// text.
pub fn credential_refusal(display: &str) -> String {
    format!(
        "read refuses credential files ({display}); credentials never enter the model's context"
    )
}

/// A filesystem failure while reading, as the host words `error`.
pub fn could_not_be_read(display: &str, error: &str) -> String {
    format!("{display} could not be read: {error}")
}

/// The refusal of a credential file for one request, or `Ok(())` when the path is an
/// ordinary one. `requested` is the model's own path, relative to the workspace or absolute;
/// the refusal comes first, so a credential file outside the workspace is named as one rather
/// than as a confinement error.
pub fn refuse_credentials(
    workspace: &Workspace,
    requested: &str,
    home: Option<&Path>,
    xdg_credentials: &[PathBuf],
) -> Result<(), String> {
    CredentialPolicy::new(home, xdg_credentials).refuse(workspace, requested)
}

/// The p1 and OpenCode stores move with their XDG override (p1-auth); a home-based path
/// below covers the default. `None` when the variable is unset.
pub fn xdg_credentials() -> Vec<PathBuf> {
    let mut files = Vec::new();
    if let Some(config) = env_path("XDG_CONFIG_HOME") {
        files.push(config.join("p1/auth.json"));
    }
    if let Some(data) = env_path("XDG_DATA_HOME") {
        files.push(data.join("opencode/auth.json"));
    }
    files
}

/// A non-empty environment variable as a path, or `None`.
fn env_path(name: &str) -> Option<PathBuf> {
    std::env::var_os(name)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

/// Make a path absolute without resolving symlinks, preserving the lexical policy spelling.
fn lexical_absolute(path: &Path) -> PathBuf {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map(|current_dir| current_dir.join(path))
            .unwrap_or_else(|_| path.to_path_buf())
    };
    let mut normalized = PathBuf::new();
    for part in absolute.components() {
        match part {
            Component::ParentDir => {
                normalized.pop();
            }
            Component::CurDir => {}
            other => normalized.push(other.as_os_str()),
        }
    }
    normalized
}

/// The canonical form of `path`, or — when it does not exist yet — its canonical parent with
/// the file name appended, so a refusal never falls back to a lexical comparison against the
/// whole path.
fn canonical_best_effort(path: &Path) -> PathBuf {
    if let Ok(canonical) = path.canonicalize() {
        return canonical;
    }
    let normalized = lexical_absolute(path);
    match (normalized.parent(), normalized.file_name()) {
        (Some(parent), Some(name)) => match parent.canonicalize() {
            Ok(parent) => parent.join(name),
            Err(_) => normalized,
        },
        _ => normalized,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn captured_credential_directory_identity_survives_link_retarget() {
        use std::os::unix::fs::symlink;
        let home = tempfile::tempdir().unwrap();
        for name in ["old/keys", "new/keys"] {
            std::fs::create_dir_all(home.path().join(name)).unwrap();
        }
        let credential = home.path().join("old/keys/old.key");
        std::fs::write(&credential, b"marker").unwrap();
        std::fs::hard_link(&credential, home.path().join("alias")).unwrap();
        let link = home.path().join(".config");
        symlink(home.path().join("old"), &link).unwrap();
        let index = ProtectedIndex::build(
            &CredentialPolicy::new(Some(home.path()), &[]),
            &CancellationToken::new(),
        )
        .unwrap();
        std::fs::remove_file(&link).unwrap();
        symlink(home.path().join("new"), &link).unwrap();
        let opened = std::fs::File::open(home.path().join("alias")).unwrap();
        assert!(index.refuses_metadata(&opened.metadata().unwrap()));
    }

    #[cfg(unix)]
    #[test]
    fn directory_enumeration_uses_its_pinned_handle_after_link_retarget() {
        use std::os::unix::fs::symlink;
        let home = tempfile::tempdir().unwrap();
        for name in ["old", "new"] {
            std::fs::create_dir_all(home.path().join(name)).unwrap();
        }
        std::fs::write(home.path().join("old/old.key"), b"old").unwrap();
        std::fs::write(home.path().join("new/new.key"), b"new").unwrap();
        let link = home.path().join(".config");
        symlink(home.path().join("old"), &link).unwrap();
        let dir = std::fs::File::open(&link).unwrap();
        std::fs::remove_file(&link).unwrap();
        symlink(home.path().join("new"), &link).unwrap();
        let entries: Vec<_> = super::pinned_read_dir(&dir, &link)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(entries, [std::ffi::OsString::from("old.key")]);
    }

    #[cfg(unix)]
    #[test]
    fn index_remembers_opened_identity_after_alias_is_unlinked() {
        use std::os::unix::fs::MetadataExt;
        let home = tempfile::tempdir().unwrap();
        let credential = home.path().join(".codex/auth.json");
        std::fs::create_dir_all(credential.parent().unwrap()).unwrap();
        std::fs::write(&credential, "marker").unwrap();
        let alias = home.path().join("alias.txt");
        std::fs::hard_link(&credential, &alias).unwrap();
        let opened = std::fs::File::open(&alias).unwrap();
        let index = ProtectedIndex::build(
            &CredentialPolicy::new(Some(home.path()), &[]),
            &CancellationToken::new(),
        )
        .unwrap();
        std::fs::remove_file(&alias).unwrap();
        assert_eq!(opened.metadata().unwrap().nlink(), 1);
        assert!(index.refuses_metadata(&opened.metadata().unwrap()));
    }

    #[cfg(unix)]
    #[test]
    fn unreadable_exact_credential_parent_refuses_hard_link_alias() {
        use std::os::unix::fs::PermissionsExt;
        let home = tempfile::tempdir().unwrap();
        let credential = home.path().join(".codex/auth.json");
        let parent = credential.parent().unwrap();
        std::fs::create_dir_all(parent).unwrap();
        std::fs::write(&credential, "marker").unwrap();
        let alias = home.path().join("alias.txt");
        std::fs::hard_link(&credential, &alias).unwrap();
        let policy = CredentialPolicy::new(Some(home.path()), &[]);
        let original_permissions = std::fs::metadata(parent).unwrap().permissions();
        std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o000)).unwrap();
        if std::fs::metadata(&credential).is_ok() {
            // Root can still search mode-000 directories, so this refusal cannot be exercised.
            std::fs::set_permissions(parent, original_permissions).unwrap();
            return;
        }
        let index = ProtectedIndex::build(&policy, &CancellationToken::new()).unwrap();
        let refused = index.refuses_path(&alias);
        std::fs::set_permissions(parent, original_permissions).unwrap();
        assert!(
            refused,
            "unreadable exact credential must fail closed for multi-link aliases"
        );
    }

    #[cfg(unix)]
    #[test]
    fn opened_metadata_is_not_second_path_resolution() {
        let home = tempfile::tempdir().unwrap();
        let secret = home.path().join(".codex/auth.json");
        std::fs::create_dir_all(secret.parent().unwrap()).unwrap();
        std::fs::write(&secret, "secret").unwrap();
        let alias = home.path().join("alias.txt");
        std::fs::hard_link(&secret, &alias).unwrap();
        let opened = std::fs::File::open(&alias).unwrap();
        let index = ProtectedIndex::build(
            &CredentialPolicy::new(Some(home.path()), &[]),
            &CancellationToken::new(),
        )
        .unwrap();
        std::fs::remove_file(&alias).unwrap();
        std::fs::write(&alias, "new ordinary file").unwrap();
        assert!(!index.refuses_path(&alias));
        assert!(index.refuses_metadata(&opened.metadata().unwrap()));
        assert_eq!(opened.metadata().unwrap().len(), 6);
    }

    #[cfg(unix)]
    #[test]
    fn index_follows_symlinked_keys_directories_and_survives_cycles() {
        use std::os::unix::fs::symlink;
        let home = tempfile::tempdir().unwrap();
        let keys = home.path().join(".config/keys");
        let sub = home.path().join("store");
        std::fs::create_dir_all(&keys).unwrap();
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(sub.join("secret.key"), "marker").unwrap();
        symlink(&sub, keys.join("linked")).unwrap();
        symlink(&keys, sub.join("back")).unwrap();
        let alias = home.path().join("alias.txt");
        std::fs::hard_link(sub.join("secret.key"), &alias).unwrap();
        let index = ProtectedIndex::build(
            &CredentialPolicy::new(Some(home.path()), &[]),
            &CancellationToken::new(),
        )
        .unwrap();
        assert!(index.refuses_path(&alias));
    }

    #[cfg(unix)]
    #[test]
    fn incomplete_index_refuses_multilink_but_not_single_link() {
        use std::os::unix::fs::symlink;
        let home = tempfile::tempdir().unwrap();
        let keys = home.path().join(".config/keys");
        std::fs::create_dir_all(&keys).unwrap();
        symlink("missing", keys.join("unreadable")).unwrap();
        let alias = home.path().join("alias.txt");
        std::fs::write(&alias, "safe").unwrap();
        let other = home.path().join("other.txt");
        std::fs::hard_link(&alias, &other).unwrap();
        let index = ProtectedIndex::build(
            &CredentialPolicy::new(Some(home.path()), &[]),
            &CancellationToken::new(),
        )
        .unwrap();
        assert!(index.refuses_path(&alias));
        std::fs::remove_file(&other).unwrap();
        assert!(!index.refuses_path(&alias));
    }

    #[test]
    fn cancelled_index_build_stops_before_reading_directories() {
        let home = tempfile::tempdir().unwrap();
        let cancel = CancellationToken::new();
        cancel.cancel();
        assert!(
            ProtectedIndex::build(&CredentialPolicy::new(Some(home.path()), &[]), &cancel).is_err()
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_directory_changed_inside_the_build_margin_is_never_reused() {
        let home = tempfile::tempdir().unwrap();
        let keys = home.path().join(".config/keys");
        std::fs::create_dir_all(&keys).unwrap();
        let policy = CredentialPolicy::new(Some(home.path()), &[]);
        let cancel = CancellationToken::new();
        // Freeze the read clock at the moment the directory was created: its kernel-set ctime
        // falls inside the margin, so the stamp cannot prove "unchanged" whatever the
        // filesystem's timestamp granularity is.
        let build_time = std::time::SystemTime::now();
        let racy = ProtectedIndex::build_with_clock(&policy, &cancel, move || build_time).unwrap();
        assert!(
            !racy.still_current(&cancel).unwrap(),
            "a stamp inside the coarse-clock margin must not be reused"
        );
        // The same stamp with the read clock ahead by the margin plus one second is settled,
        // so the directory becomes reusable: the margin, not the filesystem, decides.
        let settled_at =
            build_time + DIRECTORY_STAMP_SAFETY_MARGIN + std::time::Duration::from_secs(1);
        let settled =
            ProtectedIndex::build_with_clock(&policy, &cancel, move || settled_at).unwrap();
        assert!(settled.still_current(&cancel).unwrap());
    }

    #[test]
    fn parent_components_do_not_turn_public_files_into_credentials() {
        let home = tempfile::tempdir().unwrap();
        let policy = CredentialPolicy::new(Some(home.path()), &[]);
        assert!(!policy.refuses(&home.path().join(".config/keys/../public.txt")));
    }

    /// The credential files a tool refuses, relative to the home it was given.
    const CREDENTIAL_PATHS: [&str; 7] = [
        ".config/p1/auth.json",
        ".config/keys/tool.key",
        ".config/keys/nested/deeper.key",
        ".codex/auth.json",
        ".claude/.credentials.json",
        ".local/share/opencode/auth.json",
        ".pi/agent/auth.json",
    ];

    /// A temp home holding every refused credential file, as a real installation does.
    fn home_with_credentials() -> tempfile::TempDir {
        let home = tempfile::tempdir().unwrap();
        for relative in CREDENTIAL_PATHS {
            let path = home.path().join(relative);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, "{}\n").unwrap();
        }
        home
    }

    #[test]
    fn every_credential_path_is_refused_and_an_ordinary_file_is_not() {
        let home = home_with_credentials();
        let home_path = home.path();
        std::fs::write(home_path.join("notes.txt"), "alpha\n").unwrap();

        for relative in CREDENTIAL_PATHS {
            let candidate = home_path.join(relative);
            assert!(
                refuses_credentials(&candidate, Some(home_path), &[]),
                "{relative}"
            );
        }
        // Everything under the keys directory is refused, its own directories included: a
        // listing of it would leak the same names.
        for relative in [".config/keys", ".config/keys/nested"] {
            assert!(
                refuses_credentials(&home_path.join(relative), Some(home_path), &[]),
                "{relative}"
            );
        }
        assert!(!refuses_credentials(
            &home_path.join("notes.txt"),
            Some(home_path),
            &[]
        ));
        // A sibling of a refused file is not refused.
        assert!(!refuses_credentials(
            &home_path.join(".codex/other.json"),
            Some(home_path),
            &[]
        ));
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_or_a_relative_path_cannot_slip_past_the_refusal() {
        use std::os::unix::fs::symlink;

        let home = home_with_credentials();
        let home_path = home.path();
        let link = home_path.join("auth-link.json");
        symlink(home_path.join(".codex/auth.json"), &link).unwrap();

        // The link names a refused file once resolved, and a path through a link into the
        // keys directory stays inside it.
        assert!(refuses_credentials(&link, Some(home_path), &[]));
        let keys_link = home_path.join("keys-link");
        symlink(home_path.join(".config/keys"), &keys_link).unwrap();
        assert!(refuses_credentials(
            &keys_link.join("tool.key"),
            Some(home_path),
            &[]
        ));
        // A path that does not exist yet is compared through its canonical parent, so a
        // refused NAME with a `..` in front is still the refused file.
        assert!(refuses_credentials(
            &home_path.join(".config/../.codex/auth.json"),
            Some(home_path),
            &[]
        ));
    }

    #[test]
    fn an_empty_home_refuses_only_the_xdg_named_stores() {
        let home = home_with_credentials();
        let xdg = vec![home.path().join("xdg/p1/auth.json")];

        assert!(refuses_credentials(&xdg[0], None, &xdg));
        assert!(!refuses_credentials(
            &home.path().join(".codex/auth.json"),
            None,
            &xdg
        ));
    }

    #[test]
    fn the_refusal_names_the_workspace_relative_path_and_precedes_confinement() {
        let home = home_with_credentials();
        let elsewhere = tempfile::tempdir().unwrap();
        let workspace = Workspace::new(elsewhere.path()).unwrap();

        // Inside the workspace: refused with the display form.
        assert_eq!(
            refuse_credentials(&workspace, "notes.txt", Some(home.path()), &[]),
            Ok(())
        );
        // Outside it: the credential refusal comes first and names the rule, never the
        // confinement error.
        let absolute = home.path().join(".codex/auth.json");
        let refusal = refuse_credentials(
            &workspace,
            absolute.to_str().unwrap(),
            Some(home.path()),
            &[],
        )
        .expect_err("the credential file is refused");
        assert!(
            refusal.contains("read refuses credential files"),
            "{refusal}"
        );
        assert!(
            refusal.contains("credentials never enter the model's context"),
            "{refusal}"
        );
        assert!(
            !refusal.contains("escapes workspace"),
            "the credential rule comes first: {refusal}"
        );
    }

    #[test]
    fn the_two_texts_are_the_model_facing_wording() {
        assert_eq!(
            credential_refusal("a.txt"),
            "read refuses credential files (a.txt); credentials never enter the model's context"
        );
        assert_eq!(
            could_not_be_read("a.txt", "Is a directory (os error 21)"),
            "a.txt could not be read: Is a directory (os error 21)"
        );
    }
}
