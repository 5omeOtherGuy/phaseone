//! The `apply_patch` tool: the GPT-family V4A patch format.
//!
//! A patch is parsed by hand into hunks, then *planned* completely in memory:
//! every path is confined with [`Workspace::resolve`], every hunk must locate
//! (exact, then trailing-whitespace-insensitive, then whitespace-insensitive),
//! and every resulting file content is computed before a single byte is
//! written. Only then are the planned writes applied through the
//! descriptor-backed workspace commit: one batch when no two spellings name one
//! not-yet-existing file, and in patch order otherwise, as the native
//! `write_atomic` sequence did for an in-root directory symlink. Planning reads
//! its targets through the credential-checked handle, so a swap after the path
//! refusal does not leak an alias's bytes into the hunk match.
//!
//! `apply_patch` is exempt from read-before-mutate: the hunks must match the
//! file's CURRENT contents, which is its own staleness check. It still records
//! every file it writes in the shared [`ObservedFiles`].
//!
//! The declaration's data, input validation, the parser, the plan, the call
//! and result descriptions and every text the model sees live in
//! `p1-tool-patch-logic`, which the `p1/patch` component calls too, so both run
//! the same code. This module owns the native flow: planning over the real
//! filesystem and applying the writes, under the write gate.

use std::collections::HashMap;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use p1_contracts::tool::{ResultDescription, ResultDetail};
use p1_contracts::{
    BoxFuture, CallDescription, CancellationToken, DeclarationKind, Effect, Grammar, Tool,
    ToolCall, ToolContext, ToolDeclaration, ToolIdentity, ToolInput, ToolOutcome, ToolStatus,
};
use p1_tool_patch_logic::{self as logic, Files, Op, PATCH_GRAMMAR, PatchFailure, RawInput};
use p1_workspace::{
    Change, MutationError, MutationPolicy, ObservedFiles, SnapshotMetadata, ToolFace, Workspace,
};

/// The `apply_patch` tool. Holds one agent's workspace and observation store.
pub struct PatchTool {
    workspace: Workspace,
    observed: ObservedFiles,
    declaration: ToolDeclaration,
    identity: ToolIdentity,
    /// Whether this instance is presented as a freeform or a function tool.
    freeform: bool,
}

impl PatchTool {
    /// Build the tool with the default (`apply_patch`, GPT-family) freeform face.
    pub fn new(workspace: Workspace, observed: ObservedFiles) -> Self {
        Self {
            workspace,
            observed,
            declaration: declaration(default_face(), true),
            identity: identity("gpt"),
            freeform: true,
        }
    }

    /// Present the same implementation under another name/description and
    /// variant, keeping the current freeform/function shape.
    pub fn with_face(self, face: ToolFace, variant: &str) -> Self {
        let freeform = self.freeform;
        Self {
            workspace: self.workspace,
            observed: self.observed,
            declaration: declaration(face, freeform),
            identity: identity(variant),
            freeform,
        }
    }

    /// Present the same implementation as a function tool taking
    /// `{"patch": string}`, for routes without freeform tools.
    pub fn function_face(self) -> Self {
        let face = ToolFace::new(
            self.declaration.name.clone(),
            self.declaration.description.clone(),
        );
        Self {
            workspace: self.workspace,
            observed: self.observed,
            declaration: declaration(face, false),
            identity: identity("function"),
            freeform: false,
        }
    }
}

fn default_face() -> ToolFace {
    ToolFace::new(logic::NAME, logic::DESCRIPTION)
}

fn declaration(face: ToolFace, freeform: bool) -> ToolDeclaration {
    let kind = if freeform {
        DeclarationKind::Freeform {
            grammar: Some(Grammar {
                syntax: logic::GRAMMAR_SYNTAX.to_string(),
                definition: PATCH_GRAMMAR.to_string(),
            }),
        }
    } else {
        DeclarationKind::Function {
            input_schema: logic::function_schema(),
        }
    };
    ToolDeclaration {
        name: face.name,
        description: face.description,
        kind,
    }
}

fn identity(variant: &str) -> ToolIdentity {
    ToolIdentity {
        implementation: env!("CARGO_PKG_NAME").to_string(),
        variant: variant.to_string(),
    }
}

/// The call's input as the logic crate reads it.
fn raw_input(input: &ToolInput) -> RawInput<'_> {
    match input {
        ToolInput::Json(raw) => RawInput::Json(raw),
        ToolInput::Text(raw) => RawInput::Text(raw),
    }
}

impl Tool for PatchTool {
    fn declaration(&self) -> &ToolDeclaration {
        &self.declaration
    }

    fn identity(&self) -> &ToolIdentity {
        &self.identity
    }

    fn effect(&self, _call: &ToolCall) -> Effect {
        Effect::WritesFiles
    }

    /// ADR-0057: parse the patch's own freeform (or function) input the same way
    /// `execute` does, and name the first file it touches — or the count, for a
    /// multi-file patch.
    fn describe(&self, call: &ToolCall) -> CallDescription {
        // Natively the workspace is at hand, so an escape is decided by resolving
        // the path, symlinks included.
        let described = logic::describe(self.freeform, raw_input(&call.input), |path| {
            self.workspace.resolve(path).is_err()
        });
        CallDescription {
            verb: logic::VERB,
            target: described.target,
            edit: None,
            destructive: described.destructive,
        }
    }

    fn describe_result(
        &self,
        call: &ToolCall,
        result: &p1_contracts::ToolResultItem,
    ) -> ResultDescription {
        let described = logic::describe_result(
            self.freeform,
            raw_input(&call.input),
            result.status == ToolStatus::Ok,
            &result.content,
        );
        ResultDescription {
            summary: described.summary,
            detail: described.files.map(|paths| ResultDetail::Files { paths }),
        }
    }

