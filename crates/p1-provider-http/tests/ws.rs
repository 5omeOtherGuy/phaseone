//! The WebSocket connector seam.
//!
//! Two arms: the scripted peer (behind the `testing` feature) proves the
//! recording and the error mapping, and the real [`TungsteniteConnector`] is
//! proved against a `tokio-tungstenite` peer bound to `127.0.0.1:0` inside the
//! test — plain `ws://`, no TLS, no external network. Nothing here sleeps: every
//! step is ordered by the request/response itself or by a channel.

use std::net::SocketAddr;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use p1_provider_http::ws::{
    TungsteniteConnector, WsConnectError, WsConnection, WsConnector, WsError, WsHandshake,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, oneshot};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::handshake::server::{Request, Response};

/// A bound guard: no loopback connect may hang the suite.
const LOOPBACK_TIMEOUT: Duration = Duration::from_secs(10);

/// A header value no error text or `Debug` output may ever carry.
const SENTINEL: &str = "SENTINEL-HEADER-VALUE";

/// Bind a listener on `127.0.0.1:0` and hand back its address.
async fn listener() -> (TcpListener, SocketAddr) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    (listener, addr)
}

/// Open one real connection to `url`, bounded.
async fn connect(url: &str, headers: Vec<(String, String)>) -> Box<dyn WsConnection> {
    let connector = TungsteniteConnector::new();
    let handshake = WsHandshake {
        url: url.to_string(),
        headers,
    };
    tokio::time::timeout(LOOPBACK_TIMEOUT, connector.connect(handshake))
        .await
        .expect("the loopback connect did not finish")
        .unwrap_or_else(|error| panic!("the loopback connect failed: {error}"))
}

/// Read a request head (up to the blank line) so a peer sees a well-formed call.
async fn read_request(tcp: &mut TcpStream) -> Vec<u8> {
    let mut bytes = Vec::new();
    let mut buffer = [0u8; 1024];
    loop {
        let read = tcp.read(&mut buffer).await.expect("read the upgrade");
        if read == 0 {
            return bytes;
        }
        bytes.extend_from_slice(&buffer[..read]);
        if bytes.windows(4).any(|window| window == b"\r\n\r\n") {
            return bytes;
        }
    }
}

