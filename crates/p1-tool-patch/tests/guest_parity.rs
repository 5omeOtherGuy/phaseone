//! The component's guest logic against the native tool, over the real workspace service.
//!
//! `p1_tool_patch_logic::execute` is what the `p1/patch` component runs; here its
//! capabilities are backed by `p1-workspace` exactly as the host backs the WIT imports
//! (`stat`, `read` and the owned mutation of U-mut, assembled patch-authorized), so each
//! scenario runs once through the native `PatchTool` and once through the guest logic on
//! identical directories, and the outputs, the resulting files and the recorded
//! observations must be identical.

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use p1_contracts::{Tool, ToolCall, ToolContext, ToolInput, ToolStatus};
use p1_tool_patch::PatchTool;
use p1_tool_patch_logic::{
    Entry, EntryKind, FsError, Host as Capabilities, Mutation, Outcome, RawInput, Status, execute,
};
use p1_workspace::{
    MutationError, MutationPolicy, Observation, ObservedFiles, OwnedMutation, ReadRecord,
    Workspace, WorkspaceError,
};

/// The call read record of `docs/design/modules/workspace-mutation.md` ("Per-agent state the
/// host binds into a call"): the whole file as it was when this call last read it, keyed by
/// its resolved path. The host rechecks every change under the gate against it (step 3), so
/// a change to a target whose contents differ from what this call read is refused as stale.
#[derive(Default)]
struct CallReads(HashMap<String, Option<Vec<u8>>>);

/// The host side of the imports, over one agent's workspace and observations.
struct Host {
    workspace: Workspace,
    observed: ObservedFiles,
    runtime: tokio::runtime::Runtime,
    reads: Rc<RefCell<CallReads>>,
    changes: Rc<RefCell<Vec<String>>>,
}

struct HeldMutation {
    mutation: OwnedMutation,
    workspace: Workspace,
    reads: Rc<RefCell<CallReads>>,
    changes: Rc<RefCell<Vec<String>>>,
}

/// The host's mapping of the native service's failures onto the frozen `fs-error`
/// (docs/design/modules/workspace-mutation.md): a directory where a file was read is the
/// typed `wrong-kind`, and `io` carries the io error's own text, never a host path.
fn workspace_error(error: WorkspaceError) -> FsError {
    match error {
        WorkspaceError::OutsideWorkspace { .. } => FsError::OutsideWorkspace,
        WorkspaceError::NotFound { .. } => FsError::NotFound,
        WorkspaceError::NotADirectory(_) => FsError::WrongKind,
        WorkspaceError::Io { source, .. } => FsError::Io(source.to_string()),
    }
}

fn mutation_error(error: MutationError) -> FsError {
    match error {
        MutationError::OutsideWorkspace { .. } => FsError::OutsideWorkspace,
        MutationError::NotFound { .. } => FsError::NotFound,
        MutationError::WrongKind { .. } => FsError::WrongKind,
        MutationError::AlreadyExists { .. } => FsError::AlreadyExists,
        MutationError::Io(message) => FsError::Io(message),
    }
}

impl HeldMutation {
    /// Step 3's call read record check, and the harness's record of what the guest asked
    /// for: one line per change, with the resolved path the host acted on.
    fn record(&self, operation: &str, path: &str) -> Result<(), FsError> {
        let checked = self.workspace.check_path(path).map_err(workspace_error)?;
        let key = checked.path().to_string_lossy().into_owned();
        if let Some(read) = self.reads.borrow().0.get(&key) {
            let current = std::fs::read(checked.path()).ok();
            if *read != current {
                return Err(FsError::Io(format!(
                    "{} changed on disk since you last read it; read it again.",
                    checked.display()
                )));
            }
        }
        self.changes
            .borrow_mut()
            .push(format!("{operation} {}", checked.display()));
        Ok(())
    }
}

impl Mutation for HeldMutation {
    fn write(&self, path: &str, contents: &[u8]) -> Result<(), FsError> {
        self.record("write", path)?;
        self.mutation
            .write(path, contents.to_vec())
            .map_err(mutation_error)
    }

