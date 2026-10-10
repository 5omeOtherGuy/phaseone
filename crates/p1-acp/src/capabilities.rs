//! Conservative initialization and explicit per-extension negotiation.

use crate::codec::Codec;
use serde::Deserialize;
use serde_json::{Map, Value};

/// Version-neutral capability snapshot. The codec owns its wire representation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Capabilities {
    pub p1_extensions: bool,
    pub workflow_update: bool,
    pub run_cancel: bool,
    pub worker_stop: bool,
    pub worker_transcripts: bool,
    /// `session/close` is served: the router of several sessions serves it, a single
    /// session process does not.
    pub close_sessions: bool,
    pub stored_sessions: bool,
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
    let enabled = |name: &str| {
        declaration.as_ref().is_some_and(|declaration| {
            declaration
                .capabilities
                .iter()
                .any(|requested| requested == name)
        })
    };
    (
        Codec::negotiate(requested_version),
        Capabilities {
            p1_extensions,
            workflow_update: enabled(crate::extensions::workflow::CAPABILITY),
            run_cancel: enabled(crate::extensions::cancel::RUN_CAPABILITY),
            worker_stop: enabled(crate::extensions::cancel::WORKER_CAPABILITY),
            worker_transcripts: enabled(crate::extensions::workers::CAPABILITY),
            close_sessions: false,
            stored_sessions: false,
        },
    )
}
