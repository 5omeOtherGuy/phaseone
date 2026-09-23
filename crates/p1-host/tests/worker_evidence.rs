//! ADR-0051: a worker without a command tool may finish `done`, and every worker's
//! end says what was established — `commands passed: …` or `not verified; parent
//! verification required`.
//!
//! The worker service records the evidence from the child's ACCEPTED outcome, so
//! `worker_result` and the host's own worker line carry it without the parent asking.
//! A worker whose grant includes the shell tool keeps ADR-0037's strict rule, and a
//! `worker_continue add_tools: ["shell"]` moves it onto that rule for its next turn.
//!
//! The scratch environments below use only `{{tool_names}}` in their prompts, so each
//! assembles with whatever subset the grant names. Nothing here touches the network or
//! a real credential file: the providers are the testkit's scripted fakes and the
//! "route" is the fake's own description.
#![cfg(feature = "delegation")]

mod common;

use std::path::Path;

use common::{Harness, provider_hook, run_args, write_environment};
use p1_contracts::{Item, ProviderRequest};
use p1_testkit::{ScriptedProvider, json_call, text_response, tool_call_response};
use tempfile::tempdir;

/// A parent that can start, read and continue workers, and a scratch child whose
/// prompt names exactly the tools it is assembled with.
fn scratch_environments(root: &Path) {
    write_environment(
        root,
        "evidence-parent",
        "fake-parent",
        "model-parent",
        &["worker_start", "worker_result", "worker_continue"],
        "PARENT {{tool_names}}",
    );
    write_environment(
        root,
        "evidence-child",
        "fake-child",
        "model-child",
        &["read", "edit", "shell"],
        "CHILD {{tool_names}}",
    );
}

/// A main-agent environment (no worker tools, nobody grants anything).
fn main_environment(root: &Path) {
    write_environment(
        root,
        "evidence-main",
        "fake-main",
        "model-main",
        &["read", "edit", "finish"],
        "MAIN {{tool_names}}",
    );
}

/// Every `worker_result` content the parent's LAST request carried, in call order.
fn worker_results(requests: &[ProviderRequest]) -> Vec<String> {
    let Some(last) = requests.last() else {
        return Vec::new();
    };
    last.history
        .iter()
        .filter_map(|item| match item {
            Item::ToolResult(result) if result.name == "worker_result" => {
                Some(result.content.clone())
            }
            _ => None,
        })
        .collect()
}

/// A request's tool result contents that contain `needle` — the child's own view of
/// what its `finish` calls answered.
fn saw(requests: &[ProviderRequest], needle: &str) -> bool {
    requests.iter().any(|request| {
        request.history.iter().any(|item| match item {
            Item::ToolResult(result) => result.content.contains(needle),
            _ => false,
        })
    })
}

/// The description the assembled `finish` tool carries in this request.
fn finish_description(request: &ProviderRequest) -> String {
    request
        .tools
        .iter()
        .find(|tool| tool.name == "finish")
        .expect("every worker gets finish")
        .description
        .clone()
}

fn tool_names(request: &ProviderRequest) -> Vec<String> {
    request.tools.iter().map(|tool| tool.name.clone()).collect()
}

