//! S4.9: provider activation (ADR-0086). A route file's `adapter` key names a provider
//! component, the component is configured from the route's own data and the selected
//! profile's text, and a route that cannot activate one is refused before the first turn with
//! a sentence naming the route and the module.
//!
//! There is no native fallback any more: a host with no release module set, and a release that
//! does not ship the component, both refuse. Since S7.10-R5 that includes the Responses route's
//! WebSocket transport, which its component lowers and the broker sends (ADR-0078).
//!
//! The components are the ones `scripts/build-modules.sh --all` published, read through a
//! release manifest written into a temp directory (`common::provider_release`), the way S4.7's
//! `provider_conformance.rs` loads the same artifacts. Production discovery is the ONE path,
//! `catalog::modules::official_release_manifest` (with S3.8.0's debug fallback once it is on
//! main), so this file adds none.
//!
//! Nothing here opens a socket or reads a credential: the transport is scripted and the
//! credential source is a fixture.

mod common;
mod native_routes;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use common::{built_provider_packages, provider_release};
use native_routes::{chat_route, messages_route, responses_route};
use p1_assembly::{Catalog, ModulesLock, ToolServices};
use p1_contracts::serde_json::{self, Value, json};
use p1_contracts::{BoxFuture, Provider, ProviderError};
use p1_host::catalog::modules::{load_locked_modules, register_modules};
use p1_host::catalog::{ProviderComponents, provider_component, route_provider};
use p1_host::routes::{
    AdapterSettings, ModelBinding, ResponsesTransport, RouteFile, load_route, load_route_by_id,
};
use p1_model_profile::ModelProfile;
use p1_module_runtime::Services;
use p1_module_tests::Release;
use p1_provider_http::testing::{RefusingWsConnector, ScriptedTransport, ScriptedWsConnector};
use p1_provider_http::ws::WsConnector;
use p1_provider_http::{Credential, CredentialSource, Transport};

const BEARER: &str = "ACTIVATION-FAKE-BEARER";
const ACCOUNT: &str = "acct-activation";

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// The environment search directories the host runs with in the repository.
fn environment_dirs() -> Vec<PathBuf> {
    vec![repo_root().join("environments")]
}

/// A release laid out in a temp directory, with `edit` applied to the entry of every provider
/// package (`name` is the module's manifest name): the loader reads it as an installation does.
fn release(edit: &dyn Fn(&mut Value)) -> Release {
    provider_release(edit)
}

/// The release the build published, unedited.
fn shipped_release() -> Release {
    release(&|_| {})
}

/// The entry of `name`, tampered so the loader must refuse it.
fn release_with(name: &str, edit: &dyn Fn(&mut Value)) -> Release {
    release(&|entry| {
        if entry["name"] == name {
            edit(entry);
        }
    })
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

/// The shipped profile `id`, as the environment that selects it loads it.
fn shipped_profile(id: &str) -> Arc<ModelProfile> {
    let path = repo_root().join("profiles").join(format!("{id}.toml"));
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("{}: {error}", path.display()));
    Arc::new(ModelProfile::from_toml(id, &text).unwrap_or_else(|error| panic!("{id}: {error}")))
}

/// The shipped route `id`.
fn shipped_route(id: &str) -> RouteFile {
    let path = repo_root().join("routes").join(format!("{id}.toml"));
    load_route(&path).unwrap_or_else(|error| panic!("{error}"))
}

