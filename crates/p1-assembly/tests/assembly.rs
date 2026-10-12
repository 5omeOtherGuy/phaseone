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

const DELEGATION_KEYS: [&str; 8] = [
    "worker_start",
    "worker_result",
    "worker_continue",
    "worker_cancel",
    "workflow_start",
    "workflow_status",
    "workflow_result",
    "workflow_cancel",
];

// ------------------------------------------------------ (a) the shipped core environment

/// The core `default` environment (ADR-0197); the owner's `claude` it was modelled on, with the
/// same tools, is the owner pack's check now (`packs/owner/tests/test_pack.py`, #853).
#[test]
fn the_shipped_default_environment_assembles() {
    let mut catalog = Catalog::new();
    let probe = register_scripted_provider(&mut catalog, "anthropic");
    register_fake_tools(&mut catalog, &TOOL_KEYS);
    register_fake_tools(&mut catalog, &["finish", "todo_write"]);
    register_fake_tools(&mut catalog, &DELEGATION_KEYS);

    let environment = load_environment("default", &[shipped_environments()]).unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let assembled = assemble(&catalog, &environment, workspace.path(), &substitutions()).unwrap();

    assert!(
        !assembled.system_prompt.contains("{{"),
        "substituted default prompt still contains a placeholder"
    );
    assert_eq!(
        tool_names(&assembled.tools),
        [
            "read",
            "edit",
            "write",
            "grep",
            "shell",
            "shell_job",
            "read_output",
            "ask_user_question",
            "worker_start",
            "worker_result",
            "worker_continue",
            "worker_cancel",
            "workflow_start",
            "workflow_status",
            "workflow_result",
            "workflow_cancel",
            "todo_write",
            "skill"
        ]
    );
    assert!(
        assembled.system_prompt.contains(
            "read, edit, write, grep, shell, shell_job, read_output, ask_user_question, worker_start"
        ),
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
        [
            "read",
            "edit",
            "write",
            "grep",
            "shell",
            "shell_job",
            "read_output",
            "ask_user_question",
            "worker_start",
            "worker_result",
            "worker_continue",
            "worker_cancel",
            "workflow_start",
            "workflow_status",
            "workflow_result",
            "workflow_cancel",
            "todo_write",
            "skill"
        ]
    );
    assert_eq!(assembled.resolved.route.origin.model, "fake-model");
}

// ------------------------------------------------------ (b) coherence of the core prompt

/// The coherence rule over a catalog that also holds the Codex pair the `default` environment
/// does not assemble. The owner's `gpt` and `claude` cases are the owner pack's checks now
/// (`packs/owner/tests/test_pack.py`, #853).
#[test]
fn the_shipped_default_prompt_is_coherent_with_its_catalog() {
    let workspace = tempfile::tempdir().unwrap();
    let mut catalog = Catalog::new();
    register_scripted_provider(&mut catalog, "anthropic");
    register_fake_tools(
        &mut catalog,
        &[
            "skill",
            "read",
            "edit",
            "write",
            "grep",
            "shell",
            "shell_job",
            "read_output",
            "ask_user_question",
            "apply_patch",
        ],
    );
    register_fake_tools(&mut catalog, &["finish", "todo_write"]);
    register_fake_tools(&mut catalog, &DELEGATION_KEYS);

    let default = load_environment("default", &[shipped_environments()]).unwrap();
    let assembled = assemble(&catalog, &default, workspace.path(), &substitutions()).unwrap();
    assert!(!assembled.system_prompt.contains("`apply_patch`"));
    assert_prompt_coherent(&catalog, &assembled);
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
        profile: None,
        profile_text: None,
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
        capabilities: Default::default(),
        tool_concurrency: Default::default(),
        instructions: Default::default(),
        skills: Default::default(),
        sources: Vec::new(),
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

#[test]
fn factory_services_name_only_the_real_grant_not_catalog_or_presented_names() {
    let recorded = Arc::new(Mutex::new(Vec::new()));
    let recorder = recorded.clone();
    let mut catalog = Catalog::new();
    register_scripted_provider(&mut catalog, "test-provider");
    register_fake_tools(&mut catalog, &["shell", "write"]);
    catalog.tool(
        "read",
        Box::new(move |spec, services| {
            recorder.lock().unwrap().push((
                services.environment.clone(),
                services.modules.clone(),
                services.agent.clone(),
                services.prompt_template.clone(),
            ));
            Ok(Arc::new(FakeTool::new(spec.name.as_deref().unwrap_or("read"))) as Arc<dyn Tool>)
        }),
    );
    let mut environment = environment_file(
        "reader",
        "test-provider",
        &["read"],
        "Read with {{tool:read}} in {{workspace}}.",
    );
    environment.tools[0].name = Some("Peek".into());
    let workspace = tempfile::tempdir().unwrap();
    let assembled = p1_assembly::assemble_for_agent(
        &catalog,
        &environment,
        workspace.path(),
        &substitutions(),
        &Arc::new(p1_redact::MaskCounter::new()),
        Some("parent41"),
        |_| ModelOptions::default(),
    )
    .unwrap();
    assert_eq!(tool_names(&assembled.tools), ["Peek"]);
    assert_eq!(
        *recorded.lock().unwrap(),
        vec![(
            "reader".to_string(),
            vec!["read".to_string()],
            Some("parent41".to_string()),
            "Read with {{tool:read}} in {{workspace}}.".to_string(),
        )]
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
    // Deliberately NOT `sk-`-shaped: the gate's `scripts/secret-scan.sh` would flag a
    // key-shaped literal in the tree (issue #142). The value is still an opaque
    // sentinel the resolved environment must never carry.
    const SENTINEL: &str = "sentinel-do-not-leak";

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
