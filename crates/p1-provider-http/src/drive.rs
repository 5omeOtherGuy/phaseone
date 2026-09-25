//! The shared retrying request driver.
//!
//! Both provider adapters translate their wire format into a [`ResponseParser`]
//! and call [`drive`]; everything policy-shaped (credential refresh, the shared
//! transient budget, backoff, cancellation) lives here. [`drive`] never returns
//! `Err`: the returned stream always ends with exactly one terminal
//! [`StreamEvent::Finished`].
//!
//! The driver does not spawn a task. It is a hand-written state machine wrapped
//! in `futures_util::stream::unfold`, so dropping the returned stream drops the
//! in-flight request, the body read and the backoff sleep with it. A spawned task
//! would need a channel and an explicit shutdown path for the same guarantee.

use std::collections::VecDeque;
use std::future::Future;
use std::time::Duration;

use futures_util::StreamExt;
use futures_util::future::{Either, select};
use p1_contracts::{
    BoxFuture, CancellationToken, Outcome, ProviderError, ProviderErrorKind, ProviderStream,
    StreamEvent,
};

use crate::credential::{Credential, CredentialSource};
use crate::http::{
    ByteStream, FIRST_BYTE_TIMEOUT, HttpRequest, HttpResponse, STREAM_IDLE_TIMEOUT, Transport,
    TransportError, first_byte_timeout_message, stream_idle_timeout_message,
};
use crate::retry::{HttpClass, RetryPolicy, classify_status, retry_after};
use crate::sse::{SseDecoder, SseEvent};

/// Turns route-native SSE events into contract stream events. Pure and
/// synchronous, so it can be unit-tested without a transport.
pub trait ResponseParser: Send {
    /// Feed one SSE event. Returned events are forwarded in order. A returned
    /// `StreamEvent::Finished` ends the stream.
    fn on_event(&mut self, event: SseEvent) -> Vec<StreamEvent>;

    /// The body ended. Return the terminal outcome (normally a Transport failure
    /// "stream ended without a terminal event" unless the parser already
    /// finished).
    fn on_end(&mut self) -> Outcome;

    /// Map a non-2xx response to an error. `body` is for CLASSIFICATION ONLY
    /// (e.g. spotting a context-window error type) and must never be copied into
    /// the message.
    fn on_http_error(
        &self,
        status: u16,
        headers: &[(String, String)],
        body: &[u8],
    ) -> ProviderError;
}

/// Everything [`drive`] needs for one request, including how to rebuild it for a
/// refreshed credential.
pub struct DriveRequest {
    pub transport: std::sync::Arc<dyn Transport>,
    pub credentials: std::sync::Arc<dyn CredentialSource>,
    /// Builds the HTTP request for a credential (called again after a refresh).
    pub build: Box<dyn Fn(&Credential) -> HttpRequest + Send + Sync>,
    /// A fresh parser per attempt.
    pub new_parser: Box<dyn Fn() -> Box<dyn ResponseParser> + Send + Sync>,
    pub retry: RetryPolicy,
    pub cancel: CancellationToken,
}

/// Drive one request to a terminal event. The returned stream obeys the five
/// stream rules in `p1-contracts/src/provider.rs`.
pub fn drive(request: DriveRequest) -> ProviderStream {
    let stream = futures_util::stream::unfold(State::new(request), |state| async move {
        let (event, state) = step(state).await;
        event.map(|event| (event, state))
    })
    // `unfold` panics if polled after it has ended; `fuse` makes the terminal
    // poll idempotent, as rule 1's "nothing after Finished" requires.
    .fuse();
    Box::pin(stream)
}

/// What the driver is waiting on next.
enum Phase {
    /// Fetch the credential for the next attempt.
    NeedCredential,
    /// Build and send the request.
    Post,
    /// The request is in flight: await the response under the first-byte bound,
    /// after telling the operator once that the provider has not answered yet. The
    /// in-flight future is held here so a poll can yield the note and resume the
    /// same request.
    Posting {
        parser: Box<dyn ResponseParser>,
        post: BoxFuture<'static, Result<HttpResponse, TransportError>>,
        /// Whether the "still waiting" notice has already been emitted.
        notified: bool,
        /// When the first-byte bound expires.
        deadline: tokio::time::Instant,
    },
    /// Read the streaming body of a 2xx response.
    Read {
        parser: Box<dyn ResponseParser>,
        decoder: SseDecoder,
        body: ByteStream,
    },
    /// Wait out a backoff, racing cancellation.
    Wait { delay: Duration },
    /// The terminal event has been queued; the next poll ends the stream.
    Done,
}

struct State {
    request: DriveRequest,
    phase: Phase,
    /// Events produced by the current step, forwarded one per poll in order.
    pending: VecDeque<StreamEvent>,
    credential: Option<Credential>,
    transient_retries: u32,
    reauth_used: bool,
    /// Whether any content delta (text/reasoning/tool input) has been forwarded
    /// during the current attempt. Once true, a broken body is terminal.
    visible: bool,
}

impl State {
    fn new(request: DriveRequest) -> Self {
        Self {
            request,
            phase: Phase::NeedCredential,
            pending: VecDeque::new(),
            credential: None,
            transient_retries: 0,
            reauth_used: false,
            visible: false,
        }
    }

    /// Queue the single terminal event and stop.
    fn finish(mut self, outcome: Outcome) -> Self {
        self.pending.push_back(StreamEvent::Finished(outcome));
        self.phase = Phase::Done;
        self
    }

    /// A transient failure: retry inside the shared budget, or fail. Grants no
    /// retry once visible output has been forwarded.
    fn transient_or_fail(
        mut self,
        error: ProviderError,
        hint: Option<Duration>,
        status: Option<u16>,
    ) -> Self {
        if self.transient_retries >= self.request.retry.max_retries {
            return self.finish(Outcome::Failed(error));
        }
        self.transient_retries += 1;
        let delay = self.request.retry.delay(self.transient_retries, hint);
        let failure = match status {
            Some(status) => format!("provider returned HTTP {status}"),
            None => "provider request failed".to_string(),
        };
        self.pending.push_back(StreamEvent::Notice {
            text: format!(
                "{failure}; retry {}/{} in {}",
                self.transient_retries,
                self.request.retry.max_retries,
                format_delay(delay)
            ),
        });
        self.pending.push_back(StreamEvent::Activity);
        self.phase = Phase::Wait { delay };
        self
    }
}

/// One step of the state machine: await at most one I/O operation, then return
/// the next event (or `None` when the stream is over).
async fn step(mut state: State) -> (Option<StreamEvent>, State) {
    loop {
        if let Some(event) = state.pending.pop_front() {
            return (Some(event), state);
        }
        state = match std::mem::replace(&mut state.phase, Phase::Done) {
            Phase::Done => return (None, state),
            Phase::NeedCredential => obtain_credential(state).await,
            Phase::Post => post_once(state).await,
            Phase::Posting {
                parser,
                post,
                notified,
                deadline,
            } => await_post(state, parser, post, notified, deadline).await,
            Phase::Read {
                parser,
                decoder,
                body,
            } => read_body(state, parser, decoder, body).await,
            Phase::Wait { delay } => wait(state, delay).await,
        };
    }
}

async fn obtain_credential(mut state: State) -> State {
    if state.request.cancel.is_cancelled() {
        return state.finish(Outcome::Cancelled);
    }
    let credentials = state.request.credentials.clone();
    let cancel = state.request.cancel.clone();
    match race(cancel, credentials.access()).await {
        Raced::Cancelled => state.finish(Outcome::Cancelled),
        Raced::Done(Ok(credential)) => {
            state.credential = Some(credential);
            state.phase = Phase::Post;
            state
        }
        Raced::Done(Err(error)) => state.finish(Outcome::Failed(error)),
    }
}

