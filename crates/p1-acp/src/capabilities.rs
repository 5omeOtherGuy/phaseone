//! Conservative ACP v1 initialization; no p1.dev extensions are implemented yet.

use agent_client_protocol_schema::{
    ProtocolVersion,
    v1::{AgentCapabilities, ClientCapabilities, InitializeResponse},
};
use serde::Deserialize;
use serde_json::json;

#[derive(Deserialize)]
struct Declaration {
    version: u64,
    capabilities: Vec<String>,
}

/// Only a well-formed version 1 declaration negotiates p1.dev. Unsupported
/// versions and malformed declarations behave exactly like an absent key.
/// A valid capability list is parsed but enables nothing in this first slice.
pub fn initialize(client: &ClientCapabilities) -> InitializeResponse {
    let declaration = client
        .meta
        .as_ref()
        .and_then(|meta| meta.get("p1.dev"))
        .and_then(|value| serde_json::from_value::<Declaration>(value.clone()).ok());
    let mut capabilities = AgentCapabilities::default();
    if let Some(Declaration {
        version: 1,
        capabilities: requested,
    }) = declaration
    {
        let _ = requested;
        capabilities.meta = Some(serde_json::Map::from_iter([(
            "p1.dev".into(),
            json!({"version": 1, "extensions": []}),
        )]));
    }
    InitializeResponse::new(ProtocolVersion::V1).agent_capabilities(capabilities)
}
