//! The one bounded exchange every OAuth token refresh goes through (issue #164).
//!
//! The transport only bounds the CONNECT; a token endpoint that accepts the
//! connection and never answers would otherwise hold the refresh — and the refresh
//! lock — forever. The refresh gets the same bounds ADR-0069 gave provider reads:
//! [`FIRST_BYTE_TIMEOUT`] (120 s) for the response headers, and
//! [`STREAM_IDLE_TIMEOUT`] (300 s) between reads of the small JSON body, and the
//! whole exchange ends by [`REFRESH_DEADLINE`] (300 s) however the body trickles, so
//! a peer waiting [`LOCK_PATIENCE`] (360 s) always outlasts a holder (issue #484).
//! The body is capped at [`MAX_REFRESH_BODY`] bytes, and a non-success status is
//! reported without reading its body at all.
//!
//! Expiry is an [`ProviderErrorKind::Authentication`] error that names the bound and
//! nothing else — never a body, token or header. The caller returns it before any
//! write, so the credential file stays untouched and the lock drops with the caller's
//! frame.
//!
//! A refresh that has SENT its request runs to its end in its own task ([`detached`]):
//! the server may rotate the refresh token the moment it answers, so a caller that is
//! cancelled mid-response must not take the write-back down with it.

use std::future::Future;
use std::time::Duration;

use futures_util::StreamExt;
use p1_contracts::{ProviderError, ProviderErrorKind};
use p1_provider_http::{
    FIRST_BYTE_TIMEOUT, HttpRequest, LOCK_PATIENCE, STREAM_IDLE_TIMEOUT, Transport, TransportError,
};

/// The whole refresh exchange, headers and body, ends by this bound.
pub(crate) const REFRESH_DEADLINE: Duration = Duration::from_secs(300);

/// A token response is a few hundred bytes; anything past this is not one.
pub(crate) const MAX_REFRESH_BODY: usize = 64 * 1024;

// A peer queued on the lock must outlast the holder's whole bounded refresh.
const _: () = assert!(LOCK_PATIENCE.as_secs() > REFRESH_DEADLINE.as_secs());

/// Why a bounded refresh exchange failed.
pub(crate) enum RefreshIoError {
    /// No answer within a bound: already the final, value-free error.
    TimedOut(ProviderError),
    /// The transport or the body read failed; the caller words the error.
    Transport(TransportError),
    /// The endpoint answered with a non-success status; its body was not read.
    Status(u16),
    /// The body is larger than [`MAX_REFRESH_BODY`].
    TooLarge,
}

/// POST `request` and read the whole success body within the bounds above, counted
/// from `start`: when the refresh began, before it was handed to [`detached`] (a
/// spawned task may first run later).
pub(crate) async fn exchange(
    transport: &dyn Transport,
    request: HttpRequest,
    start: tokio::time::Instant,
) -> Result<Vec<u8>, RefreshIoError> {
    let deadline = start + REFRESH_DEADLINE;
    let headers_by = deadline.min(start + FIRST_BYTE_TIMEOUT);
    let response = match tokio::time::timeout_at(headers_by, transport.post(request)).await {
        Ok(result) => result.map_err(RefreshIoError::Transport)?,
        Err(_) => return Err(timed_out(FIRST_BYTE_TIMEOUT.min(REFRESH_DEADLINE))),
    };
    if !(200..300).contains(&response.status) {
        return Err(RefreshIoError::Status(response.status));
    }
    let mut body = response.body;
    let mut bytes = Vec::new();
    loop {
        let idle_by = deadline.min(tokio::time::Instant::now() + STREAM_IDLE_TIMEOUT);
        match tokio::time::timeout_at(idle_by, body.next()).await {
            Ok(Some(Ok(chunk))) => {
                if bytes.len().saturating_add(chunk.len()) > MAX_REFRESH_BODY {
                    return Err(RefreshIoError::TooLarge);
                }
                bytes.extend_from_slice(&chunk);
            }
            // The server accepted the request: it may already have rotated the login.
            Ok(Some(Err(_))) => {
                return Err(RefreshIoError::TimedOut(lost(
                    "token refresh response broke off".to_string(),
                )));
            }
            Ok(None) => return Ok(bytes),
            Err(_) => {
                let bound = if idle_by >= deadline {
                    REFRESH_DEADLINE
                } else {
                    STREAM_IDLE_TIMEOUT
                };
                return Err(RefreshIoError::TimedOut(lost(format!(
                    "token refresh got no response within {} s",
                    bound.as_secs()
                ))));
            }
        }
    }
}

fn timed_out(bound: Duration) -> RefreshIoError {
    RefreshIoError::TimedOut(ProviderError::new(
        ProviderErrorKind::Authentication,
        format!("token refresh got no response within {} s", bound.as_secs()),
    ))
}

