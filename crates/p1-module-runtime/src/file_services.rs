//! The capability services of the p1 file tools, as the HOST builds them for a component:
//! the read side (`workspace` and `snapshot`), the walk (`list-files` and `search`) and the
//! owned mutation (`workspace-mutation`), over `p1-workspace` and the assembling agent's own
//! services.
//!
//! They lived in the tool crates (`p1-tool-read`, `p1-tool-search`, `p1-tool-write`) while
//! those crates were the host's file tools; since read, edit, write, apply_patch and grep are
//! served by their components alone (S7.10-R1, ADR-0091), the host cannot depend on them any
//! more: a service that links a component is the host's, so it lives where the capability
//! traits do, and the tool crates re-use it for their own native tools and tests.
//!
//! Every refusal is `p1-workspace`'s and comes before confinement: the credential files under
//! the agent's home (issue #142, [`p1_workspace::refuse_credentials`]) first, then
//! confinement, so a component can never reach what the native tool would not. Every message
//! a module receives is complete and safe to show the model, so the component passes `io` text
//! through unchanged.
//!
//! The services are call-scoped (ADR-0090): [`tool_services`] and [`capability_services`]
//! build each export call's own read side and mutation service over one fresh
//! [`ReadRecord`], so the gated write of a call refuses a target another agent changed after
//! THIS call's read, and no read of an earlier or a concurrent call of the same tool
//! satisfies or blocks it.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};

use p1_contracts::{BoxFuture, CancellationToken};
use p1_workspace::{
    CheckedPath, FileKind, MutationError, MutationPolicy, Observation, ObservedFiles,
    OwnedMutation, ReadRecord, Snapshot, Workspace, WorkspaceError, refuse_credentials,
    xdg_credentials,
};

use crate::capabilities::{
    EntryKind, FsError, HeldMutation, MutationService, SearchQuery, SearchResult, Services,
    SnapshotObservation, SnapshotService, WorkspaceEntry, WorkspaceService,
};
use crate::file_walk;

/// How many files may be open for windowed reading at once. A module reads a file window by
/// window from one snapshot, so each open file holds its bytes until its last window is read;
/// a read the module abandons (a binary file, say) is dropped when newer ones push it out,
/// which bounds what abandoned reads keep in memory.
const MAX_OPEN_SNAPSHOTS: usize = 4;

// ---------------------------------------------------------------------------------------------
// The read side: the `workspace` and `snapshot` capabilities of one agent.

/// One agent's `workspace` and `snapshot` capabilities.
#[derive(Clone)]
pub struct ReadCapability {
    inner: Arc<Inner>,
}

struct Inner {
    workspace: Workspace,
    observed: ObservedFiles,
    /// What this tool read, so a mutation assembled with the same record can refuse a target
    /// that changed after the read (the gated recheck's read identity).
    reads: ReadRecord,
    home: Option<PathBuf>,
    xdg_credentials: Vec<PathBuf>,
    /// The snapshots of files being read window by window, oldest first.
    open: Mutex<Vec<(PathBuf, Snapshot)>>,
}

impl ReadCapability {
    /// The capabilities over `workspace` and `observed`, refusing the credential files under
    /// `home` (the host's injected `HOME`) and the XDG-named stores, as the native read tool
    /// built with that home does.
    pub fn new(workspace: Workspace, observed: ObservedFiles, home: Option<PathBuf>) -> Self {
        Self::with_reads(workspace, observed, ReadRecord::new(), home)
    }

    /// The same, recording every read into `reads`: one assembly shares that record between
    /// this read side and the mutation service it builds, so a change computed from a read is
    /// rechecked against it under the gate.
    pub fn with_reads(
        workspace: Workspace,
        observed: ObservedFiles,
        reads: ReadRecord,
        home: Option<PathBuf>,
    ) -> Self {
        Self {
            inner: Arc::new(Inner {
                workspace,
                observed,
                reads,
                home,
                xdg_credentials: xdg_credentials(),
                open: Mutex::new(Vec::new()),
            }),
        }
    }

