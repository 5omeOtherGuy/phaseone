//! The new import is tool-only, p1/ls-only, and linked to the production file services.
use p1_contracts::serde_json::{Value, json};
use p1_contracts::{CancellationToken, ToolCall, ToolContext, ToolInput, ToolStatus};
use p1_module_runtime::{Digest, ExecutionLimits, LoadError, Services, wasm_tool};
use p1_module_tests::Release;
use p1_redact::MaskCounter;
use p1_workspace::{ObservedFiles, Workspace};
use std::sync::Arc;

fn importing() -> Vec<u8> {
    let name = "p1:module/directory-listing@1.0.0";
    let mut bytes = vec![0, 0x61, 0x73, 0x6d, 0x0d, 0, 1, 0, 7, 3, 1, 0x42, 0];
    let mut import = vec![1, 0, name.len() as u8];
    import.extend(name.as_bytes());
    import.extend([5, 0]);
    bytes.extend([0x0a, import.len() as u8]);
    bytes.extend(import);
    bytes
}
fn release(name: &str, caps: Value, bytes: &[u8]) -> Release {
    let mut release = Release::empty();
    release.add(
        json!({"name": name, "digest": Digest::of(bytes).to_string(),
        "path":"packages/probe/probe.wasm", "kind":"tool", "world":"p1:module/tool@1.0.0",
        "protocol":"1.0", "capabilities":caps, "variant":"claude"}),
        bytes,
    );
    release
}
#[test]
fn only_ls_may_import_directory_listing_and_it_must_declare_it() {
    let bytes = importing();
    let allowed = release("p1/ls", json!(["directory-listing"]), &bytes);
    assert!(allowed.loader().load("p1/ls").is_ok());
    for (name, caps) in [
        ("p1/ls", json!([])),
        ("p1/search", json!(["directory-listing"])),
    ] {
        let denied = release(name, caps, &bytes);
        assert!(
            matches!(
                denied.loader().load(name),
                Err(LoadError::UndeclaredImport { .. })
            ),
            "{name}"
        );
    }
}

#[tokio::test]
async fn component_lists_with_host_services_and_refuses_escape_and_bad_input() {
    let dir = p1_module_tests::fixture_dir()
        .parent()
        .unwrap()
        .join("p1-module-ls");
    let bytes = std::fs::read(dir.join("p1-module-ls.wasm")).expect("build ls module first");
    let release = release("p1/ls", json!(["workspace", "directory-listing"]), &bytes);
    let loaded = release.loader().load("p1/ls").unwrap();
    assert!(
        wasm_tool(
            &loaded,
            Services::default(),
            ExecutionLimits::default(),
            &Arc::new(MaskCounter::new())
        )
        .is_err()
    );
    let temp = tempfile::tempdir().unwrap();
    std::fs::create_dir(temp.path().join("a")).unwrap();
    std::fs::File::create(temp.path().join("a/child")).unwrap();
    std::fs::File::create(temp.path().join(".hidden")).unwrap();
    std::os::unix::fs::symlink("a", temp.path().join("link")).unwrap();
    let services = p1_host::catalog::capability_services_for(
        "p1/ls",
        Workspace::new(temp.path())
            .unwrap()
            .with_credential_paths(Vec::new()),
        ObservedFiles::new(),
        None,
    );
    let tool = wasm_tool(
        &loaded,
        services,
        ExecutionLimits::default(),
        &Arc::new(MaskCounter::new()),
    )
    .unwrap();
    assert_eq!(tool.declaration().name, "ls");
    let description_call = ToolCall {
        call_id: "describe".into(),
        name: "ls".into(),
        input: ToolInput::Json("{}".into()),
    };
    assert_eq!(
        tool.effect(&description_call),
        p1_contracts::Effect::ReadOnly
    );
    assert_eq!(
        tool.describe(&description_call).target.as_deref(),
        Some(".")
    );
    // ADR-0118 Decision 1: a listing always overlaps other reads.
    assert_eq!(
        tool.concurrency(&description_call),
        p1_contracts::Concurrency::Shared
    );
    for (args, expected) in [
        (json!({"depth":2}), ToolStatus::Ok),
        (json!({"path":".."}), ToolStatus::Error),
        (json!({"path":"/"}), ToolStatus::Error),
        (
            json!({"cursor": p1_workspace::listing_cursor("../outside")}),
            ToolStatus::Error,
        ),
        (json!({"limit":501}), ToolStatus::Error),
    ] {
        let call = ToolCall {
            call_id: "listing".into(),
            name: "ls".into(),
            input: ToolInput::Json(args.to_string()),
        };
        let outcome = tool
            .execute(
                &call,
                ToolContext {
                    cancel: CancellationToken::new(),
                },
            )
            .await;
        assert_eq!(outcome.status, expected, "{}", outcome.content);
        if expected == ToolStatus::Ok {
            assert!(
                outcome.content.starts_with(".hidden\na/\n  child\nlink@\n"),
                "{}",
                outcome.content
            );
            assert!(
                outcome
                    .content
                    .contains("[4 entries; 4 shown; 4 names read]"),
                "{}",
                outcome.content
            );
        }
    }
}
