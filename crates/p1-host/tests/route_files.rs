//! Route files (`routes/<id>.toml`): the host loads account and endpoint data instead of
//! compiling it (`docs/design/routes-and-profiles.md` §1.2), registers one factory per
//! file under its id (§2), and resolves a profile to the wire model the route reaches it
//! by. The chat adapter's shared conformance suite also runs here, against the
//! SHIPPED routes built from the shipped files through this same loading path.

mod common;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use common::Harness;
use futures_util::StreamExt;
use p1_assembly::{Assembled, Substitutions, assemble, load_environment};
use p1_auth::{BorrowSource, BorrowStore, CredentialKind};
use p1_contracts::{
    BoxFuture, CancellationToken, Effort, Item, JournalRecord, ModelOptions, Origin, Outcome,
    Provider, ProviderError, ProviderRequest, StreamEvent, TurnEnd,
};
use p1_core::{Agent, AgentParts};
use p1_host::activity::CompletionHub;
use p1_host::catalog::{
    build_catalog, chat_route, resolve_environment, responses_route, route_provider,
};
use p1_host::cli::SandboxMode;
use p1_host::routes::{AdapterSettings, RouteFile, load_all_routes, load_route, load_route_by_id};
use p1_model_profile::{ModelProfile, ThinkingPolicy};
use p1_provider_conformance::{
    RouteFixtures, RouteUnderTest, fixtures::chat as chat_fixtures, run_all,
};
use p1_provider_http::testing::{RefusingWsConnector, ScriptedResponse, ScriptedTransport};
use p1_provider_http::{Credential, CredentialSource};
use p1_provider_openai::{ResponsesAccount, ResponsesAdapterSettings, ResponsesTransport};
use p1_provider_openai_chat::{ChatAdapterSettings, ChatDialect, build_request};
use p1_testkit::{PassthroughContext, RecordingEvents, RecordingJournal, ScriptedAuthorization};
use tempfile::tempdir;

/// The bearer the test credential source returns. A route file holds a credential
/// REFERENCE, so nothing in this file reads a credential file.
const BEARER: &str = "ROUTE-FILES-FAKE-BEARER";

/// The adapter's own fixtures, the ones its conformance suite runs on. The suite below
/// must mean the same thing for the shipped composed routes as it does for the adapter.
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
        _: &'a Credential,
    ) -> BoxFuture<'a, Result<Credential, ProviderError>> {
        Box::pin(async {
            Ok(Credential {
                bearer: format!("{BEARER}-refreshed"),
                account_id: None,
            })
        })
    }
}

// ------------------------------------------------------------------- the shipped files

fn repo(relative: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join(relative)
}

/// The shipped environments directory. The host looks for routes in
/// `<environments dir>/../routes`, exactly as it looks for profiles in
/// `<environments dir>/../profiles`, so this reaches the shipped route files too.
fn shipped_environment_dirs() -> Vec<PathBuf> {
    vec![repo("environments")]
}

/// What the host resolves for one environment before it assembles (spec §2 steps 1–3):
/// the route file the environment names, the profile it selects, and the wire model the
/// binding gives that profile on that route.
#[derive(Debug)]
struct Resolved {
    route: RouteFile,
    profile: Arc<ModelProfile>,
    wire_model: String,
}

fn resolve(environment_dirs: &[PathBuf], name: &str) -> Result<Resolved, String> {
    let mut loaded = load_environment(name, environment_dirs).map_err(|error| error.to_string())?;
    resolve_environment(&mut loaded, environment_dirs)?;
    let profile = loaded
        .profile
        .clone()
        .ok_or_else(|| format!("`{name}` names no profile"))?;
    let route = load_route_by_id(environment_dirs, &loaded.provider)?;
    Ok(Resolved {
        route,
        profile,
        wire_model: loaded.model,
    })
}

fn shipped(environment: &str) -> Resolved {
    resolve(&shipped_environment_dirs(), environment).expect("the shipped files resolve")
}

/// Build the provider the catalog factory would build: `catalog::route_provider` with
/// the route's binding for the resolved profile. No constructor argument is hand-made.
///
/// The connector is injected next to the transport (ADR-0047 §1) and REFUSES every
/// upgrade: a route that asks for WebSocket falls back to SSE at once (the shipped
/// Codex route does — ADR-0047 §1 was revised to make WebSocket its default), an SSE
/// route ignores it, and no test opens a socket.
fn provider_of(resolved: &Resolved, transport: ScriptedTransport) -> Arc<dyn Provider> {
    let binding = resolved
        .route
        .binding(&resolved.profile.id)
        .expect("the route serves this profile");
    route_provider(
        &resolved.route,
        binding,
        resolved.profile.clone(),
        Arc::new(transport),
        Arc::new(RefusingWsConnector::default()),
        Arc::new(Fixed),
    )
    .expect("the route composes")
}

/// One shipped route, composed from the shipped environment that names it.
fn shipped_deepseek(transport: ScriptedTransport) -> Arc<dyn Provider> {
    provider_of(&shipped("deepseek"), transport)
}

fn shipped_glm(transport: ScriptedTransport) -> Arc<dyn Provider> {
    provider_of(&shipped("glm"), transport)
}

/// The adapter's own lowering over the RESOLVED shipped route: the follow-up request a
/// reasoning replay must land in.
fn shipped_request_body(environment: &str, request: &ProviderRequest) -> serde_json::Value {
    let resolved = shipped(environment);
    let binding = resolved
        .route
        .binding(&resolved.profile.id)
        .expect("the route serves this profile");
    let route =
        chat_route(&resolved.route, binding, &resolved.profile).expect("the route composes");
    build_request(&route, &binding.wire_model, &resolved.profile, request)
        .expect("the request is representable")
}

fn deepseek_request(request: &ProviderRequest) -> serde_json::Value {
    shipped_request_body("deepseek", request)
}

fn glm_request(request: &ProviderRequest) -> serde_json::Value {
    shipped_request_body("glm", request)
}

fn shipped_kimi(transport: ScriptedTransport) -> Arc<dyn Provider> {
    provider_of(&shipped("kimi"), transport)
}

fn kimi_request(request: &ProviderRequest) -> serde_json::Value {
    shipped_request_body("kimi", request)
}

fn fixtures() -> RouteFixtures {
    RouteFixtures {
        text_turn: chat_fixtures::TEXT_TURN,
        tool_call_turn: chat_fixtures::TOOL_CALL_TURN,
        two_tool_calls: chat_fixtures::TWO_TOOL_CALLS,
        truncated_tool_call: chat_fixtures::TRUNCATED_TOOL_CALL,
        invalid_tool_json: chat_fixtures::INVALID_TOOL_JSON,
        error_event: chat_fixtures::ERROR_EVENT,
        no_usage: chat_fixtures::NO_USAGE,
        reasoning_turn: chat_fixtures::REASONING_TURN,
        events_after_terminal: chat_fixtures::EVENTS_AFTER_TERMINAL,
    }
}

