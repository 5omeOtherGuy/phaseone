//! S4.7: the shared provider conformance suite over the built provider components.
//!
//! Each case is a shipped route file and model profile, composed as the host composes a
//! provider component (`RouteFile::component_adapter_settings`, ADR-0086), built into a
//! `WasmProvider` over the component `scripts/build-modules.sh` published, and run through
//! `p1_provider_conformance::run_all` with the scripted transport: every adapter, every
//! shipped chat dialect, Anthropic's long context on and off and the three endpoint shapes
//! of the Responses route. No case opens a socket.
//!
//! Next to the shared checks:
//! - wire parity with the native adapter the host builds from the same route file: the
//!   same URL and body, the same non-credential headers in order, the same credential
//!   headers present (the broker puts the credential first, ADR-0086);
//! - error parity: the component's `classify` gives the native parser's kind and text for
//!   the same response, and the broker sends exactly as many attempts, so an exhausted,
//!   unentitled or used-up account is never refreshed or retried (ADR-0046, ADR-0062);
//! - a seeded bug on the component path the suite must catch.
//!
//! The follow-up request of the replay checks is the component's own: the body the broker
//! sent for it, so the replay payload goes out of the component's decoder and back into
//! its lowering.

use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use futures_util::StreamExt;
use p1_contracts::serde_json::{self, Value, json};
use p1_contracts::{
    BoxFuture, CancellationToken, DeclarationKind, Item, ModelOptions, Outcome, Provider,
    ProviderError, ProviderRequest, StreamEvent, ToolDeclaration,
};
use p1_host::routes::{RouteFile, load_route};
use p1_model_profile::ModelProfile;
use p1_module_runtime::{ExecutionLimits, LoadedModule, ProviderSettings, WasmProvider};
use p1_module_tests::{Release, fixture_dir};
use p1_provider_conformance::fixtures::{anthropic, chat, responses};
use p1_provider_conformance::{
    RouteFixtures, RouteUnderTest, run_all, text_deltas_then_single_terminal,
};
use p1_provider_http::testing::{
    BodyEnd, ScriptedResponse, ScriptedTransport, ScriptedWsConnector,
};
use p1_provider_http::{Credential, CredentialSource, HttpRequest};

const BEARER: &str = "CONFORMANCE-FAKE-BEARER";
const ACCOUNT: &str = "acct-conformance";
/// The headers the broker attaches from the credential: compared by presence only.
const CREDENTIAL_HEADERS: [&str; 2] = ["authorization", "chatgpt-account-id"];

const ANTHROPIC: &str = "p1/provider-anthropic";
const OPENAI: &str = "p1/provider-openai";
const OPENAI_CHAT: &str = "p1/provider-openai-chat";

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// The provider components as the build published them, loaded through a release manifest
/// like every module: the harness's release directory, next to the fixture it finds there.
struct Built {
    _release: Release,
    modules: Vec<(&'static str, LoadedModule)>,
}

fn built() -> &'static Built {
    static BUILT: OnceLock<Built> = OnceLock::new();
    BUILT.get_or_init(|| {
        let published = fixture_dir()
            .parent()
            .expect("the published packages directory")
            .to_path_buf();
        let mut release = Release::empty();
        for (name, package) in [
            (ANTHROPIC, "p1-module-provider-anthropic"),
            (OPENAI, "p1-module-provider-openai"),
            (OPENAI_CHAT, "p1-module-provider-openai-chat"),
        ] {
            let dir = published.join(package);
            let read = |file: String| {
                let path = dir.join(file);
                std::fs::read(&path).unwrap_or_else(|error| {
                    panic!(
                        "the provider artifact {} is missing ({error}): run scripts/build-modules.sh first",
                        path.display()
                    )
                })
            };
            let manifest: Value =
                serde_json::from_slice(&read(format!("{package}.manifest.json")))
                    .expect("a package manifest");
            assert_eq!(manifest["name"], name, "{package}");
            let file = name.replace('/', "-");
            let entry = json!({
                "name": name,
                "digest": manifest["digest"],
                "path": format!("packages/{file}/{file}.wasm"),
                "kind": manifest["kind"],
                "world": manifest["world"],
                "protocol": manifest["protocol"],
                "capabilities": manifest["capabilities"],
                "variant": manifest["variant"],
            });
            release.add(entry, &read(format!("{package}.wasm")));
        }
        let loader = release.loader();
        let modules = [ANTHROPIC, OPENAI, OPENAI_CHAT]
            .into_iter()
            .map(|name| {
                let module = loader
                    .load(name)
                    .unwrap_or_else(|error| panic!("load {name}: {error}"));
                (name, module)
            })
            .collect();
        Built {
            _release: release,
            modules,
        }
    })
}