    fn execute<'a>(
        &'a self,
        call: &'a ToolCall,
        context: ToolContext,
    ) -> BoxFuture<'a, ToolOutcome> {
        Box::pin(async move {
            // Cancellation before any work: touch nothing.
            if context.cancel.is_cancelled() {
                return ToolOutcome {
                    status: ToolStatus::Cancelled,
                    content: String::new(),
                };
            }
            let patch = match logic::patch_text(
                &self.declaration.name,
                self.freeform,
                raw_input(&call.input),
            ) {
                Ok(patch) => patch,
                Err(message) => return ToolOutcome::error(message),
            };
            let workspace = self.workspace.clone();
            let observed = self.observed.clone();
            let cancel = context.cancel.clone();
            let tool = self.declaration.name.clone();
            match tokio::task::spawn_blocking(move || run(&workspace, &observed, &patch, &cancel))
                .await
            {
                Ok(Ok(content)) => ToolOutcome::ok(logic::bounded(&content)),
                Ok(Err(PatchFailure::Message(message))) => ToolOutcome::error(message),
                Ok(Err(PatchFailure::Cancelled)) => ToolOutcome {
                    status: ToolStatus::Cancelled,
                    content: String::new(),
                },
                Err(error) => ToolOutcome::error(format!("{tool} failed: {error}")),
            }
        })
    }
}

fn run(
    workspace: &Workspace,
    observed: &ObservedFiles,
    text: &str,
    cancel: &CancellationToken,
) -> Result<String, PatchFailure> {
    let hunks = logic::parse_patch(text)?;
    // Hold the shared write gate across planning and applying: the hunks are located by
    // reading the files' current contents, so another agent's write must not land between
    // that read and the write it produced (ADR-0032). The component reads outside the gate
    // and is refused as stale instead, and the native tool's re-match is kept by planning
    // under the gate. A call cancelled while it waits here mutates nothing.
    let held = tokio::runtime::Handle::current().block_on(workspace.begin_owned(
        observed,
        &p1_workspace::ReadRecord::new(),
        MutationPolicy::PatchAuthorized,
    ));
    if cancel.is_cancelled() {
        return Err(PatchFailure::Cancelled);
    }
    let mut read_snapshots = HashMap::new();
    // The original, model-supplied request for each resolved path. The commit re-resolves
    // the request rather than a lossily re-encoded resolved path, so a root or in-workspace
    // symlink target holding non-UTF-8 bytes is preserved.
    let mut requests: HashMap<PathBuf, String> = HashMap::new();
    let ops = logic::plan(
        &mut NativeFiles {
            workspace,
            cancel,
            read_snapshots: &mut read_snapshots,
            requests: &mut requests,
        },
        &hunks,
    )?;
    if cancel.is_cancelled() {
        return Err(PatchFailure::Cancelled);
    }
    // Frozen native parity: two adds through an in-root directory symlink are
    // sequential writes through the same parent, unlike the guest's create-only
    // changes. Preserve that behavior using the descriptor-backed commit path, but
    // keep each write cancellable so a call cancelled while staging mutates nothing.
    if let [Op::Add { path: first, .. }, Op::Add { path: second, .. }] = ops.as_slice()
        && first != second
        && canonical_leaf_key(first).is_some_and(|key| Some(key) == canonical_leaf_key(second))
    {
        apply_two_add_aliases(&held, &ops, &requests, cancel)?;
        return Ok(logic::success_output(&ops));
    }
    let coalesced = logic::coalesce(&ops);
    let (changes, aliased) = workspace_changes(&coalesced, &requests, &read_snapshots);
    if aliased {
        // Two spellings of one not-yet-existing file (an in-root directory symlink), or a
        // shared missing subdirectory, are one canonical target the host refuses to name
        // twice in a batch. The native tool writes the patch's ops in order, so apply the
        // coalesced changes one at a time: a later creation becomes a replacement, exactly
        // as the native `write_atomic` sequence did. The shared gate is already held, so no
        // other participating writer interleaves.
        for change in &changes {
            if cancel.is_cancelled() {
                return Err(PatchFailure::Cancelled);
            }
            held.apply_all_cancellable(std::slice::from_ref(change), cancel)
                .map_err(|error| mutation_failure(&ops, error))?;
        }
    } else {
        held.apply_all_cancellable(&changes, cancel)
            .map_err(|error| mutation_failure(&ops, error))?;
    }
    Ok(logic::success_output(&ops))
}

/// The frozen native two-add sequence, applied as two cancellable commits: the token is
/// rechecked before each and each commit rechecks it after staging, so a call cancelled
/// while the first operation is staged does not then perform the second. The shared gate
/// is already held, so no other participating writer can interleave between them.
fn apply_two_add_aliases(
    held: &p1_workspace::OwnedMutation,
    ops: &[Op<PathBuf>],
    requests: &HashMap<PathBuf, String>,
    cancel: &CancellationToken,
) -> Result<(), PatchFailure> {
    let [
        Op::Add {
            path: first,
            contents: one,
            ..
        },
        Op::Add {
            path: second,
            contents: two,
            ..
        },
    ] = ops
    else {
        return Ok(());
    };
    // The first add may target a dangling symlink (the native writer replaced its entry);
    // the second aliases the first, so it is always a replacement.
    let first_change = if dangling_leaf(first) {
        Change::write(request_for(requests, first), one.to_vec())
    } else {
        Change::create(request_for(requests, first), one.to_vec())
    };
    let changes = [
        first_change,
        Change::write(request_for(requests, second), two.to_vec()),
    ];
    for change in &changes {
        if cancel.is_cancelled() {
            return Err(PatchFailure::Cancelled);
        }
        held.apply_all_cancellable(std::slice::from_ref(change), cancel)
            .map_err(|error| mutation_failure(ops, error))?;
    }
    Ok(())
}

/// Map a workspace commit failure to the patch's failure, keeping the cancellation
/// sentinel distinct so `execute` reports `ToolStatus::Cancelled` instead of an error.
fn mutation_failure(ops: &[Op<PathBuf>], error: MutationError) -> PatchFailure {
    match error {
        MutationError::Io(message) if message == "cancelled" => PatchFailure::Cancelled,
        error => PatchFailure::Message(native_commit_error(ops, error)),
    }
}

/// The canonical name two spellings of one file share, whether or not the leaf (or part
/// of its parent chain) exists yet: the deepest existing ancestor is canonicalized and the
/// missing components and the leaf are re-joined. An in-root directory symlink therefore
/// makes `link/sub/x` and `real/sub/x` one key even when `sub` does not exist, so the
/// native sequential-overwrite case is detected for every patch shape.
fn canonical_leaf_key(path: &Path) -> Option<PathBuf> {
    let mut ancestor = path.parent()?;
    let mut missing: Vec<&OsStr> = Vec::new();
    loop {
        match std::fs::symlink_metadata(ancestor) {
            Ok(_) => break,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                missing.push(ancestor.file_name()?);
                ancestor = ancestor.parent()?;
            }
            Err(_) => return None,
        }
    }
    let mut key = ancestor.canonicalize().ok()?;
    for name in missing.into_iter().rev() {
        key.push(name);
    }
    key.push(path.file_name()?);
    Some(key)
}

