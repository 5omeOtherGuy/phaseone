//! The `workspace` capability of the `p1/search` component: `list-files` and `search` over
//! this crate's native serial walk ([`crate::list_files`], [`crate::search`]), and `stat` and
//! `read` over `p1-workspace` as the native `grep` reads.
//!
//! Search is granted no `snapshot` (its package manifest), and this service reads with
//! `Workspace::read_unobserved`: nothing a search reads is recorded as an observation, so a
//! search can never give an agent the permission an edit needs.

use std::sync::Arc;

use p1_contracts::{BoxFuture, CancellationToken};
use p1_module_runtime::capabilities::{
    FileMatches, SearchLine, SearchQuery, SearchResult, WorkspaceService,
};
use p1_module_runtime::{EntryKind, FsError, Services, WorkspaceEntry};
use p1_tool_search_logic::exec as logic;
use p1_workspace::{FileKind, Workspace};

/// One agent's `workspace` capability as the search component is granted it.
#[derive(Clone)]
pub struct SearchCapability {
    workspace: Workspace,
}

impl SearchCapability {
    /// The capability over `workspace`.
    pub fn new(workspace: Workspace) -> Self {
        Self { workspace }
    }

    /// Runs `work` on a blocking thread: the walk and the reads are synchronous and must
    /// never hold the async thread, as in the native tool. `work` gets a token that is
    /// cancelled when the returned future is dropped — the runtime drops it as soon as the
    /// call is cancelled — so the walk stops at the next file instead of running on
    /// unobserved.
    fn blocking<T: Send + 'static>(
        &self,
        work: impl FnOnce(&Workspace, &CancellationToken) -> Result<T, FsError> + Send + 'static,
    ) -> BoxFuture<'_, Result<T, FsError>> {
        let workspace = self.workspace.clone();
        Box::pin(async move {
            let cancel = CancellationToken::new();
            let _stop_on_drop = cancel.clone().drop_guard();
            tokio::task::spawn_blocking(move || work(&workspace, &cancel))
                .await
                .unwrap_or_else(|error| Err(FsError::Io(format!("search failed: {error}"))))
        })
    }
}

/// The services of the `p1/search` component over one agent's workspace: `workspace` only.
pub fn search_services(workspace: Workspace) -> Services {
    Services {
        workspace: Some(Arc::new(SearchCapability::new(workspace))),
        ..Services::default()
    }
}

impl WorkspaceService for SearchCapability {
    fn stat(&self, path: String) -> BoxFuture<'_, Result<WorkspaceEntry, FsError>> {
        self.blocking(move |workspace, _| {
            let checked = workspace.check_path(&path).map_err(workspace_error)?;
            let stat = workspace.stat(&path).map_err(workspace_error)?;
            // The kinds the native `grep` gives its logic, so both render the same text.
            let kind = match stat.kind {
                FileKind::File => EntryKind::File,
                FileKind::Directory => EntryKind::Directory,
                _ => EntryKind::Other,
            };
            Ok(WorkspaceEntry {
                path: checked.display().to_owned(),
                kind,
                size: if kind == EntryKind::File {
                    stat.size
                } else {
                    0
                },
            })
        })
    }

    fn read(
        &self,
        path: String,
        offset: u64,
        length: u64,
    ) -> BoxFuture<'_, Result<Vec<u8>, FsError>> {
        self.blocking(move |workspace, _| {
            let snapshot = workspace.read_unobserved(&path).map_err(workspace_error)?;
            let offset = usize::try_from(offset).unwrap_or(usize::MAX);
            let length = usize::try_from(length).unwrap_or(usize::MAX);
            Ok(snapshot.read(offset, length).to_vec())
        })
    }

    fn list_files(
        &self,
        path: String,
        glob: Option<String>,
    ) -> BoxFuture<'_, Result<Vec<String>, FsError>> {
        self.blocking(move |workspace, cancel| {
            crate::list_files(workspace, &path, glob.as_deref(), cancel).map_err(from_logic)
        })
    }

    fn search(&self, query: SearchQuery) -> BoxFuture<'_, Result<SearchResult, FsError>> {
        self.blocking(move |workspace, cancel| {
            let query = logic::SearchQuery {
                pattern: query.pattern,
                path: query.path,
                glob: query.glob,
                case_insensitive: query.case_insensitive,
                context: query.context,
                max_lines: query.max_lines,
            };
            let result = crate::search(workspace, &query, cancel).map_err(from_logic)?;
            Ok(SearchResult {
                files: result
                    .files
                    .into_iter()
                    .map(|file| FileMatches {
                        path: file.path,
                        lines: file
                            .lines
                            .into_iter()
                            .map(|line| SearchLine {
                                line_number: line.line_number,
                                text: line.text,
                                is_match: line.is_match,
                            })
                            .collect(),
                    })
                    .collect(),
                truncated: result.truncated,
                omitted_files: result.omitted_files,
            })
        })
    }
}

/// The workspace service's failures as the frozen `fs-error`, worded as the native `grep`
/// words them.
fn workspace_error(error: p1_workspace::WorkspaceError) -> FsError {
    from_logic(crate::fs_error(error))
}

/// The logic crate's mirror of `fs-error` as the runtime's.
fn from_logic(error: logic::FsError) -> FsError {
    match error {
        logic::FsError::OutsideWorkspace => FsError::OutsideWorkspace,
        logic::FsError::NotFound => FsError::NotFound,
        logic::FsError::WrongKind => FsError::WrongKind,
        logic::FsError::AlreadyExists => FsError::AlreadyExists,
        logic::FsError::InvalidPattern(message) => FsError::InvalidPattern(message),
        logic::FsError::Cancelled => FsError::Cancelled,
        logic::FsError::Io(message) => FsError::Io(message),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_dropped_request_cancels_its_walk() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "beta\n").unwrap();
        let capability = SearchCapability::new(Workspace::new(dir.path()).unwrap());
        // The work starts, the request is dropped while it runs (as the runtime drops it
        // on a cancellation), and the work then sees its token cancelled.
        let (started, work_started) = std::sync::mpsc::channel();
        let (release, work_released) = std::sync::mpsc::channel::<()>();
        let (seen, observed) = std::sync::mpsc::channel();
        let mut request = capability.blocking(move |_, cancel| {
            started.send(()).unwrap();
            work_released.recv().unwrap();
            seen.send(cancel.is_cancelled()).unwrap();
            Ok(())
        });
        let mut context = std::task::Context::from_waker(std::task::Waker::noop());
        assert!(request.as_mut().poll(&mut context).is_pending());
        work_started.recv().unwrap();
        drop(request);
        release.send(()).unwrap();
        assert!(
            observed.recv().unwrap(),
            "the walk must see the cancellation"
        );

        assert_eq!(
            capability.list_files(".".into(), None).await,
            Ok(vec!["a.txt".to_owned()])
        );
    }
}
