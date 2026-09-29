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
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use futures_util::StreamExt;
use futures_util::stream::unfold;
use p1_contracts::{
    CancellationToken, Outcome, ProviderError, ProviderErrorKind, ProviderStream, StreamEvent,
};

use crate::broker::{LoweredHttpRequest, RouteAuthority, broker_drive};
use crate::credential::{Credential, CredentialSource};
use crate::drive::proxy_refusal_message;
use crate::http::Transport;
use crate::parser::ResponseParser;
use crate::race::{Raced, race};
use crate::retry::RetryPolicy;
use crate::sse::SseEvent;
use crate::ws::{WS_FRAME_LIMIT, WS_RESPONSE_LIMIT, WsBound, WsNext};
use crate::ws_policy::{
    OnceRows, ReadRecovery, admit_frame, is_visible, reconnect_row, use_transient_retry,
};
use crate::ws_session::{
    ConnectionState, WsAuthority, WsLease, WsRead, WsSend, WsSendError, WsSession, validate_head,
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
pub type WsLower = Box<
    dyn FnMut(
            ConnectionState,
        ) -> Pin<Box<dyn Future<Output = Result<WsLowered, ProviderError>> + Send>>
        + Send,
>;

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
    // Cancellation outranks a full admission queue: a request that is already
    // cancelled settles as `Cancelled`, never as the busy `Transport` failure the
    // queue's immediate `None` would otherwise produce before `cancel` is observed.
    if cancel.is_cancelled() {
        return Err(cancelled_lease_stream());
    }
    match race(cancel, session.lease_bounded()).await {
        Raced::Done(Some(lease)) => Ok(lease),
        Raced::Done(None) if !cancel.is_cancelled() => Err(busy_lease_stream()),
        Raced::Done(None) | Raced::Cancelled => Err(cancelled_lease_stream()),
    }
}

/// The stream of a request refused a lease because it is busy.
fn busy_lease_stream() -> ProviderStream {
    Box::pin(futures_util::stream::once(async {
        StreamEvent::Finished(Outcome::Failed(ProviderError::new(
            ProviderErrorKind::Transport,
            "WebSocket session is busy",
        )))
    }))
}

