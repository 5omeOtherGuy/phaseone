//! Workspace confinement, atomic file replacement, the observed-file registry
//! and output bounding, shared by the p1 file tools.
//!
//! Confinement is ALWAYS on. There is no permission switch and no environment
//! variable: every path a file tool touches must resolve inside the workspace
//! root *after* symlink resolution (see [`Workspace::resolve`]). This is an
//! invariant of the tools, not a policy the host may relax.
//!
//! The file policy that is not confinement lives here too: the credential files
//! every file tool refuses before confinement ([`refuse_credentials`], issue
//! #142) and the model-facing texts that refusal and a failed read carry, so a
//! component's capability service and the native tool refuse the same paths with
//! the same wording.

mod commit;
mod gate;
mod listing;
mod observe;
mod path;
mod policy;
mod prompt_data;
mod read;
mod reads;
mod text;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

pub use commit::{Change, MutationError, MutationPolicy, OwnedMutation};
pub use gate::{Mutation, WriteGate};
pub use listing::{LISTING_SCAN_CEILING, ListedEntry, ListingError, ListingPage, listing_cursor};
pub use observe::{Observation, ObservedFiles, StreamingHash};
pub use p1_contracts::tool::ToolFace;
pub use policy::{
    CredentialPolicy, IndexCancelled, ProtectedIndex, could_not_be_read, credential_refusal,
    refuse_credentials, refuses_credentials, xdg_credentials,
};
pub use prompt_data::open_prompt_file;
pub use read::{CheckedPath, DirEntry, FileKind, Snapshot, SnapshotMetadata, Stat};
pub use reads::ReadRecord;
pub use text::{bound_output, write_atomic};

/// Why a workspace path could not be used.
#[derive(Debug, thiserror::Error)]
pub enum WorkspaceError {
    /// The workspace root, or a path a caller asked to list, is not a directory.
    #[error("not a directory: {}", .0.display())]
    NotADirectory(PathBuf),
    /// The requested path resolves inside the workspace but does not exist.
    #[error("no such path in the workspace: {requested}")]
    NotFound { requested: String },
    /// The requested path resolves outside the workspace root, directly, by
    /// `..`, or through a symlink.
    #[error("path escapes workspace: {requested}")]
    OutsideWorkspace { requested: String },
    /// A filesystem operation on the path failed while resolving, reading or
    /// listing it.
    #[error("workspace I/O failed for {}: {source}", path.display())]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

/// A count of committed mutations under the workspace root (ADR-0122 point 5),
/// shared by every agent assembled from one catalog and by the host's activity
/// log. A write under the workspace root bumps it; a write under the scratch root
/// does not. The activity log compares it before and after a `WritesFiles` call to
/// decide whether that call changed the work.
#[derive(Debug, Clone, Default)]
pub struct WorkspaceMutations(Arc<AtomicU64>);

impl WorkspaceMutations {
    /// A fresh counter at zero.
    pub fn new() -> Self {
        Self::default()
    }

    /// The number of committed mutations under the workspace root so far.
    pub fn count(&self) -> u64 {
        self.0.load(Ordering::SeqCst)
    }

