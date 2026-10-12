//! The installed modules of the two classes that serve a credential (ADR-0212, ADR-0214): the
//! credential methods `p1-auth` resolves a route's method through
//! ([`p1_auth::CredentialModules`]) and the usage probes the ledger reaches an account's usage
//! endpoint through ([`p1_usage::ProbeModules`]).
//!
//! A credential method beyond the core's two, and every usage probe, is a component of that
//! class installed under a name in two ways, the first that has it winning:
//! - a lock entry (`modules.lock` beside an environments directory, which `p1 modules install`
//!   writes) whose world is the class: loaded from the installed set that holds it at the pinned
//!   digest, exactly as a lock-selected tool is, with the token origins the owner confirmed
//!   pinned beside the digest ([`crate::catalog::modules`]);
//! - the release: the release set's package of the class named `<namespace>/<name>`, which the
//!   distribution put there with the parts it composes (the subscription connectors'
//!   `connectors/` packages, ADR-0215), verified by the release manifest's digest. Two release
//!   packages of the class that claim one name are refused.
//!
//! Listing reads manifests and locks only; a module is verified, compiled and instantiated the
//! first time a route names it. Nothing registers itself.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use p1_auth::{CredentialModule, CredentialModules};
use p1_module_runtime::{
    ExecutionLimits, LoadedModule, ModuleKind, ReleaseManifest, WasmCredentialMethod,
    WasmUsageProbe,
};
use p1_usage::{ProbeModule, ProbeModules};

/// The modules of one class a host has installed: its environment directories' locks and sets,
/// and its release set.
struct Installed {
    kind: ModuleKind,
    environment_dirs: Vec<PathBuf>,
    release: Option<PathBuf>,
}

impl Installed {
    fn lock(&self) -> Result<p1_assembly::ModulesLock, String> {
        p1_assembly::load_modules_lock(&self.environment_dirs).map_err(|error| error.to_string())
    }

    fn of_class(&self, world: &str) -> bool {
        world.starts_with(&format!("p1:module/{}@", self.kind.name()))
    }

    /// The release set's packages of the class, as `(name, package)`.
    fn released(&self) -> Vec<(String, String)> {
        let Some(manifest) = self
            .release
            .as_deref()
            .and_then(|path| ReleaseManifest::read(path).ok())
        else {
            return Vec::new();
        };
        manifest
            .components()
            .iter()
            .filter(|entry| entry.kind == self.kind.name())
            .filter_map(|entry| {
                let (_, name) = entry.name.split_once('/')?;
                Some((name.to_owned(), entry.name.clone()))
            })
            .collect()
    }

    /// The names a lock or the release installs a module of the class under, sorted.
    fn names(&self) -> Vec<String> {
        let mut names: Vec<String> = self
            .lock()
            .map(|lock| {
                lock.iter()
                    .filter(|(_, locked)| self.of_class(&locked.world))
                    .map(|(name, _)| name.to_owned())
                    .collect()
            })
            .unwrap_or_default();
        names.extend(self.released().into_iter().map(|(name, _)| name));
        names.sort();
        names.dedup();
        names
    }

    /// The module of the class installed under `name`: its lock entry's package, verified
    /// against the pin, else the release set's.
    fn load(&self, name: &str) -> Result<Option<LoadedModule>, String> {
        let lock = self.lock()?;
        if let Some(locked) = lock
            .resolve(name)
            .filter(|locked| self.of_class(&locked.world))
        {
            let release = self.release.clone().unwrap_or_default();
            let installed =
                crate::catalog::modules::installed_sets(&self.environment_dirs, Some(&release));
            return crate::catalog::modules::load_locked_package(
                &installed, &release, name, locked,
            )
            .map(Some)
            .map_err(|error| error.to_string());
        }
        let packages: Vec<String> = self
            .released()
            .into_iter()
            .filter(|(released, _)| released == name)
            .map(|(_, package)| package)
            .collect();
        let package = match packages.as_slice() {
            [] => return Ok(None),
            [package] => package,
            many => {
                return Err(format!(
                    "the release ships {} {} modules named `{name}` ({}); a lock entry selects one",
                    many.len(),
                    self.kind.name(),
                    many.join(", ")
                ));
            }
        };
        let Some(release) = self.release.as_deref() else {
            return Ok(None);
        };
        let manifest = ReleaseManifest::read(release).map_err(|error| error.to_string())?;
        let root = release.parent().unwrap_or(Path::new("."));
        crate::catalog::modules::release_loader(manifest, root)
            .and_then(|loader| loader.load(package))
            .map(Some)
            .map_err(|error| error.to_string())
    }
}

