//! `p1/policy/full-access`: the shipped default authorization policy (ADR-0038). Without
//! `--ask` every call is permitted, headless and interactive, whatever its effect.
//!
//! It implements the `authorization-policy` world of `modules/wit/` and calls no import: a
//! policy component has no filesystem, transport, credential, process or worker capability,
//! and this one does not even read the call.
#![forbid(unsafe_code)]

use p1_bindings_authorization_policy::generated::{
    CallEffect, Guest, ToolCall, ToolIdentity, Verdict,
};

struct FullAccess;

impl Guest for FullAccess {
    fn authorize(_call: ToolCall, _identity: ToolIdentity, _effect: CallEffect) -> Verdict {
        Verdict::Permit
    }
}

p1_bindings_authorization_policy::generated::export!(FullAccess);