/// Build the attempt's request and start the post. The in-flight future owns a
/// clone of the transport, so the state machine can hold it across the "still
/// waiting" note without borrowing `state`.
async fn post_once(mut state: State) -> State {
    if state.request.cancel.is_cancelled() {
        return state.finish(Outcome::Cancelled);
    }
    let parser = (state.request.new_parser)();
    let credential = state
        .credential
        .clone()
        .expect("a credential is obtained before the first attempt");
    let request = (state.request.build)(&credential);
    let transport = state.request.transport.clone();
    let post: BoxFuture<'static, Result<HttpResponse, TransportError>> =
        Box::pin(async move { transport.post(request).await });
    state.phase = Phase::Posting {
        parser,
        post,
        notified: false,
        deadline: tokio::time::Instant::now() + FIRST_BYTE_TIMEOUT,
    };
    state
}

/// Await the in-flight post. It ends on the response, on cancellation, on the
/// first-byte bound, or — once — on the grace timer that tells the operator the
/// provider has not answered yet; the last case yields a `Notice` and keeps the
/// same request in flight.
async fn await_post(
    mut state: State,
    parser: Box<dyn ResponseParser>,
    mut post: BoxFuture<'static, Result<HttpResponse, TransportError>>,
    notified: bool,
    deadline: tokio::time::Instant,
) -> State {
    let cancel = state.request.cancel.clone();
    let now = tokio::time::Instant::now();
    // The note fires once, after the grace delay; after that only the bound remains.
    let wake = if notified {
        deadline
    } else {
        (now + WAITING_NOTE_AFTER).min(deadline)
    };
    // No pre-check on the deadline before the wait: the timed wait polls the post
    // first, so a response that already arrived wins even when this call resumes
    // after the grace yield and the bound has passed (the deadline only surfaces in
    // the `Err(_elapsed)` arm below, where nothing arrived).
    let outcome = race(cancel, tokio::time::timeout(wake - now, post.as_mut())).await;
    match outcome {
        Raced::Cancelled => state.finish(Outcome::Cancelled),
        Raced::Done(Ok(Err(error))) => {
            let error = ProviderError::new(
                ProviderErrorKind::Transport,
                format!("request failed: {}", error.0),
            );
            state.transient_or_fail(error, None, None)
        }
        Raced::Done(Ok(Ok(response))) => on_response(state, parser, response).await,
        Raced::Done(Err(_elapsed)) => {
            if tokio::time::Instant::now() >= deadline {
                first_byte_timeout(state)
            } else {
                state.pending.push_back(StreamEvent::Notice {
                    text: format!(
                        "waiting for the provider ({} s)",
                        WAITING_NOTE_AFTER.as_secs()
                    ),
                });
                state.phase = Phase::Posting {
                    parser,
                    post,
                    notified: true,
                    deadline,
                };
                state
            }
        }
    }
}

/// The first-byte bound expired: the provider never answered, as a named Transport
/// failure the retry policy owns.
fn first_byte_timeout(state: State) -> State {
    let error = ProviderError::new(ProviderErrorKind::Transport, first_byte_timeout_message());
    state.transient_or_fail(error, None, None)
}

/// Apply the status policy to one response, exactly as the driver always has.
async fn on_response(
    mut state: State,
    parser: Box<dyn ResponseParser>,
    response: HttpResponse,
) -> State {
    let status = response.status;
    let headers = response.headers;
    let cancel = state.request.cancel.clone();
    match classify_status(status) {
        HttpClass::Success => {
            state.visible = false;
            state.phase = Phase::Read {
                parser,
                decoder: SseDecoder::new(),
                body: response.body,
            };
            state
        }
        HttpClass::Fatal => {
            let body = match drain_body(&cancel, response.body).await {
                Raced::Cancelled => return state.finish(Outcome::Cancelled),
                Raced::Done(bytes) => bytes,
            };
            let error = parser.on_http_error(status, &headers, &body);
            state.finish(Outcome::Failed(error))
        }
        HttpClass::Retry => {
            let body = match drain_body(&cancel, response.body).await {
                Raced::Cancelled => return state.finish(Outcome::Cancelled),
                Raced::Done(bytes) => bytes,
            };
            let error = parser.on_http_error(status, &headers, &body);
            if error.kind == ProviderErrorKind::UsageLimitExhausted {
                return state.finish(Outcome::Failed(error));
            }
            state.transient_or_fail(error, retry_after(&headers), Some(status))
        }
        HttpClass::Reauth => {
            let body = match drain_body(&cancel, response.body).await {
                Raced::Cancelled => return state.finish(Outcome::Cancelled),
                Raced::Done(bytes) => bytes,
            };
            let error = parser.on_http_error(status, &headers, &body);
            if matches!(
                error.kind,
                ProviderErrorKind::InsufficientBalance | ProviderErrorKind::NotEntitled
            ) {
                // ADR-0046 / ADR-0062: an exhausted account or a plan that does
                // not allow this model is not a rejected key. Refreshing could
                // only fail, and masking the adapter's diagnosis with a refresh
                // error is the bug these kinds exist to end.
                return state.finish(Outcome::Failed(error));
            }
            if state.reauth_used {
                // Rule 1: a second 401/403 after the one refresh is terminal and
                // is always an authentication failure.
                let error = ProviderError::new(ProviderErrorKind::Authentication, error.message);
                return state.finish(Outcome::Failed(error));
            }
            state.reauth_used = true;
            let credentials = state.request.credentials.clone();
            let rejected = state
                .credential
                .clone()
                .expect("a credential is obtained before the first attempt");
            let cancel = state.request.cancel.clone();
            match race(cancel, credentials.refresh(&rejected)).await {
                Raced::Cancelled => state.finish(Outcome::Cancelled),
                Raced::Done(Ok(credential)) => {
                    state.credential = Some(credential);
                    state.phase = Phase::Post;
                    state
                }
                Raced::Done(Err(error)) => state.finish(Outcome::Failed(error)),
            }
        }
    }
}

async fn read_body(
    mut state: State,
    mut parser: Box<dyn ResponseParser>,
    mut decoder: SseDecoder,
    mut body: ByteStream,
) -> State {
    let cancel = state.request.cancel.clone();
    match next_chunk(&cancel, &mut body).await {
        Raced::Cancelled => state.finish(Outcome::Cancelled),
        // No bytes for the idle bound: the stream is silent, not slow. Any chunk —
        // including an SSE comment or ping — would have reset this clock.
        Raced::Done(Err(_elapsed)) => {
            let failure =
                ProviderError::new(ProviderErrorKind::Transport, stream_idle_timeout_message());
            if state.visible {
                // Rule 3: never retry once the consumer has seen output.
                state.finish(Outcome::Failed(failure))
            } else {
                state.transient_or_fail(failure, None, None)
            }
        }
        Raced::Done(Ok(Some(Ok(chunk)))) => {
            let events = decoder.push(&chunk);
            if let Some(outcome) = feed(&mut state, parser.as_mut(), events) {
                state.pending.push_back(StreamEvent::Finished(outcome));
                state.phase = Phase::Done;
                return state;
            }
            state.phase = Phase::Read {
                parser,
                decoder,
                body,
            };
            state
        }
        Raced::Done(Ok(Some(Err(error)))) => {
            let failure = ProviderError::new(
                ProviderErrorKind::Transport,
                format!("provider stream broke: {}", error.0),
            );
            if state.visible {
                // Rule 3: never retry once the consumer has seen output.
                state.finish(Outcome::Failed(failure))
            } else {
                state.transient_or_fail(failure, None, None)
            }
        }
        Raced::Done(Ok(None)) => {
            // A final event that lacks its trailing blank line is still an event:
            // flush it before deciding that the body ended without a terminal.
            let last = decoder.finish().into_iter().collect();
            if let Some(outcome) = feed(&mut state, parser.as_mut(), last) {
                return state.finish(outcome);
            }
            let outcome = parser.on_end();
            match outcome {
                Outcome::Failed(error) if !state.visible => {
                    state.transient_or_fail(error, None, None)
                }
                other => state.finish(other),
            }
        }
    }
}

