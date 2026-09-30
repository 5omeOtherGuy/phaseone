//! Issue #491: a skimmed read is an exploration read. It hides comments, docstrings and
//! blank lines, and it never satisfies read-before-mutate: a change after a skim is
//! refused exactly as after no read at all, and a full read still clears it.
//!
//! The change goes through `Workspace::commit` under `MutationPolicy::Observed`, the
//! read-before-mutate check edit and write run; `p1-tool-edit` itself is not a
//! dependency of this crate.

use p1_contracts::{CancellationToken, Tool, ToolCall, ToolContext, ToolInput, ToolStatus};
use p1_tool_read::ReadTool;
use p1_workspace::{Change, MutationPolicy, ObservedFiles, Workspace};

/// Long enough that hiding it saves more than the skim's own note costs.
const SOURCE: &str =
    "// a comment long enough that hiding it saves more bytes than the note costs\nfn f() {}\n";

fn setup(root: &std::path::Path) -> (ReadTool, Workspace, ObservedFiles) {
    let workspace = Workspace::new(root).unwrap();
    let observed = ObservedFiles::new();
    let read = ReadTool::new(workspace.clone(), observed.clone());
    (read, workspace, observed)
}

async fn read(tool: &ReadTool, json: &str) -> p1_contracts::ToolOutcome {
    let call = ToolCall {
        call_id: "c".into(),
        name: "read".into(),
        input: ToolInput::Json(json.into()),
    };
    tool.execute(
        &call,
        ToolContext {
            cancel: CancellationToken::new(),
        },
    )
    .await
}

fn change(workspace: &Workspace, observed: &ObservedFiles, path: &str) -> Result<(), String> {
    workspace
        .commit(
            &[Change::write(path, "fn g() {}\n")],
            observed,
            MutationPolicy::Observed,
        )
        .map_err(|error| error.to_string())
}

#[tokio::test]
async fn a_change_after_a_skimmed_read_is_refused_like_an_unread_file() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.rs"), SOURCE).unwrap();
    let (tool, workspace, observed) = setup(dir.path());

    let skim = read(&tool, r#"{"file_path":"a.rs","skim":true}"#).await;
    assert_eq!(skim.status, ToolStatus::Ok);
    assert_eq!(
        skim.content,
        "     2\tfn f() {}\n[skim: 1 lines hidden; read the file in full before editing it]"
    );

    assert_eq!(
        change(&workspace, &observed, "a.rs"),
        Err("You must read a.rs before changing it.".into())
    );
    assert_eq!(
        std::fs::read_to_string(dir.path().join("a.rs")).unwrap(),
        SOURCE
    );
}

#[tokio::test]
async fn a_skim_that_fell_back_to_the_full_rendering_still_refuses_the_change() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("b.rs"), "fn a() {}\n").unwrap();
    let (tool, workspace, observed) = setup(dir.path());

    let skim = read(&tool, r#"{"file_path":"b.rs","skim":true}"#).await;
    assert_eq!(
        skim.content,
        "     1\tfn a() {}\n[skim: the skim was not smaller; showing the full read]"
    );

    assert_eq!(
        change(&workspace, &observed, "b.rs"),
        Err("You must read b.rs before changing it.".into())
    );
}

#[tokio::test]
async fn a_skim_followed_by_a_full_read_lets_the_change_through() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("c.rs"), SOURCE).unwrap();
    let (tool, workspace, observed) = setup(dir.path());

    read(&tool, r#"{"file_path":"c.rs","skim":true}"#).await;
    assert_eq!(
        read(&tool, r#"{"file_path":"c.rs"}"#).await.status,
        ToolStatus::Ok
    );

    assert_eq!(change(&workspace, &observed, "c.rs"), Ok(()));
    assert_eq!(
        std::fs::read_to_string(dir.path().join("c.rs")).unwrap(),
        "fn g() {}\n"
    );
}
