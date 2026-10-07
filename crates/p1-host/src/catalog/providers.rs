//! The provider half of the catalog: one factory per route file, and the composition
//! of a route file with a profile binding into a provider.
//!
//! Since S4.9 a route's `adapter` key names a provider COMPONENT (ADR-0086): the factory
//! activates the component the installed release ships — or the one a user's lock selects for
//! that adapter's module (ANSWERS D083b) — configured from the route's own data
//! and the selected profile's text, and the native transport broker sends every request, over
//! HTTP or, when the component lowers one to WebSocket, over the provider's one route-bound
//! connection (ADR-0078). A host with no release module set installed has no component to
//! activate, so activation REFUSES — naming the route and the module — instead of falling back
//! to a native adapter.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use p1_assembly::{Catalog, ProviderSpec, load_modules_lock};
use p1_contracts::{BoxFuture, Provider, ProviderError, ProviderErrorKind};
use p1_module_runtime::{
    ExecutionLimits, LoadedModule, Loader, ProviderSettings, ReleaseManifest, WasmProvider,
};
use p1_provider_http::{Credential, CredentialSource, Transport, ws::WsConnector};

use crate::HostDeps;
use crate::routes::RouteFile;

/// The compiled whole-provider keys: a catalog key that consumes no profile. A route
/// file may not take one of these ids, and a `route` naming one is the wrong-form
/// error the whole provider itself reports. The list is EMPTY since ADR-0039 step 4b:
/// `anthropic-subscription` and `openai-codex-subscription` are route files now. The
/// MECHANISM stays for test fakes and the next whole provider: a key registered
/// outside the route files still reports its wrong form through [`reject_profile`],
/// which is why that helper is kept.
pub const WHOLE_PROVIDERS: [&str; 0] = [];

pub(super) fn register_providers(
    catalog: &mut Catalog,
    deps: &HostDeps,
    routes: &[RouteFile],
) -> Result<(), String> {
    register_routes(catalog, deps, routes)
}

/// Register one factory per route file, under the route id. Its adapter selects a
/// release component; the route and selected environment supply its configuration.
fn register_routes(
    catalog: &mut Catalog,
    deps: &HostDeps,
    routes: &[RouteFile],
) -> Result<(), String> {
    // The provider components the installed release ships, discovered once per catalog
    // through the one manifest path modules are read from (ADR-0079). A host with no module
    // set installed has none, and every route then REFUSES activation instead of building a
    // native adapter.
    let components = Arc::new(ProviderComponents::installed_with_sources(
        deps.verified_sources.clone(),
        &deps.build_loaders,
        super::modules::release_for_build(deps),
    )?);
    register_routes_with_components(catalog, deps, components, routes)
}

fn register_routes_with_components(
    catalog: &mut Catalog,
    deps: &HostDeps,
    components: Arc<ProviderComponents>,
    routes: &[RouteFile],
) -> Result<(), String> {
    let locations = crate::auth::locations(deps);
    for route in routes {
        if WHOLE_PROVIDERS.contains(&route.id.as_str()) {
            return Err(format!(
                "route `{}` collides with the compiled whole-provider key of the same name; \
                 rename the route file, a route cannot shadow a whole provider",
                route.id
            ));
        }
        let transport = deps.transport.clone();
        // ADR-0047 §1: the host composes the REAL WebSocket connector next to the
        // HTTP transport, once per catalog. Composition opens no socket: only a
        // request on a route that asks for `transport = "websocket"` connects.
        let ws: Arc<dyn p1_provider_http::ws::WsConnector> =
            Arc::new(p1_provider_http::ws::TungsteniteConnector::new());
        let locations = locations.clone();
        let components = components.clone();
        let environment_dirs = deps.environment_dirs.clone();
        let secrets = deps.secrets.clone();
        let route = Arc::new(route.clone());
        let data = route.clone();
        catalog.provider(
            &route.id,
            Box::new(move |spec: &ProviderSpec| {
                let profile = require_profile(spec)?;
                data.binding(&profile.id)?;
                crate::routes::check_shipped_origin(&data, &crate::routes::shipped_origins())?;
                // Origin binding follows lazy credential access: inspection can assemble
                // a custom route without using its credential, and a running provider
                // rechecks approval before every access or refresh (ADR-0110).
                let (store_origin, require_origin) = crate::routes::store_origin_policy(&data);
                let credentials = crate::auth::registering(
                    Arc::new(OriginBoundSource {
                        route: data.clone(),
                        locations: locations.clone(),
                        inner: p1_auth::resolve_with_store_origin(
                            &data.id,
                            &data.credential,
                            transport.clone(),
                            &locations,
                            store_origin.as_deref(),
                            require_origin,
                        ),
                    }),
                    secrets.clone(),
                );
                data.settings()?;
                let source = credentials.clone();
                let provider = components.activate_for_environment(
                    &environment_dirs,
                    spec.profile_text
                        .as_deref()
                        .ok_or_else(|| format!("route `{}` has no parsed profile text", data.id))?,
                    &data,
                    profile,
                    transport.clone(),
                    ws.clone(),
                    credentials,
                )?;
                Ok(crate::secret_mask::masking(
                    provider,
                    secrets.clone(),
                    Some(source),
                ))
            }),
        );
    }
    Ok(())
}