/// Run SSE events through the parser, queueing what it yields. Returns the terminal
/// outcome if the parser finished; nothing a parser returns after its own `Finished`
/// is forwarded.
fn feed(
    state: &mut State,
    parser: &mut dyn ResponseParser,
    events: Vec<SseEvent>,
) -> Option<Outcome> {
    for event in events {
        for stream_event in parser.on_event(event) {
            if let StreamEvent::Finished(outcome) = stream_event {
                return Some(outcome);
            }
            if is_visible(&stream_event) {
                state.visible = true;
            }
            state.pending.push_back(stream_event);
        }
    }
    None
}

async fn wait(mut state: State, delay: Duration) -> State {
    let cancel = state.request.cancel.clone();
    match race(cancel, tokio::time::sleep(delay)).await {
        Raced::Cancelled => state.finish(Outcome::Cancelled),
        Raced::Done(()) => {
            state.phase = Phase::Post;
            state
        }
    }
}

/// Drain a non-2xx body for classification. Body bytes never leave `post_once`
/// except into `ResponseParser::on_http_error`.
async fn drain_body(cancel: &CancellationToken, mut body: ByteStream) -> Raced<Vec<u8>> {
    let mut collected = Vec::new();
    loop {
        match next_chunk(cancel, &mut body).await {
            Raced::Cancelled => return Raced::Cancelled,
            Raced::Done(Ok(None)) => return Raced::Done(collected),
            // A body that stops answering ends classification: the status policy
            // does not depend on the rest of an error body (issue #164).
            Raced::Done(Err(_elapsed)) => return Raced::Done(collected),
            Raced::Done(Ok(Some(Ok(chunk)))) => {
                // Classification needs the error type, not the whole body: an
                // unbounded error body must not be buffered.
                let room = ERROR_BODY_LIMIT.saturating_sub(collected.len());
                collected.extend_from_slice(&chunk[..chunk.len().min(room)]);
                if room == 0 {
                    return Raced::Done(collected);
                }
            }
            // A body error after a non-2xx status does not change the status
            // policy; classify what arrived.
            Raced::Done(Ok(Some(Err(_)))) => return Raced::Done(collected),
        }
    }
}

fn format_delay(delay: Duration) -> String {
    if delay.subsec_millis() == 0 {
        format!("{} s", delay.as_secs())
    } else {
        format!("{:.1} s", delay.as_secs_f64())
    }
}

const ERROR_BODY_LIMIT: usize = 64 * 1024;

/// How long the first-byte wait runs before it tells the operator, once, that the
/// provider has not answered yet. Issue #164 wanted a note while waiting, and 30 s
/// is late enough to be a real wait and early enough to be seen. The request is NOT
/// aborted; it still ends at [`FIRST_BYTE_TIMEOUT`].
const WAITING_NOTE_AFTER: Duration = Duration::from_secs(30);

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

/// Await `future`, but stop as soon as `cancel` fires.
async fn race<T>(cancel: CancellationToken, future: impl Future<Output = T>) -> Raced<T> {
    let cancelled = cancel.cancelled();
    let future = std::pin::pin!(future);
    let cancelled = std::pin::pin!(cancelled);
    match select(future, cancelled).await {
        Either::Left((value, _)) => Raced::Done(value),
        Either::Right(((), _)) => Raced::Cancelled,
    }
}

