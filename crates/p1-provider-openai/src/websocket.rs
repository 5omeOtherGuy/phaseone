//! The WebSocket transport of the Responses adapter (ADR-0047,
//! `docs/design/websocket.md` §3–§5).
//!
//! One text frame out, one JSON event per text frame in, fed to the SAME
//! [`CodexResponseParser`] the SSE path uses — no second parser. This module owns
//! the connection's lifetime: one connection per provider instance behind an async
//! mutex, reused while it is young and recently used, dropped the moment a request
//! is cancelled or a response fails, and returned to its slot only after a clean
//! completion.
//!
//! §5 is `drive()`'s failure policy re-expressed for a handshake and for error
//! frames: one forced credential refresh on a refused upgrade, no fallback for a
//! rate limit, one reconnect per request, and a fallback to today's SSE path —
//! which also turns WebSocket off for this provider instance. After model-visible
//! output every failure is an ordinary `Transport` failure of that response. Every
//! row of that table has a named test in `tests/websocket.rs`.
//!
//! §6 (continuation) is the NEXT stage: this one always sends the FULL body.
//! [`request_frame`] is the single place that decision lands.

use std::collections::VecDeque;
use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use futures_util::StreamExt;
use futures_util::future::{Either, select};
use futures_util::stream::unfold;
use p1_contracts::{
    CancellationToken, Outcome, ProviderError, ProviderErrorKind, ProviderStream, StreamEvent,
};
use p1_provider_http::ws::{WsConnectError, WsConnection, WsConnector, WsHandshake};
use p1_provider_http::{Credential, CredentialSource, ResponseParser, SseEvent};
use serde_json::Value;
use tokio::sync::{Mutex, OwnedMutexGuard};

use crate::ResponsesAccount;
use crate::parser::CodexResponseParser;
use crate::request::{build_ws_headers, ws_frame};

/// The clock the connection-reuse policy reads (§4). Injected, so a test advances
/// time instead of sleeping (AGENTS.md forbids sleep-based timing assertions).
pub type Clock = Arc<dyn Fn() -> Instant + Send + Sync>;

/// One connect and one send are bounded by this (§4).
const BOUND: Duration = Duration::from_secs(10);
/// §4: a connection is reused while it is younger than this …
const MAX_AGE: Duration = Duration::from_secs(55 * 60);
/// … and was last used less than this ago.
const MAX_IDLE: Duration = Duration::from_secs(5 * 60);

/// The WebSocket half of one provider instance: one connector, one connection
/// slot, and the switch a fallback turns off for good (§5).
pub(crate) struct WebSocket {
    connector: Arc<dyn WsConnector>,
    clock: Clock,
    slot: Arc<Mutex<Slot>>,
    /// Set when a request fell back to SSE: this instance speaks SSE from then on.
    disabled: AtomicBool,
}

/// The ONE connection slot. A request takes the connection out of it while it uses
/// it, so a half-read socket can never be left behind: it is dropped together with
/// the request that owned it.
pub(crate) struct Slot {
    connection: Option<Live>,
}

/// One open connection and what the reuse policy knows about it.
struct Live {
    connection: Box<dyn WsConnection>,
    connected_at: Instant,
    last_used_at: Instant,
    /// Whether this connection was already open when this request took it. §5
    /// reconnects once for a socket that goes away before its first frame, which
    /// only a REUSED socket can do: a fresh one that never answered is an ordinary
    /// connect failure.
    reused: bool,
}

impl WebSocket {
    pub(crate) fn new(connector: Arc<dyn WsConnector>, clock: Clock) -> Self {
        Self {
            connector,
            clock,
            slot: Arc::new(Mutex::new(Slot { connection: None })),
            disabled: AtomicBool::new(false),
        }
    }

    /// Whether a fallback has already turned WebSocket off for this instance.
    pub(crate) fn is_disabled(&self) -> bool {
        self.disabled.load(Ordering::SeqCst)
    }

    /// Take the slot WITHOUT waiting. `None` means another request holds it, and
    /// §4 is explicit that such a request uses SSE: never a second socket, never a
    /// wait.
    pub(crate) fn try_take(self: &Arc<Self>) -> Option<OwnedMutexGuard<Slot>> {
        Arc::clone(&self.slot).try_lock_owned().ok()
    }
}

