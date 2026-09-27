//! The `workspace-mutation` capability of one agent, over U-mut's owned mutation of
//! `p1-workspace` (`Workspace::begin_owned` → `OwnedMutation`) and the agent's
//! `ObservedFiles`: what the `p1/edit`, `p1/write` and `p1/patch` components change files
//! through.
//!
//! It lives here rather than in `p1-workspace` because that crate stays free of the module
//! runtime and wasmtime: this crate already depends on `p1-workspace`, and the capability
//! trait is the runtime's, so the adapter sits where both meet, as S1's `ReadCapability`
//! does in `p1-tool-read`. It is shared by the three mutating tools; the policy is the
//! host's choice when it builds the service (observed for edit and write, patch-authorized
//! for patch), never the module's.
//!
//! Every check is `p1-workspace`'s: confinement, read-before-mutate under the held gate
//! (the exact native refusal texts), the change's read identity (the tool's [`ReadRecord`],
//! so a file another agent changed after the read is refused whatever the policy), atomic
//! per-file replacement and the observation of what was written. This module only maps its
//! errors onto the frozen `fs-error`, case by case, keeping each message as the native
//! tools print it.

use std::sync::Arc;

use p1_contracts::BoxFuture;
use p1_module_runtime::FsError;
use p1_module_runtime::capabilities::{HeldMutation, MutationService};
use p1_workspace::{
    MutationError, MutationPolicy, ObservedFiles, OwnedMutation, ReadRecord, Workspace,
};

/// One agent's `workspace-mutation` capability, with the policy the host assembled it with
/// and the read record of the tool it serves.
#[derive(Clone)]
pub struct MutationCapability {
    workspace: Workspace,
    observed: ObservedFiles,
    reads: ReadRecord,
    policy: MutationPolicy,
}

impl MutationCapability {
    /// The capability over `workspace` (its write gate is the one the agent's native tools
    /// and every agent sharing it hold) and the agent's own `observed` files, enforcing
    /// `policy`, with a read record of its own: a mutation assembled on its own has no read
    /// side to share one with.
    pub fn new(workspace: Workspace, observed: ObservedFiles, policy: MutationPolicy) -> Self {
        Self::with_reads(workspace, observed, ReadRecord::new(), policy)
    }

    /// The same, carrying `reads` — the record the read side of the same assembly fills —
    /// as the read identity of every change a mutation makes.
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
        }
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
/// and the mutation together (the catalog's rows and `p1_tool_read::tool_services`): the
/// mutation then refuses a target changed after the read that computed the change.
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

impl MutationService for MutationCapability {
    fn begin(&self) -> BoxFuture<'_, Box<dyn HeldMutation>> {
        // `begin_owned` waits without blocking a thread and holds nothing until it has the
        // gate, so the runtime may drop this future on a cancellation.
        let acquire = self
            .workspace
            .begin_owned(&self.observed, &self.reads, self.policy);
        Box::pin(async move { Box::new(Held(Arc::new(acquire.await))) as Box<dyn HeldMutation> })
    }
}

/// The held gate. Shared with the blocking task of a method in progress, so a call dropped
/// mid-write releases the gate only once that write has finished.
struct Held(Arc<OwnedMutation>);

impl Held {
    /// Runs `work` on a blocking thread: the file operations are synchronous and must never
    /// hold the async thread, as in the native tools.
    fn blocking(
        &self,
        work: impl FnOnce(&OwnedMutation) -> Result<(), MutationError> + Send + 'static,
    ) -> BoxFuture<'_, Result<(), FsError>> {
        let mutation = self.0.clone();
        Box::pin(async move {
            tokio::task::spawn_blocking(move || work(&mutation).map_err(fs_error))
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

/// A mutation error as the module sees it. `MutationError` has one variant per `fs-error`
/// case a mutation can produce, so nothing is parsed; `io` carries the native text.
fn fs_error(error: MutationError) -> FsError {
    match error {
        MutationError::OutsideWorkspace { .. } => FsError::OutsideWorkspace,
        MutationError::NotFound { .. } => FsError::NotFound,
        MutationError::WrongKind { .. } => FsError::WrongKind,
        MutationError::AlreadyExists { .. } => FsError::AlreadyExists,
        MutationError::Io(message) => FsError::Io(message),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use p1_workspace::Observation;

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
}