fn read(file: &str) -> p1_testkit::Step {
    tool_call_response(vec![json_call(
        "r1",
        "read",
        &format!(r#"{{"file_path":"{file}"}}"#),
    )])
}

fn edit(file: &str, from: &str, to: &str) -> p1_testkit::Step {
    tool_call_response(vec![json_call(
        "e1",
        "edit",
        &format!(r#"{{"file_path":"{file}","old_string":"{from}","new_string":"{to}"}}"#),
    )])
}

fn finish(status_and_rest: &str) -> p1_testkit::Step {
    tool_call_response(vec![json_call(
        "f1",
        "finish",
        &format!(r#"{{"summary":"s",{status_and_rest}}}"#),
    )])
}

fn shell(command: &str) -> p1_testkit::Step {
    tool_call_response(vec![json_call(
        "s1",
        "shell",
        &format!(r#"{{"command":"{command}"}}"#),
    )])
}

/// The parent's four steps: start, wait for the worker, answer, ack the notification.
fn parent_that_starts(environment: &str, task: &str, tools: &str) -> ScriptedProvider {
    ScriptedProvider::new(vec![
        tool_call_response(vec![json_call(
            "c1",
            "worker_start",
            &format!(r#"{{"environment":"{environment}","task":"{task}","tools":{tools}}}"#),
        )]),
        tool_call_response(vec![json_call(
            "c2",
            "worker_result",
            r#"{"id":"w1","wait":true}"#,
        )]),
        text_response("parent done"),
        text_response("parent notified"),
    ])
}

fn result_waiting() -> p1_testkit::Step {
    tool_call_response(vec![json_call(
        "c2",
        "worker_result",
        r#"{"id":"w1","wait":true}"#,
    )])
}

/// A workspace with one file, and the two calls that change it.
fn editable_workspace() -> tempfile::TempDir {
    let workspace = tempdir().unwrap();
    std::fs::write(workspace.path().join("note.txt"), "alpha\n").unwrap();
    workspace
}

fn assert_edited(workspace: &tempfile::TempDir) {
    assert_eq!(
        std::fs::read_to_string(workspace.path().join("note.txt")).unwrap(),
        "beta\n",
        "the worker's edit really happened"
    );
}

/// (a) A worker granted `[read, edit]` changes a file and finishes `done` with
/// `["none"]`: accepted, because it has no tool that runs a command — and both the
/// host's line and `worker_result` say the result is not verified.
#[tokio::test]
async fn a_worker_without_a_command_tool_finishes_done_and_says_not_verified() {
    let workspace = editable_workspace();
    let environments = tempdir().unwrap();
    scratch_environments(environments.path());

    let child = ScriptedProvider::new(vec![
        read("note.txt"),
        edit("note.txt", "alpha", "beta"),
        // ADR-0037's rule would reject this after the edit above.
        finish(r#""status":"done","verification":["none"]"#),
        text_response("child done"),
    ]);
    let child_handle = child.clone();
    let parent = parent_that_starts("evidence-child", "change the file", r#"["read","edit"]"#);
    let parent_handle = parent.clone();
    let mut harness = Harness::new(vec![environments.path().to_path_buf()], &[]);
    harness.deps.catalog_hook = Some(provider_hook(vec![
        ("fake-parent", parent),
        ("fake-child", child),
    ]));

    let code = run_args(
        &mut harness,
        &[
            "--yes",
            "--env",
            "evidence-parent",
            "--workspace",
            workspace.path().to_str().unwrap(),
            "go",
        ],
    )
    .await;

    assert_eq!(code, 0, "stderr: {}", harness.stderr.text());
    assert_edited(&workspace);

    let requests = child_handle.requests();
    assert_eq!(
        tool_names(&requests[0]),
        ["read", "edit", "finish"],
        "the grant plus finish, and nothing else"
    );
    assert!(
        finish_description(&requests[0]).contains("You have no tool that runs commands"),
        "a worker with no command tool is told so: {}",
        finish_description(&requests[0])
    );

    assert_eq!(
        worker_results(&parent_handle.requests()),
        vec![
            "tools: read, edit, finish\n\
             finish: done — not verified; parent verification required\n\
             ---\n\
             Worker w1: finished\n\n\
             child done"
                .to_string()
        ],
        "the report carries what the accepted outcome established"
    );
    let stderr = harness.stderr.text();
    assert!(
        stderr.contains(
            "· worker w1 (fake-route/fake-model; read, edit, finish) done — not verified; parent \
             verification required\n"
        ),
        "the host prints the worker's end itself: {stderr}"
    );
}

/// (b) The SAME worker names a command it never ran: rejected with ADR-0037's text,
/// inside the turn — and its next call is accepted.
#[tokio::test]
async fn a_fabricated_command_is_still_rejected_without_a_command_tool() {
    let workspace = editable_workspace();
    let environments = tempdir().unwrap();
    scratch_environments(environments.path());

    let child = ScriptedProvider::new(vec![
        read("note.txt"),
        edit("note.txt", "alpha", "beta"),
        finish(r#""status":"done","verification":["cargo test --lib"]"#),
        finish(r#""status":"done","verification":["none"]"#),
        text_response("child done"),
    ]);
    let child_handle = child.clone();
    let parent = parent_that_starts("evidence-child", "change the file", r#"["read","edit"]"#);
    let parent_handle = parent.clone();
    let mut harness = Harness::new(vec![environments.path().to_path_buf()], &[]);
    harness.deps.catalog_hook = Some(provider_hook(vec![
        ("fake-parent", parent),
        ("fake-child", child),
    ]));

    let code = run_args(
        &mut harness,
        &[
            "--yes",
            "--env",
            "evidence-parent",
            "--workspace",
            workspace.path().to_str().unwrap(),
            "go",
        ],
    )
    .await;

    assert_eq!(code, 0, "stderr: {}", harness.stderr.text());
    let requests = child_handle.requests();
    assert!(
        saw(
            &requests,
            "No successful run of `cargo test --lib` is recorded in this session. Run it, read \
             the result, then finish."
        ),
        "the ADR-0037 text, unchanged: {requests:?}"
    );
    assert!(
        saw(
            &requests,
            "No run counts right now: run your checks (without a pipe) after your last file change."
        ),
        "the trailer, unchanged: {requests:?}"
    );
    assert_eq!(
        worker_results(&parent_handle.requests()),
        vec![
            "tools: read, edit, finish\n\
             finish: done — not verified; parent verification required\n\
             ---\n\
             Worker w1: finished\n\n\
             child done"
                .to_string()
        ],
        "the rejected call stored nothing; the accepted one is what is reported"
    );
}

/// (c) A worker granted `[read, edit, shell]` keeps ADR-0037's rule: `["none"]` after
/// an edit is rejected, a command it really ran is what finishes the work, and the
/// end names that command.
#[tokio::test]
async fn a_worker_with_a_command_tool_stays_strict() {
    let workspace = editable_workspace();
    let environments = tempdir().unwrap();
    scratch_environments(environments.path());

    let child = ScriptedProvider::new(vec![
        read("note.txt"),
        edit("note.txt", "alpha", "beta"),
        finish(r#""status":"done","verification":["none"]"#),
        shell("true"),
        finish(r#""status":"done","verification":["true"]"#),
        text_response("child done"),
    ]);
    let child_handle = child.clone();
    let parent = parent_that_starts(
        "evidence-child",
        "change the file",
        r#"["read","edit","shell"]"#,
    );
    let parent_handle = parent.clone();
    let mut harness = Harness::new(vec![environments.path().to_path_buf()], &[]);
    harness.deps.catalog_hook = Some(provider_hook(vec![
        ("fake-parent", parent),
        ("fake-child", child),
    ]));

    let code = run_args(
        &mut harness,
        &[
            "--yes",
            "--env",
            "evidence-parent",
            "--workspace",
            workspace.path().to_str().unwrap(),
            "go",
        ],
    )
    .await;

    assert_eq!(code, 0, "stderr: {}", harness.stderr.text());
    let requests = child_handle.requests();
    assert!(
        saw(
            &requests,
            "This session changed files; verify the result with a command before finishing."
        ),
        "ADR-0037's rule is not relaxed for a worker that has `shell`: {requests:?}"
    );
    assert!(
        finish_description(&requests[0]).contains("verify first with a command"),
        "the strict description: {}",
        finish_description(&requests[0])
    );
    assert_eq!(
        worker_results(&parent_handle.requests()),
        vec![
            "tools: read, edit, shell, finish\n\
             finish: done — commands passed: true\n\
             ---\n\
             Worker w1: finished\n\n\
             child done"
                .to_string()
        ]
    );
    let stderr = harness.stderr.text();
    assert!(
        stderr.contains(
            "· worker w1 (fake-route/fake-model; read, edit, shell, finish) done — commands \
             passed: true\n"
        ),
        "the end names the command that was really run: {stderr}"
    );
}

/// (d) `worker_continue add_tools: ["shell"]` puts the worker back on the strict rule
/// for its next turn: the first turn may finish unverified, the second may not.
#[tokio::test]
async fn a_regranted_shell_puts_the_next_turn_on_the_strict_rule() {
    let workspace = editable_workspace();
    let environments = tempdir().unwrap();
    scratch_environments(environments.path());

    // Turn 1: change the file and finish `["none"]` — accepted, unverified.
    // Turn 2, after `add_tools: ["shell"]`: the same call is refused, so the worker
    // runs a command and names it.
    let child = ScriptedProvider::new(vec![
        read("note.txt"),
        edit("note.txt", "alpha", "beta"),
        finish(r#""status":"done","verification":["none"]"#),
        text_response("turn one"),
        finish(r#""status":"done","verification":["none"]"#),
        shell("true"),
        finish(r#""status":"done","verification":["true"]"#),
        text_response("turn two"),
    ]);
    let child_handle = child.clone();
    let parent = ScriptedProvider::new(vec![
        tool_call_response(vec![json_call(
            "c1",
            "worker_start",
            r#"{"environment":"evidence-child","task":"change the file","tools":["read","edit"]}"#,
        )]),
        result_waiting(),
        tool_call_response(vec![json_call(
            "c3",
            "worker_continue",
            r#"{"id":"w1","message":"here is shell","add_tools":["shell"]}"#,
        )]),
        result_waiting(),
        text_response("parent done"),
        text_response("parent notified"),
    ]);
    let parent_handle = parent.clone();
    let mut harness = Harness::new(vec![environments.path().to_path_buf()], &[]);
    harness.deps.catalog_hook = Some(provider_hook(vec![
        ("fake-parent", parent),
        ("fake-child", child),
    ]));

    let code = run_args(
        &mut harness,
        &[
            "--yes",
            "--env",
            "evidence-parent",
            "--workspace",
            workspace.path().to_str().unwrap(),
            "go",
        ],
    )
    .await;

    assert_eq!(code, 0, "stderr: {}", harness.stderr.text());
    let requests = child_handle.requests();
    assert_eq!(tool_names(&requests[0]), ["read", "edit", "finish"]);
    assert!(
        finish_description(&requests[0]).contains("You have no tool that runs commands"),
        "turn 1 reports to the parent: {}",
        finish_description(&requests[0])
    );
    let repaired = requests.last().unwrap();
    assert_eq!(tool_names(repaired), ["read", "edit", "shell", "finish"]);
    assert!(
        finish_description(repaired).contains("verify first with a command"),
        "the re-granted turn carries the strict rule: {}",
        finish_description(repaired)
    );
    assert!(
        saw(
            &requests,
            "This session changed files; verify the result with a command before finishing."
        ),
        "turn 2 may not finish unverified: {requests:?}"
    );

    assert_eq!(
        worker_results(&parent_handle.requests()),
        vec![
            "tools: read, edit, finish\n\
             finish: done — not verified; parent verification required\n\
             ---\n\
             Worker w1: finished\n\n\
             turn one"
                .to_string(),
            "tools: read, edit, shell, finish\n\
             finish: done — commands passed: true\n\
             ---\n\
             Worker w1: finished\n\n\
             turn two"
                .to_string(),
        ]
    );
    let stderr = harness.stderr.text();
    assert!(
        stderr.contains("done — not verified; parent verification required\n"),
        "turn 1's end: {stderr}"
    );
    assert!(
        stderr.contains("done — commands passed: true\n"),
        "turn 2's end: {stderr}"
    );
}

/// (e) A re-grant that still lacks a command tool keeps `ReportToParent`: the worker
/// may still finish unverified, and the label says so again.
#[tokio::test]
async fn a_regrant_without_a_command_tool_stays_unverified() {
    let workspace = editable_workspace();
    let environments = tempdir().unwrap();
    scratch_environments(environments.path());

    let child = ScriptedProvider::new(vec![
        read("note.txt"),
        edit("note.txt", "alpha", "beta"),
        finish(r#""status":"done","verification":["none"]"#),
        text_response("turn one"),
        finish(r#""status":"done","verification":["none"]"#),
        text_response("turn two"),
    ]);
    let child_handle = child.clone();
    let parent = ScriptedProvider::new(vec![
        tool_call_response(vec![json_call(
            "c1",
            "worker_start",
            r#"{"environment":"evidence-child","task":"change the file","tools":["read","edit"]}"#,
        )]),
        result_waiting(),
        // `grep` is no help with verification: the grant still holds no command tool.
        tool_call_response(vec![json_call(
            "c3",
            "worker_continue",
            r#"{"id":"w1","message":"look around","add_tools":["grep"]}"#,
        )]),
        result_waiting(),
        text_response("parent done"),
        text_response("parent notified"),
    ]);
    let parent_handle = parent.clone();
    let mut harness = Harness::new(vec![environments.path().to_path_buf()], &[]);
    harness.deps.catalog_hook = Some(provider_hook(vec![
        ("fake-parent", parent),
        ("fake-child", child),
    ]));

    let code = run_args(
        &mut harness,
        &[
            "--yes",
            "--env",
            "evidence-parent",
            "--workspace",
            workspace.path().to_str().unwrap(),
            "go",
        ],
    )
    .await;

    assert_eq!(code, 0, "stderr: {}", harness.stderr.text());
    let requests = child_handle.requests();
    assert_eq!(tool_names(&requests[0]), ["read", "edit", "finish"]);
    let repaired = requests.last().unwrap();
    assert_eq!(tool_names(repaired), ["read", "edit", "grep", "finish"]);
    assert!(
        finish_description(repaired).contains("You have no tool that runs commands"),
        "still no command tool, so still the parent's problem: {}",
        finish_description(repaired)
    );
    assert_eq!(
        worker_results(&parent_handle.requests()),
        vec![
            "tools: read, edit, finish\n\
             finish: done — not verified; parent verification required\n\
             ---\n\
             Worker w1: finished\n\n\
             turn one"
                .to_string(),
            "tools: read, edit, grep, finish\n\
             finish: done — not verified; parent verification required\n\
             ---\n\
             Worker w1: finished\n\n\
             turn two"
                .to_string(),
        ]
    );
}

/// (f) A main agent is untouched by the policy: after an edit, `["none"]` is still
/// refused, and it ends `blocked` as before (exit 3).
#[tokio::test]
async fn a_main_agent_keeps_the_strict_rule() {
    let workspace = editable_workspace();
    let environments = tempdir().unwrap();
    main_environment(environments.path());

    let main = ScriptedProvider::new(vec![
        read("note.txt"),
        edit("note.txt", "alpha", "beta"),
        finish(r#""status":"done","verification":["none"]"#),
        finish(r#""status":"blocked","needs":"shell""#),
        text_response("main blocked"),
    ]);
    let main_handle = main.clone();
    let mut harness = Harness::new(vec![environments.path().to_path_buf()], &[]);
    harness.deps.catalog_hook = Some(provider_hook(vec![("fake-main", main)]));

    let code = run_args(
        &mut harness,
        &[
            "--yes",
            "--env",
            "evidence-main",
            "--workspace",
            workspace.path().to_str().unwrap(),
            "change the file",
        ],
    )
    .await;

    assert_eq!(code, 3, "stderr: {}", harness.stderr.text());
    assert!(
        harness.stderr.text().contains("blocked: shell"),
        "the blocker is reported as before: {}",
        harness.stderr.text()
    );
    let requests = main_handle.requests();
    assert!(
        saw(
            &requests,
            "This session changed files; verify the result with a command before finishing."
        ),
        "a main agent has no report-to-parent policy: {requests:?}"
    );
    assert!(
        !saw(&requests, "not verified"),
        "nothing a main agent sees mentions the parent: {requests:?}"
    );
}