/// Everything one WebSocket request needs. The connection slot is NOT here: it is
/// taken before the request starts and owned by the stream this module returns.
pub(crate) struct WebSocketRequest {
    pub(crate) ws: Arc<WebSocket>,
    /// The resolved HTTPS endpoint; the handshake swaps its scheme.
    pub(crate) url: String,
    /// The body the SSE path would send. §6 will hand a continuation body in here.
    pub(crate) body: Value,
    pub(crate) account: ResponsesAccount,
    pub(crate) cache_key: Option<String>,
    pub(crate) credentials: Arc<dyn CredentialSource>,
    pub(crate) origin_route: String,
    pub(crate) model: String,
    /// Today's `drive()` path for THIS request: what a fallback runs (§5).
    pub(crate) sse: Box<dyn Fn() -> ProviderStream + Send>,
    pub(crate) cancel: CancellationToken,
}

/// Drive one request over WebSocket, falling back to `request.sse` by §5. The
/// returned stream is hand-written, like `drive`'s: dropping it drops the
/// connection, the in-flight read and the slot guard with it, which is exactly
/// what §4 calls a cancellation.
pub(crate) fn stream(request: WebSocketRequest, slot: OwnedMutexGuard<Slot>) -> ProviderStream {
    let stream = unfold(State::new(request, slot), |state| async move {
        let (event, state) = step(state).await;
        event.map(|event| (event, state))
    })
    // `unfold` panics if polled after it has ended; `fuse` makes the terminal poll
    // idempotent, as the contract's stream rules require.
    .fuse();
    Box::pin(stream)
}

/// What the state machine waits on next.
enum Phase {
    /// Fetch the credential for the first attempt.
    Credential,
    /// The forced refresh of a rejected credential (§5, 401/403).
    Refresh { rejected: Credential },
    /// Open the connection: the one the slot holds, or a new one.
    Connect,
    /// Send this request's ONE frame.
    Send,
    /// Read frames until the parser's terminal event.
    Read,
    /// Run today's SSE path for this request (§5).
    Fallback,
    /// The terminal event has been queued; the next poll ends the stream.
    Done,
}

struct State {
    request: WebSocketRequest,
    phase: Phase,
    /// Events produced by the current step, forwarded one per poll in order.
    pending: VecDeque<StreamEvent>,
    credential: Option<Credential>,
    /// A fresh parser per attempt, exactly as `drive` builds one.
    parser: Box<dyn ResponseParser>,
    live: Option<Live>,
    /// The slot this request owns for its whole life.
    slot: OwnedMutexGuard<Slot>,
    /// Whether this attempt has seen no frame yet: the state §5's
    /// "a reused socket closes before its first frame" is about.
    awaiting_first_frame: bool,
    /// Whether any content delta (text/reasoning/tool input) has been forwarded.
    /// Once true, every failure is this response's own failure: no retry, no
    /// fallback (§5).
    visible: bool,
    /// Reconnects used. §5 allows at most one per request.
    reconnects: u32,
    /// Whether the one forced credential refresh has been used.
    refreshed: bool,
    /// The fallback stream, built the first time `Phase::Fallback` is reached.
    sse: Option<ProviderStream>,
}

impl State {
    fn new(request: WebSocketRequest, slot: OwnedMutexGuard<Slot>) -> Self {
        let parser = new_parser(&request);
        Self {
            request,
            phase: Phase::Credential,
            pending: VecDeque::new(),
            credential: None,
            parser,
            live: None,
            slot,
            awaiting_first_frame: false,
            visible: false,
            reconnects: 0,
            refreshed: false,
            sse: None,
        }
    }

    /// Queue the single terminal event and stop. Every path here reaches it
    /// without the connection: a connection goes back to its slot only through
    /// [`State::terminal`], and only for a clean completion.
    fn finish(mut self, outcome: Outcome) -> Self {
        self.live = None;
        self.pending.push_back(StreamEvent::Finished(outcome));
        self.phase = Phase::Done;
        self
    }

    /// The end of a response: a completed one returns the connection to its slot
    /// (§4), every other ending drops it with this state.
    fn terminal(mut self, outcome: Outcome) -> Self {
        if matches!(outcome, Outcome::Completed(_))
            && let Some(mut live) = self.live.take()
        {
            live.last_used_at = (self.request.ws.clock)();
            self.slot.connection = Some(live);
        }
        self.finish(outcome)
    }

    /// §5: drop the connection — a socket we have read from is never reused — and
    /// open a new one, which sends the FULL body. At most once per request.
    fn reconnect(mut self) -> Self {
        self.reconnects += 1;
        self.live = None;
        self.parser = new_parser(&self.request);
        self.awaiting_first_frame = false;
        self.phase = Phase::Connect;
        self
    }

    /// §5: run today's `drive()` path for THIS request, and turn WebSocket off for
    /// this provider instance until the process ends.
    fn fall_back(mut self) -> Self {
        self.request.ws.disabled.store(true, Ordering::SeqCst);
        self.live = None;
        self.phase = Phase::Fallback;
        self
    }
}

