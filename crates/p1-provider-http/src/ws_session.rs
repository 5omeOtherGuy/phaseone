//! The host's WebSocket session: the native half of the route-bound WebSocket
//! (ADR-0078 §1, `docs/design/modules/wit.md` "WebSocket: who decides what").
//!
//! One [`WsSession`] per provider instance owns that instance's ONE connection behind
//! an async mutex (`docs/design/websocket.md` §4). A request leases it without
//! waiting ([`WsSession::try_lease`]; a busy session means the request uses HTTP,
//! never a second socket and never a wait) and, through the [`WsLease`]:
//!
//! - reads the facts of WIT `websocket.connection-state` ([`WsLease::state`]): whether
//!   a connection is open, the response id of the last response it completed
//!   cleanly, and whether this request's previous attempt failed before any output;
//! - executes a `websocket-send` ([`WsLease::send`]): with a head, it opens a NEW
//!   connection — TLS and the handshake, the credential attached here and nowhere
//!   else — under the write bound; then it writes the one text frame under the write
//!   bound;
//! - reads the response's frames ([`WsLease::next`]) under the bounds the connection
//!   measures on every raw frame, control frames included (ADR-0069,
//!   [`crate::ws::WsConnection::next_bounded`]); a ping is answered with a pong
//!   under the write bound inside that read;
//! - records a clean completion ([`WsLease::completed`]), which is the only way the
//!   connection goes back to the session.
//!
//! Every other ending drops the connection: a cancelled wait, a failed write, a
//! close, a bound expiry, a read error, or a lease dropped with its request. The
//! session decides nothing about content — which frame to send, whether a response
//! continues another, whether to fall back to HTTP; the provider's portable
//! decision function chooses from the facts reported here. Retry, backoff and the
//! one credential refresh stay with the native caller (freeze item 9).
//!
//! Reuse follows §4: a connection is reported open only while it is younger than
//! [`MAX_AGE`] and was last used less than [`MAX_IDLE`] ago, on an injected
//! [`Clock`]; an older one is dropped when the state is read.

use std::future::Future;
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures_util::future::{Either, select};
use p1_contracts::{CancellationToken, ProviderError, ProviderErrorKind};
use tokio::sync::{Mutex, OwnedMutexGuard};

use crate::broker::{CredentialScheme, CredentialUse, check_lowered_headers};
use crate::credential::Credential;
use crate::ws::{
    WRITE_TIMEOUT, WsConnectError, WsConnection, WsConnector, WsError, WsHandshake, WsNext,
};

/// The clock the reuse policy reads (§4). Injected, so a test advances time instead
/// of sleeping.
pub type Clock = Arc<dyn Fn() -> Instant + Send + Sync>;

/// §4: a connection is reused while it is younger than this …
pub const MAX_AGE: Duration = Duration::from_secs(55 * 60);
/// … and was last used less than this ago.
pub const MAX_IDLE: Duration = Duration::from_secs(5 * 60);

// Constant refusal messages: a refused path or header may carry the very secret or
// prompt text the refusal exists to keep out of logs.
const PATH_REFUSED: &str =
    "the WebSocket head's path is not a path under the route's endpoint; nothing was sent";
const ACCOUNT_ID_MISSING: &str = "the route's credential has no account id, which the \
     WebSocket head's credential use needs; nothing was sent";

/// WIT `websocket.connection-state`: facts the session reports, and nothing it
/// decided.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ConnectionState {
    /// A connection of this provider instance is open, so a send without a head goes
    /// out on it.
    pub open: bool,
    /// The response id of the last response that open connection completed cleanly;
    /// `None` when it completed none or no connection is open.
    pub last_clean_response: Option<String>,
    /// This request's previous attempt failed over WebSocket before any output, and
    /// the native caller allows it no further WebSocket attempt, so the retry may
    /// fall back to HTTP.
    pub failed_before_output: bool,
}

/// WIT `websocket.websocket-request-head`: what opens a connection.
#[derive(Clone, PartialEq, Eq)]
pub struct WsHead {
    /// Relative to the route's endpoint: empty for the endpoint itself, otherwise a
    /// path under it. The session swaps the scheme to `wss` (`ws`).
    pub path: String,
    /// Handshake headers without any credential, under the same rule as a lowered
    /// HTTP request's ([`check_lowered_headers`]).
    pub headers: Vec<(String, String)>,
    /// Where the session attaches the route's credential, ahead of `headers`.
    pub credential: CredentialUse,
}

