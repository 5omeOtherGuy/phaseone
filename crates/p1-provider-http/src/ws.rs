//! The WebSocket connector seam: one handshake in, one connection out.
//!
//! [`WsConnector`]/[`WsConnection`] are the only socket-shaped types a WebSocket
//! provider path sees, so an adapter is testable against
//! [`crate::testing::ScriptedWsConnector`] with no network. [`TungsteniteConnector`]
//! is the one real implementation, and the only code in the workspace that names
//! `tokio-tungstenite`.
//!
//! As in [`crate::http`], nothing here may put a header value into a `Debug`
//! output or an error message: a handshake carries the credential.
//!
//! A caller races a connect and a send against [`WRITE_TIMEOUT`] —
//! `docs/design/websocket.md` §4 bounds each at 10 s — and every wait races the
//! request's cancellation token; [`crate::ws_session`] is that caller. A read is
//! bounded HERE, inside the connection, because only the message loop sees every
//! frame: the first frame after a send waits [`FIRST_BYTE_TIMEOUT`], every later
//! one waits [`STREAM_IDLE_TIMEOUT`], and any message (control frames included)
//! resets the idle clock. Dropping a connect drops its socket; §4 is why an
//! abandoned connection is never reused, so a caller that gives up on a call must
//! drop the connection with it.

use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use p1_contracts::BoxFuture;
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::error::Error as TungsteniteError;
use tokio_tungstenite::tungstenite::error::ProtocolError;
use tokio_tungstenite::tungstenite::http::{HeaderName, HeaderValue};
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async};

use crate::http::{
    FIRST_BYTE_TIMEOUT, RedactedUrl, STREAM_IDLE_TIMEOUT, first_byte_timeout_message,
    stream_idle_timeout_message,
};

/// The write bound (10 s, `docs/design/websocket.md` §4): one connect, one send,
/// and the pong that answers a peer's ping. The pong is bounded like a caller's own
/// send because a peer that stops reading its socket while the write buffer fills
/// would otherwise block the read loop forever — the very unbounded provider wait
/// issue #164 removes. Cancellation is no longer the only thing that ends it.
pub const WRITE_TIMEOUT: Duration = Duration::from_secs(10);

/// One handshake: the URL to open and the headers to send with it.
#[derive(Clone, PartialEq, Eq)]
pub struct WsHandshake {
    pub url: String,
    pub headers: Vec<(String, String)>,
}

impl std::fmt::Debug for WsHandshake {
    /// Header *values* never appear (the handshake carries the credential); the
    /// query string is dropped because it may carry prompt material.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let names: Vec<&str> = self.headers.iter().map(|(name, _)| name.as_str()).collect();
        f.debug_struct("WsHandshake")
            .field("url", &RedactedUrl(&self.url))
            .field("header_names", &names)
            .finish()
    }
}

/// The WebSocket-shaped network seam.
pub trait WsConnector: Send + Sync {
    /// Open one connection. A refused upgrade is a [`WsConnectError::Status`],
    /// never an [`WsConnection`] carrying a bad status.
    fn connect<'a>(
        &'a self,
        request: WsHandshake,
    ) -> BoxFuture<'a, Result<Box<dyn WsConnection>, WsConnectError>>;
}