/// The native adapter built from the same route file and profile, the way `route_provider` built
/// it before this slice: the route value the component's `describe` is compared with. The three
/// adapter crates are still shipped, and S4.7's conformance suite compares the component with
/// them the same way; no production path builds a native adapter for these routes any more.
fn native(
    route: &RouteFile,
    binding: &ModelBinding,
    profile: Arc<ModelProfile>,
    transport: Arc<dyn Transport>,
    ws: Arc<dyn WsConnector>,
    credentials: Arc<dyn CredentialSource>,
) -> Result<Arc<dyn Provider>, String> {
    match route.settings()? {
        AdapterSettings::OpenAiChat(_) => {
            let provider = p1_provider_openai_chat::ChatProvider::new(
                chat_route(route, binding, &profile)?,
                &binding.wire_model,
                profile,
                transport,
                credentials,
            )
            .map_err(|error| error.to_string())?;
            Ok(Arc::new(provider))
        }
        AdapterSettings::AnthropicMessages(_) => {
            let provider = p1_provider_anthropic::AnthropicProvider::new(
                messages_route(route)?,
                &binding.wire_model,
                profile,
                transport,
                credentials,
            )
            .map_err(|error| error.to_string())?;
            Ok(Arc::new(provider))
        }
        AdapterSettings::OpenAiResponses(settings) => {
            let composition = p1_provider_openai::OpenAiCodexProvider::builder(
                responses_route(route)?,
                &binding.wire_model,
                profile,
                transport,
                credentials,
            );
            let composition = if settings.transport == ResponsesTransport::Websocket {
                composition.with_ws_connector(ws)
            } else {
                composition
            };
            Ok(Arc::new(
                composition.build().map_err(|error| error.to_string())?,
            ))
        }
    }
}

/// Activate `route` for `profile` through `components`: the provider or the activation
/// refusal, and the scripted transport the caller checks stayed silent.
fn activate_with(
    components: &ProviderComponents,
    route: &RouteFile,
    profile: &str,
) -> (Result<Arc<dyn Provider>, String>, ScriptedTransport) {
    activate_in(components, &environment_dirs(), route, profile)
}

/// As [`activate_with`], for the environment search directories `dirs`: activation reads the
/// lock next to them and the profile beside them, so a case can hand it its own.
fn activate_in(
    components: &ProviderComponents,
    dirs: &[PathBuf],
    route: &RouteFile,
    profile: &str,
) -> (Result<Arc<dyn Provider>, String>, ScriptedTransport) {
    let transport = ScriptedTransport::new(Vec::new());
    let provider = components.activate(
        dirs,
        route,
        shipped_profile(profile),
        Arc::new(transport.clone()),
        Arc::new(RefusingWsConnector::default()),
        Arc::new(FixedCredentials),
    );
    (provider, transport)
}

/// As [`activate_with`], for the release `manifest`: one component set per call.
fn activate(
    manifest: &Path,
    route: &RouteFile,
    profile: &str,
) -> (Result<Arc<dyn Provider>, String>, ScriptedTransport) {
    let components = ProviderComponents::read(manifest).expect("a release");
    activate_with(&components, route, profile)
}

/// The refusal of activating `route` for `profile` through `release`, and the proof that no
/// request was sent: an activation refusal is reported before the first turn.
fn refusal(release: &Release, route: &RouteFile, profile: &str) -> String {
    let (activated, transport) = activate(&release.manifest_file(), route, profile);
    let error = activated
        .err()
        .unwrap_or_else(|| panic!("{}: the route activated", route.id));
    assert!(
        transport.requests().is_empty(),
        "{}: a request was sent before the refusal",
        route.id
    );
    error
}

/// The native provider for `route` and `profile`, for a case that holds a comparison against
/// the component's own route value.
fn native_of(route: &RouteFile, profile: &str) -> Arc<dyn Provider> {
    let profile = shipped_profile(profile);
    let binding = route.binding(&profile.id).expect("a bound profile");
    native(
        route,
        binding,
        profile,
        Arc::new(ScriptedTransport::new(Vec::new())),
        Arc::new(ScriptedWsConnector::new(Vec::new())),
        Arc::new(FixedCredentials),
    )
    .unwrap_or_else(|error| panic!("{}: native: {error}", route.id))
}

