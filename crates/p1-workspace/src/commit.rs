//! Mutation side of the `workspace-mutation` capability: [`Workspace::commit`], the
//! owned mutation behind a module's `workspace-mutation.begin`, and the unobserved
//! read a search module uses.
//!
//! Everything that makes a mutation safe is host-enforced, never the module's: a
//! module's guest preopens stay empty, so nothing here hands a module a path or a
//! file descriptor. The host keeps each agent's observations ([`ObservedFiles`]),
//! refuses to change an existing file this agent has not observed in its current
//! state (read-before-mutate), and serializes writers on the shared [`WriteGate`].
//! A module reads and computes outside the gate; the host rechecks the agent's
//! observation of every target under the gate, at the moment it writes.
//!
//! A component's read is not an observation, so the gate also rechecks the target's
//! contents against the change's read identity: a [`ReadRecord`] names the whole file
//! as the reading tool saw it, and an [`OwnedMutation`] carries that identity into
//! every change of the file it read ([`Recheck::check`]). That is what refuses a
//! second writer for patch-authorized mode, which has no observation to check, and it
//! is why the read record is one tool's own state rather than the agent's.
//!
//! Atomicity is per file. Each new content is staged as a synced sibling temporary
//! file before anything is replaced, so a failure while checking or staging leaves
//! every target untouched and removes the temporaries. The replacements themselves
//! are separate renames: a crash (or an I/O error) between two files' replacements
//! can leave some of them applied. There is no multi-file crash atomicity. Missing
//! parent directories are created while staging (the temporary lives beside its
//! target), so they can remain after a refusal; no file is left behind.
//!
//! Every file operation under the gate is directory-relative: `plan` opens each
//! target's parent directory once, walking down from the root without following any
//! symlink, and keeps that directory handle in the planned change. The gate re-proves
//! that the names the walk used still lead to that handle, and the temporary file, the
//! rename and the unlink all happen relative to it, never following a symlink at the
//! leaf and never resolving the target's path again. A parent directory swapped for a
//! symlink between validation and replacement therefore cannot make a mutation land
//! outside the root, and one retargeted in between cannot move it to a directory the
//! plan never checked. A create-only replacement and a rename destination are refused
//! atomically with `RENAME_NOREPLACE`, so an ungated writer that fills the path after
//! the staged `exists` check is not silently overwritten; a filesystem without
//! `renameat2` refuses those changes rather than degrade to an overwrite, and the root
//! itself is opened without following a symlink.
//!
//! [`WriteGate`]: crate::WriteGate

use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::future::Future;
use std::io::{Read, Write};
use std::os::fd::OwnedFd;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};

use rustix::fs::{AtFlags, CWD, FileType, Mode, OFlags, RenameFlags};
use rustix::io::Errno;

use crate::gate::Held;
use crate::observe::{Observation, ObservedFiles, hash_of};
use crate::read::{Snapshot, SnapshotMetadata};
use crate::reads::ReadRecord;
use crate::text::temp_name;
use crate::{CredentialPolicy, ProtectedIndex, Workspace, WorkspaceError, xdg_credentials};
use p1_contracts::CancellationToken;

/// Maximum file size materialized by workspace mutation and snapshot reads.
pub const MAX_FILE_BYTES: u64 = 32 * 1024 * 1024;

/// Why a mutation was refused or failed. One variant per case of the WIT `fs-error`
/// that a mutation can produce, so the host maps it without parsing a message.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum MutationError {
    /// The path resolves outside the workspace root, or stopped resolving inside it
    /// (a directory swapped for a symlink) by the time the gate was held.
    #[error("path escapes workspace: {requested}")]
    OutsideWorkspace { requested: String },
    /// The file to remove or rename, or a parent it needs, does not exist.
    #[error("no such path in the workspace: {requested}")]
    NotFound { requested: String },
    /// Something other than a regular file is where a file is needed.
    #[error("not a file: {requested}")]
    WrongKind { requested: String },
    /// Something is already at the path a `create` or a `rename` would fill.
    #[error("already exists: {requested}")]
    AlreadyExists { requested: String },
    /// Any other refusal or failure, with the model-facing message. The
    /// read-before-mutate refusals are here with the exact text the native edit and
    /// write tools print, so a module's output stays identical to theirs.
    #[error("{0}")]
    Io(String),
}

/// What a mutation must prove about an existing target before it may change it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MutationPolicy {
    /// Edit and write: an existing target must be observed by this agent and
    /// unchanged since (read-before-mutate). A new file needs no observation.
    Observed,
    /// Patch: no prior observation is required, because a patch's context lines are
    /// its staleness check. Exactly the exemption `p1-tool-patch` has today.
    PatchAuthorized,
}

/// One per-file operation of a [`Workspace::commit`].
#[derive(Clone)]
pub struct Change {
    op: Op,
    computed_from: Option<u64>,
}

#[derive(Clone)]
enum Op {
    Write { path: String, contents: Vec<u8> },
    Create { path: String, contents: Vec<u8> },
    Remove { path: String },
    Rename { from: String, to: String },
}

impl Change {
    /// Replace (or create) the file at `path` with `contents`, creating missing
    /// parent directories.
    pub fn write(path: impl Into<String>, contents: impl Into<Vec<u8>>) -> Self {
        Self::new(Op::Write {
            path: path.into(),
            contents: contents.into(),
        })
    }

    /// As [`Change::write`], but refused with `AlreadyExists` when anything is at `path`.
    pub fn create(path: impl Into<String>, contents: impl Into<Vec<u8>>) -> Self {
        Self::new(Op::Create {
            path: path.into(),
            contents: contents.into(),
        })
    }

    /// Remove the file at `path`.
    pub fn remove(path: impl Into<String>) -> Self {
        Self::new(Op::Remove { path: path.into() })
    }

    /// Move the file at `from` to `to`; refused with `AlreadyExists` when anything is
    /// at `to`. Missing parents of `to` are created.
    pub fn rename(from: impl Into<String>, to: impl Into<String>) -> Self {
        Self::new(Op::Rename {
            from: from.into(),
            to: to.into(),
        })
    }

    /// Name the snapshot this change was computed from (for a rename, the source's).
    /// The commit then also refuses unless the file's current bytes are exactly the
    /// snapshot's, whatever the policy.
    pub fn computed_from(mut self, snapshot: &SnapshotMetadata) -> Self {
        self.computed_from = Some(snapshot.content_hash);
        self
    }

    /// The same identity, when the caller holds the whole file's content digest instead
    /// of a [`SnapshotMetadata`] of it (the [`ReadRecord`] an [`OwnedMutation`] carries).
    fn computed_from_hash(mut self, hash: u64) -> Self {
        self.computed_from = Some(hash);
        self
    }

    /// The path the change was computed from: its target, or a rename's source.
    fn source_path(&self) -> &str {
        match &self.op {
            Op::Write { path, .. } | Op::Create { path, .. } | Op::Remove { path } => path,
            Op::Rename { from, .. } => from,
        }
    }

    fn new(op: Op) -> Self {
        Self {
            op,
            computed_from: None,
        }
    }
}

impl std::fmt::Debug for Change {
    /// Deliberately not the contents: a change can carry a whole file, and a `{:?}`
    /// that dumped it would put a file into a log.
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut debug = formatter.debug_struct("Change");
        match &self.op {
            Op::Write { path, contents } => {
                debug.field("write", path).field("bytes", &contents.len())
            }
            Op::Create { path, contents } => {
                debug.field("create", path).field("bytes", &contents.len())
            }
            Op::Remove { path } => debug.field("remove", path),
            Op::Rename { from, to } => debug.field("rename", from).field("to", to),
        };
        debug.field("computed_from", &self.computed_from).finish()
    }
}

/// The write gate held for one module export call, with the agent, the read identity it
/// mutates for and the policy: the native side of the WIT `mutation` resource.
///
/// `'static + Send`, so the host can keep it in its resource table across import
/// calls. Each method is a single-target [`Workspace::commit`] under the gate this
/// value already holds, with the same checks and policy, and with the [`ReadRecord`]
/// entry of the change's target, when this tool read it, as the change's read identity.
/// Dropping it releases the gate. The methods do blocking file I/O, a few operations
/// each, like the native tools.
pub struct OwnedMutation {
    workspace: Workspace,
    observed: ObservedFiles,
    reads: ReadRecord,
    policy: MutationPolicy,
    _held: Held,
}

impl OwnedMutation {
    /// Replace the file at `path` with `contents`, creating missing parent directories.
    pub fn write(&self, path: &str, contents: impl Into<Vec<u8>>) -> Result<(), MutationError> {
        self.single(Change::write(path, contents))
    }

    /// As [`OwnedMutation::write`], but `AlreadyExists` when anything is at `path`.
    pub fn create(&self, path: &str, contents: impl Into<Vec<u8>>) -> Result<(), MutationError> {
        self.single(Change::create(path, contents))
    }

    /// Remove the file at `path`.
    pub fn remove(&self, path: &str) -> Result<(), MutationError> {
        self.single(Change::remove(path))
    }

    /// Move the file at `old_path` to `new_path`; `AlreadyExists` when anything is at
    /// `new_path`.
    pub fn rename(&self, old_path: &str, new_path: &str) -> Result<(), MutationError> {
        self.single(Change::rename(old_path, new_path))
    }

    /// Apply `changes` as one batch under the gate this mutation already holds, staging
    /// every replacement before any rename, exactly as [`Workspace::commit`] does. The
    /// caller is a native tool that plans its changes while the gate is held (the native
    /// patch, whose hunks are located by reading the current files), so no second
    /// acquisition of the gate is possible here; `cancel` is rechecked after staging and
    /// before the first rename, so a call cancelled while staging mutates nothing.
    pub fn apply_all_cancellable(
        &self,
        changes: &[Change],
        cancel: &CancellationToken,
    ) -> Result<(), MutationError> {
        let plan = self.workspace.plan(changes)?;
        self.workspace
            .apply_with_cancel(&plan, &self.observed, self.policy, Some(cancel))
    }

    fn single(&self, change: Change) -> Result<(), MutationError> {
        let (change, read) = self.read_identity(change)?;
        let plan = self.workspace.plan(std::slice::from_ref(&change))?;
        self.plans_the_file_read(&plan, read.as_deref(), &change)?;
        self.workspace.apply(&plan, &self.observed, self.policy)?;
        if matches!(&change.op, Op::Remove { .. } | Op::Rename { .. }) {
            let source = match &plan[0] {
                Planned::Remove { target, .. } => &target.canonical,
                Planned::Rename { from, .. } => &from.canonical,
                Planned::Write { target, .. } => &target.canonical,
            };
            self.reads.forget(source);
        }
        Ok(())
    }

    /// Refuse a plan whose source resolved to another file than the one the read
    /// identity was checked against: `plan` resolves the path again, and a symlink
    /// retargeted in between would otherwise carry the read file's digest to a file
    /// with the same bytes that this tool never read.
    fn plans_the_file_read(
        &self,
        plan: &[Planned<'_>],
        read: Option<&Path>,
        change: &Change,
    ) -> Result<(), MutationError> {
        let (Some(read), Some(planned)) = (read, plan.first()) else {
            return Ok(());
        };
        let source = match planned {
            Planned::Write { target, .. } | Planned::Remove { target, .. } => target,
            Planned::Rename { from, .. } => from,
        };
        if source.canonical == read {
            return Ok(());
        }
        Err(self.changed_since_read(change.source_path()))
    }

    fn changed_since_read(&self, requested: &str) -> MutationError {
        MutationError::Io(format!(
            "{} changed on disk since you last read it; read it again.",
            self.workspace.display(&self.workspace.spelling(requested))
        ))
    }

    /// The change with the read identity of what this tool read of its target, if
    /// anything: the gated recheck (the caller already holds the gate) then refuses any
    /// other bytes there, whatever the policy. A target this tool did not read has no
    /// identity and is checked by the policy alone. The path is resolved exactly as the
    /// change's own validation resolves it, so both name one file.
    ///
    /// A path this tool read that now resolves to another file (a symlink retargeted
    /// since the read) is refused as stale: the change was computed from a file that is
    /// not the one it would replace, and treating the new target as unread would skip
    /// the recheck entirely. With the identity comes the file it was checked against,
    /// which the plan must resolve to as well ([`Self::plans_the_file_read`]).
    fn read_identity(&self, change: Change) -> Result<(Change, Option<PathBuf>), MutationError> {
        let requested = change.source_path();
        let Ok(path) = self.workspace.resolve(requested) else {
            return Ok((change, None));
        };
        let spelling = self.workspace.spelling(requested);
        let read_as = self.reads.read_as(&spelling);
        if read_as
            .as_ref()
            .is_some_and(|read| *read != crate::observe::key(&path))
        {
            return Err(self.changed_since_read(requested));
        }
        Ok(match self.reads.recorded(&path) {
            Some(hash) => (change.computed_from_hash(hash), Some(path)),
            None if read_as.is_some() => (change, Some(path)),
            None => (change, None),
        })
    }
}

impl std::fmt::Debug for OwnedMutation {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("OwnedMutation")
            .field("root", &self.workspace.root())
            .field("policy", &self.policy)
            .finish_non_exhaustive()
    }
}

