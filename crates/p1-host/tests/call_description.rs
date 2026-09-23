//! ADR-0057: a tool describes each call's own target, and the host tracks the
//! files a session changes from that description — never by matching a tool name
//! or decoding another tool's argument keys.
//!
//! The WORKSPACE tracking is [`p1_host::tui::tracked_target`]: `effect()` says a
//! call changes files, `describe()` says which one. This test drives it with the
//! tool set a session would assemble: a RENAMED edit face and the freeform
//! `apply_patch`, whose patch text has no `file_path` key at all — the exact case
//! that never matched before this change.

use std::sync::Arc;

use p1_contracts::{Tool, ToolCall, ToolInput};
use p1_tool_edit::{EditTool, ToolFace};
use p1_tool_patch::PatchTool;
use p1_workspace::{ObservedFiles, Workspace};

fn json_call(name: &str, raw: &str) -> ToolCall {
    ToolCall {
        call_id: "c1".into(),
        name: name.into(),
        input: ToolInput::Json(raw.into()),
    }
}

fn text_call(name: &str, text: &str) -> ToolCall {
    ToolCall {
        call_id: "c2".into(),
        name: name.into(),
        input: ToolInput::Text(text.into()),
    }
}

#[test]
fn a_renamed_edit_face_and_an_apply_patch_call_track_their_own_files() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = Workspace::new(dir.path()).unwrap();
    let observed = ObservedFiles::new();
    // The edit tool under a face that changes its model-facing name: `describe`
    // reads the input, so the rename changes nothing about the target.
    let edit = Arc::new(
        EditTool::new(workspace.clone(), observed.clone())
            .with_face(ToolFace::new("EditFile", "renamed"), "gpt"),
    ) as Arc<dyn Tool>;
    let patch = Arc::new(PatchTool::new(workspace, observed)) as Arc<dyn Tool>;
    let tools = vec![edit, patch];

    let renamed = json_call(
        "EditFile",
        r#"{"file_path":"src/a.rs","old_string":"a","new_string":"b"}"#,
    );
    assert_eq!(tools[0].describe(&renamed).verb, "edit");
    assert_eq!(
        p1_host::tui::tracked_target(&tools, &renamed).as_deref(),
        Some("src/a.rs"),
        "a renamed edit face is tracked by what its own input says"
    );

    // The freeform patch: no JSON, no `file_path` key, only the V4A text.
    let patch_text = "*** Begin Patch\n*** Update File: src/b.rs\n@@\n-a\n+b\n*** End Patch\n";
    let patched = text_call("apply_patch", patch_text);
    assert_eq!(
        p1_host::tui::tracked_target(&tools, &patched).as_deref(),
        Some("src/b.rs"),
        "the freeform patch's first file comes from the tool's own parser"
    );
}
