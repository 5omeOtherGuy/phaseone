//! The Responses adapter composed from the SHIPPED files (ADR-0039 step 4b, spec
//! §7.2–7.4): `environments/gpt` → `routes/openai-codex-subscription.toml` →
//! `profiles/gpt-*.toml`, through the host's own loading path. Nothing here is
//! hand-made except the scripted transport and the fake credential source, and no
//! test touches a credential file or the network.

mod common;

#[path = "../../p1-provider-openai/tests/fixtures/mod.rs"]
mod responses_fixtures;

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
use p1_host::catalog::{build_catalog, resolve_environment, responses_route, route_provider};
use p1_host::cli::SandboxMode;
use p1_host::routes::{AdapterSettings, RouteFile, load_route_by_id};
use p1_model_profile::{ModelProfile, ThinkingPolicy};
use p1_provider_conformance::{RouteFixtures, RouteUnderTest, run_all};
use p1_provider_http::testing::{RefusingWsConnector, ScriptedTransport};
use p1_provider_http::{Credential, CredentialSource};
use p1_provider_openai::{
    ROUTE, ResponsesAccount, ResponsesAdapterSettings, ResponsesTransport, build_request,
};
use p1_testkit::{PassthroughContext, RecordingEvents, RecordingJournal, ScriptedAuthorization};
use tempfile::tempdir;

/// The bearer the fake credential source returns. Route files hold a credential
/// REFERENCE, so nothing here reads a credential file. This account also needs the
/// ChatGPT account id the route's `store: false` behaviour bills with.
const BEARER: &str = "CODEX-ROUTE-FAKE-BEARER";
const ACCOUNT_ID: &str = "codex-route-fake-account";

/// Byte for byte what the pre-split adapter wrote into every `Origin.route`. A session
/// recorded then must resume after ADR-0039 step 4b (ADR-0033).
const TODAYS_ORIGIN_ROUTE: &str = "openai-responses/codex-subscription";

