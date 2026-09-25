//! The one bounded exchange every OAuth token refresh goes through (issue #164).
//!
//! The transport only bounds the CONNECT; a token endpoint that accepts the
//! connection and never answers would otherwise hold the refresh — and the refresh
//! lock — forever. The refresh gets the same bounds ADR-0069 gave provider reads:
//! [`FIRST_BYTE_TIMEOUT`] (120 s) for the response headers, and
//! [`STREAM_IDLE_TIMEOUT`] (300 s) between reads of the small JSON body. Expiry is
//! an [`ProviderErrorKind::Authentication`] error that names the bound and nothing
//! else — never a body, token or header. The caller returns it before any write, so
//! the credential file stays untouched and the lock drops with the caller's frame.

use std::time::Duration;

use futures_util::StreamExt;
use p1_contracts::{ProviderError, ProviderErrorKind};
use p1_provider_http::{
    ByteStream, FIRST_BYTE_TIMEOUT, HttpRequest, HttpResponse, STREAM_IDLE_TIMEOUT, Transport,
    TransportError,
};

/// Why a bounded refresh exchange failed.
pub(crate) enum RefreshIoError {
    /// No answer within the bound: already the final, value-free error.
    TimedOut(ProviderError),
    /// The transport or the body read failed; the caller words the error.
    Transport(TransportError),
}

/// `transport.post`, bounded by [`FIRST_BYTE_TIMEOUT`] for the response headers.
pub(crate) async fn post(
    transport: &dyn Transport,
    request: HttpRequest,
) -> Result<HttpResponse, RefreshIoError> {
    match tokio::time::timeout(FIRST_BYTE_TIMEOUT, transport.post(request)).await {
        Ok(result) => result.map_err(RefreshIoError::Transport),
        Err(_) => Err(timed_out(FIRST_BYTE_TIMEOUT)),
    }
}

/// Read the whole body; every read waits at most [`STREAM_IDLE_TIMEOUT`].
pub(crate) async fn drain(mut body: ByteStream) -> Result<Vec<u8>, RefreshIoError> {
    let mut bytes = Vec::new();
    loop {
        match tokio::time::timeout(STREAM_IDLE_TIMEOUT, body.next()).await {
            Ok(Some(Ok(chunk))) => bytes.extend_from_slice(&chunk),
            Ok(Some(Err(error))) => return Err(RefreshIoError::Transport(error)),
            Ok(None) => return Ok(bytes),
            Err(_) => return Err(timed_out(STREAM_IDLE_TIMEOUT)),
        }
    }
}

fn timed_out(bound: Duration) -> RefreshIoError {
    RefreshIoError::TimedOut(ProviderError::new(
        ProviderErrorKind::Authentication,
        format!("token refresh got no response within {} s", bound.as_secs()),
    ))
}