/// A request this route must reject: an effort neither shipped profile lists.
fn invalid_request() -> ProviderRequest {
    ProviderRequest {
        system_prompt: String::new(),
        history: Vec::new(),
        tools: Vec::new(),
        options: ModelOptions {
            reasoning_effort: Some(Effort::Medium),
            ..ModelOptions::default()
        },
    }
}

/// One shipped route, as the shared suite needs it: a non-capturing constructor and the
/// adapter's own follow-up lowering over the same resolved route.
struct ShippedRoute {
    name: &'static str,
    build: fn(ScriptedTransport) -> Arc<dyn Provider>,
    follow_up_request: fn(&ProviderRequest) -> serde_json::Value,
}

fn shipped_routes() -> [ShippedRoute; 3] {
    [
        ShippedRoute {
            name: "opencode-go-subscription",
            build: shipped_deepseek,
            follow_up_request: deepseek_request,
        },
        ShippedRoute {
            name: "glm-subscription",
            build: shipped_glm,
            follow_up_request: glm_request,
        },
        ShippedRoute {
            name: "kimi-coding-subscription",
            build: shipped_kimi,
            follow_up_request: kimi_request,
        },
    ]
}

/// The conformance suite over the shipped COMPOSED chat routes. Each provider comes from
/// `environments/` -> `routes/*.toml` -> `profiles/*.toml`; the only thing this file
/// supplies is a scripted transport and a fake credential source.
#[test]
fn the_shipped_composed_routes_pass_the_chat_conformance_suite() {
    for route in shipped_routes() {
        run_all(&RouteUnderTest {
            name: route.name,
            build: route.build,
            fixtures: fixtures(),
            follow_up_request: route.follow_up_request,
            fake_bearer: BEARER,
            invalid_request,
        });
    }
}

// ----------------------------------------------------------------------- scratch roots

/// A scratch root with the layout the host looks in: `environments/` plus `routes/` and
/// `profiles/` next to it.
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

    fn environment_dirs(&self) -> Vec<PathBuf> {
        vec![self.root.path().join("environments")]
    }

    fn write_route(&self, stem: &str, text: &str) {
        std::fs::write(self.route_path(stem), text).unwrap();
    }

    fn route_path(&self, stem: &str) -> PathBuf {
        self.root.path().join("routes").join(format!("{stem}.toml"))
    }

    fn write_profile(&self, id: &str, limits: &str) {
        std::fs::write(
            self.root.path().join("profiles").join(format!("{id}.toml")),
            profile_file(id, limits),
        )
        .unwrap();
    }

    fn write_environment(&self, name: &str, text: &str) {
        let dir = self.root.path().join("environments").join(name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("environment.toml"), text).unwrap();
        std::fs::write(dir.join("prompt.md"), "synthetic prompt").unwrap();
    }

    fn copy_shipped_routes(&self) {
        for entry in std::fs::read_dir(repo("routes")).unwrap() {
            let path = entry.unwrap().path();
            std::fs::copy(
                &path,
                self.root
                    .path()
                    .join("routes")
                    .join(path.file_name().unwrap()),
            )
            .unwrap();
        }
    }

    fn copy_shipped_profiles(&self) {
        for entry in std::fs::read_dir(repo("profiles")).unwrap() {
            let path = entry.unwrap().path();
            std::fs::copy(
                &path,
                self.root
                    .path()
                    .join("profiles")
                    .join(path.file_name().unwrap()),
            )
            .unwrap();
        }
    }

    fn resolve(&self, name: &str) -> Result<Resolved, String> {
        resolve(&self.environment_dirs(), name)
    }
}

/// `text` with `from` replaced by `to`. A template that drifted fails loudly instead of
/// quietly testing something else.
fn with(text: &str, from: &str, to: &str) -> String {
    assert!(
        text.contains(from),
        "the template does not contain {from:?}"
    );
    text.replace(from, to)
}

/// A complete `openai-chat` route file. Everything a route declares is a parameter, so a
/// test states the route instead of inheriting one.
fn route_file(
    id: &str,
    endpoint: &str,
    dialect: &str,
    session_header: Option<&str>,
    headers: &[(&str, &str)],
    models: &str,
) -> String {
    let mut text = format!(
        "id = \"{id}\"\n\
         origin_route = \"openai-chat/{id}\"\n\
         adapter = \"openai-chat\"\n\
         endpoint = \"{endpoint}\"\n"
    );
    if !headers.is_empty() {
        text.push_str("\n[headers]\n");
        for (name, value) in headers {
            text.push_str(&format!("{name} = \"{value}\"\n"));
        }
    }
    text.push_str("\n[credential]\nkind = \"api-key\"\nenv = \"P1_ROUTE_FILES_TEST_KEY\"\n");
    text.push_str(&format!("\n[adapter_settings]\ndialect = \"{dialect}\"\n"));
    if let Some(header) = session_header {
        text.push_str(&format!("session_header = \"{header}\"\n"));
    }
    if !models.is_empty() {
        text.push('\n');
        text.push_str(models);
    }
    text
}

/// One `[models.<profile id>]` binding.
fn binding(profile_id: &str, wire_model: &str) -> String {
    format!("[models.\"{profile_id}\"]\nwire_model = \"{wire_model}\"\n")
}

/// One binding with a route ceiling on output tokens.
fn limited_binding(profile_id: &str, wire_model: &str, output_limit: u32) -> String {
    format!(
        "[models.\"{profile_id}\"]\nwire_model = \"{wire_model}\"\noutput_limit = {output_limit}\n"
    )
}

/// A minimal profile: enabled thinking and the two efforts the chat dialect encodes.
fn profile_file(id: &str, limits: &str) -> String {
    format!(
        "id = \"{id}\"\n\
         revision = 1\n\
         model_id = \"{id}\"\n\
         family = \"synthetic\"\n\
         thinking = \"enabled\"\n\
         efforts = [\"high\", \"max\"]\n\
         default_effort = \"high\"\n\
         {limits}"
    )
}

/// A new-form environment: a route and a profile, nothing else.
fn environment_file(route: &str, profile: &str) -> String {
    format!("route = \"{route}\"\nprofile = \"{profile}\"\n")
}

fn substitutions(workspace: &Path) -> Substitutions {
    Substitutions {
        workspace: workspace.display().to_string(),
        date: "2026-09-20".into(),
        os: "linux".into(),
    }
}

/// The host's own order over a scratch root: build the catalog (which registers one
/// factory per route file), load the environment, resolve it, assemble.
fn assemble_scratch(scratch: &Scratch, name: &str) -> Result<Assembled, String> {
    let dirs = scratch.environment_dirs();
    let harness = Harness::new(dirs.clone(), &[]);
    let completion = Arc::new(CompletionHub::new());
    let catalog = build_catalog(&harness.deps, SandboxMode::Off, &[], &[], &[], &completion)?;
    let mut environment = load_environment(name, &dirs).map_err(|error| error.to_string())?;
    resolve_environment(&mut environment, &dirs)?;
    let workspace = tempdir().unwrap();
    assemble(
        &catalog,
        &environment,
        workspace.path(),
        &substitutions(workspace.path()),
    )
    .map_err(|error| error.to_string())
}

