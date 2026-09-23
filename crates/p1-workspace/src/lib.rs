//! Workspace confinement, atomic file replacement, the observed-file registry
//! and output bounding, shared by the p1 file tools.
//!
//! Confinement is ALWAYS on. There is no permission switch and no environment
//! variable: every path a file tool touches must resolve inside the workspace
//! root *after* symlink resolution (see [`Workspace::resolve`]). This is an
//! invariant of the tools, not a policy the host may relax.

mod gate;
mod observe;
mod path;
mod text;

use std::path::{Path, PathBuf};

pub use gate::{Mutation, WriteGate};
pub use observe::{Observation, ObservedFiles, StreamingHash};
pub use p1_contracts::tool::ToolFace;
pub use text::{bound_output, write_atomic};

/// Why a workspace path could not be used.
#[derive(Debug, thiserror::Error)]
pub enum WorkspaceError {
    /// The configured root is not a directory.
    #[error("workspace root is not a directory: {}", .0.display())]
    NotADirectory(PathBuf),
    /// The requested path resolves outside the workspace root, directly, by
    /// `..`, or through a symlink.
    #[error("path escapes workspace: {requested}")]
    OutsideWorkspace { requested: String },
    /// A filesystem operation failed while resolving the path.
    #[error("failed to resolve {}: {source}", path.display())]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

/// A confined workspace root. Everything a file tool touches goes through
/// [`Workspace::resolve`] first.
#[derive(Debug, Clone)]
pub struct Workspace {
    root: PathBuf,
    writes: WriteGate,
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
            writes: WriteGate::new(),
        })
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
    /// path inside the root.
    ///
    /// `..` is normalized lexically first, so it can never climb out. An
    /// existing path is returned canonical, which rejects a symlink that points
    /// outside. For a path that does not exist yet, the deepest existing
    /// ancestor is canonicalized and must be inside the root — that ancestor is
    /// the one the eventual read/write would resolve through.
    pub fn resolve(&self, requested: &str) -> Result<PathBuf, WorkspaceError> {
        let candidate = path::lexical_normalize(&path::join_request(&self.root, requested));
        if !candidate.starts_with(&self.root) {
            return Err(WorkspaceError::OutsideWorkspace {
                requested: requested.to_string(),
            });
        }

        let mut ancestor = candidate.as_path();
        loop {
            if ancestor.exists() {
                let canonical = ancestor
                    .canonicalize()
                    .map_err(|source| WorkspaceError::Io {
                        path: candidate.clone(),
                        source,
                    })?;
                if !canonical.starts_with(&self.root) {
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

    /// Render `path` relative to the root with `/` separators, for model-facing
    /// messages. Falls back to the full path when `path` is not inside the root.
    pub fn display(&self, path: &Path) -> String {
        path.strip_prefix(&self.root)
            .unwrap_or(path)
            .to_string_lossy()
            .replace('\\', "/")
    }
}

#[cfg(test)]
mod tests {
    use super::{ToolFace, Workspace, WorkspaceError};

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