/// Whether the leaf is a dangling symbolic link: present as an entry, absent as a file.
/// The native atomic writer replaced such an entry; a create-only change must not, so the
/// patch classifies it as a replacement.
fn dangling_leaf(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok_and(|metadata| metadata.file_type().is_symlink())
        && std::fs::metadata(path).is_err()
}

/// The request to hand the commit for a resolved path: the model's original spelling when
/// this patch resolved it (so non-UTF-8 root or symlink bytes survive), else the resolved
/// path shown lossily.
fn request_for(requests: &HashMap<PathBuf, String>, path: &Path) -> String {
    requests
        .get(path)
        .cloned()
        .unwrap_or_else(|| path.to_string_lossy().into_owned())
}

/// Turn the coalesced changes into workspace changes, detecting whether any two target one
/// canonical file through different spellings. A later creation of an already-targeted
/// canonical path (or a dangling-leaf creation) becomes a replacement; `aliased` tells the
/// caller to apply the changes one at a time, because the host refuses a batch that names
/// one canonical file twice.
fn workspace_changes(
    changes: &[logic::Change<PathBuf>],
    requests: &HashMap<PathBuf, String>,
    read_snapshots: &HashMap<PathBuf, SnapshotMetadata>,
) -> (Vec<Change>, bool) {
    let mut used: Vec<PathBuf> = Vec::new();
    let mut aliased = false;
    let mut built = Vec::with_capacity(changes.len());
    for change in changes {
        let (path, change) = match change {
            logic::Change::Create { path, contents, .. } => {
                let key = canonical_leaf_key(path).unwrap_or_else(|| path.clone());
                let repeat = used.contains(&key);
                if repeat {
                    aliased = true;
                }
                used.push(key);
                let requested = request_for(requests, path);
                let change = if repeat || dangling_leaf(path) {
                    Change::write(requested, contents.clone())
                } else {
                    Change::create(requested, contents.clone())
                };
                (path, change)
            }
            logic::Change::Write { path, contents, .. } => {
                let key = canonical_leaf_key(path).unwrap_or_else(|| path.clone());
                used.push(key);
                (
                    path,
                    Change::write(request_for(requests, path), contents.clone()),
                )
            }
            logic::Change::Remove { path, .. } => {
                let key = canonical_leaf_key(path).unwrap_or_else(|| path.clone());
                used.push(key);
                (path, Change::remove(request_for(requests, path)))
            }
        };
        let change = read_snapshots
            .get(path)
            .map_or(change.clone(), |snapshot| change.computed_from(snapshot));
        built.push(change);
    }
    (built, aliased)
}

/// Preserve the native patch error texts for failures that now originate in
/// the descriptor-relative workspace commit instead of `write_atomic`.
fn native_commit_error(ops: &[Op<PathBuf>], error: MutationError) -> String {
    if ops.len() != 1 {
        return error.to_string();
    }
    let writing = ops.iter().find_map(|op| match op {
        Op::Add { display, .. } | Op::Modify { display, .. } => Some(display.to_string()),
        Op::Move { to, .. } => Some(to.display().to_string()),
        Op::Delete { .. } => None,
    });
    if let Some(display) = writing {
        let reason = match &error {
            MutationError::WrongKind { .. } => Some("File exists (os error 17)"),
            MutationError::Io(text) if text.contains("Not a directory") => {
                Some("Not a directory (os error 20)")
            }
            _ => None,
        };
        if let Some(reason) = reason {
            return logic::failed_to_write(&display, reason);
        }
    }
    error.to_string()
}

/// The real filesystem as the logic crate's plan sees it: keys are resolved
/// paths, so two spellings of one file share its staged contents.
struct NativeFiles<'a> {
    workspace: &'a Workspace,
    cancel: &'a CancellationToken,
    read_snapshots: &'a mut HashMap<PathBuf, SnapshotMetadata>,
    /// The model's original request for each resolved path (see [`request_for`]).
    requests: &'a mut HashMap<PathBuf, String>,
}

impl Files for NativeFiles<'_> {
    type Key = PathBuf;

    fn cancelled(&mut self) -> bool {
        self.cancel.is_cancelled()
    }

    fn resolve(&mut self, path: &str) -> Result<(PathBuf, String), PatchFailure> {
        self.workspace
            .refuse_mutation_credentials(path)
            .map_err(PatchFailure::Message)?;
        let resolved = self
            .workspace
            .resolve(path)
            .map_err(|error| PatchFailure::Message(error.to_string()))?;
        let display = self.workspace.display(&resolved);
        self.requests.insert(resolved.clone(), path.to_string());
        Ok((resolved, display))
    }

    fn exists(&mut self, path: &PathBuf) -> bool {
        path.exists()
    }

    fn read(&mut self, path: &PathBuf, display: &str) -> Result<Option<Vec<u8>>, PatchFailure> {
        let requested = request_for(self.requests, path);
        // The checked read proves the very handle the bytes come from is not a credential:
        // `resolve` refused credential paths already, but the leaf can be swapped for an
        // alias between that refusal and this open. Planning must not materialize those
        // bytes, or the hunk match becomes a content oracle.
        let snapshot = match self
            .workspace
            .read_unobserved_checked(&requested, self.cancel)
        {
            Ok(snapshot) => snapshot,
            Err(p1_workspace::WorkspaceError::NotFound { .. }) => return Ok(None),
            Err(p1_workspace::WorkspaceError::NotADirectory(_)) => {
                return Err(PatchFailure::Message(logic::not_a_regular_file(display)));
            }
            Err(p1_workspace::WorkspaceError::Io { source, .. }) => {
                return Err(PatchFailure::Message(logic::could_not_be_read(
                    display,
                    &source.to_string(),
                )));
            }
            Err(error) => {
                return Err(PatchFailure::Message(logic::could_not_be_read(
                    display,
                    &error.to_string(),
                )));
            }
        };
        self.read_snapshots
            .insert(path.clone(), snapshot.metadata());
        Ok(Some(snapshot.read(0, usize::MAX).to_vec()))
    }
}