    /// A committed mutation under the workspace root.
    pub(crate) fn bump(&self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

/// A confined workspace root. Everything a file tool touches goes through
/// [`Workspace::resolve`] first. An optional second root (ADR-0122), the run's
/// scratch directory, is confined the same way and owned by the same checks.
#[derive(Debug, Clone)]
pub struct Workspace {
    root: PathBuf,
    /// The run's scratch directory, canonical, when one exists (ADR-0122 point 2).
    scratch: Option<PathBuf>,
    writes: WriteGate,
    mutations: WorkspaceMutations,
    credential_home: Option<PathBuf>,
    credential_paths: Vec<PathBuf>,
}

impl Workspace {
    /// Canonicalize `root` and require that it is a directory. Storing the
    /// canonical form is what makes the `starts_with` confinement checks sound.
    pub fn new(root: impl AsRef<Path>) -> Result<Self, WorkspaceError> {
        let root = root.as_ref();
        let canonical = root.canonicalize().map_err(|source| WorkspaceError::Io {
            path: root.to_path_buf(),
            source,
        })?;
        if !canonical.is_dir() {
            return Err(WorkspaceError::NotADirectory(canonical));
        }
        Ok(Self {
            root: canonical,
            scratch: None,
            writes: WriteGate::new(),
            mutations: WorkspaceMutations::new(),
            credential_home: std::env::var_os("HOME").map(PathBuf::from),
            credential_paths: xdg_credentials(),
        })
    }

    /// Add the run's scratch root (ADR-0122 point 2): a second confined root
    /// outside the workspace. `scratch` is canonicalized and must be a directory.
    pub fn with_scratch(mut self, scratch: impl AsRef<Path>) -> Result<Self, WorkspaceError> {
        let scratch = scratch.as_ref();
        let canonical = scratch
            .canonicalize()
            .map_err(|source| WorkspaceError::Io {
                path: scratch.to_path_buf(),
                source,
            })?;
        if !canonical.is_dir() {
            return Err(WorkspaceError::NotADirectory(canonical));
        }
        self.scratch = Some(canonical);
        Ok(self)
    }

    /// Share a mutation counter with the host and the other agents of the run
    /// (ADR-0122 point 5).
    pub fn with_mutations(mut self, mutations: WorkspaceMutations) -> Self {
        self.mutations = mutations;
        self
    }

    /// The counter committed mutations under the workspace root move.
    pub fn mutations(&self) -> &WorkspaceMutations {
        &self.mutations
    }

    /// The run's scratch root, when one exists.
    pub fn scratch_root(&self) -> Option<&Path> {
        self.scratch.as_deref()
    }

    /// Use the agent's configured home for mutation credential refusal.
    pub fn with_credential_home(mut self, home: Option<PathBuf>) -> Self {
        self.credential_home = home;
        self
    }

    /// Use the host's resolved credential paths instead of process-environment defaults.
    /// Keep path spellings: every request and mutation stage re-canonicalizes them to
    /// follow a retargeted credential-directory symlink before checking opened objects.
    pub fn with_credential_paths(mut self, paths: Vec<PathBuf>) -> Self {
        self.credential_paths = paths;
        self
    }

    /// Explicit credential paths shared by file services and mutation checks.
    pub fn credential_paths(&self) -> &[PathBuf] {
        &self.credential_paths
    }

    /// Refuse a credential mutation before a native reference reads or plans it.
    pub fn refuse_mutation_credentials(&self, requested: &str) -> Result<(), String> {
        let policy = CredentialPolicy::new(self.credential_home.as_deref(), &self.credential_paths);
        policy.refuse(self, requested)?;
        let candidate = self.spelling(requested);
        let resolved = self.resolve(requested).map_err(|error| error.to_string())?;
        if policy.refuses(&resolved) {
            return Err(credential_refusal(&self.display(&candidate)));
        }
        if let Ok(file) = self.open_file_at(&resolved) {
            let index = ProtectedIndex::build(&policy, &p1_contracts::CancellationToken::new())
                .map_err(|_| "credential policy check cancelled".to_string())?;
            let metadata = file.metadata().map_err(|error| error.to_string())?;
            if policy.refuses_opened(&resolved, &file)
                || index.refuses_metadata(&metadata)
                || index.refuses_current_exact(&policy, &metadata)
            {
                return Err(credential_refusal(&self.display(&candidate)));
            }
        }
        Ok(())
    }

    /// Configured credential home shared by native parity tools and components.
    pub fn credential_home(&self) -> Option<&Path> {
        self.credential_home.as_deref()
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Serialize this workspace's mutations with everyone else holding `gate` —
    /// the other agents of the same process. A fresh workspace has a gate of its
    /// own, which is right for a single agent.
    pub fn with_write_gate(mut self, gate: WriteGate) -> Self {
        self.writes = gate;
        self
    }

    pub fn write_gate(&self) -> &WriteGate {
        &self.writes
    }

    /// Take the write gate for one mutation: from reading the file's current
    /// contents until the write is recorded. See [`WriteGate`].
    pub fn begin_mutation(&self) -> Mutation<'_> {
        self.writes.begin_mutation()
    }

    /// Resolve a model-supplied path (workspace-relative, or absolute) to a
    /// path inside one of the roots: the workspace, or the scratch root when the
    /// path is under it (ADR-0122 point 2).
    ///
    /// `..` is normalized lexically first, so it can never climb out. An
    /// existing path is returned canonical, which rejects a symlink that points
    /// outside. For a path that does not exist yet, the deepest existing
    /// ancestor is canonicalized and must be inside the root — that ancestor is
    /// the one the eventual read/write would resolve through. A path under
    /// neither root is refused exactly as before.
    pub fn resolve(&self, requested: &str) -> Result<PathBuf, WorkspaceError> {
        let candidate = self.spelling(requested);
        let Some(root) = self.owning_root(&candidate).map(Path::to_path_buf) else {
            return Err(WorkspaceError::OutsideWorkspace {
                requested: requested.to_string(),
            });
        };

        let mut ancestor = candidate.as_path();
        loop {
            if ancestor.exists() {
                let canonical = ancestor
                    .canonicalize()
                    .map_err(|source| WorkspaceError::Io {
                        path: candidate.clone(),
                        source,
                    })?;
                if !canonical.starts_with(&root) {
                    return Err(WorkspaceError::OutsideWorkspace {
                        requested: requested.to_string(),
                    });
                }
                break;
            }
            match ancestor.parent() {
                Some(parent) => ancestor = parent,
                None => {
                    return Err(WorkspaceError::OutsideWorkspace {
                        requested: requested.to_string(),
                    });
                }
            }
        }

        if candidate.exists() {
            return candidate
                .canonicalize()
                .map_err(|source| WorkspaceError::Io {
                    path: candidate,
                    source,
                });
        }
        Ok(candidate)
    }

    /// The root that owns `candidate`, if any: the scratch root when the path is
    /// under it (it is the more specific of the two when the scratch directory
    /// lies inside the workspace), otherwise the workspace root. `None` when the
    /// path is under neither, which every caller refuses as an escape.
    pub(crate) fn owning_root(&self, candidate: &Path) -> Option<&Path> {
        if let Some(scratch) = &self.scratch
            && candidate.starts_with(scratch)
        {
            return Some(scratch);
        }
        candidate.starts_with(&self.root).then_some(&self.root)
    }

    /// `requested` joined to the root and normalized lexically, before any symlink is
    /// resolved: the one name every spelling of a request shares, so a read record can
    /// tell a path that now resolves to another file than the one it read.
    ///
    /// A relative request joins under the workspace root; an absolute request (a
    /// `{{scratch}}` path) is kept as it is, as before.
    pub fn spelling(&self, requested: &str) -> PathBuf {
        path::lexical_normalize(&path::join_request(&self.root, requested))
    }

    /// Render `path` for model-facing messages: relative to the workspace root
    /// when it is under it, the full path otherwise (a scratch path keeps its
    /// `{{scratch}}` spelling rather than becoming ambiguous with a workspace
    /// path when the scratch directory lies inside the workspace).
    pub fn display(&self, path: &Path) -> String {
        let scratch = self
            .scratch
            .as_ref()
            .is_some_and(|scratch| path.starts_with(scratch));
        let relative = match path.strip_prefix(&self.root) {
            Ok(relative) if !scratch => relative,
            _ => path,
        };
        relative.to_string_lossy().replace('\\', "/")
    }
}

#[cfg(test)]
mod tests {
    use super::{Change, MutationPolicy, ObservedFiles, ToolFace, Workspace, WorkspaceError};

    #[test]
    fn new_canonicalizes_the_root() {
        let dir = tempfile::tempdir().unwrap();
        let nested = dir.path().join("a/./b");
        std::fs::create_dir_all(&nested).unwrap();

        let workspace = Workspace::new(&nested).unwrap();

        assert_eq!(workspace.root(), nested.canonicalize().unwrap());
    }

    #[test]
    fn new_rejects_a_missing_root_and_a_file_root() {
        let dir = tempfile::tempdir().unwrap();
        assert!(matches!(
            Workspace::new(dir.path().join("missing")),
            Err(WorkspaceError::Io { .. })
        ));

        let file = dir.path().join("file.txt");
        std::fs::write(&file, b"x").unwrap();
        assert!(matches!(
            Workspace::new(&file),
            Err(WorkspaceError::NotADirectory(_))
        ));
    }

    #[cfg(unix)]
    #[test]
    fn explicit_credential_paths_recanonicalize_for_reads_and_mutations() {
        use std::os::unix::fs::symlink;
        let dir = tempfile::tempdir().unwrap();
        for name in ["old", "new"] {
            std::fs::create_dir_all(dir.path().join(name)).unwrap();
            std::fs::write(dir.path().join(name).join("auth.json"), b"fixture").unwrap();
        }
        let link = dir.path().join("login");
        symlink(dir.path().join("old"), &link).unwrap();
        let workspace = Workspace::new(dir.path())
            .unwrap()
            .with_credential_home(None)
            .with_credential_paths(vec![link.join("auth.json")]);
        std::fs::remove_file(&link).unwrap();
        symlink(dir.path().join("new"), &link).unwrap();
        std::fs::hard_link(dir.path().join("new/auth.json"), dir.path().join("alias")).unwrap();
        for path in ["login/auth.json", "alias"] {
            assert!(
                workspace
                    .refuse_mutation_credentials(path)
                    .unwrap_err()
                    .contains("refuses credential files")
            );
            let error = workspace
                .read_unobserved_checked(path, &p1_contracts::CancellationToken::new())
                .unwrap_err();
            assert!(error.to_string().contains("refuses credential files"));
            let error = workspace
                .commit(
                    &[Change::write(path, b"replacement".to_vec())],
                    &ObservedFiles::new(),
                    MutationPolicy::PatchAuthorized,
                )
                .unwrap_err();
            assert!(error.to_string().contains("refuses credential files"));
        }
        assert_eq!(
            std::fs::read(dir.path().join("new/auth.json")).unwrap(),
            b"fixture"
        );
    }

    #[test]
    fn display_is_root_relative_with_forward_slashes() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(dir.path().join("src/a.rs"), b"x").unwrap();
        let workspace = Workspace::new(dir.path()).unwrap();

        let resolved = workspace.resolve("src/a.rs").unwrap();

        assert_eq!(workspace.display(&resolved), "src/a.rs");
    }

    #[cfg(unix)]
    #[test]
    fn resolve_confines_the_seven_spec_examples() {
        use std::os::unix::fs::symlink;

        let workspace_dir = tempfile::tempdir().unwrap();
        let outside_dir = tempfile::tempdir().unwrap();
        let root = workspace_dir.path();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/a.rs"), b"fn main() {}\n").unwrap();
        std::fs::write(outside_dir.path().join("secret.txt"), b"secret").unwrap();
        symlink(outside_dir.path(), root.join("link")).unwrap();
        symlink(outside_dir.path(), root.join("link2")).unwrap();

        let workspace = Workspace::new(root).unwrap();

        // 1. A relative path inside the workspace is accepted.
        assert_eq!(
            workspace.resolve("src/a.rs").unwrap(),
            workspace.root().join("src/a.rs")
        );
        // 2. An absolute path inside the workspace is accepted.
        let absolute_inside = workspace.root().join("src/a.rs");
        assert_eq!(
            workspace
                .resolve(absolute_inside.to_str().unwrap())
                .unwrap(),
            absolute_inside
        );
        // 3. A `..` escape is rejected.
        assert!(matches!(
            workspace.resolve("../x"),
            Err(WorkspaceError::OutsideWorkspace { .. })
        ));
        // 4. An absolute path outside the workspace is rejected.
        let absolute_outside = outside_dir.path().join("secret.txt");
        assert!(matches!(
            workspace.resolve(absolute_outside.to_str().unwrap()),
            Err(WorkspaceError::OutsideWorkspace { .. })
        ));
        // 5. A symlink whose target is outside is rejected for an existing leaf.
        assert!(matches!(
            workspace.resolve("link/secret.txt"),
            Err(WorkspaceError::OutsideWorkspace { .. })
        ));
        // 6. A path where nothing exists yet is accepted when its deepest
        //    existing ancestor is inside the workspace.
        assert_eq!(
            workspace.resolve("new/dir/file.txt").unwrap(),
            workspace.root().join("new/dir/file.txt")
        );
        // 7. A not-yet-existing leaf under a symlink that points outside is
        //    rejected: the write would resolve through the symlink.
        assert!(matches!(
            workspace.resolve("link2/new.txt"),
            Err(WorkspaceError::OutsideWorkspace { .. })
        ));
    }

    #[cfg(unix)]
    #[test]
    fn resolve_accepts_a_symlinked_ancestor_that_stays_inside() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(dir.path().join("src/a.rs"), b"x").unwrap();
        symlink(dir.path().join("src"), dir.path().join("link")).unwrap();
        let workspace = Workspace::new(dir.path()).unwrap();

        // The link resolves inside, so the canonical path is the real file.
        assert_eq!(
            workspace.resolve("link/a.rs").unwrap(),
            workspace.root().join("src/a.rs")
        );
        // A not-yet-existing leaf through the same link stays inside.
        assert_eq!(
            workspace.resolve("link/new.txt").unwrap(),
            workspace.root().join("link/new.txt")
        );
    }

    #[test]
    fn tool_face_carries_a_name_and_description() {
        let face = ToolFace::new("read", "Read a file.");
        assert_eq!(face.name, "read");
        assert_eq!(face.description, "Read a file.");
    }
}
