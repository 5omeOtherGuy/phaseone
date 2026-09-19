//! Lead acceptance: read / edit / write working together on one agent's shared state,
//! with real-shaped awkward inputs (docs/design/tools.md).

use std::fs;
use std::os::unix::fs::symlink;
use std::path::Path;

use p1_contracts::{
    CancellationToken, Tool, ToolCall, ToolContext, ToolInput, ToolOutcome, ToolStatus,
};
use p1_tool_edit::EditTool;
use p1_tool_read::ReadTool;
use p1_tool_write::WriteTool;
use p1_workspace::{ObservedFiles, Workspace};

struct Tools {
    read: ReadTool,
    edit: EditTool,
    write: WriteTool,
}

fn tools(root: &Path) -> Tools {
    let workspace = Workspace::new(root).unwrap();
    let observed = ObservedFiles::new();
    Tools {
        read: ReadTool::new(workspace.clone(), observed.clone()),
        edit: EditTool::new(workspace.clone(), observed.clone()),
        write: WriteTool::new(workspace, observed),
    }
}

async fn call(tool: &dyn Tool, json: &str) -> ToolOutcome {
    let call = ToolCall {
        call_id: "c".into(),
        name: tool.declaration().name.clone(),
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

#[tokio::test]
async fn edit_requires_a_read_then_consecutive_edits_need_no_reread() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("a.rs"), "fn a() {}\nfn b() {}\n").unwrap();
    let t = tools(dir.path());

    let blind = call(
        &t.edit,
        r#"{"file_path":"a.rs","old_string":"fn a","new_string":"fn x"}"#,
    )
    .await;
    assert_eq!(blind.status, ToolStatus::Error);
    assert_eq!(blind.content, "You must read a.rs before changing it.");
    assert_eq!(
        fs::read_to_string(dir.path().join("a.rs")).unwrap(),
        "fn a() {}\nfn b() {}\n"
    );

    assert_eq!(
        call(&t.read, r#"{"path":"a.rs"}"#).await.status,
        ToolStatus::Ok
    );
    let first = call(
        &t.edit,
        r#"{"file_path":"a.rs","old_string":"fn a","new_string":"fn x"}"#,
    )
    .await;
    assert_eq!(
        (first.status, first.content.as_str()),
        (ToolStatus::Ok, "Edited a.rs (1 replacement).")
    );
    let second = call(
        &t.edit,
        r#"{"file_path":"a.rs","old_string":"fn b","new_string":"fn y"}"#,
    )
    .await;
    assert_eq!(second.status, ToolStatus::Ok, "{}", second.content);
    assert_eq!(
        fs::read_to_string(dir.path().join("a.rs")).unwrap(),
        "fn x() {}\nfn y() {}\n"
    );
}

#[tokio::test]
async fn a_file_changed_behind_the_agents_back_must_be_read_again() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("notes.txt");
    fs::write(&path, "one\n").unwrap();
    let t = tools(dir.path());
    call(&t.read, r#"{"path":"notes.txt"}"#).await;
    fs::write(&path, "one\nadded by someone else\n").unwrap();

    let edit = call(
        &t.edit,
        r#"{"file_path":"notes.txt","old_string":"one","new_string":"1"}"#,
    )
    .await;
    assert_eq!(
        edit.content,
        "notes.txt changed on disk since you last read it; read it again."
    );
    let write = call(&t.write, r#"{"file_path":"notes.txt","content":"clobber"}"#).await;
    assert_eq!(
        write.content,
        "notes.txt changed on disk since you last read it; read it again."
    );
    assert_eq!(
        fs::read_to_string(&path).unwrap(),
        "one\nadded by someone else\n"
    );
}

#[tokio::test]
async fn write_creates_nested_files_and_makes_them_editable_but_never_clobbers_unread_ones() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("existing.txt"), "precious").unwrap();
    let t = tools(dir.path());

    let created = call(
        &t.write,
        r#"{"file_path":"deep/er/new.txt","content":"hello\n"}"#,
    )
    .await;
    assert_eq!(
        (created.status, created.content.as_str()),
        (ToolStatus::Ok, "Wrote deep/er/new.txt (6 bytes).")
    );
    let edit = call(
        &t.edit,
        r#"{"file_path":"deep/er/new.txt","old_string":"hello","new_string":"bye"}"#,
    )
    .await;
    assert_eq!(edit.status, ToolStatus::Ok, "{}", edit.content);

    let clobber = call(&t.write, r#"{"file_path":"existing.txt","content":"gone"}"#).await;
    assert_eq!(
        clobber.content,
        "You must read existing.txt before changing it."
    );
    assert_eq!(
        fs::read_to_string(dir.path().join("existing.txt")).unwrap(),
        "precious"
    );
}

#[tokio::test]
async fn no_tool_escapes_the_workspace_by_any_route() {
    let outside = tempfile::tempdir().unwrap();
    fs::write(outside.path().join("secret.txt"), "secret").unwrap();
    let dir = tempfile::tempdir().unwrap();
    symlink(outside.path(), dir.path().join("link")).unwrap();
    symlink(
        outside.path().join("secret.txt"),
        dir.path().join("file_link"),
    )
    .unwrap();
    let t = tools(dir.path());
    let abs = outside.path().join("secret.txt");

    for json in [
        r#"{"path":"../secret.txt"}"#.to_string(),
        r#"{"path":"link/secret.txt"}"#.to_string(),
        r#"{"path":"file_link"}"#.to_string(),
        r#"{"path":"a/../../secret.txt"}"#.to_string(),
        format!(r#"{{"path":"{}"}}"#, abs.display()),
    ] {
        let out = call(&t.read, &json).await;
        assert_eq!(out.status, ToolStatus::Error, "{json} -> {}", out.content);
        assert!(
            !out.content.contains("secret\n") && out.content != "secret",
            "{json} leaked"
        );
    }
    for json in [
        r#"{"file_path":"link/new.txt","content":"x"}"#,
        r#"{"file_path":"../new.txt","content":"x"}"#,
        r#"{"file_path":"file_link","content":"x"}"#,
    ] {
        let out = call(&t.write, json).await;
        assert_eq!(out.status, ToolStatus::Error, "{json} -> {}", out.content);
    }
    assert_eq!(fs::read_to_string(&abs).unwrap(), "secret");
    assert_eq!(
        fs::read_dir(outside.path()).unwrap().count(),
        1,
        "nothing may be created outside"
    );
}

#[tokio::test]
async fn a_read_file_swapped_for_an_outside_symlink_is_refused() {
    let outside = tempfile::tempdir().unwrap();
    fs::write(outside.path().join("target.txt"), "same text\n").unwrap();
    let dir = tempfile::tempdir().unwrap();
    let inside = dir.path().join("f.txt");
    fs::write(&inside, "same text\n").unwrap();
    let t = tools(dir.path());
    call(&t.read, r#"{"path":"f.txt"}"#).await;

    fs::remove_file(&inside).unwrap();
    symlink(outside.path().join("target.txt"), &inside).unwrap();

    let edit = call(
        &t.edit,
        r#"{"file_path":"f.txt","old_string":"same","new_string":"pwned"}"#,
    )
    .await;
    assert_eq!(edit.status, ToolStatus::Error, "{}", edit.content);
    assert_eq!(
        fs::read_to_string(outside.path().join("target.txt")).unwrap(),
        "same text\n"
    );
}

#[tokio::test]
async fn crlf_and_missing_final_newline_survive_an_edit() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("win.txt");
    fs::write(&path, "alpha\r\nbeta\r\ngamma").unwrap();
    let t = tools(dir.path());
    call(&t.read, r#"{"path":"win.txt"}"#).await;
    let out = call(
        &t.edit,
        r#"{"file_path":"win.txt","old_string":"beta","new_string":"BETA"}"#,
    )
    .await;
    assert_eq!(out.status, ToolStatus::Ok, "{}", out.content);
    assert_eq!(fs::read(&path).unwrap(), b"alpha\r\nBETA\r\ngamma");
}

#[tokio::test]
async fn ambiguous_and_missing_matches_change_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("dup.txt");
    fs::write(&path, "x = 1\nx = 1\n").unwrap();
    let t = tools(dir.path());
    call(&t.read, r#"{"path":"dup.txt"}"#).await;

    let twice = call(
        &t.edit,
        r#"{"file_path":"dup.txt","old_string":"x = 1","new_string":"x = 2"}"#,
    )
    .await;
    assert_eq!(
        twice.content,
        "old_string occurs 2 times in dup.txt; add context to make it unique or set replace_all."
    );
    let none = call(
        &t.edit,
        r#"{"file_path":"dup.txt","old_string":"y = 1","new_string":"y = 2"}"#,
    )
    .await;
    assert_eq!(none.content, "old_string was not found in dup.txt.");
    assert_eq!(fs::read_to_string(&path).unwrap(), "x = 1\nx = 1\n");

    let all = call(
        &t.edit,
        r#"{"file_path":"dup.txt","old_string":"x = 1","new_string":"x = 2","replace_all":true}"#,
    )
    .await;
    assert_eq!(all.content, "Edited dup.txt (2 replacements).");
}

#[tokio::test]
async fn hostile_inputs_are_errors_never_panics() {
    let dir = tempfile::tempdir().unwrap();
    let t = tools(dir.path());
    let all: [&dyn Tool; 3] = [&t.read, &t.edit, &t.write];
    for tool in all {
        for raw in [
            "",
            "null",
            "[]",
            "{",
            r#"{"path":5}"#,
            r#"{"path":"a","extra":1}"#,
            "\u{0}\u{1}",
            r#"{"file_path":"\u0000"}"#,
        ] {
            let out = call(tool, raw).await;
            assert_eq!(
                out.status,
                ToolStatus::Error,
                "{} accepted {raw:?}",
                tool.declaration().name
            );
        }
        let text = ToolCall {
            call_id: "c".into(),
            name: "x".into(),
            input: ToolInput::Text("path".into()),
        };
        let out = tool
            .execute(
                &text,
                ToolContext {
                    cancel: CancellationToken::new(),
                },
            )
            .await;
        assert!(
            out.content.starts_with("Invalid input for "),
            "{}",
            out.content
        );
    }
}

#[tokio::test]
async fn a_cancelled_call_touches_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let t = tools(dir.path());
    let cancel = CancellationToken::new();
    cancel.cancel();
    let call = ToolCall {
        call_id: "c".into(),
        name: "write".into(),
        input: ToolInput::Json(r#"{"file_path":"never.txt","content":"x"}"#.into()),
    };
    let out = t.write.execute(&call, ToolContext { cancel }).await;
    assert_eq!(out.status, ToolStatus::Cancelled);
    assert!(!dir.path().join("never.txt").exists());
}

#[tokio::test]
async fn large_files_are_windowed_and_bounded() {
    let dir = tempfile::tempdir().unwrap();
    let body: String = (1..=5000).map(|n| format!("line {n}\n")).collect();
    fs::write(dir.path().join("big.txt"), &body).unwrap();
    let t = tools(dir.path());

    let first = call(&t.read, r#"{"path":"big.txt"}"#).await;
    assert!(
        first.content.starts_with("     1\tline 1\n"),
        "{:?}",
        &first.content[..40]
    );
    assert!(
        first
            .content
            .contains("[3000 more lines; continue with offset=2001]"),
        "missing trailer"
    );
    let window = call(&t.read, r#"{"path":"big.txt","offset":4999,"limit":10}"#).await;
    assert_eq!(
        window.content.trim_end(),
        "  4999\tline 4999\n  5000\tline 5000"
    );
}

#[tokio::test]
async fn a_byte_bounded_window_still_tells_the_model_where_to_continue() {
    let dir = tempfile::tempdir().unwrap();
    let wide = "w".repeat(200);
    let body: String = (1..=1000).map(|_| format!("{wide}\n")).collect();
    fs::write(dir.path().join("wide.txt"), &body).unwrap();
    let t = tools(dir.path());

    let out = call(&t.read, r#"{"path":"wide.txt"}"#).await;
    assert!(out.content.len() < 51_000, "{} bytes", out.content.len());
    let trailer = out.content.lines().last().unwrap().to_string();
    let shown = out.content.lines().count() - 1;
    assert_eq!(
        trailer,
        format!(
            "[{} more lines; continue with offset={}]",
            1000 - shown,
            shown + 1
        )
    );
    let next = call(
        &t.read,
        &format!(r#"{{"path":"wide.txt","offset":{}}}"#, shown + 1),
    )
    .await;
    assert!(
        next.content.starts_with(&format!("{:>6}\t", shown + 1)),
        "{:?}",
        &next.content[..20]
    );
}
