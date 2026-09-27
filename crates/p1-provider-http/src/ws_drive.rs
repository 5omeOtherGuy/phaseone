//! The transport broker's WebSocket side: one request of a provider component whose
//! `lower` chose WebSocket (`modules/wit/transport.wit` `websocket`, ADR-0078 §1–§2).
//!
//! The component decides what goes out — a handshake head exactly when no connection is
//! open, the full request frame or the shorter continuation frame, or the fallback to HTTP —
//! from the facts the host session reports ([`WsLease::state`]). This driver is everything
//! else, `docs/design/websocket.md` §4–§5 as the native Responses adapter ran it: it leases
//! the session, attaches the credential through the session, reads the response's frames and
//! feeds each one to the component's decoder as an event with no name, and applies the
//! failure policy:
//!
//! - an upgrade refused with 401/403 refreshes the credential once and lowers again; a second
//!   refusal is `Authentication`; a route whose credential an egress proxy injects is never
//!   refreshed;
//! - an upgrade refused with 429 is the rate limit, with no fallback;
//! - any other refusal, and the transient row past its budget, report the failure before
//!   output ([`WsLease::fail_before_output`]) and lower again, and the component answers with
//!   the HTTP request this driver then sends through [`broker_drive`], after a display-only
//!   notice;
//! - the transient row — a connect error or timeout, or a write, read, close or first-frame
//!   bound before any output — reconnects after the retry policy's backoff, inside its budget;
//! - three "once" rows reconnect at once, once each per request: a reused socket that is gone
//!   before its first frame, and the two error frames that say the connection, not the
//!   request, cannot carry this response;
//! - after model-visible output every failure is the response's own: no retry, no fallback.
//!
//! A response that completes cleanly hands its connection back to the session with the
//! decoder's response id ([`ResponseParser::response_id`]), which the next `lower` reads as
//! `connection-state.last-clean-response`; every other ending drops the connection, and the
//! lease is released as soon as the terminal event is queued. The returned stream is
//! hand-written, like `drive`'s: dropping it drops the lease, the connection and the
//! in-flight read with it, which is what §4 calls a cancellation.

use std::collections::VecDeque;
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use futures_util::StreamExt;
use futures_util::future::{Either, select};
use futures_util::stream::unfold;
use p1_contracts::serde_json::{self, Value};
use p1_contracts::{
    CancellationToken, Outcome, ProviderError, ProviderErrorKind, ProviderStream, StreamEvent,
};

use crate::broker::{LoweredHttpRequest, RouteAuthority, broker_drive};
use crate::credential::{Credential, CredentialSource};
use crate::drive::proxy_refusal_message;
use crate::http::Transport;
use crate::parser::ResponseParser;
use crate::retry::RetryPolicy;
use crate::sse::SseEvent;
use crate::ws::{WsBound, WsNext};
use crate::ws_session::{
    ConnectionState, WsAuthority, WsLease, WsRead, WsSend, WsSendError, WsSession,
};

/// The `<reason>` of a fallback notice when no HTTP status refused the upgrade: a connect
/// failure, a timeout, or a connection that broke before any output.
const NO_CONNECTION: &str = "connection failed";

/// The notice a fallback emits (ADR-0048): display-only, never history, journal or model
/// input. The wording is the native Responses adapter's, so the operator sees the same line.
fn fallback_notice(reason: &str) -> StreamEvent {
    StreamEvent::Notice {
        text: format!(
            "transport: WebSocket unavailable ({reason}) — using HTTP (SSE) for the rest of this session"
        ),
    }
}

/// WIT `provider.lowered-request`, as the broker executes it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WsLowered {
    Http(LoweredHttpRequest),
    WebSocket(WsSend),
}

/// The component's `lower` for one request: called again, with the session's facts, for
/// every attempt after the first.
pub type WsLower = Box<dyn FnMut(ConnectionState) -> Result<WsLowered, ProviderError> + Send>;

