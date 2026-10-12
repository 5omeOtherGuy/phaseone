//! The subscription connectors' chat client identity (ADR-0200, ADR-0211): the OpenCode
//! CLI's identity at the OpenCode Zen gateway, as an implementation of the client-identity
//! port. Only a build with the `subscriptions` feature has it.

use crate::ClientIdentity;
use crate::identity::ChatClientIdentity;

/// Present the OpenCode CLI's identity to the OpenCode Zen gateway: its `user-agent` and its
/// `x-opencode-*` headers with a generated `ses_`/`msg_` id. The system prompt and the declared
/// tools are NOT changed; the `zen`/`zen2`/`zen3` environments grant the `bash`/`read` names
/// the gate wants.
pub(crate) struct Opencode;

/// The `opencode` client identity.
pub const OPENCODE: ClientIdentity = ClientIdentity::of(&Opencode);

impl ChatClientIdentity for Opencode {
    fn name(&self) -> &'static str {
        "opencode"
    }

    fn owned_headers(&self) -> &'static [&'static str] {
        &OPENCODE_IDENTITY_OWNED_HEADERS
    }

    fn headers(&self, cache_key: Option<&str>) -> Vec<(String, String)> {
        let (session, request) = opencode_ids(cache_key);
        vec![
            ("user-agent".into(), OPENCODE_USER_AGENT.into()),
            ("x-opencode-client".into(), "cli".into()),
            ("x-opencode-project".into(), "global".into()),
            ("x-opencode-session".into(), session),
            ("x-opencode-request".into(), request),
        ]
    }
}

/// The `user-agent` OpenCode's CLI sends to the Zen gateway (captured 2026-09-24,
/// opencode 1.18.31; see `docs/design/zen-client-identity-evidence.md`).
const OPENCODE_USER_AGENT: &str =
    "opencode/1.18.31 ai-sdk/provider-utils/4.0.23 runtime/bun/1.3.14";

/// The static header names owned by the OpenCode identity. These must be reserved
/// so a route cannot send a second value for a header the identity also sets.
const OPENCODE_IDENTITY_OWNED_HEADERS: [&str; 4] = [
    "x-opencode-client",
    "x-opencode-project",
    "x-opencode-session",
    "x-opencode-request",
];

/// The `ses_`/`msg_` ids OpenCode shapes as `<prefix>_<12 lowercase hex><14 alnum>`
/// (its `Identifier.ascending`): the gateway's free-tier gate accepts exactly that
/// shape and rejects a short or non-hex-prefixed id with 403. p1 derives both ids
/// from the route's cache key — a pure function, so a resumed session keeps them.
/// Without a key, a per-request nonce does the same job.
fn opencode_ids(cache_key: Option<&str>) -> (String, String) {
    use std::hash::{Hash, Hasher};
    use std::sync::atomic::{AtomicU64, Ordering};
    static NONCE: AtomicU64 = AtomicU64::new(0);
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    match cache_key {
        Some(key) => key.hash(&mut hasher),
        None => {
            // A provider component has no clock import (its capability allocation), and
            // reading one traps, so the component's ids rest on the instance's counter
            // alone; its instance lives for the provider, so the ids stay fresh per request.
            #[cfg(not(target_family = "wasm"))]
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|elapsed| elapsed.as_nanos() as u64)
                .unwrap_or(0)
                .hash(&mut hasher);
            NONCE.fetch_add(1, Ordering::Relaxed).hash(&mut hasher);
        }
    }
    let first = hasher.finish();
    let mut second = std::collections::hash_map::DefaultHasher::new();
    first.hash(&mut second);
    let hex = format!("{first:016x}{:016x}", second.finish());
    (
        format!("ses_{}", &hex[..26]),
        format!("msg_{}", &hex[6..32]),
    )
}
