//! S4.9: provider activation (ADR-0086). A route file's `adapter` key names a provider
//! component, the component is configured from the route's own data and the selected
//! profile's text, and a route that cannot activate one is refused before the first turn with
//! a sentence naming the route and the module.
//!
//! The components are the ones `scripts/build-modules.sh` published, read through a release
//! manifest written into a temp directory. Production discovery is S3.8.0's debug-build
//! fallback in `catalog::modules::official_release_manifest`, so this slice adds no second
//! path: the layout here is S0's `p1_module_tests::Release`, the same one S4.7's
//! `provider_conformance.rs` loads the same artifacts with.
//!
//! Nothing here opens a socket or reads a credential: the transport is scripted and the
//! credential source is a fixture. One shipped route keeps the native adapter — the
//! Responses route's WebSocket transport, which is S5.5's and is why this broker sends no
//! WebSocket lowering — and the case says so.

use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use p1_contracts::serde_json::{self, Value, json};
use p1_contracts::{BoxFuture, Provider, ProviderError};
use p1_host::catalog::{ProviderComponents, provider_component, route_provider};
use p1_host::routes::{AdapterSettings, RouteFile, load_route, load_route_by_id};
use p1_model_profile::ModelProfile;
use p1_module_tests::Release;
use p1_provider_http::testing::{ScriptedTransport, ScriptedWsConnector};
use p1_provider_http::{Credential, CredentialSource};

const BEARER: &str = "ACTIVATION-FAKE-BEARER";
const ACCOUNT: &str = "acct-activation";

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// The environment search directories the host runs with in the repository.
fn environment_dirs() -> Vec<PathBuf> {
    vec![repo_root().join("environments")]
}

/// One built provider package: the bytes the build published and the release entry its package
/// manifest describes.
struct Package {
    name: &'static str,
    bytes: Vec<u8>,
    entry: Value,
}

impl Package {
    /// A copy of this package's entry with `edit` applied, for a case that holds a refusal.
    fn edited(&self, edit: &dyn Fn(&mut Value)) -> Value {
        let mut entry = self.entry.clone();
        edit(&mut entry);
        entry
    }
}

/// The provider packages, as the build names and publishes them.
const PROVIDER_PACKAGES: [(&str, &str); 3] = [
    ("p1/provider-anthropic", "p1-module-provider-anthropic"),
    ("p1/provider-openai", "p1-module-provider-openai"),
    ("p1/provider-openai-chat", "p1-module-provider-openai-chat"),
];

fn read_package(name: &'static str, package: &str) -> Package {
    let dir = repo_root().join("modules/target/p1-modules").join(package);
    let read = |file: String| {
        let path = dir.join(file);
        std::fs::read(&path).unwrap_or_else(|error| {
            panic!(
                "the provider artifact {} is missing ({error}): run scripts/build-modules.sh first",
                path.display()
            )
        })
    };
    let manifest: Value = serde_json::from_slice(&read(format!("{package}.manifest.json")))
        .expect("a package manifest");
    assert_eq!(manifest["name"], name, "{package}");
    let file = name.replace('/', "-");
    Package {
        name,
        bytes: read(format!("{package}.wasm")),
        entry: json!({
            "name": name,
            "digest": manifest["digest"],
            "path": format!("packages/{file}/{file}.wasm"),
            "kind": manifest["kind"],
            "world": manifest["world"],
            "protocol": manifest["protocol"],
            "capabilities": manifest["capabilities"],
            "variant": manifest["variant"],
        }),
    }
}

/// The built provider packages, read once.
fn built() -> &'static [Package; 3] {
    static BUILT: OnceLock<[Package; 3]> = OnceLock::new();
    BUILT.get_or_init(|| PROVIDER_PACKAGES.map(|(name, package)| read_package(name, package)))
}

/// A release laid out in a temp directory, with `edit` applied to the entry of every package
/// (`name` is the module's manifest name): the loader reads it as an installation's host does.
fn release(edit: &dyn Fn(&mut Value)) -> Release {
    let mut release = Release::empty();
    for package in built() {
        release.add(package.edited(edit), &package.bytes);
    }
    release
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

/// Activate `route` for `profile` through `components`: the provider or the activation
/// refusal, and the scripted transport the caller checks stayed silent.
fn activate_with(
    components: &ProviderComponents,
    route: &RouteFile,
    profile: &str,
) -> (Result<Arc<dyn Provider>, String>, ScriptedTransport) {
    let transport = ScriptedTransport::new(Vec::new());
    let provider = components.activate(
        &environment_dirs(),
        route,
        shipped_profile(profile),
        Arc::new(transport.clone()),
        Arc::new(ScriptedWsConnector::new(Vec::new())),
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
            built().iter().any(|package| package.name == component),
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
        let native = route_provider(
            &route,
            route.binding(profile).expect("a bound profile"),
            shipped_profile(profile),
            Arc::new(transport.clone()),
            Arc::new(ScriptedWsConnector::new(Vec::new())),
            Arc::new(FixedCredentials),
        )
        .unwrap_or_else(|error| panic!("{route_id}: native: {error}"));
        assert_eq!(activated.describe(), native.describe(), "{route_id}");
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
    let mut native_websocket = 0;
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
        let package = built()
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
        let native = route_provider(
            &route,
            route.binding(&profile.id).expect("a bound profile"),
            profile.clone(),
            Arc::new(transport.clone()),
            Arc::new(ScriptedWsConnector::new(Vec::new())),
            Arc::new(FixedCredentials),
        )
        .unwrap_or_else(|error| panic!("{name}: native: {error}"));
        assert_eq!(activated.describe(), native.describe(), "{name}");

        if is_websocket_route(&route) {
            native_websocket += 1;
        } else {
            through_the_component += 1;
        }
    }
    assert!(
        through_the_component > 10,
        "only {through_the_component} shipped environments took the component path"
    );
    assert_eq!(
        native_websocket, 1,
        "exactly the Responses route's WebSocket transport keeps the native adapter (S5.5)"
    );
}

/// Whether only the native adapter can serve this route today: the Responses route's
/// WebSocket transport, which S5.5 owns.
fn is_websocket_route(route: &RouteFile) -> bool {
    matches!(
        route.settings().expect("the shipped settings parse"),
        AdapterSettings::OpenAiResponses(settings)
            if settings.transport == p1_provider_openai::ResponsesTransport::Websocket
    )
}