    fn create(&self, path: &str, contents: &[u8]) -> Result<(), FsError> {
        self.record("create", path)?;
        self.mutation
            .create(path, contents.to_vec())
            .map_err(mutation_error)
    }

    fn remove(&self, path: &str) -> Result<(), FsError> {
        self.record("remove", path)?;
        self.mutation.remove(path).map_err(mutation_error)
    }
}

impl Capabilities for Host {
    type Mutation = HeldMutation;

    fn cancelled(&mut self) -> bool {
        false
    }

    fn stat(&mut self, path: &str) -> Result<Entry, FsError> {
        let checked = self.workspace.check_path(path).map_err(workspace_error)?;
        let stat = self.workspace.stat(path).map_err(workspace_error)?;
        Ok(Entry {
            path: checked.display().to_string(),
            kind: match stat.kind {
                p1_workspace::FileKind::File => EntryKind::File,
                p1_workspace::FileKind::Directory => EntryKind::Directory,
                _ => EntryKind::Other,
            },
            size: stat.size,
        })
    }

    fn read(&mut self, path: &str, offset: u64, length: u64) -> Result<Vec<u8>, FsError> {
        let checked = self.workspace.check_path(path).map_err(workspace_error)?;
        // The host digests the whole file on every read call and the latest read of a path
        // wins, whatever window the component asked for.
        let whole = std::fs::read(checked.path()).ok();
        self.reads
            .borrow_mut()
            .0
            .insert(checked.path().to_string_lossy().into_owned(), whole);
        let snapshot = self
            .workspace
            .read_unobserved(path)
            .map_err(workspace_error)?;
        Ok(snapshot
            .read(
                usize::try_from(offset).unwrap(),
                usize::try_from(length).unwrap(),
            )
            .to_vec())
    }

    fn begin(&mut self) -> HeldMutation {
        // The patch exemption: the host assembles the patch component's mutation
        // patch-authorized, so no prior observation is required. The read record is
        // empty: [`HeldMutation`] models step 3's recheck over its own [`CallReads`], and
        // the native tool this compares against carries no record either.
        HeldMutation {
            mutation: self.runtime.block_on(self.workspace.begin_owned(
                &self.observed,
                &ReadRecord::new(),
                MutationPolicy::PatchAuthorized,
            )),
            workspace: self.workspace.clone(),
            reads: Rc::clone(&self.reads),
            changes: Rc::clone(&self.changes),
        }
    }
}

/// One side of a comparison: a fresh directory with the scenario's files.
struct Side {
    _dir: tempfile::TempDir,
    root: PathBuf,
    observed: ObservedFiles,
}

impl Side {
    fn new(files: &[(&str, &[u8])]) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        std::fs::create_dir_all(root.join("sub")).unwrap();
        for (path, contents) in files {
            let target = root.join(path);
            std::fs::create_dir_all(target.parent().unwrap()).unwrap();
            std::fs::write(target, contents).unwrap();
        }
        Self {
            _dir: dir,
            root,
            observed: ObservedFiles::new(),
        }
    }

    fn workspace(&self) -> Workspace {
        Workspace::new(&self.root).unwrap()
    }

    /// Every entry under the root, recursively, with a file's bytes and a symlink's
    /// target; nothing is followed.
    fn contents(&self) -> Vec<(String, String)> {
        let mut entries = Vec::new();
        walk(&self.root, &self.root, &mut entries);
        entries.sort();
        entries
    }

    /// Whether this side's agent has observed `path` with exactly `contents`.
    fn observed(&self, path: &str, contents: &[u8]) -> bool {
        self.observed
            .check_unchanged(&self.root.join(path), contents)
            == Observation::Unchanged
    }
}

fn walk(root: &Path, dir: &Path, entries: &mut Vec<(String, String)>) {
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        let name = path
            .strip_prefix(root)
            .unwrap()
            .to_string_lossy()
            .into_owned();
        let metadata = std::fs::symlink_metadata(&path).unwrap();
        if metadata.is_symlink() {
            let target = std::fs::read_link(&path).unwrap();
            entries.push((name, format!("-> {}", target.display())));
        } else if metadata.is_dir() {
            entries.push((format!("{name}/"), String::new()));
            walk(root, &path, entries);
        } else {
            let bytes = std::fs::read(&path).unwrap();
            entries.push((name, String::from_utf8_lossy(&bytes).into_owned()));
        }
    }
}

