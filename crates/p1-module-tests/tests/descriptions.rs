//! Module-owned tool descriptions (slice U-desc, issue #7).
//!
//! The four filesystem tools describe their own calls and results (ADR-0057, ADR-0059): the
//! model-facing name is a lookup key for the host, never a classifier of a call's input or a
//! result's output. A component answers `describe`, `describe-result` and `effect` on the
//! restricted path — from the call text alone — by handing it to its logic crate, the same
//! code the exports call (`modules/p1-module-edit/`, `-write/`, `-patch/`, `-search/`).
//!
//! Components that import the workspace capabilities cannot be loaded before
//! `wasm-loader-v1` (BLOCKERS S2-B3), so these cases run the logic crate functions those
//! exports call and compare them, call for call, with the native tools'
//! `describe`/`describe_result`/`effect` and with the literal values
//! `docs/design/modules/protocol.md` fixes. They cover the call descriptions (the closed verb
//! vocabulary, the target and the exact diff previews), the result descriptions, the
//! destructive flag, a renamed declaration, an unknown tool, and invalid input classified at
//! the tool's worst case.
//!
//! A component decides an escaping path lexically, because its `describe` runs with no
//! capability; the native tool resolves the real path. The two agree for a relative path and
//! for a path that escapes by its spelling, and diverge on an absolute path inside the root
//! and on a relative path through an escaping symlink. Those two divergences are asserted here
//! as the component's accepted restricted-path contract; the production loader checks them in
//! U-edit.2/U-write.2/U-patch.2/U-search.2.

use std::fs;
use std::path::{Path, PathBuf};

use p1_contracts::Tool;
use p1_contracts::serde_json::{self, Value, json};
use p1_contracts::tool::{ResultDescription, ResultDetail, ToolFace};
use p1_contracts::{
    BoxFuture, CallDescription, DeclarationKind, Effect, ToolCall, ToolContext, ToolDeclaration,
    ToolIdentity, ToolInput, ToolOutcome, ToolResultItem, ToolStatus,
};
use p1_tool_edit::EditTool;
use p1_tool_patch::PatchTool;
use p1_tool_search::GrepTool;
use p1_tool_write::WriteTool;
use p1_workspace::{ObservedFiles, Workspace};

// ------------------------------------------------------------- the component's exports
//
// Each function below calls the same logic-crate helper the matching export in
// `modules/p1-module-*/src/lib.rs` (`describe` and `describe-result`) calls, so the export and
// this suite answer from one body and cannot drift; the exports themselves cannot be
// instantiated on this side of `wasm-loader-v1`.

/// `modules/p1-module-edit/src/lib.rs::describe`.
fn edit_describe(call: &str) -> String {
    p1_tool_edit_logic::wire::describe_call(call)
}

/// `modules/p1-module-edit/src/lib.rs::describe_result`.
fn edit_describe_result(call: &str, result: &str) -> String {
    p1_tool_edit_logic::wire::describe_result_call(call, result)
}

/// `modules/p1-module-search/src/lib.rs::describe` (the `grep` component).
fn search_describe(call: &str) -> String {
    p1_tool_search_logic::wire::describe_call(call)
}

/// `modules/p1-module-search/src/lib.rs::describe_result`.
fn search_describe_result(call: &str, result: &str) -> String {
    p1_tool_search_logic::wire::describe_result_call(call, result)
}

/// `modules/p1-module-write/src/lib.rs::describe`.
fn write_describe(call: &str) -> String {
    p1_tool_write_logic::wire::describe_call(call)
}

/// `modules/p1-module-write/src/lib.rs::describe_result`.
fn write_describe_result(call: &str, result: &str) -> String {
    p1_tool_write_logic::wire::describe_result_call(call, result)
}

/// `modules/p1-module-patch/src/lib.rs::describe`.
fn patch_describe(call: &str) -> String {
    p1_tool_patch_logic::wire::describe_call(call)
}

/// `modules/p1-module-patch/src/lib.rs::describe_result`.
fn patch_describe_result(call: &str, result: &str) -> String {
    p1_tool_patch_logic::wire::describe_result_call(call, result)
}

// ------------------------------------------------------------- wire and native fixtures

/// The wire tool-call text (`p1:protocol/tool-call/1`) the host sends a component.
fn call_json(name: &str, kind: &str, raw: &str) -> String {
    json!({
        "call_id": "c1",
        "name": name,
        "input": {"kind": kind, "raw": raw},
    })
    .to_string()
}

/// A function-tool call: JSON input.
fn json_call(name: &str, raw: &str) -> String {
    call_json(name, "json", raw)
}

/// A freeform call: text input (the `apply_patch` component's default form).
fn text_call(name: &str, raw: &str) -> String {
    call_json(name, "text", raw)
}

/// The wire `tool_result` history item the host sends `describe-result`.
fn result_item(name: &str, status: &str, content: &str) -> String {
    json!({
        "item": "tool_result",
        "call_id": "c1",
        "name": name,
        "status": status,
        "content": content,
    })
    .to_string()
}

/// A native `Tool` call.
fn native_call(name: &str, input: ToolInput) -> ToolCall {
    ToolCall {
        call_id: "c1".into(),
        name: name.into(),
        input,
    }
}

/// JSON input for a native tool call.
fn json_input(raw: &str) -> ToolInput {
    ToolInput::Json(raw.to_string())
}

/// The `tool_result` item the native tool's `describe_result` reads.
fn native_result(call: &ToolCall, status: ToolStatus, content: &str) -> ToolResultItem {
    ToolResultItem {
        call_id: call.call_id.clone(),
        name: call.name.clone(),
        status,
        content: content.into(),
    }
}

/// A workspace over a fresh temporary directory, with its observation store. The directory
/// must outlive the workspace, so the caller keeps the handle.
fn workspace() -> (tempfile::TempDir, Workspace, ObservedFiles) {
    let dir = tempfile::tempdir().expect("temp dir");
    let workspace = Workspace::new(dir.path()).expect("workspace");
    (dir, workspace, ObservedFiles::new())
}