/// A canonical request with an explicit output cap.
fn request_with(max_output_tokens: u32) -> ProviderRequest {
    ProviderRequest {
        system_prompt: "synthetic prompt".into(),
        history: vec![Item::User {
            text: "synthetic question".into(),
        }],
        tools: Vec::new(),
        options: ModelOptions {
            max_output_tokens: Some(max_output_tokens),
            ..ModelOptions::default()
        },
    }
}

/// Drive one request through a built provider to completion.
async fn drain(provider: &Arc<dyn Provider>, request: ProviderRequest) -> Vec<StreamEvent> {
    let mut stream = provider
        .stream(request, CancellationToken::new())
        .await
        .expect("the route accepts the request");
    let mut events = Vec::new();
    while let Some(event) = stream.next().await {
        events.push(event);
    }
    events
}

fn parts(provider: Arc<dyn Provider>, journal: Arc<RecordingJournal>) -> AgentParts {
    AgentParts {
        provider,
        tools: Vec::new(),
        system_prompt: "synthetic prompt".into(),
        options: ModelOptions::default(),
        context: Arc::new(PassthroughContext),
        authorization: Arc::new(ScriptedAuthorization::permit_all()),
        journal,
        events: Arc::new(RecordingEvents::new()),
    }
}

/// Run one real turn on `provider` and return the journal records it produced.
async fn recorded_turn(provider: Arc<dyn Provider>) -> Vec<JournalRecord> {
    let journal = Arc::new(RecordingJournal::new());
    let mut agent = Agent::new(parts(provider, journal.clone())).expect("an agent over the route");
    let end = tokio::time::timeout(
        Duration::from_secs(5),
        agent.run_turn("hi".into(), CancellationToken::new()),
    )
    .await
    .expect("the scripted turn finishes");
    assert!(matches!(end, TurnEnd::Completed { .. }), "{end:?}");
    journal.records()
}

// ------------------------------------------------------- the shipped data is the route

#[test]
fn the_shipped_route_files_hold_what_the_host_used_to_hard_code() {
    let dirs = shipped_environment_dirs();
    let ids: Vec<String> = load_all_routes(&dirs)
        .expect("the shipped route files load")
        .into_iter()
        .map(|route| route.id)
        .collect();
    // `anthropic-subscription` joined the shipped routes in ADR-0039 step 4 and
    // `openai-codex-subscription` in step 4b; each one's own fields are pinned in
    // `crates/p1-host/tests/{anthropic,openai}_route.rs`. With both first-party
    // adapters routed, no shipped key is a whole provider any more.
    assert_eq!(
        ids,
        [
            "anthropic-subscription",
            "glm-subscription",
            "kimi-coding-subscription",
            "openai-codex-subscription",
            // The owner's second OpenCode Go account: data only, no Rust change.
            "opencode-go-2-subscription",
            "opencode-go-subscription"
        ]
    );
    assert!(
        p1_host::catalog::WHOLE_PROVIDERS.is_empty(),
        "every shipped provider is a route file now"
    );

    let go = load_route_by_id(&dirs, "opencode-go-subscription").expect("the shipped route file");
    assert_eq!(go.origin_route, "openai-chat/opencode-go-subscription");
    assert_eq!(go.adapter, "openai-chat");
    assert_eq!(
        go.endpoint,
        "https://opencode.ai/zen/go/v1/chat/completions"
    );
    assert_eq!(go.credential.kind, CredentialKind::ApiKey);
    assert_eq!(go.credential.env.as_deref(), Some("OPENCODE_API_KEY"));
    assert_eq!(
        go.credential.borrow,
        vec![
            BorrowSource {
                store: BorrowStore::Opencode,
                key: "opencode-go".into(),
            },
            BorrowSource {
                store: BorrowStore::Pi,
                key: "opencode-go".into(),
            },
        ]
    );
    assert_eq!(
        go.settings().expect("the adapter parses its settings"),
        AdapterSettings::OpenAiChat(ChatAdapterSettings {
            dialect: ChatDialect::ThinkingWithReasoningAlias,
            session_header: Some("x-opencode-session".into()),
        })
    );
    let deepseek = &go.models["deepseek-v4.1-flash"];
    assert_eq!(deepseek.wire_model, "deepseek-v4.1-flash");
    assert_eq!(deepseek.output_limit, None);
    assert_eq!(deepseek.context_limit, None);
    assert!(
        go.headers.is_empty(),
        "`user-agent` carries the crate version, so it stays compiled, not in the file"
    );

    let glm = load_route_by_id(&dirs, "glm-subscription").expect("the shipped route file");
    assert_eq!(glm.origin_route, "openai-chat/glm-subscription");
    assert_eq!(
        glm.endpoint,
        "https://api.z.ai/api/coding/paas/v4/chat/completions"
    );
    assert_eq!(glm.credential.kind, CredentialKind::ApiKey);
    assert_eq!(glm.credential.env.as_deref(), Some("ZAI_API_KEY"));
    assert_eq!(
        glm.credential.borrow,
        vec![BorrowSource {
            store: BorrowStore::Pi,
            key: "zai".into(),
        }]
    );
    assert_eq!(
        glm.settings().expect("the adapter parses its settings"),
        AdapterSettings::OpenAiChat(ChatAdapterSettings {
            dialect: ChatDialect::RetainedThinking,
            session_header: None,
        })
    );
    assert_eq!(glm.models["glm-5.3"].wire_model, "glm-5.3");
    assert!(glm.headers.is_empty(), "no static headers on this route");

    let kimi = load_route_by_id(&dirs, "kimi-coding-subscription").expect("the shipped route file");
    assert_eq!(kimi.origin_route, "openai-chat/kimi-coding-subscription");
    assert_eq!(kimi.adapter, "openai-chat");
    assert_eq!(
        kimi.endpoint,
        "https://api.kimi.ai/coding/v1/chat/completions"
    );
    assert_eq!(kimi.credential.kind, CredentialKind::ApiKey);
    assert_eq!(kimi.credential.env.as_deref(), Some("KIMI_API_KEY"));
    assert_eq!(
        kimi.credential.borrow,
        vec![
            BorrowSource {
                store: BorrowStore::Pi,
                key: "kimi-coding".into(),
            },
            BorrowSource {
                store: BorrowStore::Opencode,
                key: "kimi-code-plan-global".into(),
            },
        ]
    );
    assert_eq!(
        kimi.settings().expect("the adapter parses its settings"),
        AdapterSettings::OpenAiChat(ChatAdapterSettings {
            dialect: ChatDialect::RetainedThinking,
            session_header: None,
        })
    );
    assert_eq!(kimi.models.len(), 1);
    assert_eq!(kimi.models["kimi-k3"].wire_model, "k3");
    assert!(kimi.headers.is_empty(), "no static headers on this route");

    let resolved = shipped("kimi");
    assert_eq!(resolved.profile.id, "kimi-k3");
    assert_eq!(resolved.profile.model_id, "k3");
    assert_eq!(resolved.profile.family, "kimi");
    assert_eq!(resolved.profile.revision, 1);
    assert_eq!(resolved.profile.thinking, ThinkingPolicy::Preserved);
    assert_eq!(
        resolved.profile.efforts,
        [Effort::Low, Effort::High, Effort::Max]
    );
    assert_eq!(resolved.profile.default_effort, Some(Effort::High));
    assert_eq!(resolved.profile.max_output_tokens, Some(131_072));
    assert_eq!(resolved.profile.context_tokens, None);
    assert_eq!(resolved.wire_model, "k3");
}