fn module(name: &str) -> &'static LoadedModule {
    built()
        .modules
        .iter()
        .find(|(module, _)| *module == name)
        .map(|(_, module)| module)
        .unwrap_or_else(|| panic!("{name} is not built"))
}

struct FixedCredentials;

impl CredentialSource for FixedCredentials {
    fn access<'a>(&'a self) -> BoxFuture<'a, Result<Credential, ProviderError>> {
        Box::pin(async {
            Ok(Credential {
                bearer: BEARER.into(),
                account_id: Some(ACCOUNT.into()),
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
                account_id: Some(ACCOUNT.into()),
            })
        })
    }
}

/// One route file bound to one profile, served by one component.
struct Case {
    name: &'static str,
    component: &'static str,
    route: RouteFile,
    profile: String,
    fixtures: RouteFixtures,
    invalid_request: fn() -> ProviderRequest,
}

impl Case {
    fn shipped(
        name: &'static str,
        component: &'static str,
        route: &str,
        profile: &str,
        fixtures: RouteFixtures,
        invalid_request: fn() -> ProviderRequest,
    ) -> Self {
        let path = repo_root().join("routes").join(format!("{route}.toml"));
        Self {
            name,
            component,
            route: load_route(&path).unwrap_or_else(|error| panic!("{error}")),
            profile: profile.to_owned(),
            fixtures,
            invalid_request,
        }
    }

    /// The same route with `[adapter_settings]` replaced; the value is typed by the route
    /// file's own field.
    fn with_settings(mut self, settings: Value) -> Self {
        self.route.adapter_settings =
            Some(serde_json::from_value(settings).expect("an adapter settings table"));
        self
    }

    fn with_endpoint(mut self, endpoint: &str) -> Self {
        self.route.endpoint = endpoint.to_owned();
        self
    }

    fn profile_text(&self) -> String {
        let path = repo_root()
            .join("profiles")
            .join(format!("{}.toml", self.profile));
        std::fs::read_to_string(&path).unwrap_or_else(|error| panic!("{}: {error}", path.display()))
    }

    fn settings(&self) -> ProviderSettings {
        let binding = self.route.binding(&self.profile).expect("a bound profile");
        ProviderSettings {
            origin_route: self.route.origin_route.clone(),
            endpoint: self.route.endpoint.clone(),
            model: self.profile.clone(),
            wire_model: binding.wire_model.clone(),
            adapter_settings: self.route.component_adapter_settings(
                binding,
                &self.profile,
                &self.profile_text(),
            ),
        }
    }

    fn component(&self, transport: ScriptedTransport) -> Arc<dyn Provider> {
        let provider = WasmProvider::new(
            module(self.component),
            self.settings(),
            Arc::new(FixedCredentials),
            Arc::new(transport),
            ExecutionLimits::default(),
        )
        .unwrap_or_else(|error| panic!("[{}] {error}", self.name));
        Arc::new(provider)
    }

    /// The native adapter the host composes from the same route file and profile.
    fn native(&self, transport: ScriptedTransport) -> Arc<dyn Provider> {
        let binding = self.route.binding(&self.profile).expect("a bound profile");
        let profile = ModelProfile::from_toml(&self.profile, &self.profile_text())
            .expect("a shipped profile");
        p1_host::catalog::route_provider(
            &self.route,
            binding,
            Arc::new(profile),
            Arc::new(transport),
            Arc::new(ScriptedWsConnector::new(Vec::new())),
            Arc::new(FixedCredentials),
        )
        .unwrap_or_else(|error| panic!("[{}] native: {error}", self.name))
    }
}

