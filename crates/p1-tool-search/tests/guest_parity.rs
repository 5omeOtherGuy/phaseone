//! The component's guest logic over the real workspace service, as the host backs the
//! imports `grep` is granted: `control` and the read side of `workspace` (`stat`, `read`
//! without observation, `list-files` and `search` over this crate's walk).
//!
//! The native `GrepTool` runs the same `execute`; what is checked here is the part the host
//! adds: the line cap the guest asks `search` for renders exactly as the complete result,
//! the reads a search makes record no observation, so it cannot grant the permission an
//! edit needs, and the host functions keep the native walk and its errors.

use std::path::Path;

use p1_contracts::{CancellationToken, Tool, ToolCall, ToolContext, ToolInput, ToolStatus};
use p1_tool_search::{GrepTool, list_files, search};
use p1_tool_search_logic::exec::{
    CallInput, Capabilities, Entry, EntryKind, FsError, Outcome, SearchQuery, SearchResult, execute,
};
use p1_tool_search_logic::{render_content, render_files};
use p1_workspace::{
    Change, FileKind, MutationPolicy, Observation, ObservedFiles, Workspace, WorkspaceError,
};

/// The host side of the imports search links, over one agent's workspace. It holds no
/// observation registry: search links no `snapshot`, so nothing it does can reach one.
struct Host {
    workspace: Workspace,
    cancel: CancellationToken,
}

fn fs_error(error: WorkspaceError) -> FsError {
    match error {
        WorkspaceError::OutsideWorkspace { .. } => FsError::OutsideWorkspace,
        WorkspaceError::NotFound { .. } => FsError::NotFound,
        WorkspaceError::NotADirectory(_) => FsError::WrongKind,
        WorkspaceError::Io { source, .. } => FsError::Io(source.to_string()),
    }
}

impl Capabilities for Host {
    fn cancelled(&self) -> bool {
        self.cancel.is_cancelled()
    }

    fn stat(&self, path: &str) -> Result<Entry, FsError> {
        let checked = self.workspace.check_path(path).map_err(fs_error)?;
        let stat = self.workspace.stat(path).map_err(fs_error)?;
        Ok(Entry {
            path: checked.display().to_string(),
            kind: match stat.kind {
                FileKind::File => EntryKind::File,
                FileKind::Directory => EntryKind::Directory,
                _ => EntryKind::Other,
            },
        })
    }

    /// `Workspace::read_unobserved`: the host's read for an assembly without `snapshot`.
    fn read(&self, path: &str, offset: u64, length: u64) -> Result<Vec<u8>, FsError> {
        let snapshot = self.workspace.read_unobserved(path).map_err(fs_error)?;
        Ok(snapshot
            .read(
                usize::try_from(offset).unwrap(),
                usize::try_from(length).unwrap(),
            )
            .to_vec())
    }

    fn list_files(&self, path: &str, glob: Option<&str>) -> Result<Vec<String>, FsError> {
        list_files(&self.workspace, path, glob, &self.cancel)
    }

    fn search(&self, query: &SearchQuery) -> Result<SearchResult, FsError> {
        search(&self.workspace, query, &self.cancel)
    }
}

fn workspace(root: &Path) -> Workspace {
    Workspace::new(root).unwrap()
}

fn guest(root: &Path, raw: &str) -> Outcome {
    let host = Host {
        workspace: workspace(root),
        cancel: CancellationToken::new(),
    };
    execute(&host, "grep", CallInput::Json(raw))
}

fn native(root: &Path, raw: &str) -> Outcome {
    let tool = GrepTool::new(workspace(root));
    let call = ToolCall {
        call_id: "call-1".into(),
        name: "grep".into(),
        input: ToolInput::Json(raw.to_string()),
    };
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();
    let outcome = runtime.block_on(tool.execute(
        &call,
        ToolContext {
            cancel: CancellationToken::new(),
        },
    ));
    match outcome.status {
        ToolStatus::Ok => Outcome::Ok(outcome.content),
        ToolStatus::Error => Outcome::Error(outcome.content),
        ToolStatus::Cancelled => Outcome::Cancelled,
        other => panic!("the native grep tool returned {other:?}"),
    }
}

