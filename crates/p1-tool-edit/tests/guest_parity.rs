//! The component's guest logic against the native tool, over the real workspace service.
//!
//! `p1_tool_edit_logic::exec::execute` is what the `edit` component runs; here its
//! capabilities are backed by `p1-workspace` exactly as the host backs the WIT imports
//! (`stat`, `read`, `check`, `observe` and the owned mutation of U-mut), so each scenario
//! runs once through the native `EditTool` and once through the guest logic on identical
//! directories, and the outputs and resulting files must be byte-identical.

use std::path::{Path, PathBuf};

use p1_contracts::{Tool, ToolCall, ToolContext, ToolInput, ToolStatus};
use p1_tool_edit::EditTool;
use p1_tool_edit_logic::exec::{
    CallInput, Capabilities, Entry, EntryKind, FsError, Mutation, Observation, Outcome, execute,
};
use p1_workspace::{
    MutationError, MutationPolicy, ObservedFiles, OwnedMutation, Workspace, WorkspaceError,
};

/// The host side of the four imports, over one agent's workspace and observations.
struct Host {
    workspace: Workspace,
    observed: ObservedFiles,
    runtime: tokio::runtime::Runtime,
}

struct HeldMutation(OwnedMutation);

/// The host's mapping of the native service's failures onto the frozen `fs-error`
/// (docs/design/modules/workspace-mutation.md): a directory where a file was read is the
/// typed `wrong-kind`, and `io` carries the message the native tool itself prints for the
/// failure — the io error's own text, never a host path — so the guest's own wording
/// (`{path} could not be read: …`) matches the native tool's byte for byte.
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

impl Mutation for HeldMutation {
    fn write(&self, path: &str, contents: &[u8]) -> Result<(), FsError> {
        self.0
            .write(path, contents.to_vec())
            .map_err(mutation_error)
    }
}

impl Capabilities for Host {
    type Mutation = HeldMutation;

    fn cancelled(&self) -> bool {
        false
    }

    fn stat(&self, path: &str) -> Result<Entry, FsError> {
        let checked = self.workspace.check_path(path).map_err(workspace_error)?;
        let stat = self.workspace.stat(path).map_err(workspace_error)?;
        Ok(Entry {
            path: checked.display().to_string(),
            kind: match stat.kind {
                p1_workspace::FileKind::File => EntryKind::File,
                p1_workspace::FileKind::Directory => EntryKind::Directory,
                _ => EntryKind::Other,
            },
        })
    }

    fn read(&self, path: &str, offset: u64, length: u64) -> Result<Vec<u8>, FsError> {
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

    fn check(&self, path: &str, current: &[u8]) -> Result<Observation, FsError> {
        let checked = self.workspace.check_path(path).map_err(workspace_error)?;
        Ok(
            match self.observed.check_unchanged(checked.path(), current) {
                p1_workspace::Observation::NeverObserved => Observation::NeverObserved,
                p1_workspace::Observation::Unchanged => Observation::Unchanged,
                p1_workspace::Observation::ChangedSinceObserved => {
                    Observation::ChangedSinceObserved
                }
            },
        )
    }

    fn observe(&self, path: &str, contents: &[u8]) -> Result<(), FsError> {
        let checked = self.workspace.check_path(path).map_err(workspace_error)?;
        self.observed.record(checked.path(), contents);
        Ok(())
    }

    fn begin(&self) -> HeldMutation {
        HeldMutation(
            self.runtime.block_on(
                self.workspace
                    .begin_owned(&self.observed, MutationPolicy::Observed),
            ),
        )
    }
}

/// One side of a comparison: a fresh directory with the scenario's files.
struct Side {
    _dir: tempfile::TempDir,
    root: PathBuf,
    observed: ObservedFiles,
}

impl Side {
    fn new(files: &[(&str, &[u8])], read: &[&str]) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        std::fs::create_dir_all(root.join("sub")).unwrap();
        for (path, contents) in files {
            std::fs::write(root.join(path), contents).unwrap();
        }
        let observed = ObservedFiles::new();
        for path in read {
            let path = root.join(path);
            observed.record(&path, &std::fs::read(&path).unwrap());
        }
        Self {
            _dir: dir,
            root,
            observed,
        }
    }

    fn workspace(&self) -> Workspace {
        Workspace::new(&self.root).unwrap()
    }