fn anthropic_fixtures() -> RouteFixtures {
    RouteFixtures {
        text_turn: anthropic::text_turn,
        tool_call_turn: anthropic::tool_call_turn,
        two_tool_calls: anthropic::two_tool_calls,
        truncated_tool_call: anthropic::truncated_tool_call,
        invalid_tool_json: anthropic::invalid_tool_json,
        error_event: anthropic::error_event,
        no_usage: anthropic::no_usage,
        reasoning_turn: anthropic::reasoning_turn,
        events_after_terminal: anthropic::events_after_terminal,
    }
}

fn responses_fixtures() -> RouteFixtures {
    RouteFixtures {
        text_turn: responses::TEXT_TURN,
        tool_call_turn: responses::TOOL_CALL_TURN,
        two_tool_calls: responses::TWO_TOOL_CALLS,
        truncated_tool_call: responses::TRUNCATED_TOOL_CALL,
        invalid_tool_json: responses::INVALID_TOOL_JSON,
        error_event: responses::ERROR_EVENT,
        no_usage: responses::NO_USAGE,
        reasoning_turn: responses::REASONING_TURN,
        events_after_terminal: responses::EVENTS_AFTER_TERMINAL,
    }
}

fn chat_fixtures() -> RouteFixtures {
    RouteFixtures {
        text_turn: chat::TEXT_TURN,
        tool_call_turn: chat::TOOL_CALL_TURN,
        two_tool_calls: chat::TWO_TOOL_CALLS,
        truncated_tool_call: chat::TRUNCATED_TOOL_CALL,
        invalid_tool_json: chat::INVALID_TOOL_JSON,
        error_event: chat::ERROR_EVENT,
        no_usage: chat::NO_USAGE,
        reasoning_turn: chat::REASONING_TURN,
        events_after_terminal: chat::EVENTS_AFTER_TERMINAL,
    }
}

/// A freeform tool: the Messages and Chat routes carry JSON-schema function tools only.
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

/// An explicit output cap: the Responses route refuses one.
fn capped_request() -> ProviderRequest {
    ProviderRequest {
        system_prompt: "conformance prompt".into(),
        history: vec![Item::User { text: "hi".into() }],
        tools: Vec::new(),
        options: ModelOptions {
            max_output_tokens: Some(1000),
            ..ModelOptions::default()
        },
    }
}

/// The Responses route over HTTP/SSE: the component lowers HTTP for every request until
/// S5's WebSocket branch lands, so the native side is composed for SSE too.
fn codex(name: &'static str, endpoint: &str) -> Case {
    Case::shipped(
        name,
        OPENAI,
        "openai-codex-subscription",
        "gpt-5.6-sol",
        responses_fixtures(),
        capped_request,
    )
    .with_settings(json!({"account": "codex-subscription", "transport": "sse"}))
    .with_endpoint(endpoint)
}

/// The cases, by index; `build::<N>` and `follow_up::<N>` are case `N`'s suite hooks.
fn cases() -> &'static [Case] {
    static CASES: OnceLock<Vec<Case>> = OnceLock::new();
    CASES.get_or_init(|| {
        vec![
            Case::shipped(
                "anthropic-messages/claude-subscription (long context)",
                ANTHROPIC,
                "anthropic-subscription",
                "claude-sonnet-5",
                anthropic_fixtures(),
                freeform_request,
            ),
            Case::shipped(
                "anthropic-messages/claude-subscription (no long context)",
                ANTHROPIC,
                "anthropic-subscription",
                "claude-sonnet-5",
                anthropic_fixtures(),
                freeform_request,
            )
            .with_settings(json!({"account": "claude-code-subscription", "long_context": false})),
            codex(
                "openai-responses/codex-subscription (base endpoint)",
                "https://chatgpt.com/backend-api",
            ),
            codex(
                "openai-responses/codex-subscription (codex endpoint)",
                "https://chatgpt.com/backend-api/codex",
            ),
            codex(
                "openai-responses/codex-subscription (responses endpoint)",
                "https://chatgpt.com/backend-api/codex/responses",
            ),
            Case::shipped(
                "openai-chat/glm-subscription (retained-thinking)",
                OPENAI_CHAT,
                "glm-subscription",
                "glm-5.3",
                chat_fixtures(),
                freeform_request,
            ),
            Case::shipped(
                "openai-chat/opencode-go-subscription (thinking-with-reasoning-alias)",
                OPENAI_CHAT,
                "opencode-go-subscription",
                "deepseek-v4.1-flash",
                chat_fixtures(),
                freeform_request,
            ),
        ]
    })
}