#[tokio::test(start_paused = true)]
async fn route_retry_preset_reaches_the_component_broker_without_changing_other_routes() {
    use futures_util::StreamExt;
    use p1_contracts::{CancellationToken, Outcome, ProviderRequest, StreamEvent};
    use p1_host::routes::RouteRetryPolicy;
    use p1_provider_http::testing::{BodyEnd, ScriptedResponse};
    let release = shipped_release();
    let components = ProviderComponents::read(&release.manifest_file()).unwrap();
    for (preset, retries, hint) in [
        (RouteRetryPolicy::Deepseek, 5, None),
        (RouteRetryPolicy::Default, 3, None),
        (RouteRetryPolicy::Deepseek, 0, Some("11")),
        (RouteRetryPolicy::Default, 3, Some("11")),
    ] {
        let mut route = shipped_route("opencode-go-subscription");
        route.retry_policy = preset;
        let response = ScriptedResponse {
            status: 503,
            headers: hint
                .map(|value| vec![("Retry-After".into(), value.into())])
                .unwrap_or_default(),
            chunks: Vec::new(),
            end: BodyEnd::Eof,
        };
        let transport = ScriptedTransport::new(vec![response; 6]);
        let provider = components
            .activate(
                &environment_dirs(),
                &route,
                shipped_profile("deepseek-v4.1-flash"),
                Arc::new(transport.clone()),
                Arc::new(RefusingWsConnector::default()),
                Arc::new(FixedCredentials),
            )
            .unwrap();
        let mut stream = provider
            .stream(
                ProviderRequest {
                    system_prompt: "test".into(),
                    history: Vec::new(),
                    tools: Vec::new(),
                    options: Default::default(),
                },
                CancellationToken::new(),
            )
            .await
            .unwrap();
        let mut waits = Vec::new();
        let mut outcome = None;
        while let Some(event) = stream.next().await {
            match event {
                StreamEvent::Wait {
                    attempt, delay_ms, ..
                } => waits.push((attempt, delay_ms)),
                StreamEvent::Finished(value) => outcome = Some(value),
                _ => {}
            }
        }
        assert_eq!(transport.requests().len(), retries + 1);
        assert_eq!(waits.len(), retries);
        assert!(matches!(outcome, Some(Outcome::Failed(error)) if error.message.contains("503")));
        if preset == RouteRetryPolicy::Deepseek && hint.is_none() {
            assert!(
                (450..=550).contains(&waits[0].1),
                "route base must not remain 2 s"
            );
        }
    }
}

#[test]
fn a_route_names_a_provider_component_and_profiles_stay_data() {
    assert_eq!(
        provider_component("anthropic-messages").expect("a component"),
        "p1/provider-anthropic"
    );
    assert_eq!(
        provider_component("openai-responses").expect("a component"),
        "p1/provider-openai"
    );
    assert_eq!(
        provider_component("openai-chat").expect("a component"),
        "p1/provider-openai-chat"
    );
    // An adapter key no component names is refused, and the refusal lists those that have one.
    let error = provider_component("openai-embeddings").expect_err("no component");
    assert!(
        error.contains("anthropic-messages") && error.contains("openai-chat"),
        "{error}"
    );

    // Every adapter a route file may name has a component, and the shipped routes name them.
    let routes =
        p1_host::routes::load_routes(&repo_root().join("routes")).expect("the shipped routes load");
    assert!(!routes.is_empty(), "no shipped route was found");
    let mut adapters: Vec<&str> = Vec::new();
    for route in &routes {
        let component = provider_component(&route.adapter)
            .unwrap_or_else(|error| panic!("{}: {error}", route.id));
        assert!(
            built_provider_packages()
                .iter()
                .any(|package| package.name == component),
            "{}: {component} is not a built provider package",
            route.id
        );
        if !adapters.contains(&route.adapter.as_str()) {
            adapters.push(route.adapter.as_str());
        }
    }
    adapters.sort_unstable();
    let mut expected = p1_host::routes::ADAPTER_KEYS.to_vec();
    expected.sort_unstable();
    assert_eq!(adapters, expected, "every adapter has a shipped route");

    // A profile stays data: the component is configured with the profile's own file text, and
    // the route value it describes is the one the native adapter builds from the same files.
    let release = shipped_release();
    let components = ProviderComponents::read(&release.manifest_file()).expect("the built release");
    for (route_id, profile) in [
        ("glm-subscription", "glm-5.3"),
        ("anthropic-subscription", "claude-sonnet-5"),
    ] {
        let route = shipped_route(route_id);
        let (activated, transport) = activate_with(&components, &route, profile);
        let activated = activated.unwrap_or_else(|error| panic!("{route_id}: {error}"));
        assert!(
            transport.requests().is_empty(),
            "{route_id}: activation sent a request"
        );
        assert_eq!(
            activated.describe(),
            native_of(&route, profile).describe(),
            "{route_id}"
        );
    }
}