/// The component's `describe` JSON is the literal `expected`, and every field the native tool
/// answered is the same: the two sides cannot drift.
fn check_call(component_json: &str, native: &CallDescription, expected: Value) {
    let described: Value = serde_json::from_str(component_json).expect("the component's JSON");
    assert_eq!(described, expected, "the component's call description");
    assert_eq!(json!(native.verb), expected["verb"], "verb vs native");
    assert_eq!(
        native.target.as_deref(),
        expected.get("target").and_then(Value::as_str),
        "target vs native"
    );
    assert_eq!(
        json!(native.destructive),
        expected["destructive"],
        "destructive vs native"
    );
    match (native.edit.as_ref(), expected.get("edit")) {
        (None, None) => {}
        (Some(edit), Some(value)) => assert_eq!(
            value,
            &json!({"path": edit.path.as_str(), "old": edit.old.as_str(), "new": edit.new.as_str()}),
            "the diff preview vs native"
        ),
        (native, expected) => {
            panic!("a diff preview on one side only: {native:?} vs {expected:?}")
        }
    }
}

/// The component's `describe-result` JSON is the literal `expected`, and the native tool's
/// `ResultDescription` is the same value in the wire shape.
fn check_result(component_json: &str, native: &ResultDescription, expected: Value) {
    let described: Value = serde_json::from_str(component_json).expect("the component's JSON");
    assert_eq!(described, expected, "the component's result description");
    assert_eq!(
        json!(native.summary.as_str()),
        expected["summary"],
        "summary vs native"
    );
    match (native.detail.as_ref(), expected.get("detail")) {
        (None, None) => {}
        (
            Some(ResultDetail::Diff {
                path,
                before,
                after,
            }),
            Some(value),
        ) => assert_eq!(
            value,
            &json!({"kind": "diff", "path": path, "before": before, "after": after}),
            "diff vs native"
        ),
        (Some(ResultDetail::Files { paths }), Some(value)) => assert_eq!(
            value,
            &json!({"kind": "files", "paths": paths}),
            "files vs native"
        ),
        (Some(ResultDetail::Matches { count, files }), Some(value)) => assert_eq!(
            value,
            &json!({"kind": "matches", "count": count, "files": files}),
            "matches vs native"
        ),
        (native, expected) => panic!("a detail on one side only: {native:?} vs {expected:?}"),
    }
}

/// The `destructive` flag of one call on both sides: the component's `describe` JSON and the
/// native tool's `describe`.
fn destructive_both(component_json: &str, native: &CallDescription) -> (bool, bool) {
    let described: Value = serde_json::from_str(component_json).expect("the component's JSON");
    (
        described["destructive"]
            .as_bool()
            .expect("the component always states destructive"),
        native.destructive,
    )
}

/// Invalid input on both sides is the same best-effort shape: the component's `describe` JSON
/// and the native tool's `describe` name the `verb`, and neither a target, a diff preview nor
/// the destructive flag.
fn check_no_target(component_json: &str, native: &CallDescription, verb: &str) {
    let described: Value = serde_json::from_str(component_json).expect("the component's JSON");
    assert_eq!(described["verb"], verb, "the component: {described}");
    assert!(
        described.get("target").is_none(),
        "the component names a target: {described}"
    );
    assert_eq!(
        described["destructive"], false,
        "the component: {described}"
    );
    assert!(
        described.get("edit").is_none(),
        "the component previews an edit: {described}"
    );

    assert_eq!(native.verb, verb, "the native tool: {native:?}");
    assert!(native.target.is_none(), "the native tool: {native:?}");
    assert!(!native.destructive, "the native tool: {native:?}");
    assert!(native.edit.is_none(), "the native tool: {native:?}");
}

// ------------------------------------------------------------- call descriptions

/// The verb, the target and the diff preview of a call, from the component's own code and
/// from the native tool, against the literal the protocol fixes.
#[test]
fn call_descriptions_match_the_native_tools() {
    let (_dir, workspace, observed) = workspace();

    // `edit`: the file it edits and its exact preview.
    let raw = r#"{"file_path":"src/a.rs","old_string":"a","new_string":"b"}"#;
    let edit = EditTool::new(workspace.clone(), observed.clone());
    let call = native_call("edit", json_input(raw));
    check_call(
        &edit_describe(&json_call("edit", raw)),
        &edit.describe(&call),
        json!({
            "verb": "edit",
            "target": "src/a.rs",
            "edit": {"path": "src/a.rs", "old": "a", "new": "b"},
            "destructive": false,
        }),
    );

    // `write`: a whole-file replacement, so the preview's old side is empty.
    let raw = r#"{"file_path":"out.txt","content":"hi\n"}"#;
    let write = WriteTool::new(workspace.clone(), observed.clone());
    let call = native_call("write", json_input(raw));
    check_call(
        &write_describe(&json_call("write", raw)),
        &write.describe(&call),
        json!({
            "verb": "edit",
            "target": "out.txt",
            "edit": {"path": "out.txt", "old": "", "new": "hi\n"},
            "destructive": false,
        }),
    );

    // `apply_patch`: a freeform V4A patch names the first file it touches and carries no
    // preview (a patch can touch many files).
    let patch = "*** Begin Patch\n*** Update File: src/a.rs\n@@\n-a\n+b\n*** End Patch\n";
    let tool = PatchTool::new(workspace.clone(), observed.clone());
    let call = native_call("apply_patch", ToolInput::Text(patch.to_string()));
    check_call(
        &patch_describe(&text_call("apply_patch", patch)),
        &tool.describe(&call),
        json!({"verb": "edit", "target": "src/a.rs", "destructive": false}),
    );

    // … and its function-tool form (a route without freeform tools): `{"patch": …}`.
    let function = PatchTool::new(workspace.clone(), observed.clone()).function_face();
    let raw = json!({"patch": patch}).to_string();
    let call = native_call("apply_patch", json_input(&raw));
    check_call(
        &patch_describe(&json_call("apply_patch", &raw)),
        &function.describe(&call),
        json!({"verb": "edit", "target": "src/a.rs", "destructive": false}),
    );

    // A multi-file patch names the count instead of one path.
    let many =
        "*** Begin Patch\n*** Add File: a.txt\n+one\n*** Delete File: b.txt\n*** End Patch\n";
    let call = native_call("apply_patch", ToolInput::Text(many.to_string()));
    check_call(
        &patch_describe(&text_call("apply_patch", many)),
        &tool.describe(&call),
        json!({"verb": "edit", "target": "2 files", "destructive": false}),
    );

    // `grep`: the pattern and the scope searched; a search changes nothing.
    let raw = r#"{"pattern":"beta","path":"src"}"#;
    let search = GrepTool::new(workspace.clone());
    let call = native_call("grep", json_input(raw));
    check_call(
        &search_describe(&json_call("grep", raw)),
        &search.describe(&call),
        json!({"verb": "search", "target": "beta src", "destructive": false}),
    );
    let default_scope = r#"{"pattern":"beta"}"#;
    let call = native_call("grep", json_input(default_scope));
    check_call(
        &search_describe(&json_call("grep", default_scope)),
        &search.describe(&call),
        json!({"verb": "search", "target": "beta .", "destructive": false}),
    );
}