const ANTHROPIC_LONG: usize = 0;
const ANTHROPIC_SHORT: usize = 1;
const CODEX_BASE: usize = 2;
const CODEX_CODEX: usize = 3;
const CODEX_RESPONSES: usize = 4;
const CHAT_RETAINED: usize = 5;
const CHAT_ALIAS: usize = 6;

fn build<const N: usize>(transport: ScriptedTransport) -> Arc<dyn Provider> {
    cases()[N].component(transport)
}

fn follow_up<const N: usize>(request: &ProviderRequest) -> Value {
    let (_, sent) = exchange(&cases()[N].component_factory(), request, vec![status(400)]);
    let first = sent.first().expect("the follow-up request was sent");
    serde_json::from_slice(&first.body).expect("a JSON request body")
}

impl Case {
    fn component_factory(&self) -> impl Fn(ScriptedTransport) -> Arc<dyn Provider> + '_ {
        |transport| self.component(transport)
    }

    fn native_factory(&self) -> impl Fn(ScriptedTransport) -> Arc<dyn Provider> + '_ {
        |transport| self.native(transport)
    }

    fn route_under_test(
        &'static self,
        build: fn(ScriptedTransport) -> Arc<dyn Provider>,
        follow_up_request: fn(&ProviderRequest) -> Value,
    ) -> RouteUnderTest {
        RouteUnderTest {
            name: self.name,
            build,
            fixtures: self.fixtures,
            follow_up_request,
            fake_bearer: BEARER,
            invalid_request: self.invalid_request,
        }
    }
}

fn status(status: u16) -> ScriptedResponse {
    ScriptedResponse {
        status,
        headers: Vec::new(),
        chunks: Vec::new(),
        end: BodyEnd::Eof,
    }
}

/// Streams `request` on a provider `make` builds over `script`, on a runtime of its own
/// thread so a caller inside or outside a runtime can use it: the terminal outcome, or the
/// setup error, and the requests the transport recorded.
fn exchange(
    make: &(dyn Fn(ScriptedTransport) -> Arc<dyn Provider> + Sync),
    request: &ProviderRequest,
    script: Vec<ScriptedResponse>,
) -> (Result<Outcome, ProviderError>, Vec<HttpRequest>) {
    let transport = ScriptedTransport::new(script);
    let recorded = transport.clone();
    let outcome = std::thread::scope(|scope| {
        scope
            .spawn(|| {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_time()
                    .start_paused(true)
                    .build()
                    .expect("a runtime");
                runtime.block_on(async {
                    let provider = make(transport);
                    let mut stream = provider
                        .stream(request.clone(), CancellationToken::new())
                        .await?;
                    let mut last = None;
                    while let Some(event) = stream.next().await {
                        if let StreamEvent::Finished(outcome) = event {
                            last = Some(outcome);
                        }
                    }
                    Ok(last.expect("a terminal event"))
                })
            })
            .join()
            .expect("the exchange thread")
    });
    (outcome, recorded.requests())
}

fn run_case<const N: usize>() {
    run_all(&cases()[N].route_under_test(build::<N>, follow_up::<N>));
}

#[test]
fn anthropic_long_context_conformance() {
    run_case::<ANTHROPIC_LONG>();
}

#[test]
fn anthropic_without_long_context_conformance() {
    run_case::<ANTHROPIC_SHORT>();
}

#[test]
fn openai_base_endpoint_conformance() {
    run_case::<CODEX_BASE>();
}