/// One step of the state machine: await at most one I/O operation, then return the
/// next event (or `None` when the stream is over).
async fn step(mut state: State) -> (Option<StreamEvent>, State) {
    loop {
        if let Some(event) = state.pending.pop_front() {
            return (Some(event), state);
        }
        state = match std::mem::replace(&mut state.phase, Phase::Done) {
            Phase::Done => return (None, state),
            Phase::Credential => obtain_credential(state).await,
            Phase::Refresh { rejected } => refresh(state, rejected).await,
            Phase::Connect => connect(state).await,
            Phase::Send => send(state).await,
            Phase::Read => read(state).await,
            Phase::Fallback => fallback(state).await,
        };
    }
}

async fn obtain_credential(mut state: State) -> State {
    if state.request.cancel.is_cancelled() {
        return state.finish(Outcome::Cancelled);
    }
    let credentials = state.request.credentials.clone();
    let cancel = state.request.cancel.clone();
    match race(&cancel, credentials.access()).await {
        Raced::Cancelled => state.finish(Outcome::Cancelled),
        Raced::Done(Ok(credential)) => {
            state.credential = Some(credential);
            state.phase = Phase::Connect;
            state
        }
        Raced::Done(Err(error)) => state.finish(Outcome::Failed(error)),
    }
}

async fn refresh(mut state: State, rejected: Credential) -> State {
    if state.request.cancel.is_cancelled() {
        return state.finish(Outcome::Cancelled);
    }
    let credentials = state.request.credentials.clone();
    let cancel = state.request.cancel.clone();
    match race(&cancel, credentials.refresh(&rejected)).await {
        Raced::Cancelled => state.finish(Outcome::Cancelled),
        Raced::Done(Ok(credential)) => {
            state.credential = Some(credential);
            state.phase = Phase::Connect;
            state
        }
        Raced::Done(Err(error)) => state.finish(Outcome::Failed(error)),
    }
}

async fn connect(mut state: State) -> State {
    if state.request.cancel.is_cancelled() {
        return state.finish(Outcome::Cancelled);
    }
    // §4: reuse the connection the slot holds while it is young and recently used;
    // otherwise drop it and connect anew.
    if let Some(live) = state.slot.connection.take() {
        let now = (state.request.ws.clock)();
        if now.duration_since(live.connected_at) < MAX_AGE
            && now.duration_since(live.last_used_at) < MAX_IDLE
        {
            state.live = Some(Live {
                reused: true,
                ..live
            });
            state.phase = Phase::Send;
            return state;
        }
    }
    let credential = state
        .credential
        .clone()
        .expect("a credential is obtained before the first attempt");
    let handshake = match handshake(&state.request, &credential) {
        Ok(handshake) => handshake,
        Err(error) => return state.finish(Outcome::Failed(error)),
    };
    let connector = state.request.ws.connector.clone();
    let cancel = state.request.cancel.clone();
    match race_bounded(&cancel, connector.connect(handshake)).await {
        Raced::Cancelled => state.finish(Outcome::Cancelled),
        // §5: a connect error or a timeout falls back to SSE.
        Raced::Done(Err(_elapsed)) => state.fall_back(),
        Raced::Done(Ok(Err(error))) => match error {
            WsConnectError::Status { status, body } => refused_upgrade(state, status, &body),
            WsConnectError::Failed(_) => state.fall_back(),
        },
        Raced::Done(Ok(Ok(connection))) => {
            let now = (state.request.ws.clock)();
            state.live = Some(Live {
                connection,
                connected_at: now,
                last_used_at: now,
                reused: false,
            });
            state.phase = Phase::Send;
            state
        }
    }
}

/// §5's upgrade-refusal policy. The classification goes through the SAME parser
/// the SSE path uses for a non-2xx answer, so the two cannot disagree about kinds;
/// the body is classification input and never reaches a message.
fn refused_upgrade(mut state: State, status: u16, body: &[u8]) -> State {
    let error = state.parser.on_http_error(status, &[], body);
    match status {
        401 | 403 => {
            if state.refreshed {
                // §5: refused again after the one refresh is an authentication
                // failure — the same shape `drive` produces for a second 401/403.
                let error = ProviderError::new(ProviderErrorKind::Authentication, error.message);
                return state.finish(Outcome::Failed(error));
            }
            state.refreshed = true;
            // The reconnect the refresh buys is the one §5 allows per request.
            state.reconnects += 1;
            let rejected = state
                .credential
                .clone()
                .expect("a credential is obtained before the first attempt");
            state.phase = Phase::Refresh { rejected };
            state
        }
        // §5: a rate limit is not worth falling back for — SSE would hit the same
        // limit, and the caller should see the limit rather than a retry storm.
        429 => state.finish(Outcome::Failed(error)),
        // Any other refusal says nothing about SSE: fall back to it.
        _ => state.fall_back(),
    }
}