fn native(side: &Side, input: &ToolInput, function: bool) -> Outcome {
    let tool = PatchTool::new(side.workspace(), side.observed.clone());
    let tool = if function { tool.function_face() } else { tool };
    let call = ToolCall {
        call_id: "call-1".into(),
        name: "apply_patch".into(),
        input: input.clone(),
    };
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();
    let outcome = runtime.block_on(tool.execute(
        &call,
        ToolContext {
            cancel: p1_contracts::CancellationToken::new(),
        },
    ));
    Outcome {
        status: match outcome.status {
            ToolStatus::Ok => Status::Ok,
            ToolStatus::Error => Status::Error,
            ToolStatus::Cancelled => Status::Cancelled,
            other => panic!("the native patch tool returned {other:?}"),
        },
        content: outcome.content,
    }
}

fn guest(side: &Side, input: &ToolInput, function: bool) -> Outcome {
    guest_recording(side, input, function).0
}

/// As [`guest`], with the changes the guest asked the host for: one line per change, in
/// order, naming the resolved path the host acted on.
fn guest_recording(side: &Side, input: &ToolInput, function: bool) -> (Outcome, Vec<String>) {
    let mut host = Host {
        workspace: side.workspace(),
        observed: side.observed.clone(),
        runtime: tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap(),
        reads: Rc::new(RefCell::new(CallReads::default())),
        changes: Rc::new(RefCell::new(Vec::new())),
    };
    let raw = match input {
        ToolInput::Json(raw) => RawInput::Json(raw),
        ToolInput::Text(raw) => RawInput::Text(raw),
    };
    let outcome = execute(&mut host, "apply_patch", !function, raw);
    let changes = host.changes.borrow().clone();
    (outcome, changes)
}

/// `ROOT` in an input stands for each side's own root, so absolute paths compare.
fn rooted(input: &ToolInput, root: &Path) -> ToolInput {
    let root = root.to_str().unwrap();
    match input {
        ToolInput::Json(raw) => ToolInput::Json(raw.replace("ROOT", root)),
        ToolInput::Text(raw) => ToolInput::Text(raw.replace("ROOT", root)),
    }
}

fn normalized(outcome: Outcome, root: &Path) -> Outcome {
    Outcome {
        status: outcome.status,
        content: outcome.content.replace(root.to_str().unwrap(), "ROOT"),
    }
}

/// Run `inputs` in order on both sides, which start from the same files, and require
/// identical outcomes and identical directories after every call.
fn same_on(
    native_side: &Side,
    guest_side: &Side,
    inputs: &[ToolInput],
    function: bool,
) -> Vec<Outcome> {
    let mut outcomes = Vec::new();
    for input in inputs {
        let expected = normalized(
            native(native_side, &rooted(input, &native_side.root), function),
            &native_side.root,
        );
        let actual = normalized(
            guest(guest_side, &rooted(input, &guest_side.root), function),
            &guest_side.root,
        );
        assert_eq!(actual, expected, "input {input:?}");
        assert_eq!(
            guest_side.contents(),
            native_side.contents(),
            "input {input:?}"
        );
        outcomes.push(expected);
    }
    outcomes
}

fn same(files: &[(&str, &[u8])], inputs: &[ToolInput]) -> Vec<Outcome> {
    same_on(&Side::new(files), &Side::new(files), inputs, false)
}

fn text(patch: &str) -> ToolInput {
    ToolInput::Text(patch.to_string())
}

fn ok(content: &str) -> Outcome {
    Outcome {
        status: Status::Ok,
        content: content.into(),
    }
}

fn error(content: &str) -> Outcome {
    Outcome {
        status: Status::Error,
        content: content.into(),
    }
}

