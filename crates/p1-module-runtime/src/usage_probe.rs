//! `WasmUsageProbe`: the adapter from one loaded `usage-probe` component (world
//! `p1:module/usage-probe`, ADR-0214) to the two pure functions the host's usage ledger asks it:
//! what its endpoint is and what a response says.
//!
//! The class imports nothing and never receives a credential: the host builds the request,
//! attaches the credential natively and hands the component only a 2xx response's body. Each
//! call runs on a fresh instance of the [`PureComponent`], and its answer is passed through as
//! the component wrote it; reading and checking it is the ledger's.

use crate::executor::ExecutionLimits;
use crate::loader::{LoadedModule, ModuleKind};
use crate::manifest::Digest;
use crate::pure::{PureComponent, PureModuleError};

/// The adapter over one loaded usage-probe component.
pub struct WasmUsageProbe(PureComponent);

impl WasmUsageProbe {
    /// The usage probe of `module`, linked with nothing.
    pub fn new(module: &LoadedModule, limits: ExecutionLimits) -> Result<Self, PureModuleError> {
        PureComponent::new(module, ModuleKind::UsageProbe, limits).map(Self)
    }

    /// The package's manifest name, e.g. `connectors/claude`.
    pub fn name(&self) -> &str {
        self.0.name()
    }

    /// The digest of the component's verified bytes: its identity.
    pub fn digest(&self) -> Digest {
        self.0.digest()
    }

    /// The https origins the verified manifest lets this probe's request go to.
    pub fn token_origins(&self) -> &[String] {
        self.0.token_origins()
    }

    /// The endpoint, as the JSON text the module wrote.
    pub fn describe(&self) -> Result<String, String> {
        self.0
            .call::<(), (String,)>("describe", ())
            .map(|(text,)| text)
    }

    /// What the 2xx response `body` says (JSON), or the module's reason it says nothing
    /// readable.
    pub fn decode(&self, body: &[u8]) -> Result<String, String> {
        self.0
            .call::<(Vec<u8>,), (Result<String, String>,)>("decode", (body.to_vec(),))?
            .0
    }
}
