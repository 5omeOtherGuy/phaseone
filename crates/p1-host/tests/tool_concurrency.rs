//! ADR-0118 through the host's real catalog: the shipped modules report `shared` as
//! Decision 1 says, the shell per command (Decision 2), a Shared shell call runs with
//! `GIT_OPTIONAL_LOCKS=0`, and an environment with `[tool_concurrency] shell_reads = false`
//! runs every shell call alone (owner amendment 2026-10-09). Release host entries from the
//! built module set; tempdirs only, no network.

mod common;

use std::num::NonZeroUsize;
use std::sync::Arc;

use common::{Harness, provider_hook};
use p1_assembly::{Assembled, EnvironmentFile, Substitutions, ToolConcurrency, ToolSpec, assemble};
use p1_contracts::{
    CancellationToken, Concurrency, ModelOptions, Tool, ToolCall, ToolContext, ToolInput,
};
use p1_host::activity::CompletionHub;
use p1_host::catalog::build_catalog;
use p1_host::cli::SandboxMode;
use p1_testkit::ScriptedProvider;

const MODULES: [&str; 10] = [
    "read",
    "grep",
    "read_output",
    "shell",
    "shell_job",
    "edit",
    "write",
    "apply_patch",
    "ask_user_question",
    "finish",
];

fn environment(tool_concurrency: ToolConcurrency) -> EnvironmentFile {
    EnvironmentFile {
        name: "concurrency".into(),
        family: "test".into(),
        provider: "fake".into(),
        model: "fake-model".into(),
        profile: None,
        profile_text: None,
        options: ModelOptions::default(),
        tools: MODULES
            .iter()
            .map(|module| ToolSpec {
                module: (*module).into(),
                name: None,
                description: None,
                variant: None,
            })
            .collect(),
        prompt_template: "tools: {{tool_names}}".into(),
        context: None,
        summarize_prompt: None,
        capabilities: Default::default(),
        tool_concurrency,
    }
}

fn assembled(tool_concurrency: ToolConcurrency, workspace: &std::path::Path) -> Assembled {
    let environments = tempfile::tempdir().expect("environments dir");
    let mut harness = Harness::new(vec![environments.path().to_path_buf()], &[]);
    harness.deps.catalog_hook = Some(provider_hook(vec![(
        "fake",
        ScriptedProvider::new(Vec::new()),
    )]));
    let completion = Arc::new(CompletionHub::new());
    let catalog = build_catalog(&harness.deps, SandboxMode::Off, &[], &[], &[], &completion)
        .expect("the release's host entries register");
    assemble(
        &catalog,
        &environment(tool_concurrency),
        workspace,
        &Substitutions {
            workspace: workspace.display().to_string(),
            date: "2026-10-09".into(),
            os: "linux".into(),
            scratch: String::new(),
        },
    )
    .expect("the environment assembles")
}

fn tool<'a>(assembled: &'a Assembled, module: &str) -> &'a Arc<dyn Tool> {
    let index = MODULES.iter().position(|m| *m == module).unwrap();
    &assembled.tools[index]
}

fn call(tool: &Arc<dyn Tool>, raw: &str) -> ToolCall {
    ToolCall {
        call_id: "c1".into(),
        name: tool.declaration().name.clone(),
        input: ToolInput::Json(raw.into()),
    }
}

fn shell(command: &str) -> String {
    p1_contracts::serde_json::json!({ "command": command }).to_string()
}

/// ADR-0118 test 9: each shipped module reports as Decision 1 says, and the shell per
/// command (Decision 2).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shipped_modules_report_concurrency_as_decision_one_says() {
    let workspace = tempfile::tempdir().unwrap();
    let assembled = assembled(ToolConcurrency::default(), workspace.path());
    let concurrency = |module: &str, raw: &str| {
        let tool = tool(&assembled, module);
        tool.concurrency(&call(tool, raw))
    };
    assert_eq!(
        concurrency("read", r#"{"file_path":"notes.txt"}"#),
        Concurrency::Shared
    );
    assert_eq!(
        concurrency("grep", r#"{"pattern":"x"}"#),
        Concurrency::Shared
    );
    assert_eq!(
        concurrency("read_output", r#"{"handle_id":"h1"}"#),
        Concurrency::Shared
    );
    assert_eq!(
        concurrency("shell", &shell("rg x | head")),
        Concurrency::Shared
    );
    assert_eq!(
        concurrency("shell", &shell("git status")),
        Concurrency::Shared
    );
    assert_eq!(
        concurrency("shell", &shell("cargo test")),
        Concurrency::Exclusive
    );
    assert_eq!(
        concurrency(
            "shell",
            &p1_contracts::serde_json::json!({"command": "ls", "background": true}).to_string()
        ),
        Concurrency::Exclusive
    );
    for (module, raw) in [
        ("shell_job", r#"{"job_id":"j1","action":"status"}"#),
        (
            "edit",
            r#"{"file_path":"a","old_string":"x","new_string":"y"}"#,
        ),
        ("write", r#"{"file_path":"a","content":"x"}"#),
        ("apply_patch", "{}"),
        ("ask_user_question", r#"{"questions":[]}"#),
        ("finish", r#"{"status":"done","summary":"s"}"#),
    ] {
        assert_eq!(concurrency(module, raw), Concurrency::Exclusive, "{module}");
    }
}

/// Owner amendment 2026-10-09: `shell_reads = false` runs a classifier-Shared shell call
/// alone; the other tools keep their own answer.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shell_reads_false_runs_every_shell_call_alone() {
    let workspace = tempfile::tempdir().unwrap();
    let assembled = assembled(
        ToolConcurrency {
            max_parallel: NonZeroUsize::new(10).unwrap(),
            shell_reads: false,
        },
        workspace.path(),
    );
    let shell_tool = tool(&assembled, "shell");
    assert_eq!(
        shell_tool.concurrency(&call(shell_tool, &shell("rg x | head"))),
        Concurrency::Exclusive
    );
    let read = tool(&assembled, "read");
    assert_eq!(
        read.concurrency(&call(read, r#"{"file_path":"notes.txt"}"#)),
        Concurrency::Shared
    );
}

/// ADR-0118 test 11: a Shared shell call runs with `GIT_OPTIONAL_LOCKS=0`; a call that runs
/// alone runs its command unchanged.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_shared_shell_call_runs_with_optional_git_locks_off() {
    let workspace = tempfile::tempdir().unwrap();
    let assembled = assembled(ToolConcurrency::default(), workspace.path());
    let shell_tool = tool(&assembled, "shell");
    let run = |command: &str| {
        let call = call(shell_tool, &shell(command));
        async move {
            shell_tool
                .execute(
                    &call,
                    ToolContext {
                        cancel: CancellationToken::new(),
                    },
                )
                .await
                .content
        }
    };
    let shared = "cat /proc/self/environ | tr '\\0' '\\n' | grep -c '^GIT_OPTIONAL_LOCKS=0$'";
    assert_eq!(
        shell_tool.concurrency(&call(shell_tool, &shell(shared))),
        Concurrency::Shared
    );
    let output = run(shared).await;
    assert!(output.starts_with("1\n"), "{output}");
    let alone = "printenv GIT_OPTIONAL_LOCKS || echo unset";
    let output = run(alone).await;
    assert!(output.starts_with("unset\n"), "{output}");
}
