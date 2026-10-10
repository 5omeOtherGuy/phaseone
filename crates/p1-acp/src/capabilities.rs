//! Conservative initialization; no p1.dev extensions are implemented yet.

use crate::codec::Codec;
use serde::Deserialize;
use serde_json::{Map, Value};

/// Version-neutral capability snapshot. The codec owns its wire representation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Capabilities {
    pub p1_extensions: bool,
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
    let p1_extensions = match declaration {
        Some(Declaration {
            version: 1,
            capabilities: requested,
        }) => {
            // Parse the complete list even though no capability is enabled.
            let _ = requested;
            true
        }
        _ => false,
    };
    (
        Codec::negotiate(requested_version),
        Capabilities { p1_extensions },
    )
}
