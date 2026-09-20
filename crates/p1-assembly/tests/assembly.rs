//! Must-pass assembly examples (brief items a, b, e, f, g, h) and the
//! no-secrets serialisation test.

mod common;

use std::sync::{Arc, Mutex};

use common::*;
use p1_assembly::{
    Catalog, EnvironmentFile, ProviderSpec, ToolServices, ToolSpec, assemble, load_environment,
};
use p1_contracts::{ModelOptions, Provider, Tool};
use p1_testkit::FakeTool;
use p1_workspace::Observation;

// ------------------------------------------------------ (a) shipped claude

#[test]
fn shipped_claude_environment_assembles() {
    let mut catalog = Catalog::new();
    let probe = register_scripted_provider(&mut catalog, "anthropic-subscription");
    register_fake_tools(&mut catalog, &TOOL_KEYS);
    register_fake_tools(&mut catalog, &["finish"]);

    let environment = load_environment("claude", &[shipped_environments()]).unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let assembled = assemble(&catalog, &environment, workspace.path(), &substitutions()).unwrap();

    assert!(
        !assembled.system_prompt.contains("{{"),
        "substituted claude prompt still contains a placeholder"
    );
    assert_eq!(
        tool_names(&assembled.tools),
        ["read", "edit", "write", "grep", "shell", "finish"]
    );
    assert!(
        assembled
            .system_prompt
            .contains("read, edit, write, grep, shell, finish"),
        "{{tool_names}} did not render the tools in file order"
    );

    let validated = probe.validated();
    assert_eq!(validated.len(), 1, "validate must run exactly once");
    assert!(
        validated[0].history.is_empty(),
        "validate must run against the empty-history first request"
    );
    assert_eq!(validated[0].system_prompt, assembled.system_prompt);
    assert_eq!(validated[0].options, environment.options);
    let declared: Vec<&str> = validated[0]
        .tools
        .iter()
        .map(|tool| tool.name.as_str())
        .collect();
    assert_eq!(
        declared,
        ["read", "edit", "write", "grep", "shell", "finish"]
    );
    assert_eq!(assembled.resolved.route.origin.model, "fake-model");
}

// ------------------------------------------------------ (b) shipped gpt + coherence

#[test]
fn shipped_gpt_environment_assembles_and_prompts_are_coherent() {
    let workspace = tempfile::tempdir().unwrap();

    // The brief's catalogs: the gpt environment is served by the Codex-native
    // pair, the claude environment by its five tools. The coherence rule below is
    // applied against the catalog each environment actually uses.
    let mut gpt_catalog = Catalog::new();
    register_scripted_provider(&mut gpt_catalog, "openai-codex-subscription");
    register_fake_tools(&mut gpt_catalog, &["shell", "apply_patch"]);
    register_fake_tools(&mut gpt_catalog, &["finish"]);

    let gpt = load_environment("gpt", &[shipped_environments()]).unwrap();
    let gpt_assembled = assemble(&gpt_catalog, &gpt, workspace.path(), &substitutions()).unwrap();
    assert_eq!(
        tool_names(&gpt_assembled.tools),
        ["shell", "apply_patch", "finish"]
    );
    assert!(!gpt_assembled.system_prompt.contains("`edit`"));
    assert!(!gpt_assembled.system_prompt.contains("`write`"));
    assert_prompt_coherent(&gpt_catalog, &gpt_assembled);

    let mut claude_catalog = Catalog::new();
    register_scripted_provider(&mut claude_catalog, "anthropic-subscription");
    register_fake_tools(
        &mut claude_catalog,
        &["read", "edit", "write", "grep", "shell"],
    );
    register_fake_tools(&mut claude_catalog, &["finish"]);

    let claude = load_environment("claude", &[shipped_environments()]).unwrap();
    let claude_assembled =
        assemble(&claude_catalog, &claude, workspace.path(), &substitutions()).unwrap();
    assert!(!claude_assembled.system_prompt.contains("`apply_patch`"));
    assert_prompt_coherent(&claude_catalog, &claude_assembled);
}

// ------------------------------------------------------ (e) face override

#[test]
fn name_override_becomes_the_model_facing_name() {
    let mut catalog = Catalog::new();
    register_scripted_provider(&mut catalog, "test-provider");
    register_fake_tools(&mut catalog, &["shell"]);

    let environment = EnvironmentFile {
        name: "override".into(),
        family: "test".into(),
        provider: "test-provider".into(),
        model: "test-model".into(),
        options: ModelOptions::default(),
        tools: vec![ToolSpec {
            module: "shell".into(),
            name: Some("bash".into()),
            description: None,
            variant: None,
        }],
        prompt_template: "Run `{{tool:shell}}`. Tools: {{tool_names}}.".into(),
        context: None,
        summarize_prompt: None,
    };

    let workspace = tempfile::tempdir().unwrap();
    let assembled = assemble(&catalog, &environment, workspace.path(), &substitutions()).unwrap();

    assert_eq!(tool_names(&assembled.tools), ["bash"]);
    assert!(
        assembled.system_prompt.contains("Run `bash`. Tools: bash."),
        "override was not substituted: {}",
        assembled.system_prompt
    );
    assert_eq!(assembled.resolved.tools[0].declaration.name, "bash");
}