async fn send(mut state: State) -> State {
    if state.request.cancel.is_cancelled() {
        return state.finish(Outcome::Cancelled);
    }
    // Sending always precedes any output of this request, so §5's "after visible
    // output" rule cannot apply here.
    let frame = request_frame(&state.request);
    let cancel = state.request.cancel.clone();
    let sent = {
        let live = state
            .live
            .as_mut()
            .expect("a connection is open before a frame is sent");
        race_bounded(&cancel, live.connection.send_text(frame)).await
    };
    match sent {
        Raced::Cancelled => state.finish(Outcome::Cancelled),
        Raced::Done(Ok(Ok(()))) => {
            state.awaiting_first_frame = true;
            state.phase = Phase::Read;
            state
        }
        // A send that fails on a connection we reused is that socket having gone
        // away before our first frame: §5 reconnects once for it. On a fresh
        // connection it is an ordinary send failure, which falls back.
        Raced::Done(_) if reused(&state) && state.reconnects == 0 => state.reconnect(),
        Raced::Done(_) => state.fall_back(),
    }
}

async fn read(mut state: State) -> State {
    let cancel = state.request.cancel.clone();
    let received = {
        let live = state
            .live
            .as_mut()
            .expect("a connection is open while reading");
        // §4: a read is bounded by the adapter's existing idle timeout — this
        // adapter has none for a streaming body, so only cancellation ends the wait.
        race(&cancel, live.connection.next_text()).await
    };
    match received {
        Raced::Cancelled => state.finish(Outcome::Cancelled),
        Raced::Done(Ok(Some(text))) => state.on_frame(&text),
        Raced::Done(Ok(None)) | Raced::Done(Err(_)) => state.on_close(),
    }
}

impl State {
    /// One received text is ONE JSON event, fed to the existing parser with no
    /// event name (§3).
    fn on_frame(mut self, text: &str) -> State {
        // §5: the two error events that say "this connection cannot carry this
        // request" reconnect once and resend the FULL body — which is the only body
        // this stage sends. After output they are ordinary failures.
        if !self.visible && self.reconnects == 0 && reconnect_code(text) {
            return self.reconnect();
        }
        self.awaiting_first_frame = false;
        let events = self.parser.on_event(SseEvent {
            event: None,
            data: text.to_string(),
        });
        for event in events {
            if let StreamEvent::Finished(outcome) = event {
                return self.terminal(outcome);
            }
            if is_visible(&event) {
                self.visible = true;
            }
            self.pending.push_back(event);
        }
        self.phase = Phase::Read;
        self
    }

    /// The connection closed or the read failed.
    fn on_close(mut self) -> State {
        if self.awaiting_first_frame && reused(&self) && !self.visible && self.reconnects == 0 {
            return self.reconnect();
        }
        let outcome = self.parser.on_end();
        if self.visible {
            // §5: after model-visible output, a broken stream is an ordinary
            // Transport failure of that response: no retry, no fallback.
            self.terminal(outcome)
        } else {
            // §5's last row: a read error or a close before any output, with the
            // reconnects above exhausted, falls back to SSE.
            self.fall_back()
        }
    }
}

/// Whether the connection this attempt is using was already open.
fn reused(state: &State) -> bool {
    state.live.as_ref().is_some_and(|live| live.reused)
}

async fn fallback(mut state: State) -> State {
    if state.sse.is_none() {
        let stream = (state.request.sse)();
        state.sse = Some(stream);
    }
    let next = state
        .sse
        .as_mut()
        .expect("the fallback stream is built on entry")
        .next()
        .await;
    match next {
        // `drive` already produced the one terminal event; the stream is over.
        Some(StreamEvent::Finished(outcome)) => state.finish(outcome),
        Some(event) => {
            state.pending.push_back(event);
            state.phase = Phase::Fallback;
            state
        }
        None => {
            state.phase = Phase::Done;
            state
        }
    }
}

fn new_parser(request: &WebSocketRequest) -> Box<dyn ResponseParser> {
    Box::new(CodexResponseParser::new(
        &request.origin_route,
        &request.model,
    ))
}