#[test]
fn activation_refuses_an_unsupported_declaration_a_missing_capability_and_an_invalid_setting() {
    let route = shipped_route("glm-subscription");
    let profile = "glm-5.3";
    let component = provider_component(&route.adapter).expect("a component");
    let names = |error: &str| {
        assert!(
            error.contains(&route.id) && error.contains(component),
            "the refusal must name the route and the module: {error}"
        );
    };

    // An unsupported declaration: the entry names another class's world, which the loader
    // refuses before anything is compiled.
    let wrong_world = release_with(component, &|entry| {
        entry["world"] = json!("p1:module/tool@1.0.0");
    });
    let error = refusal(&wrong_world, &route, profile);
    names(&error);
    assert!(error.contains("world"), "{error}");

    // An unknown protocol major is the same shape of refusal.
    let wrong_protocol = release_with(component, &|entry| {
        entry["protocol"] = json!("2.0");
    });
    let error = refusal(&wrong_protocol, &route, profile);
    names(&error);
    assert!(error.contains("protocol"), "{error}");

    // A missing capability, first half: a manifest that does not grant `http` while the
    // component imports it. The loader refuses it before anything is compiled.
    let without_http = release_with(component, &|entry| {
        entry["capabilities"] = json!(["websocket", "credential-control"]);
    });
    let error = refusal(&without_http, &route, profile);
    names(&error);
    assert!(error.contains("p1:module/http"), "{error}");

    // Second half: a manifest that grants a capability a provider component is not linked
    // with. The world a provider implements imports `http`, `websocket` and
    // `credential-control` only, so a manifest cannot omit one of them without omitting an
    // import the component has; that the adapter also refuses a manifest missing `http` or
    // `credential-control` outright is `missing_required`'s unit test in the runtime adapter.
    let granted_clock = release_with(component, &|entry| {
        entry["capabilities"] = json!(["http", "websocket", "credential-control", "clock"]);
    });
    let error = refusal(&granted_clock, &route, profile);
    names(&error);
    assert!(error.contains("clock"), "{error}");

    // An invalid provider setting: the route's `dialect` parses but cannot express the bound
    // profile's preserved thinking, and the same `validate_composition` the native
    // constructor runs makes `configure` return a provider error, so the provider is never
    // built and nothing is sent. The setting is one only a hand-written route file can reach:
    // `load_route` refuses a settings table the adapter cannot parse.
    let mut invalid = shipped_route("glm-subscription");
    invalid.adapter_settings = Some(
        serde_json::from_value(json!({"dialect": "thinking-with-reasoning-alias"}))
            .expect("an adapter settings table"),
    );
    let error = refusal(&shipped_release(), &invalid, profile);
    names(&error);
    assert!(error.contains("refused its settings"), "{error}");
    assert!(error.contains("preserved thinking"), "{error}");
}

