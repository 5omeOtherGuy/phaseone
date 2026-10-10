//! Shared reader for operator-owned assembly configuration, not workspace input.

use std::io::{self, Read};
use std::path::Path;

use p1_workspace::{CredentialPolicy, open_prompt_file, xdg_credentials};

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

    pub(crate) fn for_home(home: Option<&Path>, credential_paths: &[std::path::PathBuf]) -> Self {
        Self {
            policy: CredentialPolicy::new(home, credential_paths),
        }
    }

    pub(crate) fn read(&self, path: &Path) -> io::Result<String> {
        self.read_file(path, None).map(|(text, _)| text)
    }

    /// Read a bounded prefix of prompt data, keeping its original byte size. The
    /// same credential and descriptor checks apply even outside the workspace.
    pub(crate) fn read_prefix(&self, path: &Path, limit: usize) -> io::Result<(String, u64)> {
        self.read_file(path, Some(limit))
    }

    fn read_file(&self, path: &Path, prefix: Option<usize>) -> io::Result<(String, u64)> {
        let file = open_prompt_file(path, &self.policy)?;
        let metadata = file.metadata()?;
        if prefix.is_none() && metadata.len() > MAX_CONFIG_BYTES as u64 {
            return Err(byte_limit());
        }
        let text = match prefix {
            None => read_bounded(file)?,
            Some(limit) => {
                let mut bytes = Vec::new();
                file.take(limit as u64).read_to_end(&mut bytes)?;
                match String::from_utf8(bytes) {
                    Ok(text) => text,
                    Err(error)
                        if error.utf8_error().error_len().is_none()
                            && metadata.len() > limit as u64 =>
                    {
                        let end = error.utf8_error().valid_up_to();
                        String::from_utf8(error.into_bytes()[..end].to_vec())
                            .expect("validated UTF-8 prefix")
                    }
                    Err(_) => {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            "prompt data is not UTF-8",
                        ));
                    }
                }
            }
        };
        Ok((text, metadata.len()))
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
