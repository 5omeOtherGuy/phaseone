//! The shared provider HTTP seam.
//!
//! [`Transport`] and [`ws::WsConnector`] are the only network-shaped types a
//! provider adapter sees; the SSE decoder, status classification and the retrying
//! [`drive`] loop are shared by every adapter. The crate owns no route knowledge:
//! an adapter supplies a request builder and a [`ResponseParser`], and this crate
//! owns the policy that is identical across routes — one credential refresh per
//! request, one shared transient-retry budget, bounded backoff that races
//! cancellation, and the rule that a stream is never retried after any output has
//! been forwarded.
//!
//! The authoritative spec is `docs/design/providers.md`. The five stream rules
//! the returned stream obeys are at the top of `p1-contracts/src/provider.rs`.

mod credential;
mod drive;
mod error_code;
mod file_lock;
mod http;
mod retry;
mod sse;
pub mod ws;

pub use credential::{Credential, CredentialSource};
pub use drive::{DriveRequest, ResponseParser, drive};
pub use error_code::{http_error_code, kind_for_status, safe_code};
pub use file_lock::{LOCK_PATIENCE, lock_exclusive};
pub use http::{
    ByteStream, HttpRequest, HttpResponse, ReqwestTransport, Transport, TransportError,
};
pub use retry::{HttpClass, RetryPolicy, classify_status, reset_after, retry_after};
pub use sse::{SseDecoder, SseEvent};

/// Test doubles for the transport seam. Enabled by the `testing` feature, and
/// always available to this crate's own tests.
#[cfg(any(test, feature = "testing"))]
pub mod testing;