impl Workspace {
    /// Apply `changes` as one mutation of the agent that owns `observed`.
    ///
    /// Every target and rename source is validated beneath the workspace before the
    /// gate is taken. Under the gate each existing target is rechecked: against the
    /// snapshot a change names ([`Change::computed_from`]) and, for
    /// [`MutationPolicy::Observed`], against this agent's observation. Then every new
    /// content is staged as a synced sibling temporary file, and only then are the
    /// files replaced, one atomic rename each (permission bits kept, missing parents
    /// created). Each written file's new bytes become this agent's observation, as
    /// after the native edit, write and patch tools; a removal records nothing. The
    /// gate is released before this returns.
    ///
    /// A path may appear in only one change of a commit. Two spellings of one file
    /// through a directory symlink are one path here, refused as the same change. See
    /// the module docs for what is and is not atomic.
    pub fn commit(
        &self,
        changes: &[Change],
        observed: &ObservedFiles,
        policy: MutationPolicy,
    ) -> Result<(), MutationError> {
        self.commit_with_cancel(changes, observed, policy, None)
    }

    /// Native calls check cancellation after acquiring the gate and before any
    /// replacement; a queued cancelled call must not write after the gate opens.
    pub fn commit_cancellable(
        &self,
        changes: &[Change],
        observed: &ObservedFiles,
        policy: MutationPolicy,
        cancel: &CancellationToken,
    ) -> Result<(), MutationError> {
        self.commit_with_cancel(changes, observed, policy, Some(cancel))
    }

    fn commit_with_cancel(
        &self,
        changes: &[Change],
        observed: &ObservedFiles,
        policy: MutationPolicy,
        cancel: Option<&CancellationToken>,
    ) -> Result<(), MutationError> {
        let plan = self.plan(changes)?;
        let mutation = self.begin_mutation();
        if cancel.is_some_and(CancellationToken::is_cancelled) {
            return Err(MutationError::Io("cancelled".into()));
        }
        let result = self.apply_with_cancel(&plan, observed, policy, cancel);
        drop(mutation);
        result
    }

    /// Wait for the write gate without blocking the calling thread, for the host
    /// function behind `workspace-mutation.begin`. The future and the
    /// [`OwnedMutation`] it resolves to are `'static + Send`; the mutation shares this
    /// workspace's gate with the native tools' [`Workspace::begin_mutation`], and it
    /// carries `reads`, the read record the changing tool filled from the read side, so
    /// every change it makes is rechecked against what that tool read.
    pub fn begin_owned(
        &self,
        observed: &ObservedFiles,
        reads: &ReadRecord,
        policy: MutationPolicy,
    ) -> impl Future<Output = OwnedMutation> + Send + 'static {
        let workspace = self.clone();
        let observed = observed.clone();
        let reads = reads.clone();
        async move {
            let held = workspace.writes.acquire_owned().await;
            OwnedMutation {
                workspace,
                observed,
                reads,
                policy,
                _held: held,
            }
        }
    }

    /// [`Workspace::read`] without recording an observation: the same [`Snapshot`],
    /// and this agent's registry is left as it was. A module granted only reading
    /// (search) reads through this, so it can never obtain edit permission by reading.
    pub fn read_unobserved(&self, requested: &str) -> Result<Snapshot, WorkspaceError> {
        // A throwaway registry receives the observation and is dropped with it, which
        // reuses S1's read exactly instead of a second copy of its logic.
        self.read(requested, &ObservedFiles::new())
    }

    /// Validate every path of `changes` beneath the workspace and open every target's
    /// parent directory once, before the gate. The handle each walk returns travels in
    /// the planned change, so staging and the replacement work in the directory the plan
    /// resolved instead of resolving the target's path again (issue #401).
    fn plan<'c>(&self, changes: &'c [Change]) -> Result<Vec<Planned<'c>>, MutationError> {
        let credentials =
            CredentialPolicy::new(self.credential_home.as_deref(), &xdg_credentials());
        for change in changes {
            let requested = match &change.op {
                Op::Write { path, .. } | Op::Create { path, .. } | Op::Remove { path } => {
                    vec![path.as_str()]
                }
                Op::Rename { from, to } => vec![from.as_str(), to.as_str()],
            };
            for path in requested {
                credentials.refuse(self, path).map_err(MutationError::Io)?;
            }
        }
        let root = open_root(&self.root)?;
        let mut plan = Vec::with_capacity(changes.len());
        let mut touched: Vec<PathBuf> = Vec::new();
        for change in changes {
            let planned = match &change.op {
                Op::Write { path, contents } | Op::Create { path, contents } => {
                    let mut target = self.target(path)?;
                    let parent = self.plan_parent(&root, &target)?;
                    target.canonical = self.parent_key(&parent, &target);
                    Planned::Write {
                        target,
                        contents,
                        create_only: matches!(change.op, Op::Create { .. }),
                        computed_from: change.computed_from,
                        parent,
                    }
                }
                Op::Remove { path } => {
                    let mut target = self.target(path)?;
                    let parent = self.plan_parent(&root, &target)?;
                    target.canonical = self.parent_key(&parent, &target);
                    Planned::Remove {
                        target,
                        computed_from: change.computed_from,
                        parent,
                    }
                }
                Op::Rename { from, to } => {
                    let mut from = self.target(from)?;
                    let mut to = self.target(to)?;
                    let from_parent = self.plan_parent(&root, &from)?;
                    let to_parent = self.plan_parent(&root, &to)?;
                    from.canonical = self.parent_key(&from_parent, &from);
                    to.canonical = self.parent_key(&to_parent, &to);
                    Planned::Rename {
                        from,
                        to,
                        from_parent,
                        to_parent,
                        computed_from: change.computed_from,
                    }
                }
            };
            // Every check runs before the first replacement, so two changes to one
            // path would each be checked against bytes the other is about to replace.
            // The key is `Target::canonical`, not the resolved spelling: a directory
            // symlink inside the workspace makes `src/new.txt` and `link/new.txt` one
            // file that a not-yet-existing leaf would otherwise spell twice.
            for target in planned.targets() {
                let key = target.canonical.clone();
                if touched.contains(&key) {
                    return Err(MutationError::Io(format!(
                        "{} is changed more than once in one commit.",
                        target.display
                    )));
                }
                touched.push(key);
            }
            plan.push(planned);
        }
        Ok(plan)
    }

    fn parent_key(&self, parent: &Parent, target: &Target) -> PathBuf {
        let mut key = self.root.clone();
        for name in parent.names.iter().chain(&parent.missing) {
            key.push(name);
        }
        key.push(leaf_of(target));
        key
    }

    fn target(&self, requested: &str) -> Result<Target, MutationError> {
        let path = self.resolve(requested).map_err(|error| match error {
            WorkspaceError::OutsideWorkspace { .. } => MutationError::OutsideWorkspace {
                requested: requested.to_string(),
            },
            WorkspaceError::NotFound { .. } => MutationError::NotFound {
                requested: requested.to_string(),
            },
            WorkspaceError::NotADirectory(_) => MutationError::WrongKind {
                requested: requested.to_string(),
            },
            WorkspaceError::Io { .. } => MutationError::Io(error.to_string()),
        })?;
        // The root itself has no parent inside the workspace to act in, and is a
        // directory, not a file.
        if path.file_name().is_none() || path == self.root {
            return Err(MutationError::WrongKind {
                requested: requested.to_string(),
            });
        }
        let display = self.display(&path);
        // Resolved once, here, for everything downstream: the one-change-per-path key,
        // the directory `plan_parent` opens, and the name a record of what this change
        // writes is keyed by are one and the same file.
        let canonical = self.canonical_path(&path, requested, &display)?;
        Ok(Target {
            requested: requested.to_string(),
            path,
            canonical,
            display,
        })
    }

    /// Check, stage and replace, with the gate already held by the caller.
    fn apply(
        &self,
        plan: &[Planned<'_>],
        observed: &ObservedFiles,
        policy: MutationPolicy,
    ) -> Result<(), MutationError> {
        self.apply_with_cancel(plan, observed, policy, None)
    }

    fn apply_with_cancel(
        &self,
        plan: &[Planned<'_>],
        observed: &ObservedFiles,
        policy: MutationPolicy,
        cancel: Option<&CancellationToken>,
    ) -> Result<(), MutationError> {
        self.apply_with_before_apply(plan, observed, policy, cancel, || {})
    }

    /// The hook is private and used only to inject an ungated replacement in a
    /// regression. Production always supplies the empty closure above.
    fn apply_with_before_apply(
        &self,
        plan: &[Planned<'_>],
        observed: &ObservedFiles,
        policy: MutationPolicy,
        cancel: Option<&CancellationToken>,
        before_apply: impl FnOnce(),
    ) -> Result<(), MutationError> {
        self.apply_with_hooks(plan, observed, policy, cancel, || {}, before_apply)
    }

    /// `before_stage` runs under the gate after the plan's directories are re-proved and
    /// before anything is staged; `before_apply` runs after staging and before the first
    /// mutation. Both hooks are private and used only to inject an ungated change in a
    /// regression; production always supplies the empty closures above.
    fn apply_with_hooks(
        &self,
        plan: &[Planned<'_>],
        observed: &ObservedFiles,
        policy: MutationPolicy,
        cancel: Option<&CancellationToken>,
        before_stage: impl FnOnce(),
        before_apply: impl FnOnce(),
    ) -> Result<(), MutationError> {
        let root = open_root(&self.root)?;
        let credentials =
            CredentialPolicy::new(self.credential_home.as_deref(), &xdg_credentials());
        let mut index = ProtectedIndex::build(&credentials, &CancellationToken::new())
            .map_err(|_| MutationError::Io("credential policy check cancelled".into()))?;
        let recheck = Recheck { observed, policy };

        // Under the gate every handle the plan opened must still be the directory the
        // workspace path names: the names the plan's walk used are re-walked from this
        // fresh root without following a symlink and must land on the handle it kept. A
        // parent swapped for a symlink since the plan, or one now leading to a different
        // directory, refuses here — this proves, it does not resolve: nothing below
        // touches the target's path.
        for planned in plan {
            planned.still_planned(&root)?;
        }

        before_stage();

        // Stage everything first: a refusal or failure here drops the staged files,
        // which removes their temporaries, and no target has been touched.
        let mut staged = Vec::with_capacity(plan.len());
        for planned in plan {
            // A hard link added to a protected directory after the index was built must be
            // refused through its ordinary alias too, so the captured stamps are revalidated
            // before the leaf is checked.
            refresh_credential_index(&mut index, &credentials)?;
            staged.push(self.stage(planned, &recheck, &credentials, &index)?);
        }

        before_apply();
        if cancel.is_some_and(CancellationToken::is_cancelled) {
            return Err(MutationError::Io("cancelled".into()));
        }
        for step in &mut staged {
            match step {
                Staged::Replace {
                    file,
                    target,
                    contents,
                    create_only,
                    inspected,
                    inspected_bytes,
                } => {
                    if let Some(identity) = inspected {
                        verify_leaf(&file.dir, &file.leaf, *identity, target)?;
                    }
                    if let Some(inspected_bytes) = inspected_bytes {
                        verify_unchanged_contents(&file.dir, &file.leaf, inspected_bytes, target)?;
                    }
                    // A create-only target is refused atomically by the rename, not
                    // only by the staged `exists` check: an ungated writer that fills
                    // it in between cannot be silently overwritten.
                    file.replace(*create_only).map_err(|error| match error {
                        Errno::EXIST => MutationError::AlreadyExists {
                            requested: target.requested.clone(),
                        },
                        other => MutationError::Io(format!(
                            "failed to write {}: {other}",
                            target.display
                        )),
                    })?;
                    // Keyed by the plan's destination, never by the spelling: a parent
                    // symlink retargeted since the plan would otherwise record a file
                    // the write never touched, leaving the real output unobserved.
                    observed.record(&target.canonical, contents);
                }
                Staged::Remove {
                    dir,
                    leaf,
                    target,
                    inspected,
                    inspected_bytes,
                } => {
                    verify_leaf(dir, leaf, *inspected, target)?;
                    verify_unchanged_contents(dir, leaf, inspected_bytes, target)?;
                    rustix::fs::unlinkat(&*dir, leaf.as_os_str(), AtFlags::empty()).map_err(
                        |error| {
                            MutationError::Io(format!(
                                "failed to delete {}: {error}",
                                target.display
                            ))
                        },
                    )?;
                    sync_directory(dir);
                    observed.forget(&target.canonical);
                }
                Staged::Rename {
                    from_dir,
                    from_leaf,
                    from,
                    to_dir,
                    to_leaf,
                    to,
                    bytes,
                    inspected,
                } => {
                    verify_leaf(from_dir, from_leaf, *inspected, from)?;
                    // `bytes` are the source's inspected contents; reusing them refuses a
                    // same-inode rewrite of the source between staging and the rename.
                    verify_unchanged_contents(from_dir, from_leaf, bytes, from)?;
                    rename_noreplace(
                        &*from_dir,
                        from_leaf.as_os_str(),
                        &*to_dir,
                        to_leaf.as_os_str(),
                        true,
                    )
                    .map_err(|error| match error {
                        Errno::EXIST => MutationError::AlreadyExists {
                            requested: to.requested.clone(),
                        },
                        other => MutationError::Io(format!(
                            "failed to rename {} to {}: {other}",
                            from.display, to.display
                        )),
                    })?;
                    sync_directory(to_dir);
                    sync_directory(from_dir);
                    // As patch's move: the destination now holds bytes this agent saw —
                    // keyed by the file the plan's handle put them in, so a parent
                    // symlink retargeted since the plan cannot name another file.
                    observed.forget(&from.canonical);
                    observed.record(&to.canonical, bytes);
                }
            }
        }
        Ok(())
    }

    /// One change checked and staged, entirely relative to the parent directory handles
    /// `plan` opened: `stage` resolves no path, so a parent retargeted after the plan
    /// cannot move the write to a directory the plan never checked.
    fn stage<'p>(
        &self,
        planned: &'p Planned<'_>,
        recheck: &Recheck<'_>,
        credentials: &CredentialPolicy,
        index: &ProtectedIndex,
    ) -> Result<Staged<'p>, MutationError> {
        match planned {
            Planned::Write {
                target,
                contents,
                create_only,
                computed_from,
                parent,
            } => {
                let dir = open_parent(parent, target, true)?;
                let leaf = leaf_of(target);
                let existing = inspect_write(&dir, leaf, target)?;
                refuse_credential(credentials, index, target, existing.as_ref())?;
                if *create_only && exists(&dir, leaf, target)? {
                    return Err(MutationError::AlreadyExists {
                        requested: target.requested.clone(),
                    });
                }
                let mode = match &existing {
                    Some(existing) => {
                        recheck.check(target, &existing.bytes, *computed_from)?;
                        Some(existing.mode)
                    }
                    None => {
                        recheck.check_absent(target, *computed_from)?;
                        None
                    }
                };
                let file = StagedFile::stage(dir, leaf, contents, mode).map_err(|error| {
                    MutationError::Io(format!("failed to write {}: {error}", target.display))
                })?;
                Ok(Staged::Replace {
                    file,
                    target,
                    contents,
                    create_only: *create_only,
                    inspected: existing
                        .as_ref()
                        .map(|file| LeafIdentity::of(&file.metadata)),
                    inspected_bytes: existing.as_ref().map(|file| file.bytes.clone()),
                })
            }
            Planned::Remove {
                target,
                computed_from,
                parent,
            } => {
                let dir = open_parent(parent, target, false)?;
                let leaf = leaf_of(target);
                let existing = inspect(&dir, leaf, target)?;
                refuse_credential(credentials, index, target, existing.as_ref())?;
                let Some(existing) = existing else {
                    return Err(MutationError::NotFound {
                        requested: target.requested.clone(),
                    });
                };
                recheck.check(target, &existing.bytes, *computed_from)?;
                Ok(Staged::Remove {
                    dir,
                    leaf: leaf.to_os_string(),
                    target,
                    inspected: LeafIdentity::of(&existing.metadata),
                    inspected_bytes: existing.bytes,
                })
            }
            Planned::Rename {
                from,
                to,
                computed_from,
                from_parent,
                to_parent,
            } => {
                let from_dir = open_parent(from_parent, from, false)?;
                let from_leaf = leaf_of(from);
                let existing = inspect(&from_dir, from_leaf, from)?;
                refuse_credential(credentials, index, from, existing.as_ref())?;
                let Some(existing) = existing else {
                    return Err(MutationError::NotFound {
                        requested: from.requested.clone(),
                    });
                };
                recheck.check(from, &existing.bytes, *computed_from)?;
                let to_dir = open_parent(to_parent, to, true)?;
                let to_leaf = leaf_of(to);
                // An occupied destination refuses as AlreadyExists even when it is
                // a dangling symlink; inspect only regular-file aliases for policy.
                let destination =
                    match rustix::fs::statat(&to_dir, to_leaf, AtFlags::SYMLINK_NOFOLLOW) {
                        Ok(stat)
                            if FileType::from_raw_mode(stat.st_mode) == FileType::RegularFile =>
                        {
                            inspect(&to_dir, to_leaf, to)?
                        }
                        _ => None,
                    };
                refuse_credential(credentials, index, to, destination.as_ref())?;
                if exists(&to_dir, to_leaf, to)? {
                    return Err(MutationError::AlreadyExists {
                        requested: to.requested.clone(),
                    });
                }
                Ok(Staged::Rename {
                    from_dir,
                    from_leaf: from_leaf.to_os_string(),
                    from,
                    to_dir,
                    to_leaf: to_leaf.to_os_string(),
                    to,
                    bytes: existing.bytes,
                    inspected: LeafIdentity::of(&existing.metadata),
                })
            }
        }
    }

    /// The canonical path two spellings of one file share: its deepest existing
    /// ancestor canonicalized, with the missing parent components and the leaf
    /// re-joined. `Target` keeps this as [`Target::canonical`] — `plan` refuses a
    /// commit that names it twice, so a directory symlink inside the workspace
    /// (`link -> src`) cannot make `src/new.txt` and `link/new.txt` slip through the
    /// one-change-per-path check as two files, and it is the name an observation of
    /// the change is keyed by.
    fn canonical_path(
        &self,
        path: &Path,
        requested: &str,
        display: &str,
    ) -> Result<PathBuf, MutationError> {
        let (mut key, missing) = self.ancestor_and_missing(path, requested, display)?;
        for name in missing.into_iter().rev() {
            key.push(name);
        }
        key.push(path.file_name().unwrap_or_default());
        Ok(key)
    }

    /// The deepest existing ancestor of `path`'s parent, canonicalized and checked
    /// beneath the root, with the names of the missing components below it (leaf-most
    /// first). `Target::canonical` rejoins them to name the file; `plan_parent` walks
    /// the ancestor once and records the missing names for [`open_parent`] to create.
    ///
    /// The canonical form has no symlink in it (a symlink that stays inside is fine,
    /// as for [`Workspace::resolve`]), so `plan_parent`'s `O_NOFOLLOW` walk succeeds
    /// unless a component was swapped since, and then the walk refuses instead of
    /// following.
    fn ancestor_and_missing<'p>(
        &self,
        path: &'p Path,
        requested: &str,
        display: &str,
    ) -> Result<(PathBuf, Vec<&'p OsStr>), MutationError> {
        let outside = || MutationError::OutsideWorkspace {
            requested: requested.to_string(),
        };
        let mut existing = path.parent().ok_or_else(outside)?;
        let mut missing: Vec<&OsStr> = Vec::new();
        loop {
            match std::fs::symlink_metadata(existing) {
                Ok(_) => break,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    missing.push(existing.file_name().ok_or_else(outside)?);
                    existing = existing.parent().ok_or_else(outside)?;
                }
                Err(error) => {
                    return Err(MutationError::Io(format!(
                        "{display} could not be resolved: {error}"
                    )));
                }
            }
        }
        let canonical = existing.canonicalize().map_err(|error| {
            MutationError::Io(format!("{display} could not be resolved: {error}"))
        })?;
        if !canonical.starts_with(&self.root) {
            return Err(outside());
        }
        Ok((canonical, missing))
    }

    /// Open the deepest existing part of `target`'s parent directory relative to the
    /// open `root`, without following a symlink on the way, and record how to reach it:
    /// the names the walk used, for [`Planned::still_planned`] to re-walk under the gate,
    /// and the names still missing below it, for [`open_parent`] to create at staging.
    ///
    /// This runs once, in [`Workspace::plan`], and opens nothing that does not exist
    /// yet: a parent directory that is not there is created only while staging, where it
    /// has always been created.
    fn plan_parent(&self, root: &OwnedFd, target: &Target) -> Result<Parent, MutationError> {
        let outside = || MutationError::OutsideWorkspace {
            requested: target.requested.clone(),
        };
        let (canonical, missing) =
            self.ancestor_and_missing(&target.path, &target.requested, &target.display)?;
        let relative = canonical.strip_prefix(&self.root).map_err(|_| outside())?;

        let mut dir = root.try_clone().map_err(|error| {
            MutationError::Io(format!("the workspace could not be opened: {error}"))
        })?;
        let mut names = Vec::new();
        for component in relative.components() {
            let Component::Normal(name) = component else {
                return Err(outside());
            };
            dir = open_directory(&dir, name, target)?;
            names.push(name.to_os_string());
        }
        Ok(Parent {
            dir,
            names,
            missing: missing
                .into_iter()
                .rev()
                .map(|name| name.to_os_string())
                .collect(),
        })
    }
}

