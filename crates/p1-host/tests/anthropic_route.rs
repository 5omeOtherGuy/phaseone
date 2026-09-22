//! The Messages adapter composed from the SHIPPED files (ADR-0039 step 4, spec
//! §7.2–7.4): `environments/claude` → `routes/anthropic-subscription.toml` →
//! `profiles/claude-*.toml`, through the host's own loading path. Nothing here is
//! hand-made except the scripted transport and the fake credential source, and no
//! test touches a credential file or the network.

mod common;

#[path = "../../p1-provider-anthropic/tests/fixtures/mod.rs"]
mod messages_fixtures;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use common::{Harness, shipped_environments};
use p1_assembly::{Assembled, AssemblyError, Substitutions, assemble, load_environment};
use p1_contracts::{
    BoxFuture, DeclarationKind, Effort, Item, ModelOptions, Origin, Provider, ProviderError,
    ProviderRequest, RecordBody, ToolDeclaration,
};
use p1_core::{Agent, AgentParts};
use p1_host::activity::CompletionHub;
use p1_host::catalog::{build_catalog, messages_route, resolve_environment, route_provider};
use p1_host::cli::SandboxMode;
use p1_host::routes::{AdapterSettings, RouteFile, load_route_by_id};
use p1_model_profile::{ModelProfile, ThinkingPolicy};
use p1_provider_anthropic::{MessagesAccount, MessagesAdapterSettings, ROUTE, build_request};
use p1_provider_conformance::{RouteFixtures, RouteUnderTest, run_all};
use p1_provider_http::testing::{RefusingWsConnector, ScriptedTransport};
use p1_provider_http::{Credential, CredentialSource};
use p1_testkit::{PassthroughContext, RecordingEvents, RecordingJournal, ScriptedAuthorization};
use tempfile::tempdir;

/// The bearer the fake credential source returns. Route files hold a credential
/// REFERENCE, so nothing here reads a credential file.
const BEARER: &str = "ANTHROPIC-ROUTE-FAKE-BEARER";

/// Byte for byte what the pre-split adapter wrote into every `Origin.route`. A session
/// recorded then must resume after ADR-0039 step 4 (ADR-0033).
const TODAYS_ORIGIN_ROUTE: &str = "anthropic-messages/claude-subscription";

/// A session file recorded BEFORE this step: the format header, the environment
/// record (with today's origin) and the user input of that turn.
const RECORDED_SESSION: &str = concat!(
    "{\"p1_journal\":1}\n",
    "{\"seq\":0,\"record\":\"environment\",\"route\":{\"origin\":{\"route\":",
    "\"anthropic-messages/claude-subscription\",\"model\":\"claude-sonnet-5\"},",
    "\"supports_freeform_tools\":false,\"mandatory_prompt_prefix\":",
    "\"You are Claude Code, Anthropic's official CLI for Claude.\",",
    "\"reports_cost\":false,\"cache_key\":\"unsupported\"},\"system_prompt\":\"\",",
    "\"tools\":[],\"options\":{\"reasoning_effort\":null,\"max_output_tokens\":null,",
    "\"cache_key\":null,\"native\":{}}}\n",
    "{\"seq\":1,\"record\":\"user_input\",\"text\":\"recorded before the split\"}\n",
);

struct Fixed;

impl CredentialSource for Fixed {
    fn access<'a>(&'a self) -> BoxFuture<'a, Result<Credential, ProviderError>> {
        Box::pin(async {
            Ok(Credential {
                bearer: BEARER.into(),
                account_id: None,
            })
        })
    }

    fn refresh<'a>(
        &'a self,
        _rejected: &'a Credential,
    ) -> BoxFuture<'a, Result<Credential, ProviderError>> {
        Box::pin(async {
            Ok(Credential {
                bearer: format!("{BEARER}-refreshed"),
                account_id: None,
            })
        })
    }
}

fn repo(relative: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join(relative)
}

