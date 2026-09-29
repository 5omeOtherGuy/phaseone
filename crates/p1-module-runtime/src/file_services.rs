//! The capability services of the p1 file tools, as the HOST builds them for a component:
//! the read side (`workspace` and `snapshot`), the walk (`list-files` and `search`) and the
//! owned mutation (`workspace-mutation`), over `p1-workspace` and the assembling agent's own
//! services.
//!
//! They lived in the tool crates (`p1-tool-read`, `p1-tool-search`, `p1-tool-write`) while
//! those crates were the host's file tools; since read, edit, write, apply_patch and grep are
//! served by their components alone (S7.10-R1, ADR-0095), the host cannot depend on them any
//! more: a service that links a component is the host's, so it lives where the capability
//! traits do, and the tool crates re-use it for their own native tools and tests.
//!
//! Every refusal is `p1-workspace`'s and comes before confinement: the credential files under
//! the agent's home (issue #142, [`p1_workspace::refuse_credentials`]) first, then
//! confinement, so a component can never reach what the native tool would not. Every message
//! a module receives is complete and safe to show the model, so the component passes `io` text
//! through unchanged.
//!
//! The services are call-scoped (ADR-0092): [`tool_services`] and [`capability_services`]
//! build each export call's own read side and mutation service over one fresh
//! [`ReadRecord`], so the gated write of a call refuses a target another agent changed after
//! THIS call's read, and no read of an earlier or a concurrent call of the same tool
//! satisfies or blocks it.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};

use p1_contracts::{BoxFuture, CancellationToken};
use p1_workspace::{
    CheckedPath, CredentialPolicy, FileKind, IndexCancelled, MutationError, MutationPolicy,
    Observation, ObservedFiles, OwnedMutation, ProtectedIndex, ReadRecord, Snapshot, Workspace,
    WorkspaceError, refuse_credentials, xdg_credentials,
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
/// Matches the component's per-read budget; host allocation is bounded independently.
const MAX_COMPONENT_READ_BYTES: u64 = 8 * 1024 * 1024;

fn protected_index_current(index: &ProtectedIndex, cancel: &CancellationToken) -> bool {
    #[cfg(unix)]
    {
        index.still_current(cancel).unwrap_or(false)
    }
    #[cfg(not(unix))]
    {
        let _ = (index, cancel);
        true
    }
}

/// The credential directory changed while a walk was reading it. The result is
/// discarded rather than returned with matches that were silently dropped.
fn stale_index_error() -> FsError {
    FsError::Io(
        "the protected credential directory changed during the search; retry the search".into(),
    )
}

/// The opened-file search check: a fresh exact-path refusal plus protected-index freshness.
/// Staleness is an error, never an ordinary exclusion, so it cannot silently drop matches.
fn search_opened_excluded(
    index: &ProtectedIndex,
    home: Option<&Path>,
    xdg_credentials: &[PathBuf],
    cancel: &CancellationToken,
    candidate: &Path,
    file: &std::fs::File,
) -> Result<bool, FsError> {
    if !protected_index_current(index, cancel) {
        return Err(stale_index_error());
    }
    let current = CredentialPolicy::new(home, xdg_credentials);
    Ok(current.refuses(candidate)
        || file.metadata().map_or(true, |metadata| {
            index.refuses_current_exact(&current, &metadata)
        }))
}

// ---------------------------------------------------------------------------------------------
// The read side: the `workspace` and `snapshot` capabilities of one agent.

/// One agent's `workspace` and `snapshot` capabilities.
#[derive(Clone)]
pub struct ReadCapability {
    inner: Arc<Inner>,
}

struct Inner {
    workspace: Workspace,
    mutation_recorded: Arc<std::sync::atomic::AtomicBool>,
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
                mutation_recorded: Arc::new(std::sync::atomic::AtomicBool::new(false)),
                xdg_credentials: xdg_credentials(),
                open: Mutex::new(Vec::new()),
            }),
        }
    }

    /// Runs `work` on a blocking thread: the file work is synchronous and must never hold the
    /// async thread, as in the native tool. `work` gets a token that is cancelled when the
    /// returned future is dropped — the runtime drops it as soon as the call is cancelled — so
    /// a protected-index scan stops instead of occupying the blocking pool unobserved.
    fn blocking<T: Send + 'static>(
        &self,
        work: impl FnOnce(&Inner, &CancellationToken) -> Result<T, FsError> + Send + 'static,
    ) -> BoxFuture<'_, Result<T, FsError>> {
        let inner = self.inner.clone();
        Box::pin(async move {
            let cancel = CancellationToken::new();
            let _stop_on_drop = cancel.clone().drop_guard();
            tokio::task::spawn_blocking(move || work(&inner, &cancel))
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

    fn opened_file(
        &self,
        checked: &CheckedPath,
        index: &ProtectedIndex,
        policy: &CredentialPolicy,
        cancel: &CancellationToken,
    ) -> Result<std::fs::File, FsError> {
        let refused = || FsError::Io(p1_workspace::credential_refusal(checked.display()));
        let file = self
            .workspace
            .open_file_at(checked.path())
            .map_err(|error| self.fs_error(error))?;
        let opened_path =
            file_walk::opened_object_path(&file, checked.path()).map_err(|_| refused())?;
        let metadata = file.metadata().map_err(|_| refused())?;
        if policy.refuses(&opened_path)
            || index.refuses_current_exact(policy, &metadata)
            || !protected_index_current(index, cancel)
        {
            return Err(refused());
        }
        Ok(file)
    }

    fn stat(&self, requested: &str, cancel: &CancellationToken) -> Result<WorkspaceEntry, FsError> {
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
        let size = if kind == EntryKind::File {
            let policy = CredentialPolicy::new(self.home.as_deref(), &self.xdg_credentials);
            let index = ProtectedIndex::build(&policy, cancel)
                .map_err(|IndexCancelled| FsError::Cancelled)?;
            self.opened_file(&checked, &index, &policy, cancel)?
                .metadata()
                .map_err(|error| FsError::Io(error.to_string()))?
                .len()
        } else {
            0
        };
        Ok(WorkspaceEntry {
            path: checked.display().to_owned(),
            kind,
            size,
        })
    }

    fn read(
        &self,
        requested: &str,
        offset: u64,
        length: u64,
        cancel: &CancellationToken,
    ) -> Result<Vec<u8>, FsError> {
        self.read_with_before_open(requested, offset, length, cancel, || {})
    }

    fn read_with_before_open(
        &self,
        requested: &str,
        offset: u64,
        length: u64,
        cancel: &CancellationToken,
        before_open: impl FnOnce(),
    ) -> Result<Vec<u8>, FsError> {
        let checked = self.check(requested)?;
        let key = checked.path().to_path_buf();
        // A read from the start takes a fresh snapshot; later windows come from the same one,
        // so a module sees one state of the file, never a mix. The scratch registry keeps this
        // read from counting as an observation: the module records one through
        // `snapshot.observe` only once it has accepted what it read, as the native tool
        // records only a successful read.
        let snapshot = match self.take_open(&key).filter(|_| offset > 0) {
            Some(snapshot) => snapshot,
            None => {
                // The protected index is built only when a new descriptor is actually opened.
                // A continuation window reuses the checked snapshot, so it must not rescan the
                // whole credential directory on every 64 KiB window of a large file.
                let policy = CredentialPolicy::new(self.home.as_deref(), &self.xdg_credentials);
                let index = ProtectedIndex::build(&policy, cancel)
                    .map_err(|IndexCancelled| FsError::Cancelled)?;
                before_open();
                let file = self.opened_file(&checked, &index, &policy, cancel)?;
                self.workspace
                    .snapshot_from_open_file(&key, file, MAX_COMPONENT_READ_BYTES)
                    .map_err(|error| self.fs_error(error))?
            }
        };
        let offset = usize::try_from(offset).unwrap_or(usize::MAX);
        let length = usize::try_from(length).unwrap_or(usize::MAX);
        let window = snapshot.read(offset, length).to_vec();
        let size = snapshot.metadata().size;
        // What this read returned is this tool's read identity of the file, whatever window
        // was asked for: the whole-file digest of the snapshot the bytes come from. The latest
        // read of a path wins, and a mutation assembled with the same record refuses any other
        // bytes at that path under the gate. The spelling is recorded too, so the mutation
        // refuses a source path that now resolves elsewhere (a retargeted symlink).
        self.reads.record_read(
            &self.workspace.spelling(requested),
            &key,
            snapshot.metadata().content_hash,
        );
        // An exactly-full last window still needs an explicit EOF probe: otherwise
        // its next read could open another version and overwrite the read identity.
        if (offset as u64).saturating_add(window.len() as u64) <= size && !window.is_empty() {
            self.keep_open(key, snapshot);
        }
        Ok(window)
    }

    fn observe(&self, requested: &str, contents: &[u8]) -> Result<(), FsError> {
        // Mutations already recorded the actual opened destination under the gate;
        // re-resolving a requested symlink here can observe an unrelated target.
        if self
            .mutation_recorded
            .load(std::sync::atomic::Ordering::Acquire)
        {
            return Ok(());
        }
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
        self.blocking(move |inner, cancel| inner.stat(&path, cancel))
    }

    fn read(
        &self,
        path: String,
        offset: u64,
        length: u64,
    ) -> BoxFuture<'_, Result<Vec<u8>, FsError>> {
        self.blocking(move |inner, cancel| inner.read(&path, offset, length, cancel))
    }
}