/// Every verb the components emit comes from the closed vocabulary of
/// `docs/design/modules/protocol.md` (freeze item 8); a module cannot mint UI vocabulary.
#[test]
fn call_verbs_come_from_the_closed_vocabulary() {
    const CLOSED: [&str; 8] = [
        "read", "edit", "run", "search", "finish", "worker", "workflow", "call",
    ];
    let verbs = [
        p1_tool_edit_logic::VERB,
        p1_tool_write_logic::VERB,
        p1_tool_patch_logic::VERB,
        p1_tool_search_logic::VERB,
    ];
    for verb in verbs {
        assert!(
            CLOSED.contains(&verb),
            "`{verb}` is outside the closed vocabulary"
        );
    }
    assert_eq!(p1_tool_edit_logic::VERB, "edit");
    assert_eq!(p1_tool_write_logic::VERB, "edit");
    assert_eq!(p1_tool_patch_logic::VERB, "edit");
    assert_eq!(p1_tool_search_logic::VERB, "search");

    // Each component really states it, verbatim, in its JSON.
    let verb_of = |json: &str| {
        serde_json::from_str::<Value>(json).expect("JSON")["verb"]
            .as_str()
            .expect("a verb")
            .to_string()
    };
    assert_eq!(verb_of(&edit_describe(&json_call("edit", "{}"))), "edit");
    assert_eq!(verb_of(&write_describe(&json_call("write", "{}"))), "edit");
    assert_eq!(
        verb_of(&patch_describe(&text_call("apply_patch", "x"))),
        "edit"
    );
    assert_eq!(
        verb_of(&search_describe(&json_call("grep", "{}"))),
        "search"
    );
}