    /// Runs `work` on a blocking thread: the file work is synchronous and must never hold the
    /// async thread, as in the native tool.
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
            WorkspaceError::Io { path, source } => FsError::Io(p1_workspace::could_not_be_read(
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
        // A read from the start takes a fresh snapshot; later windows come from the same one,
        // so a module sees one state of the file, never a mix. The scratch registry keeps this
        // read from counting as an observation: the module records one through
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
        // What this read returned is this tool's read identity of the file, whatever window
        // was asked for: the whole-file digest of the snapshot the bytes come from. The latest
        // read of a path wins, and a mutation assembled with the same record refuses any other
        // bytes at that path under the gate.
        self.reads
            .record_hash(&key, snapshot.metadata().content_hash);
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

// ---------------------------------------------------------------------------------------------
// The walk: the `workspace` capability of the search component.

/// One agent's `workspace` capability as the search component is granted it: `list-files` and
/// `search` over [`crate::file_walk`], and `stat` and `read` over `p1-workspace` as the native
/// `grep` reads.
///
/// Search is granted no `snapshot` (its package manifest), and this service reads with
/// `Workspace::read_unobserved`: nothing a search reads is recorded as an observation, so a
/// search can never give an agent the permission an edit needs.
#[derive(Clone)]
pub struct SearchCapability {
    workspace: Workspace,
}

impl SearchCapability {
    /// The capability over `workspace`.
    pub fn new(workspace: Workspace) -> Self {
        Self { workspace }
    }

    /// Runs `work` on a blocking thread: the walk and the reads are synchronous and must never
    /// hold the async thread, as in the native tool. `work` gets a token that is cancelled when
    /// the returned future is dropped — the runtime drops it as soon as the call is cancelled —
    /// so the walk stops at the next file instead of running on unobserved.
    fn blocking<T: Send + 'static>(
        &self,
        work: impl FnOnce(&Workspace, &CancellationToken) -> Result<T, FsError> + Send + 'static,
    ) -> BoxFuture<'_, Result<T, FsError>> {
        let workspace = self.workspace.clone();
        Box::pin(async move {
            let cancel = CancellationToken::new();
            let _stop_on_drop = cancel.clone().drop_guard();
            tokio::task::spawn_blocking(move || work(&workspace, &cancel))
                .await
                .unwrap_or_else(|error| Err(FsError::Io(format!("search failed: {error}"))))
        })
    }
}

impl WorkspaceService for SearchCapability {
    fn stat(&self, path: String) -> BoxFuture<'_, Result<WorkspaceEntry, FsError>> {
        self.blocking(move |workspace, _| {
            let checked = workspace
                .check_path(&path)
                .map_err(file_walk::workspace_error)?;
            let stat = workspace.stat(&path).map_err(file_walk::workspace_error)?;
            // The kinds the native `grep` gives its logic, so both render the same text.
            let kind = match stat.kind {
                FileKind::File => EntryKind::File,
                FileKind::Directory => EntryKind::Directory,
                _ => EntryKind::Other,
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
        })
    }

    fn read(
        &self,
        path: String,
        offset: u64,
        length: u64,
    ) -> BoxFuture<'_, Result<Vec<u8>, FsError>> {
        self.blocking(move |workspace, _| {
            let snapshot = workspace
                .read_unobserved(&path)
                .map_err(file_walk::workspace_error)?;
            let offset = usize::try_from(offset).unwrap_or(usize::MAX);
            let length = usize::try_from(length).unwrap_or(usize::MAX);
            Ok(snapshot.read(offset, length).to_vec())
        })
    }

    fn list_files(
        &self,
        path: String,
        glob: Option<String>,
    ) -> BoxFuture<'_, Result<Vec<String>, FsError>> {
        self.blocking(move |workspace, cancel| {
            file_walk::list_files(workspace, &path, glob.as_deref(), cancel)
        })
    }

    fn search(&self, query: SearchQuery) -> BoxFuture<'_, Result<SearchResult, FsError>> {
        self.blocking(move |workspace, cancel| file_walk::search(workspace, &query, cancel))
    }
}

// ---------------------------------------------------------------------------------------------
// The mutation: the `workspace-mutation` capability of the edit, write and patch components.

