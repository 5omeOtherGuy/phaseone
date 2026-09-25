//! The closed call verb vocabulary (freeze item 8).
//!
//! `CallDescription.verb` is a `&'static str` in `p1-contracts`, and the UI keys its
//! vocabulary on it. A module's verb arrives as a runtime string; rather than change the
//! public contract type to own a `String` (which would need an ADR), the boundary maps the
//! string onto a fixed set of statics. That also keeps UI vocabulary in the host's hands:
//! a module cannot mint a word the UI shows.

/// Every verb a module may use: exactly the verbs p1's native tools used when the boundary
/// froze. Adding one is a protocol minor-version change, because an older host would
/// show it as `call`.
pub const CALL_VERBS: [&str; 8] = [
    "read", "edit", "run", "search", "finish", "worker", "workflow", "call",
];

/// Maps a module's verb onto the vocabulary. An unknown verb becomes the neutral `call`
/// and is not kept anywhere: echoing it would let a module put arbitrary words in the UI.
pub fn call_verb(verb: &str) -> &'static str {
    CALL_VERBS
        .iter()
        .copied()
        .find(|known| *known == verb)
        .unwrap_or("call")
}
