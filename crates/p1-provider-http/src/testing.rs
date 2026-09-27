//! A scripted transport for tests, behind the cargo feature `testing`.
//!
//! This crate's own tests see it through `cfg(test)`; other crates enable the
//! `testing` feature. It is never linked into a production build.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::StreamExt;
use futures_util::stream;
use p1_contracts::BoxFuture;

use crate::http::{ByteStream, HttpRequest, HttpResponse, Transport, TransportError};
use crate::ws::{
    MessageChannel, RawMessage, WsConnectError, WsConnection, WsConnector, WsError, WsHandshake,
    WsNext, read_bounded,
};

/// A queue of canned responses plus a record of every request received.
///
/// `Clone` shares the queue and the record, so a clone handed to a provider
/// still reports the requests the provider sent.
#[derive(Clone, Debug)]
pub struct ScriptedTransport {
    inner: Arc<Mutex<ScriptedState>>,
}

#[derive(Debug)]
struct ScriptedState {
    responses: VecDeque<ScriptedResponse>,
    requests: Vec<HttpRequest>,
}

impl ScriptedTransport {
    /// Script `responses`, returned one per `post` in order.
    pub fn new(responses: Vec<ScriptedResponse>) -> Self {
        Self {
            inner: Arc::new(Mutex::new(ScriptedState {
                responses: responses.into(),
                requests: Vec::new(),
            })),
        }
    }

    /// Every request received so far, in order.
    pub fn requests(&self) -> Vec<HttpRequest> {
        self.lock().requests.clone()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, ScriptedState> {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

impl Transport for ScriptedTransport {
    fn post<'a>(
        &'a self,
        request: HttpRequest,
    ) -> BoxFuture<'a, Result<HttpResponse, TransportError>> {
        let response = {
            let mut state = self.lock();
            state.requests.push(request);
            let request_number = state.requests.len();
            state.responses.pop_front().unwrap_or_else(|| {
                panic!(
                    "ScriptedTransport: no scripted response left for request #{request_number}; \
                     script one response per expected request"
                )
            })
        };
        Box::pin(async move { response.into_http_response() })
    }
}

/// One canned response: a status, headers, body chunks and how the body ends.
#[derive(Clone, Debug)]
pub struct ScriptedResponse {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub chunks: Vec<Vec<u8>>,
    pub end: BodyEnd,
}

/// How a scripted body terminates.
#[derive(Clone, Debug)]
pub enum BodyEnd {
    /// Clean EOF.
    Eof,
    /// The body read fails with this message (a broken stream).
    Error(String),
    /// The body never produces another byte (a stalled provider).
    Hang,
}

impl ScriptedResponse {
    /// A response to a request that never reached the server: `post` itself
    /// fails. Encoded as status `0`, which no HTTP response can have.
    pub fn connect_error(message: impl Into<String>) -> Self {
        Self {
            status: 0,
            headers: Vec::new(),
            chunks: Vec::new(),
            end: BodyEnd::Error(message.into()),
        }
    }

    /// `200` with the whole SSE body in one chunk, ending cleanly.
    pub fn ok_sse(body: &str) -> Self {
        Self {
            status: 200,
            headers: Vec::new(),
            chunks: vec![body.as_bytes().to_vec()],
            end: BodyEnd::Eof,
        }
    }

    /// `200` with the SSE body split into two chunks at byte offset `at`.
    pub fn ok_sse_split(body: &str, at: usize) -> Self {
        let bytes = body.as_bytes();
        let at = at.min(bytes.len());
        Self {
            status: 200,
            headers: Vec::new(),
            chunks: vec![bytes[..at].to_vec(), bytes[at..].to_vec()],
            end: BodyEnd::Eof,
        }
    }

