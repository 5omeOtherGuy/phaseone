//! The route-bound WebSocket across the host/component boundary (S5.5, ADR-0078 §1-§2,
//! `docs/design/modules/wit.md` decision S0-R2.2).
//!
//! The host session (`p1_provider_http::ws_session`) owns the connection: the handshake,
//! the write bound, the ping/pong answer and the read bounds measured on every raw frame
//! (ADR-0069). The provider's portable decisions (`p1_provider_openai::websocket_lower`)
//! choose from the session's `connection-state` facts: a handshake head exactly when no
//! connection is open, the continuation frame only on the open connection whose last
//! clean response it continues, and HTTP once a request failed before any output with no
//! WebSocket attempt left (ADR-0047's pre-output rule).
//!
//! Fake time (`start_paused`) and `ScriptedWsConnector` only: no socket, no network, no
//! sleep. The session's reuse clock is the paused Tokio clock itself.

use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use p1_contracts::CancellationToken;
use p1_contracts::serde_json::{Value, json};
use p1_provider_http::testing::{ScriptedConnection, ScriptedFrame, ScriptedWsConnector};
use p1_provider_http::ws::{WRITE_TIMEOUT, WsBound, WsNext};
use p1_provider_http::ws_session::{
    self, Clock, WsAuthority, WsHead, WsLease, WsRead, WsSend, WsSendError, WsSession,
};
use p1_provider_http::{CredentialScheme, CredentialUse, FIRST_BYTE_TIMEOUT, STREAM_IDLE_TIMEOUT};
use p1_provider_openai::websocket_lower::{
    ConnectionState, Lowered, ResponseFacts, WebSocketDecisions, WebSocketHead, WebSocketSend,
};

const ENDPOINT: &str = "https://example.test/backend-api/codex/responses";

/// Longer than any bound here, so a hang fails the case instead of CI.
const GUARD: Duration = Duration::from_secs(3 * 60 * 60);

async fn guarded<T>(future: impl Future<Output = T>) -> T {
    tokio::time::timeout(GUARD, future)
        .await
        .expect("the session must answer within its bounds")
}

/// The session's clock is the paused Tokio clock, so fake time drives reuse too.
fn clock() -> Clock {
    Arc::new(|| tokio::time::Instant::now().into_std())
}

fn session(connector: &ScriptedWsConnector) -> WsSession {
    WsSession::new(Arc::new(connector.clone()), clock())
}

fn lease(session: &WsSession) -> WsLease {
    session.try_lease().expect("the session is free")
}

/// A proxy-injected route: the session attaches no credential, so the cases need none.
fn authority() -> WsAuthority<'static> {
    WsAuthority {
        endpoint: ENDPOINT,
        credential: None,
    }
}

/// The frame encoder the decisions are built with: the body itself, so a case can read
/// back which body the decisions chose.
fn frame_of(body: &Value) -> String {
    body.to_string()
}

fn component_head() -> WebSocketHead {
    WebSocketHead {
        path: String::new(),
        headers: vec![("originator".to_string(), "p1".to_string())],
        account_id_header: None,
    }
}

/// The WIT binding between the two sides: the component's facts in, its send out.
fn component_state(state: ws_session::ConnectionState) -> ConnectionState {
    ConnectionState {
        open: state.open,
        last_clean_response: state.last_clean_response,
        failed_before_output: state.failed_before_output,
    }
}

fn host_send(send: WebSocketSend) -> WsSend {
    WsSend {
        handshake: send.handshake.map(|head| WsHead {
            path: head.path,
            headers: head.headers,
            credential: CredentialUse {
                scheme: CredentialScheme::Bearer,
                account_id_header: head.account_id_header,
            },
        }),
        frame: send.frame,
    }
}

/// One `lower` of the component against the lease's current facts.
fn lower(decisions: &mut WebSocketDecisions, lease: &mut WsLease, body: &Value) -> Lowered {
    let state = component_state(lease.state());
    decisions.lower(body, component_head(), &state)
}

fn websocket(lowered: Lowered) -> WebSocketSend {
    match lowered {
        Lowered::WebSocket(send) => send,
        Lowered::Http => panic!("expected a WebSocket send, got the HTTP fallback"),
    }
}

/// A full send with a head, as the component lowers it on a closed session.
fn opening_send() -> WsSend {
    WsSend {
        handshake: Some(WsHead {
            path: String::new(),
            headers: Vec::new(),
            credential: CredentialUse {
                scheme: CredentialScheme::Bearer,
                account_id_header: None,
            },
        }),
        frame: r#"{"type":"response.create"}"#.to_string(),
    }
}

fn text(read: WsRead) -> String {
    match read {
        WsRead::Next(WsNext::Text(text)) => text,
        other => panic!("expected a text frame, got {other:?}"),
    }
}