impl std::fmt::Debug for WsHead {
    /// Header names only; the path carries no query by construction of a valid
    /// head, but it is never needed for a trace either.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let names: Vec<&str> = self.headers.iter().map(|(name, _)| name.as_str()).collect();
        f.debug_struct("WsHead")
            .field("path_len", &self.path.len())
            .field("header_names", &names)
            .field("credential", &self.credential)
            .finish()
    }
}

/// WIT `websocket.websocket-send`: an optional head, then one text frame.
#[derive(Clone, PartialEq, Eq)]
pub struct WsSend {
    /// Present exactly when [`ConnectionState::open`] was false.
    pub handshake: Option<WsHead>,
    /// The one text frame, chosen by the provider.
    pub frame: String,
}

impl std::fmt::Debug for WsSend {
    /// The frame holds the conversation: only its length is shown.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WsSend")
            .field("handshake", &self.handshake)
            .field("frame_len", &self.frame.len())
            .finish()
    }
}

/// The route binding a send runs under: the endpoint the route configures and the
/// credential to attach. `credential` is `None` on a route whose credential an
/// egress proxy injects (issue #134): nothing is attached then.
#[derive(Clone, Copy)]
pub struct WsAuthority<'a> {
    /// The route's resolved `http(s)` endpoint.
    pub endpoint: &'a str,
    pub credential: Option<&'a Credential>,
}

/// Why a send did not put its frame on a connection. Every variant is a failure
/// before any output of the response; none carries a header value or a frame.
#[derive(Clone, PartialEq, Eq)]
pub enum WsSendError {
    /// The request's cancellation fired first.
    Cancelled,
    /// The upgrade was answered with this status and body.
    Refused { status: u16, body: Vec<u8> },
    /// No connection: a connect error, or no answer within the write bound.
    ConnectFailed,
    /// The frame did not go out: a write error, no completion within the write
    /// bound, or the open connection a send without a head relied on is gone.
    WriteFailed,
    /// The head broke the credential or route rule; nothing was sent.
    Invalid(ProviderError),
}

impl std::fmt::Debug for WsSendError {
    /// A refused upgrade's body is dropped: only its status is diagnostic.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Cancelled => f.write_str("Cancelled"),
            Self::Refused { status, body } => f
                .debug_struct("Refused")
                .field("status", status)
                .field("body_len", &body.len())
                .finish(),
            Self::ConnectFailed => f.write_str("ConnectFailed"),
            Self::WriteFailed => f.write_str("WriteFailed"),
            Self::Invalid(error) => f.debug_tuple("Invalid").field(error).finish(),
        }
    }
}

/// One bounded read of a response.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WsRead {
    /// The request's cancellation fired first; the connection is dropped.
    Cancelled,
    /// A frame, or the end of the connection: a close or a bound expiry, after
    /// which the connection is dropped.
    Next(WsNext),
    /// The read failed (a read error, or a pong that could not be written within the
    /// write bound); the connection is dropped.
    Failed(WsError),
}

/// The WebSocket service of one provider instance.
pub struct WsSession {
    connector: Arc<dyn WsConnector>,
    clock: Clock,
    slot: Arc<Mutex<Slot>>,
}

/// The one connection slot. A lease takes the connection out of it while a
/// response uses it, so a half-read socket is dropped with the lease that owned it.
pub struct Slot {
    live: Option<Live>,
}

/// One open connection and what the session knows about it.
struct Live {
    connection: Box<dyn WsConnection>,
    connected_at: Instant,
    last_used_at: Instant,
    last_clean_response: Option<String>,
}

impl WsSession {
    pub fn new(connector: Arc<dyn WsConnector>, clock: Clock) -> Self {
        Self {
            connector,
            clock,
            slot: Arc::new(Mutex::new(Slot { live: None })),
        }
    }

