//! The hook core (ADR-0213): matching, the runner's merge and failure rule, the shell process
//! with its cleared environment, timeout and cancellation, and the two decorators. A hook
//! narrows, never grants.

use std::ffi::OsString;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use p1_contracts::serde_json::{Value, json};
use p1_contracts::{
    AuthorizationPolicy, AuthorizationRequest, CancellationToken, ContextInput, ContextPolicy,
    Decision, Effect, HookDecision, HookEvent, HookOutput, HookRunner, InboxKind, Item, ToolCall,
    ToolIdentity, ToolInput,
};
use p1_hooks::{
    HookCodec, HookCommand, HookProcess, HookSet, HookedAuthorization, HookedContext, Hooks,
    Matcher, ProcessOutput, ShellProcess, labelled_context,
};
use p1_testkit::{PassthroughContext, ScriptedAuthorization};

#[test]
fn a_matcher_is_any_names_or_a_whole_subject_pattern() {
    for any in ["", " ", "*"] {
        assert!(
            Matcher::parse(any).unwrap().matches(Some("Bash")),
            "{any:?}"
        );
    }
    let names = Matcher::parse("Bash|Edit").unwrap();
    assert!(names.matches(Some("Edit")));
    assert!(!names.matches(Some("BashOutput")), "names match exactly");
    let pattern = Matcher::parse("mcp__.*").unwrap();
    assert!(pattern.matches(Some("mcp__github__search")));
    assert!(
        !pattern.matches(Some("xmcp__a")),
        "a pattern matches the whole subject"
    );
    assert!(
        names.matches(None),
        "an event without a subject matches every hook"
    );
    assert!(
        Matcher::parse("(")
            .unwrap_err()
            .contains("regular expression")
    );
}

/// Answers by the command text: `deny:<reason>`, `ask`, `allow`, `context:<text>`,
/// `stop:<reason>` or `fail`; records what it ran and the stdin it got.
#[derive(Default)]
struct ScriptedProcess {
    ran: Mutex<Vec<(String, Vec<u8>)>>,
}

impl HookProcess for ScriptedProcess {
    fn run<'a>(
        &'a self,
        command: &'a str,
        stdin: Vec<u8>,
        _timeout: Duration,
        _cancel: &'a CancellationToken,
    ) -> p1_contracts::BoxFuture<'a, Result<ProcessOutput, String>> {
        Box::pin(async move {
            self.ran.lock().unwrap().push((command.to_owned(), stdin));
            if command == "fail" {
                return Err("could not start: scripted".to_owned());
            }
            Ok(ProcessOutput {
                status: Some(0),
                stdout: command.as_bytes().to_vec(),
                stderr: Vec::new(),
            })
        })
    }
}

/// Decodes the scripted command text; the subject is the payload's `tool`.
struct ScriptedCodec;

impl HookCodec for ScriptedCodec {
    fn subject(&self, event: HookEvent, payload: &Value) -> Option<String> {
        (event == HookEvent::PreToolUse).then(|| payload["tool_name"].as_str().unwrap().to_owned())
    }

    fn encode(&self, event: HookEvent, payload: &Value) -> Vec<u8> {
        format!("{} {payload}", event.name()).into_bytes()
    }

    fn decode(&self, _event: HookEvent, output: &ProcessOutput) -> Result<HookOutput, String> {
        let text = String::from_utf8(output.stdout.clone()).unwrap();
        let mut answer = HookOutput::default();
        match text.split_once(':') {
            Some(("deny", reason)) => {
                answer.decision = Some(HookDecision::Deny {
                    reason: reason.into(),
                })
            }
            Some(("context", context)) => answer.additional_context = Some(context.into()),
            Some(("stop", reason)) => answer.stop = Some(reason.into()),
            _ if text == "ask" => answer.decision = Some(HookDecision::Ask),
            _ if text == "allow" => answer.decision = Some(HookDecision::Allow),
            _ => return Err(format!("unreadable answer {text}")),
        }
        Ok(answer)
    }
}

fn hook(matcher: &str, command: &str) -> HookCommand {
    HookCommand {
        matcher: Matcher::parse(matcher).unwrap(),
        command: command.to_owned(),
        timeout: Duration::from_secs(5),
    }
}