/// One open connection.
pub trait WsConnection: Send {
    /// Send one text frame.
    fn send_text<'a>(&'a mut self, text: String) -> BoxFuture<'a, Result<(), WsError>>;

    /// The next TEXT payload. Binary frames are decoded as UTF-8; ping is answered
    /// and pong ignored inside the implementation; a close frame or end of stream
    /// is `Ok(None)`.
    fn next_text<'a>(&'a mut self) -> BoxFuture<'a, Result<Option<String>, WsError>>;

    /// The next frame under this connection's read bounds. The first frame after a
    /// send waits [`FIRST_BYTE_TIMEOUT`]; every later one waits
    /// [`STREAM_IDLE_TIMEOUT`], and ANY received message — text, binary, ping or
    /// pong — resets that clock, so a keep-alive peer is never called idle. A bound
    /// that expires is [`WsNext::Timeout`], distinct from a close, so a caller can
    /// name it.
    ///
    /// The default forwards [`next_text`](WsConnection::next_text) with no bound, so
    /// an existing implementor keeps compiling — it is all a simple test double
    /// needs. A real connection, or ANY decorator that wraps one, MUST override it:
    /// the default can never report [`WsNext::Timeout`], so an implementor that just
    /// delegates would silently drop the bound and see an expiry as a close.
    /// [`TungsteniteConnector`] and [`crate::testing::ScriptedWsConnector`] override it.
    fn next_bounded<'a>(&'a mut self) -> BoxFuture<'a, Result<WsNext, WsError>> {
        Box::pin(async move {
            Ok(match self.next_text().await? {
                Some(text) => WsNext::Text(text),
                None => WsNext::Closed,
            })
        })
    }
}

/// Which read bound a connection applies at this moment.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WsBound {
    /// No frame arrived after the request frame was sent.
    FirstFrame,
    /// No frame arrived for the idle bound on an open stream.
    Idle,
}

impl WsBound {
    /// The deadline this bound allows.
    pub fn limit(self) -> Duration {
        match self {
            Self::FirstFrame => FIRST_BYTE_TIMEOUT,
            Self::Idle => STREAM_IDLE_TIMEOUT,
        }
    }

    /// The `Transport` message an expiry becomes. It is the SSE arm's wording, so
    /// both transports name the bound identically.
    pub fn message(self) -> String {
        match self {
            Self::FirstFrame => first_byte_timeout_message(),
            Self::Idle => stream_idle_timeout_message(),
        }
    }
}

/// The outcome of one bounded read ([`WsConnection::next_bounded`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WsNext {
    /// A model-visible payload: TEXT, or BINARY decoded as UTF-8.
    Text(String),
    /// The peer closed, or the socket ended.
    Closed,
    /// No message arrived within the bound.
    Timeout(WsBound),
}

/// A handshake that never became a connection.
#[derive(Clone, PartialEq, Eq, thiserror::Error)]
pub enum WsConnectError {
    /// The upgrade was answered with a status other than `101`. `body` is
    /// whatever the peer sent with that answer (often empty).
    #[error("WebSocket upgrade refused with status {status}")]
    Status { status: u16, body: Vec<u8> },
    /// No HTTP answer at all: DNS, TCP, TLS or a malformed request. The message
    /// names the failure class, never a URL query string or a header value.
    #[error("{0}")]
    Failed(String),
}

impl std::fmt::Debug for WsConnectError {
    /// The refused-upgrade *body* is dropped: only the status is diagnostic.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Status { status, body } => f
                .debug_struct("Status")
                .field("status", status)
                .field("body_len", &body.len())
                .finish(),
            Self::Failed(message) => f.debug_tuple("Failed").field(message).finish(),
        }
    }
}

/// A connection failure. The message names a failure class, never a header value.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct WsError(pub String);

/// The real connector: `tokio-tungstenite` over rustls with webpki roots.
pub struct TungsteniteConnector {}

impl TungsteniteConnector {
    pub fn new() -> Self {
        Self {}
    }
}

impl Default for TungsteniteConnector {
    fn default() -> Self {
        Self::new()
    }
}

impl WsConnector for TungsteniteConnector {
    fn connect<'a>(
        &'a self,
        request: WsHandshake,
    ) -> BoxFuture<'a, Result<Box<dyn WsConnection>, WsConnectError>> {
        Box::pin(async move {
            let mut http_request = request.url.clone().into_client_request().map_err(|_| {
                WsConnectError::Failed("WebSocket connect error (request)".to_string())
            })?;
            for (name, value) in &request.headers {
                let name = HeaderName::from_bytes(name.as_bytes()).map_err(|_| {
                    WsConnectError::Failed("WebSocket connect error (header name)".to_string())
                })?;
                let value = HeaderValue::from_str(value).map_err(|_| {
                    // The offending value is deliberately not named: it is the credential.
                    WsConnectError::Failed("WebSocket connect error (header value)".to_string())
                })?;
                http_request.headers_mut().insert(name, value);
            }
            let (stream, _response) = connect_async(http_request).await.map_err(connect_error)?;
            Ok(Box::new(TungsteniteConnection {
                stream,
                awaiting_first_frame: false,
            }) as Box<dyn WsConnection>)
        })
    }
}