fn user(text: &str) -> Value {
    json!({ "type": "message", "role": "user",
            "content": [{ "type": "input_text", "text": text }] })
}

fn assistant(text: &str) -> Value {
    json!({ "type": "message", "role": "assistant",
            "content": [{ "type": "output_text", "text": text }] })
}

/// A completed response `id` whose one output item is the assistant message `answer`.
fn completed_response(id: &str, answer: &str) -> Vec<String> {
    vec![
        json!({ "type": "response.created", "response": { "id": id } }).to_string(),
        json!({ "type": "response.output_item.done", "item": {
            "type": "message", "role": "assistant",
            "content": [{ "type": "output_text", "text": answer }] } })
        .to_string(),
        json!({ "type": "response.completed", "response": { "id": id } }).to_string(),
    ]
}

// ------------------------------------------------------------------ the read bounds

/// ADR-0069: no frame within `FIRST_BYTE_TIMEOUT` after a send is the first-frame bound,
/// before any output, and the connection is gone afterwards.
#[tokio::test(start_paused = true)]
async fn no_first_frame_fails_at_the_first_byte_bound_before_any_output() {
    let connector =
        ScriptedWsConnector::new(vec![ScriptedConnection::accept(vec![ScriptedFrame::wait(
            Duration::from_secs(3600),
        )])]);
    let session = session(&connector);
    let mut lease = lease(&session);
    let cancel = CancellationToken::new();
    assert!(!lease.state().open);
    guarded(lease.send(authority(), opening_send(), &cancel))
        .await
        .expect("the send goes out");

    let start = tokio::time::Instant::now();
    let read = guarded(lease.next(&cancel)).await;
    assert_eq!(read, WsRead::Next(WsNext::Timeout(WsBound::FirstFrame)));
    assert_eq!(start.elapsed(), FIRST_BYTE_TIMEOUT);
    assert!(!lease.state().open, "an expired connection is dropped");
}

/// ADR-0069: frames, then silence for `STREAM_IDLE_TIMEOUT`, is the idle bound — after
/// output, so the caller ends the response as a failure naming the bound.
#[tokio::test(start_paused = true)]
async fn silence_after_frames_fails_at_the_idle_bound_after_output() {
    let connector = ScriptedWsConnector::new(vec![ScriptedConnection::accept(vec![
        ScriptedFrame::text("one"),
        ScriptedFrame::wait(Duration::from_secs(100)),
        ScriptedFrame::text("two"),
        ScriptedFrame::wait(Duration::from_secs(3600)),
    ])]);
    let session = session(&connector);
    let mut lease = lease(&session);
    let cancel = CancellationToken::new();
    guarded(lease.send(authority(), opening_send(), &cancel))
        .await
        .unwrap();
    assert_eq!(text(guarded(lease.next(&cancel)).await), "one");
    assert_eq!(text(guarded(lease.next(&cancel)).await), "two");

    let start = tokio::time::Instant::now();
    let read = guarded(lease.next(&cancel)).await;
    assert_eq!(read, WsRead::Next(WsNext::Timeout(WsBound::Idle)));
    assert_eq!(start.elapsed(), STREAM_IDLE_TIMEOUT);
    assert!(
        WsBound::Idle.message().contains("idle"),
        "the bound is named"
    );
    assert!(!lease.state().open);
}

/// A control ping is activity: it resets the idle clock, so ten minutes of pings each
/// under the idle bound keep the response alive, and every ping is answered with a pong
/// the session wrote itself.
#[tokio::test(start_paused = true)]
async fn a_ping_resets_the_idle_clock_and_is_answered_with_a_pong() {
    let mut frames = vec![ScriptedFrame::text("created")];
    for _ in 0..3 {
        frames.push(ScriptedFrame::wait(Duration::from_secs(200)));
        frames.push(ScriptedFrame::ping());
    }
    frames.push(ScriptedFrame::wait(Duration::from_secs(200)));
    frames.push(ScriptedFrame::text("done"));
    let connector = ScriptedWsConnector::new(vec![ScriptedConnection::accept(frames)]);
    let session = session(&connector);
    let mut lease = lease(&session);
    let cancel = CancellationToken::new();
    guarded(lease.send(authority(), opening_send(), &cancel))
        .await
        .unwrap();
    assert_eq!(text(guarded(lease.next(&cancel)).await), "created");

    let start = tokio::time::Instant::now();
    assert_eq!(text(guarded(lease.next(&cancel)).await), "done");
    assert_eq!(start.elapsed(), Duration::from_secs(800));
    assert!(start.elapsed() > STREAM_IDLE_TIMEOUT);
    assert_eq!(connector.pongs(), vec![3]);
}

