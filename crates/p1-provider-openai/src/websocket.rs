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
//! rate limit, one reconnect for each of the three "once" rows, a reconnect inside
//! the adapter's retry budget — waiting the same backoff `drive` waits — for a
//! transient failure, and a fallback to today's SSE path, which also turns
//! WebSocket off for this provider instance. After model-visible output every
//! failure is an ordinary `Transport` failure of that response. Every row of that
//! table has a named test in `tests/websocket.rs`.
//!
//! §6 (continuation, stage C): the connection also remembers the response it
//! completed last, and the next request whose body continues that response is sent
//! with `previous_response_id` and only the new items. [`request_frame`] is the
//! single place that decision lands, and [`Memory`] is everything it reads. The
//! memory lives in the connection itself, so §4's rule — every drop, every
//! reconnect, every fallback — clears it without a second bookkeeping path.

use std::collections::VecDeque;
use std::future::Future;
use std::sync::{Arc, MutexGuard};
use std::time::Duration;

use futures_util::StreamExt;
use futures_util::future::{Either, select};
use futures_util::stream::unfold;
use p1_contracts::{
    CancellationToken, Outcome, ProviderError, ProviderErrorKind, ProviderStream, StreamEvent,
};
use p1_provider_http::ws::{WsBound, WsConnector, WsNext};
use p1_provider_http::ws_session::{
    self, WsAuthority, WsHead, WsLease, WsRead, WsSend, WsSendError, WsSession,
};
use p1_provider_http::{
    Credential, CredentialScheme, CredentialSource, CredentialUse, ResponseParser, RetryPolicy,
    SseEvent, proxy_refusal_message,
};
use serde_json::Value;

use crate::ResponsesAccount;
use crate::parser::CodexResponseParser;
use crate::request::{build_ws_headers, build_ws_headers_without_credential, ws_frame};
use crate::websocket_lower::{
    ConnectionState, Lowered, ResponseFacts, WebSocketDecisions, WebSocketHead, WebSocketSend,
};

/// The clock the connection-reuse policy reads (§4). Injected, so a test advances
/// time instead of sleeping (AGENTS.md forbids sleep-based timing assertions).
pub use p1_provider_http::ws_session::Clock;

/// The `<reason>` of a fallback notice when no HTTP status refused the upgrade:
/// a connect failure, a timeout, or a connection that broke before any output.
const NO_CONNECTION: &str = "connection failed";

/// The notice a fallback emits (ADR-0048), and the ONE place its wording lives:
/// display-only, the adapter's own constant, never history, journal or model input.
fn fallback_notice(reason: &str) -> StreamEvent {
    StreamEvent::Notice {
        text: format!(
            "transport: WebSocket unavailable ({reason}) — using HTTP (SSE) for the rest of this session"
        ),
    }
}

/// The WebSocket half of one provider instance: the host's session (the one
/// connection) and the portable decisions (framing, continuation, the fallback
/// switch §5 turns off for good).
pub(crate) struct WebSocket {
    session: WsSession,
    /// §5's transient row waits this policy's backoff, up to its `max_retries`: the
    /// adapter's default policy — 3 retries, 2 s doubling, so the same backoff the
    /// SSE driver waits.
    retry: RetryPolicy,
    /// The component-side state: whether this instance fell back, and §6's memory.
    /// Only the request holding the session's lease lowers, so the lock is never
    /// contended across an await.
    decisions: std::sync::Mutex<WebSocketDecisions>,
}

impl WebSocket {
    pub(crate) fn new(connector: Arc<dyn WsConnector>, clock: Clock) -> Self {
        Self {
            session: WsSession::new(connector, clock),
            retry: RetryPolicy::default(),
            decisions: std::sync::Mutex::new(WebSocketDecisions::new(ws_frame)),
        }
    }

    /// Whether a fallback has already turned WebSocket off for this instance.
    pub(crate) fn is_disabled(&self) -> bool {
        self.decisions().is_turned_off()
    }

    /// Lease the session WITHOUT waiting. `None` means another request holds it,
    /// and §4 is explicit that such a request uses SSE: never a second socket,
    /// never a wait.
    pub(crate) fn try_take(self: &Arc<Self>) -> Option<WsLease> {
        self.session.try_lease()
    }

