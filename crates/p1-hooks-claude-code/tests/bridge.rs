//! The Claude Code bridge (ADR-0213): its settings files, its hook input, what a hook's exit
//! and output mean, and a Claude Code hook refusing a p1 shell call end to end.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use p1_contracts::serde_json::{Value, json};
use p1_contracts::{
    AuthorizationPolicy, AuthorizationRequest, Decision, Effect, HookDecision, HookEvent,
    HookOutput, ToolCall, ToolIdentity, ToolInput,
};
use p1_hooks::{HookCodec, HookedAuthorization, Hooks, ProcessOutput, ShellProcess};
use p1_hooks_claude_code::{ClaudeCodeCodec, DEFAULT_TIMEOUT, hook_files, read_hooks};
use p1_testkit::ScriptedAuthorization;

fn write(path: &Path, text: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

#[test]
fn hooks_are_read_from_the_user_then_the_project_files_and_what_cannot_run_is_noted() {
    let home = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let files = hook_files(Some(home.path()), workspace.path());
    assert_eq!(
        files,
        [
            home.path().join(".claude/settings.json"),
            workspace.path().join(".claude/settings.json"),
            workspace.path().join(".claude/settings.local.json"),
        ]
    );
    write(
        &files[0],
        r#"{"model": "x", "hooks": {"PreToolUse": [{"matcher": "Bash", "hooks": [
            {"type": "command", "command": "user-guard", "timeout": 5}]}]}}"#,
    );
    // The project's settings.json is absent; its local file adds to the user's.
    write(
        &files[2],
        r#"{"hooks": {
            "PreToolUse": [{"matcher": "Edit|Write", "hooks": [
                {"type": "command", "command": "local-guard"},
                {"type": "prompt", "prompt": "is this fine?"}]}],
            "Stop": [{"hooks": [{"type": "command", "command": "on-stop"}]}],
            "Notification": [{"hooks": [{"type": "command", "command": "notify"}]}]}}"#,
    );
    let read = read_hooks(&files).unwrap();
    let commands: Vec<(&str, Duration)> = read
        .set
        .commands(HookEvent::PreToolUse)
        .iter()
        .map(|hook| (hook.command.as_str(), hook.timeout))
        .collect();
    assert_eq!(
        commands,
        [
            ("user-guard", Duration::from_secs(5)),
            ("local-guard", DEFAULT_TIMEOUT)
        ]
    );
    assert_eq!(read.set.commands(HookEvent::Stop).len(), 1, "read, not run");
    let notes = read.notes.join("\n");
    assert_eq!(read.notes.len(), 3, "{notes}");
    assert!(notes.contains("Notification hooks are not run: p1 has no Notification event"));
    assert!(notes.contains("Stop hooks are read and not run"));
    assert!(notes.contains("of type `prompt` is not run"));

    write(&files[1], "{not json");
    let error = read_hooks(&files).unwrap_err();
    assert!(
        error.contains("settings.json: not Claude Code settings"),
        "{error}"
    );
}

fn codec() -> ClaudeCodeCodec {
    ClaudeCodeCodec::new(
        "s-1".into(),
        PathBuf::from("/work"),
        Some(PathBuf::from("/journal.jsonl")),
    )
}

#[test]
fn a_hook_reads_claude_code_input_with_claude_code_tool_names() {
    let payload = json!({"tool_name": "shell", "tool_input": {"command": "ls"}, "call_id": "c1"});
    assert_eq!(
        codec().subject(HookEvent::PreToolUse, &payload).as_deref(),
        Some("Bash")
    );
    assert_eq!(
        codec().subject(HookEvent::UserPromptSubmit, &json!({"prompt": "x"})),
        None
    );
    let input: Value =
        p1_contracts::serde_json::from_slice(&codec().encode(HookEvent::PreToolUse, &payload))
            .unwrap();
    assert_eq!(
        input,
        json!({
            "session_id": "s-1",
            "transcript_path": "/journal.jsonl",
            "cwd": "/work",
            "hook_event_name": "PreToolUse",
            "tool_name": "Bash",
            "tool_input": {"command": "ls"},
        })
    );
    let input: Value = p1_contracts::serde_json::from_slice(
        &codec().encode(HookEvent::UserPromptSubmit, &json!({"prompt": "fix it"})),
    )
    .unwrap();
    assert_eq!(input["prompt"], "fix it");
    assert_eq!(input["hook_event_name"], "UserPromptSubmit");
}

fn exited(status: i32, stdout: &str, stderr: &str) -> ProcessOutput {
    ProcessOutput {
        status: Some(status),
        stdout: stdout.as_bytes().to_vec(),
        stderr: stderr.as_bytes().to_vec(),
    }
}