/// The endpoint, the session header and every model name of the two routes now live in
/// `routes/*.toml`. If any of them is compiled into the host again, this fails.
#[test]
fn the_two_shipped_routes_have_no_compiled_literals() {
    let literals = [
        "opencode.ai",
        "api.z.ai",
        "api.kimi.ai",
        "KIMI_API_KEY",
        "kimi-coding-subscription",
        "kimi-k3",
        "x-opencode-session",
        "OPENCODE_API_KEY",
        "ZAI_API_KEY",
        "opencode-go-subscription",
        "glm-subscription",
        "deepseek-v4.1-flash",
        "glm-5.3",
        "chat/completions",
    ];
    for file in ["src/catalog.rs", "src/run.rs", "src/cli.rs", "src/auth.rs"] {
        let path = repo("crates/p1-host").join(file);
        let text = std::fs::read_to_string(&path).expect("the host source is readable");
        for literal in literals {
            assert!(
                !text.contains(literal),
                "{} still compiles route data ({literal:?})",
                path.display()
            );
        }
    }
}

// ------------------------------------------------------------------- loading rejects

#[test]
fn a_file_stem_that_disagrees_with_the_route_id_is_a_load_error() {
    let scratch = Scratch::new();
    scratch.write_route(
        "stem-b",
        &route_file(
            "stem-a",
            "https://synthetic.example.test/v1/chat/completions",
            "retained-thinking",
            None,
            &[],
            &binding("synthetic-profile", "synthetic-profile"),
        ),
    );
    let error = load_route(&scratch.route_path("stem-b")).unwrap_err();
    assert!(
        error.contains("route id \"stem-a\" must equal the file stem \"stem-b\""),
        "{error}"
    );
    assert!(
        error.contains("stem-b.toml"),
        "every load error names the file: {error}"
    );
    let error = load_all_routes(&scratch.environment_dirs()).unwrap_err();
    assert!(error.contains("must equal the file stem"), "{error}");
}

#[test]
fn an_unknown_key_in_a_route_file_is_a_load_error() {
    let scratch = Scratch::new();
    let body = with(
        &route_file(
            "extra",
            "https://synthetic.example.test/v1/chat/completions",
            "retained-thinking",
            None,
            &[],
            &binding("synthetic-profile", "synthetic-profile"),
        ),
        "adapter = ",
        "session = \"typo\"\nadapter = ",
    );
    scratch.write_route("extra", &body);
    let error = load_route(&scratch.route_path("extra")).unwrap_err();
    assert!(error.contains("session"), "{error}");
}

#[test]
fn a_secret_looking_header_is_a_load_error() {
    for (name, value) in [
        ("Authorization", "Bearer p1-route-files-header-value"),
        ("X-Api-Key", "p1-route-files-header-value"),
        ("Cookie", "session=p1-route-files-header-value"),
    ] {
        let scratch = Scratch::new();
        scratch.write_route(
            "secret",
            &route_file(
                "secret",
                "https://synthetic.example.test/v1/chat/completions",
                "retained-thinking",
                None,
                &[(name, value)],
                &binding("synthetic-profile", "synthetic-profile"),
            ),
        );
        let error = load_route(&scratch.route_path("secret")).unwrap_err();
        assert!(
            error.contains(name) && error.contains("looks like a credential"),
            "{name}: {error}"
        );
        assert!(
            !error.contains(value),
            "a load error never echoes a header value: {error}"
        );
    }
}

#[test]
fn an_unknown_adapter_lists_the_known_adapter_keys() {
    let scratch = Scratch::new();
    let body = with(
        &route_file(
            "gemini",
            "https://synthetic.example.test/v1/chat/completions",
            "retained-thinking",
            None,
            &[],
            &binding("synthetic-profile", "synthetic-profile"),
        ),
        "adapter = \"openai-chat\"",
        "adapter = \"gemini-chat\"",
    );
    scratch.write_route("gemini", &body);
    let error = load_route(&scratch.route_path("gemini")).unwrap_err();
    assert!(error.contains("unknown adapter \"gemini-chat\""), "{error}");
    assert!(
        error.contains("openai-chat"),
        "the error lists the known adapter keys: {error}"
    );
}

#[test]
fn an_unknown_credential_kind_is_a_load_error() {
    let scratch = Scratch::new();
    let body = with(
        &route_file(
            "token",
            "https://synthetic.example.test/v1/chat/completions",
            "retained-thinking",
            None,
            &[],
            &binding("synthetic-profile", "synthetic-profile"),
        ),
        "kind = \"api-key\"",
        "kind = \"bearer-token\"",
    );
    scratch.write_route("token", &body);
    let error = load_route(&scratch.route_path("token")).unwrap_err();
    assert!(error.contains("bearer-token"), "{error}");
    assert!(
        error.contains("api-key")
            && error.contains("claude-code-oauth")
            && error.contains("codex-oauth"),
        "the error lists the known kinds: {error}"
    );
}

#[test]
fn an_unknown_borrow_store_is_a_load_error() {
    for (borrow, expected) in [
        (
            "[\"keepass:opencode-go\"]",
            "unknown borrow store \"keepass\"",
        ),
        ("[\"opencode\"]", "is not `<store>:<key>`"),
    ] {
        let scratch = Scratch::new();
        let body = with(
            &route_file(
                "borrow",
                "https://synthetic.example.test/v1/chat/completions",
                "retained-thinking",
                None,
                &[],
                &binding("synthetic-profile", "synthetic-profile"),
            ),
            "env = \"P1_ROUTE_FILES_TEST_KEY\"",
            &format!("env = \"P1_ROUTE_FILES_TEST_KEY\"\nborrow = {borrow}"),
        );
        scratch.write_route("borrow", &body);
        let error = load_route(&scratch.route_path("borrow")).unwrap_err();
        assert!(error.contains(expected), "{borrow}: {error}");
        assert!(
            error.contains("opencode") && error.contains("pi"),
            "the error lists the known stores: {error}"
        );
    }
}