/// One open `tokio-tungstenite` connection. Both methods drive the same socket,
/// so they both take `&mut self`.
struct TungsteniteConnection {
    stream: WebSocketStream<MaybeTlsStream<TcpStream>>,
    /// Whether no frame has arrived since the request frame was sent. The NEXT
    /// message is awaited under [`WsBound::FirstFrame`]; a received one clears it.
    awaiting_first_frame: bool,
}

impl WsConnection for TungsteniteConnection {
    fn send_text<'a>(&'a mut self, text: String) -> BoxFuture<'a, Result<(), WsError>> {
        Box::pin(async move {
            self.stream
                .send(Message::text(text))
                .await
                .map_err(|error| WsError(format!("WebSocket send error ({})", class(&error))))?;
            // The next message is what the first-frame bound awaits.
            self.awaiting_first_frame = true;
            Ok(())
        })
    }

    fn next_text<'a>(&'a mut self) -> BoxFuture<'a, Result<Option<String>, WsError>> {
        Box::pin(async move {
            // The bounded read is the one that reports a bound expiry; the legacy
            // text view folds it into `Ok(None)` (a caller that must name the
            // expiry uses `next_bounded`).
            match self.next_bounded().await? {
                WsNext::Text(text) => Ok(Some(text)),
                WsNext::Closed | WsNext::Timeout(_) => Ok(None),
            }
        })
    }

    fn next_bounded<'a>(&'a mut self) -> BoxFuture<'a, Result<WsNext, WsError>> {
        Box::pin(
            async move { read_bounded(&mut self.stream, &mut self.awaiting_first_frame).await },
        )
    }
}

/// One raw protocol message a bounded read can see. Control frames are distinct
/// from a model-visible payload because they only reset the idle clock.
pub(crate) enum RawMessage {
    Text(String),
    Binary(Vec<u8>),
    /// A ping carrying this payload; the read loop answers it with a pong.
    Ping(Vec<u8>),
    Pong,
    Close,
}

/// The raw message channel a bounded read drives. The real socket and a test
/// double's script both provide one, so the control-frame handling and the
/// bound reset are a single implementation (issue #164).
pub(crate) trait MessageChannel: Send {
    /// The next raw message: `Ok(None)` is the end of the socket, `Err` a
    /// pre-classified failure (never a header value or a peer byte).
    fn next_message(&mut self) -> BoxFuture<'_, Result<Option<RawMessage>, WsError>>;

    /// Answer a ping.
    fn pong(&mut self, payload: Vec<u8>) -> BoxFuture<'_, Result<(), WsError>>;
}