/// The next body chunk, bounded by the stream-idle bound and by cancellation. Any
/// chunk answered within the bound — an event, a comment or a ping — resets the
/// clock simply by completing this wait.
async fn next_chunk(
    cancel: &CancellationToken,
    body: &mut ByteStream,
) -> Raced<Result<Option<Result<Vec<u8>, TransportError>>, tokio::time::error::Elapsed>> {
    race(
        cancel.clone(),
        tokio::time::timeout(STREAM_IDLE_TIMEOUT, body.next()),
    )
    .await
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use p1_contracts::{
        AssistantBlock, AssistantItem, BoxFuture, CompletedResponse, Origin, ProviderErrorKind,
        StopReason, StreamEvent,
    };

    use super::*;
    use crate::http::{HttpRequest, HttpResponse};
    use crate::testing::{BodyEnd, ScriptedResponse, ScriptedTransport};

    fn completed_response() -> CompletedResponse {
        CompletedResponse {
            item: AssistantItem {
                origin: Origin {
                    route: "test-route".to_string(),
                    model: "test-model".to_string(),
                },
                blocks: vec![AssistantBlock::Text {
                    text: "Hello world".to_string(),
                }],
            },
            stop: StopReason::EndTurn,
            usage: None,
        }
    }

    /// A minimal adapter-side parser. It understands a small command vocabulary
    /// so the driver's policy is what is under test.
    #[derive(Default)]
    struct TestParser;

    impl ResponseParser for TestParser {
        fn on_event(&mut self, event: SseEvent) -> Vec<StreamEvent> {
            match event.data.trim() {
                "delta" => vec![StreamEvent::TextDelta {
                    block: 0,
                    text: "Hello world".to_string(),
                }],
                "activity" => vec![StreamEvent::Activity],
                "done" => vec![StreamEvent::Finished(Outcome::Completed(
                    completed_response(),
                ))],
                "finish-then-delta" => vec![
                    StreamEvent::Finished(Outcome::Completed(completed_response())),
                    StreamEvent::TextDelta {
                        block: 0,
                        text: "AFTER".to_string(),
                    },
                ],
                "after" => vec![StreamEvent::TextDelta {
                    block: 0,
                    text: "AFTER".to_string(),
                }],
                "error" => vec![StreamEvent::Finished(Outcome::Failed(ProviderError::new(
                    ProviderErrorKind::Protocol,
                    "provider error event",
                )))],
                _ => Vec::new(),
            }
        }

        fn on_end(&mut self) -> Outcome {
            Outcome::Failed(ProviderError::new(
                ProviderErrorKind::Transport,
                "stream ended without a terminal event",
            ))
        }

        fn on_http_error(
            &self,
            status: u16,
            _headers: &[(String, String)],
            body: &[u8],
        ) -> ProviderError {
            if body == b"exhausted-account" {
                return ProviderError::new(
                    ProviderErrorKind::InsufficientBalance,
                    "the account has no balance",
                );
            }
            if body == b"not-entitled" {
                return ProviderError::new(
                    ProviderErrorKind::NotEntitled,
                    "the account's plan does not allow this model on this route",
                );
            }
            if body == b"usage-limit-exhausted" {
                return ProviderError::new(
                    ProviderErrorKind::UsageLimitExhausted,
                    "the account's usage allowance is used up",
                );
            }
            let kind = match status {
                401 | 403 => ProviderErrorKind::Authentication,
                408 | 425 | 429 | 500..=599 => ProviderErrorKind::Transport,
                _ => ProviderErrorKind::InvalidRequest,
            };
            ProviderError::new(kind, format!("http status {status}"))
        }
    }

    struct ScriptedCredentials {
        initial: Credential,
        refreshed: Credential,
        refresh_calls: Mutex<Vec<Credential>>,
        access_calls: AtomicUsize,
    }

    impl ScriptedCredentials {
        fn new(initial: &str, refreshed: &str) -> Self {
            Self {
                initial: Credential {
                    bearer: initial.to_string(),
                    account_id: None,
                },
                refreshed: Credential {
                    bearer: refreshed.to_string(),
                    account_id: None,
                },
                refresh_calls: Mutex::new(Vec::new()),
                access_calls: AtomicUsize::new(0),
            }
        }
    }

    impl CredentialSource for ScriptedCredentials {
        fn access<'a>(&'a self) -> BoxFuture<'a, Result<Credential, ProviderError>> {
            Box::pin(async move {
                self.access_calls.fetch_add(1, Ordering::SeqCst);
                Ok(self.initial.clone())
            })
        }

        fn refresh<'a>(
            &'a self,
            rejected: &'a Credential,
        ) -> BoxFuture<'a, Result<Credential, ProviderError>> {
            Box::pin(async move {
                self.refresh_calls.lock().unwrap().push(rejected.clone());
                Ok(self.refreshed.clone())
            })
        }
    }

    struct Harness {
        transport: ScriptedTransport,
        credentials: Arc<ScriptedCredentials>,
        cancel: CancellationToken,
        retry: RetryPolicy,
        builds: Arc<AtomicUsize>,
    }

    impl Harness {
        fn new(responses: Vec<ScriptedResponse>) -> Self {
            Self::custom(responses, RetryPolicy::default(), "OLD", "NEW")
        }

        fn custom(
            responses: Vec<ScriptedResponse>,
            retry: RetryPolicy,
            initial: &str,
            refreshed: &str,
        ) -> Self {
            Self {
                transport: ScriptedTransport::new(responses),
                credentials: Arc::new(ScriptedCredentials::new(initial, refreshed)),
                cancel: CancellationToken::new(),
                retry,
                builds: Arc::new(AtomicUsize::new(0)),
            }
        }

        fn start(&self) -> ProviderStream {
            let transport: Arc<dyn Transport> = Arc::new(self.transport.clone());
            let credentials: Arc<dyn CredentialSource> = self.credentials.clone();
            let builds = self.builds.clone();
            drive(DriveRequest {
                transport,
                credentials,
                build: Box::new(move |credential: &Credential| {
                    builds.fetch_add(1, Ordering::SeqCst);
                    HttpRequest {
                        url: "https://provider.test/v1/stream?secret=in-the-query".to_string(),
                        headers: vec![(
                            "authorization".to_string(),
                            format!("Bearer {}", credential.bearer),
                        )],
                        body: br#"{"stream":true,"prompt":"body text"}"#.to_vec(),
                    }
                }),
                new_parser: Box::new(|| Box::new(TestParser) as Box<dyn ResponseParser>),
                retry: self.retry,
                cancel: self.cancel.clone(),
            })
        }
    }

    fn status_response(status: u16) -> ScriptedResponse {
        ScriptedResponse {
            status,
            headers: Vec::new(),
            chunks: Vec::new(),
            end: BodyEnd::Eof,
        }
    }

    fn ok(text: &str) -> ScriptedResponse {
        ScriptedResponse::ok_sse(text)
    }

    /// A 401 whose body says the account has no balance, the way the chat adapter
    /// reports one (ADR-0046).
    fn exhausted_response() -> ScriptedResponse {
        let mut response = status_response(401);
        response.chunks.push(b"exhausted-account".to_vec());
        response
    }

    /// A 403 whose body says the plan does not allow this model, the way the chat
    /// adapter reports one (ADR-0062).
    fn not_entitled_response() -> ScriptedResponse {
        let mut response = status_response(403);
        response.chunks.push(b"not-entitled".to_vec());
        response
    }

    /// A 429 whose body says the account's usage allowance is used up. It is a
    /// terminal account diagnosis, not a short rate-limit window.
    fn usage_limit_response() -> ScriptedResponse {
        let mut response = status_response(429);
        response.chunks.push(b"usage-limit-exhausted".to_vec());
        response
    }

    fn text_turn() -> &'static str {
        "data: delta\n\ndata: done\n\n"
    }

    async fn collect(mut stream: ProviderStream) -> Vec<StreamEvent> {
        let mut events = Vec::new();
        while let Some(event) = stream.next().await {
            events.push(event);
        }
        events
    }

    fn finished_count(events: &[StreamEvent]) -> usize {
        events
            .iter()
            .filter(|event| matches!(event, StreamEvent::Finished(_)))
            .count()
    }

    fn terminal(events: &[StreamEvent]) -> &Outcome {
        match events.last() {
            Some(StreamEvent::Finished(outcome)) => outcome,
            other => panic!("stream did not end with Finished: {other:?}"),
        }
    }

    /// Drive one request against an arbitrary transport and policy, with the same
    /// TestParser and request builder the `Harness` uses. Lets a test inject a
    /// transport whose waits do not resolve.
    fn drive_with(transport: Arc<dyn Transport>, retry: RetryPolicy) -> ProviderStream {
        drive(DriveRequest {
            transport,
            credentials: Arc::new(ScriptedCredentials::new("OLD", "NEW")),
            build: Box::new(|_credential: &Credential| HttpRequest {
                url: "https://provider.test/v1/stream".to_string(),
                headers: Vec::new(),
                body: Vec::new(),
            }),
            new_parser: Box::new(|| Box::new(TestParser) as Box<dyn ResponseParser>),
            retry,
            cancel: CancellationToken::new(),
        })
    }

    /// A transport whose `post` never resolves, counting the attempts, so a test
    /// can prove the first-byte bound retries inside the shared budget.
    #[derive(Clone, Default)]
    struct SilentServer {
        posts: Arc<AtomicUsize>,
    }

    impl Transport for SilentServer {
        fn post<'a>(
            &'a self,
            _request: HttpRequest,
        ) -> BoxFuture<'a, Result<HttpResponse, TransportError>> {
            self.posts.fetch_add(1, Ordering::SeqCst);
            Box::pin(std::future::pending())
        }
    }

    /// A transport whose body emits one SSE comment every `gap`, then a final turn.
    /// On the paused clock the gap is virtual, so no test ever sleeps.
    struct PingingTransport {
        gap: Duration,
        pings: usize,
    }

    impl Transport for PingingTransport {
        fn post<'a>(
            &'a self,
            _request: HttpRequest,
        ) -> BoxFuture<'a, Result<HttpResponse, TransportError>> {
            let gap = self.gap;
            let pings = self.pings;
            Box::pin(async move {
                let stream = futures_util::stream::unfold(0usize, move |n| async move {
                    if n < pings {
                        tokio::time::sleep(gap).await;
                        Some((Ok::<_, TransportError>(b": ping\n\n".to_vec()), n + 1))
                    } else if n == pings {
                        Some((Ok(text_turn().as_bytes().to_vec()), n + 1))
                    } else {
                        None
                    }
                });
                Ok(HttpResponse {
                    status: 200,
                    headers: Vec::new(),
                    body: Box::pin(stream),
                })
            })
        }
    }

    /// A transport whose headers arrive only after `after`, counting the attempts so
    /// a test can prove a late poll reuses the single in-flight request. On the paused
    /// clock the delay is virtual, so no test ever sleeps.
    #[derive(Clone, Default)]
    struct SlowServer {
        after: Duration,
        posts: Arc<AtomicUsize>,
    }

    impl Transport for SlowServer {
        fn post<'a>(
            &'a self,
            _request: HttpRequest,
        ) -> BoxFuture<'a, Result<HttpResponse, TransportError>> {
            self.posts.fetch_add(1, Ordering::SeqCst);
            let after = self.after;
            Box::pin(async move {
                tokio::time::sleep(after).await;
                let body = futures_util::stream::iter(vec![Ok::<_, TransportError>(
                    text_turn().as_bytes().to_vec(),
                )]);
                Ok(HttpResponse {
                    status: 200,
                    headers: Vec::new(),
                    body: Box::pin(body),
                })
            })
        }
    }

    #[tokio::test(start_paused = true)]
    async fn transient_status_retries_then_succeeds() {
        let harness = Harness::new(vec![
            status_response(500),
            status_response(500),
            ok(text_turn()),
        ]);
        let events = collect(harness.start()).await;

        assert_eq!(harness.transport.requests().len(), 3);
        assert_eq!(finished_count(&events), 1);
        assert!(matches!(terminal(&events), Outcome::Completed(_)));
        let first_content = events
            .iter()
            .position(|event| matches!(event, StreamEvent::TextDelta { .. }))
            .expect("content delta");
        let first_activity = events
            .iter()
            .position(|event| matches!(event, StreamEvent::Activity))
            .expect("back-off activity");
        let first_notice = events
            .iter()
            .position(|event| matches!(event, StreamEvent::Notice { .. }))
            .expect("back-off notice");
        assert!(first_notice < first_activity, "notice precedes activity");
        assert!(
            first_activity < first_content,
            "activity must precede the first content event: {events:?}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn transient_status_exhausts_the_budget_then_fails_transport() {
        let harness = Harness::new(vec![
            status_response(500),
            status_response(500),
            status_response(500),
            status_response(500),
        ]);
        let events = collect(harness.start()).await;

        assert_eq!(harness.transport.requests().len(), 4);
        assert_eq!(finished_count(&events), 1);
        match terminal(&events) {
            Outcome::Failed(error) => assert_eq!(error.kind, ProviderErrorKind::Transport),
            other => panic!("expected a transport failure, got {other:?}"),
        }
    }

    #[tokio::test(start_paused = true)]
    async fn reauth_refreshes_once_with_the_rejected_credential() {
        let harness = Harness::new(vec![status_response(401), ok(text_turn())]);
        let events = collect(harness.start()).await;

        let requests = harness.transport.requests();
        assert_eq!(requests.len(), 2);
        let refresh_calls = harness.credentials.refresh_calls.lock().unwrap();
        assert_eq!(refresh_calls.len(), 1);
        assert_eq!(refresh_calls[0].bearer, "OLD");
        assert_eq!(harness.credentials.access_calls.load(Ordering::SeqCst), 1);
        assert!(requests[1].headers.iter().any(|(name, value)| {
            name.eq_ignore_ascii_case("authorization") && value == "Bearer NEW"
        }));
        assert!(matches!(terminal(&events), Outcome::Completed(_)));
    }

    #[tokio::test(start_paused = true)]
    async fn second_reauth_is_authentication() {
        let harness = Harness::new(vec![status_response(401), status_response(401)]);
        let events = collect(harness.start()).await;

        assert_eq!(harness.transport.requests().len(), 2);
        match terminal(&events) {
            Outcome::Failed(error) => assert_eq!(error.kind, ProviderErrorKind::Authentication),
            other => panic!("expected an authentication failure, got {other:?}"),
        }
    }

    #[tokio::test(start_paused = true)]
    async fn insufficient_balance_finishes_without_refresh_or_retry() {
        // A second response is scripted so a stray request is reported as a plain
        // count mismatch rather than a transport panic.
        let harness = Harness::new(vec![exhausted_response(), ok(text_turn())]);
        let events = collect(harness.start()).await;

        assert_eq!(harness.transport.requests().len(), 1);
        let refresh_calls = harness.credentials.refresh_calls.lock().unwrap();
        assert!(refresh_calls.is_empty(), "{refresh_calls:?}");
        assert_eq!(harness.credentials.access_calls.load(Ordering::SeqCst), 1);
        match terminal(&events) {
            Outcome::Failed(error) => {
                assert_eq!(error.kind, ProviderErrorKind::InsufficientBalance);
                assert_eq!(error.message, "the account has no balance");
            }
            other => panic!("expected an exhausted-account failure, got {other:?}"),
        }
    }

    #[tokio::test(start_paused = true)]
    async fn not_entitled_finishes_without_refresh_or_retry() {
        // ADR-0062: a 403 whose body says the plan does not allow this model is
        // NOT a rejected key. It must not refresh or retry, like no-balance.
        // A second response is scripted so a stray request is reported as a plain
        // count mismatch rather than a transport panic.
        let harness = Harness::new(vec![not_entitled_response(), ok(text_turn())]);
        let events = collect(harness.start()).await;

        assert_eq!(harness.transport.requests().len(), 1);
        let refresh_calls = harness.credentials.refresh_calls.lock().unwrap();
        assert!(refresh_calls.is_empty(), "{refresh_calls:?}");
        assert_eq!(harness.credentials.access_calls.load(Ordering::SeqCst), 1);
        match terminal(&events) {
            Outcome::Failed(error) => {
                assert_eq!(error.kind, ProviderErrorKind::NotEntitled);
                assert_eq!(
                    error.message,
                    "the account's plan does not allow this model on this route"
                );
            }
            other => panic!("expected a not-entitled failure, got {other:?}"),
        }
    }

    #[tokio::test(start_paused = true)]
    async fn insufficient_balance_after_a_refresh_keeps_the_diagnosis() {
        let harness = Harness::new(vec![status_response(401), exhausted_response()]);
        let events = collect(harness.start()).await;

        assert_eq!(harness.transport.requests().len(), 2);
        assert_eq!(harness.credentials.refresh_calls.lock().unwrap().len(), 1);
        match terminal(&events) {
            Outcome::Failed(error) => {
                assert_eq!(error.kind, ProviderErrorKind::InsufficientBalance)
            }
            other => panic!("expected an exhausted-account failure, got {other:?}"),
        }
    }

    #[tokio::test(start_paused = true)]
    async fn reauth_does_not_consume_the_transient_budget() {
        let harness = Harness::new(vec![
            status_response(401),
            status_response(500),
            ok(text_turn()),
        ]);
        let events = collect(harness.start()).await;

        assert_eq!(harness.transport.requests().len(), 3);
        assert_eq!(harness.credentials.refresh_calls.lock().unwrap().len(), 1);
        assert!(matches!(terminal(&events), Outcome::Completed(_)));
    }

    #[tokio::test(start_paused = true)]
    async fn usage_limit_exhausted_finishes_without_refresh_or_retry() {
        let harness = Harness::new(vec![usage_limit_response(), ok(text_turn())]);
        let events = collect(harness.start()).await;

        assert_eq!(harness.transport.requests().len(), 1);
        assert!(harness.credentials.refresh_calls.lock().unwrap().is_empty());
        assert_eq!(harness.credentials.access_calls.load(Ordering::SeqCst), 1);
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, StreamEvent::Notice { .. }))
        );
        match terminal(&events) {
            Outcome::Failed(error) => {
                assert_eq!(error.kind, ProviderErrorKind::UsageLimitExhausted);
                assert_eq!(error.message, "the account's usage allowance is used up");
            }
            other => panic!("expected a usage-limit failure, got {other:?}"),
        }
    }

    #[tokio::test(start_paused = true)]
    async fn plain_429_emits_the_computed_retry_notice_before_the_wait() {
        let policy = RetryPolicy {
            jitter: Duration::ZERO,
            ..RetryPolicy::default()
        };
        let mut throttled = status_response(429);
        throttled.headers.push(("Retry-After".into(), "8".into()));
        let harness = Harness::custom(vec![throttled, ok(text_turn())], policy, "OLD", "NEW");
        let mut stream = harness.start();

        assert_eq!(
            stream.next().await,
            Some(StreamEvent::Notice {
                text: "provider returned HTTP 429; retry 1/3 in 8 s".into()
            })
        );
        assert_eq!(harness.transport.requests().len(), 1);
        assert_eq!(stream.next().await, Some(StreamEvent::Activity));
        let rest = collect(stream).await;
        assert!(
            rest.iter()
                .any(|event| matches!(event, StreamEvent::TextDelta { .. }))
        );
        assert_eq!(harness.transport.requests().len(), 2);
    }

    #[tokio::test(start_paused = true)]
    async fn retry_after_hint_sets_the_wait() {
        let mut throttled = status_response(429);
        throttled
            .headers
            .push(("Retry-After".to_string(), "7".to_string()));
        let harness = Harness::new(vec![throttled, ok(text_turn())]);

        let start = tokio::time::Instant::now();
        let events = collect(harness.start()).await;
        let elapsed = start.elapsed();

        assert_eq!(harness.transport.requests().len(), 2);
        assert!(matches!(terminal(&events), Outcome::Completed(_)));
        assert_eq!(
            elapsed,
            Duration::from_secs(7),
            "the Retry-After hint, not the 2 s base, sets the wait"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn fatal_status_fails_without_retrying() {
        let mut invalid = status_response(400);
        invalid.chunks.push(b"{\"error\":\"bad request\"}".to_vec());
        let harness = Harness::new(vec![invalid]);
        let events = collect(harness.start()).await;

        assert_eq!(harness.transport.requests().len(), 1);
        match terminal(&events) {
            Outcome::Failed(error) => {
                assert_eq!(error.kind, ProviderErrorKind::InvalidRequest);
                assert!(error.message.contains("400"), "{}", error.message);
            }
            other => panic!("expected a fatal failure, got {other:?}"),
        }
    }

    #[tokio::test(start_paused = true)]
    async fn body_error_after_visible_output_is_not_retried() {
        let broken = ScriptedResponse {
            status: 200,
            headers: Vec::new(),
            chunks: vec![b"data: delta\n\n".to_vec()],
            end: BodyEnd::Error("connection reset".to_string()),
        };
        let harness = Harness::new(vec![broken, ok(text_turn())]);
        let events = collect(harness.start()).await;

        assert_eq!(harness.transport.requests().len(), 1);
        assert!(matches!(
            terminal(&events),
            Outcome::Failed(error) if error.kind == ProviderErrorKind::Transport
        ));
    }

    #[tokio::test(start_paused = true)]
    async fn body_error_before_content_is_retried() {
        let broken = ScriptedResponse {
            status: 200,
            headers: Vec::new(),
            chunks: Vec::new(),
            end: BodyEnd::Error("connection reset".to_string()),
        };
        let harness = Harness::new(vec![broken, ok(text_turn())]);
        let events = collect(harness.start()).await;

        assert_eq!(harness.transport.requests().len(), 2);
        assert!(matches!(terminal(&events), Outcome::Completed(_)));
    }

    #[tokio::test(start_paused = true)]
    async fn non_content_events_do_not_block_a_retry() {
        let broken = ScriptedResponse {
            status: 200,
            headers: Vec::new(),
            chunks: vec![b"data: activity\n\n".to_vec()],
            end: BodyEnd::Error("connection reset".to_string()),
        };
        let harness = Harness::new(vec![broken, ok(text_turn())]);
        let events = collect(harness.start()).await;

        assert_eq!(harness.transport.requests().len(), 2);
        assert!(matches!(terminal(&events), Outcome::Completed(_)));
    }

    #[tokio::test(start_paused = true)]
    async fn eof_after_visible_content_is_not_retried() {
        let harness = Harness::new(vec![ok("data: delta\n\n"), ok(text_turn())]);
        let events = collect(harness.start()).await;

        // The first body ends after a visible delta, so it is NOT retried: rule 3
        // wins over the missing terminal event.
        assert_eq!(harness.transport.requests().len(), 1);
        assert!(matches!(
            terminal(&events),
            Outcome::Failed(error) if error.kind == ProviderErrorKind::Transport
        ));
    }

    #[tokio::test(start_paused = true)]
    async fn eof_without_content_is_retried() {
        let harness = Harness::new(vec![ok(": keep-alive\n\n"), ok(text_turn())]);
        let events = collect(harness.start()).await;

        assert_eq!(harness.transport.requests().len(), 2);
        assert!(matches!(terminal(&events), Outcome::Completed(_)));
    }

    #[tokio::test(start_paused = true)]
    async fn cancel_during_backoff_stops_without_a_second_request() {
        let harness = Harness::new(vec![status_response(500), ok(text_turn())]);
        let mut stream = harness.start();

        let first = stream.next().await.expect("an event");
        assert!(matches!(first, StreamEvent::Notice { .. }), "{first:?}");
        harness.cancel.cancel();

        let events = collect(stream).await;
        assert_eq!(harness.transport.requests().len(), 1);
        assert_eq!(finished_count(&events), 1);
        assert!(matches!(terminal(&events), Outcome::Cancelled));
    }

    #[tokio::test(start_paused = true)]
    async fn cancel_while_the_body_hangs() {
        let hanging = ScriptedResponse {
            status: 200,
            headers: Vec::new(),
            chunks: vec![b"data: delta\n\n".to_vec()],
            end: BodyEnd::Hang,
        };
        let harness = Harness::new(vec![hanging]);
        let mut stream = harness.start();

        let mut saw_delta = false;
        while let Some(event) = stream.next().await {
            if matches!(event, StreamEvent::TextDelta { .. }) {
                saw_delta = true;
                break;
            }
        }
        assert!(saw_delta);
        harness.cancel.cancel();

        let events = collect(stream).await;
        assert_eq!(finished_count(&events), 1);
        assert!(matches!(terminal(&events), Outcome::Cancelled));
    }

    #[tokio::test(start_paused = true)]
    async fn cancel_before_the_first_byte() {
        let harness = Harness::new(vec![ok(text_turn())]);
        harness.cancel.cancel();

        let events = collect(harness.start()).await;
        assert_eq!(harness.transport.requests().len(), 0);
        assert_eq!(finished_count(&events), 1);
        assert!(matches!(terminal(&events), Outcome::Cancelled));
    }

    // Lead regression: the body's last event has no trailing blank line. It must be
    // flushed and parsed, not lost and reported as a broken stream.
    #[tokio::test(start_paused = true)]
    async fn a_final_event_without_a_trailing_blank_line_still_terminates_the_stream() {
        let harness = Harness::new(vec![ScriptedResponse::ok_sse("data: delta\n\ndata: done")]);
        let events = collect(harness.start()).await;

        assert_eq!(harness.transport.requests().len(), 1);
        assert_eq!(finished_count(&events), 1);
        assert!(
            matches!(terminal(&events), Outcome::Completed(_)),
            "{events:?}"
        );
    }

    // Lead regression: an endless error body is not buffered without bound.
    #[tokio::test(start_paused = true)]
    async fn an_oversized_error_body_is_cut_off_before_classification() {
        let chunk = vec![b'x'; 40 * 1024];
        let harness = Harness::new(vec![ScriptedResponse {
            status: 400,
            headers: Vec::new(),
            chunks: vec![chunk.clone(), chunk.clone(), chunk],
            end: BodyEnd::Hang,
        }]);
        let events = collect(harness.start()).await;

        assert_eq!(harness.transport.requests().len(), 1);
        assert!(
            matches!(terminal(&events), Outcome::Failed(_)),
            "{events:?}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn events_a_parser_returns_after_its_finished_are_not_forwarded() {
        let harness = Harness::new(vec![ok("data: finish-then-delta\n\ndata: after\n\n")]);
        let events = collect(harness.start()).await;

        assert_eq!(finished_count(&events), 1);
        assert!(matches!(terminal(&events), Outcome::Completed(_)));
        assert!(
            !events.iter().any(|event| matches!(
                event,
                StreamEvent::TextDelta { text, .. } if text == "AFTER"
            )),
            "events after Finished must be dropped: {events:?}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn the_stream_is_done_after_the_terminal_event() {
        let harness = Harness::new(vec![ok(text_turn())]);
        let mut stream = harness.start();
        let mut events = Vec::new();
        while let Some(event) = stream.next().await {
            events.push(event);
        }
        assert_eq!(finished_count(&events), 1);
        assert!(stream.next().await.is_none());
    }

    #[tokio::test(start_paused = true)]
    async fn chunk_boundaries_do_not_change_the_stream() {
        let body = text_turn();
        let expected = collect(Harness::new(vec![ok(body)]).start()).await;
        for at in 0..=body.len() {
            let harness = Harness::new(vec![ScriptedResponse::ok_sse_split(body, at)]);
            let events = collect(harness.start()).await;
            assert_eq!(events, expected, "split at byte offset {at}");
        }
    }

    /// A transport whose `post` never resolves, to prove the send wait races the
    /// cancellation token like the body and back-off waits do.
    struct HangingTransport;

    impl Transport for HangingTransport {
        fn post<'a>(
            &'a self,
            _request: HttpRequest,
        ) -> BoxFuture<'a, Result<HttpResponse, TransportError>> {
            Box::pin(std::future::pending())
        }
    }

    #[tokio::test(start_paused = true)]
    async fn cancel_while_the_request_is_in_flight() {
        let credentials = Arc::new(ScriptedCredentials::new("OLD", "NEW"));
        let cancel = CancellationToken::new();
        let mut stream = drive(DriveRequest {
            transport: Arc::new(HangingTransport),
            credentials,
            build: Box::new(|_credential: &Credential| HttpRequest {
                url: "https://provider.test/v1/stream".to_string(),
                headers: Vec::new(),
                body: Vec::new(),
            }),
            new_parser: Box::new(|| Box::new(TestParser) as Box<dyn ResponseParser>),
            retry: RetryPolicy::default(),
            cancel: cancel.clone(),
        });

        {
            let poll = stream.next();
            let yield_now = std::pin::pin!(tokio::task::yield_now());
            match futures_util::future::select(poll, yield_now).await {
                futures_util::future::Either::Left(_) => panic!("the transport must not resolve"),
                futures_util::future::Either::Right(_) => {}
            }
        }
        cancel.cancel();

        let events = collect(stream).await;
        assert_eq!(finished_count(&events), 1);
        assert!(matches!(terminal(&events), Outcome::Cancelled));
    }

    #[tokio::test(start_paused = true)]
    async fn credentials_and_bodies_never_reach_an_error_or_debug() {
        let mut rejected = status_response(400);
        rejected
            .headers
            .push(("x-secret".to_string(), "SENTINEL-SECRET-123".to_string()));
        rejected.chunks.push(b"SENTINEL-BODY-456".to_vec());
        let harness = Harness::custom(
            vec![rejected],
            RetryPolicy::default(),
            "SENTINEL-SECRET-123",
            "NEW",
        );
        let events = collect(harness.start()).await;

        let error = match terminal(&events) {
            Outcome::Failed(error) => error.clone(),
            other => panic!("expected failure, got {other:?}"),
        };
        let request_debug = format!("{:?}", harness.transport.requests()[0]);
        let event_debug = format!("{events:?}");
        for text in [
            request_debug.clone(),
            event_debug,
            format!("{error:?}"),
            error.to_string(),
        ] {
            assert!(
                !text.contains("SENTINEL-SECRET-123"),
                "leaked bearer: {text}"
            );
            assert!(!text.contains("SENTINEL-BODY-456"), "leaked body: {text}");
        }
        assert!(
            !request_debug.contains("in-the-query"),
            "leaked query: {request_debug}"
        );
        assert!(
            !request_debug.contains("body text"),
            "leaked body: {request_debug}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn connect_error_on_the_first_attempt_is_retried() {
        let harness = Harness::new(vec![
            ScriptedResponse::connect_error("connection refused"),
            ok(text_turn()),
        ]);
        let events = collect(harness.start()).await;

        assert_eq!(harness.transport.requests().len(), 2);
        assert!(matches!(terminal(&events), Outcome::Completed(_)));
    }

    #[tokio::test(start_paused = true)]
    async fn connect_errors_exhaust_the_budget_then_fail_transport() {
        let harness = Harness::new(vec![
            ScriptedResponse::connect_error("connection refused"),
            ScriptedResponse::connect_error("connection refused"),
            ScriptedResponse::connect_error("connection refused"),
            ScriptedResponse::connect_error("connection refused"),
        ]);
        let events = collect(harness.start()).await;

        assert_eq!(harness.transport.requests().len(), 4);
        assert!(matches!(
            terminal(&events),
            Outcome::Failed(error) if error.kind == ProviderErrorKind::Transport
        ));
    }

    /// Issue #164: a provider that never sends response headers must end as a
    /// `Transport` failure naming the first-byte bound, not hang the agent.
    #[tokio::test(start_paused = true)]
    async fn a_request_that_never_answers_fails_at_the_first_byte_bound() {
        // max_retries 0 isolates the bound from the retry loop.
        let stream = drive_with(
            Arc::new(HangingTransport),
            RetryPolicy {
                max_retries: 0,
                ..RetryPolicy::default()
            },
        );
        let start = tokio::time::Instant::now();
        let events =
            tokio::time::timeout(FIRST_BYTE_TIMEOUT + Duration::from_secs(1), collect(stream))
                .await
                .expect("the first-byte bound must end the wait, not hang CI");

        let error = match terminal(&events) {
            Outcome::Failed(error) => error.clone(),
            other => panic!("expected a transport failure, got {other:?}"),
        };
        assert_eq!(error.kind, ProviderErrorKind::Transport);
        assert_eq!(
            error.message,
            format!("no response within {} s", FIRST_BYTE_TIMEOUT.as_secs())
        );
        assert_eq!(
            start.elapsed(),
            FIRST_BYTE_TIMEOUT,
            "the wait ends at the bound, exactly"
        );
    }

    /// Issue #164's "transcript note while waiting": the first-byte wait says so
    /// ONCE, after the grace delay, without aborting the request — the response
    /// still arrives under the bound.
    #[tokio::test(start_paused = true)]
    async fn the_first_byte_wait_tells_the_operator_once_after_thirty_seconds() {
        let stream = drive_with(
            Arc::new(SlowServer {
                after: Duration::from_secs(60),
                ..SlowServer::default()
            }),
            RetryPolicy::default(),
        );
        let start = tokio::time::Instant::now();
        let mut stream = stream;

        let first =
            tokio::time::timeout(WAITING_NOTE_AFTER + Duration::from_secs(1), stream.next())
                .await
                .expect("the waiting note is due before the bound")
                .expect("an event");
        assert!(
            matches!(first, StreamEvent::Notice { ref text }
                if text == &format!("waiting for the provider ({} s)", WAITING_NOTE_AFTER.as_secs())),
            "{first:?}"
        );
        assert_eq!(
            start.elapsed(),
            WAITING_NOTE_AFTER,
            "the note comes at the grace delay"
        );

        let rest = tokio::time::timeout(Duration::from_secs(61), collect(stream))
            .await
            .expect("the response arrives under the bound");
        assert!(matches!(terminal(&rest), Outcome::Completed(_)));
        assert_eq!(
            rest.iter()
                .filter(|event| matches!(event, StreamEvent::Notice { .. }))
                .count(),
            0,
            "the note is emitted once: {rest:?}"
        );
        assert_eq!(
            start.elapsed(),
            Duration::from_secs(60),
            "the response at 60 s"
        );
    }

    /// Review round 2: the deadline is not pre-checked before the in-flight post is
    /// polled, so a response that arrived inside the bound still wins when the consumer
    /// only resumes polling after it — no retry, no duplicate request. (A pre-check
    /// would have failed the attempt and re-sent.)
    #[tokio::test(start_paused = true)]
    async fn a_response_that_arrived_inside_the_bound_wins_when_the_poll_resumes_late() {
        let server = SlowServer {
            after: Duration::from_secs(60),
            ..SlowServer::default()
        };
        let mut stream = drive_with(Arc::new(server.clone()), RetryPolicy::default());
        let start = tokio::time::Instant::now();

        // The 30 s note is the consumer's only event while the post stays in flight.
        let note = tokio::time::timeout(WAITING_NOTE_AFTER + Duration::from_secs(1), stream.next())
            .await
            .expect("the waiting note is due at 30 s")
            .expect("an event");
        assert!(matches!(note, StreamEvent::Notice { .. }), "{note:?}");
        assert_eq!(start.elapsed(), WAITING_NOTE_AFTER);

        // Away past the bound; the provider answered at 60 s.
        tokio::time::advance(FIRST_BYTE_TIMEOUT).await;

        let events = tokio::time::timeout(Duration::from_secs(1), collect(stream))
            .await
            .expect("the ready response must be used, not the bound");
        assert!(
            matches!(terminal(&events), Outcome::Completed(_)),
            "{events:?}"
        );
        assert_eq!(
            server.posts.load(Ordering::SeqCst),
            1,
            "the arrived response is used: no retry, no second request"
        );
    }

    /// The same wait under the default policy: it is a `Transport` failure, so it
    /// retries `max_retries` (3) times with a retry notice each, then fails. This is
    /// what the operator sees while a silent provider is given its chances.
    #[tokio::test(start_paused = true)]
    async fn a_first_byte_timeout_retries_within_the_budget_then_fails_transport() {
        let server = SilentServer::default();
        let stream = drive_with(Arc::new(server.clone()), RetryPolicy::default());
        let start = tokio::time::Instant::now();
        let policy = RetryPolicy::default();
        let bound = 4 * FIRST_BYTE_TIMEOUT
            + policy.delay(1, None)
            + policy.delay(2, None)
            + policy.delay(3, None);
        let events = tokio::time::timeout(bound + Duration::from_secs(1), collect(stream))
            .await
            .expect("the retry budget must end the wait, not hang CI");

        assert_eq!(
            server.posts.load(Ordering::SeqCst),
            4,
            "the first attempt plus max_retries (3)"
        );
        assert!(matches!(
            terminal(&events),
            Outcome::Failed(error) if error.kind == ProviderErrorKind::Transport
        ));
        let retry_notices = events
            .iter()
            .filter(|event| {
                matches!(event, StreamEvent::Notice { text }
                    if text.starts_with("provider request failed"))
            })
            .count();
        assert_eq!(retry_notices, 3, "one retry notice per retry");
        let waiting_notes = events
            .iter()
            .filter(|event| {
                matches!(event, StreamEvent::Notice { text }
                    if text.starts_with("waiting for the provider"))
            })
            .count();
        assert_eq!(waiting_notes, 4, "one waiting note per bounded attempt");
        assert_eq!(
            start.elapsed(),
            bound,
            "four bounded attempts, the retry policy's backoff between them"
        );
    }

    /// Issue #164: a stream that sends one visible event and then goes silent must
    /// end as a `Transport` failure naming the idle bound. After output it is
    /// terminal, so there is exactly one request.
    #[tokio::test(start_paused = true)]
    async fn a_stream_that_stalls_after_an_event_fails_at_the_idle_bound() {
        let stalling = ScriptedResponse {
            status: 200,
            headers: Vec::new(),
            chunks: vec![b"data: delta\n\n".to_vec()],
            end: BodyEnd::Hang,
        };
        let harness = Harness::new(vec![stalling]);
        let start = tokio::time::Instant::now();
        let events = tokio::time::timeout(
            STREAM_IDLE_TIMEOUT + Duration::from_secs(1),
            collect(harness.start()),
        )
        .await
        .expect("the idle bound must end the wait, not hang CI");

        assert_eq!(
            harness.transport.requests().len(),
            1,
            "after visible output a stall is terminal: no retry"
        );
        assert!(
            events
                .iter()
                .any(|event| matches!(event, StreamEvent::TextDelta { .. }))
        );
        let error = match terminal(&events) {
            Outcome::Failed(error) => error.clone(),
            other => panic!("expected a transport failure, got {other:?}"),
        };
        assert_eq!(error.kind, ProviderErrorKind::Transport);
        assert_eq!(
            error.message,
            format!("stream idle for {} s", STREAM_IDLE_TIMEOUT.as_secs())
        );
        assert_eq!(
            start.elapsed(),
            STREAM_IDLE_TIMEOUT,
            "the idle wait ends at the bound, exactly"
        );
    }

    /// A stalled non-2xx body must not hang classification either: the status
    /// policy is read from whatever arrived, then the retry loop proceeds.
    #[tokio::test(start_paused = true)]
    async fn a_stalled_error_body_is_classified_without_hanging() {
        let stalled = || ScriptedResponse {
            status: 500,
            headers: Vec::new(),
            chunks: Vec::new(),
            end: BodyEnd::Hang,
        };
        let harness = Harness::new(vec![stalled(), stalled(), stalled(), stalled()]);
        let start = tokio::time::Instant::now();
        let policy = RetryPolicy::default();
        let bound = 4 * STREAM_IDLE_TIMEOUT
            + policy.delay(1, None)
            + policy.delay(2, None)
            + policy.delay(3, None);
        let events = tokio::time::timeout(bound + Duration::from_secs(1), collect(harness.start()))
            .await
            .expect("a stalled error body must not hang CI");

        assert_eq!(harness.transport.requests().len(), 4);
        assert!(matches!(
            terminal(&events),
            Outcome::Failed(error) if error.kind == ProviderErrorKind::Transport
        ));
        assert_eq!(start.elapsed(), bound);
    }

    /// A keep-alive ping (an SSE comment) inside the idle bound keeps the stream
    /// alive: six pings 200 s apart are 20 minutes with no stall.
    #[tokio::test(start_paused = true)]
    async fn keep_alive_pings_reset_the_idle_bound() {
        let stream = drive_with(
            Arc::new(PingingTransport {
                gap: Duration::from_secs(200),
                pings: 6,
            }),
            RetryPolicy::default(),
        );
        let start = tokio::time::Instant::now();
        let events = tokio::time::timeout(Duration::from_secs(1201), collect(stream))
            .await
            .expect("keep-alive pings must not be called idle, and must not hang CI");

        assert!(
            matches!(terminal(&events), Outcome::Completed(_)),
            "{events:?}"
        );
        assert_eq!(
            start.elapsed(),
            Duration::from_secs(1200),
            "20 minutes of pings, none of them past the idle bound"
        );
    }
}
