//! Shared reader for operator-owned assembly configuration, not workspace input.

use std::io::{self, Read};
use std::path::Path;

use p1_contracts::CancellationToken;
use p1_workspace::{CredentialPolicy, ProtectedIndex, Workspace, WorkspaceError, xdg_credentials};

pub(crate) const MAX_CONFIG_BYTES: usize = 1024 * 1024;

pub(crate) struct ConfigReader {
    policy: CredentialPolicy,
}

impl ConfigReader {
    pub(crate) fn from_environment() -> Self {
        let home = std::env::var_os("HOME").map(std::path::PathBuf::from);
        Self {
            policy: CredentialPolicy::new(home.as_deref(), &xdg_credentials()),
        }
    }

    pub(crate) fn read(&self, path: &Path) -> io::Result<String> {
        let refused = || {
            io::Error::new(
                io::ErrorKind::PermissionDenied,
                "configuration reader refuses credential files",
            )
        };
        if self.policy.refuses(path) {
            return Err(refused());
        }
        let cancel = CancellationToken::new();
        let index = ProtectedIndex::build(&self.policy, &cancel)
            .map_err(|_| io::Error::other("credential check cancelled"))?;
        // Reuse the native descriptor walk and NONBLOCK regular-file open. Assembly
        // config is not confined to an agent workspace, so its root is the filesystem.
        let root = Workspace::new("/").map_err(open_error)?;
        let file = root.open_file_at(path).map_err(open_error)?;
        let mut current = ProtectedIndex::build(&self.policy, &cancel)
            .map_err(|_| io::Error::other("credential check cancelled"))?;
        current.retain_identities_of(&index);
        let metadata = file.metadata()?;
        if self.policy.refuses(path) || current.refuses_current_exact(&self.policy, &metadata) {
            return Err(refused());
        }
        // The second walk can race a link of the opened inode into a protected directory it
        // already enumerated. A link raises the inode's count, so a multiply linked file is
        // refused unless the rebuilt index proves itself current and settled (as p1-tool-read).
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
        // The descriptor target catches a symlink retargeted between the path check
        // and open, even if the credential file has only one link.
        #[cfg(target_os = "linux")]
        {
            use std::os::fd::AsRawFd;
            if self
                .policy
                .refuses(Path::new(&format!("/proc/self/fd/{}", file.as_raw_fd())))
            {
                return Err(refused());
            }
        }
        if metadata.len() > MAX_CONFIG_BYTES as u64 {
            return Err(byte_limit());
        }
        read_bounded(file)
    }
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

fn byte_limit() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!("configuration exceeds byte limit of {MAX_CONFIG_BYTES}"),
    )
}

fn read_bounded(reader: impl Read) -> io::Result<String> {
    let mut bytes = Vec::new();
    reader
        .take(MAX_CONFIG_BYTES as u64 + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() > MAX_CONFIG_BYTES {
        return Err(byte_limit());
    }
    String::from_utf8(bytes)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "configuration is not UTF-8"))
}

#[cfg(test)]
mod tests;
