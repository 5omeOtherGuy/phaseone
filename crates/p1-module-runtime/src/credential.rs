//! `WasmCredentialMethod`: the adapter from one loaded `credential-method` component (world
//! `p1:module/credential-method`, ADR-0212) to the five pure functions the host's credential
//! broker asks it: what it provides, what a login file holds, the refresh request, what a
//! refresh response grants and the login file with a refresh written into it.
//!
//! The class imports nothing: no network, no files, no clock, no randomness. Each call runs on
//! a fresh instance of the [`PureComponent`], and its answer is passed through as the component
//! wrote it; reading it is the broker's.

use crate::executor::ExecutionLimits;
use crate::loader::{LoadedModule, ModuleKind};
use crate::manifest::Digest;
use crate::pure::{PureComponent, PureModuleError};

/// The adapter over one loaded credential-method component.
pub struct WasmCredentialMethod(PureComponent);

impl WasmCredentialMethod {
    /// The credential method of `module`, linked with nothing.
    pub fn new(module: &LoadedModule, limits: ExecutionLimits) -> Result<Self, PureModuleError> {
        PureComponent::new(module, ModuleKind::CredentialMethod, limits).map(Self)
    }

    /// The package's manifest name, e.g. `connectors/codex-oauth`.
    pub fn name(&self) -> &str {
        self.0.name()
    }

    /// The digest of the component's verified bytes: its identity.
    pub fn digest(&self) -> Digest {
        self.0.digest()
    }

    /// The https origins the verified manifest lets this method's refresh requests go to.
    pub fn token_origins(&self) -> &[String] {
        self.0.token_origins()
    }

    /// What the module provides, as the JSON text it wrote.
    pub fn describe(&self) -> Result<String, String> {
        self.0
            .call::<(), (String,)>("describe", ())
            .map(|(text,)| text)
    }

    /// The credential `file` holds (JSON), or the module's reason it holds none usable.
    pub fn read_login(&self, file: &[u8], key: Option<&str>) -> Result<String, String> {
        self.0
            .call::<(Vec<u8>, Option<String>), (Result<String, String>,)>(
                "read-login",
                (file.to_vec(), key.map(str::to_owned)),
            )?
            .0
    }

    /// The refresh request (JSON) that spends `refresh_token`, read from `file` when it is a
    /// borrowed login.
    pub fn refresh_request(
        &self,
        refresh_token: &str,
        file: Option<&[u8]>,
    ) -> Result<String, String> {
        self.0
            .call::<(String, Option<Vec<u8>>), (Result<String, String>,)>(
                "refresh-request",
                (refresh_token.to_owned(), file.map(<[u8]>::to_vec)),
            )?
            .0
    }

    /// What the refresh response `body` grants (JSON), its expiry counted from `sent_at_ms`.
    pub fn refresh_response(&self, body: &[u8], sent_at_ms: u64) -> Result<String, String> {
        self.0
            .call::<(Vec<u8>, u64), (String,)>("refresh-response", (body.to_vec(), sent_at_ms))
            .map(|(text,)| text)
    }

    /// The login `file` with the refresh response `body` written into it.
    pub fn merge_login(
        &self,
        file: &[u8],
        body: &[u8],
        sent_at_ms: u64,
        now_ms: u64,
        keep_access: bool,
    ) -> Result<Vec<u8>, String> {
        self.0
            .call::<(Vec<u8>, Vec<u8>, u64, u64, bool), (Result<Vec<u8>, String>,)>(
                "merge-login",
                (
                    file.to_vec(),
                    body.to_vec(),
                    sent_at_ms,
                    now_ms,
                    keep_access,
                ),
            )?
            .0
    }
}