fn query(pattern: &str, context: u32, max_lines: u32) -> SearchQuery {
    SearchQuery {
        pattern: pattern.to_string(),
        path: None,
        glob: None,
        case_insensitive: false,
        context,
        max_lines,
    }
}

/// What the tool printed before the line cap existed: the whole walk searched, every line
/// carried, then rendered.
fn uncapped_content(root: &Path, pattern: &str, context: u32) -> String {
    let result = search(
        &workspace(root),
        &query(pattern, context, u32::MAX),
        &CancellationToken::new(),
    )
    .unwrap();
    assert!(!result.truncated);
    render_content(&result)
}

fn uncapped_files(root: &Path, pattern: &str) -> String {
    let result = search(
        &workspace(root),
        &query(pattern, 0, u32::MAX),
        &CancellationToken::new(),
    )
    .unwrap();
    let paths: Vec<String> = result.files.into_iter().map(|file| file.path).collect();
    render_files(&paths, paths.len())
}

fn write(root: &Path, path: &str, contents: impl AsRef<[u8]>) {
    let path = root.join(path);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, contents).unwrap();
}

/// Files whose hits cross the line cap part of the way through the walk: many files of a
/// few hits, one of many, and more after it, with context lines between the hits.
fn capped_tree(root: &Path) {
    for index in 0..400 {
        write(root, &format!("a/f{index:03}.txt"), "beta\nx\nbeta\n");
    }
    write(root, "b/big.txt", "beta\nx\n".repeat(1_500));
    for index in 0..50 {
        write(root, &format!("c/g{index:02}.txt"), "beta\n");
    }
    write(root, "c/zz.bin", b"beta\0\n");
}

