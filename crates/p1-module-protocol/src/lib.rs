//! The value protocol between the p1 host and its WebAssembly modules (ADR-0071).
//!
//! The rich contract values that cross the module boundary travel as JSON. This crate
//! holds their only serialized form: the `Wire*` types here, described by the JSON schema
//! bundle under `schema/`, converted to and from the `p1-contracts` types the native core
//! works with. Keeping the wire form apart from `p1-contracts` lets the boundary be frozen
//! and versioned without a public type change in the contracts, and lets several contract
//! types that have no serde form at all (`StreamEvent`, `Outcome`, `ToolOutcome`) cross it.
//!
//! Closed values reject unknown fields: a module that sends a field the host does not know
//! is speaking another protocol version, and silently dropping the field would hide that.
//! Optional values are omitted when absent, so unknown usage stays absent and never zero.
//!
//! Published as `docs/design/modules/protocol.md`.

#![warn(missing_docs)]

mod failure;
mod history;
mod provider;
mod tool;
mod vocabulary;

pub use failure::ModuleFailure;
pub use history::{
    WireAssistantBlock, WireAssistantItem, WireInboxKind, WireItem, WireOrigin, WireReplayData,
    WireToolCall, WireToolInput, WireToolStatus,
};
pub use provider::{
    WireCacheKeySupport, WireEffort, WireModelOptions, WireOutcome, WireProviderError,
    WireProviderErrorKind, WireRouteDescription, WireStopReason, WireStreamEvent, WireUsage,
};
pub use tool::{
    WireCallDescription, WireEditPreview, WireResultDescription, WireResultDetail, WireToolOutcome,
};
pub use vocabulary::{CALL_VERBS, call_verb};

/// A protocol version: the major changes when a value an old peer accepted changes meaning
/// or shape, the minor when something is added that an old host can refuse cleanly (such as
/// a new call verb).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ProtocolVersion {
    /// Incompatible revisions; a host refuses a module built for another major.
    pub major: u32,
    /// Compatible additions within one major.
    pub minor: u32,
}

/// The version every boundary value in this crate and in `schema/` is interpreted under.
/// The schema `$id`s carry the major (`p1:protocol/<name>/<major>`), so a schema can never
/// be mistaken for one of another major.
pub const PROTOCOL_VERSION: ProtocolVersion = ProtocolVersion { major: 1, minor: 0 };

/// A wire value that parsed but cannot be held by the native contract type.
///
/// Wire indices and counts are `u64` so the protocol does not depend on either peer's
/// pointer width, while the contracts use `usize`; on a narrower host a value can overflow.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ConversionError {
    /// An index or count exceeds the host's `usize`.
    #[error("{field} {value} does not fit this host's index size")]
    OutOfRange {
        /// The wire field that overflowed.
        field: &'static str,
        /// The value as received.
        value: u64,
    },
}

pub(crate) fn to_usize(field: &'static str, value: u64) -> Result<usize, ConversionError> {
    usize::try_from(value).map_err(|_| ConversionError::OutOfRange { field, value })
}

pub(crate) fn to_u64(value: usize) -> u64 {
    // usize is at most 64 bits on every target Rust supports, so this cannot fail; a loud
    // panic names the broken invariant where a saturation would silently lose the value
    // the module sent.
    u64::try_from(value).expect("usize is at most 64 bits on every target Rust supports")
}

/// Deserialize an optional wire field that is absent or present, but never `null`.
///
/// A bare `Option<T>` reads an explicit `null` as absent, while every schema types these
/// fields as a value and none admits `null`; the two peers must agree on the same shape, so
/// a module that sends `"usage": null` is refused here exactly as the schema refuses it.
pub(crate) fn refuse_null<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: serde::Deserialize<'de>,
{
    <T as serde::Deserialize<'de>>::deserialize(deserializer).map(Some)
}