/// Both OAuth kinds are data-driven now (ADR-0039 step 4, spec §7.2): a route
/// file may point at either compiled login. The shipped routes that do are pinned in
/// `tests/anthropic_route.rs` and `tests/openai_route.rs`; this checks the parse and
/// the validation of one synthetic file, which reads no credential.
#[test]
fn the_codex_oauth_credential_kind_parses_and_loads() {
    let kind = "codex-oauth";
    let scratch = Scratch::new();
    let body = with(
        &route_file(
            "oauth",
            "https://synthetic.example.test/v1/chat/completions",
            "retained-thinking",
            None,
            &[],
            &binding("synthetic-profile", "synthetic-profile"),
        ),
        "kind = \"api-key\"",
        &format!("kind = \"{kind}\""),
    );
    scratch.write_route("oauth", &body);
    let route = load_route(&scratch.route_path("oauth"))
        .expect("the Codex login kind has a compiled source");
    assert_eq!(route.credential.kind, CredentialKind::CodexOauth);
    assert_eq!(route.credential.kind.name(), kind);
    // The kind names WHICH compiled source is used; the file's own reference is kept
    // as it was written.
    assert_eq!(
        route.credential.env.as_deref(),
        Some("P1_ROUTE_FILES_TEST_KEY")
    );
    assert!(route.credential.borrow.is_empty());
}

#[test]
fn an_unknown_adapter_settings_key_is_rejected_by_the_adapter() {
    let scratch = Scratch::new();
    let body = with(
        &route_file(
            "settings",
            "https://synthetic.example.test/v1/chat/completions",
            "retained-thinking",
            None,
            &[],
            &binding("synthetic-profile", "synthetic-profile"),
        ),
        "dialect = ",
        "native_tools = true\ndialect = ",
    );
    scratch.write_route("settings", &body);
    let error = load_route(&scratch.route_path("settings")).unwrap_err();
    assert!(error.contains("invalid `[adapter_settings]`"), "{error}");
    assert!(
        error.contains("native_tools"),
        "the adapter names the key it does not know: {error}"
    );
}

#[test]
fn a_missing_route_file_lists_the_ids_the_directory_holds() {
    let scratch = Scratch::new();
    scratch.write_route(
        "present-route",
        &route_file(
            "present-route",
            "https://synthetic.example.test/v1/chat/completions",
            "retained-thinking",
            None,
            &[],
            &binding("synthetic-profile", "synthetic-profile"),
        ),
    );
    scratch.write_profile("synthetic-profile", "");
    let dirs = scratch.environment_dirs();
    let error = load_route_by_id(&dirs, "ghost-route").unwrap_err();
    assert!(
        error.contains("route `ghost-route` was not found"),
        "{error}"
    );
    assert!(error.contains("available: [\"present-route\"]"), "{error}");

    scratch.write_environment(
        "ghost",
        &environment_file("ghost-route", "synthetic-profile"),
    );
    let error = scratch.resolve("ghost").unwrap_err();
    assert!(
        error.contains("route `ghost-route` was not found"),
        "{error}"
    );
}

// ---------------------------------------------------------------- one factory per file

#[test]
fn a_route_id_that_collides_with_a_whole_provider_key_is_a_start_up_error() {
    // `anthropic-subscription` became a route file in ADR-0039 step 4 and
    // `openai-codex-subscription` in step 4b, so no shipped key is whole any more and
    // this loop is empty. The check stays compiled for the next whole provider (or a
    // test fake registered under such a key), and this test exercises it again as
    // soon as one is added.
    for key in p1_host::catalog::WHOLE_PROVIDERS {
        let scratch = Scratch::new();
        scratch.write_route(
            key,
            &route_file(
                key,
                "https://synthetic.example.test/v1/chat/completions",
                "retained-thinking",
                None,
                &[],
                &binding("synthetic-profile", "synthetic-profile"),
            ),
        );
        let harness = Harness::new(scratch.environment_dirs(), &[]);
        let completion = Arc::new(CompletionHub::new());
        let error = match build_catalog(&harness.deps, SandboxMode::Off, &[], &[], &[], &completion)
        {
            Ok(_) => panic!("a route id that shadows a whole provider must not build"),
            Err(error) => error,
        };
        assert!(
            error.contains(&format!(
                "route `{key}` collides with the compiled whole-provider key"
            )),
            "{error}"
        );
    }
}

#[test]
fn one_factory_per_route_file_serves_the_environment_that_names_it() {
    let scratch = Scratch::new();
    scratch.copy_shipped_routes();
    scratch.copy_shipped_profiles();
    scratch.write_environment(
        "go",
        &environment_file("opencode-go-subscription", "deepseek-v4.1-flash"),
    );
    scratch.write_environment("glm", &environment_file("glm-subscription", "glm-5.3"));
    scratch.write_environment(
        "kimi",
        &environment_file("kimi-coding-subscription", "kimi-k3"),
    );

    let assembled = assemble_scratch(&scratch, "go").expect("the shipped route file serves it");
    assert_eq!(assembled.resolved.family, "deepseek");
    assert_eq!(
        assembled.resolved.route.origin,
        Origin {
            route: "openai-chat/opencode-go-subscription".into(),
            model: "deepseek-v4.1-flash".into(),
        }
    );

    let assembled = assemble_scratch(&scratch, "glm").expect("the shipped route file serves it");
    assert_eq!(assembled.resolved.family, "glm");
    assert_eq!(
        assembled.resolved.route.origin,
        Origin {
            route: "openai-chat/glm-subscription".into(),
            model: "glm-5.3".into(),
        }
    );

    let assembled = assemble_scratch(&scratch, "kimi").expect("the shipped route file serves it");
    assert_eq!(assembled.resolved.family, "kimi");
    assert_eq!(
        assembled.resolved.route.origin,
        Origin {
            route: "openai-chat/kimi-coding-subscription".into(),
            model: "k3".into(),
        }
    );
}

// -------------------------------------------------------------------- profile binding

#[test]
fn a_route_refuses_a_profile_it_does_not_serve() {
    let scratch = Scratch::new();
    scratch.write_profile("served-profile", "");
    scratch.write_profile("other-profile", "");
    scratch.write_route(
        "serving-route",
        &route_file(
            "serving-route",
            "https://synthetic.example.test/v1/chat/completions",
            "retained-thinking",
            None,
            &[],
            &binding("served-profile", "served-profile"),
        ),
    );
    scratch.write_route(
        "empty-route",
        &route_file(
            "empty-route",
            "https://synthetic.example.test/v1/chat/completions",
            "retained-thinking",
            None,
            &[],
            "",
        ),
    );
    scratch.write_environment(
        "unserved",
        &environment_file("serving-route", "other-profile"),
    );
    scratch.write_environment("empty", &environment_file("empty-route", "other-profile"));

    let error = scratch.resolve("unserved").unwrap_err();
    assert_eq!(
        error,
        "route \"serving-route\" does not serve profile \"other-profile\" \
         (it serves: served-profile)"
    );
    // The same refusal reaches the caller through assembly, before any provider exists.
    let error = assemble_scratch(&scratch, "unserved").unwrap_err();
    assert!(
        error.contains("does not serve profile \"other-profile\""),
        "{error}"
    );
    let error = scratch.resolve("empty").unwrap_err();
    assert_eq!(
        error,
        "route \"empty-route\" does not serve profile \"other-profile\" (it serves: none)"
    );
}