/// The handshake for one attempt (`docs/design/websocket.md` §3): the endpoint the
/// adapter resolved with its scheme swapped, and the header set §3 names, in order.
fn handshake(
    request: &WebSocketRequest,
    credential: &Credential,
) -> Result<WsHandshake, ProviderError> {
    Ok(WsHandshake {
        url: websocket_url(&request.url),
        headers: build_ws_headers(request.account, credential, request.cache_key.as_deref())?,
    })
}

/// §3: the HTTPS URL the adapter already resolved, scheme swapped `https`→`wss`
/// (`http`→`ws`). The vendor documents no other difference.
fn websocket_url(url: &str) -> String {
    if let Some(rest) = url.strip_prefix("https://") {
        format!("wss://{rest}")
    } else if let Some(rest) = url.strip_prefix("http://") {
        format!("ws://{rest}")
    } else {
        // `resolve_base_url` already refused anything that is not `http(s)://`.
        url.to_string()
    }
}

/// The ONE frame this request sends.
///
/// §6's continuation decision lands EXACTLY here: it will consult the live
/// connection's remembered body, response id and output items, and when all three
/// of §6's rules hold it will send only the new items with `previous_response_id`
/// set. This stage keeps no such memory, so the FULL body always goes out and
/// `previous_response_id` is never sent.
fn request_frame(request: &WebSocketRequest) -> String {
    ws_frame(&request.body)
}

/// Whether a frame is one of the two error events §5 reconnects for. The parser
/// stays the authority for every other event; this only asks whether the
/// connection, not the request, is the problem.
fn reconnect_code(text: &str) -> bool {
    let Ok(value) = serde_json::from_str::<Value>(text) else {
        return false;
    };
    if !matches!(
        value.get("type").and_then(Value::as_str),
        Some("error") | Some("response.failed")
    ) {
        return false;
    }
    matches!(
        frame_error_code(&value).as_deref(),
        Some("previous_response_not_found") | Some("websocket_connection_limit_reached")
    )
}

/// The code of an error frame: `response.failed` carries it under
/// `response.error`, a bare `error` event at top level. The same shape the parser
/// reads (`docs/design/websocket.md` §3).
fn frame_error_code(value: &Value) -> Option<String> {
    let error = value
        .get("response")
        .and_then(|response| response.get("error"))
        .or_else(|| value.get("error"));
    error
        .and_then(|error| {
            error
                .get("code")
                .and_then(Value::as_str)
                .or_else(|| error.get("type").and_then(Value::as_str))
        })
        .or_else(|| value.get("code").and_then(Value::as_str))
        .map(str::to_string)
}

fn is_visible(event: &StreamEvent) -> bool {
    matches!(
        event,
        StreamEvent::TextDelta { .. }
            | StreamEvent::ReasoningDelta { .. }
            | StreamEvent::ToolInputDelta { .. }
    )
}

enum Raced<T> {
    Done(T),
    Cancelled,
}

/// Await `future`, but stop as soon as `cancel` fires (§4: every wait races the
/// request's cancellation token).
async fn race<T>(cancel: &CancellationToken, future: impl Future<Output = T>) -> Raced<T> {
    let cancelled = cancel.cancelled();
    let future = std::pin::pin!(future);
    let cancelled = std::pin::pin!(cancelled);
    match select(future, cancelled).await {
        Either::Left((value, _)) => Raced::Done(value),
        Either::Right(((), _)) => Raced::Cancelled,
    }
}

/// Await `future` under [`BOUND`] and under cancellation (§4). The timeout is the
/// only thing that can give up on a peer that never answers.
async fn race_bounded<T>(
    cancel: &CancellationToken,
    future: impl Future<Output = T>,
) -> Raced<Result<T, tokio::time::error::Elapsed>> {
    race(cancel, tokio::time::timeout(BOUND, future)).await
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn only_the_two_connection_error_events_are_reconnectable() {
        for code in [
            "previous_response_not_found",
            "websocket_connection_limit_reached",
        ] {
            assert!(
                reconnect_code(&format!(
                    r#"{{"type":"error","error":{{"code":"{code}"}}}}"#
                )),
                "{code}"
            );
            assert!(
                reconnect_code(&format!(
                    r#"{{"type":"response.failed","response":{{"error":{{"code":"{code}"}}}}}}"#
                )),
                "{code} as response.failed"
            );
        }
        for text in [
            r#"{"type":"error","error":{"code":"rate_limit_exceeded"}}"#,
            r#"{"type":"response.output_text.delta","delta":"hi"}"#,
            r#"{"type":"response.completed","response":{}}"#,
            "not json",
        ] {
            assert!(!reconnect_code(text), "{text}");
        }
    }
}