struct OriginBoundSource {
    route: Arc<RouteFile>,
    locations: p1_auth::Locations,
    inner: Arc<dyn CredentialSource>,
}

impl OriginBoundSource {
    fn check(&self) -> Result<(), ProviderError> {
        crate::routes::check_credential_origin(&self.route, &self.locations)
            .map_err(|message| ProviderError::new(ProviderErrorKind::Authentication, message))
    }
}

impl CredentialSource for OriginBoundSource {
    fn access<'a>(&'a self) -> BoxFuture<'a, Result<Credential, ProviderError>> {
        Box::pin(async move {
            self.check()?;
            let credential = self.inner.access().await?;
            self.check()?;
            Ok(credential)
        })
    }

    fn refresh<'a>(
        &'a self,
        rejected: &'a Credential,
    ) -> BoxFuture<'a, Result<Credential, ProviderError>> {
        Box::pin(async move {
            self.check()?;
            let credential = self.inner.refresh(rejected).await?;
            self.check()?;
            Ok(credential)
        })
    }

    fn proxy_injected(&self) -> bool {
        self.inner.proxy_injected()
    }
}

/// The one line `p1 env show` prints for a route's credential (spec §4): WHICH
/// source it would come from, never a value. `None` for an environment that names
/// no route (a whole provider, which has no `[credential]` table).
pub fn credential_line(
    environment: &p1_assembly::EnvironmentFile,
    environment_dirs: &[PathBuf],
    locations: &p1_auth::Locations,
) -> Result<Option<String>, String> {
    if environment.profile.is_none() {
        return Ok(None);
    }
    credential_line_for_route(&environment.provider, environment_dirs, locations).map(Some)
}

/// The same line for a route id alone: `p1 models` prints it for every model, so
/// both commands show one wording from one probe.
pub fn credential_line_for_route(
    route_id: &str,
    environment_dirs: &[PathBuf],
    locations: &p1_auth::Locations,
) -> Result<String, String> {
    let route = crate::routes::load_route_by_id(environment_dirs, route_id)?;
    // Issue #484: the line names paths and variables a user controls; one that carries a
    // credential shape is masked, never printed.
    Ok(crate::routes::credential_description(&route, locations))
}

/// The profile an environment selected, for a key that is a chat route. The old
/// form on such a key is a load error that says which form to write instead: a
/// route's model policy lives in a profile, and there is no fallback.
fn require_profile(spec: &ProviderSpec) -> Result<Arc<p1_model_profile::ModelProfile>, String> {
    spec.profile.clone().ok_or_else(|| {
        format!(
            "`{}` is a chat route and needs a model profile: write `route` and `profile` in the \
             environment file instead of `provider`, `model` and `family`",
            spec.key
        )
    })
}