// ------------------------------------------------------ options

#[test]
fn options_table_maps_onto_model_options() {
    let dir = tempfile::tempdir().unwrap();
    let toml = "family = \"test\"\nprovider = \"p\"\nmodel = \"m\"\n\n[options]\nreasoning_effort = \"extra_high\"\nmax_output_tokens = 4096\ncache_key = \"ck\"\n\n[options.native]\n\"anthropic-messages.thinking\" = true\n";
    write_environment(dir.path(), "opts", toml, "hi");

    let environment = load_environment("opts", &[dir.path().to_path_buf()]).unwrap();
    assert_eq!(
        environment.options.reasoning_effort,
        Some(p1_contracts::Effort::ExtraHigh)
    );
    assert_eq!(environment.options.max_output_tokens, Some(4096));
    assert_eq!(environment.options.cache_key.as_deref(), Some("ck"));
    assert_eq!(
        environment.options.native["anthropic-messages.thinking"],
        serde_json::json!(true)
    );
}

// ------------------------------------------------------ (f) search order

#[test]
fn search_order_prefers_the_first_directory_and_finds_later_ones() {
    let shipped = tempfile::tempdir().unwrap();
    let user = tempfile::tempdir().unwrap();
    let toml = "family = \"test\"\nprovider = \"p\"\nmodel = \"m\"\n";

    write_environment(shipped.path(), "claude", toml, "shipped prompt");
    write_environment(user.path(), "claude", toml, "user prompt");
    write_environment(
        shipped.path(),
        "only-shipped",
        toml,
        "later directory prompt",
    );

    let search_dirs = [user.path().to_path_buf(), shipped.path().to_path_buf()];

    let claude = load_environment("claude", &search_dirs).unwrap();
    assert_eq!(claude.prompt_template, "user prompt");

    let only_shipped = load_environment("only-shipped", &search_dirs).unwrap();
    assert_eq!(only_shipped.prompt_template, "later directory prompt");
}

// ------------------------------------------------------ (g) fresh ToolServices

#[test]
fn two_assemblies_get_fresh_observed_files() {
    let recorded: Arc<Mutex<Vec<p1_workspace::ObservedFiles>>> = Arc::new(Mutex::new(Vec::new()));
    let recorder = recorded.clone();

    let mut catalog = Catalog::new();
    register_scripted_provider(&mut catalog, "test-provider");
    catalog.tool(
        "read",
        Box::new(move |_spec: &ToolSpec, services: &ToolServices| {
            recorder.lock().unwrap().push(services.observed.clone());
            Ok(Arc::new(FakeTool::new("read")) as Arc<dyn Tool>)
        }),
    );

    let environment =
        environment_file("fresh", "test-provider", &["read"], "tools: {{tool_names}}");
    let workspace = tempfile::tempdir().unwrap();
    assemble(&catalog, &environment, workspace.path(), &substitutions()).unwrap();
    assemble(&catalog, &environment, workspace.path(), &substitutions()).unwrap();

    let recorded = recorded.lock().unwrap();
    assert_eq!(recorded.len(), 2, "each assembly builds its own services");
    let path = workspace.path().join("file.txt");
    std::fs::write(&path, b"contents").unwrap();
    recorded[0].record(&path, b"contents");
    assert_eq!(
        recorded[0].check_unchanged(&path, b"contents"),
        Observation::Unchanged
    );
    assert_eq!(
        recorded[1].check_unchanged(&path, b"contents"),
        Observation::NeverObserved,
        "a record in one agent's ObservedFiles must not be visible in another's"
    );
}

// ------------------------------------------------------ (h) empty tool list

#[test]
fn empty_tool_list_assembles() {
    let mut catalog = Catalog::new();
    register_scripted_provider(&mut catalog, "test-provider");

    let environment = environment_file("chat", "test-provider", &[], "A pure chat agent.");
    let workspace = tempfile::tempdir().unwrap();
    let assembled = assemble(&catalog, &environment, workspace.path(), &substitutions()).unwrap();

    assert!(assembled.tools.is_empty());
    assert!(assembled.resolved.tools.is_empty());
    assert_eq!(assembled.system_prompt, "A pure chat agent.");
}

// ------------------------------------------------------ no secrets

#[test]
fn resolved_environment_never_contains_a_captured_secret() {
    const SENTINEL: &str = "sk-sentinel-do-not-leak";

    let mut catalog = Catalog::new();
    catalog.provider(
        "secretive",
        Box::new(|_spec: &ProviderSpec| {
            Ok(Arc::new(HoldingProvider {
                secret: SENTINEL.to_string(),
            }) as Arc<dyn Provider>)
        }),
    );
    register_fake_tools(&mut catalog, &["read"]);

    let environment = environment_file("secret", "secretive", &["read"], "tools: {{tool_names}}");
    let workspace = tempfile::tempdir().unwrap();
    let assembled = assemble(&catalog, &environment, workspace.path(), &substitutions()).unwrap();

    let json = serde_json::to_string(&assembled.resolved).unwrap();
    assert!(
        !json.contains(SENTINEL),
        "resolved environment leaked a credential: {json}"
    );
    assert!(json.contains("\"environment\":\"secret\""));
    assert!(json.contains("fake-route"));
}