/// The stream of a request cancelled while it waited for (or was refused) a lease.
fn cancelled_lease_stream() -> ProviderStream {
    Box::pin(futures_util::stream::once(async {
        StreamEvent::Finished(Outcome::Cancelled)
    }))
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
    response_bytes: usize,
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
            response_bytes: 0,
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
    /// with the response id its decoder saw, unless the decoder's instance was lost on the
    /// way (ADR-0078 §3); every other ending drops the connection.
    fn terminal(mut self, outcome: Outcome) -> Self {
        if matches!(outcome, Outcome::Completed(_)) && !self.parser.instance_lost() {
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
        if !use_transient_retry(&mut self.transient_retries, self.retry.max_retries) {
            return self.fall_back(NO_CONNECTION);
        }
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
        self.response_bytes = 0;
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
            Phase::Lower => lower(state).await,
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
    if let Some(head) = state
        .first
        .as_ref()
        .and_then(|send| send.handshake.as_ref())
        && let Err(error) = validate_head(head)
    {
        return state.finish(Outcome::Failed(error));
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
async fn lower(mut state: State) -> State {
    let connection = state.lease().state();
    let failed_before_output = connection.failed_before_output;
    let cancel = state.cancel.clone();
    let future = (state.lower)(connection);
    let answer = match race(&cancel, future).await {
        Raced::Cancelled => return state.finish(Outcome::Cancelled),
        Raced::Done(answer) => answer,
    };
    // A lower that observed the token and answered with its cancellation error can win the
    // race above before the token is seen: the caller asked to stop, so the terminal is
    // still Cancelled, not the error.
    if cancel.is_cancelled() {
        return state.finish(Outcome::Cancelled);
    }
    match answer {
        Err(error) => state.finish(Outcome::Failed(error)),
        Ok(WsLowered::WebSocket(send)) => {
            if state.fallback_reason.is_some() || failed_before_output {
                return state.finish(Outcome::Failed(ProviderError::new(
                    ProviderErrorKind::Protocol,
                    "WebSocket retry budget exhausted",
                )));
            }
            if let Some(head) = &send.handshake
                && let Err(error) = validate_head(head)
            {
                return state.finish(Outcome::Failed(error));
            }
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
    if let Some(head) = &send.handshake
        && let Err(error) = validate_head(head)
    {
        return state.finish(Outcome::Failed(error));
    }
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
        // A capacity failure is terminal: the handshake exceeded the connector's
        // buffer bound, so another handshake cannot succeed. Terminate like an
        // oversized frame rather than spending the transient budget or falling back.
        Err(WsSendError::Capacity) => state.finish(Outcome::Failed(ProviderError::new(
            ProviderErrorKind::Protocol,
            "WebSocket handshake exceeds capacity",
        ))),
        Err(WsSendError::Refused { status, body }) => refused_upgrade(state, status, &body),
        // §5: a connect error or a timeout is the transient row: the socket never came up,
        // so nothing was sent.
        Err(WsSendError::ConnectFailed) => state.transient(),
        // A send that fails on a connection we reused is that socket having gone away
        // before our first frame: §5 reconnects once for it.
        Err(WsSendError::WriteFailed)
            if {
                let reused = state.reused();
                state.once.write_reconnect(reused)
            } =>
        {
            state.reconnect()
        }
        Err(WsSendError::WriteFailed) => state.transient(),
    }
}

/// §5's upgrade-refusal policy. The classification is the component's `classify`, the same
/// one an HTTP response gets; the body is classification input and never reaches a message.
fn refused_upgrade(mut state: State, status: u16, body: &[u8]) -> State {
    let error = state.parser.on_http_error(
        status,
        &[],
        &body[..body.len().min(crate::ws::WS_ERROR_BODY_LIMIT)],
    );
    if matches!(
        error.kind,
        ProviderErrorKind::InsufficientBalance
            | ProviderErrorKind::NotEntitled
            | ProviderErrorKind::UsageLimitExhausted
            | ProviderErrorKind::Protocol
    ) {
        return state.finish(Outcome::Failed(error));
    }
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
        WsRead::Failed(error)
            if error.0.contains("capacity") || error.0.contains("payload limit") =>
        {
            state.finish(Outcome::Failed(ProviderError::new(
                ProviderErrorKind::Protocol,
                "WebSocket frame exceeds payload limit",
            )))
        }
        WsRead::Next(WsNext::Closed) | WsRead::Failed(_) => state.on_close(),
    }
}

impl State {
    /// One received text frame is ONE event, fed to the decoder with no name.
    fn on_frame(mut self, text: String) -> State {
        if !admit_frame(
            &mut self.response_bytes,
            text.len(),
            WS_FRAME_LIMIT,
            WS_RESPONSE_LIMIT,
        ) {
            return self.finish(Outcome::Failed(ProviderError::new(
                ProviderErrorKind::Protocol,
                "WebSocket response exceeds payload limit",
            )));
        }
        // §5: the two error events that say "this connection cannot carry this request"
        // reconnect once each. After output they are ordinary failures.
        if self
            .once
            .reconnect_error(reconnect_row(&text), self.visible)
        {
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
        let reused = self.reused();
        match self
            .once
            .read_recovery(self.awaiting_first_frame, reused, self.visible)
        {
            ReadRecovery::Reconnect => self.reconnect(),
            ReadRecovery::Terminal => {
                let outcome = self.parser.on_end();
                self.terminal(outcome)
            }
            ReadRecovery::Transient => self.transient(),
        }
    }

    /// A read whose bound expired. A FIRST-FRAME expiry on a REUSED connection is the dead
    /// slot `on_close` handles for a reused socket: reconnect once. Otherwise, before any
    /// output it is the transient row; after output it is this response's own `Transport`
    /// failure, whose message names the bound that expired.
    fn on_read_timeout(mut self, bound: WsBound, error: ProviderError) -> State {
        let reused = self.reused();
        match self
            .once
            .read_recovery(bound == WsBound::FirstFrame, reused, self.visible)
        {
            ReadRecovery::Reconnect => self.reconnect(),
            ReadRecovery::Terminal => self.terminal(Outcome::Failed(error)),
            ReadRecovery::Transient => self.transient(),
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ws_policy::ReconnectRow;

    struct KindParser(ProviderErrorKind);
    impl ResponseParser for KindParser {
        fn on_event(&mut self, _: SseEvent) -> Vec<StreamEvent> {
            vec![]
        }
        fn on_end(&mut self) -> Outcome {
            Outcome::Failed(ProviderError::new(ProviderErrorKind::Transport, "ended"))
        }
        fn on_http_error(&self, _: u16, _: &[(String, String)], _: &[u8]) -> ProviderError {
            ProviderError::new(self.0, "classified")
        }
    }

    struct Source(std::sync::atomic::AtomicUsize);
    impl CredentialSource for Source {
        fn access<'a>(&'a self) -> p1_contracts::BoxFuture<'a, Result<Credential, ProviderError>> {
            self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Box::pin(async {
                Ok(Credential {
                    bearer: "fake".into(),
                    account_id: None,
                })
            })
        }
        fn refresh<'a>(
            &'a self,
            _: &'a Credential,
        ) -> p1_contracts::BoxFuture<'a, Result<Credential, ProviderError>> {
            self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Box::pin(async {
                Ok(Credential {
                    bearer: "fake".into(),
                    account_id: None,
                })
            })
        }
    }

    async fn state(kind: ProviderErrorKind) -> (State, Arc<Source>) {
        use crate::broker::{CredentialScheme, CredentialUse};
        use crate::testing::{ScriptedTransport, ScriptedWsConnector};
        use crate::ws_session::{Clock, WsHead};
        let source = Arc::new(Source(std::sync::atomic::AtomicUsize::new(0)));
        let session = WsSession::new(
            Arc::new(ScriptedWsConnector::new(vec![])),
            Arc::new(std::time::Instant::now) as Clock,
        );
        let send = WsSend {
            handshake: Some(WsHead {
                path: "/responses".into(),
                headers: vec![],
                credential: CredentialUse {
                    scheme: CredentialScheme::Bearer,
                    account_id_header: None,
                },
            }),
            frame: "{}".into(),
        };
        let authority: Arc<dyn CredentialSource> = source.clone();
        let parser: Arc<dyn Fn() -> Box<dyn ResponseParser> + Send + Sync> =
            Arc::new(move || Box::new(KindParser(kind)));
        let request = WsDriveRequest {
            lease: session.lease().await,
            send: send.clone(),
            lower: Box::new(move |_| {
                let send = send.clone();
                Box::pin(async move { Ok(WsLowered::WebSocket(send)) })
            }),
            authority: Box::new(move |_| {
                RouteAuthority::new("https://provider.test/v1", authority.clone()).unwrap()
            }),
            transport: Arc::new(ScriptedTransport::new(vec![])),
            new_parser: parser,
            retry: RetryPolicy::default(),
            cancel: CancellationToken::new(),
        };
        (State::new(request), source)
    }

    #[tokio::test]
    async fn upgrade_terminal_diagnoses_do_not_refresh_or_fall_back() {
        for (status, kind) in [
            (401, ProviderErrorKind::InsufficientBalance),
            (403, ProviderErrorKind::NotEntitled),
            (402, ProviderErrorKind::UsageLimitExhausted),
            (500, ProviderErrorKind::Protocol),
        ] {
            let (state, source) = state(kind).await;
            let result = refused_upgrade(state, status, b"classification");
            assert!(matches!(result.phase, Phase::Done));
            assert!(
                matches!(result.pending.back(), Some(StreamEvent::Finished(Outcome::Failed(error))) if error.kind == kind)
            );
            assert_eq!(source.0.load(std::sync::atomic::Ordering::SeqCst), 0);
        }
    }

    #[tokio::test]
    async fn exhausted_retry_budget_rejects_relowered_websocket() {
        let (mut state, _) = state(ProviderErrorKind::Transport).await;
        state.fallback_reason = Some("connection failed".into());
        state.lease().fail_before_output();
        let result = lower(state);
        assert!(matches!(result.phase, Phase::Done));
        assert!(
            matches!(result.pending.back(), Some(StreamEvent::Finished(Outcome::Failed(error))) if error.kind == ProviderErrorKind::Protocol)
        );
    }

    #[tokio::test]
    async fn oversized_response_is_terminal_before_parsing() {
        let (mut state, _) = state(ProviderErrorKind::Transport).await;
        for _ in 0..(WS_RESPONSE_LIMIT / WS_FRAME_LIMIT) {
            state = state.on_frame("x".repeat(WS_FRAME_LIMIT));
            assert!(matches!(state.phase, Phase::Read));
        }
        let state = state.on_frame("x".into());
        assert!(
            matches!(state.pending.back(), Some(StreamEvent::Finished(Outcome::Failed(error))) if error.kind == ProviderErrorKind::Protocol)
        );
    }

    #[tokio::test]
    async fn invalid_first_head_fails_before_accessing_credentials() {
        let (mut state, source) = state(ProviderErrorKind::Transport).await;
        state
            .first
            .as_mut()
            .unwrap()
            .handshake
            .as_mut()
            .unwrap()
            .path = "/../other".into();
        let state = obtain_credential(state).await;
        assert!(
            matches!(state.pending.back(), Some(StreamEvent::Finished(Outcome::Failed(error))) if error.kind == ProviderErrorKind::Protocol)
        );
        assert_eq!(source.0.load(std::sync::atomic::Ordering::SeqCst), 0);
    }

    /// A proxy authority header and the connector's own handshake fields are refused
    /// while credential access is still ahead: the failure is `Protocol` and the source
    /// is never read.
    #[tokio::test]
    async fn forbidden_head_fields_fail_before_accessing_credentials() {
        for name in [
            "X-Forwarded-Host",
            "X-Forwarded-Server",
            "X-Original-Host",
            "Sec-WebSocket-Key",
            "Sec-WebSocket-Version",
            "Sec-WebSocket-Extensions",
        ] {
            let (mut state, source) = state(ProviderErrorKind::Transport).await;
            state
                .first
                .as_mut()
                .unwrap()
                .handshake
                .as_mut()
                .unwrap()
                .headers
                .push((name.to_string(), "guest".into()));
            let state = obtain_credential(state).await;
            assert!(
                matches!(state.pending.back(), Some(StreamEvent::Finished(Outcome::Failed(error))) if error.kind == ProviderErrorKind::Protocol),
                "{name}"
            );
            assert_eq!(
                source.0.load(std::sync::atomic::Ordering::SeqCst),
                0,
                "{name}: no credential is read"
            );
        }
    }

    #[tokio::test]
    async fn retry_lower_wait_is_cancelled_while_guest_never_answers() {
        let cancel = CancellationToken::new();
        let other = cancel.clone();
        let (started, ready) = tokio::sync::oneshot::channel();
        let mut started = Some(started);
        let mut lower: WsLower = Box::new(move |_| {
            let started = started.take().expect("only one retry");
            Box::pin(async move {
                let _ = started.send(());
                std::future::pending::<Result<WsLowered, ProviderError>>().await
            })
        });
        let task =
            tokio::spawn(async move { race(&other, lower(ConnectionState::default())).await });
        ready.await.unwrap();
        cancel.cancel();
        assert!(matches!(task.await.unwrap(), Raced::Cancelled));
    }

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

    /// A request whose token is already cancelled settles as `Cancelled` even when the
    /// session's admission queue is full; the immediate busy result must not outrank
    /// cancellation.
    #[tokio::test]
    async fn a_cancelled_wait_is_cancelled_even_when_the_lease_queue_is_full() {
        use crate::testing::ScriptedWsConnector;
        use crate::ws_session::{Clock, MAX_LEASE_WAITERS};
        use std::future::Future;
        use std::task::{Context, Poll};
        let session = WsSession::new(
            Arc::new(ScriptedWsConnector::new(vec![])),
            Arc::new(std::time::Instant::now) as Clock,
        );
        let held = session.lease().await;
        let waker = futures_util::task::noop_waker();
        let mut context = Context::from_waker(&waker);
        let mut queued = Vec::new();
        for _ in 0..MAX_LEASE_WAITERS {
            let mut future = Box::pin(session.lease_bounded());
            assert!(matches!(future.as_mut().poll(&mut context), Poll::Pending));
            queued.push(future);
        }
        assert!(session.lease_bounded().await.is_none(), "queue full");

        let cancel = CancellationToken::new();
        cancel.cancel();
        let stream = match ws_lease(&session, &cancel).await {
            Ok(_) => panic!("a cancelled request is never leased"),
            Err(stream) => stream,
        };
        let events = stream.collect::<Vec<_>>().await;
        assert!(
            matches!(
                events.as_slice(),
                [StreamEvent::Finished(Outcome::Cancelled)]
            ),
            "{events:?}"
        );

        drop(queued);
        drop(held);
    }

    /// A handshake that exceeds the connector's capacity is terminal before output:
    /// the component driver must not spend the transient budget or fall back to HTTP.
    #[tokio::test]
    async fn capacity_handshake_failure_is_terminal_before_output() {
        use crate::broker::{CredentialScheme, CredentialUse};
        use crate::testing::{ScriptedConnection, ScriptedTransport, ScriptedWsConnector};
        use crate::ws_session::{Clock, WsHead};
        let connector = Arc::new(ScriptedWsConnector::new(vec![
            ScriptedConnection::capacity(),
        ]));
        let session = WsSession::new(
            connector.clone(),
            Arc::new(std::time::Instant::now) as Clock,
        );
        let source = Arc::new(Source(std::sync::atomic::AtomicUsize::new(0)));
        let send = WsSend {
            handshake: Some(WsHead {
                path: "/responses".into(),
                headers: vec![],
                credential: CredentialUse {
                    scheme: CredentialScheme::Bearer,
                    account_id_header: None,
                },
            }),
            frame: "{}".into(),
        };
        let authority: Arc<dyn CredentialSource> = source.clone();
        let transport = Arc::new(ScriptedTransport::new(vec![]));
        let recorded = Arc::clone(&transport);
        let parser: Arc<dyn Fn() -> Box<dyn ResponseParser> + Send + Sync> =
            Arc::new(|| Box::new(KindParser(ProviderErrorKind::Transport)));
        let request = WsDriveRequest {
            lease: session.lease().await,
            send: send.clone(),
            lower: Box::new(move |_| {
                let send = send.clone();
                Box::pin(async move { Ok(WsLowered::WebSocket(send)) })
            }),
            authority: Box::new(move |_| {
                RouteAuthority::new("https://provider.test/v1", authority.clone()).unwrap()
            }),
            transport,
            new_parser: parser,
            retry: RetryPolicy::default(),
            cancel: CancellationToken::new(),
        };
        let events = ws_drive(request).collect::<Vec<_>>().await;
        assert!(
            matches!(events.as_slice(), [StreamEvent::Finished(Outcome::Failed(error))] if error.kind == ProviderErrorKind::Protocol),
            "{events:?}"
        );
        assert_eq!(connector.handshakes().len(), 1, "no transient retry");
        assert_eq!(recorded.requests().len(), 0, "no HTTP fallback");
    }
}