#[test]
fn a_glm_profile_on_the_opencode_go_route_is_refused_until_the_file_binds_it() {
    let scratch = Scratch::new();
    scratch.copy_shipped_routes();
    scratch.copy_shipped_profiles();
    scratch.write_environment(
        "glm-on-go",
        &environment_file("opencode-go-subscription", "glm-5.3"),
    );

    let error = scratch.resolve("glm-on-go").unwrap_err();
    assert_eq!(
        error,
        "route \"opencode-go-subscription\" does not serve profile \"glm-5.3\" \
         (it serves: deepseek-v4.1-flash)"
    );

    // Bind it, with a dialect that can express preserved thinking, and the same route
    // serves it. The refusal was the file's, not the host's.
    let shipped = std::fs::read_to_string(repo("routes/opencode-go-subscription.toml"))
        .expect("the shipped route file");
    let bound = with(
        &shipped,
        "\"thinking-with-reasoning-alias\"",
        "\"retained-thinking\"",
    ) + "\n[models.\"glm-5.3\"]\nwire_model = \"glm-5.3\"\n";
    scratch.write_route("opencode-go-subscription", &bound);

    let assembled =
        assemble_scratch(&scratch, "glm-on-go").expect("the bound route serves the glm profile");
    assert_eq!(assembled.resolved.family, "glm");
    assert_eq!(
        assembled.resolved.route.origin,
        Origin {
            route: "openai-chat/opencode-go-subscription".into(),
            model: "glm-5.3".into(),
        }
    );
}

#[test]
fn the_wire_model_comes_from_the_route_file_not_the_profile() {
    let scratch = Scratch::new();
    scratch.write_profile("synthetic-profile", "");
    scratch.write_route(
        "alias-route",
        &route_file(
            "alias-route",
            "https://synthetic.example.test/v1/chat/completions",
            "retained-thinking",
            None,
            &[],
            &binding("synthetic-profile", "vendor/alias-name"),
        ),
    );
    scratch.write_environment(
        "alias",
        &environment_file("alias-route", "synthetic-profile"),
    );

    let resolved = scratch
        .resolve("alias")
        .expect("the route serves the profile");
    assert_eq!(resolved.profile.model_id, "synthetic-profile");
    assert_eq!(resolved.wire_model, "vendor/alias-name");
    // `Origin.model` is the wire model, so a recorded origin keeps its meaning.
    let provider = provider_of(&resolved, ScriptedTransport::new(vec![]));
    assert_eq!(
        provider.describe().origin,
        Origin {
            route: "openai-chat/alias-route".into(),
            model: "vendor/alias-name".into(),
        }
    );
}

#[test]
fn an_output_limit_lowers_the_profile_ceiling_and_never_raises_it() {
    let scratch = Scratch::new();
    scratch.write_profile("limited-profile", "max_output_tokens = 131072\n");
    scratch.write_route(
        "lowered-route",
        &route_file(
            "lowered-route",
            "https://synthetic.example.test/v1/chat/completions",
            "retained-thinking",
            None,
            &[],
            &limited_binding("limited-profile", "limited-profile", 100_000),
        ),
    );
    scratch.write_route(
        "raised-route",
        &route_file(
            "raised-route",
            "https://synthetic.example.test/v1/chat/completions",
            "retained-thinking",
            None,
            &[],
            &limited_binding("limited-profile", "limited-profile", 200_000),
        ),
    );
    scratch.write_environment(
        "lowered",
        &environment_file("lowered-route", "limited-profile"),
    );
    scratch.write_environment(
        "raised",
        &environment_file("raised-route", "limited-profile"),
    );

    for (environment, ceiling, below_is_allowed) in
        [("lowered", 100_000, false), ("raised", 131_072, true)]
    {
        let resolved = scratch
            .resolve(environment)
            .expect("the route serves the profile");
        assert_eq!(
            resolved.profile.max_output_tokens,
            Some(131_072),
            "the route lowers the effective limit, never the profile"
        );
        let binding = resolved.route.binding("limited-profile").unwrap();
        let route = chat_route(&resolved.route, binding, &resolved.profile).unwrap();
        assert_eq!(
            route.limits.max_output_tokens,
            Some(ceiling),
            "{environment}"
        );
        // The adapter enforces the lowered ceiling, and an explicit cap above it is an
        // error, never a silent clamp.
        assert_eq!(
            build_request(
                &route,
                &binding.wire_model,
                &resolved.profile,
                &request_with(120_000)
            )
            .is_ok(),
            below_is_allowed,
            "{environment}"
        );
        assert!(
            build_request(
                &route,
                &binding.wire_model,
                &resolved.profile,
                &request_with(ceiling + 1)
            )
            .is_err(),
            "{environment}"
        );
    }

    // `context_limit` is parsed and carried; nothing consumes it yet.
    scratch.write_route(
        "context-route",
        &route_file(
            "context-route",
            "https://synthetic.example.test/v1/chat/completions",
            "retained-thinking",
            None,
            &[],
            &with(
                &binding("limited-profile", "limited-profile"),
                "wire_model = \"limited-profile\"",
                "wire_model = \"limited-profile\"\ncontext_limit = 200000",
            ),
        ),
    );
    scratch.write_environment(
        "context",
        &environment_file("context-route", "limited-profile"),
    );
    let resolved = scratch
        .resolve("context")
        .expect("a context limit still resolves");
    let binding = resolved.route.binding("limited-profile").unwrap();
    assert_eq!(binding.context_limit, Some(200_000));
    let route = chat_route(&resolved.route, binding, &resolved.profile).unwrap();
    assert_eq!(route.limits.max_output_tokens, Some(131_072));
}

// ------------------------------------------------------------------------- the split

const PROFILE: &str = "synthetic-profile";
const ALPHA_ENDPOINT: &str = "https://alpha.example.test/v1/chat/completions";
const BETA_ENDPOINT: &str = "https://beta.example.test/v1/chat/completions";
const ALPHA_ORIGIN: &str = "synthetic/alpha-account";
const BETA_ORIGIN: &str = "synthetic/beta-account";
const ALPHA_WIRE: &str = "vendor/alpha-alias";
const BETA_WIRE: &str = "vendor/beta-alias";

/// ONE profile reached by TWO routes that declare a different endpoint, a different
/// static header, a different session header, a different wire model and a different
/// origin — and the same dialect, so the lowering itself must not change.
fn split_scratch() -> Scratch {
    let scratch = Scratch::new();
    scratch.write_profile(PROFILE, "");
    scratch.write_route(
        "alpha",
        &with(
            &route_file(
                "alpha",
                ALPHA_ENDPOINT,
                "thinking-with-reasoning-alias",
                Some("x-alpha-session"),
                &[("x-account", "alpha")],
                &binding(PROFILE, ALPHA_WIRE),
            ),
            "openai-chat/alpha",
            ALPHA_ORIGIN,
        ),
    );
    scratch.write_route(
        "beta",
        &with(
            &route_file(
                "beta",
                BETA_ENDPOINT,
                "thinking-with-reasoning-alias",
                Some("x-beta-session"),
                &[("x-account", "beta")],
                &binding(PROFILE, BETA_WIRE),
            ),
            "openai-chat/beta",
            BETA_ORIGIN,
        ),
    );
    scratch.write_environment("alpha-env", &environment_file("alpha", PROFILE));
    scratch.write_environment("beta-env", &environment_file("beta", PROFILE));
    scratch
}