    /// Lease the session for one request WITHOUT waiting. `None` means another
    /// request holds it, and §4 sends such a request over HTTP.
    pub fn try_lease(&self) -> Option<WsLease> {
        let slot = Arc::clone(&self.slot).try_lock_owned().ok()?;
        Some(WsLease {
            connector: Arc::clone(&self.connector),
            clock: Arc::clone(&self.clock),
            slot,
            live: None,
            reused: false,
            failed_before_output: false,
        })
    }
}

/// One request's hold on the session. Dropping it drops a connection it is using.
pub struct WsLease {
    connector: Arc<dyn WsConnector>,
    clock: Clock,
    slot: OwnedMutexGuard<Slot>,
    /// The connection the current attempt uses, taken out of the slot.
    live: Option<Live>,
    /// Whether the current attempt's connection was already open when it was used:
    /// a send without a head. Kept after the connection is dropped, so the caller
    /// can still tell a stale socket from a fresh one that failed.
    reused: bool,
    failed_before_output: bool,
}

impl WsLease {
    /// The facts of WIT `connection-state`. A connection past §4's age or idle bound
    /// is dropped here and reported closed.
    pub fn state(&mut self) -> ConnectionState {
        let now = (self.clock)();
        if self.slot.live.as_ref().is_some_and(|live| {
            now.duration_since(live.connected_at) >= MAX_AGE
                || now.duration_since(live.last_used_at) >= MAX_IDLE
        }) {
            self.slot.live = None;
        }
        let live = self.live.as_ref().or(self.slot.live.as_ref());
        ConnectionState {
            open: live.is_some(),
            last_clean_response: live.and_then(|live| live.last_clean_response.clone()),
            failed_before_output: self.failed_before_output,
        }
    }

    /// Execute one `websocket-send`. With a head, any connection this instance has
    /// is dropped and a new one is opened under the write bound; without one, the
    /// open connection is used. The frame is then written under the write bound.
    /// Every wait races `cancel`, and every failure drops the connection.
    pub async fn send(
        &mut self,
        authority: WsAuthority<'_>,
        send: WsSend,
        cancel: &CancellationToken,
    ) -> Result<(), WsSendError> {
        if cancel.is_cancelled() {
            return Err(WsSendError::Cancelled);
        }
        let WsSend { handshake, frame } = send;
        match handshake {
            Some(head) => {
                self.live = None;
                self.slot.live = None;
                self.reused = false;
                let handshake = handshake_for(authority, &head).map_err(WsSendError::Invalid)?;
                let connector = Arc::clone(&self.connector);
                let connection =
                    match race_bounded(cancel, WRITE_TIMEOUT, connector.connect(handshake)).await {
                        Raced::Cancelled => return Err(WsSendError::Cancelled),
                        Raced::Done(Err(_elapsed)) => return Err(WsSendError::ConnectFailed),
                        Raced::Done(Ok(Err(WsConnectError::Status { status, body }))) => {
                            return Err(WsSendError::Refused { status, body });
                        }
                        Raced::Done(Ok(Err(WsConnectError::Failed(_)))) => {
                            return Err(WsSendError::ConnectFailed);
                        }
                        Raced::Done(Ok(Ok(connection))) => connection,
                    };
                let now = (self.clock)();
                self.live = Some(Live {
                    connection,
                    connected_at: now,
                    last_used_at: now,
                    last_clean_response: None,
                });
                if cancel.is_cancelled() {
                    self.live = None;
                    return Err(WsSendError::Cancelled);
                }
            }
            None => {
                if self.live.is_none() {
                    self.live = self.slot.live.take();
                }
                self.reused = self.live.is_some();
            }
        }
        // A send without a head whose connection is gone before the frame went out
        // is a failure before any output.
        let Some(live) = self.live.as_mut() else {
            return Err(WsSendError::WriteFailed);
        };
        let written = race_bounded(cancel, WRITE_TIMEOUT, live.connection.send_text(frame)).await;
        match written {
            Raced::Done(Ok(Ok(()))) => Ok(()),
            Raced::Cancelled => {
                self.live = None;
                Err(WsSendError::Cancelled)
            }
            Raced::Done(_) => {
                self.live = None;
                Err(WsSendError::WriteFailed)
            }
        }
    }