/// A validated path: resolved beneath the root, with what the caller asked for, what the
/// model is shown, and the file this change actually writes.
#[derive(Debug)]
struct Target {
    requested: String,
    /// What [`Workspace::resolve`] returned: the canonical file for a leaf that exists,
    /// the lexical spelling for one that does not — which, under a directory symlink
    /// inside the workspace, still contains the link. Read identities compare against
    /// this, so it stays the resolution and nothing else.
    path: PathBuf,
    /// The destination this change writes: the deepest existing ancestor canonicalized
    /// (no symlink in it) with the missing components and the leaf re-joined — the file
    /// the directory handle `plan` opened names. Observations are keyed by this, never
    /// by `path`: re-resolving a spelling after a parent symlink was retargeted would
    /// record a file the write never touched, leaving the real output unobserved.
    canonical: PathBuf,
    display: String,
}

/// What [`Workspace::plan_parent`] opened for one target's parent directory: the handle
/// of its deepest existing part, the names it walked from the root to reach it, and the
/// names still missing below it. Staging works inside this handle, creating the missing
/// names when the change may, and never resolves the target's path again.
struct Parent {
    dir: OwnedFd,
    names: Vec<OsString>,
    missing: Vec<OsString>,
}

/// One validated change, with the parent directory handles `plan` opened for it.
enum Planned<'c> {
    Write {
        target: Target,
        contents: &'c [u8],
        create_only: bool,
        computed_from: Option<u64>,
        parent: Parent,
    },
    Remove {
        target: Target,
        computed_from: Option<u64>,
        parent: Parent,
    },
    Rename {
        from: Target,
        to: Target,
        from_parent: Parent,
        to_parent: Parent,
        computed_from: Option<u64>,
    },
}

impl Planned<'_> {
    fn targets(&self) -> Vec<&Target> {
        match self {
            Planned::Write { target, .. } | Planned::Remove { target, .. } => vec![target],
            Planned::Rename { from, to, .. } => vec![from, to],
        }
    }

    /// Under the gate: prove that every directory handle this change carries is still
    /// the directory its workspace path opens, before anything is staged. A parent
    /// swapped for a symlink since the plan, or one that now leads elsewhere, refuses
    /// ([`MutationError::OutsideWorkspace`]) instead of letting the write follow it.
    fn still_planned(&self, root: &OwnedFd) -> Result<(), MutationError> {
        match self {
            Planned::Write { target, parent, .. } | Planned::Remove { target, parent, .. } => {
                parent_still_planned(root, parent, target)
            }
            Planned::Rename {
                from,
                from_parent,
                to,
                to_parent,
                ..
            } => {
                parent_still_planned(root, from_parent, from)?;
                parent_still_planned(root, to_parent, to)
            }
        }
    }
}

/// One checked change, ready to apply.
enum Staged<'p> {
    Replace {
        file: StagedFile,
        target: &'p Target,
        contents: &'p [u8],
        create_only: bool,
        inspected: Option<LeafIdentity>,
        /// The bytes `stage` read from the target, when it existed; re-read immediately
        /// before apply so a same-inode rewrite during staging is refused, not lost.
        inspected_bytes: Option<Vec<u8>>,
    },
    Remove {
        dir: OwnedFd,
        leaf: OsString,
        target: &'p Target,
        inspected: LeafIdentity,
        inspected_bytes: Vec<u8>,
    },
    Rename {
        from_dir: OwnedFd,
        from_leaf: OsString,
        from: &'p Target,
        to_dir: OwnedFd,
        to_leaf: OsString,
        to: &'p Target,
        bytes: Vec<u8>,
        inspected: LeafIdentity,
    },
}

/// The rechecks every existing target gets under the gate.
struct Recheck<'o> {
    observed: &'o ObservedFiles,
    policy: MutationPolicy,
}

impl Recheck<'_> {
    fn check(
        &self,
        target: &Target,
        current: &[u8],
        computed_from: Option<u64>,
    ) -> Result<(), MutationError> {
        if computed_from.is_some_and(|expected| expected != hash_of(current)) {
            return Err(changed_on_disk(target));
        }
        if self.policy == MutationPolicy::PatchAuthorized {
            return Ok(());
        }
        // The registry is keyed by the file itself, which is `target.canonical`: the
        // spelling could resolve elsewhere than the handle this change writes through,
        // and a lookup keyed by it would then answer for another file.
        match self.observed.check_unchanged(&target.canonical, current) {
            Observation::Unchanged => Ok(()),
            Observation::NeverObserved => Err(MutationError::Io(format!(
                "You must read {} before changing it.",
                target.display
            ))),
            Observation::ChangedSinceObserved => Err(changed_on_disk(target)),
        }
    }

    /// A change computed from a snapshot of a file that is gone now is stale too.
    fn check_absent(
        &self,
        target: &Target,
        computed_from: Option<u64>,
    ) -> Result<(), MutationError> {
        match computed_from {
            Some(_) => Err(changed_on_disk(target)),
            None => Ok(()),
        }
    }
}