#[cfg(test)]
mod tests {
    use super::{PATCH_GRAMMAR, PatchTool};
    use p1_contracts::tool::ResultDetail;
    use p1_contracts::{
        CancellationToken, DeclarationKind, Effect, Grammar, Tool, ToolCall, ToolContext,
        ToolInput, ToolOutcome, ToolResultItem, ToolStatus,
    };
    use p1_workspace::{Observation, ObservedFiles, ToolFace, Workspace, WriteGate};
    use std::path::Path;
    use std::task::{Context, Poll, Waker};

    fn tool(root: &Path) -> (PatchTool, ObservedFiles) {
        let observed = ObservedFiles::new();
        (
            PatchTool::new(Workspace::new(root).unwrap(), observed.clone()),
            observed,
        )
    }

    fn text_call(patch: &str) -> ToolCall {
        ToolCall {
            call_id: "call-1".into(),
            name: "apply_patch".into(),
            input: ToolInput::Text(patch.to_string()),
        }
    }

    async fn execute(tool: &PatchTool, patch: &str) -> ToolOutcome {
        let call = text_call(patch);
        let context = ToolContext {
            cancel: CancellationToken::new(),
        };
        tool.execute(&call, context).await
    }

    fn read(root: &Path, path: &str) -> String {
        std::fs::read_to_string(root.join(path)).unwrap()
    }

    fn write(root: &Path, path: &str, contents: &str) {
        let target = root.join(path);
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(target, contents).unwrap();
    }

