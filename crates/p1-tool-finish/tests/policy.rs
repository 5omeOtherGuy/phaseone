//! ADR-0051 item 1: the completion policy is the host's, and the accepted `done`
//! carries host-owned `Evidence` (item 2).
//!
//! The strict policy is ADR-0037's rule and must not move; `ReportToParent` is for an
//! agent whose assembled tools include no tool that runs commands, so `["none"]` is
//! accepted even after a file change — and labelled, never called verified. Every
//! other check is shared: a command that never ran is rejected with the same text
//! under both policies.

use std::sync::{Arc, Mutex};

use p1_contracts::{CancellationToken, Tool, ToolCall, ToolContext, ToolInput, ToolStatus};
use p1_tool_finish::{
    Accepted, CompletionPolicy, Evidence, FinishOutcome, FinishTool, SessionActivity, ShellRun,
    ToolFace,
};

const NONE_CHANGED_FILES: &str =
    "This session changed files; verify the result with a command before finishing.";

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

/// The tool under `policy`, plus the outcome cell it writes.
fn tool(activity: Arc<FakeActivity>, policy: CompletionPolicy) -> (FinishTool, FinishOutcome) {
    let outcome = FinishOutcome::default();
    let tool = FinishTool::new(activity, outcome.clone()).with_policy(policy);
    (tool, outcome)
}

async fn execute(tool: &FinishTool, json: &str) -> p1_contracts::ToolOutcome {
    let call = ToolCall {
        call_id: "call-1".into(),
        name: "finish".into(),
        input: ToolInput::Json(json.to_string()),
    };
    let context = ToolContext {
        cancel: CancellationToken::new(),
    };
    tool.execute(&call, context).await
}