fn changed_on_disk(target: &Target) -> MutationError {
    MutationError::Io(format!(
        "{} changed on disk since you last read it; read it again.",
        target.display
    ))
}

/// The identity of the descriptor inspected before staging. Comparing the leaf in
/// the held parent directory immediately before apply refuses substitutions that
/// occurred during staging; arbitrary ungated writes after this comparison cannot
/// be excluded without an atomic filesystem compare-and-swap operation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct LeafIdentity {
    dev: u64,
    ino: u64,
}

impl LeafIdentity {
    fn of(metadata: &std::fs::Metadata) -> Self {
        Self {
            dev: metadata.dev(),
            ino: metadata.ino(),
        }
    }
}

fn verify_leaf(
    dir: &OwnedFd,
    leaf: &OsStr,
    inspected: LeafIdentity,
    target: &Target,
) -> Result<(), MutationError> {
    let stat = rustix::fs::statat(dir, leaf, AtFlags::SYMLINK_NOFOLLOW)
        .map_err(|_| changed_on_disk(target))?;
    if stat.st_dev as u64 != inspected.dev
        || stat.st_ino as u64 != inspected.ino
        || FileType::from_raw_mode(stat.st_mode) != FileType::RegularFile
    {
        return Err(changed_on_disk(target));
    }
    Ok(())
}

/// Re-read the file `stage` inspected, through the held directory handle, and refuse when its
/// bytes no longer match: [`verify_leaf`] compares identity only, so a writer that rewrites an
/// already-open inode in place — leaving `dev`/`ino` unchanged — is caught here, immediately
/// before the mutation, which is the smallest window current primitives allow.
fn verify_unchanged_contents(
    dir: &OwnedFd,
    leaf: &OsStr,
    inspected: &[u8],
    target: &Target,
) -> Result<(), MutationError> {
    let fd = rustix::fs::openat(
        dir,
        leaf,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(|_| changed_on_disk(target))?;
    let file = File::from(fd);
    if !file.metadata().is_ok_and(|metadata| metadata.is_file()) {
        return Err(changed_on_disk(target));
    }
    let mut bytes = Vec::new();
    file.take(inspected.len() as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| changed_on_disk(target))?;
    if bytes != inspected {
        return Err(changed_on_disk(target));
    }
    Ok(())
}

/// The credential index captured before staging may be stale: an ungated writer can link a
/// file into a protected directory while the change is computed. Revalidate the captured
/// directory stamps under the gate and rebuild when one moved, before any leaf is checked, so
/// the newly protected inode is refused through its alias rather than mutated.
fn refresh_credential_index(
    index: &mut ProtectedIndex,
    credentials: &CredentialPolicy,
) -> Result<(), MutationError> {
    let cancel = CancellationToken::new();
    if index.still_current(&cancel).unwrap_or(false) {
        return Ok(());
    }
    *index = ProtectedIndex::build(credentials, &cancel)
        .map_err(|_| MutationError::Io("credential policy check cancelled".into()))?;
    Ok(())
}

/// A regular file's current bytes and permission bits.
struct Existing {
    bytes: Vec<u8>,
    mode: u32,
    metadata: std::fs::Metadata,
}

fn refuse_credential(
    policy: &CredentialPolicy,
    index: &ProtectedIndex,
    target: &Target,
    existing: Option<&Existing>,
) -> Result<(), MutationError> {
    let protected = policy.refuses(&target.path)
        || policy.refuses(&target.canonical)
        || existing.is_some_and(|file| {
            index.refuses_metadata(&file.metadata)
                || index.refuses_current_exact(policy, &file.metadata)
        });
    if protected {
        return Err(MutationError::Io(crate::credential_refusal(
            &target.display,
        )));
    }
    Ok(())
}

/// A staged temporary beside its target, removed on drop unless it replaced it.
struct StagedFile {
    dir: OwnedFd,
    leaf: OsString,
    temp: Option<OsString>,
    identity: LeafIdentity,
}

impl StagedFile {
    /// Write `contents` to a new sibling temporary of `leaf` in `dir` and sync it.
    /// The temporary is created with the target's permission bits (so it is never
    /// briefly wider than the target) and set to them exactly, past the umask.
    fn stage(
        dir: OwnedFd,
        leaf: &OsStr,
        contents: &[u8],
        mode: Option<u32>,
    ) -> std::io::Result<Self> {
        let temp = temp_name(leaf);
        let fd = rustix::fs::openat(
            &dir,
            temp.as_os_str(),
            OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::from_raw_mode(mode.unwrap_or(0o666)),
        )?;
        // From here on the drop of `staged` removes the temporary on any failure.
        let mut file = File::from(fd);
        let identity = LeafIdentity::of(&file.metadata()?);
        let staged = Self {
            dir,
            leaf: leaf.to_os_string(),
            temp: Some(temp),
            identity,
        };
        file.write_all(contents)?;
        if let Some(mode) = mode {
            file.set_permissions(std::fs::Permissions::from_mode(mode))?;
        }
        file.sync_all()?;
        Ok(staged)
    }

    /// Rename the temporary over the target, in the same directory handle.
    ///
    /// `create_only` renames with `RENAME_NOREPLACE`, so a path filled after the
    /// staged `exists` check is refused with `EEXIST` instead of silently overwritten;
    /// a plain write replaces whatever is there, as before.
    fn replace(&mut self, create_only: bool) -> Result<(), Errno> {
        let Some(temp) = &self.temp else {
            return Ok(());
        };
        if !self.temp_is_staged(temp) {
            return Err(Errno::BUSY);
        }
        rename_noreplace(
            &self.dir,
            temp.as_os_str(),
            &self.dir,
            self.leaf.as_os_str(),
            create_only,
        )?;
        self.temp = None;
        sync_directory(&self.dir);
        Ok(())
    }

    fn temp_is_staged(&self, temp: &OsStr) -> bool {
        rustix::fs::statat(&self.dir, temp, AtFlags::SYMLINK_NOFOLLOW).is_ok_and(|stat| {
            stat.st_dev == self.identity.dev
                && stat.st_ino == self.identity.ino
                && FileType::from_raw_mode(stat.st_mode) == FileType::RegularFile
        })
    }
}

impl Drop for StagedFile {
    fn drop(&mut self) {
        if let Some(temp) = self.temp.take()
            && self.temp_is_staged(&temp)
        {
            let _ = rustix::fs::unlinkat(&self.dir, temp.as_os_str(), AtFlags::empty());
        }
    }
}

/// Best effort: fsync the directory so a rename or unlink itself survives a crash.
/// Not every filesystem supports directory fsync, so the error is ignored.
fn sync_directory(dir: &OwnedFd) {
    let _ = rustix::fs::fsync(dir);
}

/// Rename `from` onto `to`, refusing an occupied `to` when `noreplace` is set.
///
/// `RENAME_NOREPLACE` makes a create-only replacement's and a rename destination's
/// `AlreadyExists` refusal atomic with the rename itself: an ungated writer that
/// fills the path after the staged `exists` check is refused with `EEXIST` instead
/// of being silently overwritten. A kernel or filesystem without `renameat2`
/// support (`EINVAL`/`ENOSYS`) cannot give that guarantee, so there the no-replace
/// rename is refused with `EEXIST` too — the callers' `AlreadyExists` — rather than
/// degraded to a plain rename that would overwrite the path.
fn rename_noreplace(
    from_dir: &OwnedFd,
    from: &OsStr,
    to_dir: &OwnedFd,
    to: &OsStr,
    noreplace: bool,
) -> Result<(), Errno> {
    rename_noreplace_on(from_dir, from, to_dir, to, noreplace, Renameat2::Kernel)
}

/// Which renameat2 the no-replace rename is attempted with. Production is
/// [`Renameat2::Kernel`]; a test passes [`Renameat2::Absent`] to stand in for a
/// filesystem without it, whose refusal the decision below must treat exactly like
/// the `EINVAL`/`ENOSYS` one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Renameat2 {
    /// The kernel's `renameat2(RENAME_NOREPLACE)`.
    Kernel,
    /// As if the kernel or filesystem lacked it.
    #[cfg(test)]
    Absent,
}

/// [`rename_noreplace`] with the renameat2 the caller decides.
fn rename_noreplace_on(
    from_dir: &OwnedFd,
    from: &OsStr,
    to_dir: &OwnedFd,
    to: &OsStr,
    noreplace: bool,
    renameat2: Renameat2,
) -> Result<(), Errno> {
    if !noreplace {
        return rustix::fs::renameat(from_dir, from, to_dir, to);
    }
    let attempt = match renameat2 {
        Renameat2::Kernel => {
            rustix::fs::renameat_with(from_dir, from, to_dir, to, RenameFlags::NOREPLACE)
        }
        // A filesystem without it answers EINVAL/ENOSYS, which is what deciding sees.
        #[cfg(test)]
        Renameat2::Absent => Err(Errno::NOSYS),
    };
    match attempt {
        Ok(()) => Ok(()),
        // Fail closed: with no atomic no-replace rename there is no refusal to make, so
        // the rename is refused as `AlreadyExists` and the destination is never touched.
        Err(Errno::INVAL | Errno::NOSYS) => Err(Errno::EXIST),
        Err(error) => Err(error),
    }
}