/// Read the next model-visible frame under the connection's bounds. The wait is
/// re-armed for EVERY message — a text, a binary, a ping or a pong — so a peer
/// that answers only with WS control pings is alive, not idle (issue #164).
pub(crate) async fn read_bounded(
    channel: &mut dyn MessageChannel,
    awaiting_first_frame: &mut bool,
) -> Result<WsNext, WsError> {
    loop {
        let bound = if *awaiting_first_frame {
            WsBound::FirstFrame
        } else {
            WsBound::Idle
        };
        let message = match tokio::time::timeout(bound.limit(), channel.next_message()).await {
            Err(_elapsed) => return Ok(WsNext::Timeout(bound)),
            Ok(Err(error)) => return Err(error),
            Ok(Ok(None)) => return Ok(WsNext::Closed),
            Ok(Ok(Some(message))) => message,
        };
        // Any message is life: it ends the first-frame wait and resets the clock.
        *awaiting_first_frame = false;
        match message {
            RawMessage::Text(text) => return Ok(WsNext::Text(text)),
            RawMessage::Binary(bytes) => {
                return String::from_utf8(bytes)
                    .map(WsNext::Text)
                    .map_err(|_| WsError("WebSocket binary frame is not UTF-8".to_string()));
            }
            // Answered here, inside the read loop, so a peer sees the pong without
            // the caller having to send anything. The write is bounded like a caller's
            // own send (see `WRITE_TIMEOUT`); an expiry or a write error is the
            // read failure it already is, which the caller takes as its close path.
            RawMessage::Ping(payload) => {
                match tokio::time::timeout(WRITE_TIMEOUT, channel.pong(payload)).await {
                    Ok(result) => result?,
                    Err(_elapsed) => {
                        return Err(WsError(format!(
                            "WebSocket pong write timed out after {} s",
                            WRITE_TIMEOUT.as_secs()
                        )));
                    }
                }
            }
            RawMessage::Pong => {}
            RawMessage::Close => return Ok(WsNext::Closed),
        }
    }
}

impl MessageChannel for WebSocketStream<MaybeTlsStream<TcpStream>> {
    fn next_message(&mut self) -> BoxFuture<'_, Result<Option<RawMessage>, WsError>> {
        Box::pin(async move {
            loop {
                match self.next().await {
                    Some(Ok(message)) => {
                        if let Some(message) = raw_message(message) {
                            return Ok(Some(message));
                        }
                    }
                    // The stream is fused: a close handshake or end of stream.
                    Some(Err(error)) if is_end_of_stream(&error) => return Ok(None),
                    Some(Err(error)) => {
                        return Err(WsError(format!("WebSocket read error ({})", class(&error))));
                    }
                    None => return Ok(None),
                }
            }
        })
    }

    fn pong(&mut self, payload: Vec<u8>) -> BoxFuture<'_, Result<(), WsError>> {
        Box::pin(async move {
            self.send(Message::Pong(payload.into()))
                .await
                .map_err(|error| WsError(format!("WebSocket send error ({})", class(&error))))
        })
    }
}

/// Reduce one `tokio-tungstenite` message to a [`RawMessage`]. `Message::Frame`
/// is a raw frame no provider route uses, so it is skipped.
fn raw_message(message: Message) -> Option<RawMessage> {
    match message {
        Message::Text(text) => Some(RawMessage::Text(text.to_string())),
        Message::Binary(bytes) => Some(RawMessage::Binary(bytes.to_vec())),
        Message::Ping(payload) => Some(RawMessage::Ping(payload.to_vec())),
        Message::Pong(_) => Some(RawMessage::Pong),
        Message::Close(_) => Some(RawMessage::Close),
        Message::Frame(_) => None,
    }
}

/// A refused upgrade keeps its status and body; everything else is a class only.
fn connect_error(error: TungsteniteError) -> WsConnectError {
    match error {
        TungsteniteError::Http(response) => WsConnectError::Status {
            status: response.status().as_u16(),
            body: response.into_body().unwrap_or_default(),
        },
        other => WsConnectError::Failed(format!("WebSocket connect error ({})", class(&other))),
    }
}

/// A peer that went away without a close handshake is the end of the stream, not
/// a failure: §2 of the design says "a close frame or end of stream is `Ok(None)`".
fn is_end_of_stream(error: &TungsteniteError) -> bool {
    matches!(
        error,
        TungsteniteError::ConnectionClosed
            | TungsteniteError::AlreadyClosed
            | TungsteniteError::Protocol(ProtocolError::ResetWithoutClosingHandshake)
    )
}