/// Spawn a peer that answers the upgrade with `status_line` and `body`, and
/// never speaks WebSocket. Returns its address.
async fn spawn_refusing_peer(status_line: &'static str, body: &'static str) -> SocketAddr {
    let (listener, addr) = listener().await;
    tokio::spawn(async move {
        let (mut tcp, _) = listener.accept().await.unwrap();
        let _request = read_request(&mut tcp).await;
        let response = format!(
            "HTTP/1.1 {status_line}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        tcp.write_all(response.as_bytes()).await.unwrap();
        let _ = tcp.shutdown().await;
    });
    addr
}

/// The handshake the real connector is exercised with: two headers, both of
/// which must arrive at the peer unchanged.
fn handshake_headers() -> Vec<(String, String)> {
    vec![
        ("Authorization".to_string(), "Bearer test-token".to_string()),
        (
            "OpenAI-Beta".to_string(),
            "responses_websockets=2026-02-06".to_string(),
        ),
    ]
}

/// The same two headers, both carrying the sentinel value.
fn sentinel_headers() -> Vec<(String, String)> {
    vec![
        ("Authorization".to_string(), format!("Bearer {SENTINEL}")),
        ("chatgpt-account-id".to_string(), SENTINEL.to_string()),
    ]
}

#[tokio::test]
// The handshake callback's `Result` shape comes from tokio-tungstenite.
#[allow(clippy::result_large_err)]
async fn headers_arrive_a_text_round_trip_works_a_ping_is_answered_a_binary_frame_is_decoded_and_a_close_ends_the_stream()
 {
    let (listener, addr) = listener().await;
    let (headers_tx, headers_rx) = oneshot::channel::<Vec<(String, String)>>();
    let (seen_tx, mut seen_rx) = mpsc::unbounded_channel::<String>();

    let peer = tokio::spawn(async move {
        let (tcp, _) = listener.accept().await.unwrap();
        let mut ws = tokio_tungstenite::accept_hdr_async(
            tcp,
            move |request: &Request, response: Response| {
                let headers = request
                    .headers()
                    .iter()
                    .map(|(name, value)| {
                        (
                            name.as_str().to_string(),
                            value.to_str().unwrap_or_default().to_string(),
                        )
                    })
                    .collect();
                let _ = headers_tx.send(headers);
                Ok(response)
            },
        )
        .await
        .unwrap();

        // The client's only send, then everything the client must render.
        match ws.next().await.unwrap().unwrap() {
            Message::Text(text) => seen_tx.send(format!("text:{text}")).unwrap(),
            other => panic!("expected the client's text frame, got {other:?}"),
        }
        ws.send(Message::text("first")).await.unwrap();
        ws.send(Message::Ping("ping-payload".into())).await.unwrap();
        // Bounded so a ping the client does not answer fails the test instead of
        // hanging it.
        let pong = tokio::time::timeout(LOOPBACK_TIMEOUT, ws.next())
            .await
            .expect("the client never answered the ping");
        match pong.unwrap().unwrap() {
            Message::Pong(payload) => {
                seen_tx
                    .send(format!("pong:{}", String::from_utf8_lossy(&payload)))
                    .unwrap();
            }
            other => panic!("expected a pong, got {other:?}"),
        }
        ws.send(Message::text("after-ping")).await.unwrap();
        // A pong the peer owes nobody is ignored, not handed to the caller.
        ws.send(Message::Pong("stray".into())).await.unwrap();
        ws.send(Message::text("after-pong")).await.unwrap();
        // Valid UTF-8 in a binary frame is text to this seam.
        ws.send(Message::binary("binär".as_bytes().to_vec()))
            .await
            .unwrap();
        ws.send(Message::binary(vec![0xff, 0xfe])).await.unwrap();
        ws.send(Message::Close(None)).await.unwrap();
    });

    let mut connection =
        connect(&format!("ws://{addr}/codex/responses"), handshake_headers()).await;
    connection.send_text("hello".to_string()).await.unwrap();
    assert_eq!(
        connection.next_text().await.unwrap().as_deref(),
        Some("first")
    );
    // The ping sits between "first" and "after-ping": it is answered inside this
    // call, which is why the peer may send "after-ping" only after the pong.
    assert_eq!(
        connection.next_text().await.unwrap().as_deref(),
        Some("after-ping")
    );
    assert_eq!(
        connection.next_text().await.unwrap().as_deref(),
        Some("after-pong")
    );
    assert_eq!(
        connection.next_text().await.unwrap().as_deref(),
        Some("binär")
    );
    let error = connection
        .next_text()
        .await
        .expect_err("a binary frame that is not UTF-8 is a WsError");
    assert!(matches!(error, WsError(_)), "{error:?}");
    // A failed decode does not poison the connection, and the close frame ends it.
    assert_eq!(connection.next_text().await.unwrap(), None);

    peer.await.unwrap();

    let headers = headers_rx.await.unwrap();
    let value = |name: &str| {
        headers
            .iter()
            .find(|(header, _)| header.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    };
    assert_eq!(value("authorization"), Some("Bearer test-token"));
    assert_eq!(
        value("openai-beta"),
        Some("responses_websockets=2026-02-06")
    );

    let mut seen = Vec::new();
    while let Ok(item) = seen_rx.try_recv() {
        seen.push(item);
    }
    assert_eq!(seen, vec!["text:hello", "pong:ping-payload"]);
}

#[tokio::test]
async fn a_peer_that_vanishes_without_a_close_ends_the_stream() {
    let (listener, addr) = listener().await;
    let peer = tokio::spawn(async move {
        let (tcp, _) = listener.accept().await.unwrap();
        let mut ws = tokio_tungstenite::accept_async(tcp).await.unwrap();
        ws.send(Message::text("last")).await.unwrap();
        // No close frame: the socket just goes away.
    });

    let mut connection = connect(&format!("ws://{addr}/codex/responses"), Vec::new()).await;
    assert_eq!(
        connection.next_text().await.unwrap().as_deref(),
        Some("last")
    );
    assert_eq!(connection.next_text().await.unwrap(), None);

    peer.await.unwrap();
}

#[tokio::test]
async fn a_refused_upgrade_becomes_a_status_error() {
    let addr = spawn_refusing_peer("401 Unauthorized", "denied").await;
    let connector = TungsteniteConnector::new();
    let error = connector
        .connect(WsHandshake {
            url: format!("ws://{addr}/codex/responses"),
            headers: handshake_headers(),
        })
        .await
        .err()
        .expect("a refused upgrade is not a connection");
    match error {
        WsConnectError::Status { status, .. } => assert_eq!(status, 401),
        other => panic!("expected the refused status, got {other:?}"),
    }
}

/// Format every error the real connector can produce with `{}` and `{:?}` after
/// a handshake whose header values carry the sentinel, and assert it never
/// appears: a handshake carries the credential.
#[tokio::test]
async fn no_error_text_or_debug_output_contains_a_header_value() {
    let mut outputs = vec![
        format!(
            "{:?}",
            WsHandshake {
                url: "wss://host.test/backend-api/codex/responses".to_string(),
                headers: sentinel_headers(),
            }
        ),
        format!(
            "{:#?}",
            WsHandshake {
                url: "wss://host.test/backend-api/codex/responses".to_string(),
                headers: sentinel_headers(),
            }
        ),
    ];

    // `WsConnectError::Failed`: a request the connector refuses before any I/O.
    let connector = TungsteniteConnector::new();
    let mut malformed = sentinel_headers();
    malformed.push(("x-bad".to_string(), format!("{SENTINEL}\n")));
    let error = connector
        .connect(WsHandshake {
            url: "wss://host.test/backend-api/codex/responses".to_string(),
            headers: malformed,
        })
        .await
        .err()
        .expect("a malformed header value is not a connection");
    outputs.push(format!("{error}"));
    outputs.push(format!("{error:?}"));

    // `WsConnectError::Status`: an upgrade a loopback peer refuses.
    let addr = spawn_refusing_peer("403 Forbidden", "denied").await;
    let error = connector
        .connect(WsHandshake {
            url: format!("ws://{addr}/codex/responses"),
            headers: sentinel_headers(),
        })
        .await
        .err()
        .expect("a refused upgrade is not a connection");
    outputs.push(format!("{error}"));
    outputs.push(format!("{error:?}"));

    // `WsError`: a frame the loopback peer sends that this seam cannot render.
    let (listener, addr) = listener().await;
    let peer = tokio::spawn(async move {
        let (tcp, _) = listener.accept().await.unwrap();
        let mut ws = tokio_tungstenite::accept_async(tcp).await.unwrap();
        ws.send(Message::binary(vec![0xff, 0xfe])).await.unwrap();
        let _ = ws.close(None).await;
    });
    let mut connection = connect(&format!("ws://{addr}/codex/responses"), sentinel_headers()).await;
    let error = connection
        .next_text()
        .await
        .expect_err("an invalid UTF-8 binary frame is a WsError");
    outputs.push(format!("{error}"));
    outputs.push(format!("{error:?}"));
    peer.await.unwrap();

    for output in &outputs {
        assert!(!output.contains(SENTINEL), "{output}");
    }
    // The failure classes themselves are still diagnostic.
    assert!(outputs.iter().any(|output| output.contains("403")));
    assert!(outputs.iter().any(|output| output.contains("UTF-8")));
}

#[cfg(feature = "testing")]
mod scripted {
    use p1_provider_http::testing::{ScriptedConnection, ScriptedFrame, ScriptedWsConnector};
    use p1_provider_http::ws::{WsConnectError, WsConnector, WsHandshake};

    use super::SENTINEL;

    fn handshake() -> WsHandshake {
        WsHandshake {
            url: "wss://host.test/backend-api/codex/responses".to_string(),
            headers: vec![
                ("Authorization".to_string(), "Bearer token".to_string()),
                (
                    "OpenAI-Beta".to_string(),
                    "responses_websockets=2026-02-06".to_string(),
                ),
            ],
        }
    }

    #[tokio::test]
    async fn records_the_handshake_delivers_frames_then_ends_on_close() {
        let connector = ScriptedWsConnector::new(vec![
            ScriptedConnection::accept(vec![
                ScriptedFrame::text("{\"type\":\"response.created\"}"),
                ScriptedFrame::text("{\"type\":\"response.completed\"}"),
                ScriptedFrame::close(),
            ]),
            // A second connection, to prove the queue is consumed in order.
            ScriptedConnection::refuse(429, "slow down"),
        ]);

        let mut connection = connector.connect(handshake()).await.unwrap();
        connection.send_text("first".to_string()).await.unwrap();
        assert_eq!(
            connection.next_text().await.unwrap().as_deref(),
            Some("{\"type\":\"response.created\"}")
        );
        connection.send_text("second".to_string()).await.unwrap();
        assert_eq!(
            connection.next_text().await.unwrap().as_deref(),
            Some("{\"type\":\"response.completed\"}")
        );
        assert_eq!(connection.next_text().await.unwrap(), None);

        let error = connector
            .connect(handshake())
            .await
            .err()
            .expect("a refused upgrade is not a connection");
        assert_eq!(
            error,
            WsConnectError::Status {
                status: 429,
                body: b"slow down".to_vec(),
            }
        );

        let handshakes = connector.handshakes();
        assert_eq!(handshakes.len(), 2);
        for recorded in &handshakes {
            assert_eq!(recorded.url, "wss://host.test/backend-api/codex/responses");
            assert_eq!(
                recorded.headers,
                vec![
                    ("Authorization".to_string(), "Bearer token".to_string()),
                    (
                        "OpenAI-Beta".to_string(),
                        "responses_websockets=2026-02-06".to_string()
                    ),
                ]
            );
        }
        // Texts are recorded per connection; a refused connect sent nothing.
        assert_eq!(
            connector.sent_texts(),
            vec![vec!["first".to_string(), "second".to_string()]]
        );
    }

    #[tokio::test]
    async fn a_scripted_error_frame_is_a_ws_error() {
        let connector = ScriptedWsConnector::new(vec![ScriptedConnection::accept(vec![
            ScriptedFrame::error("socket reset"),
        ])]);
        let mut connection = connector.connect(handshake()).await.unwrap();
        assert_eq!(
            connection.next_text().await.unwrap_err().0,
            "socket reset".to_string()
        );
    }

    /// The same property as the real-connector test, for the scripted peer: its
    /// three error shapes carry no header value either.
    #[tokio::test]
    async fn scripted_errors_never_contain_a_header_value() {
        let sentinel_handshake = || WsHandshake {
            url: "wss://host.test/backend-api/codex/responses".to_string(),
            headers: vec![
                ("Authorization".to_string(), format!("Bearer {SENTINEL}")),
                ("chatgpt-account-id".to_string(), SENTINEL.to_string()),
            ],
        };

        let mut outputs = vec![
            format!("{:?}", sentinel_handshake()),
            format!("{:#?}", sentinel_handshake()),
        ];

        let connector = ScriptedWsConnector::new(vec![
            ScriptedConnection::refuse(401, SENTINEL),
            ScriptedConnection::fail("dns"),
        ]);
        for _ in 0..2 {
            let error = connector
                .connect(sentinel_handshake())
                .await
                .err()
                .expect("a refused or failed connect is not a connection");
            outputs.push(format!("{error}"));
            outputs.push(format!("{error:?}"));
        }

        let connector = ScriptedWsConnector::new(vec![ScriptedConnection::accept(vec![
            ScriptedFrame::error("read"),
        ])]);
        let mut connection = connector.connect(sentinel_handshake()).await.unwrap();
        let error = connection.next_text().await.unwrap_err();
        outputs.push(format!("{error}"));
        outputs.push(format!("{error:?}"));

        for output in &outputs {
            assert!(!output.contains(SENTINEL), "{output}");
        }
        assert!(outputs.iter().any(|output| output.contains("401")));
    }
}