#[test]
fn a_multi_file_patch_matches_and_records_what_it_wrote() {
    let files: &[(&str, &[u8])] = &[
        (
            "path/to/file.rs",
            b"fn existing_function\nunchanged context line\nremoved line\n",
        ),
        ("path/to/old_file.rs", b"old\n"),
    ];
    let native_side = Side::new(files);
    let guest_side = Side::new(files);
    let outcomes = same_on(
        &native_side,
        &guest_side,
        &[text(
            "*** Begin Patch\n*** Update File: path/to/file.rs\n@@ fn existing_function\n unchanged context line\n-removed line\n+added line\n*** Add File: path/to/new_file.rs\n+first line\n*** Delete File: path/to/old_file.rs\n*** End Patch\n",
        )],
        false,
    );
    assert_eq!(
        outcomes[0],
        ok("M path/to/file.rs\nA path/to/new_file.rs\nD path/to/old_file.rs")
    );
    // Neither side had observed anything (the patch exemption); both recorded the files
    // they wrote — the native tool itself, the guest through the host commit that applies
    // its changes (the component imports no `snapshot`).
    let modified = b"fn existing_function\nunchanged context line\nadded line\n";
    for side in [&native_side, &guest_side] {
        assert!(side.observed("path/to/file.rs", modified));
        assert!(side.observed("path/to/new_file.rs", b"first line\n"));
    }
}

#[test]
fn hunk_location_line_endings_and_eof_match_the_native_tool() {
    same(
        &[
            ("later.txt", b"ctx\nA\nctx\nA\nB\n"),
            ("tab.txt", b"ctx\n\talpha\n"),
            ("crlf.txt", b"a\r\nb\r\n"),
            ("eof.txt", b"a\nb\n"),
            ("plain.txt", b"a\nb\n"),
            ("blank.txt", b"a\n\nb\n"),
            ("bare.txt", b"a\nb"),
        ],
        &[
            text(
                "*** Begin Patch\n*** Update File: later.txt\n@@ ctx\n-A\n+first\n@@ ctx\n-A\n+second\n*** End Patch\n",
            ),
            text(
                "*** Begin Patch\n*** Update File: tab.txt\n@@\n ctx\n-\talpha   \n+\tbeta\n*** End Patch\n",
            ),
            text("*** Begin Patch\n*** Update File: crlf.txt\n@@\n-b\n+c\n*** End Patch\n"),
            text(
                "*** Begin Patch\n*** Update File: eof.txt\n@@\n+c\n*** End of File\n*** End Patch\n",
            ),
            text("*** Begin Patch\n*** Update File: plain.txt\n+x\n*** End Patch\n"),
            text("*** Begin Patch\n*** Update File: blank.txt\n@@\n a\n\n-b\n+c\n*** End Patch\n"),
            text("*** Begin Patch\n*** Update File: bare.txt\n-b\n+c\n*** End Patch\n"),
        ],
    );
}

#[test]
fn a_failing_later_file_leaves_every_file_untouched_on_both_sides() {
    let outcomes = same(
        &[
            ("first.txt", b"one\ntwo\nthree\n"),
            ("second.txt", b"alpha\n"),
        ],
        &[text(
            "*** Begin Patch\n*** Update File: first.txt\n-one\n+ONE\n*** Update File: second.txt\n-missing\n+other\n*** End Patch\n",
        )],
    );
    assert_eq!(
        outcomes[0],
        error("second.txt: hunk 1 did not match the file.")
    );
}

