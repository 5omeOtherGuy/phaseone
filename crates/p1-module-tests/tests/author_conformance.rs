//! The author conformance run (ADR-0210): p1's checks on one module package built anywhere,
//! run by a module author against their own archive (the template's `conformance.sh`) and by
//! the gate against p1's own packages republished as another publisher's.
//!
//! `P1_CONFORMANCE_PACKAGE` names the `.p1pkg` archive under test. Without it, the run checks
//! the fixture tool as `acme/fixture` and the Chat Completions provider as `acme/chat`, so the
//! suite itself stays exercised. Every package is first installed with p1's own install path
//! into a scratch configuration and loaded through the lock and the installed sets, exactly as
//! a session loads it (ADR-0209): the loader's checks (kind, world, protocol, grants, the class
//! allocation, the digest) are the first part of conformance. Then, by class:
//!
//! - a **tool** is inspected on the restricted path, with no capability linked: its
//!   declaration has a valid name, a description and, for a function tool, an object schema;
//!   `effect`, `describe` and `describe-result` answer a sample call without a trap, each in
//!   its protocol form, with a verb from the closed vocabulary (`protocol.md`);
//! - a **provider** runs the whole provider conformance suite (`p1-provider-conformance`) over
//!   its route and model profile (`P1_CONFORMANCE_ROUTE`, `P1_CONFORMANCE_PROFILE`, files)
//!   and SSE fixtures: those of `P1_CONFORMANCE_FIXTURES` (a directory with `text_turn.sse`,
//!   `tool_call_turn.sse`, `two_tool_calls.sse`, `truncated_tool_call.sse`,
//!   `invalid_tool_json.sse`, `error_event.sse`, `no_usage.sse`, `reasoning_turn.sse` and
//!   `events_after_terminal.sse`), else p1's own for the route's adapter dialect;
//! - any other class is held to the loader's checks.
//!
//! No case opens a socket: the provider runs over the scripted transport.

use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use futures_util::StreamExt;
use p1_assembly::load_modules_lock;
use p1_contracts::serde_json::{self, Value};
use p1_contracts::{
    BoxFuture, CancellationToken, DeclarationKind, Item, ModelOptions, Provider, ProviderError,
    ProviderRequest, ToolCall, ToolDeclaration, ToolInput, ToolResultItem, ToolStatus,
};
use p1_host::catalog::modules::{installed_sets, load_locked_modules_in};
use p1_host::routes::{RouteFile, load_route};
use p1_module_protocol::{CALL_VERBS, WireCallDescription, WireResultDescription};
use p1_module_runtime::{
    ExecutionLimits, LoadedModule, ModuleKind, ProviderSettings, WasmProvider, inspect_tool,
};
use p1_module_tests::{pack, publish_as};
use p1_provider_conformance::fixtures::{anthropic, chat, responses};
use p1_provider_conformance::{
    RouteFixtures, RouteUnderTest, run_all, run_all_without_reasoning_replay,
};
use p1_provider_http::testing::{BodyEnd, ScriptedResponse, ScriptedTransport};
use p1_provider_http::{Credential, CredentialSource, HttpRequest};

const PACKAGE_VAR: &str = "P1_CONFORMANCE_PACKAGE";
const ROUTE_VAR: &str = "P1_CONFORMANCE_ROUTE";
const PROFILE_VAR: &str = "P1_CONFORMANCE_PROFILE";
const FIXTURES_VAR: &str = "P1_CONFORMANCE_FIXTURES";
const BEARER: &str = "AUTHOR-CONFORMANCE-FAKE-BEARER";

/// One package under test and, for a provider, what composes it.
struct Subject {
    archive: PathBuf,
    route: Option<PathBuf>,
    profile: Option<PathBuf>,
    fixtures: Option<PathBuf>,
}

