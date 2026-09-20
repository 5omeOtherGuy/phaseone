//! `scripts/run-report.py` reads the REAL journal format: this test records a session
//! through the host (real `shell`, `read`, `write` tools, scripted model), runs the
//! script on the file and checks the evidence record. It is what keeps the script
//! honest when a record shape changes. The point of the record (review 2026-09-20):
//! a non-zero shell exit is not a failed tool call, so it is counted on its own.

mod common;

use common::{Harness, provider_hook, run_args, write_environment};
use p1_contracts::serde_json::{self, Value};
use p1_contracts::{StopReason, StreamEvent, Usage};
use p1_testkit::{ScriptedProvider, Step, json_call, text_block, tool_call_response};
use tempfile::tempdir;

fn text_with_usage(text: &str, usage: Usage) -> Step {
    Step::Events(vec![
        StreamEvent::TextDelta {
            block: 0,
            text: text.into(),
        },
        StreamEvent::Finished(p1_testkit::completed(
            vec![text_block(text)],
            StopReason::EndTurn,
            Some(usage),
        )),
    ])
}

#[tokio::test]
async fn the_report_counts_what_the_journal_holds() {
    let workspace = tempdir().unwrap();
    let environments = tempdir().unwrap();
    write_environment(
        environments.path(),
        "plain",
        "fake",
        "fake-model",
        &["read", "write", "shell"],
        "test",
    );
    let session = workspace.path().join("session.jsonl");
    let provider = ScriptedProvider::new(vec![
        tool_call_response(vec![
            json_call("c1", "shell", r#"{"command":"echo fine"}"#),
            json_call("c2", "shell", r#"{"command":"echo broken >&2; exit 3"}"#),
            json_call("c3", "read", r#"{"path":"missing.txt"}"#),
            json_call("c4", "write", r#"{"file_path":"out.txt","content":"hi"}"#),
            json_call("c5", "no_such_tool", "{}"),
        ]),
        text_with_usage(
            "done",
            Usage {
                input_uncached: Some(100),
                cache_read: Some(300),
                cache_write: None,
                output: Some(40),
                reasoning_output: None,
                cost_micro_usd: None,
            },
        ),
    ]);
    let mut harness = Harness::new(vec![environments.path().to_path_buf()], &[]);
    harness.deps.catalog_hook = Some(provider_hook(vec![("fake", provider)]));
    let code = run_args(
        &mut harness,
        &[
            "--yes",
            "--env",
            "plain",
            "--workspace",
            workspace.path().to_str().unwrap(),
            "--session",
            session.to_str().unwrap(),
            "go",
        ],
    )
    .await;
    assert_eq!(code, 0, "stderr: {}", harness.stderr.text());

    let script = concat!(env!("CARGO_MANIFEST_DIR"), "/../../scripts/run-report.py");
    let output = std::process::Command::new("python3")
        .arg(script)
        .arg(&session)
        .args(["--label", "unit", "--elapsed", "1.5", "--exit-code", "0"])
        .args(["--accepted", "yes", "--interventions", "0"])
        .output()
        .expect("python3 runs");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report: Value = serde_json::from_slice(&output.stdout).expect("one JSON record");

    assert_eq!(report["origin"]["route"], "fake-route");
    assert_eq!(report["requests"], 2);
    assert_eq!(report["user_inputs"], 1);
    assert_eq!(report["tool_calls"], 5);
    assert_eq!(report["tool_calls_by_status"]["ok"], 3);
    assert_eq!(report["tool_calls_by_status"]["error"], 1);
    assert_eq!(report["tool_calls_by_status"]["unavailable"], 1);
    assert_eq!(report["tool_calls_not_ok"], 2);
    // The failing command is an OK tool call — and is still visible as a failure.
    assert_eq!(report["shell_exits"]["zero"], 1);
    assert_eq!(report["shell_exits"]["non_zero"], 1);
    assert_eq!(report["tool_calls_started_without_result"], 0);
    // Usage: known parts are summed, unknown stays null, never zero.
    assert_eq!(report["usage"]["input_uncached"], 100);
    assert_eq!(report["usage"]["cache_read"], 300);
    assert_eq!(report["usage"]["cache_write"], Value::Null);
    assert_eq!(report["usage"]["cost_micro_usd"], Value::Null);
    assert_eq!(report["input_total"], 400);
    assert_eq!(report["cache_read_share"], 0.75);
    // The first response carried no usage at all.
    assert_eq!(report["responses_without_usage"], 1);
    assert_eq!(report["accepted"], "yes");
    assert_eq!(report["elapsed_seconds"], 1.5);
    assert_eq!(report["includes_worker_usage"], false);
}
