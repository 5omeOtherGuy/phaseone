//! The session-process launcher: how a front end starts one isolated session for a
//! workspace (ADR-0167, amending ADR-0156 by reference).
//!
//! A front end that serves several workspaces from one process (the ACP router, a GUI
//! server) keeps each session in its own process. It asks a [`SessionLauncher`] for
//! one and gets back the process's byte pipes and its exit. Nothing here names a wire
//! protocol or a front end: the bytes mean whatever the front end and the session
//! process agree on, and how the process is started is an adapter's business.

use std::path::Path;

use tokio::io::{AsyncRead, AsyncWrite};

use crate::BoxFuture;

/// What a launched session writes (its standard output).
pub type SessionReader = Box<dyn AsyncRead + Unpin + Send>;
/// What a launched session reads (its standard input).
pub type SessionWriter = Box<dyn AsyncWrite + Unpin + Send>;

/// One started session: the pipe pair it speaks over, and its exit.
pub struct Launched {
    /// What the session writes.
    pub reader: SessionReader,
    /// What the session reads.
    pub writer: SessionWriter,
    /// Resolves once the session has exited.
    pub exited: BoxFuture<'static, ()>,
}

/// Starts the session that serves one workspace.
pub trait SessionLauncher: Send + Sync {
    /// Start one isolated session whose workspace is `workspace`.
    fn launch(&self, workspace: &Path) -> std::io::Result<Launched>;
}
