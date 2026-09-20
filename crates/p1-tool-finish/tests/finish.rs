//! Rule-by-rule acceptance for the `finish` tool (spec `docs/design/completion.md` §2),
//! with the EXACT model-visible texts. Every test drives the tool through a fake
//! [`SessionActivity`]; no host, no files, no provider.

use std::sync::{Arc, Mutex};

use p1_contracts::{
    CancellationToken, Effect, Tool, ToolCall, ToolContext, ToolInput, ToolOutcome, ToolStatus,
};
use p1_tool_finish::{Accepted, FinishOutcome, FinishTool, SessionActivity, ShellRun, ToolFace};

const MISSING_VERIFICATION: &str = "Name the commands you ran to verify the work in \"verification\". If nothing can be verified by a command, say why in \"summary\" and pass [\"none\"].";
const NONE_CHANGED_FILES: &str =
    "This session changed files; verify the result with a command before finishing.";
const NEEDS: &str = "Say what you need in \"needs\".";
const CALL_SHAPE: &str =
    "Call finish again with \"verification\": [\"<one of the commands below>\"].";
const TRAILER_HEADING: &str =
    "Runs that count right now (successful, not piped, after the last file change):";
const TRAILER_NONE: &str =
    "No run counts right now: run your checks (without a pipe) after your last file change.";

/// The trailer as it appears when at least one run counts, newest last.
fn trailer(commands: &[&str]) -> String {
    let mut text = TRAILER_HEADING.to_string();
    for command in commands {
        text.push_str(&format!("\n- {command}"));
    }
    text
}

fn no_successful_run(named: &str) -> String {
    format!(
        "No successful run of `{named}` is recorded in this session. Run it, read the result, then finish."
    )
}

#[derive(Default)]
struct FakeActivity {
    last_file_change: Mutex<Option<u64>>,
    runs: Mutex<Vec<ShellRun>>,
}

impl FakeActivity {
    fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    fn changed_at(&self, order: u64) {
        *self.last_file_change.lock().unwrap() = Some(order);
    }

    fn ran(&self, command: &str, exit_code: Option<i32>, order: u64) {
        self.runs.lock().unwrap().push(ShellRun {
            command: command.to_string(),
            exit_code,
            order,
        });
    }
}

impl SessionActivity for FakeActivity {
    fn last_file_change(&self) -> Option<u64> {
        *self.last_file_change.lock().unwrap()
    }

    fn shell_runs(&self) -> Vec<ShellRun> {
        self.runs.lock().unwrap().clone()
    }
}

fn tool(activity: Arc<FakeActivity>) -> (FinishTool, FinishOutcome) {
    let outcome = FinishOutcome::default();
    (FinishTool::new(activity, outcome.clone()), outcome)
}

fn call(json: &str) -> ToolCall {
    ToolCall {
        call_id: "call-1".into(),
        name: "finish".into(),
        input: ToolInput::Json(json.to_string()),
    }
}

async fn execute(tool: &FinishTool, json: &str) -> ToolOutcome {
    let call = call(json);
    let context = ToolContext {
        cancel: CancellationToken::new(),
    };
    tool.execute(&call, context).await
}

