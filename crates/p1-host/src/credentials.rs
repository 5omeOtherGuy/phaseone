//! The installed credential modules (ADR-0212): the composition `p1-auth` resolves a route's
//! credential method through ([`p1_auth::CredentialModules`]).
//!
//! A credential method beyond the core's two is a component of the `credential-method` class.
//! One is installed under a name in two ways, the first that has it winning:
//! - a lock entry (`modules.lock` beside an environments directory, which `p1 modules install`
//!   writes) whose world is `credential-method`: loaded from the installed set that holds it at
//!   the pinned digest, exactly as a lock-selected tool is, with the token origins the owner
//!   confirmed at install pinned beside the digest ([`crate::catalog::modules`]);
//! - p1's release: the release set's `p1/<name>` package of that class.
//!
//! Listing reads manifests and locks only; a module is verified, compiled and instantiated the
//! first time a route names it. Nothing registers itself.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use p1_auth::{CredentialModule, CredentialModules};
use p1_module_runtime::loader::OFFICIAL_NAMESPACE;
use p1_module_runtime::{
    ExecutionLimits, LoadedModule, ModuleKind, ReleaseManifest, WasmCredentialMethod,
};

/// The credential modules of one host: its environment directories' locks and sets, and its
/// release.
pub struct InstalledCredentials {
    environment_dirs: Vec<PathBuf>,
    release: Option<PathBuf>,
}

impl InstalledCredentials {
    /// The modules installed beside `environment_dirs` and in the release whose manifest is
    /// `release`.
    pub fn new(environment_dirs: Vec<PathBuf>, release: Option<PathBuf>) -> Self {
        Self {
            environment_dirs,
            release,
        }
    }

    fn lock(&self) -> Result<p1_assembly::ModulesLock, String> {
        p1_assembly::load_modules_lock(&self.environment_dirs).map_err(|error| error.to_string())
    }

    /// The release's credential packages, by the name routes give them.
    fn released(&self) -> Vec<String> {
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
            .filter(|entry| entry.kind == ModuleKind::CredentialMethod.name())
            .filter_map(|entry| {
                entry
                    .name
                    .strip_prefix(OFFICIAL_NAMESPACE)
                    .and_then(|name| name.strip_prefix('/'))
                    .map(str::to_owned)
            })
            .collect()
    }

    fn load(&self, name: &str) -> Result<Option<LoadedModule>, String> {
        let lock = self.lock()?;
        if let Some(locked) = lock
            .resolve(name)
            .filter(|locked| is_credential_world(&locked.world))
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
        let Some(release) = self.release.as_deref() else {
            return Ok(None);
        };
        let manifest = ReleaseManifest::read(release).map_err(|error| error.to_string())?;
        let package = format!("{OFFICIAL_NAMESPACE}/{name}");
        if !manifest
            .entry(&package)
            .is_some_and(|entry| entry.kind == ModuleKind::CredentialMethod.name())
        {
            return Ok(None);
        }
        let root = release.parent().unwrap_or(Path::new("."));
        crate::catalog::modules::release_loader(manifest, root)
            .and_then(|loader| loader.load(&package))
            .map(Some)
            .map_err(|error| error.to_string())
    }
}

fn is_credential_world(world: &str) -> bool {
    world.starts_with(&format!(
        "p1:module/{}@",
        ModuleKind::CredentialMethod.name()
    ))
}

impl CredentialModules for InstalledCredentials {
    fn names(&self) -> Vec<String> {
        let mut names: Vec<String> = self
            .lock()
            .map(|lock| {
                lock.iter()
                    .filter(|(_, locked)| is_credential_world(&locked.world))
                    .map(|(name, _)| name.to_owned())
                    .collect()
            })
            .unwrap_or_default();
        names.extend(self.released());
        names.sort();
        names.dedup();
        names
    }

    fn module(&self, name: &str) -> Result<Option<Arc<dyn CredentialModule>>, String> {
        let Some(loaded) = self.load(name)? else {
            return Ok(None);
        };
        let method = WasmCredentialMethod::new(&loaded, ExecutionLimits::default())
            .map_err(|error| error.to_string())?;
        Ok(Some(Arc::new(Component(method))))
    }
}

/// One loaded credential component as `p1-auth` calls it.
struct Component(WasmCredentialMethod);

impl CredentialModule for Component {
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
