//! Whole-host wiring for context control (context.md §3): an environment that
//! opts in with `[context]` gets a summarizer built from its own provider and
//! options; one without the table is passthrough.
//!
//! Fake providers, captured output, `tempfile` workspaces: no network, no real
//! credentials, no sleeps.

mod common;

use common::{Harness, provider_hook, run_args, write_environment};
use p1_testkit::{ScriptedProvider, text_response};
use tempfile::tempdir;

/// All five token fields present, `summarize_at_tokens = 580` so a 2000-character
/// first answer does not trigger on the second turn but the third turn does.
const CONTEXT_TABLE: &str = "[context]\nwindow_tokens = 100000\noutput_headroom_tokens = 1000\nsummarize_at_tokens = 580\nkeep_recent_tokens = 20\nuser_verbatim_tokens = 100\n";

fn write_context_environment(root: &std::path::Path, name: &str, table: &str) {
    let dir = root.join(name);
    std::fs::create_dir_all(&dir).unwrap();
    let toml =
        format!("family = \"{name}\"\nprovider = \"fake\"\nmodel = \"fake-model\"\n\n{table}");
    std::fs::write(dir.join("environment.toml"), toml).unwrap();
    std::fs::write(dir.join("prompt.md"), "test").unwrap();
}

fn is_summary_request(request: &p1_contracts::ProviderRequest) -> bool {
    request.system_prompt == p1_context::DEFAULT_SUMMARIZER_PROMPT
        || request.system_prompt == "CUSTOM PROMPT"
}

// (a) an environment with `[context]` summarizes, renders the line, journals it.
#[tokio::test]
async fn an_opted_in_environment_summarizes_and_journals_the_replacement() {
    let workspace = tempdir().unwrap();
    let environments = tempdir().unwrap();
    write_context_environment(environments.path(), "small", CONTEXT_TABLE);
    let session_path = workspace.path().join("session.jsonl");

    let provider = ScriptedProvider::new(vec![
        text_response(&"a".repeat(2_000)),
        text_response(&"b".repeat(50)),
        text_response("SUMMARY"),
        text_response("done"),
    ]);
    let handle = provider.clone();
    let mut harness = Harness::new(
        vec![environments.path().to_path_buf()],
        &["one", "two", "three", "/exit"],
    );
    harness.deps.catalog_hook = Some(provider_hook(vec![("fake", provider)]));

    let code = run_args(
        &mut harness,
        &[
            "--env",
            "small",
            "--workspace",
            workspace.path().to_str().unwrap(),
            "--session",
            session_path.to_str().unwrap(),
        ],
    )
    .await;

    assert_eq!(code, 0, "stderr: {}", harness.stderr.text());
    assert!(
        harness.stderr.text().contains("context: summarized"),
        "stderr: {}",
        harness.stderr.text()
    );

    let requests = handle.requests();
    let summaries: Vec<usize> = requests
        .iter()
        .enumerate()
        .filter(|(_, request)| is_summary_request(request))
        .map(|(index, _)| index)
        .collect();
    assert_eq!(summaries.len(), 1, "one summarization request");
    let summary = &requests[summaries[0]];
    assert!(summary.tools.is_empty(), "the summarizer has no tools");
    assert_eq!(summary.history.len(), 1, "one rendered transcript item");
    assert!(
        summaries[0] < requests.len() - 1,
        "the summary request comes before the next model request"
    );

    let journal = std::fs::read_to_string(&session_path).unwrap();
    assert!(journal.contains("context_replaced"), "{journal}");
}

// (b) without the table there is no summarization request at all.
#[tokio::test]
async fn an_environment_without_the_table_is_passthrough() {
    let workspace = tempdir().unwrap();
    let environments = tempdir().unwrap();
    write_environment(
        environments.path(),
        "plain",
        "fake",
        "fake-model",
        &[],
        "test",
    );

    let provider = ScriptedProvider::new(vec![
        text_response(&"a".repeat(2_000)),
        text_response(&"b".repeat(50)),
        text_response("done"),
    ]);
    let handle = provider.clone();
    let mut harness = Harness::new(
        vec![environments.path().to_path_buf()],
        &["one", "two", "three", "/exit"],
    );
    harness.deps.catalog_hook = Some(provider_hook(vec![("fake", provider)]));

    let code = run_args(
        &mut harness,
        &[
            "--env",
            "plain",
            "--workspace",
            workspace.path().to_str().unwrap(),
        ],
    )
    .await;

    assert_eq!(code, 0, "stderr: {}", harness.stderr.text());
    assert!(!harness.stderr.text().contains("context: summarized"));
    assert_eq!(handle.requests().len(), 3);
    assert!(
        handle
            .requests()
            .iter()
            .all(|request| !is_summary_request(request)),
        "a passthrough environment must make no summarization request"
    );
}

