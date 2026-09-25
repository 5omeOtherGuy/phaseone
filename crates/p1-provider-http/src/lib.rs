//! The shared provider HTTP seam, split into portable protocol helpers and a
//! native transport (ADR-0071).
//!
//! PORTABLE, always compiled: the SSE decoder ([`SseDecoder`]), the
//! [`ResponseParser`] seam, status classification and server retry hints
//! ([`classify_status`], [`retry_after`], [`reset_after`]) and error-code
//! extraction ([`http_error_code`], [`kind_for_status`], [`safe_code`]). These are
//! what a provider WebAssembly component needs to lower requests, parse streams
//! and classify errors; they pull in no async runtime, no socket and no
//! credential code, so a guest build cannot reach any of those.
//!
//! NATIVE, behind the default `native` feature: [`Transport`] and [`ws::WsConnector`],
//! the only network-shaped types a native provider adapter sees, and the retrying
//! [`drive`] loop that holds the whole drive policy identical across routes — one
//! credential refresh per request, one shared transient-retry budget
//! ([`RetryPolicy`]), bounded backoff that races cancellation, the first-byte and
//! stream-idle read bounds ([`FIRST_BYTE_TIMEOUT`] 120 s, [`STREAM_IDLE_TIMEOUT`]
//! 300 s) that end a provider which never answers, and the rule that a stream is
//! never retried after any output has been forwarded. The crate owns no route
//! knowledge: an adapter supplies a request builder and a [`ResponseParser`].
//!
//! The authoritative spec is `docs/design/providers.md`. The five stream rules
//! the returned stream obeys are at the top of `p1-contracts/src/provider.rs`.

#[cfg(feature = "native")]
mod credential;
#[cfg(feature = "native")]
mod drive;
mod error_code;
#[cfg(feature = "native")]
mod file_lock;
#[cfg(feature = "native")]
mod http;
mod parser;
#[cfg(feature = "native")]
mod retry;
mod sse;
mod status;
#[cfg(feature = "native")]
pub mod ws;

#[cfg(feature = "native")]
pub use credential::{Credential, CredentialSource};
#[cfg(feature = "native")]
pub use drive::{DriveRequest, drive, proxy_refusal_message};
pub use error_code::{http_error_code, kind_for_status, safe_code};
#[cfg(feature = "native")]
pub use file_lock::{LOCK_PATIENCE, lock_exclusive};
#[cfg(feature = "native")]
pub use http::{
    ByteStream, FIRST_BYTE_TIMEOUT, HttpRequest, HttpResponse, ReqwestTransport,
    STREAM_IDLE_TIMEOUT, Transport, TransportError,
};
pub use parser::ResponseParser;
#[cfg(feature = "native")]
pub use retry::RetryPolicy;
pub use sse::{SseDecoder, SseEvent};
pub use status::{HttpClass, classify_status, reset_after, retry_after};

/// Test doubles for the transport seam. Enabled by the `testing` feature, and
/// always available to this crate's own tests.
#[cfg(all(feature = "native", any(test, feature = "testing")))]
pub mod testing;

#[cfg(test)]
mod tests {
    /// The manifest is the split's guard: a native-only dependency that stops
    /// being optional, or a `p1-auth` dependency, would let a guest build reach
    /// sockets, a runtime or credentials without any compile error here.
    const MANIFEST: &str = include_str!("../Cargo.toml");

    fn dependency_line(name: &str) -> &'static str {
        let prefix = format!("{name} = ");
        MANIFEST
            .lines()
            .find(|line| line.starts_with(&prefix))
            .unwrap_or_else(|| panic!("{name} is not a dependency"))
    }

    #[test]
    fn native_only_dependencies_are_optional_and_enabled_by_the_default_native_feature() {
        assert!(MANIFEST.contains("default = [\"native\"]"));
        assert!(MANIFEST.contains("testing = [\"native\"]"));
        let native = MANIFEST
            .split("native = [")
            .nth(1)
            .and_then(|rest| rest.split(']').next())
            .expect("a native feature");
        for name in ["reqwest", "tokio", "tokio-tungstenite"] {
            assert!(
                dependency_line(name).contains("optional = true"),
                "{name} must be optional"
            );
            assert!(
                native.contains(&format!("\"dep:{name}\"")),
                "native must enable {name}"
            );
        }
    }

    #[test]
    fn no_dependency_on_p1_auth() {
        assert!(!MANIFEST.contains("p1-auth"));
    }
}