/// A session file recorded BEFORE this step: the format header, the environment
/// record (with today's origin) and the user input of that turn.
const RECORDED_SESSION: &str = concat!(
    "{\"p1_journal\":1}\n",
    "{\"seq\":0,\"record\":\"environment\",\"route\":{\"origin\":{\"route\":",
    "\"openai-responses/codex-subscription\",\"model\":\"gpt-5.6-sol\"},",
    "\"supports_freeform_tools\":true,\"mandatory_prompt_prefix\":null,",
    "\"reports_cost\":false,\"cache_key\":\"optional\"},\"system_prompt\":\"\",",
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
                account_id: Some(ACCOUNT_ID.into()),
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
                account_id: Some(ACCOUNT_ID.into()),
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
/// is injected next to the transport (ADR-0047 §1): it REFUSES every upgrade, so the
/// shipped route — which asks for WebSocket — falls back to SSE at once and this
/// scripted transport serves every request. No test opens a socket.
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
fn the_shipped_responses_route_holds_what_the_host_used_to_compile() {
    assert!(
        !p1_host::catalog::WHOLE_PROVIDERS.contains(&"openai-codex-subscription"),
        "the Responses key is a route file now, not a whole provider"
    );
    assert!(
        p1_host::catalog::WHOLE_PROVIDERS.is_empty(),
        "no shipped provider is whole any more"
    );
    let dirs = environment_dirs();
    let route =
        load_route_by_id(&dirs, "openai-codex-subscription").expect("the shipped route file");
    assert_eq!(route.origin_route, TODAYS_ORIGIN_ROUTE);
    assert_eq!(
        route.origin_route, ROUTE,
        "the adapter's named constant is the file's string"
    );
    assert_eq!(route.adapter, "openai-responses");
    assert_eq!(route.endpoint, "https://chatgpt.com/backend-api");
    assert_eq!(route.credential.kind, p1_auth::CredentialKind::CodexOauth);
    assert_eq!(
        route.settings().expect("the adapter parses its settings"),
        AdapterSettings::OpenAiResponses(ResponsesAdapterSettings {
            account: ResponsesAccount::CodexSubscription,
            // ADR-0047 §1 (owner decision 2026-09-21): WebSocket is the default
            // wherever a route supports it, so the shipped Codex route asks for it.
            transport: ResponsesTransport::Websocket,
        })
    );
    assert!(route.headers.is_empty(), "no static headers on this route");
    // Every shipped GPT profile is reachable by its own name; a dated snapshot would
    // be a different `wire_model` here, not a different profile.
    for id in [
        "gpt-6-astra",
        "gpt-6-sol",
        "gpt-6-luna",
        "gpt-5.6-sol",
        "gpt-5.6-terra",
        "gpt-5.6-luna",
        "gpt-5.5",
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
    let text = std::fs::read_to_string(repo("routes/openai-codex-subscription.toml"))
        .expect("the shipped route file")
        .replace("\"codex-subscription\"", "\"claude-code-subscription\"");
    std::fs::write(routes.join("openai-codex-subscription.toml"), text).unwrap();
    let error = load_route_by_id(
        &[root.path().join("environments")],
        "openai-codex-subscription",
    )
    .expect_err("an unimplemented account behaviour must not load");
    assert!(
        error.contains("codex-subscription"),
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
        // This account's route carries no output-cap field (`max_output_tokens` is a
        // 400 on the wire).
        options: ModelOptions {
            max_output_tokens: Some(1_000),
            ..ModelOptions::default()
        },
    }
}

fn conformance_build(transport: ScriptedTransport) -> Arc<dyn Provider> {
    provider_of(&composed("gpt"), transport)
}

fn conformance_follow_up(request: &ProviderRequest) -> serde_json::Value {
    let composed = composed("gpt");
    let binding = composed
        .route
        .binding(&composed.profile.id)
        .expect("the route serves this profile");
    let route = responses_route(&composed.route).expect("the route is a Responses route");
    build_request(&route, &binding.wire_model, &composed.profile, request)
        .expect("the follow-up request builds")
}

/// The shared provider conformance suite over the SHIPPED composed route: the provider
/// comes from `environments/gpt` → `routes/openai-codex-subscription.toml` →
/// `profiles/gpt-5.6-sol.toml`; the only inputs this file supplies are a scripted
/// transport and a fake credential source.
#[test]
fn the_shipped_responses_route_passes_the_conformance_suite() {
    run_all(&RouteUnderTest {
        name: "openai-responses/codex-subscription",
        build: conformance_build,
        fixtures: RouteFixtures {
            text_turn: responses_fixtures::TEXT_TURN,
            tool_call_turn: responses_fixtures::TOOL_CALL_TURN,
            two_tool_calls: responses_fixtures::TWO_TOOL_CALLS,
            truncated_tool_call: responses_fixtures::TRUNCATED_TOOL_CALL,
            invalid_tool_json: responses_fixtures::INVALID_TOOL_JSON,
            error_event: responses_fixtures::ERROR_EVENT,
            no_usage: responses_fixtures::NO_USAGE,
            reasoning_turn: responses_fixtures::REASONING_TURN,
            events_after_terminal: responses_fixtures::EVENTS_AFTER_TERMINAL,
        },
        follow_up_request: conformance_follow_up,
        fake_bearer: BEARER,
        invalid_request,
    });
}

// ------------------------------------------------------------- the shipped environment

/// `p1 env show NAME` through the real CLI and catalog.
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
fn the_shipped_gpt_environment_assembles_through_the_catalog_unchanged() {
    let (code, stdout, stderr) = show_env("gpt");
    assert_eq!(code, 0, "{stderr}");
    let resolved: serde_json::Value = common::env_show_json(&stdout);
    assert_eq!(resolved["environment"], "gpt");
    assert_eq!(resolved["family"], "gpt");
    assert_eq!(
        resolved["route"]["origin"],
        serde_json::json!({
            "route": TODAYS_ORIGIN_ROUTE,
            "model": "gpt-5.6-sol",
        }),
        "the origin string is byte for byte the pre-split one"
    );
    assert_eq!(resolved["route"]["supports_freeform_tools"], true);
    assert_eq!(resolved["route"]["reports_cost"], false);
    assert_eq!(resolved["route"]["cache_key"], "optional");
    assert!(
        resolved["route"]["mandatory_prompt_prefix"].is_null(),
        "this route forces no prompt prefix: {}",
        resolved["route"]
    );

    // And the provider the catalog builds for `gpt` reports the same origin.
    let assembled = assemble_shipped("gpt");
    assert_eq!(
        assembled.provider.describe().origin,
        Origin {
            route: TODAYS_ORIGIN_ROUTE.into(),
            model: "gpt-5.6-sol".into(),
        }
    );
    assert!(assembled.provider.describe().supports_freeform_tools);
}

#[test]
fn every_shipped_gpt_profile_carries_its_thinking_policy() {
    // Earlier profiles follow the Codex CLI cache (2026-09-21); GPT-6 Sol and Luna
    // follow OpenAI's model docs. `ultra` has no p1 effort, so no profile lists it.
    for (id, efforts) in [
        (
            "gpt-6-astra",
            vec![
                Effort::Low,
                Effort::Medium,
                Effort::High,
                Effort::ExtraHigh,
                Effort::Max,
            ],
        ),
        (
            "gpt-6-sol",
            vec![
                Effort::Low,
                Effort::Medium,
                Effort::High,
                Effort::ExtraHigh,
                Effort::Max,
            ],
        ),
        (
            "gpt-6-luna",
            vec![
                Effort::Low,
                Effort::Medium,
                Effort::High,
                Effort::ExtraHigh,
                Effort::Max,
            ],
        ),
        (
            "gpt-5.6-sol",
            vec![
                Effort::Low,
                Effort::Medium,
                Effort::High,
                Effort::ExtraHigh,
                Effort::Max,
            ],
        ),
        (
            "gpt-5.6-terra",
            vec![
                Effort::Low,
                Effort::Medium,
                Effort::High,
                Effort::ExtraHigh,
                Effort::Max,
            ],
        ),
        (
            "gpt-5.6-luna",
            vec![
                Effort::Low,
                Effort::Medium,
                Effort::High,
                Effort::ExtraHigh,
                Effort::Max,
            ],
        ),
        (
            "gpt-5.5",
            vec![Effort::Low, Effort::Medium, Effort::High, Effort::ExtraHigh],
        ),
    ] {
        let profile = shipped_profile(id);
        assert_eq!(profile.thinking, ThinkingPolicy::EffortLevel, "{id}");
        assert_eq!(profile.family, "gpt", "{id}");
        assert_eq!(
            profile.efforts, efforts,
            "{id}: the profile lists the supported efforts"
        );
        // Spec §7.1: an effort-less request carries no reasoning fields, so no default.
        assert_eq!(profile.default_effort, None, "{id}");
        assert!(profile.thinking_budgets.is_empty(), "{id}");
        // Unknown capacity is stated nowhere, never as zero.
        assert_eq!(profile.context_tokens, None, "{id}");
        assert_eq!(profile.max_output_tokens, None, "{id}");
    }
}

#[test]
fn new_gpt_6_models_build_requests_with_their_wire_ids_and_efforts() {
    let route = load_route_by_id(&environment_dirs(), "openai-codex-subscription").unwrap();
    let responses = responses_route(&route).unwrap();
    let mut request = invalid_request();
    request.options.max_output_tokens = None;

    for id in ["gpt-6-sol", "gpt-6-luna"] {
        let profile = shipped_profile(id);
        for (effort, wire) in [(Effort::ExtraHigh, "xhigh"), (Effort::Max, "max")] {
            request.options.reasoning_effort = Some(effort);
            let body = build_request(&responses, id, &profile, &request).unwrap();
            assert_eq!(body["model"], id);
            assert_eq!(body["reasoning"]["effort"], wire);
        }
    }
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
            model: "gpt-5.6-sol".into(),
        }
    );

    let composed = composed("gpt");
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
        "id = \"{id}\"\nrevision = 1\nmodel_id = \"{id}\"\nfamily = \"gpt\"\n\
         thinking = \"{thinking}\"\n\
         efforts = [\"low\", \"medium\", \"high\"]\n{table}"
    )
}