/// The diff preview is exact: the tool's own parsed replacement, never a guess from argument
/// keys; a patch and a search carry none.
#[test]
fn diff_previews_are_exact() {
    let (_dir, workspace, observed) = workspace();

    let raw = r#"{"file_path":"a/b.rs","old_string":"one\ntwo\n","new_string":"ONE\nTWO\n"}"#;
    let edit = EditTool::new(workspace.clone(), observed.clone());
    let call = native_call("edit", json_input(raw));
    let described: Value = serde_json::from_str(&edit_describe(&json_call("edit", raw))).unwrap();
    assert_eq!(
        described["edit"],
        json!({"path": "a/b.rs", "old": "one\ntwo\n", "new": "ONE\nTWO\n"})
    );
    assert_eq!(
        described["edit"],
        json!({
            "path": edit.describe(&call).edit.unwrap().path,
            "old": edit.describe(&call).edit.unwrap().old,
            "new": edit.describe(&call).edit.unwrap().new,
        })
    );

    let raw = r#"{"file_path":"out.txt","content":"fresh\n"}"#;
    let described: Value = serde_json::from_str(&write_describe(&json_call("write", raw))).unwrap();
    assert_eq!(
        described["edit"],
        json!({"path": "out.txt", "old": "", "new": "fresh\n"})
    );

    let patch = "*** Begin Patch\n*** Update File: a.rs\n@@\n-a\n+b\n*** End Patch\n";
    let described: Value =
        serde_json::from_str(&patch_describe(&text_call("apply_patch", patch))).unwrap();
    assert!(described.get("edit").is_none(), "{described}");

    let described: Value =
        serde_json::from_str(&search_describe(&json_call("grep", r#"{"pattern":"x"}"#))).unwrap();
    assert!(described.get("edit").is_none(), "{described}");
}

// ------------------------------------------------------------- result descriptions

/// The result description of a real call, ok and failed, from the component's own code and
/// from the native tool, against the literal the protocol fixes.
#[test]
fn result_descriptions_match_the_native_tools() {
    let (_dir, workspace, observed) = workspace();

    // `edit`: a diff of the replaced text, `+new −old`.
    let raw = r#"{"file_path":"d.txt","old_string":"two","new_string":"TWO"}"#;
    let edit = EditTool::new(workspace.clone(), observed.clone());
    let call = native_call("edit", json_input(raw));
    let ok = native_result(&call, ToolStatus::Ok, "Edited d.txt (1 replacement).");
    check_result(
        &edit_describe_result(
            &json_call("edit", raw),
            &result_item("edit", "ok", "Edited d.txt (1 replacement)."),
        ),
        &edit.describe_result(&call, &ok),
        json!({
            "summary": "+1 −1",
            "detail": {"kind": "diff", "path": "d.txt", "before": "two", "after": "TWO"},
        }),
    );
    let failed = native_result(
        &call,
        ToolStatus::Error,
        "You must read d.txt before changing it.\nmore",
    );
    check_result(
        &edit_describe_result(
            &json_call("edit", raw),
            &result_item(
                "edit",
                "error",
                "You must read d.txt before changing it.\nmore",
            ),
        ),
        &edit.describe_result(&call, &failed),
        json!({"summary": "You must read d.txt before changing it."}),
    );

    // `write`: a whole-file diff, summarised by lines and the bytes the tool reported.
    let content = "x".repeat(4000);
    let raw = json!({"file_path": "out.txt", "content": content}).to_string();
    let write = WriteTool::new(workspace.clone(), observed.clone());
    let call = native_call("write", json_input(&raw));
    let ok = native_result(&call, ToolStatus::Ok, "Wrote out.txt (4000 bytes).");
    check_result(
        &write_describe_result(
            &json_call("write", &raw),
            &result_item("write", "ok", "Wrote out.txt (4000 bytes)."),
        ),
        &write.describe_result(&call, &ok),
        json!({
            "summary": "1 lines · 4.0 kB",
            "detail": {"kind": "diff", "path": "out.txt", "before": "", "after": content},
        }),
    );
    let failed = native_result(&call, ToolStatus::Error, "fail\nmore");
    check_result(
        &write_describe_result(
            &json_call("write", &raw),
            &result_item("write", "error", "fail\nmore"),
        ),
        &write.describe_result(&call, &failed),
        json!({"summary": "fail"}),
    );

    // `apply_patch`: one line per file it touched (`<path>\t<facts>`), from its own plan.
    let patch = "*** Begin Patch\n*** Update File: src/a.rs\n@@\n-a\n+b\n*** End Patch\n";
    let tool = PatchTool::new(workspace.clone(), observed.clone());
    let call = native_call("apply_patch", ToolInput::Text(patch.to_string()));
    let ok = native_result(&call, ToolStatus::Ok, "M src/a.rs");
    check_result(
        &patch_describe_result(
            &text_call("apply_patch", patch),
            &result_item("apply_patch", "ok", "M src/a.rs"),
        ),
        &tool.describe_result(&call, &ok),
        json!({
            "summary": "+1 −1 · 1 files",
            "detail": {"kind": "files", "paths": ["src/a.rs\t+1 −1"]},
        }),
    );
    let delete = "*** Begin Patch\n*** Delete File: old.txt\n*** End Patch\n";
    let call = native_call("apply_patch", ToolInput::Text(delete.to_string()));
    let ok = native_result(&call, ToolStatus::Ok, "D old.txt");
    check_result(
        &patch_describe_result(
            &text_call("apply_patch", delete),
            &result_item("apply_patch", "ok", "D old.txt"),
        ),
        &tool.describe_result(&call, &ok),
        json!({
            "summary": "+0 · 1 files",
            "detail": {"kind": "files", "paths": ["old.txt\tD"]},
        }),
    );

    // `grep`: the hit and file counts of the matches it rendered.
    let raw = r#"{"pattern":"beta"}"#;
    let search = GrepTool::new(workspace.clone());
    let call = native_call("grep", json_input(raw));
    let hits = "src/a.rs\n2:fn beta() {}\n\nsrc/b.rs\n1:fn beta() {}";
    let ok = native_result(&call, ToolStatus::Ok, hits);
    check_result(
        &search_describe_result(&json_call("grep", raw), &result_item("grep", "ok", hits)),
        &search.describe_result(&call, &ok),
        json!({
            "summary": "2 hits · 2 files",
            "detail": {"kind": "matches", "count": 2, "files": ["src/a.rs", "src/b.rs"]},
        }),
    );
    let raw = r#"{"pattern":"beta","mode":"files"}"#;
    let call = native_call("grep", json_input(raw));
    let files = "src/a.rs\nsrc/b.rs";
    let ok = native_result(&call, ToolStatus::Ok, files);
    check_result(
        &search_describe_result(&json_call("grep", raw), &result_item("grep", "ok", files)),
        &search.describe_result(&call, &ok),
        json!({
            "summary": "2 files",
            "detail": {"kind": "matches", "count": 2, "files": ["src/a.rs", "src/b.rs"]},
        }),
    );
    let failed = native_result(&call, ToolStatus::Error, "nope does not exist.\nmore");
    check_result(
        &search_describe_result(
            &json_call("grep", raw),
            &result_item("grep", "error", "nope does not exist.\nmore"),
        ),
        &search.describe_result(&call, &failed),
        json!({"summary": "nope does not exist."}),
    );
}

// ------------------------------------------------------------- destructive flags

/// The destructive flag is the tool's own judgement that a call writes outside the workspace
/// (ADR-0059): a write over an existing file, a patch delete and an edit inside the workspace
/// are not destructive; a path that escapes by its spelling is.
#[test]
fn destructive_flags_match_the_native_tools() {
    let (dir, workspace, observed) = workspace();

    let edit = EditTool::new(workspace.clone(), observed.clone());
    let write = WriteTool::new(workspace.clone(), observed.clone());
    let patch = PatchTool::new(workspace.clone(), observed.clone());
    let search = GrepTool::new(workspace.clone());

    // Inside by spelling: not destructive on either side.
    for raw in [
        r#"{"file_path":"src/a.rs","old_string":"a","new_string":"b"}"#,
        r#"{"file_path":"./src/../src/a.rs","old_string":"a","new_string":"b"}"#,
    ] {
        let call = native_call("edit", json_input(raw));
        assert_eq!(
            destructive_both(
                &edit_describe(&json_call("edit", raw)),
                &edit.describe(&call)
            ),
            (false, false),
            "{raw}"
        );
    }

    // `write` over a file that exists: it replaces it in place, not outside the workspace.
    fs::write(dir.path().join("existing.txt"), b"old\n").unwrap();
    let raw = r#"{"file_path":"existing.txt","content":"new\n"}"#;
    let call = native_call("write", json_input(raw));
    assert_eq!(
        destructive_both(
            &write_describe(&json_call("write", raw)),
            &write.describe(&call)
        ),
        (false, false),
        "{raw}"
    );

    // `write` that escapes by its spelling: destructive on both sides.
    let raw = r#"{"file_path":"../out.txt","content":"x"}"#;
    let call = native_call("write", json_input(raw));
    assert_eq!(
        destructive_both(
            &write_describe(&json_call("write", raw)),
            &write.describe(&call)
        ),
        (true, true),
        "{raw}"
    );

    // `apply_patch`: deleting a file inside the root is not destructive; a delete that
    // escapes is.
    for (text, expected) in [
        (
            "*** Begin Patch\n*** Delete File: old.txt\n*** End Patch\n",
            (false, false),
        ),
        (
            "*** Begin Patch\n*** Delete File: ../victim.txt\n*** End Patch\n",
            (true, true),
        ),
    ] {
        let call = native_call("apply_patch", ToolInput::Text(text.to_string()));
        assert_eq!(
            destructive_both(
                &patch_describe(&text_call("apply_patch", text)),
                &patch.describe(&call)
            ),
            expected,
            "{text}"
        );
    }

    // A search is never destructive, scope or not.
    let raw = r#"{"pattern":"beta","path":"../outside"}"#;
    let call = native_call("grep", json_input(raw));
    assert_eq!(
        destructive_both(
            &search_describe(&json_call("grep", raw)),
            &search.describe(&call)
        ),
        (false, false),
        "{raw}"
    );

    // A path that escapes by its spelling is destructive on both sides; an absolute path is
    // not further documented here because the component flags it even inside the root, below.
    for raw in [
        r#"{"file_path":"../a.rs","old_string":"a","new_string":"b"}"#,
        r#"{"file_path":"a/../../a.rs","old_string":"a","new_string":"b"}"#,
        r#"{"file_path":"/definitely/outside/a.rs","old_string":"a","new_string":"b"}"#,
    ] {
        let call = native_call("edit", json_input(raw));
        assert_eq!(
            destructive_both(
                &edit_describe(&json_call("edit", raw)),
                &edit.describe(&call)
            ),
            (true, true),
            "{raw}"
        );
    }

    // The restricted-path divergence, one direction: an absolute path inside the root is
    // provably inside to the native tool, but the component cannot resolve without the root
    // and answers worst-case.
    let inside = dir.path().join("src/a.rs");
    let raw = json!({"file_path": inside.to_str().unwrap(), "old_string": "a", "new_string": "b"})
        .to_string();
    let call = native_call("edit", json_input(&raw));
    assert_eq!(
        destructive_both(
            &edit_describe(&json_call("edit", &raw)),
            &edit.describe(&call)
        ),
        (true, false),
        "{raw}"
    );

    // The other direction: a relative path through a symlink that escapes is not flagged
    // lexically, while the native tool resolves it outside.
    #[cfg(unix)]
    {
        let outside = tempfile::tempdir().unwrap();
        fs::write(outside.path().join("victim.txt"), b"victim\n").unwrap();
        std::os::unix::fs::symlink(outside.path(), dir.path().join("link")).unwrap();
        let raw = r#"{"file_path":"link/victim.txt","old_string":"a","new_string":"b"}"#;
        let call = native_call("edit", json_input(raw));
        assert_eq!(
            destructive_both(
                &edit_describe(&json_call("edit", raw)),
                &edit.describe(&call)
            ),
            (false, true),
            "{raw}"
        );
    }
}

// ------------------------------------------------------------- renamed and unknown tools

/// A declaration presented under another name keeps its description: the answer comes from the
/// call's own input, not from the name the model used.
#[test]
fn a_renamed_declaration_keeps_its_description() {
    let (_dir, workspace, observed) = workspace();

    let raw = r#"{"file_path":"src/a.rs","old_string":"a","new_string":"b"}"#;
    let call = native_call("EditFile", json_input(raw));
    let default = EditTool::new(workspace.clone(), observed.clone()).describe(&call);
    let renamed = EditTool::new(workspace.clone(), observed.clone())
        .with_face(ToolFace::new("EditFile", "renamed"), "gpt")
        .describe(&call);
    assert_eq!(renamed, default);
    assert_eq!(renamed.verb, "edit");
    assert_eq!(renamed.target.as_deref(), Some("src/a.rs"));

    // The component answers from the input alone, so the call's name (a renamed face, or none
    // at all) cannot change its description.
    assert_eq!(
        edit_describe(&json_call("EditFile", raw)),
        edit_describe(&json_call("edit", raw)),
    );
    assert_eq!(
        edit_describe(&json_call("", raw)),
        edit_describe(&json_call("edit", raw))
    );

    let raw = r#"{"pattern":"beta","path":"src"}"#;
    let call = native_call("BetaGrep", json_input(raw));
    let default = GrepTool::new(workspace.clone()).describe(&call);
    let renamed = GrepTool::new(workspace.clone())
        .with_face(ToolFace::new("BetaGrep", "renamed"), "gpt")
        .describe(&call);
    assert_eq!(renamed, default);
    assert_eq!(renamed.verb, "search");
    assert_eq!(
        search_describe(&json_call("BetaGrep", raw)),
        search_describe(&json_call("grep", raw)),
    );

    let raw = r#"{"file_path":"out.txt","content":"hi"}"#;
    let call = native_call("WriteFile", json_input(raw));
    let default = WriteTool::new(workspace.clone(), observed.clone()).describe(&call);
    let renamed = WriteTool::new(workspace.clone(), observed.clone())
        .with_face(ToolFace::new("WriteFile", "renamed"), "gpt")
        .describe(&call);
    assert_eq!(renamed, default);
    assert_eq!(
        write_describe(&json_call("WriteFile", raw)),
        write_describe(&json_call("write", raw)),
    );

    let patch = "*** Begin Patch\n*** Update File: src/a.rs\n@@\n-a\n+b\n*** End Patch\n";
    let call = native_call("ApplyPatch", ToolInput::Text(patch.to_string()));
    let default = PatchTool::new(workspace.clone(), observed.clone()).describe(&call);
    let renamed = PatchTool::new(workspace.clone(), observed.clone())
        .with_face(ToolFace::new("ApplyPatch", "renamed"), "gpt")
        .describe(&call);
    assert_eq!(renamed, default);
    assert_eq!(renamed.verb, "edit");
    assert_eq!(
        patch_describe(&text_call("ApplyPatch", patch)),
        patch_describe(&text_call("apply_patch", patch)),
    );
}

/// A tool the host has not assembled gets the contract's generic description — verb `call`,
/// the name in `target` (ADR-0057); the host's own fallback builds the same shape.
#[test]
fn an_unknown_tool_gets_the_hosts_generic_description() {
    let tool = Unassigned {
        declaration: ToolDeclaration {
            name: "mystery".into(),
            description: "not assembled".into(),
            kind: DeclarationKind::Function {
                input_schema: json!({}),
            },
        },
        identity: ToolIdentity {
            implementation: "test".into(),
            variant: "test".into(),
        },
    };
    assert_eq!(
        tool.describe(&native_call("mystery", json_input("{}"))),
        CallDescription {
            verb: "call",
            target: Some("mystery".into()),
            edit: None,
            destructive: false,
        }
    );

    // The host reaches that shape for a name it never assembled, and only there: a call whose
    // tool is not in the set is described generically, never by another tool's guess.
    let host = fs::read_to_string(repo_root().join("crates/p1-host/src/tui.rs")).expect("tui.rs");
    assert!(
        host.contains("verb: \"call\""),
        "the host's describe fallback must be the generic `call`"
    );
    assert!(
        host.contains("target: Some(call.name.clone())"),
        "the generic description puts the call's own name in `target`"
    );
}

/// A tool that leaves `describe` to the contract's default, as the host does for a call whose
/// tool is not assembled.
struct Unassigned {
    declaration: ToolDeclaration,
    identity: ToolIdentity,
}

impl Tool for Unassigned {
    fn declaration(&self) -> &ToolDeclaration {
        &self.declaration
    }

    fn identity(&self) -> &ToolIdentity {
        &self.identity
    }

    fn effect(&self, _call: &ToolCall) -> Effect {
        Effect::ReadOnly
    }

    fn execute<'a>(
        &'a self,
        _call: &'a ToolCall,
        _context: ToolContext,
    ) -> BoxFuture<'a, ToolOutcome> {
        Box::pin(async { ToolOutcome::error("the unassigned tool never runs") })
    }
}

// ------------------------------------------------------------- invalid input

/// Invalid input is classified at the tool's worst case: a file tool still writes files and a
/// search still only reads, whatever the model sent. The module's effect export is that
/// constant, the same worst case the native tool answers.
#[test]
fn effect_is_the_worst_case_for_invalid_input() {
    let (_dir, workspace, observed) = workspace();
    let edit = EditTool::new(workspace.clone(), observed.clone());
    let write = WriteTool::new(workspace.clone(), observed.clone());
    let patch = PatchTool::new(workspace.clone(), observed.clone());
    let search = GrepTool::new(workspace.clone());

    for raw in [
        "",
        "null",
        "[]",
        r#"{"file_path": 5}"#,
        r#"{"file_path":"a.txt","old_string":"","new_string":"b"}"#,
        "not json",
    ] {
        assert_eq!(
            edit.effect(&native_call("edit", json_input(raw))),
            Effect::WritesFiles,
            "{raw}"
        );
        assert_eq!(
            write.effect(&native_call("write", json_input(raw))),
            Effect::WritesFiles,
            "{raw}"
        );
        assert_eq!(
            patch.effect(&native_call("apply_patch", json_input(raw))),
            Effect::WritesFiles,
            "{raw}"
        );
        assert_eq!(
            search.effect(&native_call("grep", json_input(raw))),
            Effect::ReadOnly,
            "{raw}"
        );
    }
    assert_eq!(
        patch.effect(&native_call(
            "apply_patch",
            ToolInput::Text("not a patch".to_string()),
        )),
        Effect::WritesFiles
    );
}

/// Invalid input never panics the restricted path and yields a best-effort description with no
/// target — the same no-target, non-destructive, preview-free shape the native tools answer on
/// the same unparseable input, so a native/component divergence cannot creep in unpinned.
#[test]
fn invalid_input_describes_without_a_target() {
    let (_dir, workspace, observed) = workspace();
    let edit = EditTool::new(workspace.clone(), observed.clone());
    let write = WriteTool::new(workspace.clone(), observed.clone());
    let patch = PatchTool::new(workspace.clone(), observed.clone());
    let search = GrepTool::new(workspace.clone());

    // Each row is one input on both sides: the component's `describe` JSON and the native
    // tool's `describe` of the same text (`'not json'`, `'garbage'`, `'not a patch'`).
    for (described, native, verb) in [
        (
            edit_describe(&json_call("edit", "not json")),
            edit.describe(&native_call("edit", json_input("not json"))),
            "edit",
        ),
        (
            edit_describe(&json_call("edit", "garbage")),
            edit.describe(&native_call("edit", json_input("garbage"))),
            "edit",
        ),
        (
            write_describe(&json_call("write", "not json")),
            write.describe(&native_call("write", json_input("not json"))),
            "edit",
        ),
        (
            write_describe(&json_call("write", "garbage")),
            write.describe(&native_call("write", json_input("garbage"))),
            "edit",
        ),
        (
            patch_describe(&text_call("apply_patch", "not a patch")),
            patch.describe(&native_call(
                "apply_patch",
                ToolInput::Text("not a patch".to_string()),
            )),
            "edit",
        ),
        (
            patch_describe(&json_call("apply_patch", "garbage")),
            patch.describe(&native_call("apply_patch", json_input("garbage"))),
            "edit",
        ),
        (
            search_describe(&json_call("grep", "not json")),
            search.describe(&native_call("grep", json_input("not json"))),
            "search",
        ),
        (
            search_describe(&json_call("grep", "garbage")),
            search.describe(&native_call("grep", json_input("garbage"))),
            "search",
        ),
    ] {
        check_no_target(&described, &native, verb);
    }

    // A malformed wire call (not even a tool call) has no native counterpart: the host sends
    // only well-formed calls. The component still answers the same no-target shape.
    for (described, verb) in [
        (edit_describe("garbage"), "edit"),
        (write_describe("garbage"), "edit"),
        (patch_describe("garbage"), "edit"),
        (search_describe("garbage"), "search"),
    ] {
        let value: Value = serde_json::from_str(&described).expect("JSON");
        assert_eq!(value["verb"], verb, "{described}");
        assert!(value.get("target").is_none(), "{described}");
        assert_eq!(value["destructive"], false, "{described}");
        assert!(value.get("edit").is_none(), "{described}");
    }
}

/// The module's `effect` export is that worst case. The export itself needs the workspace
/// capabilities and cannot be linked before `wasm-loader-v1`, so its body — one constant — is
/// read here; the production loader checks the built component in the `.2` parity slices.
#[test]
fn the_module_effect_is_the_native_worst_case() {
    for (module, constant) in [
        (
            "modules/p1-module-edit/src/lib.rs",
            "CallEffect::WritesFiles",
        ),
        (
            "modules/p1-module-write/src/lib.rs",
            "CallEffect::WritesFiles",
        ),
        (
            "modules/p1-module-patch/src/lib.rs",
            "CallEffect::WritesFiles",
        ),
        (
            "modules/p1-module-search/src/lib.rs",
            "CallEffect::ReadOnly",
        ),
    ] {
        let source = fs::read_to_string(repo_root().join(module)).expect("the module source");
        let body = source
            .split("fn effect")
            .nth(1)
            .unwrap_or_else(|| panic!("{module} has no effect export"));
        let body = body.split('}').next().expect("the effect body");
        assert!(
            body.contains(constant),
            "{module}'s effect must be `{constant}`, got: {body:?}"
        );
    }
}

// ------------------------------------------------------------- the host names no tool

/// The host's describe/effect/result paths carry no literal tool name: a call is described
/// from the tool it was assembled with, looked up by the declaration name, never by matching
/// the name (ADR-0057, ADR-0059). The catalog rows that still name a native implementation are
/// U-cat's to replace and are listed in the PR body, so this scan is kept to the paths that
/// describe calls and results and classify effects.
#[test]
fn the_host_describe_effect_and_result_paths_carry_no_tool_name() {
    let root = repo_root();
    let mut files = Vec::new();
    for dir in ["crates/p1-host/src", "crates/p1-assembly/src"] {
        source_files(&root.join(dir), true, &mut files);
    }
    assert!(!files.is_empty(), "the scan must find the host's sources");

    for path in &files {
        let source = fs::read_to_string(path).expect("the host source");
        let code = without_verbs_or_tests(&source);
        for name in ["edit", "write", "apply_patch", "grep"] {
            assert!(
                !code.contains(&format!("\"{name}\"")),
                "{} names the tool `{name}` on a describe/effect/result path; a tool name is the \
                 catalog rows' business, never a classifier here",
                path.strip_prefix(&root).unwrap_or(path).display()
            );
        }

        // … and no path branches on the name either: a `call.name ==`/`match call.name`
        // comparison against one of the four is a classifier by name, whatever it is spelled
        // with. Scanned before the verb literals are taken out, so a comparison sharing a
        // `.verb` line cannot hide behind the exemption. Names outside the four filesystem
        // tools are other streams' (the delegation tool `worker_start`, S6, classifies itself
        // by name in `crates/p1-host/src/tui.rs`).
        for (line_number, line) in without_test_modules(&source).lines().enumerate() {
            let comparison = line.contains("call.name ==")
                || line.contains("call.name !=")
                || line.contains("result.name ==")
                || line.contains("match call.name");
            let names_a_tool = ["edit", "write", "apply_patch", "grep"]
                .iter()
                .any(|name| line.contains(&format!("\"{name}\"")));
            assert!(
                !(comparison && names_a_tool),
                "{}:{} compares a call's name against a tool name; the name is the lookup key, \
                 never a classifier: {line}",
                path.strip_prefix(&root).unwrap_or(path).display(),
                line_number + 1
            );
        }
    }

    // The paths select a tool by its declaration name — the name is the lookup key.
    for rel in [
        "crates/p1-host/src/tui/describer.rs",
        "crates/p1-host/src/tui.rs",
    ] {
        let source = fs::read_to_string(root.join(rel)).expect("the host source");
        assert!(
            source.contains("declaration().name == call.name"),
            "{rel} must look a call's tool up by its declaration name"
        );
    }
}

/// The model-facing description of each tool stays its module's: the host carries no copy of
/// it. An environment may override a face (that is the environment files' business), but
/// `p1-host` and `p1-assembly` hold no tool description of their own.
#[test]
fn the_tool_descriptions_live_only_in_their_modules() {
    let root = repo_root();
    let mut files = Vec::new();
    for dir in ["crates/p1-host/src", "crates/p1-assembly/src"] {
        source_files(&root.join(dir), false, &mut files);
    }
    assert!(!files.is_empty(), "the scan must find the host's sources");

    let descriptions = [
        p1_tool_edit_logic::DESCRIPTION,
        p1_tool_write_logic::DESCRIPTION,
        p1_tool_patch_logic::DESCRIPTION,
        p1_tool_search_logic::DESCRIPTION,
    ];
    for path in &files {
        let source = fs::read_to_string(path).expect("the host source");
        for description in descriptions {
            assert!(
                !source.contains(description),
                "{} carries a copy of a tool's model-facing description; that text belongs to \
                 the module alone",
                path.strip_prefix(&root).unwrap_or(path).display()
            );
        }
    }
}

/// The verb exemption is not fail-open: it removes a verb operand, never a whole line or
/// block; it survives a brace inside a string literal; and it keeps scanning after an inline
/// test module, so a tool-name classifier in any of those places stays visible.
#[test]
fn the_verb_scan_keeps_a_name_classifier_visible() {
    // (1) A name classifier sharing a `.verb` line keeps its tool-name literal.
    let shared = "if description.verb == \"edit\" && call.name == \"write\" {\n    go();\n}\n";
    let code = without_verbs_or_tests(shared);
    assert!(!code.contains("\"edit\""), "{code}");
    assert!(code.contains("\"write\""), "{code}");

    // (2) A `match … .verb { … }` block keeps its arm bodies: a classifier inside is scanned.
    let block = "let kind = match description.verb {\n    \"read\" | \"edit\" | \"write\" => call.name == \"grep\",\n    _ => false,\n};\nlet after = 1;\n";
    let code = without_verbs_or_tests(block);
    assert!(
        !code.contains("\"edit\"") && !code.contains("\"write\""),
        "{code}"
    );
    assert!(code.contains("\"grep\""), "{code}");
    assert!(code.contains("let after = 1;"), "{code}");

    // (3) A brace inside a string literal does not desynchronize the block skip.
    let braces_in_strings =
        "let x = match a.verb {\n    \"edit\" => \"}\",\n    _ => \"{\",\n};\nlet after = 1;\n";
    let code = without_verbs_or_tests(braces_in_strings);
    assert!(code.contains("let after = 1;"), "{code}");

    // (4) Production code below an inline test module is still scanned.
    let tests_then_code = "fn before() { let _ = \"grep\"; }\nmod tests {\n    fn t() { let _ = \"write\"; }\n}\nfn after() { let _ = \"apply_patch\"; }\n";
    let code = without_verbs_or_tests(tests_then_code);
    assert!(code.contains("grep"), "{code}");
    assert!(!code.contains("\"write\""), "{code}");
    assert!(code.contains("apply_patch"), "{code}");

    // The structural check runs before the verb literals go, so a comparison on a `.verb`
    // line cannot hide behind the exemption.
    let hidden = "if description.verb == \"edit\" && call.name == \"write\" { go(); }\n";
    assert!(
        without_test_modules(hidden).contains("call.name == \"write\""),
        "{hidden}"
    );
}

/// Every `.rs` file under `dir` that is not a test file. `skip_catalog` leaves out
/// `crates/p1-host/src/catalog`, whose tool rows still name a native implementation until
/// U-cat replaces them.
fn source_files(dir: &Path, skip_catalog: bool, out: &mut Vec<PathBuf>) {
    let mut entries: Vec<PathBuf> = fs::read_dir(dir)
        .expect("the host source directory")
        .map(|entry| entry.expect("a directory entry").path())
        .collect();
    entries.sort();
    for path in entries {
        if path.is_dir() {
            if skip_catalog && path.file_name().and_then(|name| name.to_str()) == Some("catalog") {
                continue;
            }
            source_files(&path, skip_catalog, out);
        } else if path.extension().and_then(|ext| ext.to_str()) == Some("rs")
            && !path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.ends_with("tests.rs"))
        {
            out.push(path);
        }
    }
}

