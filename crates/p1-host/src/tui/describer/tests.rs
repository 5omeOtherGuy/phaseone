use super::*;
use p1_contracts::{ToolInput, ToolStatus};
use p1_workspace::{ObservedFiles, Workspace};

fn call(name: &str, input: ToolInput) -> ToolCall {
    ToolCall {
        call_id: "c1".into(),
        name: name.into(),
        input,
    }
}

fn result(call: &ToolCall, status: ToolStatus, content: &str) -> ToolResultItem {
    ToolResultItem {
        call_id: call.call_id.clone(),
        name: call.name.clone(),
        status,
        content: content.into(),
    }
}

#[test]
fn edit_hunk_comes_from_the_tool_result_description() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("f.rs"), "one\ntwo\nTHREE\nfour\n").unwrap();
    let tool: Arc<dyn Tool> = Arc::new(p1_tool_edit::EditTool::new(
        Workspace::new(dir.path()).unwrap(),
        ObservedFiles::new(),
    ));
    let describer = HostDescriber::new(dir.path().into(), "off".into(), Arc::new(vec![tool]));
    let call = call(
        "edit",
        ToolInput::Json(r#"{"file_path":"f.rs","old_string":"old","new_string":"THREE"}"#.into()),
    );
    let face = describer.result(
        &call,
        &result(&call, ToolStatus::Ok, "Edited f.rs (1 replacement)."),
        None,
    );
    assert_eq!(face.outcome.as_deref(), Some("+1 −1"));
    assert_eq!(
        face.body,
        FaceBody::Diff(vec![
            DiffRow::Context {
                line: 2,
                text: "two".into()
            },
            DiffRow::Del {
                line: 3,
                text: "old".into()
            },
            DiffRow::Add {
                line: 3,
                text: "THREE".into()
            },
            DiffRow::Context {
                line: 4,
                text: "four".into()
            },
        ])
    );
}

#[test]
fn patch_files_and_shell_command_facts_are_rendered_generically() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = Workspace::new(dir.path()).unwrap();
    let tools: Arc<Vec<Arc<dyn Tool>>> = Arc::new(vec![
        Arc::new(p1_tool_patch::PatchTool::new(
            workspace.clone(),
            ObservedFiles::new(),
        )),
        Arc::new(p1_tool_shell::ShellTool::new(workspace)),
    ]);
    let d = HostDescriber::new(dir.path().into(), "bubblewrap".into(), tools);
    let patch = call("apply_patch", ToolInput::Text("*** Begin Patch\n*** Add File: new.txt\n+one\n+two\n*** Delete File: old.txt\n*** End Patch\n".into()));
    let face = d.result(
        &patch,
        &result(&patch, ToolStatus::Ok, "A new.txt\nD old.txt"),
        None,
    );
    assert_eq!(face.outcome.as_deref(), Some("+2 · 2 files"));
    assert_eq!(
        face.body,
        FaceBody::Files(vec![
            ("new.txt".into(), "+2 −0".into()),
            ("old.txt".into(), "D".into())
        ])
    );
    let shell = call("shell", ToolInput::Json(r#"{"command":"echo hi"}"#.into()));
    let face = d.result(
        &shell,
        &result(&shell, ToolStatus::Ok, "hi\n[exit code: 0]"),
        Some(412),
    );
    assert_eq!(face.outcome.as_deref(), Some("412ms · exit 0 · 1 lines"));
    assert_eq!(
        face.meta.as_deref(),
        Some(format!("cwd {} · bubblewrap", dir.path().display()).as_str())
    );
}

#[test]
fn unassembled_tool_uses_the_generic_fallback() {
    let d = HostDescriber::new(
        PathBuf::from("/workspace"),
        "off".into(),
        Arc::new(Vec::new()),
    );
    let call = call("future_tool", ToolInput::Json(r#"{"x":"y"}"#.into()));
    assert_eq!(d.call(&call), GenericDescriber.call(&call));
    let result = result(&call, ToolStatus::Ok, "hello");
    assert_eq!(
        d.result(&call, &result, None),
        GenericDescriber.result(&call, &result, None)
    );
}