#[test]
fn moves_adds_and_deletes_match_the_native_tool() {
    let outcomes = same(
        &[
            ("old.txt", b"x\ny\n"),
            ("pure.txt", b"p\n"),
            ("taken.txt", b"t\n"),
            ("mover.txt", b"m\n"),
            ("gone.txt", b"bye\n"),
            ("exists.txt", b"original\n"),
        ],
        &[
            text(
                "*** Begin Patch\n*** Update File: old.txt\n*** Move to: sub/new.txt\n@@\n-y\n+z\n*** End Patch\n",
            ),
            text(
                "*** Begin Patch\n*** Update File: pure.txt\n*** Move to: deep/er/pure.txt\n*** End Patch\n",
            ),
            text(
                "*** Begin Patch\n*** Update File: mover.txt\n*** Move to: taken.txt\n*** End Patch\n",
            ),
            text("*** Begin Patch\n*** Delete File: gone.txt\n*** End Patch\n"),
            text("*** Begin Patch\n*** Delete File: gone.txt\n*** End Patch\n"),
            text("*** Begin Patch\n*** Add File: exists.txt\n+replacement\n*** End Patch\n"),
            text("*** Begin Patch\n*** Update File: missing.txt\n-a\n+b\n*** End Patch\n"),
            // A later hunk sees an earlier one's staged result, on both sides.
            text(
                "*** Begin Patch\n*** Add File: staged.txt\n+one\n*** Update File: staged.txt\n-one\n+two\n*** Update File: ROOT/staged.txt\n*** Move to: sub/staged.txt\n*** End Patch\n",
            ),
            text(
                "*** Begin Patch\n*** Delete File: exists.txt\n*** Add File: exists.txt\n+again\n*** End Patch\n",
            ),
        ],
    );
    assert_eq!(outcomes[0], ok("M old.txt -> sub/new.txt"));
    assert_eq!(outcomes[2], error("taken.txt already exists."));
    assert_eq!(outcomes[4], error("gone.txt does not exist."));
    assert_eq!(
        outcomes[7],
        ok("A staged.txt\nM staged.txt\nM staged.txt -> sub/staged.txt")
    );
}

/// The host rechecks every change under the gate against the call read record the harness
/// keeps (`HeldMutation::record`), so a second change to a path this call already wrote is
/// refused as stale, exactly as `docs/design/modules/workspace-mutation.md` step 3 says.
/// The guest coalesces the planned ops instead of writing one path twice, and this pins
/// that: the scenario passes, and the host is asked for one change per resolved path.
#[test]
fn a_path_the_patch_reaches_twice_is_changed_once() {
    let files: &[(&str, &[u8])] = &[("f.txt", b"a\n"), ("exists.txt", b"x\n")];
    let native_side = Side::new(files);
    let guest_side = Side::new(files);
    let cases = [
        (
            // Two Update File hunks on one file.
            "*** Begin Patch\n*** Update File: f.txt\n-a\n+b\n*** Update File: f.txt\n-b\n+c\n*** End Patch",
            "M f.txt\nM f.txt",
            "write f.txt",
        ),
        (
            // A file deleted and added again: one replacement, not an unlink and a create.
            "*** Begin Patch\n*** Delete File: exists.txt\n*** Add File: exists.txt\n+again\n*** End Patch",
            "D exists.txt\nA exists.txt",
            "write exists.txt",
        ),
    ];
    for (patch, expected, change) in cases {
        let input = text(patch);
        let native_outcome = normalized(
            native(&native_side, &rooted(&input, &native_side.root), false),
            &native_side.root,
        );
        let (guest_outcome, changes) =
            guest_recording(&guest_side, &rooted(&input, &guest_side.root), false);
        assert_eq!(native_outcome, ok(expected), "{patch}");
        assert_eq!(
            normalized(guest_outcome, &guest_side.root),
            native_outcome,
            "{patch}"
        );
        assert_eq!(changes, [change.to_string()], "{patch}");
        assert_eq!(guest_side.contents(), native_side.contents(), "{patch}");
    }
}

#[test]
fn confinement_matches_the_native_tool() {
    let outside = tempfile::tempdir().unwrap();
    std::fs::write(outside.path().join("secret"), "secret\n").unwrap();
    let native_side = Side::new(&[("real.txt", b"r\n")]);
    let guest_side = Side::new(&[("real.txt", b"r\n")]);
    for side in [&native_side, &guest_side] {
        std::os::unix::fs::symlink(outside.path(), side.root.join("link")).unwrap();
        std::os::unix::fs::symlink("real.txt", side.root.join("alias.txt")).unwrap();
    }
    let outcomes = same_on(
        &native_side,
        &guest_side,
        &[
            text("*** Begin Patch\n*** Add File: ../x\n+hello\n*** End Patch\n"),
            text("*** Begin Patch\n*** Add File: /definitely/outside.txt\n+hello\n*** End Patch\n"),
            text("*** Begin Patch\n*** Update File: link/secret\n-secret\n+new\n*** End Patch\n"),
            text("*** Begin Patch\n*** Add File: link/new.txt\n+x\n*** End Patch\n"),
            text(
                "*** Begin Patch\n*** Update File: real.txt\n*** Move to: ../moved.txt\n*** End Patch\n",
            ),
            // An in-root symlink is patched as its target, displayed as the target.
            text("*** Begin Patch\n*** Update File: alias.txt\n-r\n+R\n*** End Patch\n"),
            // Absolute in-workspace paths display relative to the root.
            text(
                "*** Begin Patch\n*** Add File: ROOT/sub/./abs.txt\n+a\n*** Update File: ROOT/real.txt\n-R\n+RR\n*** End Patch\n",
            ),
        ],
        false,
    );
    assert_eq!(outcomes[0], error("path escapes workspace: ../x"));
    assert_eq!(outcomes[5], ok("M real.txt"));
    assert_eq!(outcomes[6], ok("A sub/abs.txt\nM real.txt"));
    assert_eq!(
        std::fs::read_to_string(outside.path().join("secret")).unwrap(),
        "secret\n"
    );
    assert!(!outside.path().join("new.txt").exists());
}