/// The closed call-verb vocabulary (`docs/design/modules/protocol.md`, freeze item 8). A
/// string in a verb position is UI vocabulary, never a tool name, so the scan below does not
/// look at one.
const VERBS: [&str; 8] = [
    "read", "edit", "run", "search", "finish", "worker", "workflow", "call",
];

/// `source` with its inline test modules and its verb string literals removed. The verbs
/// (`edit`, `read`, `run`, …) are the tool's own vocabulary (`docs/design/modules/protocol.md`,
/// freeze item 8) — not a tool name — so the scan that follows looks only for a name the host
/// would branch on.
///
/// Only the verb *operand* is taken out, never a whole line or block, so a tool-name
/// classifier that shares a verb line (`description.verb == "edit" && call.name == "write"`)
/// or sits in a verb match block stays visible to the scan. A `#[cfg(test)]` module body is
/// skipped brace by brace — braces inside string literals do not count — and the scan
/// continues after it, so production code below a test module is not exempt.
fn without_verbs_or_tests(source: &str) -> String {
    strip_verbs(&without_test_modules(source))
}

/// `source` with each `#[cfg(test)]` module body replaced by newlines, the rest kept. A body
/// is matched brace by brace (braces inside string literals do not count) and the scan
/// continues after it, so production code below a test module is not exempt.
fn without_test_modules(source: &str) -> String {
    let mut out = String::with_capacity(source.len());
    let mut test_block = 0isize;
    let mut cfg_test = false;
    for line in source.lines() {
        if test_block > 0 {
            test_block += braces(line);
            out.push('\n');
            continue;
        }
        let trimmed = line.trim();
        if is_cfg_test(trimmed) {
            cfg_test = true;
            out.push('\n');
            continue;
        }
        if cfg_test {
            if trimmed.starts_with("#[") {
                // Another attribute of the same item.
                out.push('\n');
                continue;
            }
            if is_module_item(trimmed) {
                if trimmed.ends_with('{') {
                    test_block = braces(line);
                }
                cfg_test = false;
                out.push('\n');
                continue;
            }
            // The attribute decorated something else (a test-only method, say); scan it.
            cfg_test = false;
        }
        // A `mod tests` with no attribute line above it.
        if trimmed == "mod tests {" {
            test_block = braces(line);
            out.push('\n');
            continue;
        }
        if trimmed == "mod tests;" {
            out.push('\n');
            continue;
        }
        out.push_str(line);
        out.push('\n');
    }
    out
}