#[test]
fn openai_codex_endpoint_conformance() {
    run_case::<CODEX_CODEX>();
}

#[test]
fn openai_responses_endpoint_conformance() {
    run_case::<CODEX_RESPONSES>();
}

#[test]
fn chat_retained_thinking_conformance() {
    run_case::<CHAT_RETAINED>();
}

#[test]
fn chat_thinking_with_reasoning_alias_conformance() {
    run_case::<CHAT_ALIAS>();
}

/// Every adapter a route file may name has a case, and so does every chat dialect a
/// shipped route uses: a new dialect cannot ship without running here.
#[test]
fn every_adapter_and_every_shipped_chat_dialect_has_a_case() {
    use p1_host::routes::AdapterSettings;
    let dialect = |route: &RouteFile| match route.settings() {
        Ok(AdapterSettings::OpenAiChat(settings)) => Some(settings.dialect),
        _ => None,
    };
    let covered: Vec<_> = cases()
        .iter()
        .filter_map(|case| dialect(&case.route))
        .collect();
    let mut routes = 0;
    for entry in std::fs::read_dir(repo_root().join("routes")).expect("the routes directory") {
        let path = entry.expect("a route entry").path();
        if path.extension().and_then(|extension| extension.to_str()) != Some("toml") {
            continue;
        }
        routes += 1;
        let route = load_route(&path).unwrap_or_else(|error| panic!("{error}"));
        assert!(
            cases()
                .iter()
                .any(|case| case.route.adapter == route.adapter),
            "{}: adapter {} has no case",
            route.id,
            route.adapter
        );
        if let Some(shipped) = dialect(&route) {
            assert!(
                covered.contains(&shipped),
                "{}: chat dialect {shipped:?} has no case",
                route.id
            );
        }
    }
    assert!(routes > 0, "no shipped route file was found");
    for adapter in p1_host::routes::ADAPTER_KEYS {
        assert!(
            cases().iter().any(|case| case.route.adapter == *adapter),
            "adapter {adapter} has no case"
        );
    }
}

/// A request with tools, history and a cache key where the route carries one.
fn canonical_request(case: &Case) -> ProviderRequest {
    let mut request = ProviderRequest {
        system_prompt: "parity prompt".into(),
        history: vec![Item::User {
            text: "compare the wires".into(),
        }],
        tools: vec![ToolDeclaration {
            name: "read".into(),
            description: "read a file".into(),
            kind: DeclarationKind::Function {
                input_schema: json!({
                    "type": "object",
                    "properties": {"path": {"type": "string"}},
                    "required": ["path"],
                }),
            },
        }],
        options: ModelOptions::default(),
    };
    let carries_cache_key = matches!(
        case.route.settings(),
        Ok(p1_host::routes::AdapterSettings::OpenAiChat(settings)) if settings.session_header.is_some()
    );
    if carries_cache_key {
        request.options.cache_key = Some("parity-session".into());
    }
    request
}

fn is_credential(name: &str) -> bool {
    CREDENTIAL_HEADERS
        .iter()
        .any(|credential| name.eq_ignore_ascii_case(credential))
}

/// ADR-0086's comparison: the non-credential headers in order, the credential ones by
/// presence.
fn header_view(request: &HttpRequest) -> (Vec<(String, String)>, Vec<String>) {
    let others = request
        .headers
        .iter()
        .filter(|(name, _)| !is_credential(name))
        .map(|(name, value)| (name.to_ascii_lowercase(), value.clone()))
        .collect();
    let mut credentials: Vec<String> = request
        .headers
        .iter()
        .filter(|(name, _)| is_credential(name))
        .map(|(name, _)| name.to_ascii_lowercase())
        .collect();
    credentials.sort();
    (others, credentials)
}