fn budget_table() -> &'static str {
    "\n[thinking_budgets]\nlow = 4096\nmedium = 10240\nhigh = 20480\n"
}

/// Each REFUSED adapter × profile-variant pair fails ASSEMBLY, naming the variant and
/// the adapter (spec §7.1/§7.4). The route file is the shipped one.
#[test]
fn assembly_refuses_each_adapter_and_profile_variant_pair() {
    // `openai-codex-subscription` binds every GPT profile the host ships.
    let cases = [
        ("budget", budget_table()),
        ("enabled", ""),
        ("preserved", ""),
    ];
    for (thinking, table) in cases {
        let scratch = Scratch::new();
        scratch.copy_route("openai-codex-subscription");
        scratch.write_profile("gpt-5.6-sol", &profile_file("gpt-5.6-sol", thinking, table));
        scratch.write_environment("refused", "openai-codex-subscription", "gpt-5.6-sol");

        let error = scratch
            .assemble("refused")
            .expect_err("this pair cannot be composed");
        let message = error.to_string();
        match &error {
            AssemblyError::FactoryFailed { what, message } => {
                assert_eq!(what, "provider `openai-codex-subscription`", "{thinking}");
                for part in [thinking, "Responses adapter", "effort-level"] {
                    assert!(message.contains(part), "{thinking}: {message}");
                }
            }
            other => panic!("expected FactoryFailed for {thinking}, got {other:?}"),
        }
        assert!(
            message.contains("openai-codex-subscription"),
            "{thinking}: {message}"
        );
    }
}