#[test]
fn the_shipped_environments_resolve_their_provider_references_to_modules() {
    let release = shipped_release();
    let manifest = release.manifest_file();
    // One component set for every environment, as one catalog holds: a component is compiled
    // once and the routes that name its adapter configure the same bytes.
    let components = ProviderComponents::read(&manifest).expect("the built release");
    let environments = repo_root().join("environments");
    let mut through_the_component = 0;
    for entry in std::fs::read_dir(&environments).expect("the shipped environments") {
        let dir = entry.expect("an environment entry").path();
        if !dir.join("environment.toml").is_file() {
            continue;
        }
        let name = dir
            .file_name()
            .and_then(|name| name.to_str())
            .expect("an environment name")
            .to_owned();
        let environment = p1_assembly::load_environment(&name, &environment_dirs())
            .unwrap_or_else(|error| panic!("{name}: {error}"));
        let profile = environment
            .profile
            .clone()
            .unwrap_or_else(|| panic!("{name}: no profile"));
        let route = load_route_by_id(&environment_dirs(), &environment.provider)
            .unwrap_or_else(|error| panic!("{name}: {error}"));

        // The environment's provider reference resolves to a module the build ships.
        let component =
            provider_component(&route.adapter).unwrap_or_else(|error| panic!("{name}: {error}"));
        let package = built_provider_packages()
            .iter()
            .find(|package| package.name == component)
            .unwrap_or_else(|| panic!("{name}: {component} is not a built provider package"));
        assert_eq!(package.entry["kind"], "provider", "{name}");

        // It activates with a fake credential source and a scripted transport, and the route
        // value it describes is the native adapter's for the same route file and profile.
        let (activated, transport) = activate_with(&components, &route, &profile.id);
        let activated = activated.unwrap_or_else(|error| panic!("{name}: {error}"));
        assert!(
            transport.requests().is_empty(),
            "{name}: a request was sent"
        );
        assert_eq!(
            activated.describe(),
            native_of(&route, &profile.id).describe(),
            "{name}"
        );

        through_the_component += 1;
    }
    assert!(
        through_the_component > 10,
        "only {through_the_component} shipped environments took the component path"
    );
}

/// A test-only installation: an environments directory with `lock` written next to it and the
/// shipped profile `id` copied beside it. Activation reads the lock and the profile text from
/// there, so nothing here touches the shipped `modules.lock` or a real login.
fn scratch(lock: &str, profile: &str) -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("a scratch directory");
    std::fs::create_dir_all(dir.path().join("environments")).expect("the environments directory");
    std::fs::create_dir_all(dir.path().join("profiles")).expect("the profiles directory");
    std::fs::write(dir.path().join("modules.lock"), lock).expect("the test lock");
    std::fs::copy(
        repo_root().join("profiles").join(format!("{profile}.toml")),
        dir.path().join("profiles").join(format!("{profile}.toml")),
    )
    .expect("the shipped profile");
    dir
}

/// ANSWERS D083b: a user's lock may select a provider package for the module name an adapter's
/// component carries. `register_modules` accepts that package (it is never a tool), and
/// activation uses the module the lock selected in place of the release's host entry of the
/// same adapter. Nothing here reads the shipped `modules.lock`.
#[test]
fn a_lock_selected_provider_package_is_accepted_and_is_what_activation_uses() {
    // The module name the lock gives the adapter's component: its manifest name without the
    // reserved namespace, as `modules.lock` documents its keys.
    const MODULE: &str = "provider-anthropic";
    const PROFILE: &str = "claude-sonnet-5";

    // The built Anthropic component, published under a name of its own as the package a user's
    // lock selects: this release has no `p1/provider-anthropic` entry, so an activation that
    // took the release's host entry instead of the lock's package could not succeed.
    let package = &built_provider_packages()[0];
    assert_eq!(package.name, "p1/provider-anthropic");
    let mut entry = package.entry.clone();
    entry["name"] = json!("p1/anthropic-local");
    entry["path"] = json!("packages/p1-anthropic-local/p1-anthropic-local.wasm");
    let mut release = Release::empty();
    release.add(entry.clone(), &package.bytes);
    let manifest = release.manifest_file();

    let lock_text = p1_module_tests::lock_text(MODULE, &entry);
    let installation = scratch(&lock_text, PROFILE);
    let lock = ModulesLock::parse(&installation.path().join("modules.lock"), &lock_text)
        .expect("the test lock");

    // A lock that selects a provider package is accepted, and it takes no catalog key.
    let packages =
        load_locked_modules(&lock, &manifest).expect("the lock's provider package loads");
    let mut catalog = Catalog::new();
    register_modules(
        &mut catalog,
        packages,
        Arc::new(|_: &str, _: &ToolServices| Services::default()),
    )
    .expect("a lock-selected provider package is accepted");
    assert!(
        !catalog.tool_keys().contains(&MODULE.to_owned()),
        "a provider package is not a tool: {:?}",
        catalog.tool_keys()
    );

    // It is what activation uses: the route becomes the module the lock selected, and its
    // route value is the native adapter's for the same route file and profile.
    let components = ProviderComponents::read(&manifest).expect("the release");
    let route = shipped_route("anthropic-subscription");
    let dirs = vec![installation.path().join("environments")];
    let (activated, transport) = activate_in(&components, &dirs, &route, PROFILE);
    let activated = activated.unwrap_or_else(|error| panic!("{error}"));
    assert!(transport.requests().is_empty(), "activation sent a request");
    assert_eq!(activated.describe(), native_of(&route, PROFILE).describe());

    // The shipped environment directories select no provider package, and this release has no
    // host entry either, so activation refuses there: the component came through the lock.
    let (refused, _) = activate_with(&components, &route, PROFILE);
    let error = refused
        .err()
        .unwrap_or_else(|| panic!("{}: the route activated", route.id));
    assert!(
        error.contains(&route.id) && error.contains("p1/provider-anthropic"),
        "{error}"
    );
}