fn env_path(name: &str) -> Option<PathBuf> {
    std::env::var_os(name)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

/// What the run checks: the author's archive, else p1's own packages republished in `scratch`.
fn subjects(scratch: &Path) -> Vec<Subject> {
    if let Some(archive) = env_path(PACKAGE_VAR) {
        return vec![Subject {
            archive,
            route: env_path(ROUTE_VAR),
            profile: env_path(PROFILE_VAR),
            fixtures: env_path(FIXTURES_VAR),
        }];
    }
    let catalog = p1_module_tests::catalog::route_catalog();
    let profile = scratch.join("gpt-5.6-sol.toml");
    std::fs::write(
        &profile,
        p1_module_tests::catalog::profile_text("gpt-5.6-sol"),
    )
    .expect("the profile");
    let route = scratch.join("openai-chat.toml");
    std::fs::copy(catalog.route_path("openai-chat"), &route).expect("the route");
    [
        ("p1-module-fixture", "acme/fixture", None),
        ("p1-module-provider-openai-chat", "acme/chat", Some(route)),
    ]
    .into_iter()
    .map(|(package, name, route)| {
        let build = publish_as(package, name, &scratch.join("builds"), |_| {}, |_| {});
        let archive = scratch.join(format!("{package}.p1pkg"));
        pack(&build, &archive);
        Subject {
            archive,
            profile: route.as_ref().map(|_| profile.clone()),
            route,
            fixtures: None,
        }
    })
    .collect()
}

#[test]
fn the_package_conforms() {
    let scratch = tempfile::tempdir().expect("scratch");
    for subject in subjects(scratch.path()) {
        let loaded = install_and_load(&subject.archive, scratch.path());
        println!(
            "author conformance: {} {} ({})",
            loaded.name(),
            loaded.digest(),
            loaded.kind().name()
        );
        match loaded.kind() {
            ModuleKind::Tool => check_tool(&loaded),
            ModuleKind::Provider => check_provider(loaded, &subject),
            other => println!(
                "author conformance: a {} package is held to the loader's checks",
                other.name()
            ),
        }
    }
}

/// Installs `archive` into a fresh user configuration below `scratch` with p1's install path
/// and loads it through the lock it wrote and the installed sets, over an empty release.
fn install_and_load(archive: &Path, scratch: &Path) -> LoadedModule {
    let config = tempfile::tempdir_in(scratch).expect("a user configuration");
    let installed = p1_host::module_install::install(archive, config.path(), true)
        .unwrap_or_else(|error| panic!("p1 modules install refuses the package: {error}"));
    let dirs = vec![config.path().join("environments")];
    let lock = load_modules_lock(&dirs).expect("the lock install wrote");
    // An empty release: the package can come from nowhere but the set install wrote. Written
    // here, not with the harness's `Release`, which reads p1's built fixture an author lacks.
    let release = tempfile::tempdir_in(scratch).expect("a release directory");
    let manifest = release.path().join("manifest.json");
    std::fs::write(
        &manifest,
        r#"{"format":"p1-release-manifest/1","components":[]}"#,
    )
    .expect("the release manifest");
    let mut packages =
        load_locked_modules_in(&lock, &installed_sets(&dirs, Some(&manifest)), &manifest)
            .unwrap_or_else(|error| panic!("the installed package does not load: {error}"));
    let package = packages
        .drain(..)
        .find(|package| package.module == installed.key)
        .expect("the installed key loads");
    package.loaded
}

// ------------------------------------------------------------------ tools

fn check_tool(loaded: &LoadedModule) {
    let name = loaded.name();
    let inspection = |call: &ToolCall| {
        let result = ToolResultItem {
            call_id: call.call_id.clone(),
            name: call.name.clone(),
            status: ToolStatus::Ok,
            content: "conformance".to_owned(),
        };
        inspect_tool(loaded, call, &result)
            .unwrap_or_else(|error| panic!("{name}: cannot be inspected: {error}"))
    };
    let probe = ToolCall {
        call_id: "c1".to_owned(),
        name: "probe".to_owned(),
        input: ToolInput::Text(String::new()),
    };
    let declaration = inspection(&probe)
        .declaration
        .unwrap_or_else(|reason| panic!("{name}: declaration: {reason}"));
    check_declaration(name, &declaration);

    let call = ToolCall {
        call_id: "c1".to_owned(),
        name: declaration.name.clone(),
        input: match declaration.kind {
            DeclarationKind::Function { .. } => ToolInput::Json("{}".to_owned()),
            DeclarationKind::Freeform { .. } => ToolInput::Text(String::new()),
        },
    };
    let inspected = inspection(&call);
    let effect = inspected
        .effect
        .unwrap_or_else(|reason| panic!("{name}: effect: {reason}"));
    assert!(
        ["read-only", "writes-files", "delegates", "executes"].contains(&effect.as_str()),
        "{name}: effect is {effect}"
    );
    let describe = inspected
        .describe
        .unwrap_or_else(|reason| panic!("{name}: describe: {reason}"));
    let description: WireCallDescription =
        serde_json::from_str(&describe).unwrap_or_else(|error| {
            panic!("{name}: describe is not a call description ({error}): {describe}")
        });
    assert!(
        CALL_VERBS.contains(&description.verb.as_str()),
        "{name}: verb {} is not one of {CALL_VERBS:?}",
        description.verb
    );
    let described = inspected
        .describe_result
        .unwrap_or_else(|reason| panic!("{name}: describe-result: {reason}"));
    let result: WireResultDescription = serde_json::from_str(&described).unwrap_or_else(|error| {
        panic!("{name}: describe-result is not a result description ({error}): {described}")
    });
    assert!(
        !result.summary.trim().is_empty(),
        "{name}: the result summary is empty"
    );
    println!(
        "author conformance: {name}: tool `{}` conforms",
        declaration.name
    );
}

fn check_declaration(name: &str, declaration: &ToolDeclaration) {
    let tool = &declaration.name;
    assert!(
        !tool.is_empty()
            && tool.len() <= 64
            && tool
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-'),
        "{name}: the tool name `{tool}` is not 1 to 64 ASCII letters, digits, `_` and `-`"
    );
    assert!(
        !declaration.description.trim().is_empty(),
        "{name}: the tool description is empty"
    );
    if let DeclarationKind::Function { input_schema } = &declaration.kind {
        assert_eq!(
            input_schema["type"], "object",
            "{name}: the input schema is not an object schema: {input_schema}"
        );
    }
}

// ------------------------------------------------------------------ providers

/// The provider under test, for the suite's `fn` hooks.
struct ProviderSubject {
    module: LoadedModule,
    settings: ProviderSettings,
}

static PROVIDER: OnceLock<ProviderSubject> = OnceLock::new();

fn check_provider(module: LoadedModule, subject: &Subject) {
    let name = module.name().to_owned();
    let required = |path: &Option<PathBuf>, variable: &str| {
        path.clone().unwrap_or_else(|| {
            panic!("{name} is a provider: set {variable} to its route or profile file")
        })
    };
    let route = load_route(&required(&subject.route, ROUTE_VAR))
        .unwrap_or_else(|error| panic!("{name}: the route: {error}"));
    let profile_path = required(&subject.profile, PROFILE_VAR);
    let profile = profile_path
        .file_stem()
        .and_then(|stem| stem.to_str())
        .expect("a profile file named <id>.toml")
        .to_owned();
    let profile_text = std::fs::read_to_string(&profile_path)
        .unwrap_or_else(|error| panic!("{}: {error}", profile_path.display()));
    let binding = route
        .binding(&profile)
        .unwrap_or_else(|error| panic!("{name}: the route does not bind {profile}: {error}"));
    let settings = ProviderSettings {
        origin_route: route.origin_route.clone(),
        endpoint: route.endpoint.clone(),
        model: profile.clone(),
        wire_model: binding.wire_model.clone(),
        adapter_settings: route.component_adapter_settings(binding, &profile, &profile_text),
    };
    let fixtures = match &subject.fixtures {
        Some(dir) => fixtures_from(dir),
        None => dialect_fixtures(&route),
    };
    assert!(
        PROVIDER.set(ProviderSubject { module, settings }).is_ok(),
        "one provider per run"
    );
    let under_test = RouteUnderTest {
        name: "author conformance",
        build,
        max_retries: 3,
        fixtures,
        follow_up_request: follow_up,
        fake_bearer: BEARER,
        invalid_request: if route.adapter == "openai-responses" {
            effort_off_request
        } else {
            freeform_request
        },
    };
    if route.adapter == "openai-chat" {
        run_all_without_reasoning_replay(&under_test);
    } else {
        run_all(&under_test);
    }
    println!(
        "author conformance: {name}: provider on route {} conforms",
        route.id
    );
}

fn build(transport: ScriptedTransport) -> Arc<dyn Provider> {
    let subject = PROVIDER.get().expect("the provider under test");
    Arc::new(
        WasmProvider::new(
            &subject.module,
            subject.settings.clone(),
            Arc::new(FixedCredentials),
            Arc::new(transport),
            ExecutionLimits::default(),
        )
        .unwrap_or_else(|error| panic!("the provider does not build: {error}")),
    )
}

/// The body the provider sends for `request`: its own lowering, as the broker recorded it.
fn follow_up(request: &ProviderRequest) -> Value {
    let transport = ScriptedTransport::new(vec![ScriptedResponse {
        status: 400,
        headers: Vec::new(),
        chunks: Vec::new(),
        end: BodyEnd::Eof,
    }]);
    let recorded = transport.clone();
    std::thread::scope(|scope| {
        scope
            .spawn(|| {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_time()
                    .start_paused(true)
                    .build()
                    .expect("a runtime");
                runtime.block_on(async {
                    let provider = build(transport);
                    if let Ok(mut stream) = provider
                        .stream(request.clone(), CancellationToken::new())
                        .await
                    {
                        while stream.next().await.is_some() {}
                    }
                });
            })
            .join()
            .expect("the follow-up thread");
    });
    let sent: Vec<HttpRequest> = recorded.requests();
    let first = sent.first().expect("the follow-up request was sent");
    serde_json::from_slice(&first.body).expect("a JSON request body")
}

struct FixedCredentials;

impl CredentialSource for FixedCredentials {
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

/// A freeform tool: a route whose wire carries function tools only refuses it.
fn freeform_request() -> ProviderRequest {
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

/// Reasoning effort off with an output cap: what p1's Responses routes refuse.
fn effort_off_request() -> ProviderRequest {
    ProviderRequest {
        system_prompt: "conformance prompt".into(),
        history: vec![Item::User { text: "hi".into() }],
        tools: Vec::new(),
        options: ModelOptions {
            max_output_tokens: Some(1000),
            reasoning_effort: Some(p1_contracts::Effort::Off),
            ..ModelOptions::default()
        },
    }
}

/// The nine fixtures of `dir`, one `<name>.sse` file each.
fn fixtures_from(dir: &Path) -> RouteFixtures {
    let read = |name: &str| -> &'static str {
        let path = dir.join(format!("{name}.sse"));
        let text = std::fs::read_to_string(&path)
            .unwrap_or_else(|error| panic!("the fixture {}: {error}", path.display()));
        Box::leak(text.into_boxed_str())
    };
    RouteFixtures {
        text_turn: read("text_turn"),
        tool_call_turn: read("tool_call_turn"),
        two_tool_calls: read("two_tool_calls"),
        truncated_tool_call: read("truncated_tool_call"),
        invalid_tool_json: read("invalid_tool_json"),
        error_event: read("error_event"),
        no_usage: read("no_usage"),
        reasoning_turn: read("reasoning_turn"),
        events_after_terminal: read("events_after_terminal"),
    }
}

/// p1's own fixtures for the dialect of `route`'s adapter.
fn dialect_fixtures(route: &RouteFile) -> RouteFixtures {
    match route.adapter.as_str() {
        "anthropic-messages" => RouteFixtures {
            text_turn: anthropic::text_turn,
            tool_call_turn: anthropic::tool_call_turn,
            two_tool_calls: anthropic::two_tool_calls,
            truncated_tool_call: anthropic::truncated_tool_call,
            invalid_tool_json: anthropic::invalid_tool_json,
            error_event: anthropic::error_event,
            no_usage: anthropic::no_usage,
            reasoning_turn: anthropic::reasoning_turn,
            events_after_terminal: anthropic::events_after_terminal,
        },
        "openai-responses" => RouteFixtures {
            text_turn: responses::TEXT_TURN,
            tool_call_turn: responses::TOOL_CALL_TURN,
            two_tool_calls: responses::TWO_TOOL_CALLS,
            truncated_tool_call: responses::TRUNCATED_TOOL_CALL,
            invalid_tool_json: responses::INVALID_TOOL_JSON,
            error_event: responses::ERROR_EVENT,
            no_usage: responses::NO_USAGE,
            reasoning_turn: responses::REASONING_TURN,
            events_after_terminal: responses::EVENTS_AFTER_TERMINAL,
        },
        "openai-chat" => RouteFixtures {
            text_turn: chat::TEXT_TURN,
            tool_call_turn: chat::TOOL_CALL_TURN,
            two_tool_calls: chat::TWO_TOOL_CALLS,
            truncated_tool_call: chat::TRUNCATED_TOOL_CALL,
            invalid_tool_json: chat::INVALID_TOOL_JSON,
            error_event: chat::ERROR_EVENT,
            no_usage: chat::NO_USAGE,
            reasoning_turn: chat::REASONING_TURN,
            events_after_terminal: chat::EVENTS_AFTER_TERMINAL,
        },
        other => panic!(
            "route adapter `{other}` has no p1 fixtures: set {FIXTURES_VAR} to a directory of the nine .sse fixtures"
        ),
    }
}