/// A whole provider that consumes no profile. The new form on such a key is a load
/// error that says which form to write instead; there is no fallback. No shipped key
/// is whole any more (see [`WHOLE_PROVIDERS`]); this stays PUBLIC because it is the
/// half of the whole-provider mechanism a test fake's factory composes with — the
/// next whole provider registers exactly like the fakes do.
pub fn reject_profile(spec: &ProviderSpec) -> Result<(), String> {
    if spec.profile.is_some() {
        return Err(format!(
            "`{}` is a whole provider and takes no profile: write `provider`, `model` and \
             `family` in the environment file instead of `route` and `profile`",
            spec.key
        ));
    }
    Ok(())
}

/// The composition of one route file with one profile binding: the one construction path
/// (spec §2 step 4). The adapter key the file names selects a provider COMPONENT (ADR-0086),
/// which [`ProviderComponents::activate`] configures from the route's own data and the selected
/// profile's text; the native transport broker then sends every request. Activation resolves
/// the same binding from the route and the profile, so `_binding` only names what the caller
/// already resolved.
///
/// `environment_dirs` resolve the effective lock (`modules.lock`). Direct callers of this
/// compatibility entry point resolve profile text from the first matching directory; production
/// assembly passes the exact text parsed by `load_environment`.
/// `ws` is the WebSocket connector, next to the HTTP transport (ADR-0047 §1): the production
/// factory passes the real one and a test or live check injects its own, so a test that
/// composes a shipped WebSocket route never opens a socket. Only a request the component lowers
/// to WebSocket — on a route that asks for `transport = "websocket"` — connects through it.
#[allow(clippy::too_many_arguments)]
pub fn route_provider(
    components: &ProviderComponents,
    environment_dirs: &[PathBuf],
    route: &crate::routes::RouteFile,
    _binding: &crate::routes::ModelBinding,
    profile: Arc<p1_model_profile::ModelProfile>,
    transport: Arc<dyn p1_provider_http::Transport>,
    ws: Arc<dyn WsConnector>,
    credentials: Arc<dyn p1_provider_http::CredentialSource>,
) -> Result<Arc<dyn Provider>, String> {
    // A route whose own `[adapter_settings]` do not parse never activates.
    route.settings()?;
    components.activate(environment_dirs, route, profile, transport, ws, credentials)
}

/// The provider component each adapter key names (ADR-0086). One `match` at the root: a route
/// file's `adapter` is the whole selection of a component, so there is no registry and
/// nothing registers itself. Route files and profiles are unchanged by this — a profile stays
/// data the component parses.
pub fn provider_component(adapter: &str) -> Result<&'static str, String> {
    match adapter {
        "anthropic-messages" => Ok("p1/provider-anthropic"),
        "openai-responses" => Ok("p1/provider-openai"),
        "openai-chat" => Ok("p1/provider-openai-chat"),
        other => Err(format!(
            "adapter \"{other}\" names no provider component; the adapters with one are {}",
            crate::routes::ADAPTER_KEYS.join(", ")
        )),
    }
}

/// The provider components one installed release ships, and the only way a provider component
/// is read: by its manifest name through the runtime loader, never from a path or bytes a
/// route could name (freeze item 6).
pub struct ProviderComponents {
    /// The release's loader, or `None` when the host has no module set installed.
    loader: Option<Arc<Loader>>,
    /// One compiled component per module name, so the routes that name the same adapter
    /// configure the same bytes.
    loaded: Mutex<HashMap<String, Arc<LoadedModule>>>,
    sources: Option<Arc<super::modules::VerifiedSources>>,
}

impl ProviderComponents {
    /// A host with no release module set: no route has a component to activate, so every
    /// activation refuses. Public because [`ProviderComponents::installed`] returns exactly this
    /// in a checkout with no module set, and the refusal is what a test must hold.
    pub fn none() -> Self {
        Self {
            loader: None,
            loaded: Mutex::new(HashMap::new()),
            sources: None,
        }
    }

    /// The components of the release whose manifest is `path`.
    pub fn read(path: &Path) -> Result<Self, String> {
        let name = |message: String| format!("{}: {message}", path.display());
        let manifest = ReleaseManifest::read(path).map_err(|error| name(error.to_string()))?;
        manifest
            .check_unique_digests()
            .map_err(|error| name(error.to_string()))?;
        let root = path.parent().unwrap_or(Path::new("."));
        let loader = Loader::new(manifest, root)
            .map_err(|error| format!("cannot start the module runtime: {error}"))?;
        Ok(Self {
            loader: Some(Arc::new(loader)),
            loaded: Mutex::new(HashMap::new()),
            sources: None,
        })
    }