/// Whether a trimmed line declares a module (`mod x {`, `mod x;`, `pub mod x {`).
fn is_module_item(trimmed: &str) -> bool {
    trimmed
        .strip_prefix("pub ")
        .unwrap_or(trimmed)
        .strip_prefix("mod ")
        .is_some_and(|rest| rest.ends_with('{') || rest.ends_with(';'))
}

/// Whether a trimmed line is a `cfg` attribute that turns test on (`#[cfg(test)]`,
/// `#[cfg(all(test, feature = "…"))]`), never `#[cfg(not(test))]` or a feature gate.
fn is_cfg_test(trimmed: &str) -> bool {
    trimmed.starts_with("#[cfg(") && trimmed.contains("test") && !trimmed.contains("not(")
}

/// `source` with the verb string literals removed: a `… .verb == "edit"` comparison keeps its
/// operand only (`edit` is vocabulary), and a `match … .verb { … }` block keeps its arm bodies.
fn strip_verbs(source: &str) -> String {
    let mut out = String::with_capacity(source.len());
    let mut verb_block = 0isize;
    for line in source.lines() {
        if verb_block > 0 {
            out.push_str(&without_verb_literals(line));
            out.push('\n');
            verb_block += braces(line);
            continue;
        }
        if line.contains(".verb") {
            if line.contains("match") && line.contains('{') {
                verb_block = braces(line);
                out.push_str(&without_string_literals(line));
            } else {
                out.push_str(&without_verb_operand(line));
            }
            out.push('\n');
            continue;
        }
        out.push_str(line);
        out.push('\n');
    }
    out
}