fn deny(reason: &str) -> Option<HookDecision> {
    Some(HookDecision::Deny {
        reason: reason.into(),
    })
}

#[test]
fn an_answer_is_read_from_the_exit_code_and_the_json_claude_code_defines() {
    let decode = |event, output: ProcessOutput| codec().decode(event, &output);
    let tool = HookEvent::PreToolUse;
    assert_eq!(decode(tool, exited(0, "", "")), Ok(HookOutput::default()));
    assert_eq!(
        decode(tool, exited(2, "ignored", "  no rm  \n"))
            .unwrap()
            .decision,
        deny("no rm")
    );
    let failed = decode(tool, exited(1, "", "boom")).unwrap_err();
    assert_eq!(failed, "exited 1: boom", "any other exit blocks nothing");
    let specific = r#"{"hookSpecificOutput": {"hookEventName": "PreToolUse",
        "permissionDecision": "deny", "permissionDecisionReason": "protected path"}}"#;
    assert_eq!(
        decode(tool, exited(0, specific, "")).unwrap().decision,
        deny("protected path")
    );
    let ask = r#"{"hookSpecificOutput": {"permissionDecision": "ask"}}"#;
    assert_eq!(
        decode(tool, exited(0, ask, "")).unwrap().decision,
        Some(HookDecision::Ask)
    );
    let legacy = r#"{"decision": "block", "reason": "legacy says no"}"#;
    assert_eq!(
        decode(tool, exited(0, legacy, "")).unwrap().decision,
        deny("legacy says no")
    );
    let approve = r#"{"decision": "approve"}"#;
    assert_eq!(
        decode(tool, exited(0, approve, "")).unwrap().decision,
        Some(HookDecision::Allow)
    );
    let stop = r#"{"continue": false, "stopReason": "budget spent"}"#;
    assert_eq!(
        decode(tool, exited(0, stop, "")).unwrap().stop.as_deref(),
        Some("budget spent")
    );
    // Plain text is context for a prompt or session hook, and a log line for a tool hook.
    let prompt = HookEvent::UserPromptSubmit;
    assert_eq!(
        decode(prompt, exited(0, "today is a holiday\n", ""))
            .unwrap()
            .additional_context
            .as_deref(),
        Some("today is a holiday")
    );
    assert_eq!(
        decode(tool, exited(0, "checked", "")),
        Ok(HookOutput::default())
    );
    let context = r#"{"hookSpecificOutput": {"additionalContext": "use pnpm"}}"#;
    assert_eq!(
        decode(prompt, exited(0, context, ""))
            .unwrap()
            .additional_context
            .as_deref(),
        Some("use pnpm")
    );
}

#[tokio::test]
async fn a_claude_code_hook_that_exits_2_refuses_a_p1_shell_call() {
    let home = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let files = hook_files(Some(home.path()), workspace.path());
    // The hook reads its input and refuses a Bash command that starts with rm.
    write(
        &files[1],
        r#"{"hooks": {"PreToolUse": [{"matcher": "Bash", "hooks": [{"type": "command",
            "command": "grep -q '\"command\":\"rm ' && { echo 'rm is not allowed here' >&2; exit 2; }; exit 0"}]}]}}"#,
    );
    let read = read_hooks(&files).unwrap();
    assert!(read.notes.is_empty(), "{:?}", read.notes);
    let hooks = Hooks::new(
        read.set,
        Arc::new(ClaudeCodeCodec::new(
            "s-1".into(),
            workspace.path().to_owned(),
            None,
        )),
        Arc::new(ShellProcess::new(
            workspace.path().to_owned(),
            vec![(OsString::from("PATH"), OsString::from("/usr/bin:/bin"))],
        )),
        Arc::new(|line| panic!("no hook fails here: {line}")),
    );
    let session = ScriptedAuthorization::permit_all();
    let policy = HookedAuthorization::new(Arc::new(session.clone()), Arc::new(hooks), None);
    let identity = ToolIdentity {
        implementation: "p1/shell".into(),
        variant: "default".into(),
    };
    let decide = |command: &str| {
        let call = ToolCall {
            call_id: "c1".into(),
            name: "shell".into(),
            input: ToolInput::Json(json!({ "command": command }).to_string()),
        };
        let policy = &policy;
        let identity = &identity;
        async move {
            policy
                .authorize(AuthorizationRequest {
                    call: &call,
                    identity,
                    effect: Effect::Executes,
                })
                .await
        }
    };
    assert_eq!(
        decide("rm -rf build").await,
        Decision::Deny {
            reason: "a hook refused this call: rm is not allowed here".into()
        }
    );
    assert_eq!(decide("ls build").await, Decision::Permit);
    assert_eq!(
        session.seen().len(),
        1,
        "only the call the hook let through"
    );
}