impl SnapshotService for ReadCapability {
    fn observe(&self, path: String, contents: Vec<u8>) -> BoxFuture<'_, Result<(), FsError>> {
        self.blocking(move |inner, _cancel| inner.observe(&path, &contents))
    }

    fn check(
        &self,
        path: String,
        current: Vec<u8>,
    ) -> BoxFuture<'_, Result<SnapshotObservation, FsError>> {
        self.blocking(move |inner, _cancel| inner.compare(&path, &current))
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
    home: Option<PathBuf>,
    xdg_credentials: Vec<PathBuf>,
    index: IndexCache,
    /// The calling tool's cancellation token, when a native caller wired one in. The
    /// blocking operations make a child token of it, so cancelling the call stops a walk
    /// that is already running, not only dropping its future.
    cancel: Option<CancellationToken>,
    #[cfg(test)]
    after_index: Option<Arc<dyn Fn() + Send + Sync>>,
    #[cfg(test)]
    before_search: Option<Arc<dyn Fn() + Send + Sync>>,
}

/// The protected-index cache of one search capability. A call captures a request-scoped index;
/// [`cached_index`] revalidates its directory stamps, [`cached_index_reused`] reuses it without
/// the full-tree rescan, and the test-only count proves ordinary candidates take the cheap path.
#[derive(Clone)]
struct IndexCache {
    index: Arc<Mutex<Option<Arc<ProtectedIndex>>>>,
    #[cfg(all(test, unix))]
    revalidations: Arc<std::sync::atomic::AtomicUsize>,
}

impl IndexCache {
    fn new() -> Self {
        Self {
            index: Arc::new(Mutex::new(None)),
            #[cfg(all(test, unix))]
            revalidations: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        }
    }
}

impl SearchCapability {
    /// The capability over `workspace`, refusing credentials under the agent's home.
    pub fn new(workspace: Workspace, home: Option<PathBuf>) -> Self {
        Self {
            workspace,
            home,
            xdg_credentials: xdg_credentials(),
            index: IndexCache::new(),
            cancel: None,
            #[cfg(test)]
            after_index: None,
            #[cfg(test)]
            before_search: None,
        }
    }

    /// Wire the calling tool's cancellation token in: every blocking operation's own token
    /// becomes its child, so cancelling the call stops the work (the native `grep` passes the
    /// context token here). The component's service keeps its drop-token only.
    pub fn with_cancel(mut self, cancel: CancellationToken) -> Self {
        self.cancel = Some(cancel);
        self
    }

    /// Runs `work` on a blocking thread: the walk and the reads are synchronous and must never
    /// hold the async thread, as in the native tool. `work` gets a token that is cancelled when
    /// the returned future is dropped — the runtime drops it as soon as the call is cancelled —
    /// and that is also a child of the caller's token when one was wired in, so the walk stops
    /// at the next file instead of running on unobserved.
    fn blocking<T: Send + 'static>(
        &self,
        work: impl FnOnce(&Workspace, &CancellationToken) -> Result<T, FsError> + Send + 'static,
    ) -> BoxFuture<'_, Result<T, FsError>> {
        let workspace = self.workspace.clone();
        let parent = self.cancel.clone();
        Box::pin(async move {
            let cancel = match parent {
                Some(parent) => parent.child_token(),
                None => CancellationToken::new(),
            };
            let _stop_on_drop = cancel.clone().drop_guard();
            tokio::task::spawn_blocking(move || work(&workspace, &cancel))
                .await
                .unwrap_or_else(|error| Err(FsError::Io(format!("search failed: {error}"))))
        })
    }
}