/// One agent's `workspace-mutation` capability, with the policy the host assembled it with and
/// the read record of the tool it serves.
///
/// Every check is `p1-workspace`'s: confinement, read-before-mutate under the held gate (the
/// exact native refusal texts), the change's read identity (the tool's [`ReadRecord`], so a
/// file another agent changed after the read is refused whatever the policy), atomic per-file
/// replacement and the observation of what was written. This service only maps its errors onto
/// the frozen `fs-error`, case by case, keeping each message as the native tools print it. The
/// policy is the host's choice when it builds the service (observed for edit and write,
/// patch-authorized for patch), never the module's.
#[derive(Clone)]
pub struct MutationCapability {
    workspace: Workspace,
    observed: ObservedFiles,
    reads: ReadRecord,
    policy: MutationPolicy,
}

impl MutationCapability {
    /// The capability over `workspace` (its write gate is the one the agent's native tools and
    /// every agent sharing it hold) and the agent's own `observed` files, enforcing `policy`,
    /// with a read record of its own: a mutation assembled on its own has no read side to share
    /// one with.
    pub fn new(workspace: Workspace, observed: ObservedFiles, policy: MutationPolicy) -> Self {
        Self::with_reads(workspace, observed, ReadRecord::new(), policy)
    }

    /// The same, carrying `reads` — the record the read side of the same assembly fills — as
    /// the read identity of every change a mutation makes.
    pub fn with_reads(
        workspace: Workspace,
        observed: ObservedFiles,
        reads: ReadRecord,
        policy: MutationPolicy,
    ) -> Self {
        Self {
            workspace,
            observed,
            reads,
            policy,
        }
    }
}

impl MutationService for MutationCapability {
    fn begin(&self) -> BoxFuture<'_, Box<dyn HeldMutation>> {
        // `begin_owned` waits without blocking a thread and holds nothing until it has the
        // gate, so the runtime may drop this future on a cancellation.
        let acquire = self
            .workspace
            .begin_owned(&self.observed, &self.reads, self.policy);
        Box::pin(async move { Box::new(Held(Arc::new(acquire.await))) as Box<dyn HeldMutation> })
    }
}

/// The held gate. Shared with the blocking task of a method in progress, so a call dropped
/// mid-write releases the gate only once that write has finished.
struct Held(Arc<OwnedMutation>);

impl Held {
    /// Runs `work` on a blocking thread: the file operations are synchronous and must never
    /// hold the async thread, as in the native tools.
    fn blocking(
        &self,
        work: impl FnOnce(&OwnedMutation) -> Result<(), MutationError> + Send + 'static,
    ) -> BoxFuture<'_, Result<(), FsError>> {
        let mutation = self.0.clone();
        Box::pin(async move {
            tokio::task::spawn_blocking(move || work(&mutation).map_err(fs_error))
                .await
                .unwrap_or_else(|error| Err(FsError::Io(format!("the change failed: {error}"))))
        })
    }
}

impl HeldMutation for Held {
    fn write(&self, path: String, contents: Vec<u8>) -> BoxFuture<'_, Result<(), FsError>> {
        self.blocking(move |mutation| mutation.write(&path, contents))
    }

    fn create(&self, path: String, contents: Vec<u8>) -> BoxFuture<'_, Result<(), FsError>> {
        self.blocking(move |mutation| mutation.create(&path, contents))
    }

    fn remove(&self, path: String) -> BoxFuture<'_, Result<(), FsError>> {
        self.blocking(move |mutation| mutation.remove(&path))
    }

    fn rename(&self, old_path: String, new_path: String) -> BoxFuture<'_, Result<(), FsError>> {
        self.blocking(move |mutation| mutation.rename(&old_path, &new_path))
    }
}

/// A mutation error as the module sees it. `MutationError` has one variant per `fs-error` case
/// a mutation can produce, so nothing is parsed; `io` carries the native text.
fn fs_error(error: MutationError) -> FsError {
    match error {
        MutationError::OutsideWorkspace { .. } => FsError::OutsideWorkspace,
        MutationError::NotFound { .. } => FsError::NotFound,
        MutationError::WrongKind { .. } => FsError::WrongKind,
        MutationError::AlreadyExists { .. } => FsError::AlreadyExists,
        MutationError::Io(message) => FsError::Io(message),
    }
}

// ---------------------------------------------------------------------------------------------
// The builders: the services a tool component's assembly is linked with.