    /// The next frame of the response, under the connection's read bounds and racing
    /// `cancel`. Anything but a text frame ends the connection.
    pub async fn next(&mut self, cancel: &CancellationToken) -> WsRead {
        let Some(live) = self.live.as_mut() else {
            return WsRead::Failed(WsError("WebSocket connection is gone".to_string()));
        };
        let read = race(cancel, live.connection.next_bounded()).await;
        match read {
            Raced::Done(Ok(WsNext::Text(text))) => WsRead::Next(WsNext::Text(text)),
            Raced::Done(Ok(end)) => {
                self.live = None;
                WsRead::Next(end)
            }
            Raced::Done(Err(error)) => {
                self.live = None;
                WsRead::Failed(error)
            }
            Raced::Cancelled => {
                self.live = None;
                WsRead::Cancelled
            }
        }
    }

    /// The response completed cleanly: the connection goes back to the session,
    /// remembering `response_id` as its last clean response.
    pub fn completed(&mut self, response_id: Option<String>) {
        if let Some(mut live) = self.live.take() {
            live.last_used_at = (self.clock)();
            live.last_clean_response = response_id;
            self.slot.live = Some(live);
        }
    }

    /// Drop the connection the current attempt uses (a cancelled or failed
    /// response, or a reconnect).
    pub fn drop_connection(&mut self) {
        self.live = None;
    }

    /// The attempt failed before any output and the caller allows it no further
    /// WebSocket attempt: drop the connection and report
    /// [`ConnectionState::failed_before_output`] from now on.
    pub fn fail_before_output(&mut self) {
        self.live = None;
        self.failed_before_output = true;
    }

    /// Whether the current (or just failed) attempt used a connection that was
    /// already open.
    pub fn reused(&self) -> bool {
        self.reused
    }
}

/// The handshake a head opens: the route's endpoint with its scheme swapped and the
/// head's path appended, the credential headers first, then the head's own.
fn handshake_for(authority: WsAuthority<'_>, head: &WsHead) -> Result<WsHandshake, ProviderError> {
    if !path_under_endpoint(&head.path) {
        return Err(ProviderError::new(
            ProviderErrorKind::Protocol,
            PATH_REFUSED,
        ));
    }
    check_lowered_headers(&head.headers, &head.credential)?;
    let mut headers = Vec::with_capacity(head.headers.len() + 2);
    if let Some(credential) = authority.credential {
        match head.credential.scheme {
            CredentialScheme::Bearer => headers.push((
                "Authorization".to_string(),
                format!("Bearer {}", credential.bearer),
            )),
        }
        if let Some(name) = &head.credential.account_id_header {
            let account_id = credential.account_id.as_deref().ok_or_else(|| {
                ProviderError::new(ProviderErrorKind::Authentication, ACCOUNT_ID_MISSING)
            })?;
            headers.push((name.clone(), account_id.to_string()));
        }
    }
    headers.extend(head.headers.iter().cloned());
    Ok(WsHandshake {
        url: format!("{}{}", websocket_url(authority.endpoint), head.path),
        headers,
    })
}

/// Empty (the endpoint itself), or an absolute path that cannot leave it: no
/// authority, query, fragment, escape or dot segment.
fn path_under_endpoint(path: &str) -> bool {
    if path.is_empty() {
        return true;
    }
    let Some(rest) = path.strip_prefix('/') else {
        return false;
    };
    let lowered = path.to_ascii_lowercase();
    !(rest.starts_with('/')
        || lowered.contains("%2f")
        || lowered.contains("%5c")
        || lowered.contains("%2e")
        || path
            .chars()
            .any(|c| matches!(c, '\\' | '#' | '@' | '?') || c.is_whitespace() || c.is_control())
        || rest
            .split('/')
            .any(|segment| segment == "." || segment == ".."))
}

/// §3: the `http(s)` endpoint with its scheme swapped `https`→`wss` (`http`→`ws`).
fn websocket_url(url: &str) -> String {
    if let Some(rest) = url.strip_prefix("https://") {
        format!("wss://{rest}")
    } else if let Some(rest) = url.strip_prefix("http://") {
        format!("ws://{rest}")
    } else {
        url.to_string()
    }
}

enum Raced<T> {
    Done(T),
    Cancelled,
}

