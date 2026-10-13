//! E5: the shell tool's guest logic over the channel, running a real command through the
//! runtime's own (unsandboxed) process service in a scratch directory.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use p1_channel_spike::process::ProcessChannel;
use p1_channel_spike::{Capability, Grants, Plugin};
use p1_module_protocol::{WireToolCall, WireToolInput, WireToolOutcome, WireToolStatus};
use p1_module_runtime::process::{ProcessCapability, ProcessService};

fn shell() -> Vec<u8> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../modules/target/p1-channel/channel-shell.wasm");
    std::fs::read(&path).unwrap_or_else(|error| {
        panic!(
            "{}: {error}; run crates/p1-channel-spike/build-guest.sh",
            path.display()
        )
    })
}

fn call(input: &str) -> String {
    serde_json::to_string(&WireToolCall {
        call_id: "c1".to_owned(),
        name: "shell".to_owned(),
        input: WireToolInput::Json(input.to_owned()),
    })
    .expect("a call serializes")
}

#[tokio::test(flavor = "current_thread")]
async fn the_shell_guest_runs_a_command_over_the_channel() {
    let workspace = tempfile::tempdir().expect("scratch");
    let service = ProcessCapability::new(Arc::new(ProcessService::new(workspace.path())));
    let process: Arc<dyn Capability> = Arc::new(ProcessChannel(Arc::new(service)));
    let grants: Grants = [("process".to_owned(), process)].into();
    let engine = p1_module_runtime::engine().expect("engine");
    let plugin = Plugin::load(&engine, &shell(), &grants)
        .await
        .expect("loads");

    let answer = plugin
        .emit(
            "tool/execute",
            &call(r#"{"command":"printf 'hello\\n'; printf 'world\\n' >&2; exit 3"}"#),
            HashMap::new(),
        )
        .await
        .expect("runs")
        .expect("answers");
    let outcome: WireToolOutcome = serde_json::from_str(&answer).expect("an outcome");
    assert_eq!(outcome.status, WireToolStatus::Ok, "{}", outcome.content);
    assert!(outcome.content.contains("hello"), "{}", outcome.content);
    assert!(outcome.content.contains("world"), "{}", outcome.content);
    assert!(
        outcome.content.contains("[exit code: 3]"),
        "{}",
        outcome.content
    );

    let described = plugin
        .emit(
            "tool/describe",
            &call(r#"{"command":"rm -rf build"}"#),
            HashMap::new(),
        )
        .await
        .expect("runs")
        .expect("answers");
    let described: serde_json::Value = serde_json::from_str(&described).expect("JSON");
    assert_eq!(described["destructive"], true, "{described}");
}

#[tokio::test(flavor = "current_thread")]
async fn without_the_grant_the_shell_runs_nothing() {
    let engine = p1_module_runtime::engine().expect("engine");
    let plugin = Plugin::load(&engine, &shell(), &Grants::new())
        .await
        .expect("loads");
    let answer = plugin
        .emit(
            "tool/execute",
            &call(r#"{"command":"touch marker"}"#),
            HashMap::new(),
        )
        .await
        .expect("runs")
        .expect("answers");
    let outcome: WireToolOutcome = serde_json::from_str(&answer).expect("an outcome");
    assert_eq!(outcome.status, WireToolStatus::Error);
    assert!(
        outcome.content.contains("not granted"),
        "{}",
        outcome.content
    );
}