#[test]
fn wrong_kinds_and_encodings_match_the_native_tool() {
    let outcomes = same(
        &[("d.txt", b"one\n"), ("bin.txt", &[0xff, 0xfe, b'a', b'\n'])],
        &[
            text("*** Begin Patch\n*** Update File: d.txt/x\n-a\n+b\n*** End Patch\n"),
            text("*** Begin Patch\n*** Delete File: ROOT/d.txt/x\n*** End Patch\n"),
            text("*** Begin Patch\n*** Update File: sub\n-a\n+b\n*** End Patch\n"),
            text("*** Begin Patch\n*** Delete File: sub\n*** End Patch\n"),
            text("*** Begin Patch\n*** Add File: sub\n+a\n*** End Patch\n"),
            text("*** Begin Patch\n*** Add File: d.txt/x\n+a\n*** End Patch\n"),
            text("*** Begin Patch\n*** Update File: bin.txt\n-a\n+b\n*** End Patch\n"),
        ],
    );
    assert_eq!(
        outcomes[0],
        error("d.txt/x could not be read: Not a directory (os error 20)")
    );
    assert_eq!(outcomes[2], error("sub is not a regular file."));
    assert_eq!(
        outcomes[5],
        error("failed to write d.txt/x: File exists (os error 17)")
    );
    assert_eq!(outcomes[6], error("bin.txt is not valid UTF-8."));
}

/// The divergences from the native texts that the component cannot close from its side,
/// pinned so that they cannot grow silently; U-patch.2 carries them to the production host.
///
/// - The native move names its target by the absolute host path when writing it fails
///   (`to.display()`, not the root-relative display every other text uses). A component
///   never learns the root, so it shows the root-relative path, as every other native text
///   does.
/// - A target below a file deeper than its direct parent is refused by the host's own
///   resolution ("… could not be resolved: …") where the native `create_dir_all` fails
///   while writing ("failed to write …: …"); that wording is the host's (`p1-workspace`).
#[test]
fn the_texts_the_component_cannot_match_are_pinned() {
    let files: &[(&str, &[u8])] = &[("d.txt", b"one\n")];
    let cases = [
        (
            "*** Begin Patch\n*** Update File: d.txt\n*** Move to: d.txt/moved\n*** End Patch\n",
            "failed to write ROOT/d.txt/moved: File exists (os error 17)",
            "failed to write d.txt/moved: File exists (os error 17)",
        ),
        (
            "*** Begin Patch\n*** Add File: d.txt/deeper/x\n+a\n*** End Patch\n",
            "failed to write d.txt/deeper/x: Not a directory (os error 20)",
            "d.txt/deeper/x could not be resolved: Not a directory (os error 20)",
        ),
    ];
    for (patch, native_text, guest_text) in cases {
        let native_side = Side::new(files);
        let guest_side = Side::new(files);
        let input = text(patch);
        assert_eq!(
            normalized(native(&native_side, &input, false), &native_side.root),
            error(native_text)
        );
        assert_eq!(
            normalized(guest(&guest_side, &input, false), &guest_side.root),
            error(guest_text)
        );
        // Whatever the text, both sides leave the workspace as it was.
        assert_eq!(guest_side.contents(), native_side.contents());
    }
}