/// The failure *class* is enough to act on (fall back to SSE vs fail); a URL, a
/// query string, a header value and a peer's bytes are all deliberately dropped.
fn class(error: &TungsteniteError) -> &'static str {
    match error {
        TungsteniteError::ConnectionClosed | TungsteniteError::AlreadyClosed => "closed",
        TungsteniteError::Io(_) => "io",
        TungsteniteError::Tls(_) => "tls",
        TungsteniteError::Capacity(_) => "capacity",
        TungsteniteError::Protocol(ProtocolError::ResetWithoutClosingHandshake) => "reset",
        TungsteniteError::Protocol(_) => "protocol",
        TungsteniteError::WriteBufferFull(_) => "write buffer",
        TungsteniteError::Utf8(_) => "utf8",
        TungsteniteError::AttackAttempt => "attack",
        TungsteniteError::Url(_) => "url",
        TungsteniteError::Http(_) => "http",
        TungsteniteError::HttpFormat(_) => "http format",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn handshake_debug_never_shows_a_header_value() {
        let handshake = WsHandshake {
            url: "wss://host.test/path?secret=SENTINEL-HEADER-VALUE".to_string(),
            headers: vec![(
                "Authorization".to_string(),
                "SENTINEL-HEADER-VALUE".to_string(),
            )],
        };
        let text = format!("{handshake:?}");
        assert!(!text.contains("SENTINEL-HEADER-VALUE"), "{text}");
        assert!(text.contains("Authorization"), "{text}");
        assert!(text.contains("wss://host.test/path"), "{text}");
    }

    #[test]
    fn connect_error_debug_never_shows_the_body_or_a_header_value() {
        let error = WsConnectError::Status {
            status: 401,
            body: b"SENTINEL-HEADER-VALUE".to_vec(),
        };
        let text = format!("{error:?} {error}");
        assert!(!text.contains("SENTINEL-HEADER-VALUE"), "{text}");
        assert!(text.contains("401"), "{text}");
        assert_eq!(
            WsConnectError::Failed("class".to_string()).to_string(),
            "class"
        );
    }

    /// One step of the scripted raw channel below.
    enum Step {
        Message(RawMessage),
        Wait(Duration),
    }

    /// The WS test double for the bounded read: a scripted raw channel that drives
    /// the crate's REAL `read_bounded`, so a test exercises the production clock.
    struct Scripted {
        steps: std::collections::VecDeque<Step>,
        /// A pong write that never completes, the way a peer that stopped reading its
        /// socket fills the write buffer (see `WRITE_TIMEOUT`).
        stalling_pong: bool,
    }

    impl Scripted {
        fn new(steps: Vec<Step>) -> Self {
            Self {
                steps: steps.into(),
                stalling_pong: false,
            }
        }

        fn with_stalling_pong(steps: Vec<Step>) -> Self {
            Self {
                steps: steps.into(),
                stalling_pong: true,
            }
        }
    }

    impl MessageChannel for Scripted {
        fn next_message(&mut self) -> BoxFuture<'_, Result<Option<RawMessage>, WsError>> {
            Box::pin(async move {
                loop {
                    match self.steps.pop_front() {
                        Some(Step::Message(message)) => return Ok(Some(message)),
                        Some(Step::Wait(delay)) => tokio::time::sleep(delay).await,
                        None => return Ok(None),
                    }
                }
            })
        }

        fn pong(&mut self, _payload: Vec<u8>) -> BoxFuture<'_, Result<(), WsError>> {
            let stalling = self.stalling_pong;
            Box::pin(async move {
                if stalling {
                    tokio::time::sleep(Duration::from_secs(3600)).await;
                }
                Ok(())
            })
        }
    }

    /// Issue #164: a peer that keeps the socket alive with CONTROL pings is not
    /// idle — every message resets the clock — so 20 minutes of pings then a text
    /// frame completes instead of being failed as silent.
    #[tokio::test(start_paused = true)]
    async fn a_control_ping_resets_the_idle_clock() {
        let mut steps = Vec::new();
        for _ in 0..6 {
            steps.push(Step::Message(RawMessage::Ping(Vec::new())));
            steps.push(Step::Wait(Duration::from_secs(200)));
        }
        steps.push(Step::Message(RawMessage::Text("done".to_string())));
        let mut channel = Scripted::new(steps);
        let mut awaiting_first_frame = true;

        let start = tokio::time::Instant::now();
        let next = tokio::time::timeout(
            Duration::from_secs(1201),
            read_bounded(&mut channel, &mut awaiting_first_frame),
        )
        .await
        .expect("control pings must keep the stream alive, not hang CI");
        assert_eq!(next.unwrap(), WsNext::Text("done".to_string()));
        assert_eq!(start.elapsed(), Duration::from_secs(1200));
    }

    /// A pong-only keep-alive resets the idle clock exactly like a ping.
    #[tokio::test(start_paused = true)]
    async fn a_pong_only_keep_alive_resets_the_idle_clock() {
        let mut steps = Vec::new();
        for _ in 0..6 {
            steps.push(Step::Message(RawMessage::Pong));
            steps.push(Step::Wait(Duration::from_secs(200)));
        }
        steps.push(Step::Message(RawMessage::Text("done".to_string())));
        let mut channel = Scripted::new(steps);
        let mut awaiting_first_frame = true;

        let start = tokio::time::Instant::now();
        let next = tokio::time::timeout(
            Duration::from_secs(1201),
            read_bounded(&mut channel, &mut awaiting_first_frame),
        )
        .await
        .expect("pongs must keep the stream alive, not hang CI");
        assert_eq!(next.unwrap(), WsNext::Text("done".to_string()));
        assert_eq!(start.elapsed(), Duration::from_secs(1200));
    }

    /// Review round 2: the pong the read loop owes a ping is bounded like a caller's
    /// send, so a peer that stopped reading cannot block the read forever — a stalled
    /// write is the read error the caller already takes as its close path.
    #[tokio::test(start_paused = true)]
    async fn a_pong_write_that_stalls_fails_the_read_instead_of_hanging() {
        let mut channel =
            Scripted::with_stalling_pong(vec![Step::Message(RawMessage::Ping(Vec::new()))]);
        let mut awaiting_first_frame = false;

        let start = tokio::time::Instant::now();
        let next = tokio::time::timeout(
            WRITE_TIMEOUT + Duration::from_secs(1),
            read_bounded(&mut channel, &mut awaiting_first_frame),
        )
        .await
        .expect("a stalled pong write must not hang the read");
        let error = next.expect_err("a stalled pong write is a read failure");
        assert!(error.0.contains("pong"), "{error:?}");
        assert_eq!(start.elapsed(), WRITE_TIMEOUT);
    }

    #[tokio::test(start_paused = true)]
    async fn silence_past_the_idle_bound_is_a_timeout() {
        let mut channel = Scripted::new(vec![Step::Wait(Duration::from_secs(3600))]);
        let mut awaiting_first_frame = false;

        let start = tokio::time::Instant::now();
        let next = tokio::time::timeout(
            Duration::from_secs(301),
            read_bounded(&mut channel, &mut awaiting_first_frame),
        )
        .await
        .expect("the idle bound must end the wait, not hang CI");
        assert_eq!(next.unwrap(), WsNext::Timeout(WsBound::Idle));
        assert_eq!(start.elapsed(), STREAM_IDLE_TIMEOUT);
    }

    #[tokio::test(start_paused = true)]
    async fn no_first_frame_past_the_first_byte_bound_is_a_timeout() {
        let mut channel = Scripted::new(vec![Step::Wait(Duration::from_secs(3600))]);
        let mut awaiting_first_frame = true;

        let start = tokio::time::Instant::now();
        let next = tokio::time::timeout(
            Duration::from_secs(121),
            read_bounded(&mut channel, &mut awaiting_first_frame),
        )
        .await
        .expect("the first-frame bound must end the wait, not hang CI");
        assert_eq!(next.unwrap(), WsNext::Timeout(WsBound::FirstFrame));
        assert_eq!(start.elapsed(), FIRST_BYTE_TIMEOUT);
    }
}
