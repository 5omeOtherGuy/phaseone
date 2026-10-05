//! The additive `directory-listing` capability (ADR-0115).
#[cfg(test)]
#[path = "directory_listing_tests.rs"]
mod tests;
use std::path::PathBuf;

use p1_contracts::{BoxFuture, CancellationToken};
use p1_workspace::{
    CredentialPolicy, FileKind, ListingError, ListingPage, ProtectedIndex, Workspace,
};
use wasmtime::bail;
use wasmtime::component::{Linker, Val};

use crate::capabilities::{CallState, FsError, check_arity, unless_cancelled};
use crate::loader::interface_import;

/// The native request behind one bounded directory page.
#[derive(Debug, Clone)]
pub struct ListingRequest {
    pub path: String,
    pub depth: u32,
    pub limit: u32,
    pub ignore: Vec<String>,
    pub continuation: Option<String>,
}

pub trait DirectoryListingService: Send + Sync {
    fn list_directory(
        &self,
        request: ListingRequest,
        cancel: CancellationToken,
    ) -> BoxFuture<'_, Result<ListingPage, FsError>>;
}

/// One agent's workspace and credential policy, never selected by the guest.
pub struct DirectoryListingCapability {
    workspace: Workspace,
}

impl DirectoryListingCapability {
    pub fn new(workspace: Workspace, home: Option<PathBuf>) -> Self {
        Self {
            workspace: workspace.with_credential_home(home),
        }
    }
}

impl DirectoryListingService for DirectoryListingCapability {
    fn list_directory(
        &self,
        request: ListingRequest,
        cancel: CancellationToken,
    ) -> BoxFuture<'_, Result<ListingPage, FsError>> {
        let workspace = self.workspace.clone();
        Box::pin(async move {
            tokio::task::spawn_blocking(move || {
                let policy = CredentialPolicy::new(
                    workspace.credential_home(),
                    workspace.credential_paths(),
                );
                policy
                    .refuse(&workspace, &request.path)
                    .map_err(FsError::Io)?;
                let index =
                    ProtectedIndex::build(&policy, &cancel).map_err(|_| FsError::Cancelled)?;
                let mut globs = ignore::gitignore::GitignoreBuilder::new(workspace.root());
                globs.allow_unclosed_class(false);
                for pattern in &request.ignore {
                    // Each input is an exclusion glob, not a gitignore directive.
                    if pattern.starts_with('!') || pattern.starts_with('#') {
                        return Err(FsError::InvalidPattern(
                            "ignore expects exclusion globs, not gitignore directives".into(),
                        ));
                    }
                    globs
                        .add_line(None, pattern)
                        .map_err(|e| FsError::InvalidPattern(e.to_string()))?;
                }
                let globs = globs
                    .build()
                    .map_err(|e| FsError::InvalidPattern(e.to_string()))?;
                let result = workspace
                    .list_directory(
                        &request.path,
                        request.depth,
                        request.limit,
                        request.continuation.as_deref(),
                        &cancel,
                        |path, file| {
                            let candidate = workspace.root().join(path);
                            if policy.refuses(&candidate) {
                                return Ok(true);
                            }
                            let metadata = file
                                .metadata()
                                .map_err(|e| ListingError::Io(e.to_string()))?;
                            if policy.refuses_opened(&candidate, file)
                                || index.refuses_metadata(&metadata)
                                || index.refuses_current_exact(&policy, &metadata)
                                || index
                                    .refuses_unsettled_alias(&metadata, &cancel)
                                    .map_err(|_| ListingError::Cancelled)?
                            {
                                return Ok(true);
                            }
                            Ok(globs.matched(&candidate, metadata.is_dir()).is_ignore())
                        },
                    )
                    .map_err(listing_error);
                if !index
                    .stamps_unchanged(&cancel)
                    .map_err(|_| FsError::Cancelled)?
                {
                    return Err(FsError::Io(
                        "protected credential directory changed during listing; retry".into(),
                    ));
                }
                result
            })
            .await
            .unwrap_or_else(|e| Err(FsError::Io(format!("directory listing failed: {e}"))))
        })
    }
}

pub(crate) fn listing_error(error: ListingError) -> FsError {
    match error {
        ListingError::Workspace(e) => crate::file_walk::workspace_error(e),
        ListingError::Invalid(message) => FsError::InvalidPattern(message),
        ListingError::Cancelled => FsError::Cancelled,
        ListingError::Io(message) => FsError::Io(message),
    }
}

pub(crate) fn link(linker: &mut Linker<CallState>) -> wasmtime::Result<()> {
    let mut listing = linker.instance(&interface_import("directory-listing"))?;
    listing.func_new_async("list-directory", |store, _ty, params, results| {
        let request = (|| {
            check_arity("directory-listing.list-directory", params, results, 5, 1)?;
            let (
                Val::String(path),
                Val::U32(depth),
                Val::U32(limit),
                Val::List(ignore),
                Val::Option(cursor),
            ) = (&params[0], &params[1], &params[2], &params[3], &params[4])
            else {
                bail!("directory-listing.list-directory: invalid parameter types");
            };
            let ignore = ignore
                .iter()
                .map(|v| match v {
                    Val::String(s) => Ok(s.clone()),
                    _ => Err(wasmtime::format_err!(
                        "directory-listing: ignore must be strings"
                    )),
                })
                .collect::<wasmtime::Result<Vec<_>>>()?;
            let continuation = match cursor {
                None => None,
                Some(value) => match value.as_ref() {
                    Val::String(s) => Some(s.clone()),
                    _ => bail!("directory-listing: continuation must be a string"),
                },
            };
            Ok(ListingRequest {
                path: path.clone(),
                depth: *depth,
                limit: *limit,
                ignore,
                continuation,
            })
        })();
        Box::new(async move {
            let request = request?;
            let Some(service) = store.data().directory_listing.clone() else {
                bail!("directory-listing called without a directory-listing service");
            };
            let cancel = store.data().cancel.clone();
            results[0] = crate::capabilities::fs_result(
                unless_cancelled(&cancel, service.list_directory(request, cancel.clone()))
                    .await
                    .map(|page| Some(page_val(page))),
            );
            Ok(())
        })
    })
}

fn page_val(page: ListingPage) -> Val {
    Val::Record(vec![
        (
            "entries".into(),
            Val::List(
                page.entries
                    .into_iter()
                    .map(|entry| {
                        Val::Record(vec![
                            ("path".into(), Val::String(entry.path)),
                            (
                                "kind".into(),
                                Val::Enum(
                                    match entry.kind {
                                        FileKind::File => "file",
                                        FileKind::Directory => "directory",
                                        FileKind::Symlink => "symlink",
                                        FileKind::Other => "other",
                                    }
                                    .into(),
                                ),
                            ),
                            ("size".into(), Val::U64(entry.size)),
                            ("depth".into(), Val::U32(entry.depth)),
                        ])
                    })
                    .collect(),
            ),
        ),
        (
            "next".into(),
            Val::Option(page.next.map(|s| Box::new(Val::String(s)))),
        ),
        ("scanned".into(), Val::U64(page.scanned)),
        ("scan-capped".into(), Val::Bool(page.scan_capped)),
    ])
}
