//! The `workspace` (read side) and `snapshot` capabilities of one agent, over S1.2's read
//! API of `p1-workspace` (`Workspace::check_path`, `stat` and `read` → `Snapshot`) and the
//! agent's `ObservedFiles`: what the `p1/read` component reads files through.
//!
//! The refusals the native `ReadTool` applies before it touches a file hold here too, so a
//! module cannot reach what the native tool would not: confinement is `p1-workspace`'s, and
//! the credential files under the agent's home (issue #142) are refused before confinement,
//! with the same model-facing text. Every message a module receives is complete and safe to
//! show the model, so the component passes `io` text through unchanged.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};

use p1_contracts::BoxFuture;
use p1_module_runtime::{
    EntryKind, FsError, Services, SnapshotObservation, SnapshotService, WorkspaceEntry,
    WorkspaceService,
};
use p1_workspace::{
    CheckedPath, FileKind, Observation, ObservedFiles, Snapshot, Workspace, WorkspaceError,
};

use crate::{refuse_credentials, xdg_credentials};

/// How many files may be open for windowed reading at once. A module reads a file window
/// by window from one snapshot, so each open file holds its bytes until its last window is
/// read; a read the module abandons (a binary file, say) is dropped when newer ones push it
/// out, which bounds what abandoned reads keep in memory.
const MAX_OPEN_SNAPSHOTS: usize = 4;

/// One agent's `workspace` and `snapshot` capabilities.
#[derive(Clone)]
pub struct ReadCapability {
    inner: Arc<Inner>,
}

struct Inner {
    workspace: Workspace,
    observed: ObservedFiles,
    home: Option<PathBuf>,
    xdg_credentials: Vec<PathBuf>,
    /// The snapshots of files being read window by window, oldest first.
    open: Mutex<Vec<(PathBuf, Snapshot)>>,
}

impl ReadCapability {
    /// The capabilities over `workspace` and `observed`, refusing the credential files under
    /// `home` (the host's injected `HOME`) and the XDG-named stores, as the native
    /// [`crate::ReadTool`] built with that home does.
    pub fn new(workspace: Workspace, observed: ObservedFiles, home: Option<PathBuf>) -> Self {
        Self {
            inner: Arc::new(Inner {
                workspace,
                observed,
                home,
                xdg_credentials: xdg_credentials(),
                open: Mutex::new(Vec::new()),
            }),
        }
    }

    /// Runs `work` on a blocking thread: the file work is synchronous and must never hold
    /// the async thread, as in the native tool.
    fn blocking<T: Send + 'static>(
        &self,
        work: impl FnOnce(&Inner) -> Result<T, FsError> + Send + 'static,
    ) -> BoxFuture<'_, Result<T, FsError>> {
        let inner = self.inner.clone();
        Box::pin(async move {
            tokio::task::spawn_blocking(move || work(&inner))
                .await
                .unwrap_or_else(|error| Err(FsError::Io(format!("read failed: {error}"))))
        })
    }
}

/// The `workspace` and `snapshot` services of one agent, for the module tools the host
/// links: `home` is the host's (issue #142), passed in as it is to the native `read`.
pub fn capability_services(
    workspace: Workspace,
    observed: ObservedFiles,
    home: Option<PathBuf>,
) -> Services {
    let capability = Arc::new(ReadCapability::new(workspace, observed, home));
    Services {
        workspace: Some(capability.clone()),
        snapshot: Some(capability),
        ..Services::default()
    }
}

impl Inner {
    /// The credential refusal, then confinement: the order the native tool keeps.
    fn check(&self, requested: &str) -> Result<CheckedPath, FsError> {
        refuse_credentials(
            &self.workspace,
            requested,
            self.home.as_deref(),
            &self.xdg_credentials,
        )
        .map_err(FsError::Io)?;
        self.workspace
            .check_path(requested)
            .map_err(|error| self.fs_error(error))
    }