async fn done(tool: &FinishTool, verification: &str) -> ToolOutcome {
    execute(
        tool,
        &format!(r#"{{"status":"done","summary":"s","verification":{verification}}}"#),
    )
    .await
}

async fn blocked(tool: &FinishTool, body: &str) -> ToolOutcome {
    execute(
        tool,
        &format!(r#"{{"status":"blocked","summary":"s",{body}}}"#),
    )
    .await
}

#[test]
fn declaration_effect_and_identity() {
    let activity = FakeActivity::new();
    let (finish, _) = tool(activity);
    assert_eq!(finish.declaration().name, "finish");
    assert_eq!(finish.identity().implementation, "p1-tool-finish");
    assert_eq!(finish.identity().variant, "claude");
    assert_eq!(finish.effect(&call("{}")), Effect::ReadOnly);

    let reshaped = finish.with_face(ToolFace::new("EndTask", "custom"), "gpt");
    assert_eq!(reshaped.declaration().name, "EndTask");
    assert_eq!(reshaped.declaration().description, "custom");
    assert_eq!(reshaped.identity().implementation, "p1-tool-finish");
    assert_eq!(reshaped.identity().variant, "gpt");
}

// --------------------------------------------------------- rule 1: verification

#[tokio::test]
async fn done_with_missing_verification_is_rejected() {
    let activity = FakeActivity::new();
    let (finish, outcome) = tool(activity);

    let result = execute(&finish, r#"{"status":"done","summary":"s"}"#).await;

    assert_eq!(result.status, ToolStatus::Error);
    assert_eq!(
        result.content,
        format!("{MISSING_VERIFICATION}\n\n{CALL_SHAPE}\n\n{TRAILER_NONE}")
    );
    assert_eq!(outcome.get(), None);
}

#[tokio::test]
async fn done_with_empty_verification_is_rejected() {
    let activity = FakeActivity::new();
    let (finish, _) = tool(activity);

    let result = done(&finish, "[]").await;

    assert_eq!(result.status, ToolStatus::Error);
    assert_eq!(
        result.content,
        format!("{MISSING_VERIFICATION}\n\n{CALL_SHAPE}\n\n{TRAILER_NONE}")
    );
}

#[tokio::test]
async fn none_is_accepted_only_without_a_file_change() {
    let activity = FakeActivity::new();
    let (finish, outcome) = tool(activity.clone());

    let accepted = done(&finish, r#"["none"]"#).await;
    assert_eq!(accepted.status, ToolStatus::Ok);
    assert_eq!(accepted.content, "Finished.");
    assert_eq!(
        outcome.get(),
        Some(Accepted::Done {
            summary: "s".to_string()
        })
    );

    activity.changed_at(3);
    let rejected = done(&finish, r#"["none"]"#).await;
    assert_eq!(rejected.status, ToolStatus::Error);
    assert_eq!(
        rejected.content,
        format!("{NONE_CHANGED_FILES}\n\n{TRAILER_NONE}")
    );
}

// --------------------------------------------------------- rule 2: the recorded run

#[tokio::test]
async fn a_command_that_never_ran_is_rejected() {
    let activity = FakeActivity::new();
    activity.ran("echo ok", Some(0), 1);
    let (finish, outcome) = tool(activity);

    let result = done(&finish, r#"["cargo test"]"#).await;

    assert_eq!(result.status, ToolStatus::Error);
    assert_eq!(
        result.content,
        format!(
            "{}\n\n{}",
            no_successful_run("cargo test"),
            trailer(&["echo ok"])
        )
    );
    assert_eq!(outcome.get(), None);
}

#[tokio::test]
async fn a_nonzero_run_is_rejected() {
    let activity = FakeActivity::new();
    activity.ran("false", Some(1), 1);
    let (finish, outcome) = tool(activity);

    let result = done(&finish, r#"["false"]"#).await;

    assert_eq!(result.status, ToolStatus::Error);
    assert_eq!(
        result.content,
        format!("{}\n\n{TRAILER_NONE}", no_successful_run("false"))
    );
    assert_eq!(outcome.get(), None);
}

#[tokio::test]
async fn a_missing_exit_code_never_counts_as_success() {
    let activity = FakeActivity::new();
    activity.ran("sleep 100", None, 1);
    let (finish, _) = tool(activity);

    let result = done(&finish, r#"["sleep 100"]"#).await;

    assert_eq!(result.status, ToolStatus::Error);
    assert!(
        result
            .content
            .starts_with("No successful run of `sleep 100`")
    );
}

#[tokio::test]
async fn a_successful_run_is_accepted_and_commands_are_trimmed() {
    let activity = FakeActivity::new();
    activity.ran("cargo test", Some(0), 1);
    let (finish, outcome) = tool(activity);

    let result = done(&finish, r#"["  cargo test  "]"#).await;

    assert_eq!(result.status, ToolStatus::Ok);
    assert_eq!(result.content, "Finished.");
    assert_eq!(
        outcome.get(),
        Some(Accepted::Done {
            summary: "s".to_string()
        })
    );
}

#[tokio::test]
async fn a_later_failing_rerun_invalidates_an_earlier_success() {
    let activity = FakeActivity::new();
    activity.ran("cat marker", Some(0), 1);
    activity.ran("cat marker", Some(1), 2);
    let (finish, _) = tool(activity);

    let result = done(&finish, r#"["cat marker"]"#).await;

    assert_eq!(result.status, ToolStatus::Error);
    assert_eq!(
        result.content,
        format!("{}\n\n{TRAILER_NONE}", no_successful_run("cat marker"))
    );
}

// --------------------------------------------------------- rule 3: order vs writes

#[tokio::test]
async fn a_run_before_the_last_file_change_is_rejected() {
    let activity = FakeActivity::new();
    activity.ran("cargo test", Some(0), 1);
    activity.changed_at(2);
    let (finish, _) = tool(activity);

    let result = done(&finish, r#"["cargo test"]"#).await;

    assert_eq!(result.status, ToolStatus::Error);
    assert_eq!(
        result.content,
        format!(
            "You changed files after running `cargo test`. Run it again, then finish.\n\n{TRAILER_NONE}"
        )
    );
}

#[tokio::test]
async fn a_run_after_the_last_file_change_is_accepted() {
    let activity = FakeActivity::new();
    activity.changed_at(1);
    activity.ran("cargo test", Some(0), 2);
    let (finish, _) = tool(activity);

    let result = done(&finish, r#"["cargo test"]"#).await;

    assert_eq!(result.content, "Finished.");
}

// ------------------------------------- revision: normalised matching + pipes

#[tokio::test]
async fn normalised_matching_ignores_a_leading_cd_and_whitespace() {
    // Named without `cd`, recorded with it.
    let activity = FakeActivity::new();
    activity.ran("cd /w/x && cargo fmt --check", Some(0), 1);
    let (finish, _) = tool(activity);
    assert_eq!(
        done(&finish, r#"["cargo fmt --check"]"#).await.content,
        "Finished."
    );

    // The reverse: named with `cd`, recorded without it.
    let activity = FakeActivity::new();
    activity.ran("cargo fmt --check", Some(0), 1);
    let (finish, _) = tool(activity);
    assert_eq!(
        done(&finish, r#"["cd /w/x && cargo fmt --check"]"#)
            .await
            .content,
        "Finished."
    );

    // Extra inner whitespace on either side.
    let activity = FakeActivity::new();
    activity.ran("cargo   fmt  --check", Some(0), 1);
    let (finish, _) = tool(activity);
    assert_eq!(
        done(&finish, r#"["  cargo fmt --check  "]"#).await.content,
        "Finished."
    );
}

#[tokio::test]
async fn only_one_leading_cd_is_dropped() {
    // The recorded form keeps its second `cd`, so the bare tail does not match...
    let activity = FakeActivity::new();
    activity.ran("cd a && cd b && x", Some(0), 1);
    let (finish, _) = tool(activity);
    let rejected = done(&finish, r#"["x"]"#).await;
    assert_eq!(rejected.status, ToolStatus::Error);
    assert_eq!(
        rejected.content,
        format!("{}\n\n{}", no_successful_run("x"), trailer(&["cd b && x"]))
    );

    // ...but the command named as it was run does.
    let activity = FakeActivity::new();
    activity.ran("cd a && cd b && x", Some(0), 1);
    let (finish, _) = tool(activity);
    assert_eq!(
        done(&finish, r#"["cd a && cd b && x"]"#).await.content,
        "Finished."
    );
}

#[tokio::test]
async fn a_piped_run_is_rejected_with_the_pipe_error() {
    let activity = FakeActivity::new();
    activity.ran("cargo test 2>&1 | tail -5", Some(0), 1);
    let (finish, outcome) = tool(activity);

    let result = done(&finish, r#"["cargo test 2>&1 | tail -5"]"#).await;

    assert_eq!(result.status, ToolStatus::Error);
    assert_eq!(
        result.content,
        format!(
            "`cargo test 2>&1 | tail -5` was run through a pipe, so its exit code says nothing about it. Run it without a pipe, then finish.\n\n{TRAILER_NONE}"
        )
    );
    assert_eq!(outcome.get(), None);
}

#[tokio::test]
async fn an_unquoted_pipe_is_detected() {
    let activity = FakeActivity::new();
    activity.ran("grep x f | wc -l", Some(0), 1);
    let (finish, _) = tool(activity);

    let result = done(&finish, r#"["grep x f | wc -l"]"#).await;

    assert!(
        result
            .content
            .starts_with("`grep x f | wc -l` was run through a pipe"),
        "{}",
        result.content
    );
}

#[tokio::test]
async fn a_double_pipe_is_not_a_pipe() {
    let activity = FakeActivity::new();
    activity.ran("a || b", Some(0), 1);
    let (finish, _) = tool(activity);

    assert_eq!(done(&finish, r#"["a || b"]"#).await.content, "Finished.");
}

#[tokio::test]
async fn quoted_pipes_are_not_pipes() {
    let activity = FakeActivity::new();
    activity.ran("echo 'a|b'", Some(0), 1);
    activity.ran("echo \"a|b\"", Some(0), 2);
    let (finish, _) = tool(activity);

    assert_eq!(
        done(&finish, r#"["echo 'a|b'"]"#).await.content,
        "Finished."
    );
    assert_eq!(
        done(&finish, r#"["echo \"a|b\""]"#).await.content,
        "Finished."
    );
}

#[tokio::test]
async fn the_trailer_lists_at_most_five_runs_newest_last() {
    let activity = FakeActivity::new();
    for (index, command) in ["c1", "c2", "c3", "c4", "c5", "c6"].iter().enumerate() {
        activity.ran(command, Some(0), index as u64 + 1);
    }
    let (finish, _) = tool(activity);

    let result = execute(&finish, r#"{"status":"done","summary":"s"}"#).await;

    assert_eq!(result.status, ToolStatus::Error);
    assert_eq!(
        result.content,
        format!(
            "{MISSING_VERIFICATION}\n\n{CALL_SHAPE}\n\n{}",
            trailer(&["c2", "c3", "c4", "c5", "c6"])
        )
    );
}

#[tokio::test]
async fn the_trailer_excludes_failed_piped_and_stale_runs() {
    let activity = FakeActivity::new();
    activity.changed_at(2);
    activity.ran("stale", Some(0), 1);
    activity.ran("fresh", Some(0), 3);
    activity.ran("failed", Some(1), 4);
    activity.ran("piped | tail", Some(0), 5);
    let (finish, _) = tool(activity);

    let result = execute(&finish, r#"{"status":"done","summary":"s"}"#).await;

    assert_eq!(
        result.content,
        format!(
            "{MISSING_VERIFICATION}\n\n{CALL_SHAPE}\n\n{}",
            trailer(&["fresh"])
        )
    );
}

#[tokio::test]
async fn no_run_counts_right_now_when_nothing_qualifies() {
    let activity = FakeActivity::new();
    activity.ran("false", Some(1), 1);
    let (finish, _) = tool(activity);

    let result = execute(&finish, r#"{"status":"done","summary":"s"}"#).await;

    assert_eq!(
        result.content,
        format!("{MISSING_VERIFICATION}\n\n{CALL_SHAPE}\n\n{TRAILER_NONE}")
    );
}

#[tokio::test]
async fn the_trailer_excludes_a_piped_run_but_keeps_an_earlier_unpiped_one() {
    let activity = FakeActivity::new();
    activity.ran("cargo test", Some(0), 1);
    activity.ran("cargo test 2>&1 | tail -5", Some(0), 2);
    let (finish, _) = tool(activity);

    let result = done(&finish, r#"["cargo test 2>&1 | tail -5"]"#).await;

    assert_eq!(result.status, ToolStatus::Error);
    assert_eq!(
        result.content,
        format!(
            "`cargo test 2>&1 | tail -5` was run through a pipe, so its exit code says nothing about it. Run it without a pipe, then finish.\n\n{}",
            trailer(&["cargo test"])
        )
    );
}

// --------------------------------------------------------- rule 4/5: blocked + outcome

#[tokio::test]
async fn blocked_requires_needs() {
    let activity = FakeActivity::new();
    let (finish, outcome) = tool(activity);

    let missing = execute(&finish, r#"{"status":"blocked","summary":"s"}"#).await;
    assert_eq!(missing.status, ToolStatus::Error);
    assert_eq!(missing.content, NEEDS);

    let empty = blocked(&finish, r#""needs":"   ""#).await;
    assert_eq!(empty.status, ToolStatus::Error);
    assert_eq!(empty.content, NEEDS);
    assert_eq!(outcome.get(), None);
}

#[tokio::test]
async fn blocked_is_accepted_and_stores_the_full_record() {
    let activity = FakeActivity::new();
    let (finish, outcome) = tool(activity);

    let result = blocked(
        &finish,
        r#""needs":"an API token","tried":["looked in the environment"]"#,
    )
    .await;

    assert_eq!(result.status, ToolStatus::Ok);
    assert_eq!(result.content, "Recorded as blocked.");
    assert_eq!(
        outcome.get(),
        Some(Accepted::Blocked {
            summary: "s".to_string(),
            needs: "an API token".to_string(),
            tried: vec!["looked in the environment".to_string()],
        })
    );
}

#[tokio::test]
async fn a_rejected_call_stores_nothing_and_the_last_accepted_call_wins() {
    let activity = FakeActivity::new();
    let (finish, outcome) = tool(activity.clone());

    // Rejected: no run recorded.
    let rejected = done(&finish, r#"["true"]"#).await;
    assert_eq!(rejected.status, ToolStatus::Error);
    assert_eq!(outcome.get(), None);

    activity.ran("true", Some(0), 1);
    let first = done(&finish, r#"["true"]"#).await;
    assert_eq!(first.status, ToolStatus::Ok);

    // A later accepted blocked call replaces the done outcome.
    let second = blocked(&finish, r#""needs":"a token""#).await;
    assert_eq!(second.status, ToolStatus::Ok);
    assert_eq!(
        outcome.get(),
        Some(Accepted::Blocked {
            summary: "s".to_string(),
            needs: "a token".to_string(),
            tried: Vec::new(),
        })
    );

    outcome.clear();
    assert_eq!(outcome.get(), None);
}

// --------------------------------------------------------- common input rules

#[tokio::test]
async fn invalid_input_stays_an_invalid_input_error() {
    let activity = FakeActivity::new();
    let (finish, _) = tool(activity);
    let garbage = [
        "",
        "null",
        "[]",
        r#"{"status":"maybe","summary":"s"}"#,
        r#"{"status":"done"}"#,
        r#"{"status":"done","summary":"s","unknown":1}"#,
        r#"{"status":"done","summary":5}"#,
    ];
    for json in garbage {
        let result = execute(&finish, json).await;
        assert_eq!(result.status, ToolStatus::Error, "input: {json:?}");
        assert!(
            result.content.starts_with("Invalid input for finish: "),
            "input: {json:?} -> {result:?}"
        );
    }
}

#[tokio::test]
async fn text_input_is_invalid_input() {
    let activity = FakeActivity::new();
    let (finish, _) = tool(activity);
    let call = ToolCall {
        call_id: "c".into(),
        name: "finish".into(),
        input: ToolInput::Text("status=done".into()),
    };
    let context = ToolContext {
        cancel: CancellationToken::new(),
    };

    let result = finish.execute(&call, context).await;

    assert_eq!(result.status, ToolStatus::Error);
    assert!(result.content.starts_with("Invalid input for finish: "));
}
