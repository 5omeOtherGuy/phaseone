//! Protected regular-file opens for operator prompt data outside the workspace.
use std::fs::File;
use std::io;
use std::path::Path;

use p1_contracts::CancellationToken;

use crate::{CredentialPolicy, ProtectedIndex, Workspace, WorkspaceError};

/// Opens without reading bytes. Credential paths, symlink targets and hard-link
/// identities are refused with the same checks configuration readers use.
pub fn open_prompt_file(path: &Path, policy: &CredentialPolicy) -> io::Result<File> {
    let refused = || {
        io::Error::new(
            io::ErrorKind::PermissionDenied,
            "configuration reader refuses credential files",
        )
    };
    if policy.refuses(path) {
        return Err(refused());
    }
    let cancel = CancellationToken::new();
    let index = ProtectedIndex::build(policy, &cancel)
        .map_err(|_| io::Error::other("credential check cancelled"))?;
    let root = Workspace::new("/").map_err(open_error)?;
    let file = root.open_file_at(path).map_err(open_error)?;
    let mut current = ProtectedIndex::build(policy, &cancel)
        .map_err(|_| io::Error::other("credential check cancelled"))?;
    current.retain_identities_of(&index);
    let metadata = file.metadata()?;
    if policy.refuses(path) || current.refuses_current_exact(policy, &metadata) {
        return Err(refused());
    }
    // A link added after the protected-directory walk must not expose its inode.
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if metadata.nlink() > 1
            && !current
                .still_current(&cancel)
                .map_err(|_| io::Error::other("credential check cancelled"))?
        {
            return Err(refused());
        }
    }
    #[cfg(target_os = "linux")]
    {
        use std::os::fd::AsRawFd;
        if policy.refuses(Path::new(&format!("/proc/self/fd/{}", file.as_raw_fd()))) {
            return Err(refused());
        }
    }
    Ok(file)
}

fn open_error(error: WorkspaceError) -> io::Error {
    match error {
        WorkspaceError::Io { source, .. } => source,
        WorkspaceError::NotFound { .. } => {
            io::Error::new(io::ErrorKind::NotFound, "configuration file not found")
        }
        _ => io::Error::new(
            io::ErrorKind::InvalidInput,
            "configuration input must be a regular file",
        ),
    }
}