fn environment_dirs() -> Vec<PathBuf> {
    vec![shipped_environments()]
}

/// The shipped route and profile one environment selects, resolved exactly as the host
/// resolves them before it assembles (spec §2 steps 1–3).
struct Composed {
    route: RouteFile,
    profile: Arc<ModelProfile>,
    wire_model: String,
}

fn composed(environment: &str) -> Composed {
    let dirs = environment_dirs();
    let mut loaded = load_environment(environment, &dirs).expect("the shipped environment loads");
    resolve_environment(&mut loaded, &dirs).expect("the shipped route serves its profile");
    let profile = loaded
        .profile
        .clone()
        .expect("a routed environment names a profile");
    let route = load_route_by_id(&dirs, &loaded.provider).expect("the shipped route file");
    Composed {
        route,
        profile,
        wire_model: loaded.model,
    }
}

/// The provider the catalog factory would build for this composition. The connector
/// is injected next to the transport (ADR-0047 §1); a Messages route never asks for
/// WebSocket, so it ignores it.
fn provider_of(composed: &Composed, transport: ScriptedTransport) -> Arc<dyn Provider> {
    let binding = composed
        .route
        .binding(&composed.profile.id)
        .expect("the route serves this profile");
    route_provider(
        &composed.route,
        binding,
        composed.profile.clone(),
        Arc::new(transport),
        Arc::new(RefusingWsConnector::default()),
        Arc::new(Fixed),
    )
    .expect("the shipped route composes")
}

fn shipped_profile(id: &str) -> ModelProfile {
    let text = std::fs::read_to_string(repo(&format!("profiles/{id}.toml")))
        .unwrap_or_else(|error| panic!("profiles/{id}.toml: {error}"));
    ModelProfile::from_toml(id, &text).unwrap_or_else(|error| panic!("{id}: {error}"))
}

fn assemble_shipped(name: &str) -> Assembled {
    let harness = Harness::new(vec![shipped_environments()], &[]);
    let completion = Arc::new(CompletionHub::new());
    let catalog = build_catalog(&harness.deps, SandboxMode::Off, &[], &[], &[], &completion)
        .expect("the routes the harness can see are valid");
    let mut environment =
        load_environment(name, &harness.deps.environment_dirs).expect("the shipped environment");
    resolve_environment(&mut environment, &harness.deps.environment_dirs)
        .expect("the shipped route serves its profile");
    let workspace = tempdir().unwrap();
    assemble(
        &catalog,
        &environment,
        workspace.path(),
        &Substitutions {
            workspace: workspace.path().display().to_string(),
            date: "2026-09-20".into(),
            os: "linux".into(),
        },
    )
    .unwrap_or_else(|error| panic!("{name} must assemble: {error}"))
}

// ------------------------------------------------------------- the shipped route file

#[test]
fn the_shipped_messages_route_holds_what_the_host_used_to_compile() {
    assert!(
        !p1_host::catalog::WHOLE_PROVIDERS.contains(&"anthropic-subscription"),
        "the Messages key is a route file now, not a whole provider"
    );
    let dirs = environment_dirs();
    let route = load_route_by_id(&dirs, "anthropic-subscription").expect("the shipped route file");
    assert_eq!(route.origin_route, TODAYS_ORIGIN_ROUTE);
    assert_eq!(
        route.origin_route, ROUTE,
        "the adapter's named constant is the file's string"
    );
    assert_eq!(route.adapter, "anthropic-messages");
    assert_eq!(route.endpoint, "https://api.anthropic.com");
    assert_eq!(
        route.credential.kind,
        p1_auth::CredentialKind::ClaudeCodeOauth
    );
    assert_eq!(
        route.settings().expect("the adapter parses its settings"),
        AdapterSettings::AnthropicMessages(MessagesAdapterSettings {
            account: MessagesAccount::ClaudeCodeSubscription,
        })
    );
    assert!(route.headers.is_empty(), "no static headers on this route");
    // Every shipped Claude profile is reachable by its own name; a dated snapshot would
    // be a different `wire_model` here, not a different profile.
    for id in [
        "claude-fable-5",
        "claude-opus-4-6",
        "claude-opus-5",
        "claude-opus-5-5",
        "claude-sonnet-4-6",
        "claude-sonnet-5",
    ] {
        assert_eq!(route.binding(id).expect("served").wire_model, id);
    }
}