    fn into_http_response(self) -> Result<HttpResponse, TransportError> {
        let Self {
            status,
            headers,
            chunks,
            end,
        } = self;
        if status == 0 {
            let message = match end {
                BodyEnd::Error(message) => message,
                _ => "scripted connect error".to_string(),
            };
            return Err(TransportError(message));
        }
        Ok(HttpResponse {
            status,
            headers,
            body: body_stream(chunks, end),
        })
    }
}

fn body_stream(chunks: Vec<Vec<u8>>, end: BodyEnd) -> ByteStream {
    let chunks = chunks.into_iter().map(Ok::<Vec<u8>, TransportError>);
    match end {
        BodyEnd::Eof => Box::pin(stream::iter(chunks)),
        BodyEnd::Error(message) => Box::pin(stream::iter(
            chunks.chain(std::iter::once(Err(TransportError(message)))),
        )),
        BodyEnd::Hang => Box::pin(
            stream::iter(chunks).chain(stream::pending::<Result<Vec<u8>, TransportError>>()),
        ),
    }
}

/// A queue of scripted WebSocket connections plus a record of every handshake
/// received and every text frame sent.
///
/// `Clone` shares the queue and the record, so a clone handed to a provider
/// still reports what the provider sent.
#[derive(Clone, Debug)]
pub struct ScriptedWsConnector {
    inner: Arc<Mutex<ScriptedWsState>>,
}

#[derive(Debug)]
struct ScriptedWsState {
    connections: VecDeque<ScriptedConnection>,
    handshakes: Vec<WsHandshake>,
    /// Texts sent, one entry per ACCEPTED connection in connect order.
    sent_texts: Vec<Vec<String>>,
    /// Pongs written, one count per ACCEPTED connection in connect order.
    pongs: Vec<usize>,
}

impl ScriptedWsConnector {
    /// Script `connections`, consumed one per `connect` in order.
    pub fn new(connections: Vec<ScriptedConnection>) -> Self {
        Self {
            inner: Arc::new(Mutex::new(ScriptedWsState {
                connections: connections.into(),
                handshakes: Vec::new(),
                sent_texts: Vec::new(),
                pongs: Vec::new(),
            })),
        }
    }

    /// Every handshake received so far, in order, with its header names *and*
    /// values (a test may assert on them; nothing here is logged).
    pub fn handshakes(&self) -> Vec<WsHandshake> {
        self.lock().handshakes.clone()
    }

    /// The texts sent on each accepted connection, in connect order. Refused or
    /// failed connections never produced a connection, so they have no entry.
    pub fn sent_texts(&self) -> Vec<Vec<String>> {
        self.lock().sent_texts.clone()
    }

    /// The pongs written on each accepted connection, in connect order: every one
    /// answers a scripted ping, and one that stalled was never written.
    pub fn pongs(&self) -> Vec<usize> {
        self.lock().pongs.clone()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, ScriptedWsState> {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

impl WsConnector for ScriptedWsConnector {
    fn connect<'a>(
        &'a self,
        request: WsHandshake,
    ) -> BoxFuture<'a, Result<Box<dyn WsConnection>, WsConnectError>> {
        let (scripted, connection) = {
            let mut state = self.lock();
            state.handshakes.push(request);
            let handshake_number = state.handshakes.len();
            let scripted = state.connections.pop_front().unwrap_or_else(|| {
                panic!(
                    "ScriptedWsConnector: no scripted connection left for handshake \
                     #{handshake_number}; script one connection per expected connect"
                )
            });
            let connection = match scripted {
                ScriptedConnection::Accept(_) => {
                    state.sent_texts.push(Vec::new());
                    state.pongs.push(0);
                    Some(state.sent_texts.len() - 1)
                }
                ScriptedConnection::Refuse { .. } | ScriptedConnection::Fail(_) => None,
            };
            (scripted, connection)
        };
        let inner = Arc::clone(&self.inner);
        Box::pin(async move {
            match scripted {
                ScriptedConnection::Refuse { status, body } => {
                    Err(WsConnectError::Status { status, body })
                }
                ScriptedConnection::Fail(message) => Err(WsConnectError::Failed(message)),
                ScriptedConnection::Accept(frames) => {
                    let connection = connection.expect("an accepted connection is recorded");
                    Ok(Box::new(ScriptedWsConnection {
                        channel: ScriptedChannel {
                            frames: frames.into(),
                            inner: Arc::clone(&inner),
                            connection,
                            stall_pong: false,
                        },
                        awaiting_first_frame: false,
                        inner,
                        connection,
                    }) as Box<dyn WsConnection>)
                }
            }
        })
    }
}

/// A connector that REFUSES every connect with one status forever.
///
/// `ScriptedWsConnector` consumes one scripted attempt per connect, so a test that
/// only wants "this route never reaches a socket" would have to guess how many
/// attempts the provider makes. This one answers every handshake the same way — the
/// upgrade is refused with status 404 by default, which §5 of
/// `docs/design/websocket.md` turns into an immediate fall back to SSE — and counts
/// the handshakes so a test can prove the connector was never used at all.
#[derive(Clone, Debug)]
pub struct RefusingWsConnector {
    status: u16,
    handshakes: Arc<std::sync::atomic::AtomicUsize>,
}

impl RefusingWsConnector {
    /// Refuse every upgrade with `status`.
    pub fn new(status: u16) -> Self {
        Self {
            status,
            handshakes: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        }
    }