/// The pong is written under the write bound: a peer that stopped reading cannot block
/// the read loop, and the stalled write is a read failure that drops the connection.
#[tokio::test(start_paused = true)]
async fn a_pong_is_written_under_the_write_bound() {
    let connector = ScriptedWsConnector::new(vec![ScriptedConnection::accept(vec![
        ScriptedFrame::text("created"),
        ScriptedFrame::ping_with_stalled_pong(),
    ])]);
    let session = session(&connector);
    let mut lease = lease(&session);
    let cancel = CancellationToken::new();
    guarded(lease.send(authority(), opening_send(), &cancel))
        .await
        .unwrap();
    assert_eq!(text(guarded(lease.next(&cancel)).await), "created");

    let start = tokio::time::Instant::now();
    match guarded(lease.next(&cancel)).await {
        WsRead::Failed(error) => assert!(error.0.contains("pong"), "{error:?}"),
        other => panic!("expected the stalled pong to fail the read, got {other:?}"),
    }
    assert_eq!(start.elapsed(), WRITE_TIMEOUT);
    assert_eq!(connector.pongs(), vec![0]);
    assert!(!lease.state().open);
}

// ---------------------------------------------------------- connection lifetime

/// After a failed response the session reports `open = false`, the component's next
/// lowering carries a handshake, and the send opens a NEW connection.
#[tokio::test(start_paused = true)]
async fn after_a_failed_response_the_next_send_carries_a_handshake() {
    let connector = ScriptedWsConnector::new(vec![
        ScriptedConnection::accept(vec![
            ScriptedFrame::text("created"),
            ScriptedFrame::error("reset"),
        ]),
        ScriptedConnection::accept(Vec::new()),
    ]);
    let session = session(&connector);
    let mut decisions = WebSocketDecisions::new(frame_of);
    let body = json!({ "model": "m", "input": [user("hi")] });
    let cancel = CancellationToken::new();

    let mut first = lease(&session);
    let send = websocket(lower(&mut decisions, &mut first, &body));
    assert!(send.handshake.is_some(), "no connection is open yet");
    guarded(first.send(authority(), host_send(send), &cancel))
        .await
        .unwrap();
    assert_eq!(text(guarded(first.next(&cancel)).await), "created");
    assert!(matches!(
        guarded(first.next(&cancel)).await,
        WsRead::Failed(_)
    ));
    drop(first);

    let mut second = lease(&session);
    let state = second.state();
    assert!(
        !state.open,
        "a failed response never returns its connection"
    );
    assert_eq!(state.last_clean_response, None);
    let send = websocket(lower(&mut decisions, &mut second, &body));
    assert!(send.handshake.is_some(), "the retry reconnects");
    guarded(second.send(authority(), host_send(send), &cancel))
        .await
        .unwrap();
    assert_eq!(connector.handshakes().len(), 2);
    assert_eq!(connector.sent_texts().len(), 2, "two connections");
}

/// A cancelled response drops the connection, and the next send carries a handshake.
#[tokio::test(start_paused = true)]
async fn after_a_cancelled_response_the_next_send_carries_a_handshake() {
    let connector = ScriptedWsConnector::new(vec![
        ScriptedConnection::accept(vec![ScriptedFrame::wait(Duration::from_secs(3600))]),
        ScriptedConnection::accept(Vec::new()),
    ]);
    let session = session(&connector);
    let mut decisions = WebSocketDecisions::new(frame_of);
    let body = json!({ "model": "m", "input": [user("hi")] });

    let mut first = lease(&session);
    let cancel = CancellationToken::new();
    let send = websocket(lower(&mut decisions, &mut first, &body));
    guarded(first.send(authority(), host_send(send), &cancel))
        .await
        .unwrap();
    cancel.cancel();
    assert_eq!(guarded(first.next(&cancel)).await, WsRead::Cancelled);
    assert!(!first.state().open);
    drop(first);

    let mut second = lease(&session);
    assert!(!second.state().open);
    let send = websocket(lower(&mut decisions, &mut second, &body));
    assert!(send.handshake.is_some());
    guarded(second.send(authority(), host_send(send), &CancellationToken::new()))
        .await
        .unwrap();
    assert_eq!(connector.handshakes().len(), 2);
}

/// A send without a head relies on the open connection; when it is gone before the
/// frame went out, that is a failure before any output, and nothing is connected.
#[tokio::test(start_paused = true)]
async fn a_send_without_a_head_on_a_gone_connection_fails_before_output() {
    let connector = ScriptedWsConnector::new(Vec::new());
    let session = session(&connector);
    let mut lease = lease(&session);
    let send = WsSend {
        handshake: None,
        frame: "{}".to_string(),
    };
    let sent = guarded(lease.send(authority(), send, &CancellationToken::new())).await;
    assert_eq!(sent, Err(WsSendError::WriteFailed));
    assert!(connector.handshakes().is_empty());
}