#[test]
fn an_unknown_account_behaviour_is_a_load_error() {
    let root = tempdir().unwrap();
    // The layout the host looks in: `<environments>/../routes`.
    std::fs::create_dir_all(root.path().join("environments")).unwrap();
    let routes = root.path().join("routes");
    std::fs::create_dir_all(&routes).unwrap();
    let text = std::fs::read_to_string(repo("routes/anthropic-subscription.toml"))
        .expect("the shipped route file")
        .replace("\"claude-code-subscription\"", "\"codex-subscription\"");
    std::fs::write(routes.join("anthropic-subscription.toml"), text).unwrap();
    let error = load_route_by_id(
        &[root.path().join("environments")],
        "anthropic-subscription",
    )
    .expect_err("an unimplemented account behaviour must not load");
    assert!(
        error.contains("claude-code-subscription"),
        "the error names the implemented one: {error}"
    );
}

// ------------------------------------------------------------------- conformance

fn invalid_request() -> ProviderRequest {
    ProviderRequest {
        system_prompt: "conformance prompt".into(),
        history: vec![Item::User { text: "hi".into() }],
        tools: vec![ToolDeclaration {
            name: "apply_patch".into(),
            description: "freeform".into(),
            kind: DeclarationKind::Freeform { grammar: None },
        }],
        options: ModelOptions::default(),
    }
}

fn conformance_build(transport: ScriptedTransport) -> Arc<dyn Provider> {
    provider_of(&composed("claude"), transport)
}

fn conformance_follow_up(request: &ProviderRequest) -> serde_json::Value {
    let composed = composed("claude");
    let binding = composed
        .route
        .binding(&composed.profile.id)
        .expect("the route serves this profile");
    let route = messages_route(&composed.route).expect("the route is a Messages route");
    build_request(&route, &binding.wire_model, &composed.profile, request)
        .expect("the follow-up request builds")
}

/// The shared provider conformance suite over the SHIPPED composed route: the provider
/// comes from `environments/claude` → `routes/anthropic-subscription.toml` →
/// `profiles/claude-sonnet-5.toml`; the only inputs this file supplies are a scripted
/// transport and a fake credential source.
#[test]
fn the_shipped_messages_route_passes_the_conformance_suite() {
    run_all(&RouteUnderTest {
        name: "anthropic-messages/claude-subscription",
        build: conformance_build,
        fixtures: RouteFixtures {
            text_turn: messages_fixtures::text_turn,
            tool_call_turn: messages_fixtures::tool_call_turn,
            two_tool_calls: messages_fixtures::two_tool_calls,
            truncated_tool_call: messages_fixtures::truncated_tool_call,
            invalid_tool_json: messages_fixtures::invalid_tool_json,
            error_event: messages_fixtures::error_event,
            no_usage: messages_fixtures::no_usage,
            reasoning_turn: messages_fixtures::reasoning_turn,
            events_after_terminal: messages_fixtures::events_after_terminal,
        },
        follow_up_request: conformance_follow_up,
        fake_bearer: BEARER,
        invalid_request,
    });
}

// ------------------------------------------------------------- the shipped environments

/// `p1 env show NAME` through the real CLI and catalog: the delegating environment
/// needs the `env show` worker stub, so both shipped environments go this way.
fn show_env(name: &str) -> (i32, String, String) {
    let mut harness = Harness::new(vec![shipped_environments()], &[]);
    common::isolated_environment(&mut harness);
    let code = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap()
        .block_on(common::run_args(&mut harness, &["env", "show", name]));
    (code, harness.stdout.text(), harness.stderr.text())
}

