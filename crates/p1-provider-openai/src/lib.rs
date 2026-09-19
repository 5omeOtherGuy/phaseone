//! The ChatGPT/Codex subscription route of the OpenAI Responses API.
//!
//! This adapter translates the p1 provider contract into the HTTPS + SSE wire
//! shape documented in `docs/design/routes.md` §B. The donor's WebSocket
//! transport, continuation, native compaction, structured summaries and login
//! flow are deliberately not taken; credentials are read from the Codex CLI's
//! own auth file, never written by a login flow.
//!
//! Layout:
//! - [`request`]: the pure request builder, header builder and base-URL resolver.
//! - `parser`: the pure SSE state machine, surfaced through [`p1_provider_http::drive`].
//! - [`provider`]: the [`p1_contracts::Provider`] implementation.
//! - [`credentials`]: a file-based [`p1_provider_http::CredentialSource`] over the
//!   Codex CLI auth file, with rotating-refresh write-back.

mod credentials;
mod parser;
mod provider;
mod request;

pub use credentials::{Clock, CodexCliCredentials};
pub use provider::OpenAiCodexProvider;
pub use request::{ROUTE, build_headers, build_request, resolve_base_url};
