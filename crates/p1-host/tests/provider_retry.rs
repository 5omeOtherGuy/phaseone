//! Must-pass tests for §3b of `docs/design/completion.md`: a HEADLESS run that
//! ends a turn on a TRANSIENT provider failure must WAIT and then continue,
//! instead of giving up. End to end through the real host, with scripted
//! providers and the REAL `shell`/`finish` tools in a tempdir workspace.
//!
//! The wait is injected through `HostDeps::wait`, so every test here RECORDS the
//! durations the host asked for and asserts the schedule from that record. No
//! test sleeps.

mod common;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use common::{Harness, provider_hook, run_args, write_environment};
use p1_contracts::{Item, ProviderError, ProviderErrorKind, RecordBody};
use p1_testkit::{ScriptedProvider, Step, json_call, text_response, tool_call_response};
use tempfile::tempdir;

const DONE_TRUE: &str = r#"{"status":"done","summary":"wrote it","verification":["true"]}"#;

fn transport() -> Step {
    Step::SetupError(ProviderError::new(
        ProviderErrorKind::Transport,
        "connection reset by peer",
    ))
}

fn rate_limited() -> Step {
    Step::SetupError(ProviderError::new(
        ProviderErrorKind::RateLimited,
        "429 too many requests",
    ))
}

