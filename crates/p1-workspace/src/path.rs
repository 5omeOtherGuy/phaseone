//! Lexical path helpers used by [`crate::Workspace::resolve`].
//!
//! These are deliberately filesystem-free: confinement must not depend on a
//! path existing, and lexical normalization is what makes `..` unable to climb.

use std::path::{Component, Path, PathBuf};

/// Absolutize `requested` against `root`. An absolute request is used as-is
/// (and later rejected unless it is inside the root).
pub(crate) fn join_request(root: &Path, requested: &str) -> PathBuf {
    let requested = Path::new(requested);
    if requested.is_absolute() {
        requested.to_path_buf()
    } else {
        root.join(requested)
    }
}

/// Collapse `.` and `..` without touching the filesystem.
pub(crate) fn lexical_normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}