/// A `match … .verb { … }` block line with its verb literals removed: the string literals
/// before the arm's `=>` (its patterns and any guard) go, the arm body after it stays, so a
/// `call.name` comparison inside a body is still scanned. A line with no `=>` yet takes out
/// only the closed vocabulary, leaving any other literal for the scan.
fn without_verb_literals(line: &str) -> String {
    match line.find("=>") {
        Some(arrow) => format!(
            "{}{}",
            without_string_literals(&line[..arrow]),
            &line[arrow..]
        ),
        None => without_closed_verbs(line),
    }
}

/// `line` with the operand after `.verb` removed, when that operand is closed vocabulary; the
/// rest of the line (a `call.name == "write"` sharing it) stays so the scan can see it.
fn without_verb_operand(line: &str) -> String {
    let Some((before, after)) = line.split_once(".verb") else {
        return line.to_string();
    };
    let Some(open) = after.find('"') else {
        return line.to_string();
    };
    let Some(close) = after[open + 1..].find('"') else {
        return line.to_string();
    };
    if !VERBS
        .iter()
        .any(|verb| *verb == &after[open + 1..open + 1 + close])
    {
        return line.to_string();
    }
    format!(
        "{before}.verb{}{}",
        &after[..open],
        &after[open + 1 + close + 1..]
    )
}