fn scripted(
    hooks: &[(HookEvent, &str, &str)],
) -> (Arc<Hooks>, Arc<ScriptedProcess>, Arc<Mutex<Vec<String>>>) {
    let mut set = HookSet::default();
    for (event, matcher, command) in hooks {
        set.push(*event, hook(matcher, command));
    }
    let process = Arc::new(ScriptedProcess::default());
    let reports = Arc::new(Mutex::new(Vec::new()));
    let sink = reports.clone();
    let runner = Hooks::new(
        set,
        Arc::new(ScriptedCodec),
        process.clone(),
        Arc::new(move |line| sink.lock().unwrap().push(line)),
    );
    (Arc::new(runner), process, reports)
}

#[tokio::test]
async fn the_runner_runs_matching_hooks_in_order_merges_and_reports_failures() {
    let (runner, process, reports) = scripted(&[
        (HookEvent::PreToolUse, "Edit", "deny:not this one"),
        (HookEvent::PreToolUse, "Bash", "ask"),
        (HookEvent::PreToolUse, "*", "fail"),
        (HookEvent::PreToolUse, "Bash|Read", "deny:dangerous"),
        (HookEvent::Stop, "", "stop:never run"),
    ]);
    let cancel = CancellationToken::new();
    let answer = runner
        .run(HookEvent::PreToolUse, json!({"tool_name": "Bash"}), &cancel)
        .await;
    assert_eq!(
        answer.decision,
        Some(HookDecision::Deny {
            reason: "dangerous".into()
        })
    );
    let ran: Vec<String> = process
        .ran
        .lock()
        .unwrap()
        .iter()
        .map(|(c, _)| c.clone())
        .collect();
    assert_eq!(
        ran,
        ["ask", "fail", "deny:dangerous"],
        "matching hooks, in file order"
    );
    assert_eq!(
        process.ran.lock().unwrap()[0].1,
        br#"PreToolUse {"tool_name":"Bash"}"#.to_vec(),
        "every hook reads the codec's encoding"
    );
    let reports = reports.lock().unwrap().clone();
    assert_eq!(reports.len(), 1, "{reports:?}");
    assert!(reports[0].contains("PreToolUse hook `fail`") && reports[0].contains("does not block"));

    // An event no decision point serves is read and not run.
    let answer = runner.run(HookEvent::Stop, json!({}), &cancel).await;
    assert_eq!(answer, HookOutput::default());
    assert_eq!(process.ran.lock().unwrap().len(), 3);
}

