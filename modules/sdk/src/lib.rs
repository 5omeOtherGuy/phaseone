//! The guest SDK for p1 modules (ADR-0210): what a module built outside this repository
//! depends on, as one crate.
//!
//! Each module class has a feature that brings in its world's generated bindings, re-exported
//! under the class's name: `tool`, `provider`, `context_policy`, `authorization_policy`,
//! `workflow_decision` and `credential_method`. A module implements the world's `Guest` trait
//! and exports it with the world's macro, naming this crate's path to the bindings:
//!
//! ```ignore
//! struct Echo;
//! impl p1_module_sdk::tool::Guest for Echo { /* … */ }
//! p1_module_sdk::tool::export!(Echo with_types_in p1_module_sdk::tool);
//! ```
//!
//! The bindings are generated in their own crates (`modules/p1-bindings-*`), so the `export!`
//! macro, whose expansion needs `unsafe`, is defined in one crate and expanded in another: a
//! module keeps `#![forbid(unsafe_code)]` (ADR-0113, `docs/design/modules/toolchain.md`).
//!
//! The `protocol` feature adds the value protocol (`p1-module-protocol`): the `Wire*` types every
//! JSON value across the boundary has, and the `serde_json` they are read and written with.
//!
//! Versions (ADR-0210): this crate's version is the WIT package's, `p1:module@1.1.0`, and its
//! protocol is `p1_module_sdk::protocol::PROTOCOL_VERSION`. A module's manifest names the world
//! and protocol it was built with; a host serves the same major at any minor up to its own.
#![forbid(unsafe_code)]

#[cfg(feature = "authorization-policy")]
pub use p1_bindings_authorization_policy::generated as authorization_policy;
#[cfg(feature = "context-policy")]
pub use p1_bindings_context_policy::generated as context_policy;
#[cfg(feature = "credential-method")]
pub use p1_bindings_credential_method::generated as credential_method;
#[cfg(feature = "provider")]
pub use p1_bindings_provider::generated as provider;
#[cfg(feature = "tool")]
pub use p1_bindings_tool::generated as tool;
#[cfg(feature = "workflow-decision")]
pub use p1_bindings_workflow_decision::generated as workflow_decision;
#[cfg(feature = "protocol")]
pub use p1_module_protocol as protocol;
#[cfg(feature = "protocol")]
pub use serde_json;