#[tokio::test]
async fn two_routes_that_share_one_profile_differ_only_in_their_declared_fields() {
    let scratch = split_scratch();
    let mut request = ProviderRequest {
        system_prompt: "synthetic prompt".into(),
        history: vec![Item::User {
            text: "synthetic question".into(),
        }],
        tools: Vec::new(),
        options: ModelOptions::default(),
    };
    request.options.reasoning_effort = Some(Effort::Max);
    request.options.max_output_tokens = Some(500);
    request.options.cache_key = Some("synthetic-session".into());

    let mut bodies = Vec::new();
    let mut origins = Vec::new();
    for (environment, id, endpoint, own_session, other_session, account, wire) in [
        (
            "alpha-env",
            "alpha",
            ALPHA_ENDPOINT,
            "x-alpha-session",
            "x-beta-session",
            "alpha",
            ALPHA_WIRE,
        ),
        (
            "beta-env",
            "beta",
            BETA_ENDPOINT,
            "x-beta-session",
            "x-alpha-session",
            "beta",
            BETA_WIRE,
        ),
    ] {
        let resolved = scratch
            .resolve(environment)
            .expect("the route serves the profile");
        assert_eq!(resolved.wire_model, wire, "{id}");
        let transport =
            ScriptedTransport::new(vec![ScriptedResponse::ok_sse(chat_fixtures::NO_USAGE)]);
        let provider = provider_of(&resolved, transport.clone());
        let events = drain(&provider, request.clone()).await;
        assert!(
            matches!(
                events.last(),
                Some(StreamEvent::Finished(Outcome::Completed(_)))
            ),
            "{id}: {events:?}"
        );

        let seen = transport.requests();
        assert_eq!(seen.len(), 1, "{id}");
        assert_eq!(seen[0].url, endpoint, "{id}");
        let header = |name: &str| {
            seen[0]
                .headers
                .iter()
                .find(|(header, _)| header.as_str() == name)
                .map(|(_, value)| value.as_str())
        };
        assert_eq!(header("x-account"), Some(account), "{id}");
        assert_eq!(header(own_session), Some("synthetic-session"), "{id}");
        assert_eq!(header(other_session), None, "{id}");
        assert!(
            header("user-agent").is_some_and(|value| value.starts_with("p1/")),
            "{id}: the compiled user-agent still travels"
        );
        assert_eq!(
            header("authorization"),
            Some(format!("Bearer {BEARER}").as_str()),
            "{id}: the credential comes from the credential source, not the file"
        );
        bodies.push(
            serde_json::from_slice::<serde_json::Value>(&seen[0].body).expect("the body is JSON"),
        );
        origins.push(provider.describe().origin);
    }

    // The wire model is the route's, and it is the only body field the two differ in.
    assert_eq!(bodies[0]["model"], serde_json::json!(ALPHA_WIRE));
    assert_eq!(bodies[1]["model"], serde_json::json!(BETA_WIRE));
    let mut stripped = Vec::new();
    for mut body in bodies {
        body.as_object_mut()
            .expect("a chat body is an object")
            .remove("model");
        stripped.push(body);
    }
    assert_eq!(
        stripped[0], stripped[1],
        "the same profile lowers to the same body on both routes"
    );

    assert_eq!(
        origins[0],
        Origin {
            route: ALPHA_ORIGIN.into(),
            model: ALPHA_WIRE.into(),
        }
    );
    assert_eq!(
        origins[1],
        Origin {
            route: BETA_ORIGIN.into(),
            model: BETA_WIRE.into(),
        }
    );
    assert_ne!(origins[0], origins[1], "two accounts are two origins");
}

/// A session recorded on one route continues on the other when that route's adapter
/// accepts the recorded history (ADR-0049, superseding ADR-0033's refusal): the
/// origin change is reported as an environment change, committed at the next turn.
#[tokio::test]
async fn a_session_recorded_on_one_route_resumes_on_the_other_as_an_environment_change() {
    let scratch = split_scratch();
    let alpha = scratch
        .resolve("alpha-env")
        .expect("alpha serves the profile");
    let beta = scratch
        .resolve("beta-env")
        .expect("beta serves the profile");

    let transport =
        ScriptedTransport::new(vec![ScriptedResponse::ok_sse(chat_fixtures::TEXT_TURN)]);
    let records = recorded_turn(provider_of(&alpha, transport)).await;
    assert!(!records.is_empty(), "the turn was journalled");

    let (_, report) = Agent::resume(
        parts(
            provider_of(&beta, ScriptedTransport::new(vec![])),
            Arc::new(RecordingJournal::new()),
        ),
        &records,
    )
    .expect("beta's adapter carries alpha's history");
    assert!(report.environment_changed, "another origin is a change");
}

/// A helper kept next to the split tests: the endpoint a route file declares is what the
/// transport is asked for, with nothing appended.
#[test]
fn a_declared_endpoint_is_used_verbatim() {
    let scratch = split_scratch();
    let alpha = scratch
        .resolve("alpha-env")
        .expect("alpha serves the profile");
    let beta = scratch
        .resolve("beta-env")
        .expect("beta serves the profile");
    assert_eq!(alpha.route.endpoint, ALPHA_ENDPOINT);
    assert_eq!(beta.route.endpoint, BETA_ENDPOINT);
    assert_eq!(
        alpha.route.credential.env.as_deref(),
        Some("P1_ROUTE_FILES_TEST_KEY")
    );
}

// ---------------------------------------------- the Responses `transport` setting

/// A scratch root holding the shipped Codex route and the profile it serves, with
/// one environment selecting them: the Responses adapter's own composition path.
fn codex_scratch() -> Scratch {
    let scratch = Scratch::new();
    scratch.copy_shipped_routes();
    scratch.copy_shipped_profiles();
    scratch.write_environment(
        "codex",
        &environment_file("openai-codex-subscription", "gpt-5.6-sol"),
    );
    scratch
}

/// The transport setting the SHIPPED Responses route file carries (ADR-0047 §1:
/// WebSocket is the default wherever a route supports it).
const SHIPPED_TRANSPORT: &str = "transport = \"websocket\"";

/// The shipped Responses route file with its `transport` line replaced by
/// `settings`; an empty `settings` removes the line, which is the "absent means
/// sse" case.
fn codex_route_with(scratch: &Scratch, settings: &str) {
    let shipped = std::fs::read_to_string(repo("routes/openai-codex-subscription.toml"))
        .expect("the shipped route file");
    let body = with(&shipped, SHIPPED_TRANSPORT, settings);
    scratch.write_route("openai-codex-subscription", &body);
}

fn codex_settings(scratch: &Scratch) -> AdapterSettings {
    load_route(&scratch.route_path("openai-codex-subscription"))
        .expect("the route loads")
        .settings()
        .expect("the adapter parses its settings")
}

