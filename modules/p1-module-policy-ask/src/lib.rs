//! `p1/policy/ask`: the restrictive authorization policy `--ask` selects (ADR-0038). A
//! `read-only` call is permitted; every other effect is `ask`, which the host's native ask
//! bridge resolves through the front end (a headless host denies without waiting for input),
//! so only permit or deny reaches the core (ADR-0024).
//!
//! It implements the `authorization-policy` world of `modules/wit/` and calls no import. It
//! decides from the tool's effect alone: the call's JSON is never parsed, so the guest needs
//! no wire crate.
#![forbid(unsafe_code)]

use p1_bindings_authorization_policy::generated::{
    CallEffect, Guest, ToolCall, ToolIdentity, Verdict,
};

struct Ask;

impl Guest for Ask {
    fn authorize(_call: ToolCall, _identity: ToolIdentity, effect: CallEffect) -> Verdict {
        match effect {
            CallEffect::ReadOnly => Verdict::Permit,
            CallEffect::WritesFiles | CallEffect::Executes | CallEffect::Delegates => Verdict::Ask,
        }
    }
}

p1_bindings_authorization_policy::generated::export!(Ask);