#[test]
fn a_capped_content_search_prints_what_the_uncapped_one_did() {
    let dir = tempfile::tempdir().unwrap();
    capped_tree(dir.path());

    for (raw, pattern, context) in [
        (r#"{"pattern":"beta"}"#, "beta", 0),
        (r#"{"pattern":"beta","context":1}"#, "beta", 1),
    ] {
        let expected = uncapped_content(dir.path(), pattern, context);
        assert!(expected.contains("more matching files not shown"));
        assert_eq!(guest(dir.path(), raw), Outcome::Ok(expected.clone()));
        assert_eq!(native(dir.path(), raw), Outcome::Ok(expected));
    }

    // A first file over the cap is cut inside it, with the later files counted.
    let only_big = tempfile::tempdir().unwrap();
    write(only_big.path(), "big.txt", "beta\n".repeat(3_000));
    write(only_big.path(), "more.txt", "beta\n");
    let expected = uncapped_content(only_big.path(), "beta", 0);
    assert!(expected.ends_with(
        "[truncated inside big.txt after line 1998; 1 more matching files not shown; narrow with path, glob or a stricter pattern]"
    ));
    assert_eq!(
        guest(only_big.path(), r#"{"pattern":"beta"}"#),
        Outcome::Ok(expected)
    );
}

#[test]
fn a_capped_files_search_names_every_file_it_would_have() {
    let dir = tempfile::tempdir().unwrap();
    // Thirty files of a hundred hits: the capped search carries twenty of them.
    for index in 0..30 {
        write(
            dir.path(),
            &format!("f{index:02}.txt"),
            "beta\n".repeat(100),
        );
    }
    write(dir.path(), "zz.bin", b"beta\0\n");
    let raw = r#"{"pattern":"beta","mode":"files"}"#;
    let expected = uncapped_files(dir.path(), "beta");
    assert_eq!(expected.lines().count(), 30);
    assert_eq!(guest(dir.path(), raw), Outcome::Ok(expected.clone()));
    assert_eq!(native(dir.path(), raw), Outcome::Ok(expected));

    let many = tempfile::tempdir().unwrap();
    capped_tree(many.path());
    assert_eq!(
        guest(many.path(), raw),
        Outcome::Ok(uncapped_files(many.path(), "beta"))
    );
}

#[test]
fn search_reads_record_no_observation_and_grant_no_edit() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    write(&root, "d.txt", "beta\n");
    write(&root, "e.md", "anything\n");
    let workspace = workspace(&root);
    let observed = ObservedFiles::new();
    let host = Host {
        workspace: workspace.clone(),
        cancel: CancellationToken::new(),
    };

    // Content search, and the listing that reads every file's first bytes.
    for raw in [r#"{"pattern":"beta"}"#, r#"{"pattern":"","mode":"files"}"#] {
        assert!(matches!(
            execute(&host, "grep", CallInput::Json(raw)),
            Outcome::Ok(_)
        ));
    }
    for path in ["d.txt", "e.md"] {
        let contents = std::fs::read(root.join(path)).unwrap();
        assert_eq!(
            observed.check_unchanged(&root.join(path), &contents),
            Observation::NeverObserved
        );
    }
    // An observed mutation after the search is refused as never read.
    assert_eq!(
        workspace
            .commit(
                &[Change::write("d.txt", "changed\n")],
                &observed,
                MutationPolicy::Observed
            )
            .unwrap_err()
            .to_string(),
        "You must read d.txt before changing it."
    );
    assert_eq!(std::fs::read(root.join("d.txt")).unwrap(), b"beta\n");
}

#[test]
fn scope_and_pattern_errors_match_the_native_tool() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    write(&root, "sub/a.rs", "beta\n");
    let absolute_missing = format!(
        r#"{{"pattern":"beta","path":"{}/sub/nope"}}"#,
        root.display()
    );
    let cases = [
        (
            r#"{"pattern":"beta","path":"../"}"#.to_string(),
            "path escapes workspace: ../",
        ),
        (
            r#"{"pattern":"beta","path":"./sub/../nope"}"#.to_string(),
            "nope does not exist.",
        ),
        (absolute_missing, "sub/nope does not exist."),
        (
            r#"{"pattern":"beta","glob":"a{","mode":"files"}"#.to_string(),
            "invalid glob pattern",
        ),
        // The regex is checked before the glob, as the native tool always did.
        (
            r#"{"pattern":"(","glob":"a{"}"#.to_string(),
            "invalid regex pattern",
        ),
        (
            r#"{"pattern":"","mode":"files","path":"nope"}"#.to_string(),
            "nope does not exist.",
        ),
    ];
    for (raw, expected) in cases {
        let from_guest = guest(&root, &raw);
        assert_eq!(from_guest, native(&root, &raw), "input {raw}");
        match from_guest {
            Outcome::Error(message) => assert!(message.starts_with(expected), "{message}"),
            other => panic!("input {raw}: {other:?}"),
        }
    }
}

#[test]
fn the_host_walk_is_sorted_confined_and_cancellable() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), ".gitignore", "target/\n");
    write(dir.path(), "src/b.rs", "beta\n");
    write(dir.path(), "src/a.rs", "beta\n");
    write(dir.path(), "target/x.rs", "beta\n");
    write(dir.path(), ".hidden/c.rs", "beta\n");
    let workspace = workspace(dir.path());
    let cancel = CancellationToken::new();

    assert_eq!(
        list_files(&workspace, ".", None, &cancel).unwrap(),
        vec!["src/a.rs", "src/b.rs"]
    );
    assert_eq!(
        list_files(&workspace, "src/b.rs", None, &cancel).unwrap(),
        vec!["src/b.rs"]
    );
    assert_eq!(
        list_files(&workspace, "..", None, &cancel),
        Err(FsError::OutsideWorkspace)
    );
    let result = search(&workspace, &query("beta", 0, 1), &cancel).unwrap();
    assert_eq!(result.files.len(), 1);
    assert_eq!(result.files[0].path, "src/a.rs");
    assert!(result.truncated);
    assert_eq!(result.omitted_files, 1);

    cancel.cancel();
    assert_eq!(
        search(&workspace, &query("beta", 0, 10), &cancel),
        Err(FsError::Cancelled)
    );
    assert_eq!(
        list_files(&workspace, ".", None, &cancel),
        Err(FsError::Cancelled)
    );
}