/// A not-yet-existing file named through an in-root directory symlink (owner decision
/// 2026-10-01, X4): `link/x` and `real/x` (with `link -> real`) are one file, so a second
/// addition of it is refused while planning, on both sides, with the same text and before
/// any write — no partial first write through the alias. Either spelling first, a missing
/// subdirectory below the alias, or a third operation in the patch changes nothing of that.
#[test]
fn an_absent_path_through_a_directory_symlink_is_refused_before_any_write_on_both_sides() {
    let files: &[(&str, &[u8])] = &[("real/kept.txt", b"k\n")];
    let cases = [
        (
            "*** Begin Patch\n*** Add File: link/x\n+one\n*** Add File: real/x\n+two\n*** End Patch\n",
            "real/x already exists.",
        ),
        (
            "*** Begin Patch\n*** Add File: real/x\n+one\n*** Add File: link/x\n+two\n*** End Patch\n",
            "link/x already exists.",
        ),
        (
            "*** Begin Patch\n*** Add File: link/sub/x\n+one\n*** Add File: real/sub/x\n+two\n*** End Patch\n",
            "real/sub/x already exists.",
        ),
        (
            "*** Begin Patch\n*** Add File: other/y\n+zero\n*** Add File: link/x\n+one\n*** Add File: real/x\n+two\n*** End Patch\n",
            "real/x already exists.",
        ),
        (
            "*** Begin Patch\n*** Update File: real/kept.txt\n-k\n+K\n*** Add File: ROOT/link/x\n+one\n*** Add File: real/./x\n+two\n*** End Patch\n",
            "real/x already exists.",
        ),
    ];
    for (patch, expected) in cases {
        let native_side = Side::new(files);
        let guest_side = Side::new(files);
        for side in [&native_side, &guest_side] {
            std::os::unix::fs::symlink("real", side.root.join("link")).unwrap();
        }
        let input = text(patch);
        let native_outcome = normalized(
            native(&native_side, &rooted(&input, &native_side.root), false),
            &native_side.root,
        );
        let (guest_outcome, changes) =
            guest_recording(&guest_side, &rooted(&input, &guest_side.root), false);
        assert_eq!(native_outcome, error(expected), "{patch}");
        assert_eq!(
            normalized(guest_outcome, &guest_side.root),
            native_outcome,
            "{patch}"
        );
        // Refused while planning: the guest asked the host for no change at all, and both
        // trees are exactly the scenario's, nothing written through either spelling.
        assert!(changes.is_empty(), "{patch}: {changes:?}");
        for side in [&native_side, &guest_side] {
            assert_eq!(
                side.contents(),
                [
                    ("link".to_string(), "-> real".to_string()),
                    ("real/".to_string(), String::new()),
                    ("real/kept.txt".to_string(), "k\n".to_string()),
                    ("sub/".to_string(), String::new()),
                ],
                "{patch}"
            );
        }
    }
}

/// The other side of X4: as one file, a path added through the alias is the file a later
/// hunk updates through the real directory, on both sides, and is written once.
#[test]
fn an_absent_path_through_a_directory_symlink_is_one_file_for_later_hunks() {
    let files: &[(&str, &[u8])] = &[("real/kept.txt", b"k\n")];
    let native_side = Side::new(files);
    let guest_side = Side::new(files);
    for side in [&native_side, &guest_side] {
        std::os::unix::fs::symlink("real", side.root.join("link")).unwrap();
    }
    let input = text(
        "*** Begin Patch\n*** Add File: link/sub/x\n+one\n*** Update File: real/sub/x\n-one\n+two\n*** End Patch\n",
    );
    let native_outcome = normalized(native(&native_side, &input, false), &native_side.root);
    let (guest_outcome, changes) = guest_recording(&guest_side, &input, false);
    assert_eq!(native_outcome, ok("A link/sub/x\nM real/sub/x"));
    assert_eq!(normalized(guest_outcome, &guest_side.root), native_outcome);
    // One change, named by the spelling that first reached the file.
    assert_eq!(changes, ["create link/sub/x"]);
    assert_eq!(guest_side.contents(), native_side.contents());
    assert_eq!(
        std::fs::read_to_string(guest_side.root.join("real/sub/x")).unwrap(),
        "two\n"
    );
}

