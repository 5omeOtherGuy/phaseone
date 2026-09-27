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
//! Every file operation under the gate is directory-relative: the canonical parent is
//! opened once by walking down from the root without following any symlink, and the
//! temporary file, the rename and the unlink all happen relative to that directory
//! handle, never following a symlink at the leaf. A parent directory swapped for a
//! symlink between validation and replacement therefore cannot make a mutation land
//! outside the root. A create-only replacement and a rename destination are refused
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
use std::os::unix::fs::PermissionsExt;
use std::path::{Component, PathBuf};

use rustix::fs::{AtFlags, CWD, FileType, Mode, OFlags, RenameFlags};
use rustix::io::Errno;

use crate::gate::Held;
use crate::observe::{Observation, ObservedFiles, hash_of};
use crate::read::{Snapshot, SnapshotMetadata};
use crate::reads::ReadRecord;
use crate::text::temp_name;
use crate::{Workspace, WorkspaceError};

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

    fn single(&self, change: Change) -> Result<(), MutationError> {
        let change = self.read_identity(change);
        let plan = self.workspace.plan(std::slice::from_ref(&change))?;
        self.workspace.apply(&plan, &self.observed, self.policy)
    }

    /// The change with the read identity of what this tool read of its target, if
    /// anything: the gated recheck (the caller already holds the gate) then refuses any
    /// other bytes there, whatever the policy. A target this tool did not read has no
    /// identity and is checked by the policy alone. The path is resolved exactly as the
    /// change's own validation resolves it, so both name one file.
    fn read_identity(&self, change: Change) -> Change {
        match self
            .workspace
            .resolve(change.source_path())
            .ok()
            .and_then(|path| self.reads.recorded(&path))
        {
            Some(hash) => change.computed_from_hash(hash),
            None => change,
        }
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
        let plan = self.plan(changes)?;
        let mutation = self.begin_mutation();
        let result = self.apply(&plan, observed, policy);
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

    /// Validate every path of `changes` beneath the workspace, before the gate.
    fn plan<'c>(&self, changes: &'c [Change]) -> Result<Vec<Planned<'c>>, MutationError> {
        let mut plan = Vec::with_capacity(changes.len());
        let mut touched: Vec<PathBuf> = Vec::new();
        for change in changes {
            let planned = match &change.op {
                Op::Write { path, contents } | Op::Create { path, contents } => Planned::Write {
                    target: self.target(path)?,
                    contents,
                    create_only: matches!(change.op, Op::Create { .. }),
                    computed_from: change.computed_from,
                },
                Op::Remove { path } => Planned::Remove {
                    target: self.target(path)?,
                    computed_from: change.computed_from,
                },
                Op::Rename { from, to } => Planned::Rename {
                    from: self.target(from)?,
                    to: self.target(to)?,
                    computed_from: change.computed_from,
                },
            };
            // Every check runs before the first replacement, so two changes to one
            // path would each be checked against bytes the other is about to replace.
            // The key is the canonical path, not the resolved spelling: a directory
            // symlink inside the workspace makes `src/new.txt` and `link/new.txt` one
            // file that a not-yet-existing leaf would otherwise spell twice.
            for target in planned.targets() {
                let key = self.canonical_key(target)?;
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
        Ok(Target {
            requested: requested.to_string(),
            path,
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
        let root = open_root(&self.root)?;
        let recheck = Recheck { observed, policy };

        // Stage everything first: a refusal or failure here drops the staged files,
        // which removes their temporaries, and no target has been touched.
        let mut staged = Vec::with_capacity(plan.len());
        for planned in plan {
            staged.push(self.stage(&root, planned, &recheck)?);
        }

        for step in &mut staged {
            match step {
                Staged::Replace {
                    file,
                    target,
                    contents,
                    create_only,
                } => {
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
                    observed.record(&target.path, contents);
                }
                Staged::Remove { dir, leaf, target } => {
                    rustix::fs::unlinkat(&*dir, leaf.as_os_str(), AtFlags::empty()).map_err(
                        |error| {
                            MutationError::Io(format!(
                                "failed to delete {}: {error}",
                                target.display
                            ))
                        },
                    )?;
                    sync_directory(dir);
                }
                Staged::Rename {
                    from_dir,
                    from_leaf,
                    from,
                    to_dir,
                    to_leaf,
                    to,
                    bytes,
                } => {
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
                    // As patch's move: the destination now holds bytes this agent saw.
                    observed.record(&to.path, bytes);
                }
            }
        }
        Ok(())
    }

    fn stage<'p>(
        &self,
        root: &OwnedFd,
        planned: &'p Planned<'_>,
        recheck: &Recheck<'_>,
    ) -> Result<Staged<'p>, MutationError> {
        match planned {
            Planned::Write {
                target,
                contents,
                create_only,
                computed_from,
            } => {
                let dir = self.open_parent(root, target, true)?;
                let leaf = leaf_of(target);
                if *create_only && exists(&dir, leaf, target)? {
                    return Err(MutationError::AlreadyExists {
                        requested: target.requested.clone(),
                    });
                }
                let existing = inspect(&dir, leaf, target)?;
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
                })
            }
            Planned::Remove {
                target,
                computed_from,
            } => {
                let dir = self.open_parent(root, target, false)?;
                let leaf = leaf_of(target);
                let Some(existing) = inspect(&dir, leaf, target)? else {
                    return Err(MutationError::NotFound {
                        requested: target.requested.clone(),
                    });
                };
                recheck.check(target, &existing.bytes, *computed_from)?;
                Ok(Staged::Remove {
                    dir,
                    leaf: leaf.to_os_string(),
                    target,
                })
            }
            Planned::Rename {
                from,
                to,
                computed_from,
            } => {
                let from_dir = self.open_parent(root, from, false)?;
                let from_leaf = leaf_of(from);
                let Some(existing) = inspect(&from_dir, from_leaf, from)? else {
                    return Err(MutationError::NotFound {
                        requested: from.requested.clone(),
                    });
                };
                recheck.check(from, &existing.bytes, *computed_from)?;
                let to_dir = self.open_parent(root, to, true)?;
                let to_leaf = leaf_of(to);
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
                })
            }
        }
    }

    /// The canonical path two spellings of `target` share: its deepest existing
    /// ancestor canonicalized, with the missing parent components and the leaf
    /// re-joined. `plan` refuses a commit that names this path twice, so a directory
    /// symlink inside the workspace (`link -> src`) cannot make `src/new.txt` and
    /// `link/new.txt` slip through the one-change-per-path check as two files.
    fn canonical_key(&self, target: &Target) -> Result<PathBuf, MutationError> {
        let (mut key, missing) = self.ancestor_and_missing(target)?;
        for name in missing.into_iter().rev() {
            key.push(name);
        }
        key.push(leaf_of(target));
        Ok(key)
    }

    /// The deepest existing ancestor of `target`'s parent, canonicalized and checked
    /// beneath the root, with the names of the missing components below it (leaf-most
    /// first). `canonical_key` rejoins them to name the file; `open_parent` creates
    /// them and walks the canonical ancestor.
    ///
    /// The canonical form has no symlink in it (a symlink that stays inside is fine,
    /// as for [`Workspace::resolve`]), so `open_parent`'s `O_NOFOLLOW` walk succeeds
    /// unless a component was swapped since, and then the walk refuses instead of
    /// following.
    fn ancestor_and_missing<'t>(
        &self,
        target: &'t Target,
    ) -> Result<(PathBuf, Vec<&'t OsStr>), MutationError> {
        let outside = || MutationError::OutsideWorkspace {
            requested: target.requested.clone(),
        };
        let mut existing = target.path.parent().ok_or_else(outside)?;
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
                        "{} could not be resolved: {error}",
                        target.display
                    )));
                }
            }
        }
        let canonical = existing.canonicalize().map_err(|error| {
            MutationError::Io(format!("{} could not be resolved: {error}", target.display))
        })?;
        if !canonical.starts_with(&self.root) {
            return Err(outside());
        }
        Ok((canonical, missing))
    }

    /// Open `target`'s parent directory relative to the open `root`, without following
    /// a symlink on the way, creating missing directories when `create` is set.
    fn open_parent(
        &self,
        root: &OwnedFd,
        target: &Target,
        create: bool,
    ) -> Result<OwnedFd, MutationError> {
        let outside = || MutationError::OutsideWorkspace {
            requested: target.requested.clone(),
        };
        let (canonical, missing) = self.ancestor_and_missing(target)?;
        let relative = canonical.strip_prefix(&self.root).map_err(|_| outside())?;

        let mut dir = root.try_clone().map_err(|error| {
            MutationError::Io(format!("the workspace could not be opened: {error}"))
        })?;
        for component in relative.components() {
            let Component::Normal(name) = component else {
                return Err(outside());
            };
            dir = open_directory(&dir, name, target)?;
        }

        if !missing.is_empty() && !create {
            return Err(MutationError::NotFound {
                requested: target.requested.clone(),
            });
        }
        for name in missing.into_iter().rev() {
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
}

/// A validated path: resolved beneath the root, with what the caller asked for and
/// what the model is shown.
#[derive(Debug)]
struct Target {
    requested: String,
    path: PathBuf,
    display: String,
}

/// One validated change.
enum Planned<'c> {
    Write {
        target: Target,
        contents: &'c [u8],
        create_only: bool,
        computed_from: Option<u64>,
    },
    Remove {
        target: Target,
        computed_from: Option<u64>,
    },
    Rename {
        from: Target,
        to: Target,
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
}

/// One checked change, ready to apply.
enum Staged<'p> {
    Replace {
        file: StagedFile,
        target: &'p Target,
        contents: &'p [u8],
        create_only: bool,
    },
    Remove {
        dir: OwnedFd,
        leaf: OsString,
        target: &'p Target,
    },
    Rename {
        from_dir: OwnedFd,
        from_leaf: OsString,
        from: &'p Target,
        to_dir: OwnedFd,
        to_leaf: OsString,
        to: &'p Target,
        bytes: Vec<u8>,
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
        match self.observed.check_unchanged(&target.path, current) {
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

/// A regular file's current bytes and permission bits.
struct Existing {
    bytes: Vec<u8>,
    mode: u32,
}

/// A staged temporary beside its target, removed on drop unless it replaced it.
struct StagedFile {
    dir: OwnedFd,
    leaf: OsString,
    temp: Option<OsString>,
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
        let staged = Self {
            dir,
            leaf: leaf.to_os_string(),
            temp: Some(temp),
        };
        let mut file = File::from(fd);
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
}

impl Drop for StagedFile {
    fn drop(&mut self) {
        if let Some(temp) = self.temp.take() {
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
/// component. The root is re-opened by path under the gate, so a root swapped for a
/// symlink after the workspace was resolved must refuse instead of re-opening wherever
/// the link points; `O_DIRECTORY` alone would follow it.
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
    let mut file = File::from(fd);
    let metadata = file.metadata().map_err(read_error)?;
    if !metadata.is_file() {
        return Err(wrong_kind());
    }
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes).map_err(read_error)?;
    Ok(Some(Existing {
        bytes,
        mode: metadata.permissions().mode() & 0o7777,
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
        while workspace.write_gate().sync_waiters() == 0 {
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