fn shell_call(id: &str, command: &str) -> p1_contracts::ToolCall {
    debug_assert!(!command.contains('"') && !command.contains('\\'));
    json_call(id, "shell", &format!(r#"{{"command":"{command}"}}"#))
}

/// The three requests of a turn that finishes the task: run a check, call
/// `finish` (its verification must count), then end with the model's final text.
fn finish_turn() -> Vec<Step> {
    vec![
        tool_call_response(vec![shell_call("s1", "true")]),
        tool_call_response(vec![json_call("f1", "finish", DONE_TRUE)]),
        text_response("done"),
    ]
}

fn finish_environment(root: &std::path::Path, tools: &[&str]) {
    write_environment(root, "finish-env", "fake", "fake-model", tools, "test");
}

fn plain_environment(root: &std::path::Path) {
    write_environment(root, "plain", "fake", "fake-model", &["read"], "test");
}

fn user_inputs(records: &[p1_contracts::JournalRecord]) -> Vec<String> {
    records
        .iter()
        .filter_map(|record| match &record.body {
            RecordBody::UserInput { text } => Some(text.clone()),
            _ => None,
        })
        .collect()
}

/// `HostDeps::wait` that RECORDS the requested duration and returns an
/// immediately-ready future.
#[derive(Clone, Default)]
struct WaitLog {
    asked: Arc<Mutex<Vec<Duration>>>,
}

impl WaitLog {
    fn hook(&self) -> p1_host::WaitFn {
        let asked = self.asked.clone();
        Arc::new(move |duration| {
            asked.lock().unwrap().push(duration);
            Box::pin(std::future::ready(()))
        })
    }

    /// The durations the host asked to wait, in order.
    fn asked(&self) -> Vec<Duration> {
        self.asked.lock().unwrap().clone()
    }
}

/// `HostDeps::wait` that RECORDS the duration and then stays pending until the
/// test releases it, so a cancel can be fired DURING a wait.
#[derive(Clone, Default)]
struct GatedWait {
    asked: Arc<Mutex<Vec<Duration>>>,
    gate: Arc<tokio::sync::Notify>,
}

impl GatedWait {
    fn hook(&self) -> p1_host::WaitFn {
        let asked = self.asked.clone();
        let gate = self.gate.clone();
        Arc::new(move |duration| {
            asked.lock().unwrap().push(duration);
            let gate = gate.clone();
            Box::pin(async move { gate.notified().await })
        })
    }

    fn asked(&self) -> Vec<Duration> {
        self.asked.lock().unwrap().clone()
    }
}

fn secs(durations: &[Duration]) -> Vec<u64> {
    durations.iter().map(|d| d.as_secs()).collect()
}

/// The `! provider failed: …` lines the renderer prints for a turn end.
fn failure_lines(stderr: &str) -> Vec<String> {
    stderr
        .lines()
        .filter(|line| line.contains("! provider failed"))
        .map(|line| line.to_string())
        .collect()
}

/// The retry lines the renderer prints, in order.
fn retry_lines(stderr: &str) -> Vec<String> {
    stderr
        .lines()
        .filter(|line| line.contains("provider failed (") && line.contains("retry "))
        .map(|line| line.to_string())
        .collect()
}

fn retry_environment(environments: &std::path::Path) -> Harness {
    finish_environment(environments, &["shell", "finish"]);
    Harness::new(vec![environments.to_path_buf()], &[])
}

#[test]
fn the_retry_message_is_exactly_the_spec_text() {
    // §3b: "one fixed user-role message (PROVIDER_RETRY_MESSAGE: the connection to
    // the model failed, the last response was lost, nothing else changed, continue)".
    assert_eq!(
        p1_host::run::PROVIDER_RETRY_MESSAGE,
        "The connection to the model failed and the last response was lost; nothing else \
         changed. Continue the work now."
    );
}

// ------------------------------------------------------------------ schedules

#[tokio::test]
async fn a_transport_failure_waits_then_retries_the_same_turn() {
    let workspace = tempdir().unwrap();
    let environments = tempdir().unwrap();
    let session = workspace.path().join("session.jsonl");
    let mut harness = retry_environment(environments.path());
    let wait = WaitLog::default();
    harness.deps.wait = wait.hook();
    let mut script = vec![transport(), transport(), transport()];
    script.extend(finish_turn());
    let provider = ScriptedProvider::new(script);
    harness.deps.catalog_hook = Some(provider_hook(vec![("fake", provider.clone())]));

    let code = run_args(
        &mut harness,
        &[
            "--yes",
            "--env",
            "finish-env",
            "--workspace",
            workspace.path().to_str().unwrap(),
            "--session",
            session.to_str().unwrap(),
            "go",
        ],
    )
    .await;

    assert_eq!(code, 0, "stderr: {}", harness.stderr.text());
    assert_eq!(
        secs(&wait.asked()),
        vec![5, 30, 120],
        "§3b: Transport waits 5 s, 30 s, 120 s"
    );
    assert_eq!(
        provider.requests().len(),
        6,
        "three failures, then the three requests of the turn that finishes"
    );
    let loaded = p1_journal::load(&session).unwrap();
    assert_eq!(
        user_inputs(&loaded.records),
        vec![
            "go".to_string(),
            p1_host::run::PROVIDER_RETRY_MESSAGE.to_string(),
            p1_host::run::PROVIDER_RETRY_MESSAGE.to_string(),
            p1_host::run::PROVIDER_RETRY_MESSAGE.to_string(),
        ],
        "every retry is journalled as a normal user input"
    );
}

#[tokio::test]
async fn a_rate_limit_uses_the_phase_two_schedule() {
    let workspace = tempdir().unwrap();
    let environments = tempdir().unwrap();
    let mut harness = retry_environment(environments.path());
    let wait = WaitLog::default();
    harness.deps.wait = wait.hook();
    let mut script = vec![rate_limited(), rate_limited(), rate_limited()];
    script.extend(finish_turn());
    let provider = ScriptedProvider::new(script);
    harness.deps.catalog_hook = Some(provider_hook(vec![("fake", provider.clone())]));

    let code = run_args(
        &mut harness,
        &[
            "--yes",
            "--env",
            "finish-env",
            "--workspace",
            workspace.path().to_str().unwrap(),
            "go",
        ],
    )
    .await;

    assert_eq!(code, 0, "stderr: {}", harness.stderr.text());
    assert_eq!(
        secs(&wait.asked()),
        vec![60, 300, 900],
        "§3b: RateLimited waits 60 s, 300 s, 900 s"
    );
    assert_eq!(provider.requests().len(), 6);
}

#[tokio::test]
async fn the_fourth_consecutive_transport_failure_ends_the_run_by_default() {
    let workspace = tempdir().unwrap();
    let environments = tempdir().unwrap();
    let mut harness = retry_environment(environments.path());
    let wait = WaitLog::default();
    harness.deps.wait = wait.hook();
    let provider = ScriptedProvider::new(vec![transport(), transport(), transport(), transport()]);
    harness.deps.catalog_hook = Some(provider_hook(vec![("fake", provider.clone())]));

    let code = run_args(
        &mut harness,
        &[
            "--yes",
            "--env",
            "finish-env",
            "--workspace",
            workspace.path().to_str().unwrap(),
            "go",
        ],
    )
    .await;

    assert_eq!(
        code,
        p1_host::run::EXIT_FAILURE,
        "the fourth consecutive transient end ends the run as it does today"
    );
    assert_eq!(
        provider.requests().len(),
        4,
        "three retries with the default N=3, then one more attempt"
    );
    assert_eq!(secs(&wait.asked()), vec![5, 30, 120]);
    assert_eq!(
        failure_lines(&harness.stderr.text()).len(),
        4,
        "stderr: {}",
        harness.stderr.text()
    );
}

#[tokio::test]
async fn a_budget_longer_than_the_schedule_repeats_its_last_wait() {
    let workspace = tempdir().unwrap();
    let environments = tempdir().unwrap();
    let mut harness = retry_environment(environments.path());
    let wait = WaitLog::default();
    harness.deps.wait = wait.hook();
    let provider = ScriptedProvider::new(vec![
        transport(),
        transport(),
        transport(),
        transport(),
        transport(),
        transport(),
    ]);
    harness.deps.catalog_hook = Some(provider_hook(vec![("fake", provider.clone())]));

    let code = run_args(
        &mut harness,
        &[
            "--yes",
            "--provider-retries",
            "5",
            "--env",
            "finish-env",
            "--workspace",
            workspace.path().to_str().unwrap(),
            "go",
        ],
    )
    .await;

    assert_eq!(code, p1_host::run::EXIT_FAILURE);
    assert_eq!(
        provider.requests().len(),
        6,
        "five retries then the sixth, last attempt"
    );
    assert_eq!(
        secs(&wait.asked()),
        vec![5, 30, 120, 120, 120],
        "a budget longer than the three-entry schedule repeats its last entry"
    );
}

// ------------------------------------------------------------------ the budget

#[tokio::test]
async fn provider_retries_zero_disables_retrying() {
    let workspace = tempdir().unwrap();
    let environments = tempdir().unwrap();
    let mut harness = retry_environment(environments.path());
    let wait = WaitLog::default();
    harness.deps.wait = wait.hook();
    let provider =
        ScriptedProvider::new(vec![transport(), text_response("must never be requested")]);
    harness.deps.catalog_hook = Some(provider_hook(vec![("fake", provider.clone())]));

    let code = run_args(
        &mut harness,
        &[
            "--yes",
            "--provider-retries",
            "0",
            "--env",
            "finish-env",
            "--workspace",
            workspace.path().to_str().unwrap(),
            "go",
        ],
    )
    .await;

    assert_eq!(code, p1_host::run::EXIT_FAILURE);
    assert_eq!(provider.requests().len(), 1);
    assert!(wait.asked().is_empty(), "0 disables the wait entirely");
    assert!(retry_lines(&harness.stderr.text()).is_empty());
}

#[tokio::test]
async fn provider_retries_bounds_the_consecutive_failures() {
    let workspace = tempdir().unwrap();
    let environments = tempdir().unwrap();
    let mut harness = retry_environment(environments.path());
    let wait = WaitLog::default();
    harness.deps.wait = wait.hook();
    let provider = ScriptedProvider::new(vec![transport(), transport(), transport()]);
    harness.deps.catalog_hook = Some(provider_hook(vec![("fake", provider.clone())]));

    let code = run_args(
        &mut harness,
        &[
            "--yes",
            "--provider-retries",
            "2",
            "--env",
            "finish-env",
            "--workspace",
            workspace.path().to_str().unwrap(),
            "go",
        ],
    )
    .await;

    assert_eq!(code, p1_host::run::EXIT_FAILURE);
    assert_eq!(
        provider.requests().len(),
        3,
        "two retries then the third failure ends the run"
    );
    assert_eq!(secs(&wait.asked()), vec![5, 30]);
}

// ------------------------------------------------------------------ the reset

#[tokio::test]
async fn a_completed_response_in_the_turn_resets_the_consecutive_count() {
    let workspace = tempdir().unwrap();
    let environments = tempdir().unwrap();
    let mut harness = retry_environment(environments.path());
    let wait = WaitLog::default();
    harness.deps.wait = wait.hook();
    // The second turn completes a response (a shell tool call) and only then
    // fails transiently, so its failure is the FIRST of a new run of failures.
    let mut script = vec![
        transport(),
        tool_call_response(vec![shell_call("s1", "echo kept")]),
        transport(),
    ];
    script.extend(finish_turn());
    let provider = ScriptedProvider::new(script);
    harness.deps.catalog_hook = Some(provider_hook(vec![("fake", provider.clone())]));

    let code = run_args(
        &mut harness,
        &[
            "--yes",
            "--env",
            "finish-env",
            "--workspace",
            workspace.path().to_str().unwrap(),
            "go",
        ],
    )
    .await;

    assert_eq!(code, 0, "stderr: {}", harness.stderr.text());
    assert_eq!(
        secs(&wait.asked()),
        vec![5, 5],
        "a completed response in the turn resets the schedule to its first wait"
    );
    assert_eq!(provider.requests().len(), 6);
    assert_eq!(
        provider
            .requests()
            .last()
            .unwrap()
            .history
            .iter()
            .filter(|item| matches!(item, Item::ToolResult(result)
                if result.name == "shell" && result.content.contains("kept")))
            .count(),
        1,
        "the retried turn kept the tool result the interrupted turn had already produced"
    );
}

// --------------------------------------------------- everything else is as before

#[tokio::test]
async fn a_non_transient_failure_ends_the_run_as_today() {
    for kind in [
        ProviderErrorKind::Authentication,
        ProviderErrorKind::InvalidRequest,
        ProviderErrorKind::ContextWindowExceeded,
    ] {
        let workspace = tempdir().unwrap();
        let environments = tempdir().unwrap();
        let mut harness = retry_environment(environments.path());
        let wait = WaitLog::default();
        harness.deps.wait = wait.hook();
        let provider = ScriptedProvider::new(vec![
            Step::SetupError(ProviderError::new(kind, "no retry can help")),
            text_response("must never be requested"),
        ]);
        harness.deps.catalog_hook = Some(provider_hook(vec![("fake", provider.clone())]));

        let code = run_args(
            &mut harness,
            &[
                "--yes",
                "--env",
                "finish-env",
                "--workspace",
                workspace.path().to_str().unwrap(),
                "go",
            ],
        )
        .await;

        assert_eq!(code, p1_host::run::EXIT_FAILURE, "{kind:?}");
        assert_eq!(provider.requests().len(), 1, "{kind:?}");
        assert!(wait.asked().is_empty(), "{kind:?}");
        assert!(
            retry_lines(&harness.stderr.text()).is_empty(),
            "{kind:?} stdout: {}",
            harness.stderr.text()
        );
        assert_eq!(failure_lines(&harness.stderr.text()).len(), 1, "{kind:?}");
    }
}

// ------------------------------------------------------------------ protocol

/// §3c amendment to §3b: a `Protocol` failure (a malformed response) is the
/// model's or the route's one-off and joins the transient kinds on the Transport
/// schedule. Previously this exact case ended the run; the assertion that
/// `Protocol` is never retried was removed from
/// `a_non_transient_failure_ends_the_run_as_today`.
#[tokio::test]
async fn a_protocol_failure_waits_then_retries_on_the_transport_schedule() {
    let workspace = tempdir().unwrap();
    let environments = tempdir().unwrap();
    let mut harness = retry_environment(environments.path());
    let wait = WaitLog::default();
    harness.deps.wait = wait.hook();
    let mut script = vec![Step::SetupError(ProviderError::new(
        ProviderErrorKind::Protocol,
        "unsupported chat tool type",
    ))];
    script.extend(finish_turn());
    let provider = ScriptedProvider::new(script);
    harness.deps.catalog_hook = Some(provider_hook(vec![("fake", provider.clone())]));

    let code = run_args(
        &mut harness,
        &[
            "--yes",
            "--env",
            "finish-env",
            "--workspace",
            workspace.path().to_str().unwrap(),
            "go",
        ],
    )
    .await;

    assert_eq!(code, 0, "stderr: {}", harness.stderr.text());
    assert_eq!(
        secs(&wait.asked()),
        vec![5],
        "§3c: Protocol uses the Transport schedule"
    );
    assert_eq!(provider.requests().len(), 4);
    assert_eq!(
        retry_lines(&harness.stderr.text()),
        vec!["provider failed (Protocol): retry 1/3 in 5 s".to_string()],
        "stderr: {}",
        harness.stderr.text()
    );
}

#[tokio::test]
async fn an_interactive_run_does_not_wait_or_retry() {
    let workspace = tempdir().unwrap();
    let environments = tempdir().unwrap();
    plain_environment(environments.path());
    let mut harness = Harness::new(vec![environments.path().to_path_buf()], &["go"]);
    let wait = WaitLog::default();
    harness.deps.wait = wait.hook();
    let provider =
        ScriptedProvider::new(vec![transport(), text_response("must never be requested")]);
    harness.deps.catalog_hook = Some(provider_hook(vec![("fake", provider.clone())]));

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

    assert_eq!(code, 0);
    assert_eq!(provider.requests().len(), 1);
    assert!(wait.asked().is_empty());
}

#[tokio::test]
async fn a_headless_run_without_finish_also_waits_out_a_transient_failure() {
    let workspace = tempdir().unwrap();
    let environments = tempdir().unwrap();
    plain_environment(environments.path());
    let session = workspace.path().join("session.jsonl");
    let mut harness = Harness::new(vec![environments.path().to_path_buf()], &[]);
    let wait = WaitLog::default();
    harness.deps.wait = wait.hook();
    let provider = ScriptedProvider::new(vec![transport(), text_response("plain answer")]);
    harness.deps.catalog_hook = Some(provider_hook(vec![("fake", provider.clone())]));

    let code = run_args(
        &mut harness,
        &[
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
    assert_eq!(provider.requests().len(), 2);
    assert_eq!(secs(&wait.asked()), vec![5]);
    let loaded = p1_journal::load(&session).unwrap();
    assert_eq!(
        user_inputs(&loaded.records),
        vec![
            "go".to_string(),
            p1_host::run::PROVIDER_RETRY_MESSAGE.to_string()
        ],
        "one failure, one retry message — and nothing else added to history"
    );
}

// ------------------------------------------------------------------ rendering

#[tokio::test]
async fn each_wait_prints_one_line_naming_the_kind_and_the_delay() {
    let workspace = tempdir().unwrap();
    let environments = tempdir().unwrap();
    let mut harness = retry_environment(environments.path());
    let wait = WaitLog::default();
    harness.deps.wait = wait.hook();
    let mut script = vec![rate_limited(), rate_limited(), rate_limited()];
    script.extend(finish_turn());
    let provider = ScriptedProvider::new(script);
    harness.deps.catalog_hook = Some(provider_hook(vec![("fake", provider.clone())]));

    let code = run_args(
        &mut harness,
        &[
            "--yes",
            "--env",
            "finish-env",
            "--workspace",
            workspace.path().to_str().unwrap(),
            "go",
        ],
    )
    .await;

    assert_eq!(code, 0, "stderr: {}", harness.stderr.text());
    assert_eq!(
        retry_lines(&harness.stderr.text()),
        vec![
            "provider failed (RateLimited): retry 1/3 in 60 s".to_string(),
            "provider failed (RateLimited): retry 2/3 in 300 s".to_string(),
            "provider failed (RateLimited): retry 3/3 in 900 s".to_string(),
        ],
        "stderr: {}",
        harness.stderr.text()
    );
}

// ------------------------------------------------------------------ cancelling

#[tokio::test]
async fn cancelling_during_the_wait_ends_cancelled_without_another_request() {
    let workspace = tempdir().unwrap();
    let environments = tempdir().unwrap();
    let mut harness = retry_environment(environments.path());
    let wait = GatedWait::default();
    harness.deps.wait = wait.hook();
    let provider =
        ScriptedProvider::new(vec![transport(), text_response("must never be requested")]);
    harness.deps.catalog_hook = Some(provider_hook(vec![("fake", provider.clone())]));

    let interrupt = harness.interrupt.clone();
    let watched = wait.clone();
    tokio::spawn(async move {
        while watched.asked().is_empty() {
            tokio::task::yield_now().await;
        }
        interrupt.fire();
    });

    let code = run_args(
        &mut harness,
        &[
            "--yes",
            "--env",
            "finish-env",
            "--workspace",
            workspace.path().to_str().unwrap(),
            "go",
        ],
    )
    .await;

    assert_eq!(code, p1_host::run::EXIT_CANCELLED);
    assert_eq!(
        secs(&wait.asked()),
        vec![5],
        "the cancel arrived during the wait, so only the first wait was asked for"
    );
    assert_eq!(
        provider.requests().len(),
        1,
        "cancelling during the wait must not send another request"
    );
    assert_eq!(
        retry_lines(&harness.stderr.text()),
        vec!["provider failed (Transport): retry 1/3 in 5 s".to_string()],
        "stderr: {}",
        harness.stderr.text()
    );
}