/// The workspace, `snapshot` and `workspace-mutation` services of one agent, for a tool
/// component: `home` is the host's (issue #142), passed in as it is to the native read tool.
/// `mutation` is the mode the assembling row grants: `Some(Observed)` for the edit and write
/// rows, `Some(PatchAuthorized)` for the patch row, `None` for a tool that does not mutate
/// (its manifest does not grant `workspace-mutation` either way).
///
/// The services are call-scoped (ADR-0090): every export call gets its own read side and
/// mutation service over one fresh [`ReadRecord`], so the gated write of a call refuses a
/// target another agent changed after THIS call's read, and no read of an earlier or a
/// concurrent call of the same tool satisfies or blocks it.
pub fn tool_services(
    workspace: Workspace,
    observed: ObservedFiles,
    home: Option<PathBuf>,
    mutation: Option<MutationPolicy>,
) -> Services {
    Services::call_scoped(move || call_services(&workspace, &observed, home.clone(), mutation))
}

/// The services of one call: the read side and the mutation over the call's own record.
fn call_services(
    workspace: &Workspace,
    observed: &ObservedFiles,
    home: Option<PathBuf>,
    mutation: Option<MutationPolicy>,
) -> Services {
    let reads = ReadRecord::new();
    let read = Arc::new(ReadCapability::with_reads(
        workspace.clone(),
        observed.clone(),
        reads.clone(),
        home,
    ));
    Services {
        workspace: Some(read.clone()),
        snapshot: Some(read),
        workspace_mutation: mutation.map(|policy| {
            mutation_service_over(workspace.clone(), observed.clone(), reads, policy)
        }),
        ..Services::default()
    }
}

/// [`tool_services`] with the walk (`list-files`, `search`) beside the read side, so one
/// builder serves every tool component of the release — `p1/read`, `p1/edit`, `p1/write`,
/// `p1/patch` and `p1/search`.
///
/// The mutation is assembled patch-authorized, the only mode that serves all of them: the
/// patch component must never be held to an agent's observation (ADR-0025's exemption), and
/// the edit and write components check the observation themselves before they begin (ADR-0088
/// point 4). What the host enforces for all three, whatever the mode, is the read record,
/// call-scoped as in [`tool_services`]. A caller that knows the module's row uses
/// [`tool_services`] with its mode (the catalog does; a row that grants no mutation passes
/// `None`).
pub fn capability_services(
    workspace: Workspace,
    observed: ObservedFiles,
    home: Option<PathBuf>,
) -> Services {
    Services::call_scoped(move || {
        let mut services = call_services(
            &workspace,
            &observed,
            home.clone(),
            Some(MutationPolicy::PatchAuthorized),
        );
        let read = services
            .workspace
            .take()
            .expect("call_services links the read side");
        services.workspace = Some(Arc::new(ToolWorkspace {
            read,
            search: Arc::new(SearchCapability::new(workspace.clone())),
        }));
        services
    })
}

/// The workspace service of every tool component whose assembly carries the walk: the read
/// side (its credential refusal included), with the search tool's walk beside it. `stat`,
/// `read` and `snapshot` are the read tool's; the walk is the search tool's, and a search
/// module is linked the search service alone over its own workspace ([`search_services`]).
struct ToolWorkspace {
    read: Arc<dyn WorkspaceService>,
    search: Arc<SearchCapability>,
}

impl WorkspaceService for ToolWorkspace {
    fn stat(&self, path: String) -> BoxFuture<'_, Result<WorkspaceEntry, FsError>> {
        self.read.stat(path)
    }

    fn read(
        &self,
        path: String,
        offset: u64,
        length: u64,
    ) -> BoxFuture<'_, Result<Vec<u8>, FsError>> {
        self.read.read(path, offset, length)
    }

    fn list_files(
        &self,
        path: String,
        glob: Option<String>,
    ) -> BoxFuture<'_, Result<Vec<String>, FsError>> {
        self.search.list_files(path, glob)
    }

    fn search(&self, query: SearchQuery) -> BoxFuture<'_, Result<SearchResult, FsError>> {
        self.search.search(query)
    }
}

/// The `workspace-mutation` service of one agent, as the host links it into a module's
/// `Services::workspace_mutation`.
pub fn mutation_service(
    workspace: Workspace,
    observed: ObservedFiles,
    policy: MutationPolicy,
) -> Arc<dyn MutationService> {
    Arc::new(MutationCapability::new(workspace, observed, policy))
}