    /// (a) The three-file example from `environments/gpt/prompt.md`.
    #[tokio::test]
    async fn prompt_md_three_file_example_applies() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "path/to/file.rs",
            "fn existing_function\nunchanged context line\nremoved line\n",
        );
        write(dir.path(), "path/to/old_file.rs", "old\n");
        let (tool, _) = tool(dir.path());
        let patch = "*** Begin Patch\n*** Update File: path/to/file.rs\n@@ fn existing_function\n unchanged context line\n-removed line\n+added line\n*** Add File: path/to/new_file.rs\n+first line\n*** Delete File: path/to/old_file.rs\n*** End Patch\n";

        let outcome = execute(&tool, patch).await;

        assert_eq!(outcome.status, ToolStatus::Ok, "{outcome:?}");
        assert_eq!(
            outcome.content,
            "M path/to/file.rs\nA path/to/new_file.rs\nD path/to/old_file.rs"
        );
        let result = ToolResultItem {
            call_id: "call-1".into(),
            name: "apply_patch".into(),
            status: outcome.status,
            content: outcome.content,
        };
        let described = tool.describe_result(&text_call(patch), &result);
        assert_eq!(described.summary, "+2 · 3 files");
        assert_eq!(
            described.detail,
            Some(ResultDetail::Files {
                paths: vec![
                    "path/to/file.rs\t+1 −1".into(),
                    "path/to/new_file.rs\t+1 −0".into(),
                    "path/to/old_file.rs\tD".into(),
                ],
            })
        );
        assert_eq!(
            read(dir.path(), "path/to/file.rs"),
            "fn existing_function\nunchanged context line\nadded line\n"
        );
        assert_eq!(read(dir.path(), "path/to/new_file.rs"), "first line\n");
        assert!(!dir.path().join("path/to/old_file.rs").exists());
    }

    /// (b) The second hunk's context also occurs earlier; it must apply later.
    #[tokio::test]
    async fn second_hunk_applies_at_the_later_position() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "f.txt", "ctx\nA\nctx\nA\nB\n");
        let (tool, _) = tool(dir.path());
        let patch = "*** Begin Patch\n*** Update File: f.txt\n@@ ctx\n-A\n+first\n@@ ctx\n-A\n+second\n*** End Patch\n";

        let outcome = execute(&tool, patch).await;

        assert_eq!(outcome.status, ToolStatus::Ok, "{outcome:?}");
        assert_eq!(read(dir.path(), "f.txt"), "ctx\nfirst\nctx\nsecond\nB\n");
    }

    /// (c) A later failure must leave the first file byte-identical.
    #[tokio::test]
    async fn a_failed_second_file_leaves_the_first_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let original = "one\ntwo\nthree\n";
        write(dir.path(), "first.txt", original);
        write(dir.path(), "second.txt", "alpha\n");
        let (tool, _) = tool(dir.path());
        let patch = "*** Begin Patch\n*** Update File: first.txt\n-one\n+ONE\n*** Update File: second.txt\n-missing\n+other\n*** End Patch\n";

        let outcome = execute(&tool, patch).await;

        assert_eq!(outcome.status, ToolStatus::Error, "{outcome:?}");
        assert_eq!(
            outcome.content,
            "second.txt: hunk 1 did not match the file."
        );
        assert_eq!(read(dir.path(), "first.txt"), original);
        assert_eq!(read(dir.path(), "second.txt"), "alpha\n");
    }

    /// (d) The whitespace ladder matches a tab-indented file whose patch
    /// context line carries trailing spaces.
    #[tokio::test]
    async fn whitespace_ladder_applies_a_tab_indented_file() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "f.txt", "ctx\n\talpha\n");
        let (tool, _) = tool(dir.path());
        let patch = "*** Begin Patch\n*** Update File: f.txt\n@@\n ctx\n-\talpha   \n+\tbeta\n*** End Patch\n";

        let outcome = execute(&tool, patch).await;

        assert_eq!(outcome.status, ToolStatus::Ok, "{outcome:?}");
        assert_eq!(read(dir.path(), "f.txt"), "ctx\n\tbeta\n");
    }

    /// (e) An escaping path or a symlink out of the workspace is rejected and
    /// nothing is written.
    #[cfg(unix)]
    #[tokio::test]
    async fn escaping_paths_and_symlinks_are_rejected() {
        use std::os::unix::fs::symlink;
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("secret"), "secret\n").unwrap();
        symlink(outside.path(), dir.path().join("link")).unwrap();
        let (tool, _) = tool(dir.path());

        let add = execute(
            &tool,
            "*** Begin Patch\n*** Add File: ../x\n+hello\n*** End Patch\n",
        )
        .await;
        assert_eq!(add.status, ToolStatus::Error, "{add:?}");
        assert!(add.content.contains("escapes workspace"), "{add:?}");

        let update = execute(
            &tool,
            "*** Begin Patch\n*** Update File: link/secret\n-old\n+new\n*** End Patch\n",
        )
        .await;
        assert_eq!(update.status, ToolStatus::Error, "{update:?}");
        assert!(update.content.contains("escapes workspace"), "{update:?}");
        assert_eq!(
            std::fs::read_to_string(outside.path().join("secret")).unwrap(),
            "secret\n"
        );
    }

    /// (f) Adding a file that already exists is an error.
    #[tokio::test]
    async fn add_of_an_existing_file_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "exists.txt", "original\n");
        let (tool, _) = tool(dir.path());

        let outcome = execute(
            &tool,
            "*** Begin Patch\n*** Add File: exists.txt\n+replacement\n*** End Patch\n",
        )
        .await;

        assert_eq!(outcome.status, ToolStatus::Error, "{outcome:?}");
        assert_eq!(outcome.content, "exists.txt already exists.");
        assert_eq!(read(dir.path(), "exists.txt"), "original\n");
    }

    /// (g) Garbage is always `Invalid patch: …`, never a panic.
    #[tokio::test]
    async fn garbage_patches_are_invalid_patches() {
        let dir = tempfile::tempdir().unwrap();
        let (tool, _) = tool(dir.path());
        let garbage = [
            "",
            "*** Update File: f.txt\n@@\n-a\n+b\n*** End Patch\n",
            "*** Begin Patch\n*** Add File: z.txt\n+x\n",
            "*** Begin Patch\n@@\n-a\n+b\n*** End Patch\n",
            "*** Begin Patch\n*** End Patch\n",
            "*** Begin Patch\n*** Frobnicate: x\n*** End Patch\n",
            "\u{0}\u{1}not a patch at all",
        ];
        for patch in garbage {
            let outcome = execute(&tool, patch).await;
            assert_eq!(outcome.status, ToolStatus::Error, "patch: {patch:?}");
            assert!(
                outcome.content.starts_with("Invalid patch: "),
                "patch: {patch:?} -> {outcome:?}"
            );
        }
        assert!(!dir.path().join("z.txt").exists());
    }

    /// (h) A CRLF file stays CRLF.
    #[tokio::test]
    async fn a_crlf_file_stays_crlf() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "f.txt", "a\r\nb\r\n");
        let (tool, _) = tool(dir.path());

        let outcome = execute(
            &tool,
            "*** Begin Patch\n*** Update File: f.txt\n@@\n-b\n+c\n*** End Patch\n",
        )
        .await;

        assert_eq!(outcome.status, ToolStatus::Ok, "{outcome:?}");
        assert_eq!(read(dir.path(), "f.txt"), "a\r\nc\r\n");
    }

    /// (i) A move and an edit in one hunk.
    #[tokio::test]
    async fn move_and_edit_in_one_hunk() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "old.txt", "x\ny\n");
        let (tool, _) = tool(dir.path());

        let outcome = execute(
            &tool,
            "*** Begin Patch\n*** Update File: old.txt\n*** Move to: sub/new.txt\n@@\n-y\n+z\n*** End Patch\n",
        )
        .await;

        assert_eq!(outcome.status, ToolStatus::Ok, "{outcome:?}");
        assert_eq!(outcome.content, "M old.txt -> sub/new.txt");
        assert!(!dir.path().join("old.txt").exists());
        assert_eq!(read(dir.path(), "sub/new.txt"), "x\nz\n");
    }

    #[tokio::test]
    async fn a_pure_move_needs_no_change_lines() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "old.txt", "x\n");
        let (tool, _) = tool(dir.path());

        let outcome = execute(
            &tool,
            "*** Begin Patch\n*** Update File: old.txt\n*** Move to: new.txt\n*** End Patch\n",
        )
        .await;

        assert_eq!(outcome.status, ToolStatus::Ok, "{outcome:?}");
        assert_eq!(outcome.content, "M old.txt -> new.txt");
        assert!(!dir.path().join("old.txt").exists());
        assert_eq!(read(dir.path(), "new.txt"), "x\n");
    }

    #[tokio::test]
    async fn a_move_onto_an_existing_path_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "old.txt", "x\n");
        write(dir.path(), "taken.txt", "y\n");
        let (tool, _) = tool(dir.path());

        let outcome = execute(
            &tool,
            "*** Begin Patch\n*** Update File: old.txt\n*** Move to: taken.txt\n*** End Patch\n",
        )
        .await;

        assert_eq!(outcome.status, ToolStatus::Error, "{outcome:?}");
        assert_eq!(outcome.content, "taken.txt already exists.");
        assert_eq!(read(dir.path(), "old.txt"), "x\n");
        assert_eq!(read(dir.path(), "taken.txt"), "y\n");
    }

    /// (j) `*** End of File` appends at the end of the file.
    #[tokio::test]
    async fn end_of_file_appends_at_the_end() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "f.txt", "a\nb\n");
        let (tool, _) = tool(dir.path());

        let outcome = execute(
            &tool,
            "*** Begin Patch\n*** Update File: f.txt\n@@\n+c\n*** End of File\n*** End Patch\n",
        )
        .await;

        assert_eq!(outcome.status, ToolStatus::Ok, "{outcome:?}");
        assert_eq!(read(dir.path(), "f.txt"), "a\nb\nc\n");
    }

    #[tokio::test]
    async fn a_plain_addition_without_context_appends_like_codex() {
        // A hunk with nothing to match has no anchor: Codex appends it at the end
        // of the file, and so does p1.
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "f.txt", "a\nb\n");
        let (tool, _) = tool(dir.path());

        let outcome = execute(
            &tool,
            "*** Begin Patch\n*** Update File: f.txt\n+x\n*** End Patch\n",
        )
        .await;

        assert_eq!(outcome.status, ToolStatus::Ok, "{outcome:?}");
        assert_eq!(read(dir.path(), "f.txt"), "a\nb\nx\n");
    }

    #[tokio::test]
    async fn a_change_without_lines_is_invalid() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "f.txt", "a\n");
        let (tool, _) = tool(dir.path());

        let outcome = execute(
            &tool,
            "*** Begin Patch\n*** Update File: f.txt\n@@ a\n*** End Patch\n",
        )
        .await;

        assert_eq!(outcome.status, ToolStatus::Error, "{outcome:?}");
        assert!(
            outcome.content.starts_with("Invalid patch: "),
            "{outcome:?}"
        );
        assert_eq!(read(dir.path(), "f.txt"), "a\n");
    }

    /// (k) The function face does the same as the freeform face.
    #[tokio::test]
    async fn function_face_behaves_like_the_freeform_face() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "f.txt", "a\nb\n");
        let observed = ObservedFiles::new();
        let tool = PatchTool::new(Workspace::new(dir.path()).unwrap(), observed).function_face();
        let patch = "*** Begin Patch\n*** Update File: f.txt\n-b\n+B\n*** End Patch\n";
        let call = ToolCall {
            call_id: "call-1".into(),
            name: "apply_patch".into(),
            input: ToolInput::Json(serde_json::json!({ "patch": patch }).to_string()),
        };
        let context = ToolContext {
            cancel: CancellationToken::new(),
        };

        let outcome = tool.execute(&call, context).await;

        assert_eq!(outcome.status, ToolStatus::Ok, "{outcome:?}");
        assert_eq!(outcome.content, "M f.txt");
        assert_eq!(read(dir.path(), "f.txt"), "a\nB\n");
    }

    #[tokio::test]
    async fn a_heredoc_wrapper_and_crlf_patch_are_tolerated() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "f.txt", "a\nb\n");
        let (tool, _) = tool(dir.path());
        let wrapped = "<<'EOF'\r\n*** Begin Patch\r\n*** Update File: f.txt\r\n@@\r\n-b\r\n+c\r\n*** End Patch\r\nEOF\r\n";

        let outcome = execute(&tool, wrapped).await;

        assert_eq!(outcome.status, ToolStatus::Ok, "{outcome:?}");
        assert_eq!(read(dir.path(), "f.txt"), "a\nc\n");
    }

    #[tokio::test]
    async fn a_fenced_code_block_wrapper_is_tolerated() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "f.txt", "a\nb\n");
        let (tool, _) = tool(dir.path());
        let fenced =
            "```diff\n*** Begin Patch\n*** Update File: f.txt\n-b\n+c\n*** End Patch\n```\n";

        let outcome = execute(&tool, fenced).await;

        assert_eq!(outcome.status, ToolStatus::Ok, "{outcome:?}");
        assert_eq!(read(dir.path(), "f.txt"), "a\nc\n");
    }

    #[tokio::test]
    async fn a_missing_final_newline_in_the_patch_is_tolerated() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "f.txt", "a\nb\n");
        let (tool, _) = tool(dir.path());
        let patch = "*** Begin Patch\n*** Update File: f.txt\n@@\n-b\n+c\n*** End Patch";

        let outcome = execute(&tool, patch).await;

        assert_eq!(outcome.status, ToolStatus::Ok, "{outcome:?}");
        assert_eq!(read(dir.path(), "f.txt"), "a\nc\n");
    }

    #[tokio::test]
    async fn an_empty_context_line_is_a_context_line() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "f.txt", "a\n\nb\n");
        let (tool, _) = tool(dir.path());
        let patch = "*** Begin Patch\n*** Update File: f.txt\n@@\n a\n\n-b\n+c\n*** End Patch\n";

        let outcome = execute(&tool, patch).await;

        assert_eq!(outcome.status, ToolStatus::Ok, "{outcome:?}");
        assert_eq!(read(dir.path(), "f.txt"), "a\n\nc\n");
    }

    #[tokio::test]
    async fn a_missing_update_target_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let (tool, _) = tool(dir.path());

        let outcome = execute(
            &tool,
            "*** Begin Patch\n*** Update File: gone.txt\n-a\n+b\n*** End Patch\n",
        )
        .await;

        assert_eq!(outcome.status, ToolStatus::Error, "{outcome:?}");
        assert_eq!(outcome.content, "gone.txt does not exist.");
    }

    #[tokio::test]
    async fn delete_removes_the_file() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "gone.txt", "bye\n");
        let (tool, _) = tool(dir.path());

        let outcome = execute(
            &tool,
            "*** Begin Patch\n*** Delete File: gone.txt\n*** End Patch\n",
        )
        .await;

        assert_eq!(outcome.status, ToolStatus::Ok, "{outcome:?}");
        assert_eq!(outcome.content, "D gone.txt");
        assert!(!dir.path().join("gone.txt").exists());
    }

    #[tokio::test]
    async fn written_files_are_recorded_as_observed() {
        let dir = tempfile::tempdir().unwrap();
        let (tool, observed) = tool(dir.path());

        execute(
            &tool,
            "*** Begin Patch\n*** Add File: new.txt\n+contents\n*** End Patch\n",
        )
        .await;

        assert_eq!(
            observed.check_unchanged(&dir.path().join("new.txt"), b"contents\n"),
            Observation::Unchanged
        );
    }

    #[test]
    fn declaration_is_freeform_with_the_v4a_grammar() {
        let dir = tempfile::tempdir().unwrap();
        let (tool, _) = tool(dir.path());
        assert_eq!(tool.declaration().name, "apply_patch");
        match &tool.declaration().kind {
            DeclarationKind::Freeform {
                grammar: Some(Grammar { syntax, definition }),
            } => {
                assert_eq!(syntax, "lark");
                assert_eq!(definition, PATCH_GRAMMAR);
            }
            other => panic!("expected a freeform declaration, got {other:?}"),
        }
    }

    #[test]
    fn function_face_declares_the_exact_schema() {
        let dir = tempfile::tempdir().unwrap();
        let (tool, _) = tool(dir.path());
        let tool = tool.function_face();

        assert_eq!(
            tool.declaration().kind,
            DeclarationKind::Function {
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": { "patch": { "type": "string" } },
                    "required": ["patch"],
                    "additionalProperties": false
                })
            }
        );
        assert_eq!(tool.identity().variant, "function");
    }

    #[test]
    fn identity_defaults_to_gpt_and_survives_a_face_change() {
        let dir = tempfile::tempdir().unwrap();
        let (tool, _) = tool(dir.path());
        assert_eq!(tool.identity().implementation, "p1-tool-patch");
        assert_eq!(tool.identity().variant, "gpt");

        let reshaped = tool.with_face(ToolFace::new("Patch", "custom"), "claude");
        assert_eq!(reshaped.declaration().name, "Patch");
        assert_eq!(reshaped.declaration().description, "custom");
        assert_eq!(reshaped.identity().variant, "claude");
        assert!(matches!(
            reshaped.declaration().kind,
            DeclarationKind::Freeform { .. }
        ));
    }

    #[test]
    fn effect_is_writes_files() {
        let dir = tempfile::tempdir().unwrap();
        let (tool, _) = tool(dir.path());
        assert_eq!(tool.effect(&text_call("")), Effect::WritesFiles);
    }

    /// ADR-0057: the freeform patch is parsed the same way `execute` parses it, so
    /// `describe` names the first file it touches — or the count. A renamed face
    /// changes nothing.
    #[test]
    fn describe_parses_the_freeform_text_for_the_file_or_the_count() {
        let dir = tempfile::tempdir().unwrap();
        let (tool, _) = tool(dir.path());
        let one = tool.describe(&text_call(
            "*** Begin Patch\n*** Update File: src/a.rs\n@@\n-a\n+b\n*** End Patch\n",
        ));
        assert_eq!(one.verb, "edit");
        assert_eq!(one.target.as_deref(), Some("src/a.rs"));
        assert!(!one.destructive);
        assert!(
            !tool
                .describe(&text_call(&format!(
                    "*** Begin Patch\n*** Add File: {}\n+x\n*** End Patch\n",
                    dir.path().join("inside").display()
                )))
                .destructive
        );

        let two = tool.describe(&text_call(
            "*** Begin Patch\n*** Add File: a\n+x\n*** Add File: b\n+y\n*** End Patch\n",
        ));
        assert_eq!(two.target.as_deref(), Some("2 files"));
        assert!(
            tool.describe(&text_call(
                "*** Begin Patch\n*** Add File: ../outside\n+x\n*** End Patch\n"
            ))
            .destructive
        );
        let outside = dir.path().parent().unwrap().join("outside");
        assert!(
            tool.describe(&text_call(&format!(
                "*** Begin Patch\n*** Add File: {}\n+x\n*** End Patch\n",
                outside.display()
            )))
            .destructive
        );

        let renamed = tool.with_face(ToolFace::new("Patch", "custom"), "claude");
        assert_eq!(
            renamed
                .describe(&text_call(
                    "*** Begin Patch\n*** Delete File: old.rs\n*** End Patch\n"
                ))
                .target
                .as_deref(),
            Some("old.rs")
        );
    }

    #[tokio::test]
    async fn invalid_function_input_reports_a_prefix_and_never_panics() {
        let dir = tempfile::tempdir().unwrap();
        let (tool, _) = tool(dir.path());
        let tool = tool.function_face();
        let garbage = [
            "",
            "null",
            "[]",
            "{}",
            "{\"patch\": 5}",
            "{\"patch\":\"x\",\"extra\":1}",
        ];
        for raw in garbage {
            let call = ToolCall {
                call_id: "call-1".into(),
                name: "apply_patch".into(),
                input: ToolInput::Json(raw.to_string()),
            };
            let context = ToolContext {
                cancel: CancellationToken::new(),
            };
            let outcome = tool.execute(&call, context).await;
            assert_eq!(outcome.status, ToolStatus::Error, "input: {raw:?}");
            assert!(
                outcome
                    .content
                    .starts_with("Invalid input for apply_patch: "),
                "input: {raw:?} -> {outcome:?}"
            );
        }
    }

    #[tokio::test]
    async fn the_wrong_input_kind_is_invalid_input() {
        let dir = tempfile::tempdir().unwrap();
        let (tool, _) = tool(dir.path());
        let json_call = ToolCall {
            call_id: "call-1".into(),
            name: "apply_patch".into(),
            input: ToolInput::Json("{}".into()),
        };
        let context = ToolContext {
            cancel: CancellationToken::new(),
        };
        let outcome = tool.execute(&json_call, context).await;
        assert_eq!(outcome.status, ToolStatus::Error);
        assert!(
            outcome
                .content
                .starts_with("Invalid input for apply_patch: ")
        );

        let function_tool = tool.function_face();
        let patch_call = text_call("*** Begin Patch\n*** End Patch\n");
        let context = ToolContext {
            cancel: CancellationToken::new(),
        };
        let outcome = function_tool.execute(&patch_call, context).await;
        assert_eq!(outcome.status, ToolStatus::Error);
        assert!(
            outcome
                .content
                .starts_with("Invalid input for apply_patch: ")
        );
    }

    #[tokio::test]
    async fn execute_returns_cancelled_without_touching_the_filesystem() {
        let dir = tempfile::tempdir().unwrap();
        let (tool, _) = tool(dir.path());
        let call = text_call("*** Begin Patch\n*** Add File: new.txt\n+x\n*** End Patch\n");
        let cancel = CancellationToken::new();
        cancel.cancel();

        let outcome = tool.execute(&call, ToolContext { cancel }).await;

        assert_eq!(outcome.status, ToolStatus::Cancelled);
        assert!(!dir.path().join("new.txt").exists());
    }

    /// A patch whose call is cancelled while it waits for the shared write gate must not
    /// mutate: the native patch checks the token as soon as it holds the gate, before it
    /// plans or writes.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_patch_cancelled_while_queued_on_the_gate_does_not_write() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "log.txt", "one\n");
        let gate = WriteGate::new();
        let workspace = Workspace::new(dir.path())
            .unwrap()
            .with_write_gate(gate.clone());
        let tool = PatchTool::new(workspace, ObservedFiles::new());
        let held = gate.begin_mutation();
        let cancel = CancellationToken::new();
        let call =
            text_call("*** Begin Patch\n*** Update File: log.txt\n@@\n-one\n+two\n*** End Patch\n");
        let mut future = tool.execute(
            &call,
            ToolContext {
                cancel: cancel.clone(),
            },
        );
        // Let the call pass its pre-work cancellation check and queue on the gate.
        let mut context = Context::from_waker(Waker::noop());
        assert!(matches!(future.as_mut().poll(&mut context), Poll::Pending));
        while gate.waiting_writers() == 0 {
            tokio::task::yield_now().await;
        }
        cancel.cancel();
        drop(held);

        let outcome = future.await;
        assert_eq!(outcome.status, ToolStatus::Cancelled, "{outcome:?}");
        assert_eq!(read(dir.path(), "log.txt"), "one\n");
    }

    /// A workspace commit failure that is the cancellation sentinel must surface as the
    /// patch's own `Cancelled`, so `execute` reports `ToolStatus::Cancelled`.
    #[test]
    fn a_cancelled_commit_maps_to_a_cancelled_patch() {
        let ops: Vec<super::Op<std::path::PathBuf>> = Vec::new();
        assert_eq!(
            super::mutation_failure(&ops, super::MutationError::Io("cancelled".into())),
            super::PatchFailure::Cancelled
        );
    }

    /// A two-add patch through an in-root directory symlink is the frozen sequential
    /// native sequence; a call cancelled before it starts must not write either file.
    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_cancelled_two_add_alias_patch_writes_nothing() {
        use std::os::unix::fs::symlink;
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "real/kept.txt", "k\n");
        symlink("real", dir.path().join("link")).unwrap();
        let observed = ObservedFiles::new();
        let workspace = Workspace::new(dir.path()).unwrap();
        let held = workspace
            .begin_owned(
                &observed,
                &p1_workspace::ReadRecord::new(),
                super::MutationPolicy::PatchAuthorized,
            )
            .await;
        let cancel = CancellationToken::new();
        cancel.cancel();
        let first = dir.path().join("link/x");
        let second = dir.path().join("real/x");
        let ops = vec![
            super::Op::Add {
                path: first.clone(),
                display: "link/x".into(),
                contents: b"one".to_vec(),
            },
            super::Op::Add {
                path: second.clone(),
                display: "real/x".into(),
                contents: b"two".to_vec(),
            },
        ];
        let requests = std::collections::HashMap::from([
            (first.clone(), "link/x".to_string()),
            (second.clone(), "real/x".to_string()),
        ]);
        let result = super::apply_two_add_aliases(&held, &ops, &requests, &cancel);
        assert_eq!(result, Err(super::PatchFailure::Cancelled));
        assert!(!first.exists());
        assert!(!second.exists());
    }

    /// An in-root directory symlink's two add spellings must keep the native sequential
    /// overwrite even when a third operation shares the patch.
    #[cfg(unix)]
    #[tokio::test]
    async fn alias_adds_with_a_third_operation_are_applied_in_order() {
        use std::os::unix::fs::symlink;
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "real/kept.txt", "k\n");
        symlink("real", dir.path().join("link")).unwrap();
        let (tool, _) = tool(dir.path());
        let patch = "*** Begin Patch\n*** Add File: link/x\n+one\n*** Add File: real/x\n+two\n*** Add File: other/y\n+three\n*** End Patch\n";
        let outcome = execute(&tool, patch).await;
        assert_eq!(outcome.status, ToolStatus::Ok, "{outcome:?}");
        assert_eq!(outcome.content, "A link/x\nA real/x\nA other/y");
        assert_eq!(read(dir.path(), "real/x"), "two\n");
        assert_eq!(read(dir.path(), "other/y"), "three\n");
    }

    /// The alias detection canonicalizes the deepest existing ancestor, so two adds under
    /// a subdirectory that does not exist yet are one file.
    #[cfg(unix)]
    #[tokio::test]
    async fn alias_adds_under_a_shared_missing_subdirectory_are_applied_in_order() {
        use std::os::unix::fs::symlink;
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "real/kept.txt", "k\n");
        symlink("real", dir.path().join("link")).unwrap();
        let (tool, _) = tool(dir.path());
        let patch = "*** Begin Patch\n*** Add File: link/sub/x\n+one\n*** Add File: real/sub/x\n+two\n*** End Patch\n";
        let outcome = execute(&tool, patch).await;
        assert_eq!(outcome.status, ToolStatus::Ok, "{outcome:?}");
        assert_eq!(outcome.content, "A link/sub/x\nA real/sub/x");
        assert_eq!(read(dir.path(), "real/sub/x"), "two\n");
    }

    /// A resolved path with non-UTF-8 bytes (an in-workspace symlink target) must be
    /// patched: the commit re-resolves the model's original request, not a lossily
    /// re-encoded resolved path.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_non_utf8_symlink_target_is_patched() {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;
        use std::os::unix::fs::symlink;
        let dir = tempfile::tempdir().unwrap();
        let weird = OsStr::from_bytes(b"weird-\xff");
        std::fs::write(dir.path().join(weird), "a\n").unwrap();
        symlink(weird, dir.path().join("link")).unwrap();
        let (tool, _) = tool(dir.path());
        let outcome = execute(
            &tool,
            "*** Begin Patch\n*** Update File: link\n@@\n-a\n+b\n*** End Patch\n",
        )
        .await;
        assert_eq!(outcome.status, ToolStatus::Ok, "{outcome:?}");
        assert_eq!(
            std::fs::read_to_string(dir.path().join(weird)).unwrap(),
            "b\n"
        );
    }

    /// A native `Add File` over a dangling symlink replaces the link entry, as
    /// `write_atomic` did, instead of refusing the create-only target.
    #[cfg(unix)]
    #[tokio::test]
    async fn an_add_over_a_dangling_link_replaces_the_link() {
        use std::os::unix::fs::symlink;
        let dir = tempfile::tempdir().unwrap();
        symlink("missing.txt", dir.path().join("link.txt")).unwrap();
        let (tool, _) = tool(dir.path());
        let outcome = execute(
            &tool,
            "*** Begin Patch\n*** Add File: link.txt\n+x\n*** End Patch\n",
        )
        .await;
        assert_eq!(outcome.status, ToolStatus::Ok, "{outcome:?}");
        assert_eq!(read(dir.path(), "link.txt"), "x\n");
        assert!(!dir.path().join("missing.txt").exists());
        assert!(
            !std::fs::symlink_metadata(dir.path().join("link.txt"))
                .unwrap()
                .file_type()
                .is_symlink()
        );
    }

    /// A native move whose destination is a dangling symlink replaces the link entry too.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_move_over_a_dangling_link_replaces_the_link() {
        use std::os::unix::fs::symlink;
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "from.txt", "a\n");
        symlink("missing.txt", dir.path().join("link.txt")).unwrap();
        let (tool, _) = tool(dir.path());
        let outcome = execute(
            &tool,
            "*** Begin Patch\n*** Update File: from.txt\n*** Move to: link.txt\n*** End Patch\n",
        )
        .await;
        assert_eq!(outcome.status, ToolStatus::Ok, "{outcome:?}");
        assert_eq!(read(dir.path(), "link.txt"), "a\n");
        assert!(!dir.path().join("from.txt").exists());
        assert!(!dir.path().join("missing.txt").exists());
    }
}