    fn installed_with_sources(
        sources: Arc<super::modules::VerifiedSources>,
        loaders: &super::modules::BuildLoaders,
        release: Option<PathBuf>,
    ) -> Result<Self, String> {
        let Some(path) = release else {
            return Ok(Self::none());
        };
        if !path.is_file() {
            return Ok(Self::none());
        }
        let manifest = loaders
            .manifest_for(&path)
            .map_err(|error| error.to_string())?;
        manifest
            .check_unique_digests()
            .map_err(|error| error.to_string())?;
        Ok(Self {
            loader: Some(
                loaders
                    .for_release(&path, manifest)
                    .map_err(|error| error.to_string())?,
            ),
            loaded: Mutex::new(HashMap::new()),
            sources: Some(sources),
        })
    }

    /// The components of the installed release module set: the one discovery path (S1's
    /// `official_release_manifest`, ADR-0079). A build that has no module set — a development
    /// checkout, where S3.8.0's debug discovery is what finds the built components — has
    /// none, and every activation then refuses instead of building a native adapter.
    pub fn installed() -> Result<Self, String> {
        let Some(path) = super::modules::official_release_manifest() else {
            return Ok(Self::none());
        };
        if !path.is_file() {
            return Ok(Self::none());
        }
        Self::read(&path)
    }

    /// The compiled component `name`.
    fn module(&self, name: &str) -> Result<Arc<LoadedModule>, String> {
        let Some(loader) = self.loader.as_ref() else {
            return Err(missing_module(name));
        };
        let mut loaded = self.loaded.lock().expect("the component cache");
        if let Some(module) = loaded.get(name) {
            return Ok(module.clone());
        }
        let module = Arc::new(loader.load(name).map_err(|error| error.to_string())?);
        if let Some(sources) = &self.sources {
            sources.record(name, &module);
        }
        loaded.insert(name.to_owned(), module.clone());
        Ok(module)
    }

    /// The provider one environment's route file and profile activate, before the first turn:
    /// the route's `adapter` selects the component, the route's own data and the profile's
    /// text configure it, and the broker sends with the route's credential and endpoint, which
    /// no component can name (ADR-0086). A refusal names the route and the module and is
    /// reported like a route-file load error; nothing is sent.
    ///
    /// There is NO native fallback: a host with no release module set installed, or a release
    /// that does not ship the component, refuses here. `ws` is the connector of the provider's
    /// one WebSocket connection, which only a request the component lowers to WebSocket opens.
    ///
    /// A USER's lock may select another package for the route's adapter module; the module the
    /// lock names is then what activation uses ([`locked_package`], ANSWERS D083b).
    pub fn activate(
        &self,
        environment_dirs: &[PathBuf],
        route: &RouteFile,
        profile: Arc<p1_model_profile::ModelProfile>,
        transport: Arc<dyn Transport>,
        ws: Arc<dyn WsConnector>,
        credentials: Arc<dyn CredentialSource>,
    ) -> Result<Arc<dyn Provider>, String> {
        self.activate_with_profile_dirs(
            environment_dirs,
            environment_dirs,
            None,
            route,
            profile,
            transport,
            ws,
            credentials,
        )
    }