    /// A workspace error as the module sees it. `io` carries the native tool's wording.
    fn fs_error(&self, error: WorkspaceError) -> FsError {
        match error {
            WorkspaceError::OutsideWorkspace { .. } => FsError::OutsideWorkspace,
            WorkspaceError::NotFound { .. } => FsError::NotFound,
            WorkspaceError::NotADirectory(_) => FsError::WrongKind,
            WorkspaceError::Io { path, source } => FsError::Io(p1_read_guest::could_not_be_read(
                &self.workspace.display(&path),
                &source.to_string(),
            )),
        }
    }

    fn stat(&self, requested: &str) -> Result<WorkspaceEntry, FsError> {
        let checked = self.check(requested)?;
        let stat = self
            .workspace
            .stat(requested)
            .map_err(|error| self.fs_error(error))?;
        let kind = match stat.kind {
            FileKind::File => EntryKind::File,
            FileKind::Directory => EntryKind::Directory,
            // `stat` reports a link only where confinement left the leaf unresolved: its
            // target is absent, so nothing is there to read, as the native tool says.
            FileKind::Symlink => return Err(FsError::NotFound),
            FileKind::Other => EntryKind::Other,
        };
        Ok(WorkspaceEntry {
            path: checked.display().to_owned(),
            kind,
            size: if kind == EntryKind::File {
                stat.size
            } else {
                0
            },
        })
    }

    fn read(&self, requested: &str, offset: u64, length: u64) -> Result<Vec<u8>, FsError> {
        let checked = self.check(requested)?;
        let key = checked.path().to_path_buf();
        // A read from the start takes a fresh snapshot; later windows come from the same
        // one, so a module sees one state of the file, never a mix. The scratch registry
        // keeps this read from counting as an observation: the module records one through
        // `snapshot.observe` only once it has accepted what it read, as the native tool
        // records only a successful read.
        let snapshot = match self.take_open(&key).filter(|_| offset > 0) {
            Some(snapshot) => snapshot,
            None => self
                .workspace
                .read(requested, &ObservedFiles::new())
                .map_err(|error| self.fs_error(error))?,
        };
        let offset = usize::try_from(offset).unwrap_or(usize::MAX);
        let length = usize::try_from(length).unwrap_or(usize::MAX);
        let window = snapshot.read(offset, length).to_vec();
        let size = snapshot.metadata().size;
        if (offset as u64).saturating_add(window.len() as u64) < size {
            self.keep_open(key, snapshot);
        }
        Ok(window)
    }

    fn observe(&self, requested: &str, contents: &[u8]) -> Result<(), FsError> {
        let checked = self.check(requested)?;
        self.observed.record(checked.path(), contents);
        Ok(())
    }

    fn compare(&self, requested: &str, current: &[u8]) -> Result<SnapshotObservation, FsError> {
        let checked = self.check(requested)?;
        Ok(
            match self.observed.check_unchanged(checked.path(), current) {
                Observation::NeverObserved => SnapshotObservation::NeverObserved,
                Observation::Unchanged => SnapshotObservation::Unchanged,
                Observation::ChangedSinceObserved => SnapshotObservation::ChangedSinceObserved,
            },
        )
    }

    fn take_open(&self, path: &Path) -> Option<Snapshot> {
        let mut open = self.open();
        let index = open.iter().position(|(key, _)| key == path)?;
        Some(open.remove(index).1)
    }

    fn keep_open(&self, path: PathBuf, snapshot: Snapshot) {
        let mut open = self.open();
        if open.len() >= MAX_OPEN_SNAPSHOTS {
            open.remove(0);
        }
        open.push((path, snapshot));
    }

    /// Recover from a poisoned lock: a panic elsewhere must not fail every later read.
    fn open(&self) -> MutexGuard<'_, Vec<(PathBuf, Snapshot)>> {
        self.open
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

impl WorkspaceService for ReadCapability {
    fn stat(&self, path: String) -> BoxFuture<'_, Result<WorkspaceEntry, FsError>> {
        self.blocking(move |inner| inner.stat(&path))
    }