    fn decisions(&self) -> MutexGuard<'_, WebSocketDecisions> {
        self.decisions
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// Everything one WebSocket request needs. The session lease is NOT here: it is
/// taken before the request starts and owned by the stream this module returns.
pub(crate) struct WebSocketRequest {
    pub(crate) ws: Arc<WebSocket>,
    /// The resolved HTTPS endpoint; the handshake swaps its scheme.
    pub(crate) url: String,
    /// The body the SSE path would send; the decisions frame it, in full or as a
    /// continuation (§6).
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
/// returned stream is hand-written, like `drive`'s: dropping it drops the lease,
/// the connection and the in-flight read with it, which is exactly what §4 calls a
/// cancellation.
pub(crate) fn stream(request: WebSocketRequest, lease: WsLease) -> ProviderStream {
    let stream = unfold(State::new(request, lease), |state| async move {
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
    /// Lower this attempt from the session's connection state: HTTP (the fallback),
    /// or a send with or without a handshake.
    Lower,
    /// Hand this attempt's send to the session: open a connection when the send
    /// has a head, then write the ONE frame.
    Send { send: WsSend },
    /// Read frames until the parser's terminal event.
    Read,
    /// Wait out the backoff of §5's transient row, racing cancellation.
    Wait { delay: Duration },
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
    /// What THIS attempt's stream has reported for §6: its response id and its
    /// re-encodable output items. Reset with the parser on every attempt.
    facts: ResponseFacts,
    /// The session lease this request owns for its whole life.
    lease: WsLease,
    /// Whether this attempt has seen no frame yet: the state §5's
    /// "a reused socket closes before its first frame" is about.
    awaiting_first_frame: bool,
    /// Whether any content delta (text/reasoning/tool input) has been forwarded.
    /// Once true, every failure is this response's own failure: no retry, no
    /// fallback (§5).
    visible: bool,
    /// §5's transient row: the reconnects (each after the retry policy's backoff)
    /// this request has already used.
    transient_retries: u32,
    /// §5's three "once" rows, each of which reconnects at most once per request.
    once: OnceRows,
    /// Whether the one forced credential refresh has been used.
    refreshed: bool,
    /// The `<reason>` of the fallback notice, set when §5 allows this request no
    /// further WebSocket attempt.
    fallback_reason: Option<String>,
    /// The fallback stream, built the first time `Phase::Fallback` is reached.
    sse: Option<ProviderStream>,
}

/// §5's three "once" rows: each of them reconnects at most ONCE per request,
/// independently of the transient budget ("Reconnects per request: one for each of
/// the three 'once' rows, and up to `max_retries` for the transient row").
#[derive(Default)]
struct OnceRows {
    /// "A reused socket closes before its first frame".
    reused_close: bool,
    /// The two error events that say the CONNECTION, not the request, cannot carry
    /// this response. Slotted by [`ReconnectRow::slot`].
    error_event: [bool; 2],
}

/// The two error events §5 reconnects for, each an "once" row of its own.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReconnectRow {
    PreviousResponseNotFound,
    ConnectionLimitReached,
}

impl ReconnectRow {
    /// The slot this row's once-flag lives in.
    fn slot(self) -> usize {
        match self {
            Self::PreviousResponseNotFound => 0,
            Self::ConnectionLimitReached => 1,
        }
    }
}

impl State {
    fn new(request: WebSocketRequest, lease: WsLease) -> Self {
        let parser = new_parser(&request);
        Self {
            request,
            phase: Phase::Credential,
            pending: VecDeque::new(),
            credential: None,
            parser,
            facts: ResponseFacts::default(),
            lease,
            awaiting_first_frame: false,
            visible: false,
            transient_retries: 0,
            once: OnceRows::default(),
            refreshed: false,
            fallback_reason: None,
            sse: None,
        }
    }

    /// Queue the single terminal event and stop. Every path here reaches it
    /// without the connection: a connection goes back to the session only through
    /// [`State::terminal`], and only for a clean completion.
    fn finish(mut self, outcome: Outcome) -> Self {
        self.lease.drop_connection();
        self.pending.push_back(StreamEvent::Finished(outcome));
        self.phase = Phase::Done;
        self
    }

    /// The end of a response: a completed one returns the connection to the
    /// session (§4) with its response id, and the decisions remember what it just
    /// answered (§6); every other ending drops the connection.
    fn terminal(mut self, outcome: Outcome) -> Self {
        if matches!(outcome, Outcome::Completed(_)) {
            let response_id = self.facts.response_id().map(str::to_string);
            self.lease.completed(response_id);
            self.request
                .ws
                .decisions()
                .completed(&self.request.body, &self.facts);
        }
        self.finish(outcome)
    }

    /// §5's "once" rows: drop the connection — a socket we have read from is never
    /// reused — and lower again at once: no connection is open, so the send opens a
    /// new one with the FULL body. The caller has already spent that row's one
    /// allowance.
    fn reconnect(mut self) -> Self {
        self.restart();
        self.phase = Phase::Lower;
        self
    }

    /// §5's transient row: a connect error or timeout, or a read/send error or a
    /// close before any model-visible output, which is not one of the "once" rows.
    /// Reconnect with the FULL body after the retry policy's backoff, inside
    /// `max_retries`; the budget spent, fall back to SSE.
    fn transient(mut self) -> Self {
        if self.transient_retries >= self.request.ws.retry.max_retries {
            return self.fall_back(NO_CONNECTION);
        }
        self.transient_retries += 1;
        let delay = self.request.ws.retry.delay(self.transient_retries, None);
        // Stream rule 4: a back-off yields `Activity`, so the consumer sees life
        // before the first content event of the next attempt — `drive` does the
        // same on its own transient path.
        self.pending.push_back(StreamEvent::Activity);
        self.restart();
        self.phase = Phase::Wait { delay };
        self
    }

    /// Drop the connection and everything that belonged to this attempt, so the
    /// next one starts from the beginning of the response on a NEW socket (§4:
    /// a half-read socket is never reused).
    fn restart(&mut self) {
        self.lease.drop_connection();
        self.parser = new_parser(&self.request);
        self.facts = ResponseFacts::default();
        self.awaiting_first_frame = false;
    }

    /// §5 allows this request no further WebSocket attempt: the session reports the
    /// failure before output, and the next lowering is the decisions' — which is
    /// HTTP, running today's `drive()` path for THIS request and turning WebSocket
    /// off for this provider instance until the process ends.
    fn fall_back(mut self, reason: &str) -> Self {
        self.lease.fail_before_output();
        self.fallback_reason = Some(reason.to_string());
        self.phase = Phase::Lower;
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
            Phase::Lower => lower(state),
            Phase::Send { send } => send_frame(state, send).await,
            Phase::Read => read(state).await,
            Phase::Wait { delay } => wait(state, delay).await,
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
            state.phase = Phase::Lower;
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
            state.phase = Phase::Lower;
            state
        }
        Raced::Done(Err(error)) => state.finish(Outcome::Failed(error)),
    }
}

/// WIT `lower(request, connection-state)`: the session reports its facts, the
/// portable decisions choose. The fallback is announced here, ONCE, before the SSE
/// request is even started (ADR-0048), so the notice precedes the response's first
/// event. A disabled instance never starts a WebSocket request again, so one
/// request announces at most once, and a later request, already on SSE, announces
/// nothing.
fn lower(mut state: State) -> State {
    let connection = connection_state(state.lease.state());
    let head = match handshake_head(&state.request) {
        Ok(head) => head,
        Err(error) => return state.finish(Outcome::Failed(error)),
    };
    let lowered = state
        .request
        .ws
        .decisions()
        .lower(&state.request.body, head, &connection);
    match lowered {
        Lowered::Http => {
            state.lease.drop_connection();
            let reason = state
                .fallback_reason
                .take()
                .unwrap_or_else(|| NO_CONNECTION.to_string());
            state.pending.push_back(fallback_notice(&reason));
            state.phase = Phase::Fallback;
        }
        Lowered::WebSocket(send) => {
            state.phase = Phase::Send {
                send: host_send(send),
            };
        }
    }
    state
}

async fn send_frame(mut state: State, send: WsSend) -> State {
    // Sending always precedes any output of this request, so §5's "after visible
    // output" rule cannot apply here. A route whose credential an egress proxy
    // injects (issue #134) attaches no credential at all.
    let credential = if state.request.credentials.proxy_injected() {
        None
    } else {
        state.credential.as_ref()
    };
    let authority = WsAuthority {
        endpoint: &state.request.url,
        credential,
    };
    let sent = state
        .lease
        .send(authority, send, &state.request.cancel)
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
        // §5: a connect error or a timeout is the transient row, whatever the
        // failure class: the socket never came up, so nothing was sent.
        Err(WsSendError::ConnectFailed) => state.transient(),
        // A send that fails on a connection we reused is that socket having gone
        // away before our first frame: §5 reconnects once for it — the third
        // "once" row. Any other send failure is the transient row.
        Err(WsSendError::WriteFailed) if state.lease.reused() && !state.once.reused_close => {
            state.once.reused_close = true;
            state.reconnect()
        }
        Err(WsSendError::WriteFailed) => state.transient(),
    }
}

/// §5's upgrade-refusal policy. The classification goes through the SAME parser
/// the SSE path uses for a non-2xx answer, so the two cannot disagree about kinds;
/// the body is classification input and never reaches a message.
fn refused_upgrade(mut state: State, status: u16, body: &[u8]) -> State {
    let error = state.parser.on_http_error(status, &[], body);
    match status {
        401 | 403 => {
            // Issue #134: a route whose credential an egress proxy injects sends no
            // credential at all, so there is nothing p1 could refresh here — the
            // refusal is the proxy's, reported with the SSE driver's own message so
            // the two transports cannot drift apart. This never enters
            // `Phase::Refresh`.
            if state.request.credentials.proxy_injected() {
                let error = ProviderError::new(
                    ProviderErrorKind::Authentication,
                    proxy_refusal_message(status),
                );
                return state.finish(Outcome::Failed(error));
            }
            if state.refreshed {
                // §5: refused again after the one refresh is an authentication
                // failure — the same shape `drive` produces for a second 401/403.
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
        // §5: a rate limit is not worth falling back for — SSE would hit the same
        // limit, and the caller should see the limit rather than a retry storm.
        429 => state.finish(Outcome::Failed(error)),
        // Any other refusal says nothing about SSE: fall back to it.
        _ => state.fall_back(&format!("HTTP {status}")),
    }
}

async fn read(mut state: State) -> State {
    // §4: the read is bounded INSIDE the connection, where the message loop sees
    // every frame — so ANY message, a control ping included, resets the idle clock
    // (issue #164). Here we only classify the outcome.
    let received = state.lease.next(&state.request.cancel).await;
    match received {
        WsRead::Cancelled => state.finish(Outcome::Cancelled),
        WsRead::Next(WsNext::Timeout(bound)) => {
            let error = ProviderError::new(ProviderErrorKind::Transport, bound.message());
            state.on_read_timeout(bound, error)
        }
        WsRead::Next(WsNext::Text(text)) => state.on_frame(&text),
        WsRead::Next(WsNext::Closed) | WsRead::Failed(_) => state.on_close(),
    }
}

impl State {
    /// One received text is ONE JSON event, fed to the existing parser with no
    /// event name (§3).
    fn on_frame(mut self, text: &str) -> State {
        // §5: the two error events that say "this connection cannot carry this
        // request" reconnect once each and resend the FULL body. After output they
        // are ordinary failures.
        if !self.visible
            && let Some(row) = reconnect_row(text)
            && !self.once.error_event[row.slot()]
        {
            self.once.error_event[row.slot()] = true;
            return self.reconnect();
        }
        self.awaiting_first_frame = false;
        self.facts.record(text);
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
        if self.awaiting_first_frame
            && self.lease.reused()
            && !self.visible
            && !self.once.reused_close
        {
            self.once.reused_close = true;
            return self.reconnect();
        }
        let outcome = self.parser.on_end();
        if self.visible {
            // §5: after model-visible output, a broken stream is an ordinary
            // Transport failure of that response: no retry, no fallback.
            self.terminal(outcome)
        } else {
            // §5's last row: a read error or a close before any output is the
            // transient row — reconnect inside the retry budget, then fall back.
            self.transient()
        }
    }

    /// A read whose bound expired (the connection reports it as
    /// [`WsNext::Timeout`]). A FIRST-FRAME expiry on a REUSED connection is §5's
    /// third "once" row — the same dead-slot case `on_close` handles for a reused
    /// socket that closes before its first frame: the connection was already stale
    /// when this request picked it up, so reconnect once instead of spending a retry
    /// byte, waiting the backoff, and risking the SSE fallback on a healthy turn.
    ///
    /// Otherwise, before any output it is §5's transient row — the reconnect budget,
    /// then the SSE fallback. After output it is this response's own `Transport`
    /// failure, whose message names the bound that expired.
    fn on_read_timeout(mut self, bound: WsBound, error: ProviderError) -> State {
        if bound == WsBound::FirstFrame
            && self.lease.reused()
            && !self.visible
            && !self.once.reused_close
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

/// Wait out §5's transient-row backoff, racing cancellation. The same wait the SSE
/// driver performs (`tokio::time::sleep`), so a test drives it on the paused clock
/// and no test ever sleeps for real.
async fn wait(mut state: State, delay: Duration) -> State {
    let cancel = state.request.cancel.clone();
    match race(&cancel, tokio::time::sleep(delay)).await {
        Raced::Cancelled => state.finish(Outcome::Cancelled),
        Raced::Done(()) => {
            state.phase = Phase::Lower;
            state
        }
    }
}

/// Today's SSE path for this request (§5). The fallback's notice is already in the
/// queue when this is reached, so the operator sees it before the response's first
/// event (ADR-0048).
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

/// The handshake head of an attempt (`docs/design/websocket.md` §3): the resolved
/// endpoint itself, the header set §3 names without any credential, and where the
/// session attaches the credential — ahead of these headers, as `request.rs`'s
/// identity headers order them. On a route whose credential an egress proxy
/// injects (issue #134) the session attaches nothing.
fn handshake_head(request: &WebSocketRequest) -> Result<WebSocketHead, ProviderError> {
    let cache_key = request.cache_key.as_deref();
    Ok(WebSocketHead {
        path: String::new(),
        headers: build_ws_headers_without_credential(request.account, cache_key)?,
        account_id_header: account_id_header(request.account, cache_key)?,
    })
}

/// The header that carries the credential's account id, read off `request.rs`'s
/// own builders so the two cannot drift: the credential headers are exactly what
/// [`build_ws_headers`] puts ahead of [`build_ws_headers_without_credential`] —
/// `Authorization`, then the account id for an account that needs one. The probe
/// credential is empty; no credential is read here.
fn account_id_header(
    account: ResponsesAccount,
    cache_key: Option<&str>,
) -> Result<Option<String>, ProviderError> {
    let probe = Credential {
        bearer: String::new(),
        account_id: Some(String::new()),
    };
    let with = build_ws_headers(account, &probe, cache_key)?;
    let without = build_ws_headers_without_credential(account, cache_key)?;
    let credential_headers = with.len().saturating_sub(without.len());
    Ok(with[..credential_headers]
        .iter()
        .map(|(name, _)| name)
        .find(|name| !name.eq_ignore_ascii_case("authorization"))
        .cloned())
}

/// The session's facts, as the portable decisions read them.
fn connection_state(state: ws_session::ConnectionState) -> ConnectionState {
    ConnectionState {
        open: state.open,
        last_clean_response: state.last_clean_response,
        failed_before_output: state.failed_before_output,
    }
}

/// The decisions' send, as the session executes it.
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

/// The "once" row a frame names, if any: the two error events §5 reconnects for.
/// The parser stays the authority for every other event; this only asks whether the
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

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::websocket_lower::{EchoedItem, Memory, echoed_item};

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

    /// §6's memory is only ever read by the rules; its `Debug` is for a trace, so it
    /// carries lengths and the response id and never one word of the conversation.
    #[test]
    fn remembered_state_debug_is_lengths_and_the_response_id_only() {
        let memory = Memory {
            body: json!({
                "instructions": "SENTINEL-INSTRUCTIONS",
                "input": [
                    { "type": "message", "role": "user",
                      "content": [{ "type": "input_text", "text": "SENTINEL-TEXT" }] },
                    { "type": "message", "role": "user",
                      "content": [{ "type": "input_text", "text": "SENTINEL-TEXT-2" }] },
                ],
            }),
            response_id: "resp_1".to_string(),
            items: vec![EchoedItem {
                kind: "message",
                role: Some("assistant"),
                id: Some("msg_1".to_string()),
                call_id: None,
            }],
        };
        let text = format!("{memory:?}");
        assert!(!text.contains("SENTINEL"), "{text}");
        for field in ["input_items: 2", "output_items: 1", "resp_1"] {
            assert!(text.contains(field), "{field} missing from {text}");
        }
    }

    /// Rule 3's echo is made of the items the next request WOULD carry: the four
    /// item types the parser turns into blocks, minus the two the request builder
    /// drops again (an empty assistant message, reasoning with no encrypted content).
    #[test]
    fn only_items_the_next_request_re_encodes_are_echoed() {
        let echo = |text: &str| {
            let item: Value = serde_json::from_str(text).unwrap();
            echoed_item(&item).map(|echoed| {
                (
                    echoed.kind,
                    echoed.role,
                    echoed.call_id.clone(),
                    echoed.answers(&item),
                )
            })
        };
        assert_eq!(
            echo(
                r#"{"type":"message","role":"assistant","content":[{"type":"output_text","text":"ok"}]}"#
            ),
            Some(("message", Some("assistant"), None, true)),
            "a message with text comes back as an assistant message"
        );
        assert_eq!(
            echo(r#"{"type":"function_call","call_id":"call_1","name":"read","arguments":"{}"}"#),
            Some(("function_call", None, Some("call_1".to_string()), true))
        );
        assert_eq!(
            echo(r#"{"type":"function_call","id":"fc_1","name":"read","arguments":"{}"}"#),
            Some(("function_call", None, Some("fc_1".to_string()), true)),
            "without a call_id the wire's id is the call id, as in the parser"
        );
        assert_eq!(
            echo(r#"{"type":"custom_tool_call","call_id":"c","name":"patch","input":"***"}"#),
            Some(("custom_tool_call", None, Some("c".to_string()), true))
        );
        assert_eq!(
            echo(r#"{"type":"reasoning","encrypted_content":"enc-1","summary":[]}"#),
            Some(("reasoning", None, None, true))
        );
        for dropped in [
            r#"{"type":"message","role":"assistant","content":[]}"#,
            r#"{"type":"message","role":"assistant","content":[{"type":"output_text","text":""}]}"#,
            r#"{"type":"reasoning","summary":[{"type":"summary_text","text":"s"}]}"#,
            r#"{"type":"reasoning","encrypted_content":""}"#,
            r#"{"type":"web_search_call","id":"ws_1"}"#,
        ] {
            assert_eq!(echo(dropped), None, "{dropped} is not in the next input");
        }
    }

    /// An item only answers the fingerprint if the fields the next request
    /// re-encodes come back; the item `id` comes back only when a client sends one.
    #[test]
    fn a_fingerprint_requires_the_fields_the_encoding_carries() {
        let item: Value =
            serde_json::from_str(r#"{"type":"function_call","call_id":"call_1","name":"read"}"#)
                .unwrap();
        let fingerprint = echoed_item(&item).unwrap();
        assert!(fingerprint.answers(&item));
        for wrong in [
            r#"{"type":"message","call_id":"call_1"}"#,
            r#"{"type":"function_call","call_id":"call_2"}"#,
            r#"{"type":"function_call"}"#,
            r#"{"type":"function_call","call_id":"call_1","id":"fc_1"}"#,
        ] {
            assert!(
                !fingerprint.answers(&serde_json::from_str::<Value>(wrong).unwrap()),
                "{wrong}"
            );
        }
    }
}
