#![cfg(feature = "shadow-hook")]

mod common;

use std::ffi::OsString;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use common::{Harness, provider_hook, run_args, write_environment};
use p1_hook_shadow::ShadowHook;
use p1_testkit::{ScriptedProvider, json_call, text_response, tool_call_response};

#[test]
fn shadow_settings_are_typed_and_reject_unknown_keys() {
    let settings: p1_host::models::Settings =
        toml::from_str("[shadow]\nbrain_packet_shadow = \"/tmp/fake-shadow\"\n").unwrap();
    assert_eq!(
        settings.shadow.unwrap().brain_packet_shadow.unwrap(),
        Path::new("/tmp/fake-shadow")
    );
    let error = toml::from_str::<p1_host::models::Settings>(
        "[shadow]\nbrain_packet_shdow = \"/tmp/fake-shadow\"\n",
    )
    .unwrap_err()
    .to_string();
    assert!(error.contains("brain_packet_shdow"), "{error}");
}

fn fake_hook(workspace: &Path) -> Arc<ShadowHook> {
    let binary = workspace.join("fake-shadow");
    fs::write(
        &binary,
        "#!/bin/sh\nprintf '%s\\n' \"$@\" > \"$6.args.tmp\"\nmv \"$6.args.tmp\" \"$6.args\"\n",
    )
    .unwrap();
    fs::set_permissions(&binary, fs::Permissions::from_mode(0o700)).unwrap();
    let state = workspace.join("brain-state");
    let home = workspace.join("home");
    Arc::new(ShadowHook::new(
        binary,
        Arc::new(move |name| match name {
            "BRAIN_PACKET_STATE" => Some(OsString::from(&state)),
            "HOME" => Some(OsString::from(&home)),
            _ => None,
        }),
    ))
}

fn wait_for_args(state: &Path, count: usize) -> Vec<Vec<String>> {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let args: Vec<_> = fs::read_dir(state.join("inbox"))
            .unwrap()
            .filter_map(Result::ok)
            .filter_map(|entry| {
                let path = entry.path();
                if path.extension().is_some_and(|ext| ext == "args") {
                    let text = fs::read_to_string(path).ok()?;
                    Some(text.lines().map(str::to_string).collect())
                } else {
                    None
                }
            })
            .collect();
        if args.len() == count {
            return args;
        }
        assert!(Instant::now() < deadline, "expected {count} shadow spawns");
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[tokio::test]
async fn committed_parent_input_is_observed() {
    let workspace = tempfile::tempdir().unwrap();
    let environments = tempfile::tempdir().unwrap();
    write_environment(
        environments.path(),
        "a",
        "fake",
        "model",
        &["finish"],
        "test",
    );
    let provider = ScriptedProvider::new(vec![
        text_response("stopped"),
        tool_call_response(vec![json_call(
            "f1",
            "finish",
            r#"{"status":"done","summary":"done","verification":["none"]}"#,
        )]),
        text_response("done"),
    ]);
    let mut harness = Harness::new(vec![environments.path().to_path_buf()], &[]);
    harness.deps.catalog_hook = Some(provider_hook(vec![("fake", provider)]));
    harness.deps.shadow = Some(fake_hook(workspace.path()));
    let session = workspace.path().join("session.jsonl");
    let code = run_args(
        &mut harness,
        &[
            "--env",
            "a",
            "--workspace",
            workspace.path().to_str().unwrap(),
            "--session",
            session.to_str().unwrap(),
            "one real prompt",
        ],
    )
    .await;
    assert_eq!(code, 0, "{}", harness.stderr.text());
    let records = p1_journal::load(&session).unwrap().records;
    assert!(records.iter().any(|record| matches!(
        &record.body,
        p1_contracts::RecordBody::UserInput { text }
            if text == p1_host::run::CONTINUATION_MESSAGE
    )));
    let args = wait_for_args(&workspace.path().join("brain-state"), 1);
    assert_eq!(args[0][..4], ["--harness", "p1", "--origin", "hook"]);
    let task = Path::new(&args[0][5]);
    assert_eq!(fs::read_to_string(task).unwrap(), "one real prompt");
    assert_eq!(args[0][7], workspace.path().to_str().unwrap());
}

#[cfg(feature = "delegation")]
#[tokio::test]
async fn worker_dispatch_is_observed_after_its_own_commit() {
    let workspace = tempfile::tempdir().unwrap();
    let environments = tempfile::tempdir().unwrap();
    write_environment(
        environments.path(),
        "a",
        "fake-a",
        "model-a",
        &["worker_start", "worker_result"],
        "parent",
    );
    write_environment(
        environments.path(),
        "b",
        "fake-b",
        "model-b",
        &["read"],
        "child",
    );
    let parent = ScriptedProvider::new(vec![
        tool_call_response(vec![json_call(
            "c1",
            "worker_start",
            r#"{"environment":"b","task":"brief for child","tools":["read"]}"#,
        )]),
        text_response("started"),
        text_response("ack"),
    ]);
    let child = ScriptedProvider::new(vec![text_response("done")]);
    let mut harness = Harness::new(vec![environments.path().to_path_buf()], &[]);
    harness.deps.catalog_hook = Some(provider_hook(vec![("fake-a", parent), ("fake-b", child)]));
    harness.deps.shadow = Some(fake_hook(workspace.path()));
    let session = workspace.path().join("session.jsonl");
    let code = run_args(
        &mut harness,
        &[
            "--yes",
            "--env",
            "a",
            "--workspace",
            workspace.path().to_str().unwrap(),
            "--session",
            session.to_str().unwrap(),
            "start",
        ],
    )
    .await;
    assert_eq!(code, 0, "{}", harness.stderr.text());
    let args = wait_for_args(&workspace.path().join("brain-state"), 2);
    let child = args
        .iter()
        .find(|args| args.contains(&"--family".to_string()))
        .unwrap();
    assert_eq!(
        child[child.iter().position(|arg| arg == "--family").unwrap() + 1],
        "worker"
    );
    assert_eq!(
        child[child.iter().position(|arg| arg == "--provider").unwrap() + 1],
        "p1"
    );
    let source = child.iter().position(|arg| arg == "--source-ref").unwrap();
    assert_eq!(
        child[source + 1],
        format!("{}:1", session.with_extension("jsonl.w1.jsonl").display())
    );
    assert_eq!(fs::read_to_string(&child[5]).unwrap(), "brief for child");
}