async fn done(tool: &FinishTool, verification: &str) -> p1_contracts::ToolOutcome {
    execute(
        tool,
        &format!(r#"{{"status":"done","summary":"s","verification":{verification}}}"#),
    )
    .await
}

/// (a) `ReportToParent` accepts `["none"]` after a file change and records that
/// nothing ran; the strict policy still rejects the same call unchanged.
#[tokio::test]
async fn report_to_parent_accepts_none_after_a_file_change() {
    let activity = FakeActivity::new();
    activity.changed_at(7);
    let (finish, outcome) = tool(activity.clone(), CompletionPolicy::ReportToParent);

    let accepted = done(&finish, r#"["none"]"#).await;
    assert_eq!(accepted.status, ToolStatus::Ok, "{}", accepted.content);
    assert_eq!(accepted.content, "Finished.");
    assert_eq!(
        outcome.get(),
        Some(Accepted::Done {
            summary: "s".to_string(),
            evidence: Evidence::NotRun("no command tool granted".to_string()),
        })
    );

    // The same session under the default policy: rejected, unchanged text.
    let (strict, strict_outcome) = tool(activity, CompletionPolicy::RecordedCommands);
    let rejected = done(&strict, r#"["none"]"#).await;
    assert_eq!(rejected.status, ToolStatus::Error);
    assert!(
        rejected.content.starts_with(NONE_CHANGED_FILES),
        "ADR-0037's text is unchanged: {}",
        rejected.content
    );
    assert_eq!(strict_outcome.get(), None);
}

/// (b) A fabricated command is rejected under `ReportToParent` too, with the same
/// text and the same trailer as the strict policy — invalid evidence never
/// downgrades to an accepted unverified result.
#[tokio::test]
async fn report_to_parent_rejects_a_command_that_never_ran() {
    let activity = FakeActivity::new();
    activity.changed_at(3);
    activity.ran("echo ok", Some(0), 1);
    let (finish, outcome) = tool(activity.clone(), CompletionPolicy::ReportToParent);

    let result = done(&finish, r#"["cargo test --lib"]"#).await;

    assert_eq!(result.status, ToolStatus::Error);
    assert!(
        result.content.starts_with(
            "No successful run of `cargo test --lib` is recorded in this session. Run it, read \
             the result, then finish."
        ),
        "the ADR-0037 text: {}",
        result.content
    );
    assert!(
        result.content.ends_with(
            "No run counts right now: run your checks (without a pipe) after your last file change."
        ),
        "the trailer is unchanged: {}",
        result.content
    );
    assert_eq!(outcome.get(), None);

    // The strict policy says exactly the same thing.
    let (strict, _) = tool(activity, CompletionPolicy::RecordedCommands);
    assert_eq!(
        done(&strict, r#"["cargo test --lib"]"#).await.content,
        result.content
    );
}

/// (c) A command the agent really ran and that succeeded after its last change is
/// accepted under `ReportToParent` and carried as evidence.
#[tokio::test]
async fn report_to_parent_still_accepts_a_recorded_command() {
    let activity = FakeActivity::new();
    activity.ran("cargo test -p x", Some(0), 1);
    activity.changed_at(2);
    activity.ran("  cargo   test -p x  ", Some(0), 3);
    let (finish, outcome) = tool(activity, CompletionPolicy::ReportToParent);

    let accepted = done(&finish, r#"["cargo test -p x"]"#).await;

    assert_eq!(accepted.status, ToolStatus::Ok, "{}", accepted.content);
    assert_eq!(accepted.content, "Finished.");
    assert_eq!(
        outcome.get(),
        Some(Accepted::Done {
            summary: "s".to_string(),
            evidence: Evidence::CommandsPassed(vec!["cargo test -p x".to_string()]),
        })
    );
}

/// (d) `blocked` without `needs` is rejected under both policies, and an empty
/// `verification` is still rule 1's error under `ReportToParent`.
#[tokio::test]
async fn blocked_without_needs_and_empty_verification_are_still_rejected() {
    let activity = FakeActivity::new();
    let (finish, outcome) = tool(activity, CompletionPolicy::ReportToParent);

    let blocked = execute(&finish, r#"{"status":"blocked","summary":"s"}"#).await;
    assert_eq!(blocked.status, ToolStatus::Error);
    assert_eq!(blocked.content, "Say what you need in \"needs\".");

    let empty = execute(&finish, r#"{"status":"done","summary":"s"}"#).await;
    assert_eq!(empty.status, ToolStatus::Error);
    assert!(
        empty.content.starts_with(
            "Name the commands you ran to verify the work in \"verification\". If nothing can be \
             verified by a command, say why in \"summary\" and pass [\"none\"]."
        ),
        "rule 1's text: {}",
        empty.content
    );
    assert_eq!(outcome.get(), None);
}

/// (e) The strict policy records `NotRun("no file changed")` for an accepted
/// `["none"]`: not writing a file is no proof that an answer is right.
#[tokio::test]
async fn the_strict_policy_labels_none_without_a_change() {
    let activity = FakeActivity::new();
    let (finish, outcome) = tool(activity, CompletionPolicy::RecordedCommands);

    let accepted = done(&finish, r#"["none"]"#).await;

    assert_eq!(accepted.status, ToolStatus::Ok);
    assert_eq!(
        outcome.get(),
        Some(Accepted::Done {
            summary: "s".to_string(),
            evidence: Evidence::NotRun("no file changed".to_string()),
        })
    );
}

/// (f) `FinishTool::new` stays the strict policy, and the two policies present the
/// same tool under the same name and identity with a different description: the
/// description is where the model reads which rule applies to it.
#[test]
fn the_policy_changes_only_the_description_and_the_rule() {
    let plain = FinishTool::new(FakeActivity::new(), FinishOutcome::default());
    assert_eq!(plain.declaration().name, "finish");
    assert_eq!(plain.identity().implementation, "p1-tool-finish");
    assert_eq!(plain.identity().variant, "claude");

    let report = FinishTool::new(FakeActivity::new(), FinishOutcome::default())
        .with_policy(CompletionPolicy::ReportToParent);
    assert_eq!(report.declaration().name, "finish");
    assert_eq!(report.declaration().kind, plain.declaration().kind);
    assert_eq!(report.identity(), plain.identity());
    assert_ne!(
        report.declaration().description,
        plain.declaration().description
    );

    // The strict face is today's, byte for byte: it still asks for a command.
    assert!(
        plain
            .declaration()
            .description
            .contains("verify first with a command"),
        "{}",
        plain.declaration().description
    );
    // The report-to-parent face says what is expected of an agent that cannot run one.
    let description = &report.declaration().description;
    for expected in [
        "You have no tool that runs commands",
        "[\"none\"]",
        "not verified; parent verification required",
        "what remains unchecked",
    ] {
        assert!(description.contains(expected), "{expected}: {description}");
    }
}

/// A `with_face` after `with_policy` still wins (the catalog applies an environment's
/// face override last), and a policy switch leaves the name and variant alone.
#[test]
fn a_face_override_after_the_policy_still_wins() {
    let tool = FinishTool::new(FakeActivity::new(), FinishOutcome::default())
        .with_policy(CompletionPolicy::ReportToParent)
        .with_face(ToolFace::new("done", "the environment's own words"), "gpt");
    assert_eq!(tool.declaration().name, "done");
    assert_eq!(
        tool.declaration().description,
        "the environment's own words"
    );
    assert_eq!(tool.identity().variant, "gpt");
}
