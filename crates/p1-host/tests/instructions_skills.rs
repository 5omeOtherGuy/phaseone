//! The real catalog serves frozen skill data from explicit scratch homes.
mod common;

use std::path::Path;
use std::sync::Arc;

use common::*;
use p1_assembly::{Substitutions, assemble, load_environment};
use p1_contracts::{
    CancellationToken, Concurrency, Effect, ToolCall, ToolContext, ToolInput, ToolStatus,
};
use p1_host::{activity::CompletionHub, catalog::build_catalog, cli::SandboxMode};
use p1_testkit::ScriptedProvider;
use serde_json::json;

fn write(path: &Path, text: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

#[tokio::test]
async fn skill_tool_returns_body_directory_unknown_error_and_utf8_cap() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("home");
    let workspace = temp.path().join("workspace");
    let environments = temp.path().join("environments");
    write_environment(&environments, "test", "fake", "model", &["skill"], "base");
    write(&workspace.join("AGENTS.md"), "local rules");
    write(&home.join(".agents/AGENTS.md"), "global rules");
    let skill_path = home.join(".agents/skills/example/SKILL.md");
    write(
        &skill_path,
        "---\nname: example\ndescription: Example tasks\n---\nBODY\n",
    );
    write(
        &home.join(".agents/skills/large/SKILL.md"),
        &format!(
            "---\ndescription: large body\n---\n{}éTAIL",
            "x".repeat(100 * 1024 - 1)
        ),
    );
    let mut harness = Harness::new(vec![environments.clone()], &[]);
    isolated_environment(&mut harness);
    harness.deps.home = Some(home.clone());
    harness.deps.catalog_hook = Some(provider_hook(vec![(
        "fake",
        ScriptedProvider::new(Vec::new()),
    )]));
    let catalog = build_catalog(
        &harness.deps,
        SandboxMode::Off,
        &[],
        &[],
        &[],
        &Arc::new(CompletionHub::new()),
    )
    .unwrap();
    let environment = load_environment("test", &[environments]).unwrap();
    let assembled = assemble(
        &catalog,
        &environment,
        &workspace,
        &Substitutions {
            workspace: workspace.display().to_string(),
            date: "2026-10-10".into(),
            os: "linux".into(),
            scratch: String::new(),
        },
    )
    .unwrap();
    let sources = p1_host::catalog::modules::module_sources(&harness.deps).unwrap();
    let identity = p1_host::run::assembly_identity(&assembled, "fake", false, &sources);
    assert_eq!(identity.instructions.len(), 2);
    assert_eq!(
        identity.instructions[0].path,
        home.join(".agents/AGENTS.md")
    );
    assert_eq!(
        identity.instructions[0].sha256,
        "62700f9580c3a31104a1d5c1458b12498c8f64a1982b1166aa0a08275a8c8f12"
    );
    assert_eq!(identity.skills[0].path, skill_path);
    // Editing disk after assembly does not change the catalog's frozen skill body.
    write(&skill_path, "changed");
    let call = |name: &str| ToolCall {
        call_id: "c".into(),
        name: "skill".into(),
        input: ToolInput::Json(json!({"name": name}).to_string()),
    };
    let tool = &assembled.tools[0];
    assert_eq!(tool.identity().implementation, "p1/skill");
    assert_eq!(tool.concurrency(&call("example")), Concurrency::Shared);
    assert_eq!(tool.effect(&call("example")), Effect::ReadOnly);
    let context = || ToolContext {
        cancel: CancellationToken::new(),
    };
    let result = tool.execute(&call("example"), context()).await;
    assert_eq!(result.status, ToolStatus::Ok);
    assert_eq!(
        result.content,
        format!(
            "Skill directory: {}\n\nBODY\n",
            skill_path.parent().unwrap().display()
        )
    );
    let unknown = tool.execute(&call("absent"), context()).await;
    assert_eq!(unknown.status, ToolStatus::Error);
    assert!(unknown.content.contains("unknown skill: absent"));
    for raw in [
        "{}",
        "{\"name\": 7}",
        "{\"name\": \"example\", \"path\": \"/\"}",
    ] {
        let invalid = ToolCall {
            input: ToolInput::Json(raw.into()),
            ..call("example")
        };
        let result = tool.execute(&invalid, context()).await;
        assert_eq!(result.status, ToolStatus::Error);
        assert!(result.content.contains("invalid skill input"));
    }
    let large = tool.execute(&call("large"), context()).await;
    assert_eq!(large.status, ToolStatus::Ok);
    let body = large.content;
    assert!(body.ends_with("\n[Skill body truncated; at most 100 KiB returned.]\n"));
    assert!(body.contains(&"x".repeat(100 * 1024 - 1)));
    assert!(!body.contains('é'));
    assert!(!body.contains("TAIL"));
}

#[tokio::test]
async fn env_show_honors_settings_global_override_and_disabled_environment() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("home");
    let workspace = temp.path().join("workspace");
    let environments = temp.path().join("environments");
    write_environment(&environments, "test", "fake", "model", &["skill"], "base");
    write(
        &home.join(".config/p1/settings.toml"),
        "instructions_global = '~/rules.md'\n",
    );
    write(&home.join("rules.md"), "override rules");
    write(&home.join(".agents/AGENTS.md"), "not selected");
    write(&workspace.join("AGENTS.md"), "workspace rules");
    write(
        &home.join(".agents/skills/example/SKILL.md"),
        "---\ndescription: example task\n---\nbody",
    );
    let mut harness = Harness::new(vec![environments.clone()], &[]);
    isolated_environment(&mut harness);
    harness.deps.home = Some(home.clone());
    harness.deps.shell_env = Some(vec![("HOME".into(), home.as_os_str().to_owned())]);
    harness.deps.catalog_hook = Some(provider_hook(vec![(
        "fake",
        ScriptedProvider::new(Vec::new()),
    )]));
    let mut options = p1_host::cli::parse(&["env".into(), "show".into(), "test".into()]).unwrap();
    options.workspace = Some(workspace.clone());
    assert_eq!(
        p1_host::run::run(&mut harness.deps, options.clone()).await,
        0,
        "{}",
        harness.stderr.text()
    );
    let stdout = harness.stdout.text();
    assert!(stdout.contains(&home.join("rules.md").display().to_string()));
    assert!(!stdout.contains("not selected"));
    let resolved = env_show_json(&stdout);
    assert_eq!(
        resolved["instruction_data"]["files"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    assert_eq!(resolved["instruction_data"]["skills"][0]["name"], "example");
    assert!(stdout.contains("body-sha256:"));
    write(
        &environments.join("test/environment.toml"),
        "family = 'test'\nprovider = 'fake'\nmodel = 'model'\n[instructions]\nenabled = false\n",
    );
    let mut disabled = Harness::new(vec![environments], &[]);
    isolated_environment(&mut disabled);
    disabled.deps.home = Some(home);
    disabled.deps.catalog_hook = Some(provider_hook(vec![(
        "fake",
        ScriptedProvider::new(Vec::new()),
    )]));
    assert_eq!(p1_host::run::run(&mut disabled.deps, options).await, 0);
    let resolved = env_show_json(&disabled.stdout.text());
    assert_eq!(resolved["instructions"]["enabled"], false);
    assert_eq!(resolved["instruction_data"]["files"], json!([]));
    assert_eq!(resolved["system_prompt"], "base");
}