/// `line` with every string literal removed, keeping everything else. A removed literal cannot
/// desynchronize a brace count.
fn without_string_literals(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut in_string = false;
    let mut chars = line.chars();
    while let Some(character) = chars.next() {
        match character {
            '\\' if in_string => {
                chars.next();
            }
            '"' => in_string = !in_string,
            _ if !in_string => out.push(character),
            _ => {}
        }
    }
    out
}

/// `line` with only the closed-vocabulary string literals removed; a literal outside the
/// vocabulary stays, so a host that compares a call's name to it is still scanned.
fn without_closed_verbs(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut rest = line;
    while let Some(open) = rest.find('"') {
        out.push_str(&rest[..open]);
        let after = &rest[open + 1..];
        let Some(close) = after.find('"') else {
            out.push_str(&rest[open..]);
            return out;
        };
        if !VERBS.iter().any(|verb| *verb == &after[..close]) {
            out.push('"');
            out.push_str(&after[..close]);
            out.push('"');
        }
        rest = &after[close + 1..];
    }
    out.push_str(rest);
    out
}

/// The net brace depth of one line, ignoring braces inside string literals.
fn braces(line: &str) -> isize {
    let mut depth = 0isize;
    let mut in_string = false;
    let mut chars = line.chars();
    while let Some(character) = chars.next() {
        match character {
            '\\' if in_string => {
                chars.next();
            }
            '"' => in_string = !in_string,
            '{' if !in_string => depth += 1,
            '}' if !in_string => depth -= 1,
            _ => {}
        }
    }
    depth
}

/// The repository root: `CARGO_MANIFEST_DIR` is `crates/p1-module-tests`.
fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("the repository root")
        .to_path_buf()
}