/// ADR-0047 §1: `transport` is route data. The SHIPPED route file asks for
/// `websocket` — the owner decision of 2026-09-21 made WebSocket the default
/// wherever a route supports it — and a route file WITHOUT the key is `sse`.
#[test]
fn a_responses_route_declares_its_transport_and_absent_means_sse() {
    let scratch = codex_scratch();
    let shipped = std::fs::read_to_string(repo("routes/openai-codex-subscription.toml"))
        .expect("the shipped route file");
    assert!(
        shipped.contains(SHIPPED_TRANSPORT),
        "ADR-0047 §1: the shipped Codex route asks for WebSocket"
    );

    assert_eq!(
        codex_settings(&scratch),
        AdapterSettings::OpenAiResponses(ResponsesAdapterSettings {
            account: ResponsesAccount::CodexSubscription,
            transport: ResponsesTransport::Websocket,
        })
    );
    let route = load_route(&scratch.route_path("openai-codex-subscription")).unwrap();
    assert_eq!(
        responses_route(&route)
            .expect("the route composes")
            .transport,
        ResponsesTransport::Websocket
    );

    // Absent means `sse`: the adapter's own default for a route file that says
    // nothing, so a new Responses route opts in explicitly.
    codex_route_with(&scratch, "");
    assert_eq!(
        codex_settings(&scratch),
        AdapterSettings::OpenAiResponses(ResponsesAdapterSettings {
            account: ResponsesAccount::CodexSubscription,
            transport: ResponsesTransport::Sse,
        })
    );

    for (value, expected) in [
        ("sse", ResponsesTransport::Sse),
        ("websocket", ResponsesTransport::Websocket),
    ] {
        codex_route_with(&scratch, &format!("transport = \"{value}\""));
        assert_eq!(
            codex_settings(&scratch),
            AdapterSettings::OpenAiResponses(ResponsesAdapterSettings {
                account: ResponsesAccount::CodexSubscription,
                transport: expected,
            }),
            "{value}"
        );
        // The route the provider is composed from carries it: the transport is
        // route data, never a compiled decision.
        let route = load_route(&scratch.route_path("openai-codex-subscription")).unwrap();
        assert_eq!(
            responses_route(&route)
                .expect("the route composes")
                .transport,
            expected,
            "{value}"
        );
    }
}

/// Any other value fails the load, like every other unknown setting (ADR-0047 §1).
#[test]
fn an_unknown_transport_is_a_route_file_error() {
    for value in ["quic", "WebSocket", "ws", ""] {
        let scratch = codex_scratch();
        codex_route_with(&scratch, &format!("transport = \"{value}\""));
        let error = load_route(&scratch.route_path("openai-codex-subscription"))
            .expect_err("only the two documented values load");
        assert!(
            error.contains("invalid `[adapter_settings]`"),
            "{value}: {error}"
        );
        assert!(
            error.contains("websocket") && error.contains("sse"),
            "{value}: the error names the values that exist: {error}"
        );
    }
}

/// The host composes the real connector for a route that asks for WebSocket —
/// assembly succeeds, which it cannot do without one (the provider refuses a
/// WebSocket route that has none) — and hands the injected one to
/// `catalog::route_provider`. Composition opens no socket and reads no credential.
#[test]
fn a_websocket_route_assembles_with_the_real_connector_and_an_unchanged_origin() {
    let scratch = codex_scratch();
    codex_route_with(&scratch, "transport = \"websocket\"");

    let assembled = assemble_scratch(&scratch, "codex").expect("the route composes");
    assert_eq!(
        assembled.resolved.route.origin,
        Origin {
            route: "openai-responses/codex-subscription".into(),
            model: "gpt-5.6-sol".into(),
        },
        "the transport is not part of a response's origin (§7): the same route keeps the same origin"
    );

    // The same through the catalog factory the host itself uses, with the connector
    // injected like the transport (ADR-0047 §1): a test never opens a socket.
    let resolved = scratch
        .resolve("codex")
        .expect("the route serves the profile");
    let binding = resolved
        .route
        .binding(&resolved.profile.id)
        .expect("the route serves this profile");
    let connector = Arc::new(RefusingWsConnector::default());
    let provider = route_provider(
        &resolved.route,
        binding,
        resolved.profile.clone(),
        Arc::new(ScriptedTransport::new(Vec::new())),
        connector.clone(),
        Arc::new(Fixed),
    )
    .expect("a websocket route composes with the connector it is handed");
    assert_eq!(
        provider.describe().origin,
        Origin {
            route: "openai-responses/codex-subscription".into(),
            model: "gpt-5.6-sol".into(),
        }
    );
    assert_eq!(connector.handshakes(), 0, "composition opens no socket");
}

/// ADR-0047 §1: a route that does NOT ask for WebSocket ignores the connector. It
/// composes through `route_provider` with a connector in hand (the provider REFUSES
/// a connector on an SSE route, so handing it over would fail right here), and a
/// whole turn on such a route is served by the scripted SSE transport with the
/// connector never reached.
#[tokio::test]
async fn a_route_that_does_not_ask_for_websocket_ignores_the_connector() {
    // A Responses route with no `transport` line: the adapter's `sse` default.
    let scratch = codex_scratch();
    codex_route_with(&scratch, "");
    let resolved = scratch
        .resolve("codex")
        .expect("the route serves the profile");
    let binding = resolved
        .route
        .binding(&resolved.profile.id)
        .expect("the route serves this profile");
    let provider = route_provider(
        &resolved.route,
        binding,
        resolved.profile.clone(),
        Arc::new(ScriptedTransport::new(Vec::new())),
        Arc::new(RefusingWsConnector::default()),
        Arc::new(Fixed),
    )
    .expect("an SSE route composes: the connector is ignored");
    assert_eq!(provider.describe().origin.model, "gpt-5.6-sol");

    // A whole turn on a route of another family, driven to its terminal event by the
    // scripted transport: the connector sees no handshake at all.
    let resolved = shipped("deepseek");
    let binding = resolved
        .route
        .binding(&resolved.profile.id)
        .expect("the route serves this profile");
    let connector = Arc::new(RefusingWsConnector::default());
    let transport =
        ScriptedTransport::new(vec![ScriptedResponse::ok_sse(chat_fixtures::TEXT_TURN)]);
    let provider = route_provider(
        &resolved.route,
        binding,
        resolved.profile.clone(),
        Arc::new(transport.clone()),
        connector.clone(),
        Arc::new(Fixed),
    )
    .expect("a chat route composes");
    let request = ProviderRequest {
        system_prompt: "SYS".to_string(),
        history: vec![Item::User {
            text: "hi".to_string(),
        }],
        tools: Vec::new(),
        options: ModelOptions::default(),
    };
    let events = drain(&provider, request).await;
    assert!(
        matches!(
            events.last(),
            Some(StreamEvent::Finished(Outcome::Completed(_)))
        ),
        "{events:?}"
    );
    assert_eq!(
        transport.requests().len(),
        1,
        "the SSE path served the turn"
    );
    assert_eq!(connector.handshakes(), 0, "the connector was never reached");
}