/// The same three variants, but posed as data: `assemble` is the only thing that
/// refuses them, because each profile is valid on its own.
#[test]
fn the_refused_profiles_are_valid_on_their_own() {
    for (thinking, table) in [
        ("effort-level", ""),
        ("budget", budget_table()),
        ("enabled", ""),
        ("preserved", ""),
    ] {
        let text = profile_file("synthetic", thinking, table);
        ModelProfile::from_toml("synthetic", &text)
            .unwrap_or_else(|error| panic!("{thinking} is a valid profile: {error}"));
    }
}

// ------------------------------------------------------------- the wrong form, twice

/// The new form on a ROUTED key: an environment that still writes the pre-split
/// `provider`/`model`/`family` fails assembly, naming the form to write instead.
#[test]
fn a_routed_key_in_the_old_form_is_refused_and_says_which_form_to_write() {
    let scratch = Scratch::new();
    scratch.copy_route("openai-codex-subscription");
    let dir = scratch.root.path().join("environments").join("legacy");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("environment.toml"),
        "family = \"gpt\"\nprovider = \"openai-codex-subscription\"\nmodel = \"gpt-5.6-sol\"\n",
    )
    .unwrap();
    std::fs::write(dir.join("prompt.md"), "synthetic prompt").unwrap();

    let dirs = scratch.dirs();
    let harness = Harness::new(dirs.clone(), &[]);
    let completion = Arc::new(CompletionHub::new());
    let catalog = build_catalog(&harness.deps, SandboxMode::Off, &[], &[], &[], &completion)
        .expect("the scratch route is valid");
    let environment = load_environment("legacy", &dirs).expect("the environment loads");
    let workspace = tempdir().unwrap();
    let error = assemble(
        &catalog,
        &environment,
        workspace.path(),
        &Substitutions {
            workspace: workspace.path().display().to_string(),
            date: "2026-09-20".into(),
            os: "linux".into(),
        },
    )
    .expect_err("the old form on a routed key must not assemble");
    let message = error.to_string();
    match &error {
        AssemblyError::FactoryFailed { what, .. } => {
            assert_eq!(what, "provider `openai-codex-subscription`")
        }
        other => panic!("expected FactoryFailed, got {other:?}"),
    }
    for form in ["`route`", "`profile`", "`provider`", "`model`", "`family`"] {
        assert!(message.contains(form), "{message}");
    }
}

/// A route file the adapter key cannot serve: a Responses route whose endpoint the
/// adapter refuses fails assembly too (the route data is validated where it is used).
#[test]
fn assembly_refuses_a_responses_route_without_https() {
    let scratch = Scratch::new();
    scratch.copy_route("openai-codex-subscription");
    let path = scratch
        .root
        .path()
        .join("routes/openai-codex-subscription.toml");
    let text = std::fs::read_to_string(&path).unwrap().replace(
        "https://chatgpt.com/backend-api",
        "http://chatgpt.com/backend-api",
    );
    std::fs::write(&path, text).unwrap();
    scratch.write_profile(
        "gpt-5.6-sol",
        &profile_file("gpt-5.6-sol", "effort-level", ""),
    );
    scratch.write_environment("plain", "openai-codex-subscription", "gpt-5.6-sol");

    let error = scratch
        .assemble("plain")
        .expect_err("a plain-HTTP Responses route must not compose");
    assert!(error.to_string().contains("HTTPS"), "{error}");
}

/// The Responses fixtures deliberately omit `model` from `response.completed`, so the
/// parser's origin falls back to the CONFIGURED model. This pins that the composed
/// provider is configured with the route file's binding, not with a fixture's echo.
#[test]
fn the_shipped_route_is_configured_as_the_wire_model() {
    assert!(
        !responses_fixtures::TEXT_TURN.contains("\"model\""),
        "the fixture must not name a model"
    );
    assert_eq!(composed("gpt").wire_model, "gpt-5.6-sol");
    let provider = provider_of(&composed("gpt"), ScriptedTransport::new(vec![]));
    assert_eq!(provider.describe().origin.model, "gpt-5.6-sol");
    assert_eq!(provider.describe().origin.route, TODAYS_ORIGIN_ROUTE);
}