/// The same service over an existing read record, for an assembly that builds the read side
/// and the mutation together ([`tool_services`], [`capability_services`] and the catalog's
/// rows): the mutation then refuses a target changed after the read that computed the change.
pub fn mutation_service_over(
    workspace: Workspace,
    observed: ObservedFiles,
    reads: ReadRecord,
    policy: MutationPolicy,
) -> Arc<dyn MutationService> {
    Arc::new(MutationCapability::with_reads(
        workspace, observed, reads, policy,
    ))
}

/// The services of the `p1/search` component over one agent's workspace: the walk alone.
pub fn search_services(workspace: Workspace) -> Services {
    Services {
        workspace: Some(Arc::new(SearchCapability::new(workspace))),
        ..Services::default()
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
    async fn a_read_records_the_whole_file_whatever_window_was_asked_for() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.txt");
        std::fs::write(&path, "alpha\nbeta\n").unwrap();
        let reads = ReadRecord::new();
        let capability = ReadCapability::with_reads(
            Workspace::new(dir.path()).unwrap(),
            ObservedFiles::new(),
            reads.clone(),
            None,
        );
        assert_eq!(reads.recorded(&path), None, "nothing is read yet");

        // A window that stops before the end records the whole file, not the window.
        let window = WorkspaceService::read(&capability, "a.txt".into(), 0, 4)
            .await
            .unwrap();
        assert_eq!(window, b"alph");
        let whole = ReadRecord::new();
        whole.record(&path, b"alpha\nbeta\n");
        assert_eq!(
            reads.recorded(&path),
            whole.recorded(&path),
            "the identity is the whole file the window came from"
        );

        // A read of the new contents is the new identity, as the design says.
        std::fs::write(&path, "ALPHA\nBETA\n").unwrap();
        assert_eq!(reads.recorded(&path), whole.recorded(&path));
        WorkspaceService::read(&capability, "a.txt".into(), 0, 64)
            .await
            .unwrap();
        let changed = ReadRecord::new();
        changed.record(&path, b"ALPHA\nBETA\n");
        assert_eq!(reads.recorded(&path), changed.recorded(&path));
        assert_ne!(reads.recorded(&path), whole.recorded(&path));

        // A read that finds nothing records nothing: there is no contents to refuse
        // another state against, and a new file needs no observation either way.
        assert_eq!(
            WorkspaceService::read(&capability, "missing.txt".into(), 0, 64).await,
            Err(FsError::NotFound)
        );
        assert_eq!(reads.recorded(&dir.path().join("missing.txt")), None);
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
            Err(FsError::Io(p1_workspace::credential_refusal(
                ".codex/auth.json"
            )))
        );
        assert_eq!(
            WorkspaceService::read(&capability, ".codex/auth.json".into(), 0, 64).await,
            Err(FsError::Io(p1_workspace::credential_refusal(
                ".codex/auth.json"
            )))
        );
        assert_eq!(
            WorkspaceService::read(&capability, "sub".into(), 0, 64).await,
            Err(FsError::WrongKind)
        );
    }

    #[tokio::test]
    async fn a_dropped_request_cancels_its_walk() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "beta\n").unwrap();
        let capability = SearchCapability::new(Workspace::new(dir.path()).unwrap());
        // The work starts, the request is dropped while it runs (as the runtime drops it on
        // a cancellation), and the work then sees its token cancelled.
        let (started, work_started) = std::sync::mpsc::channel();
        let (release, work_released) = std::sync::mpsc::channel::<()>();
        let (seen, observed) = std::sync::mpsc::channel();
        let mut request = capability.blocking(move |_, cancel| {
            started.send(()).unwrap();
            work_released.recv().unwrap();
            seen.send(cancel.is_cancelled()).unwrap();
            Ok(())
        });
        let mut context = std::task::Context::from_waker(std::task::Waker::noop());
        assert!(request.as_mut().poll(&mut context).is_pending());
        work_started.recv().unwrap();
        drop(request);
        release.send(()).unwrap();
        assert!(
            observed.recv().unwrap(),
            "the walk must see the cancellation"
        );

        assert_eq!(
            capability.list_files(".".into(), None).await,
            Ok(vec!["a.txt".to_owned()])
        );
    }

    #[tokio::test]
    async fn the_policy_is_the_hosts_and_errors_keep_the_native_text() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "one\n").unwrap();
        let workspace = Workspace::new(dir.path()).unwrap();
        let observed = ObservedFiles::new();

        let observed_mode = mutation_service(
            workspace.clone(),
            observed.clone(),
            MutationPolicy::Observed,
        );
        let held = observed_mode.begin().await;
        assert_eq!(
            held.write("a.txt".into(), b"two\n".to_vec()).await,
            Err(FsError::Io(
                "You must read a.txt before changing it.".to_owned()
            ))
        );
        assert_eq!(
            held.write("../x.txt".into(), b"x".to_vec()).await,
            Err(FsError::OutsideWorkspace)
        );
        assert_eq!(held.remove("nope.txt".into()).await, Err(FsError::NotFound));
        assert_eq!(
            held.create("a.txt".into(), b"x".to_vec()).await,
            Err(FsError::AlreadyExists)
        );
        drop(held);

        // The same unobserved file under the patch exemption: changed, and observed.
        let patch_mode =
            mutation_service(workspace, observed.clone(), MutationPolicy::PatchAuthorized);
        let held = patch_mode.begin().await;
        held.write("a.txt".into(), b"patched\n".to_vec())
            .await
            .unwrap();
        held.rename("a.txt".into(), "b.txt".into()).await.unwrap();
        drop(held);
        assert_eq!(
            std::fs::read_to_string(dir.path().join("b.txt")).unwrap(),
            "patched\n"
        );
        assert_eq!(
            observed.check_unchanged(
                &dir.path().canonicalize().unwrap().join("b.txt"),
                b"patched\n"
            ),
            Observation::Unchanged
        );
    }

    /// The moved walk, directly: hidden entries and `.gitignore`d ones are left out (a
    /// scratch directory with no `.git` still honours its ignore file), symlinks are never
    /// followed, the display paths are root-relative and sorted bytewise, `glob` keeps only
    /// matching files, and a cancelled walk stops instead of running on unobserved.
    #[cfg(unix)]
    #[test]
    fn the_walk_skips_hidden_and_ignored_entries_and_stops_when_cancelled() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::create_dir_all(root.join("target")).unwrap();
        std::fs::create_dir_all(root.join(".hidden")).unwrap();
        std::fs::write(root.join(".gitignore"), "target/\n").unwrap();
        std::fs::write(root.join("b.txt"), "beta\n").unwrap();
        std::fs::write(root.join("src/a.rs"), "beta\n").unwrap();
        std::fs::write(root.join("target/x.rs"), "beta\n").unwrap();
        std::fs::write(root.join(".hidden/c.rs"), "beta\n").unwrap();
        std::fs::write(outside.path().join("o.txt"), "beta\n").unwrap();
        symlink(outside.path().join("o.txt"), root.join("link.txt")).unwrap();

        let workspace = Workspace::new(root).unwrap();
        let cancel = CancellationToken::new();
        assert_eq!(
            file_walk::list_files(&workspace, ".", None, &cancel),
            Ok(vec!["b.txt".to_owned(), "src/a.rs".to_owned()])
        );
        assert_eq!(
            file_walk::list_files(&workspace, ".", Some("*.rs"), &cancel),
            Ok(vec!["src/a.rs".to_owned()])
        );
        assert_eq!(
            file_walk::list_files(&workspace, "nope", None, &cancel),
            Err(FsError::NotFound)
        );
        assert_eq!(
            file_walk::list_files(&workspace, "../", None, &cancel),
            Err(FsError::OutsideWorkspace)
        );

        let query = SearchQuery {
            pattern: "beta".to_owned(),
            path: None,
            glob: None,
            case_insensitive: false,
            context: 0,
            max_lines: 10,
        };
        let result = file_walk::search(&workspace, &query, &cancel).unwrap();
        assert_eq!(
            result
                .files
                .iter()
                .map(|file| file.path.as_str())
                .collect::<Vec<_>>(),
            ["b.txt", "src/a.rs"],
            "the link and the ignored and hidden trees are out"
        );
        assert!(!result.truncated);
        assert_eq!(result.omitted_files, 0);

        cancel.cancel();
        assert_eq!(
            file_walk::list_files(&workspace, ".", None, &cancel),
            Err(FsError::Cancelled)
        );
        assert_eq!(
            file_walk::search(&workspace, &query, &cancel),
            Err(FsError::Cancelled)
        );
    }
}