    fn contents(&self) -> Vec<(String, Vec<u8>)> {
        let mut entries: Vec<(String, Vec<u8>)> = std::fs::read_dir(&self.root)
            .unwrap()
            .map(|entry| {
                let entry = entry.unwrap();
                let bytes = if entry.path().is_file() {
                    std::fs::read(entry.path()).unwrap()
                } else {
                    Vec::new()
                };
                (entry.file_name().to_string_lossy().into_owned(), bytes)
            })
            .collect();
        entries.sort();
        entries
    }
}

fn native(side: &Side, input: &ToolInput) -> Outcome {
    let tool = EditTool::new(side.workspace(), side.observed.clone());
    let call = ToolCall {
        call_id: "call-1".into(),
        name: "edit".into(),
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
    match outcome.status {
        ToolStatus::Ok => Outcome::Ok(outcome.content),
        ToolStatus::Error => Outcome::Error(outcome.content),
        ToolStatus::Cancelled => Outcome::Cancelled,
        other => panic!("the native edit tool returned {other:?}"),
    }
}

fn guest(side: &Side, input: &ToolInput) -> Outcome {
    let host = Host {
        workspace: side.workspace(),
        observed: side.observed.clone(),
        runtime: tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap(),
    };
    let call_input = match input {
        ToolInput::Json(raw) => CallInput::Json(raw),
        ToolInput::Text(raw) => CallInput::Text(raw),
    };
    execute(&host, "edit", call_input)
}

/// Run `inputs` in order on both sides, which start from the same files and reads, and
/// require identical outcomes and identical directories after every call.
fn same(files: &[(&str, &[u8])], read: &[&str], inputs: &[ToolInput]) -> Vec<Outcome> {
    let native_side = Side::new(files, read);
    let guest_side = Side::new(files, read);
    let mut outcomes = Vec::new();
    for input in inputs {
        let input = rooted(input, &native_side.root, &guest_side.root);
        let expected = native(&native_side, &input.0);
        let actual = guest(&guest_side, &input.1);
        assert_eq!(
            normalize(&actual, &guest_side.root),
            normalize(&expected, &native_side.root),
            "input {input:?}"
        );
        assert_eq!(
            guest_side.contents(),
            native_side.contents(),
            "input {input:?}"
        );
        outcomes.push(expected);
    }
    outcomes
}

/// `ROOT` in an input stands for each side's own root, so absolute paths compare.
fn rooted(input: &ToolInput, native_root: &Path, guest_root: &Path) -> (ToolInput, ToolInput) {
    let on = |root: &Path| match input {
        ToolInput::Json(raw) => ToolInput::Json(raw.replace("ROOT", root.to_str().unwrap())),
        ToolInput::Text(raw) => ToolInput::Text(raw.clone()),
    };
    (on(native_root), on(guest_root))
}

fn normalize(outcome: &Outcome, root: &Path) -> Outcome {
    let root = root.to_str().unwrap();
    match outcome {
        Outcome::Ok(text) => Outcome::Ok(text.replace(root, "ROOT")),
        Outcome::Error(text) => Outcome::Error(text.replace(root, "ROOT")),
        Outcome::Cancelled => Outcome::Cancelled,
    }
}

fn edit(path: &str, old: &str, new: &str) -> ToolInput {
    ToolInput::Json(
        serde_json::json!({"file_path": path, "old_string": old, "new_string": new}).to_string(),
    )
}

fn edit_all(path: &str, old: &str, new: &str) -> ToolInput {
    ToolInput::Json(
        serde_json::json!({"file_path": path, "old_string": old, "new_string": new, "replace_all": true})
            .to_string(),
    )
}

#[test]
fn successful_edits_match_the_native_tool() {
    let outcomes = same(
        &[("d.txt", b"one\ntwo\nthree\n")],
        &["d.txt"],
        &[
            edit("d.txt", "two", "TWO"),
            // No re-read: the first write became the observation on both sides.
            edit("d.txt", "one", "ONE"),
            edit("ROOT/d.txt", "three", "3"),
        ],
    );
    assert_eq!(
        outcomes[0],
        Outcome::Ok("Edited d.txt (1 replacement).".into())
    );
}

#[test]
fn replace_all_ambiguity_and_absent_text_match_the_native_tool() {
    let outcomes = same(
        &[("e.txt", b"dup\ndup\ndup\n")],
        &["e.txt"],
        &[
            edit("e.txt", "dup", "x"),
            edit("e.txt", "nope", "x"),
            edit_all("e.txt", "dup", "x"),
        ],
    );
    assert_eq!(
        outcomes[2],
        Outcome::Ok("Edited e.txt (3 replacements).".into())
    );
}

#[test]
fn read_before_mutate_matches_the_native_tool() {
    same(
        &[("d.txt", b"one\ntwo\n")],
        &[],
        &[edit("d.txt", "two", "TWO"), edit("d.txt", "absent", "x")],
    );

    // Changed on disk after the read, on both sides alike.
    let native_side = Side::new(&[("d.txt", b"one\ntwo\n")], &["d.txt"]);
    let guest_side = Side::new(&[("d.txt", b"one\ntwo\n")], &["d.txt"]);
    for side in [&native_side, &guest_side] {
        std::fs::write(side.root.join("d.txt"), b"one\ntwo\nthree\n").unwrap();
    }
    let input = edit("d.txt", "two", "TWO");
    let expected = native(&native_side, &input);
    assert_eq!(guest(&guest_side, &input), expected);
    assert_eq!(
        expected,
        Outcome::Error("d.txt changed on disk since you last read it; read it again.".into())
    );
    assert_eq!(guest_side.contents(), native_side.contents());
}

#[test]
fn line_endings_bom_and_encoding_match_the_native_tool() {
    same(
        &[
            ("crlf.txt", b"one\r\ntwo\r\n"),
            ("cr.txt", b"one\rtwo\r"),
            ("bare.txt", b"one\ntwo"),
            ("bom.txt", "\u{FEFF}alpha\n".as_bytes()),
            ("bin.txt", &[0xff, 0xfe, b'a']),
        ],
        &["crlf.txt", "cr.txt", "bare.txt", "bom.txt", "bin.txt"],
        &[
            edit("crlf.txt", "one\ntwo", "1\n2"),
            edit("cr.txt", "two", "TWO"),
            edit("bare.txt", "two", "TWO"),
            edit("bom.txt", "alpha", "beta"),
            edit("bin.txt", "a", "b"),
        ],
    );
}

#[test]
fn missing_escaping_and_wrong_kind_targets_match_the_native_tool() {
    let outcomes = same(
        &[],
        &[],
        &[
            edit("missing.txt", "a", "b"),
            edit("./sub/../gone.txt", "a", "b"),
            edit("ROOT/sub/new.txt", "a", "b"),
            edit("ROOT/nowhere/deep.txt", "a", "b"),
            edit("sub", "a", "b"),
            edit("../victim.txt", "a", "b"),
            edit("/definitely/outside.txt", "a", "b"),
        ],
    );
    assert_eq!(
        outcomes[2],
        Outcome::Error("sub/new.txt does not exist.".into())
    );
}

/// A target whose component is a file (`<existing-file>/x`) is the native tool's
/// `Not a directory` (`std::fs::read` reports `ENOTDIR` there), while a directory target is
/// its `Is a directory` (`EISDIR`): the two native errors the frozen `wrong-kind` folds
/// together must both come out as the native text.
#[test]
fn a_file_used_as_a_directory_component_matches_the_native_tool() {
    let outcomes = same(
        &[("d.txt", b"one\ntwo\n")],
        &["d.txt"],
        &[
            edit("d.txt/x", "two", "TWO"),
            edit("ROOT/d.txt/x", "two", "TWO"),
            edit("sub", "two", "TWO"),
            edit("sub/x/y", "two", "TWO"),
        ],
    );
    assert_eq!(
        outcomes[0],
        Outcome::Error("d.txt/x could not be read: Not a directory (os error 20)".into())
    );
    assert_eq!(
        outcomes[1],
        Outcome::Error("d.txt/x could not be read: Not a directory (os error 20)".into())
    );
    assert_eq!(
        outcomes[2],
        Outcome::Error("sub could not be read: Is a directory (os error 21)".into())
    );
    assert_eq!(
        outcomes[3],
        Outcome::Error("sub/x/y does not exist.".into())
    );
}

#[test]
fn invalid_input_matches_the_native_tool() {
    same(
        &[],
        &[],
        &[
            ToolInput::Json(String::new()),
            ToolInput::Json("null".into()),
            ToolInput::Json("{\"file_path\": 5}".into()),
            ToolInput::Json(
                "{\"file_path\":\"a.txt\",\"old_string\":\"a\",\"new_string\":\"b\",\"extra\":1}"
                    .into(),
            ),
            ToolInput::Json(
                "{\"file_path\":\"a.txt\",\"old_string\":\"\",\"new_string\":\"b\"}".into(),
            ),
            ToolInput::Json(
                "{\"file_path\":\"a.txt\",\"old_string\":\"a\",\"new_string\":\"a\"}".into(),
            ),
            ToolInput::Text("file_path=a.txt".into()),
        ],
    );
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
            p1_tool_edit_logic::bound_output(text, bytes, lines),
            p1_workspace::bound_output(text, bytes, lines),
        );
    }
}