    /// How many handshakes were attempted.
    pub fn handshakes(&self) -> usize {
        self.handshakes.load(std::sync::atomic::Ordering::SeqCst)
    }
}

impl Default for RefusingWsConnector {
    /// The "the endpoint says no" refusal: §5 falls back to SSE at once.
    fn default() -> Self {
        Self::new(404)
    }
}

impl WsConnector for RefusingWsConnector {
    fn connect<'a>(
        &'a self,
        _request: WsHandshake,
    ) -> BoxFuture<'a, Result<Box<dyn WsConnection>, WsConnectError>> {
        self.handshakes
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let status = self.status;
        Box::pin(async move {
            Err(WsConnectError::Status {
                status,
                body: Vec::new(),
            })
        })
    }
}

/// One scripted connection attempt.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ScriptedConnection {
    /// The upgrade is answered with this status and body.
    Refuse { status: u16, body: Vec<u8> },
    /// No HTTP answer at all (DNS, TCP or TLS failure).
    Fail(String),
    /// The upgrade succeeds, and the connection then replays `frames` in order.
    Accept(Vec<ScriptedFrame>),
}

impl ScriptedConnection {
    /// Answer the upgrade with `status` and `body`.
    pub fn refuse(status: u16, body: impl Into<Vec<u8>>) -> Self {
        Self::Refuse {
            status,
            body: body.into(),
        }
    }

    /// Fail before any HTTP answer, with this message.
    pub fn fail(message: impl Into<String>) -> Self {
        Self::Fail(message.into())
    }

    /// Accept the upgrade and replay `frames`.
    pub fn accept(frames: Vec<ScriptedFrame>) -> Self {
        Self::Accept(frames)
    }
}

/// One scripted incoming message.
///
/// `Ping`/`Pong` are the WebSocket CONTROL frames a real peer sends to keep the
/// socket alive; `Wait` is a pause before the next frame is delivered, which the
/// fake clock advances instead of sleeping (issue #164's bound tests).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ScriptedFrame {
    Text(String),
    Ping,
    /// A ping whose pong write never completes, the way a peer that stopped
    /// reading its socket fills the write buffer.
    PingWithStalledPong,
    Pong,
    Wait(Duration),
    Error(String),
    Close,
}

impl ScriptedFrame {
    pub fn text(text: impl Into<String>) -> Self {
        Self::Text(text.into())
    }

    pub fn ping() -> Self {
        Self::Ping
    }

    /// A ping whose pong write never completes: the write bound must end it.
    pub fn ping_with_stalled_pong() -> Self {
        Self::PingWithStalledPong
    }

    pub fn pong() -> Self {
        Self::Pong
    }

    /// Pause for `delay` before the next frame is delivered.
    pub fn wait(delay: Duration) -> Self {
        Self::Wait(delay)
    }

    pub fn error(message: impl Into<String>) -> Self {
        Self::Error(message.into())
    }

    pub fn close() -> Self {
        Self::Close
    }
}

struct ScriptedWsConnection {
    channel: ScriptedChannel,
    /// Whether no frame has arrived since this connection's last `send_text`; the
    /// bounded read uses it exactly as the real connection does.
    awaiting_first_frame: bool,
    inner: Arc<Mutex<ScriptedWsState>>,
    connection: usize,
}

impl WsConnection for ScriptedWsConnection {
    fn send_text<'a>(&'a mut self, text: String) -> BoxFuture<'a, Result<(), WsError>> {
        Box::pin(async move {
            let mut state = self
                .inner
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            state.sent_texts[self.connection].push(text);
            drop(state);
            self.awaiting_first_frame = true;
            Ok(())
        })
    }

    fn next_text<'a>(&'a mut self) -> BoxFuture<'a, Result<Option<String>, WsError>> {
        Box::pin(async move {
            Ok(match self.next_bounded().await? {
                WsNext::Text(text) => Some(text),
                WsNext::Closed | WsNext::Timeout(_) => None,
            })
        })
    }

    fn next_bounded<'a>(&'a mut self) -> BoxFuture<'a, Result<WsNext, WsError>> {
        Box::pin(
            async move { read_bounded(&mut self.channel, &mut self.awaiting_first_frame).await },
        )
    }
}

/// The scripted raw message source: it replays [`ScriptedFrame`]s and drives the
/// crate's real bounded read, so a test exercises the production idle clock.
struct ScriptedChannel {
    frames: VecDeque<ScriptedFrame>,
    inner: Arc<Mutex<ScriptedWsState>>,
    connection: usize,
    /// Whether the ping just delivered was a [`ScriptedFrame::PingWithStalledPong`].
    stall_pong: bool,
}