#[test]
fn the_two_shipped_claude_environments_assemble_through_the_catalog_unchanged() {
    // `claude-delegating` names the `worker_*` tools, which only the `delegation`
    // feature registers. With it both environments assemble unchanged; without it
    // `claude-delegating` cannot assemble at all, and the error says exactly which
    // module is missing, while `claude` is untouched.
    #[cfg(feature = "delegation")]
    let assembled_here = ["claude", "claude-delegating"];
    #[cfg(not(feature = "delegation"))]
    let assembled_here = ["claude"];

    for name in assembled_here {
        let (code, stdout, stderr) = show_env(name);
        assert_eq!(code, 0, "{name}: {stderr}");
        let resolved: serde_json::Value = common::env_show_json(&stdout);
        assert_eq!(resolved["environment"], name, "{name}");
        assert_eq!(resolved["family"], "claude", "{name}");
        assert_eq!(
            resolved["route"]["origin"],
            serde_json::json!({
                "route": TODAYS_ORIGIN_ROUTE,
                "model": "claude-sonnet-5",
            }),
            "{name}: the origin string is byte for byte the pre-split one"
        );
        assert_eq!(
            resolved["route"]["mandatory_prompt_prefix"],
            "You are Claude Code, Anthropic's official CLI for Claude.",
            "{name}"
        );
        assert_eq!(resolved["route"]["cache_key"], "unsupported", "{name}");
    }

    #[cfg(not(feature = "delegation"))]
    {
        let (code, _stdout, stderr) = show_env("claude-delegating");
        assert_ne!(code, 0, "claude-delegating needs the delegation feature");
        assert!(
            stderr.starts_with("unknown tool module `worker_start`; available: "),
            "the error names the compiled-out module: {stderr}"
        );
    }

    // And the provider the catalog builds for `claude` reports the same origin.
    let assembled = assemble_shipped("claude");
    assert_eq!(
        assembled.provider.describe().origin,
        Origin {
            route: TODAYS_ORIGIN_ROUTE.into(),
            model: "claude-sonnet-5".into(),
        }
    );
    assert!(!assembled.provider.describe().supports_freeform_tools);
}

#[test]
fn every_shipped_claude_profile_carries_its_thinking_policy_and_budgets() {
    for (id, thinking) in [
        ("claude-fable-5", ThinkingPolicy::EffortLevel),
        ("claude-opus-5", ThinkingPolicy::EffortLevel),
        ("claude-opus-5-5", ThinkingPolicy::EffortLevel),
        ("claude-sonnet-5", ThinkingPolicy::EffortLevel),
        ("claude-opus-4-6", ThinkingPolicy::Budget),
        ("claude-sonnet-4-6", ThinkingPolicy::Budget),
    ] {
        let profile = shipped_profile(id);
        assert_eq!(profile.thinking, thinking, "{id}");
        assert_eq!(profile.family, "claude", "{id}");
        assert_eq!(
            profile.efforts,
            vec![
                Effort::Low,
                Effort::Medium,
                Effort::High,
                Effort::ExtraHigh,
                Effort::Max
            ],
            "{id}"
        );
        // Spec §7.1: an effort-less request carries no thinking fields, so no default.
        assert_eq!(profile.default_effort, None, "{id}");
        // Unknown capacity is stated nowhere, never as zero.
        assert_eq!(profile.context_tokens, None, "{id}");
        assert_eq!(profile.max_output_tokens, None, "{id}");
        if thinking == ThinkingPolicy::Budget {
            // The numbers the adapter used to compile, unchanged.
            assert_eq!(profile.budget_for(Effort::Low), Some(4_096), "{id}");
            assert_eq!(profile.budget_for(Effort::Medium), Some(10_240), "{id}");
            assert_eq!(profile.budget_for(Effort::High), Some(20_480), "{id}");
            assert_eq!(profile.budget_for(Effort::ExtraHigh), Some(32_768), "{id}");
            assert_eq!(profile.budget_for(Effort::Max), Some(32_768), "{id}");
        } else {
            assert!(profile.thinking_budgets.is_empty(), "{id}");
        }
    }
}