#[test]
fn the_component_sends_what_the_native_adapter_sends() {
    for case in cases() {
        let request = canonical_request(case);
        let (native_outcome, native) =
            exchange(&case.native_factory(), &request, vec![status(400)]);
        let (component_outcome, component) =
            exchange(&case.component_factory(), &request, vec![status(400)]);
        assert!(native_outcome.is_ok(), "[{}] native setup", case.name);
        assert!(component_outcome.is_ok(), "[{}] component setup", case.name);
        assert_eq!(native.len(), 1, "[{}]", case.name);
        assert_eq!(component.len(), 1, "[{}]", case.name);
        let (native, component) = (&native[0], &component[0]);
        assert_eq!(component.url, native.url, "[{}] url", case.name);
        let body = |request: &HttpRequest| -> Value {
            serde_json::from_slice(&request.body).expect("a JSON body")
        };
        assert_eq!(body(component), body(native), "[{}] body", case.name);
        let (native_headers, native_credentials) = header_view(native);
        let (component_headers, component_credentials) = header_view(component);
        assert_eq!(
            component_headers, native_headers,
            "[{}] non-credential headers",
            case.name
        );
        assert_eq!(
            component_credentials, native_credentials,
            "[{}] credential headers",
            case.name
        );
        assert!(
            component_credentials.contains(&"authorization".to_owned()),
            "[{}] no credential was attached",
            case.name
        );
    }
}

#[test]
fn classify_matches_the_native_parser_and_account_diagnoses_are_never_refreshed_or_retried() {
    let responses: [(u16, &str); 9] = [
        (400, "{}"),
        (401, r#"{"error":{"type":"invalid_api_key"}}"#),
        (401, r#"{"error":{"type":"creditserror"}}"#),
        (
            403,
            r#"{"error":{"type":"FreeTierError","message":"only in the vendor client"}}"#,
        ),
        (
            402,
            r#"{"error":{"type":"GoUsageLimitError","message":"wire text"}}"#,
        ),
        (
            429,
            r#"{"error":{"type":"GoUsageLimitError","message":"wire text"}}"#,
        ),
        (429, r#"{"error":{"type":"rate_limit_error"}}"#),
        (413, r#"{"error":{"code":"context_length_exceeded"}}"#),
        (500, "{}"),
    ];
    for case in cases() {
        let request = canonical_request(case);
        for (code, body) in responses {
            let script = || {
                (0..4)
                    .map(|_| ScriptedResponse {
                        status: code,
                        headers: Vec::new(),
                        chunks: vec![body.as_bytes().to_vec()],
                        end: BodyEnd::Eof,
                    })
                    .collect::<Vec<_>>()
            };
            let (native_outcome, native) = exchange(&case.native_factory(), &request, script());
            let (component_outcome, component) =
                exchange(&case.component_factory(), &request, script());
            let failure = |outcome: Result<Outcome, ProviderError>| match outcome {
                Ok(Outcome::Failed(error)) => (error.kind, error.message),
                other => panic!(
                    "[{}] HTTP {code}: expected a failure, saw {other:?}",
                    case.name
                ),
            };
            assert_eq!(
                failure(component_outcome),
                failure(native_outcome),
                "[{}] HTTP {code} {body}",
                case.name
            );
            assert_eq!(
                component.len(),
                native.len(),
                "[{}] HTTP {code} {body}: attempts",
                case.name
            );
        }
    }
}

/// The seeded bug of the component path: a route bound to the wrong provider component.
/// The chat component decodes the Messages wire into nothing, and the suite must say so.
fn build_wrong_component(transport: ScriptedTransport) -> Arc<dyn Provider> {
    cases()[CHAT_ALIAS].component(transport)
}

#[test]
fn the_suite_catches_a_route_bound_to_the_wrong_component() {
    let wrong = RouteUnderTest {
        fixtures: anthropic_fixtures(),
        ..cases()[CHAT_ALIAS].route_under_test(build_wrong_component, follow_up::<CHAT_ALIAS>)
    };
    let caught = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        text_deltas_then_single_terminal(&wrong);
    }));
    let payload = caught.expect_err("the seeded bug was not caught");
    let message = payload
        .downcast_ref::<String>()
        .cloned()
        .or_else(|| {
            payload
                .downcast_ref::<&str>()
                .map(|text| (*text).to_owned())
        })
        .unwrap_or_default();
    assert!(
        message.contains("text_deltas_then_single_terminal"),
        "the panic did not name the check: {message}"
    );
}