impl MessageChannel for ScriptedChannel {
    fn next_message(&mut self) -> BoxFuture<'_, Result<Option<RawMessage>, WsError>> {
        Box::pin(async move {
            loop {
                match self.frames.pop_front() {
                    Some(ScriptedFrame::Text(text)) => return Ok(Some(RawMessage::Text(text))),
                    Some(ScriptedFrame::Ping) => return Ok(Some(RawMessage::Ping(Vec::new()))),
                    Some(ScriptedFrame::PingWithStalledPong) => {
                        self.stall_pong = true;
                        return Ok(Some(RawMessage::Ping(Vec::new())));
                    }
                    Some(ScriptedFrame::Pong) => return Ok(Some(RawMessage::Pong)),
                    Some(ScriptedFrame::Wait(delay)) => tokio::time::sleep(delay).await,
                    Some(ScriptedFrame::Error(message)) => return Err(WsError(message)),
                    // A close frame, and an exhausted script, both end the stream.
                    Some(ScriptedFrame::Close) | None => return Ok(None),
                }
            }
        })
    }

    fn pong(&mut self, _payload: Vec<u8>) -> BoxFuture<'_, Result<(), WsError>> {
        Box::pin(async move {
            if std::mem::take(&mut self.stall_pong) {
                // Never written: only the connection's write bound ends this wait.
                std::future::pending::<()>().await;
            }
            let mut state = self
                .inner
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            state.pongs[self.connection] += 1;
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use futures_util::StreamExt;

    use super::*;

    #[tokio::test]
    async fn records_requests_and_replays_responses_in_order() {
        let transport = ScriptedTransport::new(vec![
            ScriptedResponse::ok_sse("data: one\n\n"),
            ScriptedResponse::ok_sse("data: two\n\n"),
        ]);
        let request = |url: &str| HttpRequest {
            url: url.to_string(),
            headers: Vec::new(),
            body: Vec::new(),
        };

        let first = transport.post(request("https://a.test")).await.unwrap();
        assert_eq!(first.status, 200);
        let second = transport.post(request("https://b.test")).await.unwrap();
        assert_eq!(second.status, 200);

        let requests = transport.requests();
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0].url, "https://a.test");
        assert_eq!(requests[1].url, "https://b.test");
    }

    #[tokio::test]
    async fn connect_error_fails_post_instead_of_returning_a_response() {
        let transport = ScriptedTransport::new(vec![ScriptedResponse::connect_error("refused")]);
        let error = transport
            .post(HttpRequest {
                url: "https://a.test".to_string(),
                headers: Vec::new(),
                body: Vec::new(),
            })
            .await
            .unwrap_err();
        assert_eq!(error, TransportError("refused".to_string()));
    }

    #[tokio::test]
    async fn error_body_ends_the_stream_with_a_transport_error() {
        let response = ScriptedResponse {
            status: 200,
            headers: Vec::new(),
            chunks: vec![b"data: x\n\n".to_vec()],
            end: BodyEnd::Error("reset".to_string()),
        }
        .into_http_response()
        .unwrap();
        let mut body = response.body;
        assert_eq!(body.next().await.unwrap().unwrap(), b"data: x\n\n".to_vec());
        assert_eq!(body.next().await.unwrap().unwrap_err().0, "reset");
        assert!(body.next().await.is_none());
    }

    #[tokio::test(start_paused = true)]
    async fn hang_body_never_yields_after_its_chunks() {
        let response = ScriptedResponse {
            status: 200,
            headers: Vec::new(),
            chunks: Vec::new(),
            end: BodyEnd::Hang,
        }
        .into_http_response()
        .unwrap();
        let mut body = response.body;
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(10), body.next())
                .await
                .is_err(),
            "a hanging body must not produce an item"
        );
    }

    #[tokio::test]
    async fn split_sse_preserves_the_body_bytes() {
        let body = "data: hello\n\n";
        let response = ScriptedResponse::ok_sse_split(body, 5);
        let mut stream = response.into_http_response().unwrap().body;
        let mut bytes = Vec::new();
        while let Some(chunk) = stream.next().await {
            bytes.extend_from_slice(&chunk.unwrap());
        }
        assert_eq!(bytes, body.as_bytes());
    }

    #[tokio::test]
    #[should_panic(expected = "no scripted response left")]
    async fn panics_when_asked_for_more_responses_than_scripted() {
        let transport = ScriptedTransport::new(Vec::new());
        let _ = transport
            .post(HttpRequest {
                url: "https://a.test".to_string(),
                headers: Vec::new(),
                body: Vec::new(),
            })
            .await;
    }

    fn handshake(url: &str) -> WsHandshake {
        WsHandshake {
            url: url.to_string(),
            headers: vec![("Authorization".to_string(), "Bearer token".to_string())],
        }
    }

    #[tokio::test]
    async fn ws_records_the_handshake_and_replays_frames_in_order() {
        let connector = ScriptedWsConnector::new(vec![ScriptedConnection::accept(vec![
            ScriptedFrame::text("one"),
            ScriptedFrame::text("two"),
            ScriptedFrame::close(),
        ])]);

        let mut connection = connector
            .connect(handshake("wss://a.test/responses"))
            .await
            .unwrap();
        connection.send_text("out".to_string()).await.unwrap();
        assert_eq!(
            connection.next_text().await.unwrap().as_deref(),
            Some("one")
        );
        assert_eq!(
            connection.next_text().await.unwrap().as_deref(),
            Some("two")
        );
        assert_eq!(connection.next_text().await.unwrap(), None);

        let handshakes = connector.handshakes();
        assert_eq!(handshakes.len(), 1);
        assert_eq!(handshakes[0].url, "wss://a.test/responses");
        assert_eq!(
            handshakes[0].headers,
            vec![("Authorization".to_string(), "Bearer token".to_string())]
        );
        assert_eq!(connector.sent_texts(), vec![vec!["out".to_string()]]);
    }

    #[tokio::test]
    async fn ws_scripted_error_frame_fails_the_read() {
        let connector = ScriptedWsConnector::new(vec![ScriptedConnection::accept(vec![
            ScriptedFrame::error("boom"),
            ScriptedFrame::text("unreachable"),
        ])]);
        let mut connection = connector.connect(handshake("wss://a.test")).await.unwrap();
        assert_eq!(
            connection.next_text().await.unwrap_err(),
            WsError("boom".to_string())
        );
    }

    #[tokio::test]
    async fn ws_refused_upgrade_keeps_the_status_and_body() {
        let connector = ScriptedWsConnector::new(vec![ScriptedConnection::refuse(401, b"denied")]);
        let error = connector
            .connect(handshake("wss://a.test"))
            .await
            .err()
            .expect("a refused upgrade is not a connection");
        assert_eq!(
            error,
            WsConnectError::Status {
                status: 401,
                body: b"denied".to_vec(),
            }
        );
        assert!(connector.sent_texts().is_empty());
    }

    #[tokio::test]
    async fn ws_failed_connection_never_opens_a_connection() {
        let connector = ScriptedWsConnector::new(vec![ScriptedConnection::fail("dns")]);
        let error = connector
            .connect(handshake("wss://a.test"))
            .await
            .err()
            .expect("a failed connect is not a connection");
        assert_eq!(error, WsConnectError::Failed("dns".to_string()));
    }

    #[tokio::test]
    async fn ws_exhausted_script_ends_the_stream() {
        let connector =
            ScriptedWsConnector::new(vec![ScriptedConnection::accept(vec![ScriptedFrame::text(
                "only",
            )])]);
        let mut connection = connector.connect(handshake("wss://a.test")).await.unwrap();
        assert_eq!(
            connection.next_text().await.unwrap().as_deref(),
            Some("only")
        );
        assert_eq!(connection.next_text().await.unwrap(), None);
    }

    #[tokio::test]
    #[should_panic(expected = "no scripted connection left")]
    async fn ws_panics_when_asked_for_more_connections_than_scripted() {
        let connector = ScriptedWsConnector::new(Vec::new());
        let _ = connector.connect(handshake("wss://a.test")).await;
    }

    #[tokio::test]
    async fn refusing_connector_refuses_every_attempt_and_counts_them() {
        let connector = RefusingWsConnector::default();
        for _ in 0..3 {
            let error = connector
                .connect(handshake("wss://a.test"))
                .await
                .err()
                .expect("a refusing connector never opens a connection");
            assert_eq!(
                error,
                WsConnectError::Status {
                    status: 404,
                    body: Vec::new(),
                }
            );
        }
        assert_eq!(connector.handshakes(), 3);
        let unauthorized = RefusingWsConnector::new(401);
        let error = unauthorized
            .connect(handshake("wss://a.test"))
            .await
            .err()
            .expect("a refusing connector never opens a connection");
        assert_eq!(
            error,
            WsConnectError::Status {
                status: 401,
                body: Vec::new(),
            },
            "the refusal status is the caller's"
        );
    }
}