    /// Production assembly passes the exact profile text it parsed, while lock resolution
    /// still uses the complete configuration search path.
    #[allow(clippy::too_many_arguments)]
    fn activate_for_environment(
        &self,
        environment_dirs: &[PathBuf],
        parsed_profile_text: &str,
        route: &RouteFile,
        profile: Arc<p1_model_profile::ModelProfile>,
        transport: Arc<dyn Transport>,
        ws: Arc<dyn WsConnector>,
        credentials: Arc<dyn CredentialSource>,
    ) -> Result<Arc<dyn Provider>, String> {
        self.activate_with_profile_dirs(
            environment_dirs,
            environment_dirs,
            Some(parsed_profile_text),
            route,
            profile,
            transport,
            ws,
            credentials,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn activate_with_profile_dirs(
        &self,
        environment_dirs: &[PathBuf],
        profile_dirs: &[PathBuf],
        parsed_profile_text: Option<&str>,
        route: &RouteFile,
        profile: Arc<p1_model_profile::ModelProfile>,
        transport: Arc<dyn Transport>,
        ws: Arc<dyn WsConnector>,
        credentials: Arc<dyn CredentialSource>,
    ) -> Result<Arc<dyn Provider>, String> {
        let binding = route.binding(&profile.id)?;
        let name = provider_component(&route.adapter)
            .map_err(|reason| selection_refusal(route, reason))?;
        let package = locked_package(environment_dirs, name)
            .map_err(|reason| activation_refusal(route, name, reason))?;
        let module = self
            .module(&package)
            .map_err(|reason| activation_refusal(route, name, reason))?;
        let settings = ProviderSettings {
            origin_route: route.origin_route.clone(),
            endpoint: route.endpoint.clone(),
            model: profile.id.clone(),
            wire_model: binding.wire_model.clone(),
            adapter_settings: route.component_adapter_settings(
                binding,
                &profile.id,
                &component_profile_text(profile_dirs, &profile.id, parsed_profile_text)
                    .map_err(|reason| activation_refusal(route, name, reason))?,
            ),
        };
        WasmProvider::new(
            &module,
            settings,
            credentials,
            transport,
            ExecutionLimits::default(),
        )
        .map(|provider| {
            if let Some(sources) = &self.sources {
                sources.record(&route.id, &module);
            }
            Arc::new(provider.with_websocket(ws, Arc::new(Instant::now))) as Arc<dyn Provider>
        })
        .map_err(|error| activation_refusal(route, module.name(), error.to_string()))
    }
}

/// Why a route cannot activate its provider component because no module set is installed: the
/// sentence names the module the route asked for, so an operator sees which one is missing.
fn missing_module(name: &str) -> String {
    format!(
        "provider module {name} is not installed: this host has no release module set \
         (no manifest.json beside the executable)"
    )
}

/// The refusal of a route that cannot select a provider at all: no component serves its
/// adapter key, or its own settings do not parse. Both are the route file's fault, reported
/// like a route-file load error and before anything is sent.
fn selection_refusal(route: &RouteFile, reason: impl std::fmt::Display) -> String {
    format!(
        "route \"{}\" cannot activate its provider: {reason}",
        route.id
    )
}

/// The package activation loads for the provider module `name`: the one the effective
/// `modules.lock` selects for that module, or `name` itself — the release's own host entry of
/// the adapter — when no lock selects one.
///
/// A lock key is a manifest name without the reserved `p1/` namespace
/// (`docs/design/modules/package.md`), so `p1/provider-anthropic` is selected under
/// `provider-anthropic`. The entry's own digest, world and protocol were checked against the
/// release when the catalog loaded the lock (`modules::load_locked_modules`), and the module
/// itself is verified and compiled by this component set's loader, so a user can select among
/// the release's packages and cannot name bytes (freeze item 6).
fn locked_package(environment_dirs: &[PathBuf], name: &str) -> Result<String, String> {
    let lock = load_modules_lock(environment_dirs).map_err(|error| error.to_string())?;
    Ok(lock
        .resolve(name.strip_prefix("p1/").unwrap_or(name))
        .map(|locked| locked.package.clone())
        .unwrap_or_else(|| name.to_owned()))
}

/// The one wording of an activation refusal: the route that cannot become a provider, the
/// component it names, and why. Nothing has been sent when it is printed.
fn activation_refusal(
    route: &RouteFile,
    component: &str,
    reason: impl std::fmt::Display,
) -> String {
    format!(
        "route \"{}\" cannot activate provider component {component}: {reason}",
        route.id
    )
}

/// Production uses the bytes assembly parsed; direct callers preserve file lookup.
fn component_profile_text(
    dirs: &[PathBuf],
    id: &str,
    parsed_profile_text: Option<&str>,
) -> Result<String, String> {
    match parsed_profile_text {
        Some(text) => Ok(text.to_owned()),
        None => profile_text(dirs, id),
    }
}

/// Direct-call compatibility lookup from the supplied directories; production uses the
/// exact text from the environment instead. A component reads no file (ADR-0086).
fn profile_text(environment_dirs: &[PathBuf], id: &str) -> Result<String, String> {
    let mut searched: Vec<PathBuf> = Vec::new();
    for dir in environment_dirs {
        let path = dir.join("../profiles").join(format!("{id}.toml"));
        match std::fs::read_to_string(&path) {
            Ok(text) => return Ok(text),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => searched.push(path),
            Err(error) => return Err(format!("{}: {error}", path.display())),
        }
    }
    let searched: Vec<String> = searched
        .iter()
        .map(|path| path.display().to_string())
        .collect();
    Err(format!(
        "profile `{id}` was not found in {}",
        searched.join(", ")
    ))
}

#[cfg(test)]
mod regression_tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct CountingCredentials(AtomicUsize);

    impl CredentialSource for CountingCredentials {
        fn access<'a>(&'a self) -> BoxFuture<'a, Result<Credential, ProviderError>> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Box::pin(async {
                Ok(Credential {
                    bearer: "FAKE-TEST".into(),
                    account_id: None,
                })
            })
        }

        fn refresh<'a>(
            &'a self,
            _: &'a Credential,
        ) -> BoxFuture<'a, Result<Credential, ProviderError>> {
            self.access()
        }
    }

    struct ReplacingCredentials {
        locations: p1_auth::Locations,
    }

    impl CredentialSource for ReplacingCredentials {
        fn access<'a>(&'a self) -> BoxFuture<'a, Result<Credential, ProviderError>> {
            Box::pin(async move {
                // Deterministic seam: login replaces approval between outer check
                // and return from credential acquisition, without a timing assertion.
                p1_auth::store::put_api_key_at_origin(
                    "race-route",
                    "FAKE-REPLACEMENT",
                    Some("https://origin-b.example"),
                    &self.locations,
                )
                .await
                .unwrap();
                p1_auth::resolve(
                    "race-route",
                    &p1_auth::CredentialSpec {
                        kind: p1_auth::CredentialKind::ApiKey,
                        env: None,
                        borrow: vec![],
                        store_only: true,
                        login_dir: None,
                    },
                    Arc::new(p1_provider_http::testing::ScriptedTransport::new(vec![])),
                    &self.locations,
                )
                .access()
                .await
            })
        }

        fn refresh<'a>(
            &'a self,
            _: &'a Credential,
        ) -> BoxFuture<'a, Result<Credential, ProviderError>> {
            self.access()
        }
    }

    #[tokio::test]
    async fn returned_credential_is_refused_when_login_changes_approval() {
        let home = tempfile::tempdir().unwrap();
        let locations = p1_auth::Locations::none().with_home(Some(home.path().to_owned()));
        let mut route =
            crate::routes::load_routes(&Path::new(env!("CARGO_MANIFEST_DIR")).join("../../routes"))
                .unwrap()
                .into_iter()
                .find(|route| route.credential.kind == p1_auth::CredentialKind::ApiKey)
                .unwrap();
        route.id = "race-route".into();
        route.endpoint = "https://origin-a.example/v1".into();
        let source = OriginBoundSource {
            route: Arc::new(route),
            locations: locations.clone(),
            inner: Arc::new(ReplacingCredentials {
                locations: locations.clone(),
            }),
        };
        for refresh in [false, true] {
            p1_auth::store::trust_endpoint("race-route", "https://origin-a.example", &locations)
                .await
                .unwrap();
            let rejected = Credential {
                bearer: "FAKE-REJECTED".into(),
                account_id: None,
            };
            let result = if refresh {
                source.refresh(&rejected).await
            } else {
                source.access().await
            };
            assert!(
                result.is_err(),
                "replacement credential escaped its origin approval"
            );
        }
    }

    #[tokio::test]
    async fn origin_binding_precedes_every_credential_read_and_refresh() {
        let home = tempfile::tempdir().unwrap();
        let locations = p1_auth::Locations::none().with_home(Some(home.path().to_owned()));
        let source_tree = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../routes");
        let mut route = crate::routes::load_routes(&source_tree)
            .unwrap()
            .into_iter()
            .find(|route| route.credential.kind == p1_auth::CredentialKind::ApiKey)
            .unwrap();
        route.id = "new-origin-test".into();
        route.endpoint = "https://custom.example/v1".into();
        let credentials = Arc::new(CountingCredentials(AtomicUsize::new(0)));
        let source = OriginBoundSource {
            route: Arc::new(route),
            locations: locations.clone(),
            inner: credentials.clone(),
        };
        assert!(source.access().await.is_err());
        let rejected = Credential {
            bearer: "FAKE-REJECTED".into(),
            account_id: None,
        };
        assert!(source.refresh(&rejected).await.is_err());
        assert_eq!(credentials.0.load(Ordering::SeqCst), 0);
        p1_auth::store::trust_endpoint("new-origin-test", "https://custom.example", &locations)
            .await
            .unwrap();
        source.access().await.unwrap();
        assert_eq!(credentials.0.load(Ordering::SeqCst), 1);
        p1_auth::store::remove("new-origin-test", &locations)
            .await
            .unwrap();
        assert!(source.access().await.is_err());
        assert!(source.refresh(&rejected).await.is_err());
        assert_eq!(credentials.0.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn provider_uses_exact_parsed_profile_text_even_after_file_changes() {
        let dir = tempfile::tempdir().unwrap();
        let first = dir.path().join("first/environments");
        let second = dir.path().join("second/environments");
        for base in [&first, &second] {
            std::fs::create_dir_all(base.join("../profiles")).unwrap();
        }
        std::fs::create_dir_all(second.join("chosen")).unwrap();
        std::fs::write(
            second.join("chosen/environment.toml"),
            "route = \"fake\"\nprofile = \"model\"\n",
        )
        .unwrap();
        std::fs::write(second.join("chosen/prompt.md"), "test").unwrap();
        let original = "id = \"model\"\nrevision = 1\nmodel_id = \"model\"\nfamily = \"test\"\nthinking = \"enabled\"\nefforts = [\"high\"]\n";
        std::fs::write(first.join("../profiles/model.toml"), "wrong").unwrap();
        let path = second.join("../profiles/model.toml");
        std::fs::write(&path, original).unwrap();
        let dirs = [first, second];
        let environment = p1_assembly::load_environment("chosen", &dirs).unwrap();
        std::fs::write(&path, "changed").unwrap();
        assert_eq!(
            component_profile_text(&dirs, "model", environment.profile_text.as_deref()).unwrap(),
            original
        );
    }

    /// The shipped route whose adapter has the provider component this case builds into a
    /// release, and a profile it binds that the checkout ships. The case needs those ids
    /// without compiling them: the frozen `the_two_shipped_routes_have_no_compiled_literals`
    /// case scans this file for exactly those route and model literals, so they are read from
    /// the shipped route files instead.
    fn shipped_route_binding(source: &Path) -> Option<(String, String)> {
        let mut files: Vec<PathBuf> = std::fs::read_dir(source.join("routes"))
            .expect("the checkout ships route files")
            .map(|entry| entry.expect("a route directory entry").path())
            .filter(|path| {
                path.extension()
                    .is_some_and(|ext| ext.to_str() == Some("toml"))
            })
            .collect();
        files.sort();
        for path in files {
            let text = std::fs::read_to_string(&path).expect("a route file is readable");
            let Ok(route) = toml::from_str::<RouteFile>(&text) else {
                continue;
            };
            if route.adapter != "openai-chat" {
                continue;
            }
            if let Some(profile) = route.models.keys().find(|candidate| {
                source
                    .join("profiles")
                    .join(format!("{candidate}.toml"))
                    .is_file()
            }) {
                return Some((route.id.clone(), profile.clone()));
            }
        }
        None
    }

    #[test]
    fn registered_provider_activates_with_selected_profile_not_conflicting_higher_priority_file() {
        use p1_contracts::serde_json::{self, json};
        let root = tempfile::tempdir().unwrap();
        let first = root.path().join("first/environments");
        let second = root.path().join("second/environments");
        std::fs::create_dir_all(first.join("../profiles")).unwrap();
        std::fs::create_dir_all(second.join("../profiles")).unwrap();
        std::fs::create_dir_all(second.join("../routes")).unwrap();
        std::fs::create_dir_all(second.join("chosen")).unwrap();
        let source = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let (route, profile) =
            shipped_route_binding(&source).expect("a shipped route binds a shipped profile");
        std::fs::copy(
            source.join(format!("profiles/{profile}.toml")),
            second.join(format!("../profiles/{profile}.toml")),
        )
        .unwrap();
        std::fs::copy(
            source.join(format!("routes/{route}.toml")),
            second.join(format!("../routes/{route}.toml")),
        )
        .unwrap();
        std::fs::write(
            first.join(format!("../profiles/{profile}.toml")),
            "not a profile",
        )
        .unwrap();
        std::fs::write(
            second.join("chosen/environment.toml"),
            format!("route = \"{route}\"\nprofile = \"{profile}\"\n"),
        )
        .unwrap();
        std::fs::write(second.join("chosen/prompt.md"), "hello").unwrap();
        let dirs = vec![first, second];
        let environment = p1_assembly::load_environment("chosen", &dirs).unwrap();
        std::fs::write(
            dirs[1].join(format!("../profiles/{profile}.toml")),
            "changed after load",
        )
        .unwrap();

        let package = "p1-module-provider-openai-chat";
        let built = source.join(format!("modules/target/p1-modules/{package}"));
        let manifest: serde_json::Value = serde_json::from_slice(
            &std::fs::read(built.join(format!("{package}.manifest.json"))).unwrap(),
        )
        .unwrap();
        let bytes = std::fs::read(built.join(format!("{package}.wasm"))).unwrap();
        let mut release = p1_module_tests::Release::empty();
        release.add(
            json!({
                "name": "p1/provider-openai-chat",
                "digest": manifest["digest"], "path": "packages/provider.wasm",
                "kind": manifest["kind"], "world": manifest["world"],
                "protocol": manifest["protocol"], "capabilities": manifest["capabilities"],
                "variant": manifest["variant"],
            }),
            &bytes,
        );
        let components = Arc::new(ProviderComponents::read(&release.manifest_file()).unwrap());
        let output = || -> crate::SharedWriter { Arc::new(Mutex::new(Box::new(std::io::sink()))) };
        let deps = HostDeps::new(
            output(),
            output(),
            Arc::new(crate::StdinLines::new()),
            Arc::new(p1_provider_http::testing::ScriptedTransport::new(Vec::new())),
            "2026-01-01".into(),
            Arc::new(crate::SignalInterrupt),
            dirs,
            false,
        );
        let mut catalog = Catalog::new();
        let routes = crate::routes::load_all_routes(&deps.environment_dirs).unwrap();
        register_routes_with_components(&mut catalog, &deps, components, &routes).unwrap();
        let substitutions = p1_assembly::Substitutions {
            workspace: root.path().display().to_string(),
            date: "2026-01-01".into(),
            os: "test".into(),
            scratch: String::new(),
        };
        p1_assembly::assemble(&catalog, &environment, root.path(), &substitutions)
            .expect("production route factory must use the selected parsed profile text");
    }
}

#[cfg(test)]
mod verified_identity_tests {
    use super::*;

    #[test]
    fn provider_load_records_verified_digest_not_a_native_identity() {
        let release = super::super::modules::official_release_manifest().expect("release path");
        let sources = Arc::new(super::super::modules::VerifiedSources::default());
        let loaders = super::super::modules::BuildLoaders::default();
        let components = ProviderComponents::installed_with_sources(
            sources.clone(),
            &loaders,
            Some(release.clone()),
        )
        .expect("release provider components");
        let name = provider_component("anthropic-messages").expect("adapter");
        let loaded = components.module(name).expect("verified provider module");
        let recorded = sources.resolve(name).expect("recorded load");
        assert_eq!(recorded.digest, loaded.digest().to_string());
        assert_eq!(recorded.abi, loaded.abi());
        assert!(release.is_file());
    }
}