/// A response lost after the server accepted the refresh: the server may have rotated
/// the refresh token already, and only a new login can replace it then.
fn lost(what: String) -> ProviderError {
    ProviderError::new(
        ProviderErrorKind::Authentication,
        format!(
            "{what}; the server may already have rotated the login, so if the next refresh \
             is refused, log in again"
        ),
    )
}

/// Run a started token rotation to its end in its own task, so dropping the caller
/// (a cancelled turn, a timeout above it) never loses a response the server already
/// rotated for. Without a runtime to spawn on it simply runs inline.
pub(crate) async fn detached<T: Send + 'static>(
    rotation: impl Future<Output = Result<T, ProviderError>> + Send + 'static,
) -> Result<T, ProviderError> {
    match tokio::runtime::Handle::try_current() {
        Ok(runtime) => runtime.spawn(rotation).await.unwrap_or_else(|_| {
            Err(ProviderError::new(
                ProviderErrorKind::Authentication,
                "the token refresh stopped before it finished; if this login stops \
                 working, log in again",
            ))
        }),
        Err(_) => rotation.await,
    }
}

/// The lifetime a refresh response grants, as milliseconds to add to the time the
/// request was SENT: zero is refused, an implausibly long one is capped.
pub(crate) fn lifetime_ms(expires_in_secs: u64) -> Option<u64> {
    /// A year: no OAuth access token p1 refreshes lives longer.
    const MAX_LIFETIME_SECS: u64 = 366 * 24 * 3600;
    // A token that would already count as due for refresh is no rotation: every later
    // access would rotate it again.
    (expires_in_secs.saturating_mul(1000) > REFRESH_MARGIN_MS)
        .then(|| expires_in_secs.min(MAX_LIFETIME_SECS) * 1000)
}

/// How long before its recorded expiry an OAuth access token counts as due for refresh.
pub(crate) const REFRESH_MARGIN_MS: u64 = 300_000;

#[cfg(test)]
mod tests {
    use super::*;
    use p1_provider_http::testing::{BodyEnd, ScriptedResponse, ScriptedTransport};

    fn request() -> HttpRequest {
        HttpRequest {
            url: "https://token.invalid/".to_string(),
            headers: Vec::new(),
            body: Vec::new(),
        }
    }

    fn response(status: u16, chunks: Vec<Vec<u8>>, end: BodyEnd) -> ScriptedTransport {
        ScriptedTransport::new(vec![ScriptedResponse {
            status,
            headers: Vec::new(),
            chunks,
            end,
        }])
    }

    #[tokio::test(start_paused = true)]
    async fn a_failed_status_is_reported_without_reading_a_hanging_body() {
        let transport = response(401, vec![b"partial".to_vec()], BodyEnd::Hang);
        let result = tokio::time::timeout(
            Duration::from_secs(1),
            exchange(&transport, request(), tokio::time::Instant::now()),
        )
        .await
        .expect("the status is reported at once");
        assert!(matches!(result, Err(RefreshIoError::Status(401))));
    }

    #[tokio::test(start_paused = true)]
    async fn an_oversized_body_is_refused() {
        let transport = response(
            200,
            vec![vec![b'x'; MAX_REFRESH_BODY], vec![b'x']],
            BodyEnd::Eof,
        );
        assert!(matches!(
            exchange(&transport, request(), tokio::time::Instant::now()).await,
            Err(RefreshIoError::TooLarge)
        ));
    }

    #[tokio::test(start_paused = true)]
    async fn a_trickling_body_ends_at_the_overall_deadline() {
        // Headers at once, then one byte every 299 s: never idle long enough for the
        // idle bound, so only the overall deadline ends it.
        let transport = Trickle;
        let start = tokio::time::Instant::now();
        let result = exchange(&transport, request(), start).await;
        assert!(matches!(result, Err(RefreshIoError::TimedOut(_))));
        assert_eq!(start.elapsed(), REFRESH_DEADLINE);
    }

    /// Headers at once; then one byte every 299 s, forever.
    struct Trickle;

    impl Transport for Trickle {
        fn post<'a>(
            &'a self,
            _request: HttpRequest,
        ) -> p1_contracts::BoxFuture<'a, Result<p1_provider_http::HttpResponse, TransportError>>
        {
            let body = futures_util::stream::unfold((), |()| async {
                tokio::time::sleep(Duration::from_secs(299)).await;
                Some((Ok(b"{".to_vec()), ()))
            });
            Box::pin(async move {
                Ok(p1_provider_http::HttpResponse {
                    status: 200,
                    headers: Vec::new(),
                    body: Box::pin(body),
                })
            })
        }
    }

    #[test]
    fn a_zero_lifetime_is_refused_and_a_huge_one_capped() {
        assert_eq!(lifetime_ms(0), None);
        assert_eq!(lifetime_ms(300), None);
        assert_eq!(lifetime_ms(301), Some(301_000));
        assert_eq!(lifetime_ms(3600), Some(3_600_000));
        assert_eq!(lifetime_ms(u64::MAX), Some(366 * 24 * 3600 * 1000));
    }
}
