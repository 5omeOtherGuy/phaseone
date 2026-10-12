//! The model is the selector (ADR-0217 Decision 3): a bare `--model P` or `/model P` resolves
//! its route — the session's route when it binds the profile, else the route `settings.toml`
//! prefers for the profile, then for its family, else the sole route that binds it, else a
//! refusal naming the candidates — and the kit of the profile's family supplies the tools, the
//! prompt, the context and the options. Fake providers, scratch config, no network.

mod common;

use std::path::{Path, PathBuf};

use common::{Harness, provider_hook, run_args, shipped_catalog};
use p1_contracts::ProviderRequest;
use p1_testkit::{ScriptedProvider, text_response};

fn write(path: &Path, text: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

/// A harness on `environment_dirs` whose home and config are `config`.
fn harness(environment_dirs: Vec<PathBuf>, config: &Path, lines: &[&str]) -> Harness {
    let mut harness = Harness::new(environment_dirs, lines);
    harness.deps.shell_env = Some(vec![
        ("HOME".into(), config.as_os_str().to_os_string()),
        ("XDG_CONFIG_HOME".into(), config.as_os_str().to_os_string()),
    ]);
    harness
}

fn tool_names(request: &ProviderRequest) -> Vec<String> {
    request.tools.iter().map(|tool| tool.name.clone()).collect()
}

/// `p1 --model gpt-6-sol` from the shipped `default` environment, with the user's route
/// preferences, runs the `gpt` kit on the Codex route; `/model claude-opus-5-5` then switches
/// tools, prompt and route back to the `claude` kit on the preferred Anthropic route.
#[tokio::test]
async fn a_bare_model_takes_its_preferred_route_and_its_familys_kit_and_switches_back() {
    let workspace = tempfile::tempdir().unwrap();
    let config = tempfile::tempdir().unwrap();
    write(
        &config.path().join("p1/settings.toml"),
        "[preferred_routes]\ngpt = \"openai-codex-subscription\"\nclaude = \"anthropic\"\n",
    );
    let session = workspace.path().join("session.jsonl");
    let codex = ScriptedProvider::new(vec![text_response("from gpt")]);
    let anthropic = ScriptedProvider::new(vec![text_response("from claude")]);
    let mut harness = harness(
        shipped_catalog(),
        config.path(),
        &["hello", "/model claude-opus-5-5", "go", "/exit"],
    );
    harness.deps.catalog_hook = Some(provider_hook(vec![
        ("openai-codex-subscription", codex.clone()),
        ("anthropic", anthropic.clone()),
    ]));

    let code = run_args(
        &mut harness,
        &[
            "--yes",
            "--model",
            "gpt-6-sol",
            "--workspace",
            workspace.path().to_str().unwrap(),
            "--session",
            session.to_str().unwrap(),
        ],
    )
    .await;
    let stderr = harness.stderr.text();
    assert_eq!(code, 0, "{stderr}");

    let gpt = codex.requests();
    assert_eq!(
        gpt.len(),
        1,
        "the first turn ran on the Codex route: {stderr}"
    );
    let tools = tool_names(&gpt[0]);
    assert!(tools.contains(&"apply_patch".to_string()), "{tools:?}");
    assert!(tools.contains(&"shell".to_string()), "{tools:?}");
    assert!(!tools.contains(&"edit".to_string()), "{tools:?}");
    assert!(
        gpt[0]
            .system_prompt
            .starts_with("You are a coding agent working in the repository at"),
        "the gpt kit's prompt: {}",
        &gpt[0].system_prompt[..200]
    );

    let claude = anthropic.requests();
    assert_eq!(
        claude.len(),
        1,
        "the switched turn ran on the Anthropic route"
    );
    let tools = tool_names(&claude[0]);
    for tool in ["read", "edit", "write", "grep"] {
        assert!(tools.contains(&tool.to_string()), "{tools:?}");
    }
    assert!(!tools.contains(&"apply_patch".to_string()), "{tools:?}");
    assert!(
        claude[0]
            .system_prompt
            .starts_with("You are a coding agent sharing one workspace with the user"),
        "the claude kit's prompt (base): {}",
        &claude[0].system_prompt[..200]
    );
    assert!(
        stderr.contains("· model: default/claude-opus-5-5"),
        "{stderr}"
    );
}

/// Without a preference a bare model several routes bind is refused, naming them; with only
/// one route it needs none.
#[tokio::test]
async fn a_bare_model_on_several_routes_without_a_preference_is_refused_naming_them() {
    let workspace = tempfile::tempdir().unwrap();
    let config = tempfile::tempdir().unwrap();
    let mut harness = harness(shipped_catalog(), config.path(), &[]);
    let code = run_args(
        &mut harness,
        &[
            "--model",
            "gpt-6-sol",
            "--workspace",
            workspace.path().to_str().unwrap(),
            "hi",
        ],
    )
    .await;
    assert_ne!(code, 0);
    let stderr = harness.stderr.text();
    assert!(
        stderr.contains(
            "`gpt-6-sol` is bound by more than one route: openai, openai-codex-subscription"
        ),
        "{stderr}"
    );
    assert!(stderr.contains("[preferred_routes]"), "{stderr}");
}

/// A scratch catalog of two families: `pa` (family `fa`) on route `ra` and `pb` (family `fb`)
/// on route `rb`, each kit with its own tools, prompt, compaction threshold and summarizer.
fn two_families(root: &Path) -> PathBuf {
    for (profile, family) in [("pa", "fa"), ("pb", "fb")] {
        write(
            &root.join(format!("profiles/{profile}.toml")),
            &format!(
                "id = \"{profile}\"\nrevision = 1\nmodel_id = \"{profile}-model\"\n\
                 family = \"{family}\"\nthinking = \"enabled\"\nefforts = [\"high\"]\n\
                 default_effort = \"high\"\n"
            ),
        );
    }
    for (route, profile) in [("ra", "pa"), ("rb", "pb")] {
        write(
            &root.join(format!("routes/{route}.toml")),
            &format!(
                "id = \"{route}\"\norigin_route = \"openai-chat/{route}\"\nadapter = \"openai-chat\"\n\
                 endpoint = \"https://example.invalid/v1/chat/completions\"\n[credential]\n\
                 kind = \"api-key\"\nenv = \"KEY\"\n[adapter_settings]\n\
                 dialect = \"thinking-with-reasoning-alias\"\n[models.\"{profile}\"]\n\
                 wire_model = \"{profile}-wire\"\n"
            ),
        );
    }
    // Both kits summarize early; each names its own summarizer.
    for (family, tools, marker) in [
        ("fa", "[[tools]]\nmodule = \"write\"\n", "A"),
        ("fb", "", "B"),
    ] {
        write(
            &root.join(format!("kits/{family}/kit.toml")),
            &format!(
                "prompt_file = \"prompt.md\"\n[[tools]]\nmodule = \"shell\"\n{tools}\
                 [context]\nwindow_tokens = 100000\noutput_headroom_tokens = 1000\n\
                 summarize_at_tokens = 580\nkeep_recent_tokens = 20\nuser_verbatim_tokens = 100\n"
            ),
        );
        write(
            &root.join(format!("kits/{family}/prompt.md")),
            &format!("prompt {marker}"),
        );
        write(
            &root.join(format!("kits/{family}/summarize.md")),
            &format!("SUMMARIZE {marker}"),
        );
    }
    write(
        &root.join("environments/e/environment.toml"),
        "model = \"pa\"\nroute = \"ra\"\n",
    );
    root.join("environments")
}

/// `/model pb` from a session on `ra`: the sole route that binds `pb` is `rb`, and `pb`'s family
/// kit supplies the tools, the prompt and the context policy — the summarizer the next
/// compaction runs is the new kit's.
#[tokio::test]
async fn a_switch_to_another_family_takes_its_tools_prompt_route_and_context_policy() {
    let workspace = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    let config = tempfile::tempdir().unwrap();
    let environments = two_families(root.path());
    let ra = ScriptedProvider::new(vec![text_response(&"a".repeat(2_000))]);
    let rb = ScriptedProvider::new(vec![
        text_response(&"b".repeat(50)),
        text_response("SUMMARY"),
        text_response("done"),
    ]);
    let mut harness = harness(
        vec![environments],
        config.path(),
        &["one", "/model pb", "two", "three", "/exit"],
    );
    harness.deps.catalog_hook = Some(provider_hook(vec![("ra", ra.clone()), ("rb", rb.clone())]));

    let code = run_args(
        &mut harness,
        &[
            "--yes",
            "--env",
            "e",
            "--workspace",
            workspace.path().to_str().unwrap(),
        ],
    )
    .await;
    let stderr = harness.stderr.text();
    assert_eq!(code, 0, "{stderr}");

    let first = ra.requests();
    assert_eq!(first.len(), 1, "{stderr}");
    assert_eq!(tool_names(&first[0]), ["shell", "write"]);
    assert!(first[0].system_prompt.starts_with("prompt A"));

    let switched = rb.requests();
    let turns: Vec<&ProviderRequest> = switched
        .iter()
        .filter(|request| request.system_prompt.starts_with("prompt B"))
        .collect();
    assert_eq!(turns.len(), 2, "turns two and three ran on rb: {stderr}");
    assert_eq!(tool_names(turns[0]), ["shell"]);
    let summaries: Vec<&str> = switched
        .iter()
        .map(|request| request.system_prompt.as_str())
        .filter(|prompt| prompt.starts_with("SUMMARIZE"))
        .collect();
    assert_eq!(
        summaries,
        ["SUMMARIZE B"],
        "the new kit's context policy compacted"
    );
    assert!(stderr.contains("· model: e/pb"), "{stderr}");
}