/// Everything one WebSocket request needs.
pub struct WsDriveRequest {
    /// The session lease this request holds until its terminal event.
    pub lease: WsLease,
    /// The first attempt's send, lowered from `lease`'s state before the request started.
    pub send: WsSend,
    pub lower: WsLower,
    /// The route authority a lowered path goes to: the handshake's endpoint and the fallback
    /// request's. Every authority it returns has the route's one credential source.
    pub authority: Box<dyn Fn(&str) -> RouteAuthority + Send + Sync>,
    /// The transport the HTTP fallback is sent on.
    pub transport: Arc<dyn Transport>,
    /// A fresh parser per attempt: the component's decoder.
    pub new_parser: Arc<dyn Fn() -> Box<dyn ResponseParser> + Send + Sync>,
    /// §5's transient row waits this policy's backoff, up to its `max_retries`; the fallback
    /// request is driven under it too.
    pub retry: RetryPolicy,
    pub cancel: CancellationToken,
}

/// Lease `session` for one request, waiting while another request's response holds it and
/// racing `cancel`: `Err` is the stream of a request cancelled while it waited, which settles
/// at once as cancelled.
pub async fn ws_lease(
    session: &WsSession,
    cancel: &CancellationToken,
) -> Result<WsLease, ProviderStream> {
    match race(cancel, session.lease()).await {
        Raced::Done(lease) => Ok(lease),
        Raced::Cancelled => Err(Box::pin(futures_util::stream::once(async {
            StreamEvent::Finished(Outcome::Cancelled)
        }))),
    }
}

