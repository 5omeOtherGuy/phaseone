#[path = "../src/tui/describer.rs"]
mod describer;

use std::sync::Arc;

use describer::HostDescriber;
use p1_contracts::{
    BoxFuture, DeclarationKind, Effect, Tool, ToolCall, ToolContext, ToolDeclaration, ToolIdentity,
    ToolInput, ToolOutcome, ToolResultItem, ToolStatus,
};
use p1_tool_edit::{EditTool, ToolFace};
use p1_tui::face::{FaceBody, ToolDescriber};
use p1_workspace::{ObservedFiles, Workspace};

struct FutureTool {
    declaration: ToolDeclaration,
    identity: ToolIdentity,
}

impl Tool for FutureTool {
    fn declaration(&self) -> &ToolDeclaration {
        &self.declaration
    }
    fn identity(&self) -> &ToolIdentity {
        &self.identity
    }
    fn effect(&self, _: &ToolCall) -> Effect {
        Effect::ReadOnly
    }
    fn execute<'a>(&'a self, _: &'a ToolCall, _: ToolContext) -> BoxFuture<'a, ToolOutcome> {
        Box::pin(async { ToolOutcome::ok("first line\nsecond line") })
    }
}

fn call(name: &str, input: &str) -> ToolCall {
    ToolCall {
        call_id: "c1".into(),
        name: name.into(),
        input: ToolInput::Json(input.into()),
    }
}

#[test]
fn renamed_edit_diff_and_unknown_tool_default_both_render() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("file.txt"), "new\n").unwrap();
    let tools: Arc<Vec<Arc<dyn Tool>>> = Arc::new(vec![
        Arc::new(
            EditTool::new(Workspace::new(dir.path()).unwrap(), ObservedFiles::new())
                .with_face(ToolFace::new("ReplaceFile", "renamed edit"), "test"),
        ),
        Arc::new(FutureTool {
            declaration: ToolDeclaration {
                name: "brand_new".into(),
                description: "future".into(),
                kind: DeclarationKind::Function {
                    input_schema: serde_json::json!({}),
                },
            },
            identity: ToolIdentity {
                implementation: "future".into(),
                variant: "default".into(),
            },
        }),
    ]);
    let d = HostDescriber::new(dir.path().into(), "off".into(), tools);
    let edit = call(
        "ReplaceFile",
        r#"{"file_path":"file.txt","old_string":"old","new_string":"new"}"#,
    );
    assert_eq!(d.call(&edit).target, "file.txt");
    let result = ToolResultItem {
        call_id: "c1".into(),
        name: edit.name.clone(),
        status: ToolStatus::Ok,
        content: "Edited file.txt (1 replacement).".into(),
    };
    let face = d.result(&edit, &result, None);
    assert_eq!(face.outcome.as_deref(), Some("+1 −1"));
    assert!(matches!(face.body, FaceBody::Diff(_)));

    let future = call("brand_new", r#"{"unfamiliar":true}"#);
    assert_eq!(d.call(&future).target, "brand_new");
    let result = ToolResultItem {
        call_id: "c1".into(),
        name: future.name.clone(),
        status: ToolStatus::Ok,
        content: "first line\nsecond line".into(),
    };
    let face = d.result(&future, &result, None);
    assert_eq!(face.outcome.as_deref(), Some("first line"));
    assert_eq!(face.body, FaceBody::None);
}