    fn read(
        &self,
        path: String,
        offset: u64,
        length: u64,
    ) -> BoxFuture<'_, Result<Vec<u8>, FsError>> {
        self.blocking(move |inner| inner.read(&path, offset, length))
    }
}

impl SnapshotService for ReadCapability {
    fn observe(&self, path: String, contents: Vec<u8>) -> BoxFuture<'_, Result<(), FsError>> {
        self.blocking(move |inner| inner.observe(&path, &contents))
    }

    fn check(
        &self,
        path: String,
        current: Vec<u8>,
    ) -> BoxFuture<'_, Result<SnapshotObservation, FsError>> {
        self.blocking(move |inner| inner.compare(&path, &current))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn capability(root: &Path) -> (ReadCapability, ObservedFiles) {
        let observed = ObservedFiles::new();
        let capability = ReadCapability::new(
            Workspace::new(root).unwrap(),
            observed.clone(),
            Some(root.to_path_buf()),
        );
        (capability, observed)
    }

    #[tokio::test]
    async fn windows_come_from_one_snapshot_and_only_observe_records() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.txt");
        std::fs::write(&path, "alpha\nbeta\n").unwrap();
        let (capability, observed) = capability(dir.path());

        let first = WorkspaceService::read(&capability, "a.txt".into(), 0, 4)
            .await
            .unwrap();
        // A change after the first window is not seen by the rest of this read.
        std::fs::write(&path, "ALPHA\nBETA\n").unwrap();
        let rest = WorkspaceService::read(&capability, "a.txt".into(), 4, 64)
            .await
            .unwrap();
        assert_eq!([first, rest].concat(), b"alpha\nbeta\n");
        assert_eq!(
            observed.check_unchanged(&path, b"alpha\nbeta\n"),
            Observation::NeverObserved
        );

        capability
            .observe("a.txt".into(), b"ALPHA\nBETA\n".to_vec())
            .await
            .unwrap();
        assert_eq!(
            capability
                .check("a.txt".into(), b"ALPHA\nBETA\n".to_vec())
                .await,
            Ok(SnapshotObservation::Unchanged)
        );
        // The finished read left nothing open; the next read from the start sees the file.
        assert_eq!(
            WorkspaceService::read(&capability, "a.txt".into(), 0, 64)
                .await
                .unwrap(),
            b"ALPHA\nBETA\n"
        );
    }

    #[tokio::test]
    async fn stat_describes_files_and_directories_and_refuses_what_read_refuses() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("sub")).unwrap();
        std::fs::write(dir.path().join("sub/a.txt"), "abc").unwrap();
        std::fs::create_dir_all(dir.path().join(".codex")).unwrap();
        std::fs::write(dir.path().join(".codex/auth.json"), "{}").unwrap();
        let (capability, _) = capability(dir.path());

        assert_eq!(
            capability.stat("sub/../sub/a.txt".into()).await,
            Ok(WorkspaceEntry {
                path: "sub/a.txt".into(),
                kind: EntryKind::File,
                size: 3
            })
        );
        assert_eq!(
            capability.stat("sub".into()).await.unwrap().kind,
            EntryKind::Directory
        );
        assert_eq!(
            capability.stat("nope.txt".into()).await,
            Err(FsError::NotFound)
        );
        assert_eq!(
            capability.stat("../x".into()).await,
            Err(FsError::OutsideWorkspace)
        );
        assert_eq!(
            capability.stat(".codex/auth.json".into()).await,
            Err(FsError::Io(p1_read_guest::credential_refusal(
                ".codex/auth.json"
            )))
        );
        assert_eq!(
            WorkspaceService::read(&capability, ".codex/auth.json".into(), 0, 64).await,
            Err(FsError::Io(p1_read_guest::credential_refusal(
                ".codex/auth.json"
            )))
        );
        assert_eq!(
            WorkspaceService::read(&capability, "sub".into(), 0, 64).await,
            Err(FsError::WrongKind)
        );
    }
}