fn cached_index(
    cache: &IndexCache,
    policy: &CredentialPolicy,
    cancel: &CancellationToken,
) -> Result<Arc<ProtectedIndex>, FsError> {
    let mut guard = cache
        .index
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some(index) = guard.as_ref()
        && index.matches_policy(policy)
    {
        #[cfg(all(test, unix))]
        cache
            .revalidations
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        if index
            .still_current(cancel)
            .map_err(|IndexCancelled| FsError::Cancelled)?
        {
            return Ok(index.clone());
        }
    }
    let index = Arc::new(
        ProtectedIndex::build(policy, cancel).map_err(|IndexCancelled| FsError::Cancelled)?,
    );
    *guard = Some(index.clone());
    Ok(index)
}

/// The request-scoped index without revalidating its stamps: a single-link candidate cannot
/// alias a protected inode, so the captured identities add nothing the exact-path recheck does
/// not already cover, and the caller avoids a full-tree rescan per candidate.
#[cfg(unix)]
fn cached_index_reused(
    cache: &IndexCache,
    policy: &CredentialPolicy,
    cancel: &CancellationToken,
) -> Result<Arc<ProtectedIndex>, FsError> {
    let mut guard = cache
        .index
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some(index) = guard.as_ref()
        && index.matches_policy(policy)
    {
        return Ok(index.clone());
    }
    let index = Arc::new(
        ProtectedIndex::build(policy, cancel).map_err(|IndexCancelled| FsError::Cancelled)?,
    );
    *guard = Some(index.clone());
    Ok(index)
}

fn refuses_at_open(
    cache: &IndexCache,
    policy: &CredentialPolicy,
    cancel: &CancellationToken,
    metadata: &std::fs::Metadata,
) -> Result<bool, FsError> {
    #[cfg(unix)]
    let index = if may_alias_a_protected_inode(metadata) {
        cached_index(cache, policy, cancel)?
    } else {
        cached_index_reused(cache, policy, cancel)?
    };
    // Without Unix link counts every candidate must revalidate.
    #[cfg(not(unix))]
    let index = cached_index(cache, policy, cancel)?;
    Ok(index.refuses_current_exact(policy, metadata))
}

/// Whether `metadata` can share an inode with a protected file: only a multiply-linked file
/// can alias one. A single-link file is protected, if at all, by its own path, which
/// `CredentialPolicy::refuses` and the exact-path recheck still cover.
#[cfg(unix)]
fn may_alias_a_protected_inode(metadata: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;
    metadata.nlink() > 1
}

impl WorkspaceService for SearchCapability {
    fn stat(&self, path: String) -> BoxFuture<'_, Result<WorkspaceEntry, FsError>> {
        let home = self.home.clone();
        let xdg_credentials = self.xdg_credentials.clone();
        let cache = self.index.clone();
        self.blocking(move |workspace, cancel| {
            let credential_policy = CredentialPolicy::new(home.as_deref(), &xdg_credentials);
            cached_index(&cache, &credential_policy, cancel)?;
            credential_policy
                .refuse(workspace, &path)
                .map_err(FsError::Io)?;
            let checked = workspace
                .check_path(&path)
                .map_err(file_walk::workspace_error)?;
            let stat = workspace.stat(&path).map_err(file_walk::workspace_error)?;
            let (kind, size) = if stat.kind == FileKind::File {
                let file = workspace
                    .open_file_at(checked.path())
                    .map_err(file_walk::workspace_error)?;
                let metadata = file
                    .metadata()
                    .map_err(|error| FsError::Io(error.to_string()))?;
                let refused = || FsError::Io(p1_workspace::credential_refusal(checked.display()));
                let opened_path =
                    file_walk::opened_object_path(&file, checked.path()).map_err(|_| refused())?;
                let current = CredentialPolicy::new(home.as_deref(), &xdg_credentials);
                if current.refuses(&opened_path)
                    || refuses_at_open(&cache, &current, cancel, &metadata)?
                {
                    return Err(refused());
                }
                (EntryKind::File, metadata.len())
            } else {
                (
                    if stat.kind == FileKind::Directory {
                        EntryKind::Directory
                    } else {
                        EntryKind::Other
                    },
                    0,
                )
            };
            Ok(WorkspaceEntry {
                path: checked.display().to_owned(),
                kind,
                size,
            })
        })
    }

    fn read(
        &self,
        path: String,
        offset: u64,
        length: u64,
    ) -> BoxFuture<'_, Result<Vec<u8>, FsError>> {
        // Only the requested window, as the native `grep` reads: file-list mode sniffs a
        // prefix of every listed file, which must not load a large file whole.
        let home = self.home.clone();
        let xdg_credentials = self.xdg_credentials.clone();
        let cache = self.index.clone();
        #[cfg(test)]
        let after_index = self.after_index.clone();
        self.blocking(move |workspace, cancel| {
            let credential_policy = CredentialPolicy::new(home.as_deref(), &xdg_credentials);
            cached_index(&cache, &credential_policy, cancel)?;
            #[cfg(test)]
            if let Some(after_index) = after_index {
                after_index();
            }
            credential_policy
                .refuse(workspace, &path)
                .map_err(FsError::Io)?;
            file_walk::read_window_excluding(
                workspace,
                &path,
                offset,
                length,
                &|candidate, file| {
                    let current = CredentialPolicy::new(home.as_deref(), &xdg_credentials);
                    if current.refuses(candidate) {
                        return Ok(true);
                    }
                    match file.metadata() {
                        Ok(metadata) => refuses_at_open(&cache, &current, cancel, &metadata),
                        Err(_) => Ok(true),
                    }
                },
            )
        })
    }

    fn list_files(
        &self,
        path: String,
        glob: Option<String>,
    ) -> BoxFuture<'_, Result<Vec<String>, FsError>> {
        let home = self.home.clone();
        let xdg_credentials = self.xdg_credentials.clone();
        let cache = self.index.clone();
        self.blocking(move |workspace, cancel| {
            let credential_policy = CredentialPolicy::new(home.as_deref(), &xdg_credentials);
            cached_index(&cache, &credential_policy, cancel)?;
            credential_policy
                .refuse(workspace, &path)
                .map_err(FsError::Io)?;
            file_walk::list_files_excluding(
                workspace,
                &path,
                glob.as_deref(),
                cancel,
                |candidate| {
                    let current = CredentialPolicy::new(home.as_deref(), &xdg_credentials);
                    if current.refuses(candidate) {
                        return Ok(true);
                    }
                    match std::fs::metadata(candidate) {
                        Ok(metadata) => refuses_at_open(&cache, &current, cancel, &metadata),
                        Err(_) => Ok(true),
                    }
                },
            )
        })
    }

    fn search(&self, query: SearchQuery) -> BoxFuture<'_, Result<SearchResult, FsError>> {
        let home = self.home.clone();
        let xdg_credentials = self.xdg_credentials.clone();
        let cache = self.index.clone();
        #[cfg(test)]
        let before_search = self.before_search.clone();
        self.blocking(move |workspace, cancel| {
            let credential_policy = CredentialPolicy::new(home.as_deref(), &xdg_credentials);
            let index = cached_index(&cache, &credential_policy, cancel)?;
            #[cfg(test)]
            if let Some(before_search) = before_search {
                before_search();
            }
            if let Some(path) = query.path.as_deref() {
                credential_policy
                    .refuse(workspace, path)
                    .map_err(FsError::Io)?;
            }
            file_walk::search_excluding_opened(
                workspace,
                &query,
                cancel,
                |candidate| {
                    let current = CredentialPolicy::new(home.as_deref(), &xdg_credentials);
                    if current.refuses(candidate) {
                        return Ok(true);
                    }
                    match std::fs::metadata(candidate) {
                        Ok(metadata) => refuses_at_open(&cache, &current, cancel, &metadata),
                        Err(_) => Ok(true),
                    }
                },
                |candidate, file| {
                    search_opened_excluded(
                        &index,
                        home.as_deref(),
                        &xdg_credentials,
                        cancel,
                        candidate,
                        file,
                    )
                },
            )
        })
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
    mutation_recorded: Option<Arc<std::sync::atomic::AtomicBool>>,
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
            mutation_recorded: None,
        }
    }

    fn with_observation_marker(mut self, marker: Arc<std::sync::atomic::AtomicBool>) -> Self {
        self.mutation_recorded = Some(marker);
        self
    }
}