/// The parse/validate table of the two new variants and the budget table, through the
/// file loader the host uses.
#[test]
fn a_profile_file_whose_thinking_policy_is_unusable_is_a_load_error() {
    let cases = [
        // An unknown spelling is rejected by the enum, not ignored.
        (
            "id = \"x\"\nrevision = 1\nmodel_id = \"x\"\nfamily = \"claude\"\n\
             thinking = \"adaptive\"\nefforts = [\"low\"]\n",
            "adaptive",
        ),
        // `budget` without the table, and a table under another variant.
        (
            "id = \"x\"\nrevision = 1\nmodel_id = \"x\"\nfamily = \"claude\"\n\
             thinking = \"budget\"\nefforts = [\"low\"]\n",
            "per listed effort",
        ),
        (
            "id = \"x\"\nrevision = 1\nmodel_id = \"x\"\nfamily = \"claude\"\n\
             thinking = \"effort-level\"\nefforts = [\"low\"]\n\n[thinking_budgets]\nlow = 4096\n",
            "thinking_budgets",
        ),
        // A table entry for an effort the profile does not list.
        (
            "id = \"x\"\nrevision = 1\nmodel_id = \"x\"\nfamily = \"claude\"\n\
             thinking = \"budget\"\nefforts = [\"low\"]\n\n[thinking_budgets]\nlow = 4096\nhigh = 20480\n",
            "per listed effort",
        ),
        // Below the API floor.
        (
            "id = \"x\"\nrevision = 1\nmodel_id = \"x\"\nfamily = \"claude\"\n\
             thinking = \"budget\"\nefforts = [\"low\"]\n\n[thinking_budgets]\nlow = 1023\n",
            "1024",
        ),
    ];
    for (text, named) in cases {
        let error =
            ModelProfile::from_toml("x", text).expect_err(&format!("{named} must be rejected"));
        assert!(
            error.contains(named),
            "{named}: the error names the problem: {error}"
        );
    }
    // The valid shape of the same file parses, with one budget per listed effort.
    let profile = ModelProfile::from_toml(
        "x",
        "id = \"x\"\nrevision = 1\nmodel_id = \"x\"\nfamily = \"claude\"\nthinking = \"budget\"\n\
         efforts = [\"low\", \"high\"]\n\n[thinking_budgets]\nlow = 4096\nhigh = 20480\n",
    )
    .expect("a budget profile with one budget per listed effort is valid");
    assert_eq!(profile.budget_for(Effort::Low), Some(4_096));
    assert_eq!(profile.budget_for(Effort::High), Some(20_480));
    assert_eq!(profile.budget_for(Effort::Medium), None);
    assert_eq!(profile.thinking_budgets.len(), 2, "the table is the point");
}

// -------------------------------------------------------------------------- resume