/// The workspace root's directory handle, never following a symlink at its final
/// component. `plan` walks down from this handle, and it is re-opened by path under the
/// gate, so a root swapped for a symlink after the workspace was resolved must refuse
/// instead of re-opening wherever the link points; `O_DIRECTORY` alone would follow it.
fn open_root(root: &std::path::Path) -> Result<OwnedFd, MutationError> {
    rustix::fs::openat(
        CWD,
        root,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(|error| {
        // `O_DIRECTORY` may report a symlink as "not a directory" before `O_NOFOLLOW`'s
        // ELOOP; only a look at the entry itself tells a swapped link from a file.
        if matches!(error, Errno::LOOP | Errno::NOTDIR)
            && rustix::fs::statat(CWD, root, AtFlags::SYMLINK_NOFOLLOW)
                .is_ok_and(|stat| FileType::from_raw_mode(stat.st_mode) == FileType::Symlink)
        {
            return MutationError::Io("the workspace root is a symlink".to_owned());
        }
        MutationError::Io(format!("the workspace could not be opened: {error}"))
    })
}

fn leaf_of(target: &Target) -> &OsStr {
    // `Workspace::target` refused a path without a file name.
    target.path.file_name().unwrap_or_default()
}

/// Open the directory `name` inside `dir`, refusing a symlink.
fn open_directory(dir: &OwnedFd, name: &OsStr, target: &Target) -> Result<OwnedFd, MutationError> {
    rustix::fs::openat(
        dir,
        name,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(|error| match error {
        Errno::LOOP => MutationError::OutsideWorkspace {
            requested: target.requested.clone(),
        },
        // O_DIRECTORY may report a symlink as "not a directory" before O_NOFOLLOW's
        // ELOOP; only a look at the entry itself tells a swapped link from a file.
        Errno::NOTDIR => match rustix::fs::statat(dir, name, AtFlags::SYMLINK_NOFOLLOW) {
            Ok(stat) if FileType::from_raw_mode(stat.st_mode) == FileType::Symlink => {
                MutationError::OutsideWorkspace {
                    requested: target.requested.clone(),
                }
            }
            _ => MutationError::WrongKind {
                requested: target.requested.clone(),
            },
        },
        Errno::NOENT => MutationError::NotFound {
            requested: target.requested.clone(),
        },
        other => MutationError::Io(format!("{} could not be resolved: {other}", target.display)),
    })
}

/// The parent directory staging works in: the handle `plan` opened, with the names it
/// recorded missing below it created now. `create` decides whether a parent still
/// missing is a refusal (`NotFound`, as a removal or a rename source needs) or is made,
/// as a write's or a rename's destination's is. Directories are created here and
/// nowhere else, inside the plan's handle — no path of `target` is resolved again.
fn open_parent(parent: &Parent, target: &Target, create: bool) -> Result<OwnedFd, MutationError> {
    if !parent.missing.is_empty() && !create {
        return Err(MutationError::NotFound {
            requested: target.requested.clone(),
        });
    }
    let mut dir = parent.dir.try_clone().map_err(|error| {
        MutationError::Io(format!("the workspace could not be opened: {error}"))
    })?;
    for name in &parent.missing {
        match rustix::fs::mkdirat(&dir, name, Mode::from_raw_mode(0o777)) {
            Ok(()) | Err(Errno::EXIST) => {}
            Err(error) => {
                return Err(MutationError::Io(format!(
                    "failed to create the parent directories of {}: {error}",
                    target.display
                )));
            }
        }
        dir = open_directory(&dir, name, target)?;
    }
    Ok(dir)
}

/// Prove, with the gate held, that `parent` is still the directory its workspace path
/// opens: the names `plan` walked are re-walked from the freshly opened root without
/// following a symlink, and must land on the handle the plan kept. A component swapped
/// for a symlink refuses in the walk itself (`open_directory` never follows one), and a
/// component that now leads to another directory refuses here. This checks the plan's
/// handle; it is not how staging finds the directory — staging works in the handle.
fn parent_still_planned(
    root: &OwnedFd,
    parent: &Parent,
    target: &Target,
) -> Result<(), MutationError> {
    let mut walked = root.try_clone().map_err(|error| {
        MutationError::Io(format!("the workspace could not be opened: {error}"))
    })?;
    for name in &parent.names {
        walked = open_directory(&walked, name, target)?;
    }
    let same = same_directory(&walked, &parent.dir).map_err(|error| {
        MutationError::Io(format!("{} could not be resolved: {error}", target.display))
    })?;
    if same {
        return Ok(());
    }
    Err(MutationError::OutsideWorkspace {
        requested: target.requested.clone(),
    })
}

/// Whether two handles are one directory: what the gate re-walked must be the directory
/// the plan opened, not a directory that took its place while the gate was being taken.
fn same_directory(one: &OwnedFd, other: &OwnedFd) -> Result<bool, Errno> {
    let (one, other) = (rustix::fs::fstat(one)?, rustix::fs::fstat(other)?);
    Ok(one.st_dev == other.st_dev && one.st_ino == other.st_ino)
}

/// Whether anything at all (a dangling symlink included) is at `leaf` in `dir`.
fn exists(dir: &OwnedFd, leaf: &OsStr, target: &Target) -> Result<bool, MutationError> {
    match rustix::fs::statat(dir, leaf, AtFlags::SYMLINK_NOFOLLOW) {
        Ok(_) => Ok(true),
        Err(Errno::NOENT) => Ok(false),
        Err(error) => Err(MutationError::Io(format!(
            "{} could not be read: {error}",
            target.display
        ))),
    }
}

/// A dangling leaf link has no target at validation, so a write replaces the
/// link entry itself (never follows it), just as the native atomic writer did.
fn inspect_write(
    dir: &OwnedFd,
    leaf: &OsStr,
    target: &Target,
) -> Result<Option<Existing>, MutationError> {
    if let Ok(stat) = rustix::fs::statat(dir, leaf, AtFlags::SYMLINK_NOFOLLOW)
        && FileType::from_raw_mode(stat.st_mode) == FileType::Symlink
        && matches!(
            rustix::fs::statat(dir, leaf, AtFlags::empty()),
            Err(Errno::NOENT)
        )
    {
        return Ok(None);
    }
    inspect(dir, leaf, target)
}

/// The regular file at `leaf` in `dir`, or `None` when nothing is there. Anything
/// else, a symlink included, is `WrongKind`: validation resolved an existing leaf to
/// its canonical file, so a link at the leaf now is not the file that was validated.
fn inspect(
    dir: &OwnedFd,
    leaf: &OsStr,
    target: &Target,
) -> Result<Option<Existing>, MutationError> {
    let read_error = |error: std::io::Error| {
        MutationError::Io(format!("{} could not be read: {error}", target.display))
    };
    let wrong_kind = || MutationError::WrongKind {
        requested: target.requested.clone(),
    };
    let stat = match rustix::fs::statat(dir, leaf, AtFlags::SYMLINK_NOFOLLOW) {
        Ok(stat) => stat,
        Err(Errno::NOENT) => return Ok(None),
        Err(error) => return Err(read_error(error.into())),
    };
    if FileType::from_raw_mode(stat.st_mode) != FileType::RegularFile {
        return Err(wrong_kind());
    }
    // NONBLOCK: should a fifo be swapped in after the stat, opening it must not hang
    // the gate; the kind is checked again on the opened file.
    let fd = rustix::fs::openat(
        dir,
        leaf,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(|error| match error {
        Errno::LOOP => wrong_kind(),
        other => read_error(other.into()),
    })?;
    let file = File::from(fd);
    let metadata = file.metadata().map_err(read_error)?;
    if !metadata.is_file() {
        return Err(wrong_kind());
    }
    if metadata.len() > MAX_FILE_BYTES {
        return Err(MutationError::Io(format!(
            "{} is too large to mutate (limit {MAX_FILE_BYTES} bytes)",
            target.display
        )));
    }
    let mut bytes = Vec::new();
    file.take(MAX_FILE_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(read_error)?;
    if bytes.len() as u64 > MAX_FILE_BYTES {
        return Err(MutationError::Io(format!(
            "{} is too large to mutate (limit {MAX_FILE_BYTES} bytes)",
            target.display
        )));
    }
    Ok(Some(Existing {
        bytes,
        mode: metadata.permissions().mode() & 0o7777,
        metadata,
    }))
}

#[cfg(test)]
mod tests {
    use super::{Change, MutationError, MutationPolicy, OwnedMutation};
    use crate::{Observation, ObservedFiles, ReadRecord, Workspace};
    use std::ffi::OsStr;
    use std::fs;
    use std::future::Future;
    use std::os::unix::fs::{PermissionsExt, symlink};
    use std::path::Path;
    use std::pin::pin;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::task::{Context, Poll, Waker};

    const OBSERVED: MutationPolicy = MutationPolicy::Observed;
    const PATCH: MutationPolicy = MutationPolicy::PatchAuthorized;

    /// A workspace in a fresh temporary directory, with the files `(path, contents)`.
    fn workspace(files: &[(&str, &str)]) -> (tempfile::TempDir, Workspace) {
        let dir = tempfile::tempdir().unwrap();
        for (path, contents) in files {
            let path = dir.path().join(path);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, contents).unwrap();
        }
        let workspace = Workspace::new(dir.path()).unwrap();
        (dir, workspace)
    }

    fn text(workspace: &Workspace, path: &str) -> String {
        fs::read_to_string(workspace.root().join(path)).unwrap()
    }

    /// Every entry under `dir`, recursively, as root-relative names.
    fn entries(dir: &Path) -> Vec<String> {
        let mut found = Vec::new();
        let mut pending = vec![dir.to_path_buf()];
        while let Some(next) = pending.pop() {
            for entry in fs::read_dir(&next).unwrap() {
                let entry = entry.unwrap();
                let path = entry.path();
                found.push(
                    path.strip_prefix(dir)
                        .unwrap()
                        .to_string_lossy()
                        .into_owned(),
                );
                if entry.file_type().unwrap().is_dir() {
                    pending.push(path);
                }
            }
        }
        found.sort();
        found
    }

    fn no_temporaries(dir: &Path) {
        let temporaries: Vec<String> = entries(dir)
            .into_iter()
            .filter(|name| name.contains(".p1-tmp-"))
            .collect();
        assert!(temporaries.is_empty(), "left behind: {temporaries:?}");
    }

    /// Poll a future that must be ready at once (the gate is free).
    fn ready<F: Future>(future: F) -> F::Output {
        match pin!(future).poll(&mut Context::from_waker(Waker::noop())) {
            Poll::Ready(output) => output,
            Poll::Pending => panic!("the gate was expected to be free"),
        }
    }

    /// The two halves of `Workspace::commit`, so a test can change the filesystem
    /// between validation and the gated apply.
    fn commit_in_two_steps(
        workspace: &Workspace,
        changes: &[Change],
        observed: &ObservedFiles,
        between: impl FnOnce(),
    ) -> Result<(), MutationError> {
        let plan = workspace.plan(changes)?;
        between();
        let _mutation = workspace.begin_mutation();
        workspace.apply(&plan, observed, OBSERVED)
    }

    fn io(message: &str) -> MutationError {
        MutationError::Io(message.to_string())
    }

    #[test]
    fn cancellation_while_queued_refuses_before_mutating() {
        let (dir, workspace) = workspace(&[("a", "old")]);
        let seen = ObservedFiles::new();
        workspace.read("a", &seen).unwrap();
        let held = workspace.begin_mutation();
        let cancel = p1_contracts::CancellationToken::new();
        let worker = {
            let workspace = workspace.clone();
            let seen = seen.clone();
            let cancel = cancel.clone();
            std::thread::spawn(move || {
                workspace.commit_cancellable(
                    &[Change::write("a", b"new")],
                    &seen,
                    OBSERVED,
                    &cancel,
                )
            })
        };
        while workspace.write_gate().waiting_writers() == 0 {
            std::thread::yield_now();
        }
        cancel.cancel();
        drop(held);
        assert_eq!(worker.join().unwrap(), Err(io("cancelled")));
        assert_eq!(fs::read(dir.path().join("a")).unwrap(), b"old");
    }

    #[test]
    fn a_cancelled_batch_under_a_held_gate_mutates_nothing() {
        let (dir, workspace) = workspace(&[("a", "old")]);
        let observed = ObservedFiles::new();
        let cancel = p1_contracts::CancellationToken::new();
        cancel.cancel();
        let held = ready(workspace.begin_owned(&observed, &ReadRecord::new(), PATCH));
        let result = held.apply_all_cancellable(&[Change::write("a", b"new")], &cancel);
        assert_eq!(result, Err(io("cancelled")));
        assert_eq!(fs::read(dir.path().join("a")).unwrap(), b"old");
        no_temporaries(dir.path());
    }

    #[test]
    fn writing_a_dangling_leaf_link_replaces_the_link_not_its_target() {
        let (dir, workspace) = workspace(&[]);
        symlink("missing.txt", dir.path().join("link.txt")).unwrap();
        let observed = ObservedFiles::new();
        workspace
            .commit(&[Change::write("link.txt", b"new")], &observed, OBSERVED)
            .unwrap();
        assert_eq!(fs::read(dir.path().join("link.txt")).unwrap(), b"new");
        assert!(!dir.path().join("missing.txt").exists());
        assert!(
            !fs::symlink_metadata(dir.path().join("link.txt"))
                .unwrap()
                .file_type()
                .is_symlink()
        );
        symlink("missing.txt", dir.path().join("another.txt")).unwrap();
        assert!(matches!(
            workspace.commit(&[Change::create("another.txt", b"new")], &observed, PATCH),
            Err(MutationError::AlreadyExists { .. })
        ));
    }

    #[test]
    fn apply_refuses_substituted_leaf_for_write_remove_and_rename() {
        for change in [
            Change::write("a", b"replacement"),
            Change::remove("a"),
            Change::rename("a", "b"),
        ] {
            let (dir, workspace) = workspace(&[("a", "original")]);
            let changes = [change];
            let plan = workspace.plan(&changes).unwrap();
            let observed = ObservedFiles::new();
            let _gate = workspace.begin_mutation();
            let result = workspace.apply_with_before_apply(&plan, &observed, PATCH, None, || {
                fs::rename(dir.path().join("a"), dir.path().join("saved")).unwrap();
                // Identical bytes do not prove this is the file staged earlier.
                fs::write(dir.path().join("a"), b"original").unwrap();
            });
            assert_eq!(
                result,
                Err(io(
                    "a changed on disk since you last read it; read it again."
                ))
            );
            assert_eq!(fs::read(dir.path().join("a")).unwrap(), b"original");
            assert_eq!(fs::read(dir.path().join("saved")).unwrap(), b"original");
            assert!(!dir.path().join("b").exists());
            no_temporaries(dir.path());
        }
    }

    #[test]
    fn checked_leaf_identity_refuses_same_contents_substitution() {
        for change in [
            Change::write("a", "new"),
            Change::remove("a"),
            Change::rename("a", "b"),
        ] {
            let (dir, workspace) = workspace(&[("a", "old")]);
            let observed = ObservedFiles::new();
            let plan = workspace.plan(std::slice::from_ref(&change)).unwrap();
            let credentials = crate::CredentialPolicy::new(None, &[]);
            let index =
                crate::ProtectedIndex::build(&credentials, &p1_contracts::CancellationToken::new())
                    .unwrap();
            let staged = workspace
                .stage(
                    &plan[0],
                    &super::Recheck {
                        observed: &observed,
                        policy: PATCH,
                    },
                    &credentials,
                    &index,
                )
                .unwrap();
            fs::rename(dir.path().join("a"), dir.path().join("old-file")).unwrap();
            fs::write(dir.path().join("a"), b"old").unwrap();
            let (directory, leaf, identity, target) = match &staged {
                super::Staged::Replace {
                    file,
                    target,
                    inspected: Some(identity),
                    ..
                } => (&file.dir, file.leaf.as_os_str(), *identity, *target),
                super::Staged::Remove {
                    dir,
                    leaf,
                    inspected,
                    target,
                    ..
                } => (dir, leaf.as_os_str(), *inspected, *target),
                super::Staged::Rename {
                    from_dir,
                    from_leaf,
                    inspected,
                    from,
                    ..
                } => (from_dir, from_leaf.as_os_str(), *inspected, *from),
                _ => panic!("expected inspected leaf"),
            };
            assert!(super::verify_leaf(directory, leaf, identity, target).is_err());
            assert_eq!(fs::read(dir.path().join("a")).unwrap(), b"old");
            assert!(!dir.path().join("b").exists());
        }
    }

    #[test]
    fn apply_refuses_an_in_place_change_to_an_inspected_file() {
        for change in [
            Change::write("a", b"replacement"),
            Change::remove("a"),
            Change::rename("a", "b"),
        ] {
            let (dir, workspace) = workspace(&[("a", "original")]);
            let changes = [change];
            let plan = workspace.plan(&changes).unwrap();
            let observed = ObservedFiles::new();
            let _gate = workspace.begin_mutation();
            let result = workspace.apply_with_before_apply(&plan, &observed, PATCH, None, || {
                // Truncating through a fresh descriptor keeps the same inode, so only the
                // contents prove the file moved on after staging inspected it.
                fs::write(dir.path().join("a"), b"external").unwrap();
            });
            assert_eq!(
                result,
                Err(io(
                    "a changed on disk since you last read it; read it again."
                ))
            );
            assert_eq!(fs::read(dir.path().join("a")).unwrap(), b"external");
            assert!(!dir.path().join("b").exists());
            no_temporaries(dir.path());
        }
    }

    #[test]
    fn apply_refreshes_the_credential_index_for_a_new_protected_hard_link() {
        let home = tempfile::tempdir().unwrap();
        fs::create_dir_all(home.path().join(".config/keys")).unwrap();
        fs::write(home.path().join("alias"), b"original").unwrap();
        let workspace = Workspace::new(home.path())
            .unwrap()
            .with_credential_home(Some(home.path().to_path_buf()));
        let changes = [Change::write("alias", b"replacement")];
        let plan = workspace.plan(&changes).unwrap();
        let observed = ObservedFiles::new();
        let _gate = workspace.begin_mutation();
        let result = workspace.apply_with_hooks(
            &plan,
            &observed,
            PATCH,
            None,
            || {
                // An ungated writer links the ordinary target into the protected store
                // after the index was captured and before its leaf is checked.
                fs::hard_link(
                    home.path().join("alias"),
                    home.path().join(".config/keys/new.key"),
                )
                .unwrap();
            },
            || {},
        );
        let error = result.unwrap_err();
        assert!(
            format!("{error}").contains("refuses credential files"),
            "{error}"
        );
        assert_eq!(fs::read(home.path().join("alias")).unwrap(), b"original");
    }

    #[test]
    fn credential_mutations_refuse_names_and_hardlink_aliases() {
        let dir = tempfile::tempdir().unwrap();
        let credential = dir.path().join(".codex/auth.json");
        fs::create_dir_all(credential.parent().unwrap()).unwrap();
        fs::write(&credential, b"private-marker").unwrap();
        fs::hard_link(&credential, dir.path().join("alias")).unwrap();
        fs::write(dir.path().join("ordinary"), b"ordinary").unwrap();
        let workspace = Workspace::new(dir.path())
            .unwrap()
            .with_credential_home(Some(dir.path().to_path_buf()));
        let observed = ObservedFiles::new();
        for change in [
            Change::write(".codex/auth.json", "altered"),
            Change::remove(".codex/auth.json"),
            Change::rename(".codex/auth.json", "elsewhere"),
            Change::write("alias", "altered"),
            Change::remove("alias"),
            Change::rename("alias", "elsewhere"),
            Change::rename("ordinary", "alias"),
            Change::create(".config/keys/new.key", "altered"),
        ] {
            let error = workspace.commit(&[change], &observed, PATCH).unwrap_err();
            assert!(
                format!("{error}").contains("refuses credential files"),
                "{error}"
            );
            assert_eq!(fs::read(&credential).unwrap(), b"private-marker");
            assert_eq!(
                fs::read(dir.path().join("alias")).unwrap(),
                b"private-marker"
            );
            assert_eq!(fs::read(dir.path().join("ordinary")).unwrap(), b"ordinary");
        }
        let owned = ready(workspace.begin_owned(&observed, &ReadRecord::new(), PATCH));
        assert!(owned.write("alias", "altered").is_err());
        assert!(owned.remove(".codex/auth.json").is_err());
        drop(owned);
        fs::remove_file(dir.path().join("alias")).unwrap();
        fs::remove_file(&credential).unwrap();
        assert!(
            workspace
                .commit(
                    &[Change::create(".codex/auth.json", "altered")],
                    &observed,
                    PATCH
                )
                .is_err()
        );
        assert!(!credential.exists());
    }

    #[test]
    fn removed_and_renamed_sources_need_a_new_observation() {
        let (dir, workspace) = workspace(&[("a", "same"), ("b", "same")]);
        let seen = ObservedFiles::new();
        workspace.read("a", &seen).unwrap();
        workspace.read("b", &seen).unwrap();
        workspace
            .commit(
                &[Change::remove("a"), Change::rename("b", "c")],
                &seen,
                OBSERVED,
            )
            .unwrap();
        for path in ["a", "b"] {
            fs::write(dir.path().join(path), b"same").unwrap();
            assert_eq!(
                seen.check_unchanged(&dir.path().join(path), b"same"),
                Observation::NeverObserved
            );
            assert!(
                workspace
                    .commit(&[Change::write(path, b"new")], &seen, OBSERVED)
                    .is_err()
            );
        }
    }

    #[test]
    fn sparse_target_over_limit_is_refused_without_materializing_it() {
        let (dir, workspace) = workspace(&[]);
        let file = fs::File::create(dir.path().join("large")).unwrap();
        file.set_len(super::MAX_FILE_BYTES + 1).unwrap();
        let result = workspace.commit(&[Change::remove("large")], &ObservedFiles::new(), PATCH);
        assert!(matches!(result, Err(MutationError::Io(text)) if text.contains("too large")));
        assert!(dir.path().join("large").exists());
    }

    #[test]
    fn owned_removal_clears_the_old_read_identity() {
        let (dir, workspace) = workspace(&[("a", "same")]);
        let reads = ReadRecord::new();
        reads.record(&dir.path().join("a"), b"same");
        let owned = ready(workspace.begin_owned(&ObservedFiles::new(), &reads, PATCH));
        owned.remove("a").unwrap();
        assert_eq!(reads.recorded(&dir.path().join("a")), None);
        owned.create("a", b"new").unwrap();
        assert_eq!(text(&workspace, "a"), "new");
    }

    #[test]
    fn commit_applies_every_change_and_records_what_it_wrote() {
        let (dir, workspace) = workspace(&[
            ("edit.txt", "old\n"),
            ("gone.txt", "bye\n"),
            ("from.txt", "moving\n"),
        ]);
        fs::set_permissions(
            dir.path().join("edit.txt"),
            fs::Permissions::from_mode(0o640),
        )
        .unwrap();
        let observed = ObservedFiles::new();
        for path in ["edit.txt", "gone.txt", "from.txt"] {
            workspace.read(path, &observed).unwrap();
        }

        workspace
            .commit(
                &[
                    Change::write("edit.txt", "new\n"),
                    Change::create("deep/er/new.txt", "fresh\n"),
                    Change::remove("gone.txt"),
                    Change::rename("from.txt", "moved/to.txt"),
                ],
                &observed,
                OBSERVED,
            )
            .unwrap();

        assert_eq!(text(&workspace, "edit.txt"), "new\n");
        assert_eq!(text(&workspace, "deep/er/new.txt"), "fresh\n");
        assert!(!dir.path().join("gone.txt").exists());
        assert!(!dir.path().join("from.txt").exists());
        assert_eq!(text(&workspace, "moved/to.txt"), "moving\n");
        let mode = fs::metadata(dir.path().join("edit.txt"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o640, "permission bits are kept");
        for (path, contents) in [
            ("edit.txt", "new\n"),
            ("deep/er/new.txt", "fresh\n"),
            ("moved/to.txt", "moving\n"),
        ] {
            assert_eq!(
                observed.check_unchanged(&workspace.root().join(path), contents.as_bytes()),
                Observation::Unchanged,
                "{path} is recorded as this agent's observation"
            );
        }
        no_temporaries(dir.path());
        // The gate was released before `commit` returned.
        drop(workspace.begin_mutation());
    }

    #[test]
    fn an_unobserved_or_stale_target_is_refused_with_the_native_messages() {
        let (dir, workspace) = workspace(&[("a.txt", "one\n"), ("b.txt", "two\n")]);
        let observed = ObservedFiles::new();

        assert_eq!(
            workspace.commit(&[Change::write("a.txt", "x")], &observed, OBSERVED),
            Err(io("You must read a.txt before changing it."))
        );
        assert_eq!(
            workspace.commit(&[Change::remove("a.txt")], &observed, OBSERVED),
            Err(io("You must read a.txt before changing it."))
        );
        assert_eq!(
            workspace.commit(&[Change::rename("a.txt", "c.txt")], &observed, OBSERVED),
            Err(io("You must read a.txt before changing it."))
        );

        workspace.read("b.txt", &observed).unwrap();
        fs::write(dir.path().join("b.txt"), "changed by someone else\n").unwrap();
        assert_eq!(
            workspace.commit(&[Change::write("b.txt", "x")], &observed, OBSERVED),
            Err(io(
                "b.txt changed on disk since you last read it; read it again."
            ))
        );

        assert_eq!(text(&workspace, "a.txt"), "one\n");
        assert_eq!(text(&workspace, "b.txt"), "changed by someone else\n");
        assert!(!dir.path().join("c.txt").exists());
        no_temporaries(dir.path());
    }

    #[test]
    fn a_change_computed_from_an_older_snapshot_is_refused_whatever_the_policy() {
        let (dir, workspace) = workspace(&[("a.txt", "one\n")]);
        let observed = ObservedFiles::new();
        let snapshot = workspace.read("a.txt", &observed).unwrap().metadata();
        // This agent observes the new state, but the change was computed from the old one.
        fs::write(dir.path().join("a.txt"), "two\n").unwrap();
        workspace.read("a.txt", &observed).unwrap();

        for policy in [OBSERVED, PATCH] {
            assert_eq!(
                workspace.commit(
                    &[Change::write("a.txt", "x").computed_from(&snapshot)],
                    &observed,
                    policy
                ),
                Err(io(
                    "a.txt changed on disk since you last read it; read it again."
                ))
            );
        }
        assert_eq!(text(&workspace, "a.txt"), "two\n");

        let current = workspace.read("a.txt", &observed).unwrap().metadata();
        workspace
            .commit(
                &[Change::write("a.txt", "three\n").computed_from(&current)],
                &observed,
                OBSERVED,
            )
            .unwrap();
        assert_eq!(text(&workspace, "a.txt"), "three\n");
    }

    #[test]
    fn the_patch_exemption_needs_no_observation_and_records_like_patch() {
        let (dir, workspace) =
            workspace(&[("a.txt", "one\n"), ("gone.txt", "x\n"), ("from.txt", "m\n")]);
        let observed = ObservedFiles::new();

        workspace
            .commit(
                &[
                    Change::write("a.txt", "patched\n"),
                    Change::remove("gone.txt"),
                    Change::rename("from.txt", "to.txt"),
                ],
                &observed,
                PATCH,
            )
            .unwrap();

        assert_eq!(text(&workspace, "a.txt"), "patched\n");
        assert!(!dir.path().join("gone.txt").exists());
        assert_eq!(text(&workspace, "to.txt"), "m\n");
        let root = workspace.root();
        assert_eq!(
            observed.check_unchanged(&root.join("a.txt"), b"patched\n"),
            Observation::Unchanged
        );
        assert_eq!(
            observed.check_unchanged(&root.join("to.txt"), b"m\n"),
            Observation::Unchanged
        );
        // A follow-up observed edit needs no re-read, as after the patch tool.
        workspace
            .commit(&[Change::write("a.txt", "edited\n")], &observed, OBSERVED)
            .unwrap();
    }

    #[test]
    fn create_and_rename_refuse_an_occupied_path() {
        let (dir, workspace) = workspace(&[("a.txt", "a\n"), ("b.txt", "b\n")]);
        symlink(dir.path().join("absent"), dir.path().join("dangling")).unwrap();
        let observed = ObservedFiles::new();
        workspace.read("a.txt", &observed).unwrap();

        for occupied in ["b.txt", "dangling"] {
            assert_eq!(
                workspace.commit(&[Change::create(occupied, "new")], &observed, PATCH),
                Err(MutationError::AlreadyExists {
                    requested: occupied.to_string()
                })
            );
            assert_eq!(
                workspace.commit(&[Change::rename("a.txt", occupied)], &observed, OBSERVED),
                Err(MutationError::AlreadyExists {
                    requested: occupied.to_string()
                })
            );
        }
        assert_eq!(text(&workspace, "a.txt"), "a\n");
        assert_eq!(text(&workspace, "b.txt"), "b\n");
        no_temporaries(dir.path());
    }

    #[test]
    fn errors_map_onto_the_wit_fs_error_cases() {
        let (dir, workspace) = workspace(&[("a.txt", "a\n")]);
        fs::create_dir(dir.path().join("sub")).unwrap();
        let observed = ObservedFiles::new();

        assert_eq!(
            workspace.commit(&[Change::write("../x.txt", "x")], &observed, PATCH),
            Err(MutationError::OutsideWorkspace {
                requested: "../x.txt".into()
            })
        );
        assert_eq!(
            workspace.commit(&[Change::remove("missing.txt")], &observed, PATCH),
            Err(MutationError::NotFound {
                requested: "missing.txt".into()
            })
        );
        assert_eq!(
            workspace.commit(&[Change::rename("nope/a.txt", "b.txt")], &observed, PATCH),
            Err(MutationError::NotFound {
                requested: "nope/a.txt".into()
            })
        );
        assert_eq!(
            workspace.commit(&[Change::write("sub", "x")], &observed, PATCH),
            Err(MutationError::WrongKind {
                requested: "sub".into()
            })
        );
        assert_eq!(
            workspace.commit(&[Change::write("a.txt/x", "x")], &observed, PATCH),
            Err(MutationError::WrongKind {
                requested: "a.txt/x".into()
            })
        );
        assert_eq!(
            workspace.commit(
                &[Change::write("a.txt", "1"), Change::remove("./a.txt")],
                &observed,
                PATCH
            ),
            Err(io("a.txt is changed more than once in one commit."))
        );
        assert_eq!(text(&workspace, "a.txt"), "a\n");
    }

    #[test]
    fn a_failure_while_staging_leaves_every_target_untouched_and_no_temporaries() {
        let (dir, workspace) =
            workspace(&[("a.txt", "a\n"), ("b.txt", "b\n"), ("never.txt", "n\n")]);
        fs::create_dir(dir.path().join("sub")).unwrap();
        let observed = ObservedFiles::new();
        workspace.read("a.txt", &observed).unwrap();
        workspace.read("b.txt", &observed).unwrap();

        // The first two changes are staged before the third is refused.
        assert_eq!(
            workspace.commit(
                &[
                    Change::write("a.txt", "A\n"),
                    Change::create("new/c.txt", "C\n"),
                    Change::write("never.txt", "N\n"),
                ],
                &observed,
                OBSERVED,
            ),
            Err(io("You must read never.txt before changing it."))
        );
        assert_eq!(
            workspace.commit(
                &[
                    Change::write("a.txt", "A\n"),
                    Change::remove("b.txt"),
                    Change::write("sub", "S\n"),
                ],
                &observed,
                OBSERVED,
            ),
            Err(MutationError::WrongKind {
                requested: "sub".into()
            })
        );

        assert_eq!(text(&workspace, "a.txt"), "a\n");
        assert_eq!(text(&workspace, "b.txt"), "b\n");
        assert_eq!(text(&workspace, "never.txt"), "n\n");
        assert!(!dir.path().join("new/c.txt").exists());
        no_temporaries(dir.path());
    }

    #[test]
    fn a_parent_swapped_for_an_escaping_symlink_after_validation_is_refused() {
        let outside = tempfile::tempdir().unwrap();
        fs::write(outside.path().join("victim.txt"), "outside\n").unwrap();
        fs::write(outside.path().join("from.txt"), "outside\n").unwrap();
        let (dir, workspace) =
            workspace(&[("sub/victim.txt", "inside\n"), ("sub/from.txt", "inside\n")]);
        let observed = ObservedFiles::new();
        workspace.read("sub/victim.txt", &observed).unwrap();
        workspace.read("sub/from.txt", &observed).unwrap();
        let swap = || {
            fs::rename(dir.path().join("sub"), dir.path().join("sub.real")).unwrap();
            symlink(outside.path(), dir.path().join("sub")).unwrap();
        };
        let unswap = || {
            fs::remove_file(dir.path().join("sub")).unwrap();
            fs::rename(dir.path().join("sub.real"), dir.path().join("sub")).unwrap();
        };

        for change in [
            Change::write("sub/victim.txt", "pwned\n"),
            Change::create("sub/new.txt", "pwned\n"),
            Change::remove("sub/victim.txt"),
            Change::rename("sub/from.txt", "moved.txt"),
            Change::rename("from-root.txt", "sub/landed.txt"),
        ] {
            if matches!(&change.op, super::Op::Rename { from, .. } if from == "from-root.txt") {
                fs::write(dir.path().join("from-root.txt"), "root\n").unwrap();
                workspace.read("from-root.txt", &observed).unwrap();
            }
            let result =
                commit_in_two_steps(&workspace, std::slice::from_ref(&change), &observed, swap);
            assert!(
                matches!(result, Err(MutationError::OutsideWorkspace { .. })),
                "{change:?}: {result:?}"
            );
            unswap();
        }

        assert_eq!(
            entries(outside.path()),
            vec!["from.txt".to_string(), "victim.txt".to_string()]
        );
        assert_eq!(
            fs::read_to_string(outside.path().join("victim.txt")).unwrap(),
            "outside\n"
        );
        assert_eq!(text(&workspace, "sub/victim.txt"), "inside\n");
        assert!(!dir.path().join("moved.txt").exists());
        no_temporaries(dir.path());
    }

    #[test]
    fn the_walk_refuses_a_symlinked_directory_even_one_that_points_inside() {
        // The window between canonicalizing the parent and walking it: the walk itself
        // must not follow a link that appeared there.
        let (dir, workspace) = workspace(&[("real/a.txt", "a\n")]);
        symlink(dir.path().join("real"), dir.path().join("link")).unwrap();
        let root = rustix::fs::openat(
            rustix::fs::CWD,
            workspace.root(),
            rustix::fs::OFlags::RDONLY | rustix::fs::OFlags::DIRECTORY,
            rustix::fs::Mode::empty(),
        )
        .unwrap();
        let target = workspace.target("link/a.txt").unwrap();

        assert!(matches!(
            super::open_directory(&root, std::ffi::OsStr::new("link"), &target),
            Err(MutationError::OutsideWorkspace { .. })
        ));
        assert!(super::open_directory(&root, std::ffi::OsStr::new("real"), &target).is_ok());
    }

    #[test]
    fn concurrent_commits_serialize_and_lose_no_update() {
        const WRITERS: usize = 4;
        const ROUNDS: usize = 40;
        let (_dir, workspace) = workspace(&[("log.txt", "")]);
        let start = Arc::new(std::sync::Barrier::new(WRITERS));

        let writers: Vec<_> = (0..WRITERS)
            .map(|writer| {
                let workspace = workspace.clone();
                let start = start.clone();
                std::thread::spawn(move || {
                    // Each writer is its own agent: its own observations, the shared gate.
                    let observed = ObservedFiles::new();
                    start.wait();
                    for round in 0..ROUNDS {
                        loop {
                            let snapshot = workspace.read("log.txt", &observed).unwrap();
                            let metadata = snapshot.metadata();
                            let mut bytes = snapshot.read(0, usize::MAX).to_vec();
                            bytes.extend_from_slice(format!("{writer} {round}\n").as_bytes());
                            match workspace.commit(
                                &[Change::write("log.txt", bytes).computed_from(&metadata)],
                                &observed,
                                OBSERVED,
                            ) {
                                Ok(()) => break,
                                Err(MutationError::Io(message)) => {
                                    assert!(message.contains("changed on disk"), "{message}")
                                }
                                Err(other) => panic!("{other:?}"),
                            }
                        }
                    }
                })
            })
            .collect();
        for writer in writers {
            writer.join().unwrap();
        }

        let log = text(&workspace, "log.txt");
        assert_eq!(log.lines().count(), WRITERS * ROUNDS);
        for writer in 0..WRITERS {
            for round in 0..ROUNDS {
                let line = format!("{writer} {round}");
                assert_eq!(log.lines().filter(|found| *found == line).count(), 1);
            }
        }
    }

    #[test]
    fn the_owned_and_the_sync_gate_exclude_each_other() {
        let (_dir, workspace) = workspace(&[("a.txt", "a\n")]);
        let other_agent = workspace.clone();
        let observed = ObservedFiles::new();
        let reads = ReadRecord::new();
        let mut context = Context::from_waker(Waker::noop());

        // A native tool holds the gate: the owned acquisition waits.
        let native = other_agent.begin_mutation();
        let mut begin = pin!(workspace.begin_owned(&observed, &reads, PATCH));
        assert!(begin.as_mut().poll(&mut context).is_pending());
        drop(native);
        let Poll::Ready(owned) = begin.as_mut().poll(&mut context) else {
            panic!("the release reaches the owned acquisition");
        };

        // The owned mutation holds it: a native commit waits until it is dropped.
        let committed = Arc::new(AtomicBool::new(false));
        let native = {
            let committed = committed.clone();
            std::thread::spawn(move || {
                other_agent
                    .commit(
                        &[Change::write("b.txt", "b\n")],
                        &ObservedFiles::new(),
                        OBSERVED,
                    )
                    .unwrap();
                committed.store(true, Ordering::SeqCst);
            })
        };
        while workspace.write_gate().waiting_writers() == 0 {
            std::thread::yield_now();
        }
        assert!(!committed.load(Ordering::SeqCst));
        owned.write("a.txt", "A\n").unwrap();
        drop(owned);
        native.join().unwrap();
        assert!(committed.load(Ordering::SeqCst));
        assert_eq!(text(&workspace, "a.txt"), "A\n");
        assert_eq!(text(&workspace, "b.txt"), "b\n");
    }

    #[test]
    fn the_owned_mutation_maps_each_wit_operation_with_the_same_checks() {
        fn assert_static_send<T: Send + 'static>(_: &T) {}
        let (dir, workspace) = workspace(&[("a.txt", "a\n"), ("b.txt", "b\n")]);
        let observed = ObservedFiles::new();
        let reads = ReadRecord::new();
        let begin = workspace.begin_owned(&observed, &reads, OBSERVED);
        assert_static_send(&begin);
        let owned: OwnedMutation = ready(begin);
        assert_static_send(&owned);

        assert_eq!(
            owned.write("a.txt", "x"),
            Err(io("You must read a.txt before changing it."))
        );
        assert_eq!(
            owned.create("b.txt", "x"),
            Err(MutationError::AlreadyExists {
                requested: "b.txt".into()
            })
        );
        owned.create("c/new.txt", "new\n").unwrap();
        workspace.read("a.txt", &observed).unwrap();
        owned.write("a.txt", "A\n").unwrap();
        // The write is recorded, so a second change needs no re-read.
        owned.rename("a.txt", "d.txt").unwrap();
        assert_eq!(
            owned.rename("d.txt", "b.txt"),
            Err(MutationError::AlreadyExists {
                requested: "b.txt".into()
            })
        );
        owned.remove("d.txt").unwrap();
        assert_eq!(
            owned.remove("b.txt"),
            Err(io("You must read b.txt before changing it."))
        );
        drop(owned);

        assert_eq!(text(&workspace, "c/new.txt"), "new\n");
        assert_eq!(text(&workspace, "b.txt"), "b\n");
        assert!(!dir.path().join("a.txt").exists());
        assert!(!dir.path().join("d.txt").exists());
    }

    #[test]
    fn read_unobserved_returns_the_snapshot_and_records_nothing() {
        let (_dir, workspace) = workspace(&[("a.txt", "a\n")]);
        let observed = ObservedFiles::new();

        let unobserved = workspace.read_unobserved("a.txt").unwrap();

        assert_eq!(unobserved.read(0, 10), b"a\n");
        assert_eq!(
            observed.check_unchanged(&workspace.root().join("a.txt"), b"a\n"),
            Observation::NeverObserved
        );
        assert_eq!(
            workspace.commit(&[Change::write("a.txt", "x")], &observed, OBSERVED),
            Err(io("You must read a.txt before changing it."))
        );
        let observed_read = workspace.read("a.txt", &ObservedFiles::new()).unwrap();
        assert_eq!(unobserved.metadata(), observed_read.metadata());
    }

    #[test]
    fn plan_key_is_derived_from_the_opened_parent() {
        let (dir, workspace) = workspace(&[("a/keep", "a"), ("b/keep", "b")]);
        symlink(dir.path().join("a"), dir.path().join("link")).unwrap();
        let old = workspace.target("link/new").unwrap();
        assert_eq!(old.canonical, dir.path().join("a/new"));
        fs::remove_file(dir.path().join("link")).unwrap();
        symlink(dir.path().join("b"), dir.path().join("link")).unwrap();
        let root = super::open_root(workspace.root()).unwrap();
        let parent = workspace.plan_parent(&root, &old).unwrap();
        assert_eq!(
            workspace.parent_key(&parent, &old),
            dir.path().join("b/new")
        );
    }

    #[test]
    fn two_spellings_of_one_new_file_through_a_symlink_are_refused_as_one_change() {
        let (dir, workspace) = workspace(&[("src/keep.txt", "k\n")]);
        symlink(dir.path().join("src"), dir.path().join("link")).unwrap();
        let observed = ObservedFiles::new();

        // `link` points inside the workspace at `src`, so both changes name the same
        // not-yet-existing file: the resolved spellings differ, the canonical key
        // does not.
        assert_eq!(
            workspace.commit(
                &[
                    Change::create("src/new.txt", "first\n"),
                    Change::create("link/new.txt", "second\n"),
                ],
                &observed,
                PATCH,
            ),
            Err(io("link/new.txt is changed more than once in one commit."))
        );
        assert!(!dir.path().join("src/new.txt").exists());
        no_temporaries(dir.path());
    }

    #[test]
    fn a_create_only_replace_refuses_a_file_that_appeared_after_staging() {
        let (dir, workspace) = workspace(&[("keep.txt", "k\n")]);
        let root = rustix::fs::openat(
            rustix::fs::CWD,
            workspace.root(),
            rustix::fs::OFlags::RDONLY | rustix::fs::OFlags::DIRECTORY,
            rustix::fs::Mode::empty(),
        )
        .unwrap();
        let mut file =
            super::StagedFile::stage(root, OsStr::new("new.txt"), b"new\n", None).unwrap();
        // An ungated writer fills the create-only target between the staged `exists`
        // check and the rename; the rename itself must refuse it.
        fs::write(dir.path().join("new.txt"), "ungated\n").unwrap();

        assert_eq!(file.replace(true), Err(rustix::io::Errno::EXIST));

        assert_eq!(text(&workspace, "new.txt"), "ungated\n");
        drop(file);
        no_temporaries(dir.path());
    }

    #[test]
    fn a_rename_refuses_a_destination_that_appeared_after_staging() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("from.txt"), "from\n").unwrap();
        let fd = rustix::fs::openat(
            rustix::fs::CWD,
            dir.path(),
            rustix::fs::OFlags::RDONLY | rustix::fs::OFlags::DIRECTORY,
            rustix::fs::Mode::empty(),
        )
        .unwrap();
        // The destination appears after staging's `exists` check.
        fs::write(dir.path().join("to.txt"), "ungated\n").unwrap();

        assert_eq!(
            super::rename_noreplace(&fd, OsStr::new("from.txt"), &fd, OsStr::new("to.txt"), true,),
            Err(rustix::io::Errno::EXIST)
        );

        assert_eq!(
            fs::read_to_string(dir.path().join("to.txt")).unwrap(),
            "ungated\n"
        );
        assert_eq!(
            fs::read_to_string(dir.path().join("from.txt")).unwrap(),
            "from\n"
        );
    }

    #[test]
    fn a_second_writer_between_the_read_and_the_gated_write_is_refused() {
        let (dir, workspace) = workspace(&[("a.txt", "one\n")]);
        let observed = ObservedFiles::new();
        let reads = ReadRecord::new();
        // The call reads the file through the read side, which is what the change it
        // computes is computed from, and takes the gate afterwards.
        let snapshot = workspace.read("a.txt", &ObservedFiles::new()).unwrap();
        reads.record_hash(
            &workspace.resolve("a.txt").unwrap(),
            snapshot.metadata().content_hash,
        );
        let owned = ready(workspace.begin_owned(&observed, &reads, PATCH));

        // A second writer changes the file between the read and the gated write.
        fs::write(dir.path().join("a.txt"), "changed by another agent\n").unwrap();
        assert_eq!(
            owned.write("a.txt", "pwned\n"),
            Err(io(
                "a.txt changed on disk since you last read it; read it again."
            ))
        );
        assert_eq!(text(&workspace, "a.txt"), "changed by another agent\n");

        // The other agent's contents read again are the identity the write then may
        // replace, and the written contents become this agent's observation.
        reads.record(
            &workspace.root().join("a.txt"),
            b"changed by another agent\n",
        );
        owned.write("a.txt", "mine\n").unwrap();
        assert_eq!(text(&workspace, "a.txt"), "mine\n");
        assert_eq!(
            observed.check_unchanged(&workspace.root().join("a.txt"), b"mine\n"),
            Observation::Unchanged
        );
        drop(owned);
        no_temporaries(dir.path());
    }

    #[test]
    fn a_symlink_retargeted_between_the_read_and_the_gated_write_is_refused() {
        let (dir, workspace) = workspace(&[("a.txt", "read\n"), ("b.txt", "never read\n")]);
        std::os::unix::fs::symlink("a.txt", dir.path().join("link.txt")).unwrap();
        let observed = ObservedFiles::new();
        let reads = ReadRecord::new();
        // The call reads through the symlink, as the read side records it.
        let snapshot = workspace.read("link.txt", &ObservedFiles::new()).unwrap();
        reads.record_read(
            &workspace.spelling("link.txt"),
            &workspace.resolve("link.txt").unwrap(),
            snapshot.metadata().content_hash,
        );
        let owned = ready(workspace.begin_owned(&observed, &reads, PATCH));

        // Another writer retargets the link to a file the call never read.
        fs::remove_file(dir.path().join("link.txt")).unwrap();
        std::os::unix::fs::symlink("b.txt", dir.path().join("link.txt")).unwrap();
        assert_eq!(
            owned.write("link.txt", "pwned\n"),
            Err(io(
                "link.txt changed on disk since you last read it; read it again."
            ))
        );
        assert_eq!(text(&workspace, "b.txt"), "never read\n");
        assert_eq!(text(&workspace, "a.txt"), "read\n");

        // Retargeted to a path with nothing behind it, the change is refused too.
        fs::remove_file(dir.path().join("link.txt")).unwrap();
        std::os::unix::fs::symlink("gone.txt", dir.path().join("link.txt")).unwrap();
        assert_eq!(
            owned.write("link.txt", "pwned\n"),
            Err(io(
                "link.txt changed on disk since you last read it; read it again."
            ))
        );
        assert!(!dir.path().join("gone.txt").exists());
        drop(owned);
        no_temporaries(dir.path());
    }

    #[test]
    fn a_symlink_retargeted_between_the_identity_check_and_the_plan_is_refused() {
        // Both targets hold the same bytes, so the read digest alone would accept either.
        let (dir, workspace) = workspace(&[("a.txt", "same\n"), ("b.txt", "same\n")]);
        std::os::unix::fs::symlink("a.txt", dir.path().join("link.txt")).unwrap();
        let observed = ObservedFiles::new();
        let reads = ReadRecord::new();
        let snapshot = workspace.read("link.txt", &ObservedFiles::new()).unwrap();
        reads.record_read(
            &workspace.spelling("link.txt"),
            &workspace.resolve("link.txt").unwrap(),
            snapshot.metadata().content_hash,
        );
        let owned = ready(workspace.begin_owned(&observed, &reads, PATCH));

        // `single`'s steps, with another writer retargeting the link between the
        // identity check and the plan's own resolution.
        let (change, read) = owned
            .read_identity(Change::write("link.txt", "pwned\n"))
            .unwrap();
        fs::remove_file(dir.path().join("link.txt")).unwrap();
        std::os::unix::fs::symlink("b.txt", dir.path().join("link.txt")).unwrap();
        let plan = workspace.plan(std::slice::from_ref(&change)).unwrap();
        assert_eq!(
            owned.plans_the_file_read(&plan, read.as_deref(), &change),
            Err(io(
                "link.txt changed on disk since you last read it; read it again."
            ))
        );
        drop(plan);
        drop(owned);
        assert_eq!(text(&workspace, "a.txt"), "same\n");
        assert_eq!(text(&workspace, "b.txt"), "same\n");
        no_temporaries(dir.path());
    }

    #[test]
    fn a_parent_symlink_retargeted_between_the_plan_and_the_stage_never_moves_the_change() {
        // The parent is a directory symlink inside the workspace. `plan` resolves it to
        // the directory it points at and opens that directory, so an ungated writer that
        // retargets the link after the plan can neither move the change into a
        // directory the plan never checked nor out of the workspace.
        let (dir, workspace) = workspace(&[
            ("planned/keep.txt", "planned\n"),
            ("new/keep.txt", "new\n"),
            ("src/moving.txt", "moving\n"),
        ]);
        symlink(dir.path().join("planned"), dir.path().join("link")).unwrap();
        let observed = ObservedFiles::new();
        let retarget = || {
            fs::remove_file(dir.path().join("link")).unwrap();
            symlink(dir.path().join("new"), dir.path().join("link")).unwrap();
        };

        // A new file written through the link: the change may land in the directory the
        // plan opened or be refused, never in the directory `link` was retargeted to.
        let written = commit_in_two_steps(
            &workspace,
            std::slice::from_ref(&Change::write("link/fresh.txt", "written\n")),
            &observed,
            retarget,
        );
        match written {
            Ok(()) => {
                assert_eq!(
                    fs::read_to_string(dir.path().join("planned/fresh.txt")).unwrap(),
                    "written\n",
                    "the write lands in the directory the plan opened"
                );
                assert_eq!(
                    observed
                        .check_unchanged(&workspace.root().join("planned/fresh.txt"), b"written\n"),
                    Observation::Unchanged,
                    "the observation names the file the planned handle wrote"
                );
                assert_eq!(
                    observed.check_unchanged(&workspace.root().join("new/fresh.txt"), b"written\n"),
                    Observation::NeverObserved,
                    "the directory the link was retargeted to is never observed"
                );
            }
            Err(error) => assert!(
                !dir.path().join("planned/fresh.txt").exists(),
                "a refused change writes nothing: {error:?}"
            ),
        }
        assert!(
            !dir.path().join("new/fresh.txt").exists(),
            "the retargeted directory must never receive the write"
        );
        assert_eq!(text(&workspace, "planned/keep.txt"), "planned\n");
        assert_eq!(text(&workspace, "new/keep.txt"), "new\n");

        // The same through a rename's destination, which the plan resolves and opens
        // the same way: the final rename runs between the two handles the plan holds.
        fs::remove_file(dir.path().join("link")).unwrap();
        symlink(dir.path().join("planned"), dir.path().join("link")).unwrap();
        workspace.read("src/moving.txt", &observed).unwrap();
        let moved = commit_in_two_steps(
            &workspace,
            std::slice::from_ref(&Change::rename("src/moving.txt", "link/landed.txt")),
            &observed,
            retarget,
        );
        match moved {
            Ok(()) => {
                assert_eq!(text(&workspace, "planned/landed.txt"), "moving\n");
                assert!(!dir.path().join("src/moving.txt").exists());
                assert_eq!(
                    observed
                        .check_unchanged(&workspace.root().join("planned/landed.txt"), b"moving\n"),
                    Observation::Unchanged,
                    "the destination observation names the planned file"
                );
                assert_eq!(
                    observed.check_unchanged(&workspace.root().join("new/landed.txt"), b"moving\n"),
                    Observation::NeverObserved,
                    "the directory the link was retargeted to is never observed"
                );
            }
            Err(error) => {
                assert_eq!(
                    text(&workspace, "src/moving.txt"),
                    "moving\n",
                    "a refused change moves nothing: {error:?}"
                );
                assert!(!dir.path().join("planned/landed.txt").exists());
            }
        }
        assert!(
            !dir.path().join("new/landed.txt").exists(),
            "the rename must never follow the retargeted parent"
        );
        assert_eq!(text(&workspace, "new/keep.txt"), "new\n");
        no_temporaries(dir.path());
    }

    #[test]
    fn a_no_replace_rename_without_renameat2_refuses_instead_of_overwriting() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("from.txt"), "from\n").unwrap();
        // The destination is already there, or appears after the staged `exists` check;
        // either way a kernel without `renameat2` can make no atomic refusal.
        fs::write(dir.path().join("to.txt"), "ungated\n").unwrap();
        let fd = rustix::fs::openat(
            rustix::fs::CWD,
            dir.path(),
            rustix::fs::OFlags::RDONLY | rustix::fs::OFlags::DIRECTORY,
            rustix::fs::Mode::empty(),
        )
        .unwrap();

        assert_eq!(
            super::rename_noreplace_on(
                &fd,
                OsStr::new("from.txt"),
                &fd,
                OsStr::new("to.txt"),
                true,
                super::Renameat2::Absent,
            ),
            Err(rustix::io::Errno::EXIST),
            "no renameat2 must refuse, never fall back to a plain rename"
        );

        assert_eq!(
            fs::read_to_string(dir.path().join("to.txt")).unwrap(),
            "ungated\n"
        );
        assert_eq!(
            fs::read_to_string(dir.path().join("from.txt")).unwrap(),
            "from\n"
        );
    }

    #[test]
    fn a_root_swapped_for_a_symlink_is_refused_and_writes_nothing_outside() {
        let outside = tempfile::tempdir().unwrap();
        fs::write(outside.path().join("a.txt"), "outside\n").unwrap();
        let (dir, workspace) = workspace(&[("a.txt", "one\n")]);
        let observed = ObservedFiles::new();
        workspace.read("a.txt", &observed).unwrap();
        let changes = [Change::write("a.txt", "pwned\n")];
        let plan = workspace.plan(&changes).unwrap();

        // The root itself is swapped for a symlink to another directory after the
        // change was validated: the gate must re-open a directory, not the link.
        fs::rename(dir.path(), dir.path().with_extension("real")).unwrap();
        symlink(outside.path(), dir.path()).unwrap();
        let _mutation = workspace.begin_mutation();
        let result = workspace.apply(&plan, &observed, OBSERVED);

        assert_eq!(
            result,
            Err(io("the workspace root is a symlink")),
            "a root that is no longer the resolved directory must refuse"
        );
        assert_eq!(
            fs::read_to_string(outside.path().join("a.txt")).unwrap(),
            "outside\n"
        );
        assert_eq!(
            fs::read_to_string(dir.path().with_extension("real").join("a.txt")).unwrap(),
            "one\n"
        );
    }
}