impl MutationService for MutationCapability {
    fn begin(&self) -> BoxFuture<'_, Box<dyn HeldMutation>> {
        // `begin_owned` waits without blocking a thread and holds nothing until it has the
        // gate, so the runtime may drop this future on a cancellation.
        let acquire = self
            .workspace
            .begin_owned(&self.observed, &self.reads, self.policy);
        let marker = self.mutation_recorded.clone();
        Box::pin(
            async move { Box::new(Held(Arc::new(acquire.await), marker)) as Box<dyn HeldMutation> },
        )
    }
}

/// The held gate. Shared with the blocking task of a method in progress, so a call dropped
/// mid-write releases the gate only once that write has finished.
struct Held(
    Arc<OwnedMutation>,
    Option<Arc<std::sync::atomic::AtomicBool>>,
);

impl Held {
    /// Runs `work` on a blocking thread: the file operations are synchronous and must never
    /// hold the async thread, as in the native tools.
    fn blocking(
        &self,
        work: impl FnOnce(&OwnedMutation) -> Result<(), MutationError> + Send + 'static,
    ) -> BoxFuture<'_, Result<(), FsError>> {
        let mutation = self.0.clone();
        let marker = self.1.clone();
        Box::pin(async move {
            tokio::task::spawn_blocking(move || {
                work(&mutation)
                    .map(|()| {
                        if let Some(marker) = marker {
                            marker.store(true, std::sync::atomic::Ordering::Release);
                        }
                    })
                    .map_err(fs_error)
            })
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
/// The services are call-scoped (ADR-0092): every export call gets its own read side and
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
    let mutation_workspace = workspace.clone().with_credential_home(home.clone());
    let read = Arc::new(ReadCapability::with_reads(
        workspace.clone(),
        observed.clone(),
        reads.clone(),
        home,
    ));
    let marker = read.inner.mutation_recorded.clone();
    Services {
        workspace: Some(read.clone()),
        snapshot: Some(read),
        workspace_mutation: mutation.map(|policy| {
            Arc::new(
                MutationCapability::with_reads(mutation_workspace, observed.clone(), reads, policy)
                    .with_observation_marker(marker),
            ) as Arc<dyn MutationService>
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
            search: Arc::new(SearchCapability::new(workspace.clone(), home.clone())),
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
pub fn search_services(workspace: Workspace, home: Option<PathBuf>) -> Services {
    Services {
        workspace: Some(Arc::new(SearchCapability::new(workspace, home))),
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

    #[cfg(unix)]
    #[test]
    fn read_capability_refuses_credential_hardlinks_even_after_check_open_swap() {
        let dir = tempfile::tempdir().unwrap();
        let protected = dir.path().join(".config/keys/token.key");
        std::fs::create_dir_all(protected.parent().unwrap()).unwrap();
        std::fs::write(&protected, b"synthetic private fixture").unwrap();
        let notes = dir.path().join("notes.txt");
        std::fs::write(&notes, b"public").unwrap();
        let observed = ObservedFiles::new();
        let capability = ReadCapability::new(
            Workspace::new(dir.path()).unwrap(),
            observed.clone(),
            Some(dir.path().to_path_buf()),
        );
        let outcome = capability.inner.read_with_before_open(
            "notes.txt",
            0,
            64,
            &CancellationToken::new(),
            || {
                std::fs::remove_file(&notes).unwrap();
                std::fs::hard_link(&protected, &notes).unwrap();
            },
        );
        assert!(matches!(outcome, Err(FsError::Io(_))));
        assert_eq!(
            observed.check_unchanged(&notes, b"synthetic private fixture"),
            Observation::NeverObserved
        );
        assert!(matches!(
            capability
                .inner
                .stat("notes.txt", &CancellationToken::new()),
            Err(FsError::Io(_))
        ));
    }

    #[cfg(unix)]
    #[test]
    fn read_capability_refuses_new_protected_directory_inodes() {
        let dir = tempfile::tempdir().unwrap();
        let notes = dir.path().join("notes.txt");
        std::fs::write(&notes, b"public").unwrap();
        let cap = ReadCapability::new(
            Workspace::new(dir.path()).unwrap(),
            ObservedFiles::new(),
            Some(dir.path().to_path_buf()),
        );
        let result =
            cap.inner
                .read_with_before_open("notes.txt", 0, 64, &CancellationToken::new(), || {
                    let protected = dir.path().join(".config/keys/new.key");
                    std::fs::create_dir_all(protected.parent().unwrap()).unwrap();
                    std::fs::write(&protected, b"private fixture").unwrap();
                    std::fs::remove_file(&notes).unwrap();
                    std::fs::hard_link(&protected, &notes).unwrap();
                });
        assert!(matches!(result, Err(FsError::Io(_))));
    }

    #[cfg(unix)]
    #[test]
    fn search_index_detects_protected_directory_added_before_open() {
        let dir = tempfile::tempdir().unwrap();
        let policy = CredentialPolicy::new(Some(dir.path()), &[]);
        let cancel = CancellationToken::new();
        let index = ProtectedIndex::build(&policy, &cancel).unwrap();
        let protected = dir.path().join(".config/keys/new.key");
        std::fs::create_dir_all(protected.parent().unwrap()).unwrap();
        std::fs::write(&protected, b"synthetic fixture").unwrap();
        let alias = dir.path().join("notes.txt");
        std::fs::hard_link(&protected, &alias).unwrap();
        let file = std::fs::File::open(&alias).unwrap();
        assert!(!index.still_current(&cancel).unwrap());
        assert!(!index.refuses_current_exact(&policy, &file.metadata().unwrap()));
        // The check the search walk runs on each opened file reports the staleness as an
        // error, so the walk fails closed instead of dropping the file as an exclusion.
        let outcome =
            super::search_opened_excluded(&index, Some(dir.path()), &[], &cancel, &alias, &file);
        assert!(
            matches!(outcome, Err(FsError::Io(message)) if message.contains("changed during the search"))
        );
    }

    #[tokio::test]
    async fn a_dropped_read_request_cancels_its_index_scan() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "beta\n").unwrap();
        let capability = ReadCapability::new(
            Workspace::new(dir.path()).unwrap(),
            ObservedFiles::new(),
            None,
        );
        // The read side's own blocking work now gets a drop-guarded token, so a cancelled
        // call stops a protected-index scan instead of leaving it on the blocking pool.
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
            "the read's blocking work must see the cancellation"
        );
    }

    #[test]
    fn bounded_host_snapshot_refuses_growth_after_stat() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("growing.txt");
        std::fs::write(&path, b"ok").unwrap();
        let cap = ReadCapability::new(
            Workspace::new(dir.path()).unwrap(),
            ObservedFiles::new(),
            None,
        );
        assert_eq!(
            cap.inner
                .stat("growing.txt", &CancellationToken::new())
                .unwrap()
                .size,
            2
        );
        let result = cap.inner.read_with_before_open(
            "growing.txt",
            0,
            16,
            &CancellationToken::new(),
            || {
                std::fs::OpenOptions::new()
                    .write(true)
                    .open(&path)
                    .unwrap()
                    .set_len(super::MAX_COMPONENT_READ_BYTES + 1)
                    .unwrap();
            },
        );
        assert!(matches!(result, Err(FsError::Io(message)) if message.contains("read budget")));
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

    #[cfg(unix)]
    #[tokio::test]
    async fn post_mutation_observe_cannot_follow_a_retargeted_link() {
        use std::os::unix::fs::symlink;
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a"), b"new").unwrap();
        std::fs::write(dir.path().join("b"), b"new").unwrap();
        symlink("a", dir.path().join("link")).unwrap();
        let observed = ObservedFiles::new();
        let read = ReadCapability::new(Workspace::new(dir.path()).unwrap(), observed.clone(), None);
        read.inner
            .mutation_recorded
            .store(true, std::sync::atomic::Ordering::Release);
        std::fs::remove_file(dir.path().join("link")).unwrap();
        symlink("b", dir.path().join("link")).unwrap();
        read.observe("link".into(), b"new".to_vec()).await.unwrap();
        assert_eq!(
            observed.check_unchanged(&dir.path().join("b"), b"new"),
            Observation::NeverObserved
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_held_mutation_marks_the_paired_read_side() {
        use std::os::unix::fs::symlink;
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a"), b"old").unwrap();
        std::fs::write(dir.path().join("b"), b"old").unwrap();
        symlink("a", dir.path().join("link")).unwrap();
        let observed = ObservedFiles::new();
        let services = tool_services(
            Workspace::new(dir.path()).unwrap(),
            observed.clone(),
            None,
            Some(MutationPolicy::Observed),
        );
        let workspace = services.workspace.clone().unwrap();
        let snapshot = services.snapshot.clone().unwrap();
        let mutation = services.workspace_mutation.clone().unwrap();
        // The observation the held write rechecks comes from the paired read side, and the
        // marker is set by the successful mutation itself, never by hand.
        workspace.read("link".into(), 0, 64).await.unwrap();
        snapshot
            .observe("link".into(), b"old".to_vec())
            .await
            .unwrap();
        let held = mutation.begin().await;
        held.write("link".into(), b"new".to_vec()).await.unwrap();
        drop(held);
        // Retarget the link; the mutation's marker must make the later observe a no-op so
        // the unrelated target stays unobserved.
        std::fs::remove_file(dir.path().join("link")).unwrap();
        symlink("b", dir.path().join("link")).unwrap();
        snapshot
            .observe("link".into(), b"new".to_vec())
            .await
            .unwrap();
        assert_eq!(
            observed.check_unchanged(&dir.path().join("b"), b"new"),
            Observation::NeverObserved
        );
    }

    #[tokio::test]
    async fn exactly_full_window_keeps_the_original_snapshot_until_eof() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("file");
        std::fs::write(&path, b"abcd").unwrap();
        let reads = ReadRecord::new();
        let capability = ReadCapability::with_reads(
            Workspace::new(dir.path()).unwrap(),
            ObservedFiles::new(),
            reads.clone(),
            None,
        );
        assert_eq!(
            WorkspaceService::read(&capability, "file".into(), 0, 4)
                .await
                .unwrap(),
            b"abcd"
        );
        let original = reads.recorded(&path);
        std::fs::write(&path, b"WXYZsuffix").unwrap();
        assert_eq!(
            WorkspaceService::read(&capability, "file".into(), 4, 4)
                .await
                .unwrap(),
            b""
        );
        assert_eq!(reads.recorded(&path), original);
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
        let capability = SearchCapability::new(Workspace::new(dir.path()).unwrap(), None);
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

    #[cfg(unix)]
    #[tokio::test]
    async fn search_capability_tracks_home_config_symlink_changes() {
        use std::os::unix::fs::symlink;

        let home = tempfile::tempdir().unwrap();
        let config = home.path().join(".config");
        let credential = config.join("p1/auth.json");
        std::fs::create_dir_all(credential.parent().unwrap()).unwrap();
        std::fs::write(&credential, "credential-marker\n").unwrap();
        std::fs::write(home.path().join("notes.txt"), "ordinary file\n").unwrap();
        let capability = SearchCapability::new(
            Workspace::new(home.path()).unwrap(),
            Some(home.path().to_path_buf()),
        );

        std::fs::rename(&config, home.path().join(".config-real")).unwrap();
        symlink(".config-real", &config).unwrap();

        assert_eq!(
            capability.stat(".config/p1/auth.json".into()).await,
            Err(FsError::Io(p1_workspace::credential_refusal(
                ".config/p1/auth.json"
            )))
        );
        assert_eq!(
            capability.read(".config/p1/auth.json".into(), 0, 128).await,
            Err(FsError::Io(p1_workspace::credential_refusal(
                ".config/p1/auth.json"
            )))
        );
        assert_eq!(
            capability.list_files(".".into(), None).await,
            Ok(vec!["notes.txt".to_owned()])
        );
        let result = capability
            .search(SearchQuery {
                pattern: "credential-marker".into(),
                path: None,
                glob: None,
                case_insensitive: false,
                context: 0,
                max_lines: 10,
            })
            .await
            .unwrap();
        assert!(result.files.is_empty());
    }

    #[test]
    fn cancellation_during_file_exclusion_returns_cancelled() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "content\n").unwrap();
        let workspace = Workspace::new(dir.path()).unwrap();
        let cancel = CancellationToken::new();

        let result = file_walk::list_files_excluding(&workspace, ".", None, &cancel, |_| {
            cancel.cancel();
            Ok(false)
        });

        assert_eq!(result, Err(FsError::Cancelled));
    }

    /// A cancellation observed while a multiply linked candidate's index is revalidated
    /// must surface as cancellation, not be folded into a credential refusal (or, on the
    /// last file of a search, into a successful empty result).
    #[cfg(unix)]
    #[tokio::test]
    async fn cancelling_during_an_index_refresh_returns_cancelled() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("candidate");
        std::fs::write(&file, b"content").unwrap();
        std::fs::hard_link(&file, dir.path().join("candidate-link")).unwrap();
        let cancel = CancellationToken::new();
        let trigger = cancel.clone();
        let mut capability = SearchCapability::new(Workspace::new(dir.path()).unwrap(), None);
        capability.after_index = Some(Arc::new(move || trigger.cancel()));
        let capability = capability.with_cancel(cancel.clone());

        // The candidate is multiply linked, so its open-time check revalidates the index
        // with the now-cancelled child token.
        let result = capability.read("candidate".into(), 0, 32).await;

        assert_eq!(result, Err(FsError::Cancelled));
    }

    /// The native `grep` wires its call's cancellation token into the capability: cancelling
    /// the call while a walk is running stops it, rather than only dropping a future the
    /// native `block_on` keeps alive.
    #[tokio::test]
    async fn cancelling_the_calling_tool_stops_a_capability_search() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "needle\n").unwrap();
        let cancel = CancellationToken::new();
        let trigger = cancel.clone();
        let mut capability = SearchCapability::new(Workspace::new(dir.path()).unwrap(), None);
        capability.before_search = Some(Arc::new(move || trigger.cancel()));
        let capability = capability.with_cancel(cancel.clone());

        let result = capability
            .search(SearchQuery {
                pattern: "needle".into(),
                path: None,
                glob: None,
                case_insensitive: false,
                context: 0,
                max_lines: 10,
            })
            .await;

        assert_eq!(result, Err(FsError::Cancelled));
    }

    #[tokio::test]
    async fn search_capability_refuses_credentials_in_every_mode() {
        let dir = tempfile::tempdir().unwrap();
        let relative_credential = ".config/p1/auth.json";
        let credential = dir.path().join(relative_credential);
        std::fs::create_dir_all(credential.parent().unwrap()).unwrap();
        std::fs::write(&credential, "credential-marker\n").unwrap();
        std::fs::write(dir.path().join("notes.txt"), "safe match\n").unwrap();
        let workspace = Workspace::new(dir.path()).unwrap();
        // Empty XDG list proves refusal comes from the agent-home policy.
        let capability = SearchCapability {
            workspace,
            home: Some(dir.path().to_path_buf()),
            xdg_credentials: vec![],
            index: super::IndexCache::new(),
            cancel: None,
            after_index: None,
            before_search: None,
        };

        assert_eq!(
            capability.stat(relative_credential.into()).await,
            Err(FsError::Io(p1_workspace::credential_refusal(
                relative_credential
            )))
        );
        assert_eq!(
            capability.read(relative_credential.into(), 0, 128).await,
            Err(FsError::Io(p1_workspace::credential_refusal(
                relative_credential
            )))
        );
        assert_eq!(
            capability.list_files(".".into(), None).await,
            Ok(vec!["notes.txt".to_owned()])
        );

        let result = capability
            .search(SearchQuery {
                pattern: "credential-marker|safe match".into(),
                path: None,
                glob: None,
                case_insensitive: false,
                context: 0,
                max_lines: 10,
            })
            .await
            .unwrap();
        assert_eq!(
            result
                .files
                .iter()
                .map(|file| file.path.as_str())
                .collect::<Vec<_>>(),
            ["notes.txt"],
            "content search must neither match nor list credential files"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn search_refuses_hard_link_to_credential() {
        let home = tempfile::tempdir().unwrap();
        let credential = home.path().join(".codex/auth.json");
        std::fs::create_dir_all(credential.parent().unwrap()).unwrap();
        std::fs::write(&credential, "hard-link-marker").unwrap();
        std::fs::hard_link(&credential, home.path().join("notes.txt")).unwrap();
        let capability = SearchCapability::new(
            Workspace::new(home.path()).unwrap(),
            Some(home.path().to_path_buf()),
        );
        assert_eq!(
            capability.read("notes.txt".into(), 0, 64).await,
            Err(FsError::Io(p1_workspace::credential_refusal("notes.txt")))
        );
        assert!(
            capability
                .search(SearchQuery {
                    pattern: "hard-link-marker".into(),
                    path: None,
                    glob: None,
                    case_insensitive: false,
                    context: 0,
                    max_lines: 10,
                })
                .await
                .unwrap()
                .files
                .is_empty()
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn search_refuses_hard_links_into_credential_directories() {
        for credential_path in [".config/keys/a.key", ".config/keys/nested/b.key"] {
            let home = tempfile::tempdir().unwrap();
            let credential = home.path().join(credential_path);
            std::fs::create_dir_all(credential.parent().unwrap()).unwrap();
            std::fs::write(&credential, "directory-secret-marker").unwrap();
            std::fs::hard_link(&credential, home.path().join("notes.txt")).unwrap();
            let capability = SearchCapability::new(
                Workspace::new(home.path()).unwrap(),
                Some(home.path().to_path_buf()),
            );
            assert_eq!(
                capability.read("notes.txt".into(), 0, 64).await,
                Err(FsError::Io(p1_workspace::credential_refusal("notes.txt"))),
                "hard link to {credential_path} must be refused"
            );
            assert!(
                capability
                    .search(SearchQuery {
                        pattern: "directory-secret-marker".into(),
                        path: None,
                        glob: None,
                        case_insensitive: false,
                        context: 0,
                        max_lines: 10,
                    })
                    .await
                    .unwrap()
                    .files
                    .is_empty(),
                "hard link to {credential_path} must not match"
            );
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn aliases_are_absent_from_list_files_and_empty_pattern_files_mode() {
        let home = tempfile::tempdir().unwrap();
        let credential = home.path().join(".codex/auth.json");
        std::fs::create_dir_all(credential.parent().unwrap()).unwrap();
        std::fs::write(&credential, "marker").unwrap();
        std::fs::hard_link(&credential, home.path().join("alias.txt")).unwrap();
        std::fs::write(home.path().join("safe.txt"), "safe").unwrap();
        let cap = SearchCapability::new(
            Workspace::new(home.path()).unwrap(),
            Some(home.path().into()),
        );
        assert_eq!(
            cap.list_files(".".into(), None).await.unwrap(),
            vec!["safe.txt"]
        );
        assert!(
            cap.search(SearchQuery {
                pattern: String::new(),
                path: Some("alias.txt".into()),
                glob: None,
                case_insensitive: false,
                context: 0,
                max_lines: 10,
            })
            .await
            .unwrap()
            .files
            .is_empty()
        );
        assert_eq!(
            cap.stat("alias.txt".into()).await,
            Err(FsError::Io(p1_workspace::credential_refusal("alias.txt")))
        );
    }

    #[tokio::test]
    async fn absolute_credential_scope_is_refused_before_confinement() {
        let home = tempfile::tempdir().unwrap();
        let workspace_dir = tempfile::tempdir().unwrap();
        let credential = home.path().join(".codex/auth.json");
        std::fs::create_dir_all(credential.parent().unwrap()).unwrap();
        std::fs::write(&credential, "marker").unwrap();
        let cap = SearchCapability::new(
            Workspace::new(workspace_dir.path()).unwrap(),
            Some(home.path().into()),
        );
        assert!(matches!(cap.search(SearchQuery {
            pattern: "marker".into(), path: Some(credential.display().to_string()), glob: None,
            case_insensitive: false, context: 0, max_lines: 10,
        }).await, Err(FsError::Io(message)) if message.contains("read refuses credential files")));
    }

    #[tokio::test]
    async fn parent_component_out_of_keys_is_allowed() {
        let home = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(home.path().join(".config/keys")).unwrap();
        std::fs::write(home.path().join(".config/public.txt"), "public").unwrap();
        let cap = SearchCapability::new(
            Workspace::new(home.path()).unwrap(),
            Some(home.path().into()),
        );
        assert_eq!(
            cap.read(".config/keys/../public.txt".into(), 0, 32).await,
            Ok(b"public".to_vec())
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn renamed_exact_credential_is_refused_after_index_was_cached() {
        let home = tempfile::tempdir().unwrap();
        let credential = home.path().join(".codex/auth.json");
        std::fs::create_dir_all(credential.parent().unwrap()).unwrap();
        std::fs::write(&credential, "old").unwrap();
        std::fs::write(home.path().join("safe.txt"), "safe").unwrap();
        let cap = SearchCapability::new(
            Workspace::new(home.path()).unwrap(),
            Some(home.path().into()),
        );
        assert_eq!(
            cap.read("safe.txt".into(), 0, 10).await,
            Ok(b"safe".to_vec())
        );
        std::fs::rename(&credential, home.path().join("old.txt")).unwrap();
        std::fs::write(&credential, "new-marker").unwrap();
        std::fs::hard_link(&credential, home.path().join("alias.txt")).unwrap();
        assert_eq!(
            cap.read("alias.txt".into(), 0, 20).await,
            Err(FsError::Io(p1_workspace::credential_refusal("alias.txt")))
        );
        assert!(
            cap.search(SearchQuery {
                pattern: "new-marker".into(),
                path: None,
                glob: None,
                case_insensitive: false,
                context: 0,
                max_lines: 10,
            })
            .await
            .unwrap()
            .files
            .iter()
            .all(|file| file.path != "alias.txt")
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn retarget_between_index_build_and_alias_open_never_returns_credential_bytes() {
        use std::os::unix::fs::symlink;
        let home = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(home.path().join("old/keys")).unwrap();
        std::fs::create_dir_all(home.path().join("new/keys")).unwrap();
        let protected = home.path().join("new/keys/fixture.key");
        std::fs::write(&protected, b"private-marker").unwrap();
        std::fs::hard_link(&protected, home.path().join("alias")).unwrap();
        let link = home.path().join(".config");
        symlink(home.path().join("old"), &link).unwrap();
        let mut cap = SearchCapability::new(
            Workspace::new(home.path()).unwrap(),
            Some(home.path().into()),
        );
        let destination = home.path().join("new");
        cap.after_index = Some(Arc::new(move || {
            std::fs::remove_file(&link).unwrap();
            symlink(&destination, &link).unwrap();
        }));
        assert_eq!(
            cap.read("alias".into(), 0, 32).await,
            Err(FsError::Io(p1_workspace::credential_refusal("alias")))
        );
        assert!(
            cap.search(SearchQuery {
                pattern: "private-marker".into(),
                path: None,
                glob: None,
                case_insensitive: false,
                context: 0,
                max_lines: 10,
            })
            .await
            .unwrap()
            .files
            .iter()
            .all(|file| file.path != "alias")
        );
        assert!(
            !cap.list_files(".".into(), None)
                .await
                .unwrap()
                .contains(&"alias".into())
        );
    }

    #[cfg(unix)]
    #[test]
    fn opened_alias_rechecks_retargeted_directory_against_a_fresh_index() {
        use std::os::unix::fs::symlink;
        let home = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(home.path().join("old/keys")).unwrap();
        std::fs::create_dir_all(home.path().join("new/keys")).unwrap();
        let credential = home.path().join("new/keys/fixture.key");
        std::fs::write(&credential, b"private-marker").unwrap();
        std::fs::hard_link(&credential, home.path().join("alias")).unwrap();
        let config = home.path().join(".config");
        symlink(home.path().join("old"), &config).unwrap();
        let cache = super::IndexCache::new();
        let cancel = CancellationToken::new();
        let old = CredentialPolicy::new(Some(home.path()), &[]);
        super::cached_index(&cache, &old, &cancel).unwrap();
        let opened = std::fs::File::open(home.path().join("alias")).unwrap();
        std::fs::remove_file(&config).unwrap();
        symlink(home.path().join("new"), &config).unwrap();
        let current = CredentialPolicy::new(Some(home.path()), &[]);
        assert!(
            super::refuses_at_open(&cache, &current, &cancel, &opened.metadata().unwrap()).unwrap()
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn retargeted_config_keys_rebuild_the_cached_index() {
        use std::os::unix::fs::symlink;
        let home = tempfile::tempdir().unwrap();
        let old = home.path().join("old/keys");
        let new = home.path().join("new/keys");
        std::fs::create_dir_all(&old).unwrap();
        std::fs::create_dir_all(&new).unwrap();
        let credential = new.join("new.key");
        std::fs::write(&credential, "new-key-marker").unwrap();
        std::fs::hard_link(&credential, home.path().join("alias.txt")).unwrap();
        let config = home.path().join(".config");
        symlink(home.path().join("old"), &config).unwrap();
        let cap = SearchCapability::new(
            Workspace::new(home.path()).unwrap(),
            Some(home.path().into()),
        );
        assert_eq!(
            cap.read("alias.txt".into(), 0, 32).await,
            Ok(b"new-key-marker".to_vec())
        );
        std::fs::remove_file(&config).unwrap();
        symlink(home.path().join("new"), &config).unwrap();
        assert_eq!(
            cap.read("alias.txt".into(), 0, 32).await,
            Err(FsError::Io(p1_workspace::credential_refusal("alias.txt")))
        );
        assert!(
            cap.search(SearchQuery {
                pattern: "new-key-marker".into(),
                path: None,
                glob: None,
                case_insensitive: false,
                context: 0,
                max_lines: 10,
            })
            .await
            .unwrap()
            .files
            .iter()
            .all(|file| file.path != "alias.txt")
        );
    }

    #[cfg(unix)]
    #[test]
    fn retargeted_exact_parent_requires_a_fresh_open_time_policy() {
        use std::os::unix::fs::symlink;
        let home = tempfile::tempdir().unwrap();
        for name in ["old", "new"] {
            std::fs::create_dir_all(home.path().join(name)).unwrap();
        }
        let credential = home.path().join("new/auth.json");
        std::fs::write(&credential, "marker").unwrap();
        let alias = home.path().join("alias.txt");
        std::fs::hard_link(&credential, &alias).unwrap();
        let link = home.path().join(".codex");
        symlink(home.path().join("old"), &link).unwrap();
        let old_policy = CredentialPolicy::new(Some(home.path()), &[]);
        let index = ProtectedIndex::build(&old_policy, &CancellationToken::new()).unwrap();
        std::fs::remove_file(&link).unwrap();
        symlink(home.path().join("new"), &link).unwrap();
        let opened = std::fs::File::open(&alias).unwrap();
        let metadata = opened.metadata().unwrap();
        assert!(!index.refuses_current_exact(&old_policy, &metadata));
        let current = CredentialPolicy::new(Some(home.path()), &[]);
        assert!(index.refuses_current_exact(&current, &metadata));
    }

    #[cfg(unix)]
    #[test]
    fn directory_index_cache_reuses_and_rebuilds_on_change() {
        let home = tempfile::tempdir().unwrap();
        let keys = home.path().join(".config/keys");
        std::fs::create_dir_all(&keys).unwrap();
        let cap = SearchCapability::new(
            Workspace::new(home.path()).unwrap(),
            Some(home.path().into()),
        );
        let policy = CredentialPolicy::new(Some(home.path()), &cap.xdg_credentials);
        let cancel = CancellationToken::new();
        let first = cached_index(&cap.index, &policy, &cancel).unwrap();
        let reused = cached_index(&cap.index, &policy, &cancel).unwrap();
        assert!(Arc::ptr_eq(&first, &reused));
        std::fs::write(keys.join("new.key"), "fixture").unwrap();
        let rebuilt = cached_index(&cap.index, &policy, &cancel).unwrap();
        assert!(!Arc::ptr_eq(&first, &rebuilt));
        assert!(rebuilt.refuses_path(&keys.join("new.key")));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn ordinary_candidates_do_not_revalidate_the_credential_index() {
        const FILES: usize = 64;
        let home = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(home.path().join(".config/keys")).unwrap();
        for index in 0..FILES {
            std::fs::write(home.path().join(format!("file-{index}.txt")), "ordinary\n").unwrap();
        }
        let cap = SearchCapability::new(
            Workspace::new(home.path()).unwrap(),
            Some(home.path().into()),
        );
        // The first call captures the request-scoped index; the second must not rescan the
        // protected tree once per single-link candidate. Only the call's own top-level
        // validation may touch the stamps, so the count stays below the candidate count.
        assert!(cap.list_files(".".into(), None).await.is_ok());
        let before = cap
            .index
            .revalidations
            .load(std::sync::atomic::Ordering::Relaxed);
        assert!(cap.list_files(".".into(), None).await.is_ok());
        let after = cap
            .index
            .revalidations
            .load(std::sync::atomic::Ordering::Relaxed);
        assert!(
            after - before < FILES,
            "single-link candidates must reuse the validated index: {} revalidations for {FILES} files",
            after - before
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn ordinary_hard_link_remains_searchable() {
        let home = tempfile::tempdir().unwrap();
        std::fs::write(home.path().join("original.txt"), "ordinary-linked-marker").unwrap();
        std::fs::hard_link(
            home.path().join("original.txt"),
            home.path().join("notes.txt"),
        )
        .unwrap();
        let capability = SearchCapability::new(
            Workspace::new(home.path()).unwrap(),
            Some(home.path().to_path_buf()),
        );
        assert_eq!(
            capability.read("notes.txt".into(), 0, 64).await,
            Ok(b"ordinary-linked-marker".to_vec())
        );
        let found = capability
            .search(SearchQuery {
                pattern: "ordinary-linked-marker".into(),
                path: None,
                glob: None,
                case_insensitive: false,
                context: 0,
                max_lines: 10,
            })
            .await
            .unwrap();
        assert!(found.files.iter().any(|file| file.path == "notes.txt"));
    }

    /// The search capability's `read` returns only the requested window and records no
    /// observation (`p1/search` is granted no `snapshot`), so file-list mode's binary sniff
    /// cannot load a large file whole or give a search the permission an edit needs.
    #[tokio::test]
    async fn a_search_read_returns_only_the_requested_window() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "0123456789").unwrap();
        let capability = SearchCapability::new(Workspace::new(dir.path()).unwrap(), None);
        assert_eq!(
            capability.read("a.txt".into(), 2, 3).await,
            Ok(b"234".to_vec())
        );
        assert_eq!(
            capability.read("a.txt".into(), 8, 100).await,
            Ok(b"89".to_vec())
        );
        assert_eq!(
            capability.read("../outside".into(), 0, 1).await,
            Err(FsError::OutsideWorkspace)
        );
    }
}