#[test]
fn a_recorded_session_with_todays_origin_still_resumes() {
    let workspace = tempdir().unwrap();
    let path = workspace.path().join("session.jsonl");
    std::fs::write(&path, RECORDED_SESSION).unwrap();
    let loaded = p1_journal::load(&path).expect("the recorded session loads");
    assert!(loaded.truncated_tail.is_none());

    let recorded_origin = match &loaded.records[0].body {
        RecordBody::Environment { route, .. } => route.origin.clone(),
        other => panic!("the first record is the environment: {other:?}"),
    };
    assert_eq!(
        recorded_origin,
        Origin {
            route: TODAYS_ORIGIN_ROUTE.into(),
            model: "claude-sonnet-5".into(),
        }
    );

    let composed = composed("claude");
    let provider = provider_of(&composed, ScriptedTransport::new(Vec::new()));
    let parts = AgentParts {
        provider,
        tools: Vec::new(),
        system_prompt: String::new(),
        options: ModelOptions::default(),
        context: Arc::new(PassthroughContext),
        authorization: Arc::new(ScriptedAuthorization::permit_all()),
        journal: Arc::new(RecordingJournal::new()),
        events: Arc::new(RecordingEvents::new()),
    };
    let (_agent, report) = Agent::resume(parts, &loaded.records)
        .expect("a session recorded on today's origin resumes on the composed route");
    assert!(
        !report.environment_changed,
        "the recorded environment is exactly what the shipped files compose today"
    );
    assert!(report.unresolved_calls.is_empty());
}

// ------------------------------------------------- refused adapter x profile variant

/// A synthetic root with `environments/`, `routes/` and `profiles/` next to each other,
/// exactly the layout the host looks in.
struct Scratch {
    root: tempfile::TempDir,
}

impl Scratch {
    fn new() -> Self {
        let root = tempdir().unwrap();
        for dir in ["environments", "routes", "profiles"] {
            std::fs::create_dir_all(root.path().join(dir)).unwrap();
        }
        Self { root }
    }

    fn dirs(&self) -> Vec<PathBuf> {
        vec![self.root.path().join("environments")]
    }

    /// Copy one shipped route file into the scratch root.
    fn copy_route(&self, id: &str) {
        std::fs::copy(
            repo(&format!("routes/{id}.toml")),
            self.root.path().join("routes").join(format!("{id}.toml")),
        )
        .unwrap();
    }

    fn write_profile(&self, id: &str, text: &str) {
        std::fs::write(
            self.root.path().join("profiles").join(format!("{id}.toml")),
            text,
        )
        .unwrap();
    }

    fn write_environment(&self, name: &str, route: &str, profile: &str) {
        let dir = self.root.path().join("environments").join(name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("environment.toml"),
            format!("route = \"{route}\"\nprofile = \"{profile}\"\n"),
        )
        .unwrap();
        std::fs::write(dir.join("prompt.md"), "synthetic prompt").unwrap();
    }

    /// The host's own order: build the catalog, load the environment, resolve it,
    /// assemble.
    fn assemble(&self, name: &str) -> Result<Assembled, AssemblyError> {
        let dirs = self.dirs();
        let harness = Harness::new(dirs.clone(), &[]);
        let completion = Arc::new(CompletionHub::new());
        let catalog = build_catalog(&harness.deps, SandboxMode::Off, &[], &[], &[], &completion)
            .expect("the scratch routes are valid");
        let mut environment = load_environment(name, &dirs).expect("the environment loads");
        resolve_environment(&mut environment, &dirs).expect("the route serves the profile");
        let workspace = tempdir().unwrap();
        assemble(
            &catalog,
            &environment,
            workspace.path(),
            &Substitutions {
                workspace: workspace.path().display().to_string(),
                date: "2026-09-20".into(),
                os: "linux".into(),
            },
        )
    }
}

fn profile_file(id: &str, thinking: &str, table: &str) -> String {
    format!(
        "id = \"{id}\"\nrevision = 1\nmodel_id = \"{id}\"\nfamily = \"claude\"\n\
         thinking = \"{thinking}\"\n\
         efforts = [\"low\", \"medium\", \"high\", \"extra_high\", \"max\"]\n{table}"
    )
}

fn budget_table() -> &'static str {
    "\n[thinking_budgets]\nlow = 4096\nmedium = 10240\nhigh = 20480\n\
     extra_high = 32768\nmax = 32768\n"
}