// (c) `summarize.md` overrides the compiled-in prompt.
#[tokio::test]
async fn summarize_md_overrides_the_summarizer_prompt() {
    let workspace = tempdir().unwrap();
    let environments = tempdir().unwrap();
    write_context_environment(environments.path(), "small", CONTEXT_TABLE);
    std::fs::write(
        environments.path().join("small").join("summarize.md"),
        "CUSTOM PROMPT",
    )
    .unwrap();

    let provider = ScriptedProvider::new(vec![
        text_response(&"a".repeat(2_000)),
        text_response(&"b".repeat(50)),
        text_response("SUMMARY"),
        text_response("done"),
    ]);
    let handle = provider.clone();
    let mut harness = Harness::new(
        vec![environments.path().to_path_buf()],
        &["one", "two", "three", "/exit"],
    );
    harness.deps.catalog_hook = Some(provider_hook(vec![("fake", provider)]));

    let code = run_args(
        &mut harness,
        &[
            "--env",
            "small",
            "--workspace",
            workspace.path().to_str().unwrap(),
        ],
    )
    .await;

    assert_eq!(code, 0, "stderr: {}", harness.stderr.text());
    let requests = handle.requests();
    let summary = requests
        .iter()
        .find(|request| request.system_prompt == "CUSTOM PROMPT")
        .expect("the override is used as the summarizer prompt");
    assert!(summary.tools.is_empty());
    assert!(
        requests
            .iter()
            .all(|request| request.system_prompt != p1_context::DEFAULT_SUMMARIZER_PROMPT),
        "the compiled-in prompt must not be used when summarize.md exists"
    );
}

// (d) an invalid table fails the run before any provider request, naming the file.
#[tokio::test]
async fn an_invalid_table_exits_before_any_request_and_names_the_file() {
    let workspace = tempdir().unwrap();
    let environments = tempdir().unwrap();
    let invalid = "[context]\nwindow_tokens = 100\noutput_headroom_tokens = 20\nsummarize_at_tokens = 80\nkeep_recent_tokens = 10\nuser_verbatim_tokens = 10\n";
    write_context_environment(environments.path(), "bad", invalid);

    let provider = ScriptedProvider::new(vec![]);
    let handle = provider.clone();
    let mut harness = Harness::new(vec![environments.path().to_path_buf()], &[]);
    harness.deps.catalog_hook = Some(provider_hook(vec![("fake", provider)]));

    let code = run_args(
        &mut harness,
        &[
            "--env",
            "bad",
            "--workspace",
            workspace.path().to_str().unwrap(),
            "hi",
        ],
    )
    .await;

    assert_eq!(code, 1);
    assert!(
        harness.stderr.text().contains("environment.toml"),
        "stderr: {}",
        harness.stderr.text()
    );
    assert!(handle.requests().is_empty(), "no request may be made");
}

// (e) `env show` prints the resolved table.
#[tokio::test]
async fn env_show_prints_the_context_table() {
    let environments = tempdir().unwrap();
    write_context_environment(environments.path(), "small", CONTEXT_TABLE);

    let provider = ScriptedProvider::new(vec![]);
    let mut harness = Harness::new(vec![environments.path().to_path_buf()], &[]);
    harness.deps.catalog_hook = Some(provider_hook(vec![("fake", provider)]));

    let code = run_args(&mut harness, &["env", "show", "small"]).await;

    assert_eq!(code, 0, "stderr: {}", harness.stderr.text());
    let stdout = harness.stdout.text();
    assert!(stdout.contains("\"context\""), "{stdout}");
    assert!(stdout.contains("\"summarize_at_tokens\": 580"), "{stdout}");
    assert!(stdout.contains("\"window_tokens\": 100000"), "{stdout}");
}