/// After a clean completion the session reports the connection open with that response
/// id, and the component's continuation frame goes out on the SAME connection.
#[tokio::test(start_paused = true)]
async fn a_clean_completion_is_reused_and_continued_on_the_same_connection() {
    let mut frames: Vec<ScriptedFrame> = completed_response("resp_1", "hello")
        .into_iter()
        .map(ScriptedFrame::text)
        .collect();
    frames.push(ScriptedFrame::text("second"));
    let connector = ScriptedWsConnector::new(vec![ScriptedConnection::accept(frames)]);
    let session = session(&connector);
    let mut decisions = WebSocketDecisions::new(frame_of);
    let cancel = CancellationToken::new();
    let first_body = json!({ "model": "m", "input": [user("hi")] });

    let mut first = lease(&session);
    let send = websocket(lower(&mut decisions, &mut first, &first_body));
    guarded(first.send(authority(), host_send(send), &cancel))
        .await
        .unwrap();
    let mut facts = ResponseFacts::default();
    for _ in 0..3 {
        facts.record(&text(guarded(first.next(&cancel)).await));
    }
    assert_eq!(facts.response_id(), Some("resp_1"));
    first.completed(facts.response_id().map(str::to_string));
    decisions.completed(&first_body, &facts);
    drop(first);

    let mut second = lease(&session);
    let state = second.state();
    assert!(state.open);
    assert_eq!(state.last_clean_response.as_deref(), Some("resp_1"));
    let second_body = json!({
        "model": "m",
        "input": [user("hi"), assistant("hello"), user("more")],
    });
    let send = websocket(lower(&mut decisions, &mut second, &second_body));
    assert!(
        send.handshake.is_none(),
        "an open connection needs no handshake"
    );
    let frame: Value = p1_contracts::serde_json::from_str(&send.frame).unwrap();
    assert_eq!(frame["previous_response_id"], "resp_1");
    assert_eq!(frame["input"], json!([user("more")]), "only the new items");
    guarded(second.send(authority(), host_send(send), &cancel))
        .await
        .unwrap();
    assert_eq!(text(guarded(second.next(&cancel)).await), "second");

    assert_eq!(connector.handshakes().len(), 1, "one connection");
    let sent = connector.sent_texts();
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].len(), 2, "both frames on the same connection");
}

/// The continuation is decided on the facts: a connection whose last clean response is
/// NOT the one the component remembers gets the full frame.
#[test]
fn a_different_last_clean_response_gets_the_full_frame() {
    let mut decisions = WebSocketDecisions::new(frame_of);
    let first_body = json!({ "model": "m", "input": [user("hi")] });
    let mut facts = ResponseFacts::default();
    for frame in completed_response("resp_1", "hello") {
        facts.record(&frame);
    }
    decisions.completed(&first_body, &facts);
    let body = json!({ "model": "m", "input": [user("hi"), assistant("hello"), user("more")] });
    let state = ConnectionState {
        open: true,
        last_clean_response: Some("resp_other".to_string()),
        failed_before_output: false,
    };
    let send = websocket(decisions.lower(&body, component_head(), &state));
    assert!(send.handshake.is_none());
    assert_eq!(send.frame, frame_of(&body), "the full frame");
}

// ------------------------------------------------------------------- the fallback

/// ADR-0047's pre-output rule: `failed-before-output` leads to HTTP, and the instance
/// never makes a second WebSocket attempt — not for this request, not for the next.
#[tokio::test(start_paused = true)]
async fn failed_before_output_falls_back_to_http_for_the_instance() {
    let connector = ScriptedWsConnector::new(vec![ScriptedConnection::refuse(404, Vec::new())]);
    let session = session(&connector);
    let mut decisions = WebSocketDecisions::new(frame_of);
    let body = json!({ "model": "m", "input": [user("hi")] });
    let cancel = CancellationToken::new();

    let mut first = lease(&session);
    let send = websocket(lower(&mut decisions, &mut first, &body));
    let refused = guarded(first.send(authority(), host_send(send), &cancel)).await;
    assert!(matches!(
        refused,
        Err(WsSendError::Refused { status: 404, .. })
    ));
    // §5: the endpoint said no, so the native side allows no further attempt.
    first.fail_before_output();
    assert!(first.state().failed_before_output);
    assert_eq!(
        lower(&mut decisions, &mut first, &body),
        Lowered::Http,
        "the retry falls back"
    );
    assert!(decisions.is_turned_off());
    drop(first);

    let mut second = lease(&session);
    assert!(
        !second.state().failed_before_output,
        "the fact is per request"
    );
    assert_eq!(
        lower(&mut decisions, &mut second, &body),
        Lowered::Http,
        "WebSocket stays off for the instance"
    );
    assert_eq!(
        connector.handshakes().len(),
        1,
        "no second WebSocket attempt"
    );
}