/// Each REFUSED adapter × profile-variant pair fails ASSEMBLY, naming the variant and
/// the adapter (spec §7.1/§7.4). The route files are the shipped ones.
#[test]
fn assembly_refuses_each_adapter_and_profile_variant_pair() {
    // `anthropic-subscription` binds `claude-sonnet-5`; `glm-subscription` binds `glm-5.3`.
    let cases = [
        (
            "anthropic-subscription",
            "claude-sonnet-5",
            "enabled",
            "",
            "enabled",
            "Messages adapter",
        ),
        (
            "anthropic-subscription",
            "claude-sonnet-5",
            "preserved",
            "",
            "preserved",
            "Messages adapter",
        ),
        (
            "glm-subscription",
            "glm-5.3",
            "effort-level",
            "",
            "effort-level",
            "retained-thinking",
        ),
        (
            "glm-subscription",
            "glm-5.3",
            "budget",
            budget_table(),
            "budget",
            "retained-thinking",
        ),
    ];
    for (route, profile_id, thinking, table, named, adapter) in cases {
        let scratch = Scratch::new();
        scratch.copy_route(route);
        scratch.write_profile(profile_id, &profile_file(profile_id, thinking, table));
        scratch.write_environment("refused", route, profile_id);

        let error = scratch
            .assemble("refused")
            .expect_err("this pair cannot be composed");
        let message = error.to_string();
        match &error {
            AssemblyError::FactoryFailed { what, message } => {
                assert_eq!(what, &format!("provider `{route}`"), "{thinking}");
                for part in [named, adapter] {
                    assert!(message.contains(part), "{thinking}: {message}");
                }
            }
            other => panic!("expected FactoryFailed for {thinking}, got {other:?}"),
        }
        assert!(message.contains(route), "{thinking}: {message}");
    }
}

/// The same four pairs, but posed as a REQUEST: `assemble` is the only thing that
/// refuses them, because the profile itself is valid.
#[test]
fn the_refused_profiles_are_valid_on_their_own() {
    for (thinking, table) in [
        ("enabled", ""),
        ("preserved", ""),
        ("effort-level", ""),
        ("budget", budget_table()),
    ] {
        let text = profile_file("synthetic", thinking, table);
        ModelProfile::from_toml("synthetic", &text)
            .unwrap_or_else(|error| panic!("{thinking} is a valid profile: {error}"));
    }
}

/// A route file the adapter key cannot serve: a Messages route whose endpoint the
/// adapter refuses fails assembly too (the route data is validated where it is used).
#[test]
fn assembly_refuses_a_messages_route_without_https() {
    let scratch = Scratch::new();
    scratch.copy_route("anthropic-subscription");
    let path = scratch
        .root
        .path()
        .join("routes/anthropic-subscription.toml");
    let text = std::fs::read_to_string(&path)
        .unwrap()
        .replace("https://api.anthropic.com", "http://api.anthropic.com");
    std::fs::write(&path, text).unwrap();
    scratch.write_profile(
        "claude-sonnet-5",
        &profile_file("claude-sonnet-5", "effort-level", ""),
    );
    scratch.write_environment("plain", "anthropic-subscription", "claude-sonnet-5");

    let error = scratch
        .assemble("plain")
        .expect_err("a plain-HTTP Messages route must not compose");
    assert!(error.to_string().contains("HTTPS"), "{error}");
}

/// The conformance suite's own fixtures echo a dated alias; the composed provider is
/// configured `claude-sonnet-5`, so this pins that origin is the configured model.
#[test]
fn the_shipped_route_is_configured_as_the_wire_model_not_the_echo() {
    assert!(messages_fixtures::reasoning_turn.contains("claude-sonnet-4-6"));
    assert_eq!(composed("claude").wire_model, "claude-sonnet-5");
    let provider = provider_of(&composed("claude"), ScriptedTransport::new(vec![]));
    assert_eq!(provider.describe().origin.model, "claude-sonnet-5");
    assert_eq!(provider.describe().origin.route, TODAYS_ORIGIN_ROUTE);
}
