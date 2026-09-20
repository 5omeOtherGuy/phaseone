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
//! Both connection calls return a [`BoxFuture`], so a caller races them against
//! its own bounds — `docs/design/websocket.md` §4 bounds a connect and a send at
//! 10 s and every wait races the request's cancellation token. Dropping a connect
//! drops its socket; §4 is why an abandoned connection is never reused, so a
//! caller that gives up on a call must drop the connection with it.

use futures_util::{SinkExt, StreamExt};
use p1_contracts::BoxFuture;
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::error::Error as TungsteniteError;
use tokio_tungstenite::tungstenite::error::ProtocolError;
use tokio_tungstenite::tungstenite::http::{HeaderName, HeaderValue};
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async};

use crate::http::RedactedUrl;

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
            Ok(Box::new(TungsteniteConnection { stream }) as Box<dyn WsConnection>)
        })
    }
}

/// One open `tokio-tungstenite` connection. Both methods drive the same socket,
/// so they both take `&mut self`.
struct TungsteniteConnection {
    stream: WebSocketStream<MaybeTlsStream<TcpStream>>,
}

impl WsConnection for TungsteniteConnection {
    fn send_text<'a>(&'a mut self, text: String) -> BoxFuture<'a, Result<(), WsError>> {
        Box::pin(async move {
            self.stream
                .send(Message::text(text))
                .await
                .map_err(|error| WsError(format!("WebSocket send error ({})", class(&error))))
        })
    }

    fn next_text<'a>(&'a mut self) -> BoxFuture<'a, Result<Option<String>, WsError>> {
        Box::pin(async move {
            loop {
                let message = match self.stream.next().await {
                    Some(Ok(message)) => message,
                    // The stream is fused: a close handshake or end of stream.
                    Some(Err(error)) if is_end_of_stream(&error) => return Ok(None),
                    Some(Err(error)) => {
                        return Err(WsError(format!("WebSocket read error ({})", class(&error))));
                    }
                    None => return Ok(None),
                };
                match message {
                    Message::Text(text) => return Ok(Some(text.to_string())),
                    Message::Binary(bytes) => {
                        return String::from_utf8(bytes.to_vec()).map(Some).map_err(|_| {
                            WsError("WebSocket binary frame is not UTF-8".to_string())
                        });
                    }
                    // Answered here, inside `next_text`, so a peer sees the pong
                    // without the caller having to send anything.
                    Message::Ping(payload) => {
                        self.stream
                            .send(Message::Pong(payload))
                            .await
                            .map_err(|error| {
                                WsError(format!("WebSocket send error ({})", class(&error)))
                            })?;
                    }
                    Message::Pong(_) => {}
                    Message::Close(_) => return Ok(None),
                    Message::Frame(_) => {}
                }
            }
        })
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
}