/// D083b, the native drop: every route a shipped environment names activates its release host
/// entry COMPONENT — or the package a user's lock selects — the Responses route's WebSocket
/// transport included (S7.10-R5). With a host that has no release module set at all, every
/// route therefore REFUSES, naming the route and the module it asked for: a missing module set
/// is never a fallback to a native adapter.
#[test]
fn a_route_activates_no_native_provider() {
    let components = ProviderComponents::none();
    let dirs = environment_dirs();
    let mut refused = 0;
    for entry in std::fs::read_dir(dirs[0].clone()).expect("the shipped environments") {
        let dir = entry.expect("an environment entry").path();
        if !dir.join("environment.toml").is_file() {
            continue;
        }
        let name = dir
            .file_name()
            .and_then(|name| name.to_str())
            .expect("an environment name")
            .to_owned();
        let environment = p1_assembly::load_environment(&name, &dirs)
            .unwrap_or_else(|error| panic!("{name}: {error}"));
        let profile = environment
            .profile
            .clone()
            .unwrap_or_else(|| panic!("{name}: no profile"));
        let route = load_route_by_id(&dirs, &environment.provider)
            .unwrap_or_else(|error| panic!("{name}: {error}"));
        let module =
            provider_component(&route.adapter).unwrap_or_else(|error| panic!("{name}: {error}"));
        let transport = ScriptedTransport::new(Vec::new());
        let binding = route.binding(&profile.id).expect("a bound profile");
        let activated = route_provider(
            &components,
            &dirs,
            &route,
            binding,
            profile.clone(),
            Arc::new(transport.clone()),
            Arc::new(ScriptedWsConnector::new(Vec::new())),
            Arc::new(FixedCredentials),
        );
        let error = activated
            .err()
            .unwrap_or_else(|| panic!("{name}: the route activated a native provider"));
        assert!(
            error.contains(&route.id) && error.contains(module),
            "{name}: the refusal must name the route and the module: {error}"
        );
        assert!(
            error.contains("no release module set"),
            "{name}: the refusal must say why: {error}"
        );
        assert!(
            transport.requests().is_empty(),
            "{name}: a request was sent"
        );
        refused += 1;
    }
    assert!(refused > 10, "only {refused} shipped environments refused");
}

/// A release that ships no provider component at all (an installation built before the
/// providers were added): activation refuses naming the module the route asked for, and no
/// request is sent.
#[test]
fn a_missing_release_module_refuses_activation_naming_the_module() {
    let empty = Release::empty();
    for (route_id, profile) in [
        ("glm-subscription", "glm-5.3"),
        ("anthropic-subscription", "claude-sonnet-5"),
    ] {
        let route = shipped_route(route_id);
        let module = provider_component(&route.adapter).expect("a component");
        let error = refusal(&empty, &route, profile);
        assert!(
            error.contains(&route.id) && error.contains(module),
            "{route_id}: the refusal must name the route and the module: {error}"
        );
        assert!(
            error.contains("not in the release manifest"),
            "{route_id}: the refusal must say why: {error}"
        );
    }
}