/// The credential modules of one host.
pub struct InstalledCredentials(Installed);

impl InstalledCredentials {
    /// The credential methods installed beside `environment_dirs` and found in their sets or
    /// in the release set whose manifest is `release`.
    pub fn new(environment_dirs: Vec<PathBuf>, release: Option<PathBuf>) -> Self {
        Self(Installed {
            kind: ModuleKind::CredentialMethod,
            environment_dirs,
            release,
        })
    }
}

impl CredentialModules for InstalledCredentials {
    fn names(&self) -> Vec<String> {
        self.0.names()
    }

    fn module(&self, name: &str) -> Result<Option<Arc<dyn CredentialModule>>, String> {
        let Some(loaded) = self.0.load(name)? else {
            return Ok(None);
        };
        let method = WasmCredentialMethod::new(&loaded, ExecutionLimits::default())
            .map_err(|error| error.to_string())?;
        Ok(Some(Arc::new(Method(method))))
    }
}

/// One loaded credential component as `p1-auth` calls it.
struct Method(WasmCredentialMethod);

impl CredentialModule for Method {
    fn describe(&self) -> Result<String, String> {
        self.0.describe()
    }

    fn read_login(&self, file: &[u8], key: Option<&str>) -> Result<String, String> {
        self.0.read_login(file, key)
    }

    fn refresh_request(&self, refresh_token: &str, file: Option<&[u8]>) -> Result<String, String> {
        self.0.refresh_request(refresh_token, file)
    }

    fn refresh_response(&self, body: &[u8], sent_at_ms: u64) -> Result<String, String> {
        self.0.refresh_response(body, sent_at_ms)
    }

    fn merge_login(
        &self,
        file: &[u8],
        body: &[u8],
        sent_at_ms: u64,
        now_ms: u64,
        keep_access: bool,
    ) -> Result<Vec<u8>, String> {
        self.0
            .merge_login(file, body, sent_at_ms, now_ms, keep_access)
    }

    fn token_origins(&self) -> &[String] {
        self.0.token_origins()
    }
}

/// The usage probes of one host.
pub struct InstalledProbes(Installed);

impl InstalledProbes {
    /// The usage probes installed beside `environment_dirs` and found in their sets or in the
    /// release set whose manifest is `release`.
    pub fn new(environment_dirs: Vec<PathBuf>, release: Option<PathBuf>) -> Self {
        Self(Installed {
            kind: ModuleKind::UsageProbe,
            environment_dirs,
            release,
        })
    }
}

impl ProbeModules for InstalledProbes {
    fn names(&self) -> Vec<String> {
        self.0.names()
    }

    fn module(&self, name: &str) -> Result<Option<Arc<dyn ProbeModule>>, String> {
        let Some(loaded) = self.0.load(name)? else {
            return Ok(None);
        };
        let probe = WasmUsageProbe::new(&loaded, ExecutionLimits::default())
            .map_err(|error| error.to_string())?;
        Ok(Some(Arc::new(Probe(probe))))
    }
}

/// One loaded usage-probe component as `p1-usage` calls it.
struct Probe(WasmUsageProbe);

impl ProbeModule for Probe {
    fn describe(&self) -> Result<String, String> {
        self.0.describe()
    }

    fn decode(&self, body: &[u8]) -> Result<String, String> {
        self.0.decode(body)
    }

    fn token_origins(&self) -> &[String] {
        self.0.token_origins()
    }
}