fn request_parts(name: &str) -> (ToolCall, ToolIdentity) {
    (
        ToolCall {
            call_id: "c1".into(),
            name: name.into(),
            input: ToolInput::Json(r#"{"command":"rm -rf /"}"#.into()),
        },
        ToolIdentity {
            implementation: "p1/shell".into(),
            variant: "default".into(),
        },
    )
}

async fn decide(policy: &dyn AuthorizationPolicy, name: &str) -> Decision {
    let (call, identity) = request_parts(name);
    policy
        .authorize(AuthorizationRequest {
            call: &call,
            identity: &identity,
            effect: Effect::Executes,
        })
        .await
}

#[tokio::test]
async fn a_hook_narrows_the_session_authorization_and_never_widens_it() {
    let (runner, process, _) = scripted(&[
        (HookEvent::PreToolUse, "Bash", "deny:no shells"),
        (HookEvent::PreToolUse, "Read", "allow"),
        (HookEvent::PreToolUse, "Edit", "ask"),
    ]);
    let inner = ScriptedAuthorization::denying(&["Read"]);
    let asker = ScriptedAuthorization::denying(&["Edit"]);
    let hooked = HookedAuthorization::new(
        Arc::new(inner.clone()),
        runner.clone(),
        Some(Arc::new(asker.clone())),
    );
    // A deny refuses whatever the session would have permitted.
    assert_eq!(
        decide(&hooked, "Bash").await,
        Decision::Deny {
            reason: "a hook refused this call: no shells".into()
        }
    );
    // An allow grants nothing: the session's own refusal stands.
    assert_eq!(
        decide(&hooked, "Read").await,
        Decision::Deny {
            reason: "denied by test policy".into()
        }
    );
    // An ask goes to the policy that asks the user, not the session's.
    assert_eq!(
        decide(&hooked, "Edit").await,
        Decision::Deny {
            reason: "denied by test policy".into()
        }
    );
    assert_eq!(asker.seen().len(), 1);
    // No hook objects: the session decides.
    assert_eq!(decide(&hooked, "Write").await, Decision::Permit);
    assert_eq!(
        inner.seen().len(),
        2,
        "Read and Write reached the session policy"
    );
    let payload: Value = p1_contracts::serde_json::from_slice(
        &process.ran.lock().unwrap()[0].1["PreToolUse ".len()..],
    )
    .unwrap();
    assert_eq!(payload["tool_input"]["command"], "rm -rf /");
    assert_eq!(payload["implementation"], "p1/shell");

    // Without a way to ask, a hook's ask refuses.
    let headless = HookedAuthorization::new(Arc::new(inner), runner, None);
    assert!(matches!(
        decide(&headless, "Edit").await,
        Decision::Deny { reason } if reason.contains("cannot ask")
    ));
}

#[tokio::test]
async fn context_hooks_label_what_they_add_run_once_per_prompt_and_can_block() {
    let (runner, process, _) = scripted(&[
        (HookEvent::SessionStart, "", "context:the branch is main"),
        (
            HookEvent::UserPromptSubmit,
            "",
            "context:remember the style guide",
        ),
    ]);
    let hooked = HookedContext::new(Arc::new(PassthroughContext), runner);
    let cancel = CancellationToken::new();
    let history = vec![Item::User {
        text: "fix the bug".into(),
    }];
    let prepare = |history: &[Item]| {
        let history = history.to_vec();
        let hooked = &hooked;
        let cancel = &cancel;
        async move {
            hooked
                .prepare(ContextInput {
                    history: &history,
                    last_usage: None,
                    cancel,
                })
                .await
        }
    };
    let prepared = prepare(&history).await.unwrap().expect("context was added");
    assert_eq!(
        prepared.items[1..],
        [
            Item::Inbox {
                kind: InboxKind::Notification,
                text: labelled_context(HookEvent::SessionStart, "the branch is main"),
            },
            Item::Inbox {
                kind: InboxKind::Notification,
                text: labelled_context(HookEvent::UserPromptSubmit, "remember the style guide"),
            },
        ]
    );
    // The same prompt prepared again runs nothing; the session starts once.
    assert!(prepare(&history).await.unwrap().is_none());
    assert_eq!(process.ran.lock().unwrap().len(), 2);

    let (blocking, _, _) = scripted(&[(HookEvent::UserPromptSubmit, "", "deny:not on friday")]);
    let blocked = HookedContext::new(Arc::new(PassthroughContext), blocking)
        .prepare(ContextInput {
            history: &history,
            last_usage: None,
            cancel: &cancel,
        })
        .await;
    assert!(
        matches!(&blocked, Err(error) if error.to_string().contains("UserPromptSubmit hook blocked the prompt: not on friday")),
        "{:?}",
        blocked.err()
    );
}

fn shell(dir: &std::path::Path) -> ShellProcess {
    ShellProcess::new(
        dir.to_owned(),
        vec![(OsString::from("PATH"), OsString::from("/usr/bin:/bin"))],
    )
}

#[tokio::test]
async fn a_shell_hook_reads_its_input_in_a_cleared_environment_and_reports_its_exit() {
    let dir = tempfile::tempdir().unwrap();
    let process = shell(dir.path());
    let cancel = CancellationToken::new();
    let output = process
        .run(
            "cat; echo \"[$HOME|$PATH]\" >&2; exit 3",
            b"payload".to_vec(),
            Duration::from_secs(10),
            &cancel,
        )
        .await
        .unwrap();
    assert_eq!(output.status, Some(3));
    assert_eq!(output.stdout, b"payload");
    assert_eq!(
        output.stderr, b"[|/usr/bin:/bin]\n",
        "only the given variables"
    );
    let pwd = process
        .run("pwd", Vec::new(), Duration::from_secs(10), &cancel)
        .await
        .unwrap();
    assert_eq!(
        String::from_utf8(pwd.stdout).unwrap().trim(),
        dir.path().canonicalize().unwrap().display().to_string()
    );
}

#[tokio::test]
async fn a_shell_hook_that_runs_too_long_or_is_cancelled_is_stopped() {
    let dir = tempfile::tempdir().unwrap();
    let process = shell(dir.path());
    let cancel = CancellationToken::new();
    let error = process
        .run("sleep 30", Vec::new(), Duration::from_millis(200), &cancel)
        .await
        .unwrap_err();
    assert!(error.contains("ran longer than"), "{error}");
    let cancelled = CancellationToken::new();
    cancelled.cancel();
    let error = process
        .run("sleep 30", Vec::new(), Duration::from_secs(30), &cancelled)
        .await
        .unwrap_err();
    assert_eq!(error, "was cancelled");
}
