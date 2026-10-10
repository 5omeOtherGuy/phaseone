//! Conservative initialization and explicit per-extension negotiation.

use crate::codec::Codec;
use serde::Deserialize;
use serde_json::{Map, Value};

/// Version-neutral capability snapshot. The codec owns its wire representation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Capabilities {
    pub p1_extensions: bool,
    pub workflow_update: bool,
    /// `session/close` is served: the router of several sessions serves it, a single
    /// session process does not.
    pub close_sessions: bool,
}

#[derive(Deserialize)]
struct Declaration {
    version: u64,
    capabilities: Vec<String>,
}

/// Negotiate a supported protocol version and p1.dev independently. Unsupported
/// protocol versions receive our latest supported version, per ACP initialization.
/// Malformed or unsupported p1.dev declarations enable nothing and get no key.
pub fn initialize(
    requested_version: u16,
    client_meta: Option<&Map<String, Value>>,
) -> (Codec, Capabilities) {
    let declaration = client_meta
        .and_then(|meta| meta.get("p1.dev"))
        .and_then(|value| serde_json::from_value::<Declaration>(value.clone()).ok());
    let declaration = declaration.filter(|declaration| declaration.version == 1);
    let p1_extensions = declaration.is_some();
    let workflow_update = declaration.is_some_and(|declaration| {
        declaration
            .capabilities
            .iter()
            .any(|name| name == crate::extensions::workflow::CAPABILITY)
    });
    (
        Codec::negotiate(requested_version),
        Capabilities {
            p1_extensions,
            workflow_update,
            close_sessions: false,
        },
    )
}