/// Await `future`, but stop as soon as `cancel` fires (§4).
async fn race<T>(cancel: &CancellationToken, future: impl Future<Output = T>) -> Raced<T> {
    let cancelled = cancel.cancelled();
    let future = std::pin::pin!(future);
    let cancelled = std::pin::pin!(cancelled);
    match select(future, cancelled).await {
        Either::Left((value, _)) => Raced::Done(value),
        Either::Right(((), _)) => Raced::Cancelled,
    }
}

/// Await `future` under `bound` and under cancellation (§4).
async fn race_bounded<T>(
    cancel: &CancellationToken,
    bound: Duration,
    future: impl Future<Output = T>,
) -> Raced<Result<T, tokio::time::error::Elapsed>> {
    race(cancel, tokio::time::timeout(bound, future)).await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn head(path: &str, headers: Vec<(&str, &str)>) -> WsHead {
        WsHead {
            path: path.to_string(),
            headers: headers
                .into_iter()
                .map(|(name, value)| (name.to_string(), value.to_string()))
                .collect(),
            credential: CredentialUse {
                scheme: CredentialScheme::Bearer,
                account_id_header: Some("chatgpt-account-id".to_string()),
            },
        }
    }

    fn credential() -> Credential {
        Credential {
            bearer: "SENTINEL-BEARER".to_string(),
            account_id: Some("acct".to_string()),
        }
    }

    #[test]
    fn the_scheme_is_swapped_and_nothing_else_changes() {
        assert_eq!(
            websocket_url("https://chatgpt.com/backend-api/codex/responses"),
            "wss://chatgpt.com/backend-api/codex/responses"
        );
        assert_eq!(
            websocket_url("http://example.test/codex/responses"),
            "ws://example.test/codex/responses"
        );
        assert_eq!(websocket_url("wss://example.test"), "wss://example.test");
    }

    #[test]
    fn the_credential_is_attached_first_and_only_by_the_session() {
        let credential = credential();
        let authority = WsAuthority {
            endpoint: "https://host.test/codex/responses",
            credential: Some(&credential),
        };
        let handshake = handshake_for(authority, &head("", vec![("originator", "p1")])).unwrap();
        assert_eq!(handshake.url, "wss://host.test/codex/responses");
        let names: Vec<&str> = handshake.headers.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, ["Authorization", "chatgpt-account-id", "originator"]);

        let proxied = WsAuthority {
            credential: None,
            ..authority
        };
        let handshake = handshake_for(proxied, &head("", vec![("originator", "p1")])).unwrap();
        assert_eq!(
            handshake.headers.len(),
            1,
            "a proxy-injected route attaches nothing"
        );
    }

    #[test]
    fn a_head_that_names_a_credential_or_leaves_the_endpoint_is_refused() {
        let credential = credential();
        let authority = WsAuthority {
            endpoint: "https://host.test/codex/responses",
            credential: Some(&credential),
        };
        for bad in [
            head("", vec![("Authorization", "Bearer SENTINEL-HEADER-VALUE")]),
            head("", vec![("chatgpt-account-id", "other")]),
            head("", vec![("x", "a\r\nb")]),
        ] {
            let error = handshake_for(authority, &bad).unwrap_err();
            assert_eq!(error.kind, ProviderErrorKind::Protocol);
            assert!(!error.message.contains("SENTINEL"), "{error}");
        }
        for path in ["x", "//evil.test", "/../x", "/a?q=1", "/%2e%2e/x", "/a#b"] {
            let error = handshake_for(authority, &head(path, Vec::new())).unwrap_err();
            assert_eq!(error.message, PATH_REFUSED, "{path}");
        }
        assert!(handshake_for(authority, &head("/sub/path", Vec::new())).is_ok());
    }

    #[test]
    fn a_send_and_a_send_error_debug_never_show_a_frame_body_or_header_value() {
        let send = WsSend {
            handshake: Some(head("", vec![("session-id", "SENTINEL-HEADER-VALUE")])),
            frame: "SENTINEL-FRAME".to_string(),
        };
        let error = WsSendError::Refused {
            status: 401,
            body: b"SENTINEL-BODY".to_vec(),
        };
        let text = format!("{send:?} {error:?}");
        assert!(!text.contains("SENTINEL"), "{text}");
        assert!(
            text.contains("session-id") && text.contains("401"),
            "{text}"
        );
    }
}