/// X3 over the real workspace: a path one patch adds and deletes again is never written on
/// either side, and both report every op.
#[test]
fn a_path_added_and_deleted_by_one_patch_is_never_written_on_both_sides() {
    let files: &[(&str, &[u8])] = &[("f.txt", b"a\n")];
    let native_side = Side::new(files);
    let guest_side = Side::new(files);
    let input = text(
        "*** Begin Patch\n*** Add File: n.txt\n+x\n*** Delete File: n.txt\n*** Update File: f.txt\n-a\n+b\n*** End Patch\n",
    );
    let native_outcome = normalized(native(&native_side, &input, false), &native_side.root);
    let (guest_outcome, changes) = guest_recording(&guest_side, &input, false);
    assert_eq!(native_outcome, ok("A n.txt\nD n.txt\nM f.txt"));
    assert_eq!(normalized(guest_outcome, &guest_side.root), native_outcome);
    assert_eq!(changes, ["write f.txt"]);
    assert_eq!(guest_side.contents(), native_side.contents());
    assert!(!guest_side.root.join("n.txt").exists());
}

#[test]
fn wrappers_garbage_and_input_kinds_match_the_native_tool() {
    same(
        &[("f.txt", b"a\nb\n")],
        &[
            text(
                "<<'EOF'\r\n*** Begin Patch\r\n*** Update File: f.txt\r\n@@\r\n-b\r\n+c\r\n*** End Patch\r\nEOF\r\n",
            ),
            text("```diff\n*** Begin Patch\n*** Update File: f.txt\n-c\n+d\n*** End Patch\n```\n"),
            text("*** Begin Patch\n*** Update File: f.txt\n@@\n-d\n+e\n*** End Patch"),
            text(""),
            text("*** Update File: f.txt\n@@\n-a\n+b\n*** End Patch\n"),
            text("*** Begin Patch\n*** Add File: z.txt\n+x\n"),
            text("*** Begin Patch\n*** End Patch\n"),
            text("*** Begin Patch\n*** Frobnicate: x\n*** End Patch\n"),
            text("*** Begin Patch\n*** Update File: f.txt\n@@ a\n*** End Patch\n"),
            text("\u{0}\u{1}not a patch at all"),
            ToolInput::Json("{}".into()),
        ],
    );
}

#[test]
fn the_function_form_matches_the_native_function_face() {
    let files: &[(&str, &[u8])] = &[("f.txt", b"a\nb\n")];
    let outcomes = same_on(
        &Side::new(files),
        &Side::new(files),
        &[
            ToolInput::Json(
                serde_json::json!({
                    "patch": "*** Begin Patch\n*** Update File: f.txt\n-b\n+B\n*** End Patch\n"
                })
                .to_string(),
            ),
            ToolInput::Json(String::new()),
            ToolInput::Json("null".into()),
            ToolInput::Json("{\"patch\": 5}".into()),
            ToolInput::Json("{\"patch\":\"x\",\"extra\":1}".into()),
            text("*** Begin Patch\n*** End Patch\n"),
        ],
        true,
    );
    assert_eq!(outcomes[0], ok("M f.txt"));
}

#[test]
fn the_logic_crates_output_bound_is_the_workspace_services() {
    let long_line = "x".repeat(60_000);
    let many_lines = "line\n".repeat(3_000);
    let multibyte = "é".repeat(30_000);
    for (text, bytes, lines) in [
        ("short", 50_000, 2_000),
        (long_line.as_str(), 50_000, 2_000),
        (many_lines.as_str(), 50_000, 2_000),
        (multibyte.as_str(), 50_001, 2_000),
        ("a\nb\nc\n", 100, 2),
        ("a\nb\nc", 100, 2),
        ("", 0, 0),
    ] {
        assert_eq!(
            p1_tool_patch_logic::bound_output(text, bytes, lines),
            p1_workspace::bound_output(text, bytes, lines),
        );
    }
}
