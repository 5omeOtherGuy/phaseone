//! `--compact` on `--resume` (ADR-0076, issue #203): the resumed history is
//! summarized once before the first turn, so the first model request carries the
//! summary and not the old history.
//!
//! Fake providers, captured output, `tempfile` workspaces: no network, no real
//! credentials, no sleeps.

mod common;

use common::{Harness, provider_hook, run_args};
use p1_contracts::{Item, RecordBody};
use p1_testkit::{ScriptedProvider, text_response};
use tempfile::tempdir;

/// A summarizer that never triggers on its own: only the flag summarizes.
const CONTEXT_TABLE: &str = "[context]\nwindow_tokens = 100000\noutput_headroom_tokens = 1000\nsummarize_at_tokens = 90000\nkeep_recent_tokens = 20\nuser_verbatim_tokens = 100\n";

fn write_context_environment(root: &std::path::Path) {
    let dir = root.join("small");
    std::fs::create_dir_all(&dir).unwrap();
    let toml = format!(
        "family = \"small\"\nprovider = \"fake\"\nmodel = \"fake-model\"\n\n{CONTEXT_TABLE}"
    );
    std::fs::write(dir.join("environment.toml"), toml).unwrap();
    std::fs::write(dir.join("prompt.md"), "test").unwrap();
}

#[tokio::test]
async fn compact_with_resume_summarizes_before_the_first_request() {
    let workspace = tempdir().unwrap();
    let environments = tempdir().unwrap();
    write_context_environment(environments.path());
    let session_path = workspace.path().join("session.jsonl");
    let workspace_arg = workspace.path().to_str().unwrap().to_string();
    let session_arg = session_path.to_str().unwrap().to_string();

    // A session of three exchanges, far below the threshold.
    let old_answer = "a".repeat(400);
    let first = ScriptedProvider::new(vec![
        text_response(&old_answer),
        text_response(&"b".repeat(400)),
        text_response(&"c".repeat(400)),
    ]);
    let mut harness = Harness::new(
        vec![environments.path().to_path_buf()],
        &["one", "two", "three", "/exit"],
    );
    harness.deps.catalog_hook = Some(provider_hook(vec![("fake", first)]));
    let code = run_args(
        &mut harness,
        &[
            "--env",
            "small",
            "--workspace",
            &workspace_arg,
            "--session",
            &session_arg,
        ],
    )
    .await;
    assert_eq!(code, 0, "stderr: {}", harness.stderr.text());

    let second = ScriptedProvider::new(vec![text_response("SUMMARY"), text_response("done")]);
    let mut harness = Harness::new(vec![environments.path().to_path_buf()], &[]);
    harness.deps.catalog_hook = Some(provider_hook(vec![("fake", second.clone())]));
    let code = run_args(
        &mut harness,
        &[
            "--env",
            "small",
            "--workspace",
            &workspace_arg,
            "--session",
            &session_arg,
            "--resume",
            "--compact",
            "four",
        ],
    )
    .await;
    assert_eq!(code, 0, "stderr: {}", harness.stderr.text());
    assert!(
        harness.stderr.text().contains("compacted: ") && harness.stderr.text().contains(" tokens"),
        "stderr: {}",
        harness.stderr.text()
    );

    let requests = second.requests();
    assert_eq!(requests.len(), 2, "one summary, then the turn");
    assert_eq!(
        requests[0].system_prompt,
        p1_context::DEFAULT_SUMMARIZER_PROMPT,
        "the summary comes before the first model request"
    );
    let first_turn = &requests[1].history;
    assert!(
        matches!(&first_turn[0], Item::User { text } if text.starts_with(p1_context::SUMMARY_MARKER)),
        "the first request carries the summary: {first_turn:?}"
    );
    assert!(
        !first_turn.iter().any(
            |item| matches!(item, Item::Assistant(assistant) if assistant.text() == old_answer)
        ),
        "the first request no longer carries the old history"
    );
    assert!(matches!(first_turn.last(), Some(Item::User { text }) if text == "four"));

    // The journal: the replacement is committed before the turn's input.
    let records = p1_journal::load(&session_path).unwrap().records;
    let replaced = records
        .iter()
        .position(|record| matches!(record.body, RecordBody::ContextReplaced { .. }))
        .expect("the compaction is journalled");
    let input = records
        .iter()
        .position(|record| matches!(&record.body, RecordBody::UserInput { text } if text == "four"))
        .expect("the turn's input is journalled");
    assert!(replaced < input);
    for (index, record) in records.iter().enumerate() {
        assert_eq!(record.seq, index as u64, "seq stays dense");
    }
}

#[test]
fn compact_without_resume_is_a_usage_error() {
    let args: Vec<String> = ["--session", "s.jsonl", "--compact", "go"]
        .iter()
        .map(|arg| arg.to_string())
        .collect();
    let error = p1_host::cli::parse(&args).unwrap_err();
    assert_eq!(
        error.message,
        "--compact needs --resume (a fresh session has nothing to compact)"
    );
}