/// Drive one request over WebSocket, falling back to HTTP when the component chooses it.
/// The stream obeys the five stream rules of `p1-contracts/src/provider.rs`.
pub fn ws_drive(request: WsDriveRequest) -> ProviderStream {
    let stream = unfold(State::new(request), |state| async move {
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
    /// Lower this attempt from the session's connection state.
    Lower,
    /// Hand this attempt's send to the session: open a connection when the send has a
    /// head, then write the one frame.
    Send { send: WsSend },
    /// Read frames until the parser's terminal event.
    Read,
    /// Wait out the backoff of §5's transient row, racing cancellation.
    Wait { delay: Duration },
    /// Forward the HTTP fallback's stream.
    Fallback { stream: ProviderStream },
    /// The terminal event has been queued; the next poll ends the stream.
    Done,
}

struct State {
    lower: WsLower,
    authority: Box<dyn Fn(&str) -> RouteAuthority + Send + Sync>,
    credentials: Arc<dyn CredentialSource>,
    transport: Arc<dyn Transport>,
    new_parser: Arc<dyn Fn() -> Box<dyn ResponseParser> + Send + Sync>,
    retry: RetryPolicy,
    cancel: CancellationToken,
    phase: Phase,
    /// The first attempt's send, until the credential for it exists.
    first: Option<WsSend>,
    /// Events produced by the current step, forwarded one per poll in order.
    pending: VecDeque<StreamEvent>,
    credential: Option<Credential>,
    /// A fresh parser per attempt.
    parser: Box<dyn ResponseParser>,
    /// The session lease, released when the terminal event is queued or the request falls
    /// back to HTTP.
    lease: Option<WsLease>,
    /// Whether this attempt has seen no frame yet: the state §5's "a reused socket closes
    /// before its first frame" is about.
    awaiting_first_frame: bool,
    /// Whether any content delta (text/reasoning/tool input) has been forwarded. Once true,
    /// every failure is this response's own failure: no retry, no fallback (§5).
    visible: bool,
    /// §5's transient row: the reconnects this request has already used.
    transient_retries: u32,
    /// §5's three "once" rows, each of which reconnects at most once per request.
    once: OnceRows,
    /// Whether the one forced credential refresh has been used.
    refreshed: bool,
    /// The `<reason>` of the fallback notice, set when §5 allows this request no further
    /// WebSocket attempt.
    fallback_reason: Option<String>,
}

/// §5's three "once" rows: each reconnects at most ONCE per request, independently of the
/// transient budget.
#[derive(Default)]
struct OnceRows {
    /// "A reused socket closes before its first frame".
    reused_close: bool,
    /// The two error events that say the CONNECTION, not the request, cannot carry this
    /// response. Slotted by [`ReconnectRow::slot`].
    error_event: [bool; 2],
}

/// The two error events §5 reconnects for, each an "once" row of its own.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReconnectRow {
    PreviousResponseNotFound,
    ConnectionLimitReached,
}

impl ReconnectRow {
    fn slot(self) -> usize {
        match self {
            Self::PreviousResponseNotFound => 0,
            Self::ConnectionLimitReached => 1,
        }
    }
}

impl State {
    fn new(request: WsDriveRequest) -> Self {
        let parser = (request.new_parser)();
        let credentials = (request.authority)("").credentials.clone();
        Self {
            lower: request.lower,
            authority: request.authority,
            credentials,
            transport: request.transport,
            new_parser: request.new_parser,
            retry: request.retry,
            cancel: request.cancel,
            phase: Phase::Credential,
            first: Some(request.send),
            pending: VecDeque::new(),
            credential: None,
            parser,
            lease: Some(request.lease),
            awaiting_first_frame: false,
            visible: false,
            transient_retries: 0,
            once: OnceRows::default(),
            refreshed: false,
            fallback_reason: None,
        }
    }

    /// The lease of a request that has not fallen back or ended; every phase that reaches
    /// the session runs before either.
    fn lease(&mut self) -> &mut WsLease {
        self.lease
            .as_mut()
            .expect("the lease is held until the request falls back or ends")
    }

    /// Whether the current (or just failed) attempt used a connection that was already open.
    fn reused(&self) -> bool {
        self.lease.as_ref().is_some_and(WsLease::reused)
    }

    /// Queue the single terminal event and stop, dropping the connection and releasing the
    /// lease. A connection goes back to the session only through [`State::terminal`].
    fn finish(mut self, outcome: Outcome) -> Self {
        if let Some(mut lease) = self.lease.take() {
            lease.drop_connection();
        }
        self.pending.push_back(StreamEvent::Finished(outcome));
        self.phase = Phase::Done;
        self
    }

    /// The end of a response: a completed one returns the connection to the session (§4)
    /// with the response id its decoder saw; every other ending drops the connection.
    fn terminal(mut self, outcome: Outcome) -> Self {
        if matches!(outcome, Outcome::Completed(_)) {
            let response_id = self.parser.response_id();
            if let Some(mut lease) = self.lease.take() {
                lease.completed(response_id);
            }
        }
        self.finish(outcome)
    }

    /// §5's "once" rows: drop the connection and lower again at once. No connection is
    /// open then, so the component sends a handshake with the FULL body.
    fn reconnect(mut self) -> Self {
        self.restart();
        self.phase = Phase::Lower;
        self
    }

    /// §5's transient row: reconnect after the retry policy's backoff, inside its budget;
    /// the budget spent, report the failure and let the component fall back.
    fn transient(mut self) -> Self {
        if self.transient_retries >= self.retry.max_retries {
            return self.fall_back(NO_CONNECTION);
        }
        self.transient_retries += 1;
        let delay = self.retry.delay(self.transient_retries, None);
        // Stream rule 4: a back-off yields `Activity`, as `drive` does on its own
        // transient path.
        self.pending.push_back(StreamEvent::Activity);
        self.restart();
        self.phase = Phase::Wait { delay };
        self
    }

    /// Drop the connection and everything that belonged to this attempt, so the next one
    /// starts from the beginning of the response on a NEW socket.
    fn restart(&mut self) {
        self.lease().drop_connection();
        self.parser = (self.new_parser)();
        self.awaiting_first_frame = false;
    }

    /// §5 allows this request no further WebSocket attempt: the session reports the
    /// failure before output, and the next lowering is the component's.
    fn fall_back(mut self, reason: &str) -> Self {
        self.lease().fail_before_output();
        self.fallback_reason = Some(reason.to_string());
        self.phase = Phase::Lower;
        self
    }
}

/// One step of the state machine: await at most one I/O operation, then return the next
/// event (or `None` when the stream is over).
async fn step(mut state: State) -> (Option<StreamEvent>, State) {
    loop {
        if let Some(event) = state.pending.pop_front() {
            return (Some(event), state);
        }
        state = match std::mem::replace(&mut state.phase, Phase::Done) {
            Phase::Done => return (None, state),
            Phase::Credential => obtain_credential(state).await,
            Phase::Refresh { rejected } => refresh(state, rejected).await,
            Phase::Lower => lower(state),
            Phase::Send { send } => send_frame(state, send).await,
            Phase::Read => read(state).await,
            Phase::Wait { delay } => wait(state, delay).await,
            Phase::Fallback { stream } => fallback(state, stream).await,
        };
    }
}

async fn obtain_credential(mut state: State) -> State {
    if state.cancel.is_cancelled() {
        return state.finish(Outcome::Cancelled);
    }
    let credentials = state.credentials.clone();
    let cancel = state.cancel.clone();
    match race(&cancel, credentials.access()).await {
        Raced::Cancelled => state.finish(Outcome::Cancelled),
        Raced::Done(Ok(credential)) => {
            state.credential = Some(credential);
            state.phase = match state.first.take() {
                Some(send) => Phase::Send { send },
                None => Phase::Lower,
            };
            state
        }
        Raced::Done(Err(error)) => state.finish(Outcome::Failed(error)),
    }
}

async fn refresh(mut state: State, rejected: Credential) -> State {
    if state.cancel.is_cancelled() {
        return state.finish(Outcome::Cancelled);
    }
    let credentials = state.credentials.clone();
    let cancel = state.cancel.clone();
    match race(&cancel, credentials.refresh(&rejected)).await {
        Raced::Cancelled => state.finish(Outcome::Cancelled),
        Raced::Done(Ok(credential)) => {
            state.credential = Some(credential);
            state.phase = Phase::Lower;
            state
        }
        Raced::Done(Err(error)) => state.finish(Outcome::Failed(error)),
    }
}

/// WIT `lower(request, connection-state)` for a retry: the session reports its facts and
/// the component chooses. Its HTTP answer is the fallback, announced here ONCE, before the
/// HTTP request is even started, so the notice precedes the response's first event.
fn lower(mut state: State) -> State {
    let connection = state.lease().state();
    match (state.lower)(connection) {
        Err(error) => state.finish(Outcome::Failed(error)),
        Ok(WsLowered::WebSocket(send)) => {
            state.phase = Phase::Send { send };
            state
        }
        Ok(WsLowered::Http(request)) => {
            // The lease goes back now: the HTTP request needs no connection, and another
            // request may use the session meanwhile.
            if let Some(mut lease) = state.lease.take() {
                lease.drop_connection();
            }
            let reason = state
                .fallback_reason
                .take()
                .unwrap_or_else(|| NO_CONNECTION.to_string());
            state.pending.push_back(fallback_notice(&reason));
            let authority = (state.authority)(&request.path);
            let new_parser = state.new_parser.clone();
            let sent = broker_drive(
                &authority,
                state.transport.clone(),
                &request,
                Box::new(move || new_parser()),
                state.retry,
                state.cancel.clone(),
            );
            match sent {
                Ok(stream) => {
                    state.phase = Phase::Fallback { stream };
                    state
                }
                Err(error) => state.finish(Outcome::Failed(error)),
            }
        }
    }
}

async fn send_frame(mut state: State, send: WsSend) -> State {
    // A route whose credential an egress proxy injects attaches no credential at all.
    let credential = if state.credentials.proxy_injected() {
        None
    } else {
        state.credential.clone()
    };
    // The session reads the endpoint only for a send that opens a connection.
    let path = send
        .handshake
        .as_ref()
        .map_or("", |head| head.path.as_str());
    let authority = (state.authority)(path);
    let cancel = state.cancel.clone();
    let sent = state
        .lease()
        .send(
            WsAuthority {
                endpoint: authority.endpoint_base(),
                credential: credential.as_ref(),
            },
            send,
            &cancel,
        )
        .await;
    match sent {
        Ok(()) => {
            state.awaiting_first_frame = true;
            state.phase = Phase::Read;
            state
        }
        Err(WsSendError::Cancelled) => state.finish(Outcome::Cancelled),
        Err(WsSendError::Invalid(error)) => state.finish(Outcome::Failed(error)),
        Err(WsSendError::Refused { status, body }) => refused_upgrade(state, status, &body),
        // §5: a connect error or a timeout is the transient row: the socket never came up,
        // so nothing was sent.
        Err(WsSendError::ConnectFailed) => state.transient(),
        // A send that fails on a connection we reused is that socket having gone away
        // before our first frame: §5 reconnects once for it.
        Err(WsSendError::WriteFailed) if state.reused() && !state.once.reused_close => {
            state.once.reused_close = true;
            state.reconnect()
        }
        Err(WsSendError::WriteFailed) => state.transient(),
    }
}

/// §5's upgrade-refusal policy. The classification is the component's `classify`, the same
/// one an HTTP response gets; the body is classification input and never reaches a message.
fn refused_upgrade(mut state: State, status: u16, body: &[u8]) -> State {
    let error = state.parser.on_http_error(status, &[], body);
    match status {
        401 | 403 => {
            // A route whose credential an egress proxy injects sends no credential, so there
            // is nothing to refresh: the refusal is the proxy's, worded as `drive` words it.
            if state.credentials.proxy_injected() {
                let error = ProviderError::new(
                    ProviderErrorKind::Authentication,
                    proxy_refusal_message(status),
                );
                return state.finish(Outcome::Failed(error));
            }
            if state.refreshed {
                let error = ProviderError::new(ProviderErrorKind::Authentication, error.message);
                return state.finish(Outcome::Failed(error));
            }
            state.refreshed = true;
            let rejected = state
                .credential
                .clone()
                .expect("a credential is obtained before the first attempt");
            state.phase = Phase::Refresh { rejected };
            state
        }
        // §5: HTTP would hit the same limit, and the caller should see the limit rather
        // than a retry storm.
        429 => state.finish(Outcome::Failed(error)),
        // Any other refusal says nothing about HTTP: fall back to it.
        _ => state.fall_back(&format!("HTTP {status}")),
    }
}

async fn read(mut state: State) -> State {
    // §4: the read is bounded INSIDE the connection, where the message loop sees every
    // frame, a control ping included (ADR-0069). Here the outcome is only classified.
    let cancel = state.cancel.clone();
    let received = state.lease().next(&cancel).await;
    match received {
        WsRead::Cancelled => state.finish(Outcome::Cancelled),
        WsRead::Next(WsNext::Timeout(bound)) => {
            let error = ProviderError::new(ProviderErrorKind::Transport, bound.message());
            state.on_read_timeout(bound, error)
        }
        WsRead::Next(WsNext::Text(text)) => state.on_frame(text),
        WsRead::Next(WsNext::Closed) | WsRead::Failed(_) => state.on_close(),
    }
}

impl State {
    /// One received text frame is ONE event, fed to the decoder with no name.
    fn on_frame(mut self, text: String) -> State {
        // §5: the two error events that say "this connection cannot carry this request"
        // reconnect once each. After output they are ordinary failures.
        if !self.visible
            && let Some(row) = reconnect_row(&text)
            && !self.once.error_event[row.slot()]
        {
            self.once.error_event[row.slot()] = true;
            return self.reconnect();
        }
        self.awaiting_first_frame = false;
        let events = self.parser.on_event(SseEvent {
            event: None,
            data: text,
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
        if self.awaiting_first_frame && self.reused() && !self.visible && !self.once.reused_close {
            self.once.reused_close = true;
            return self.reconnect();
        }
        if self.visible {
            // §5: after model-visible output, a broken stream is an ordinary failure of
            // that response: no retry, no fallback.
            let outcome = self.parser.on_end();
            self.terminal(outcome)
        } else {
            // §5's last row: a read error or a close before any output is the transient
            // row. The attempt's decoder is dropped unfinished with its connection.
            self.transient()
        }
    }

    /// A read whose bound expired. A FIRST-FRAME expiry on a REUSED connection is the dead
    /// slot `on_close` handles for a reused socket: reconnect once. Otherwise, before any
    /// output it is the transient row; after output it is this response's own `Transport`
    /// failure, whose message names the bound that expired.
    fn on_read_timeout(mut self, bound: WsBound, error: ProviderError) -> State {
        if bound == WsBound::FirstFrame && self.reused() && !self.visible && !self.once.reused_close
        {
            self.once.reused_close = true;
            return self.reconnect();
        }
        if self.visible {
            self.terminal(Outcome::Failed(error))
        } else {
            self.transient()
        }
    }
}

/// Wait out §5's transient-row backoff, racing cancellation, on the runtime's clock, so a
/// test drives it on the paused clock and never sleeps.
async fn wait(mut state: State, delay: Duration) -> State {
    let cancel = state.cancel.clone();
    match race(&cancel, tokio::time::sleep(delay)).await {
        Raced::Cancelled => state.finish(Outcome::Cancelled),
        Raced::Done(()) => {
            state.phase = Phase::Lower;
            state
        }
    }
}

/// The HTTP fallback's stream, forwarded until its own terminal event.
async fn fallback(mut state: State, mut stream: ProviderStream) -> State {
    match stream.next().await {
        Some(StreamEvent::Finished(outcome)) => state.finish(outcome),
        Some(event) => {
            state.pending.push_back(event);
            state.phase = Phase::Fallback { stream };
            state
        }
        None => state,
    }
}

/// The "once" row a frame names, if any: the two error events §5 reconnects for. The
/// decoder stays the authority for every other event; this only asks whether the
/// connection, not the request, is the problem.
fn reconnect_row(text: &str) -> Option<ReconnectRow> {
    let value: Value = serde_json::from_str(text).ok()?;
    if !matches!(
        value.get("type").and_then(Value::as_str),
        Some("error") | Some("response.failed")
    ) {
        return None;
    }
    match frame_error_code(&value).as_deref() {
        Some("previous_response_not_found") => Some(ReconnectRow::PreviousResponseNotFound),
        Some("websocket_connection_limit_reached") => Some(ReconnectRow::ConnectionLimitReached),
        _ => None,
    }
}

/// The code of an error frame: `response.failed` carries it under `response.error`, a bare
/// `error` event at top level.
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_two_connection_error_events_are_reconnectable() {
        for (code, row) in [
            (
                "previous_response_not_found",
                ReconnectRow::PreviousResponseNotFound,
            ),
            (
                "websocket_connection_limit_reached",
                ReconnectRow::ConnectionLimitReached,
            ),
        ] {
            assert_eq!(
                reconnect_row(&format!(
                    r#"{{"type":"error","error":{{"code":"{code}"}}}}"#
                )),
                Some(row),
                "{code}"
            );
            assert_eq!(
                reconnect_row(&format!(
                    r#"{{"type":"response.failed","response":{{"error":{{"code":"{code}"}}}}}}"#
                )),
                Some(row),
                "{code} as response.failed"
            );
        }
        for text in [
            r#"{"type":"error","error":{"code":"rate_limit_exceeded"}}"#,
            r#"{"type":"response.output_text.delta","delta":"hi"}"#,
            r#"{"type":"response.completed","response":{}}"#,
            "not json",
        ] {
            assert_eq!(reconnect_row(text), None, "{text}");
        }
    }
}
