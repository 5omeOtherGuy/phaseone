#![forbid(unsafe_code)]

use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use futures_util::StreamExt;
use p1_contracts::{
    AssistantBlock, DeclarationKind, Item, ModelOptions, Outcome, Provider, ProviderErrorKind,
    ProviderRequest, StopReason, StreamEvent, ToolDeclaration, ToolInput,
};
use p1_provider_http::SseDecoder;
use p1_provider_http::testing::{BodyEnd, ScriptedResponse, ScriptedTransport};

pub struct RouteUnderTest {
    pub name: &'static str,
    pub build: fn(ScriptedTransport) -> Arc<dyn Provider>,
    pub fixtures: RouteFixtures,
    pub follow_up_request: fn(&ProviderRequest) -> serde_json::Value,
    pub fake_bearer: &'static str,
    /// A request THIS route must reject (e.g. a freeform tool on a function-only route,
    /// an option it cannot carry). Used by check 14.
    pub invalid_request: fn() -> ProviderRequest,
}

#[derive(Clone, Copy)]
pub struct RouteFixtures {
    pub text_turn: &'static str,
    pub tool_call_turn: &'static str,
    pub two_tool_calls: &'static str,
    pub truncated_tool_call: &'static str,
    pub invalid_tool_json: &'static str,
    pub error_event: &'static str,
    pub no_usage: &'static str,
    pub reasoning_turn: &'static str,
    pub events_after_terminal: &'static str,
}

macro_rules! check {
    ($route:expr, $name:expr, $condition:expr, $($arg:tt)+) => {
        assert!($condition, "[{}] {}: {}", $route.name, $name, format_args!($($arg)+))
    };
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .start_paused(true)
        .build()
        .expect("current-thread conformance runtime")
}

fn request() -> ProviderRequest {
    fn function(name: &str, property: &str) -> ToolDeclaration {
        ToolDeclaration {
            name: name.to_string(),
            description: format!("{name} tool"),
            kind: DeclarationKind::Function {
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": { property: { "type": "string" } },
                    "required": [property]
                }),
            },
        }
    }
    ProviderRequest {
        system_prompt: "conformance prompt".to_string(),
        history: vec![Item::User {
            text: "hi".to_string(),
        }],
        tools: vec![function("read", "path"), function("grep", "pattern")],
        options: ModelOptions::default(),
    }
}

fn responses(response: ScriptedResponse) -> Vec<ScriptedResponse> {
    vec![
        response.clone(),
        response.clone(),
        response.clone(),
        response,
    ]
}

async fn bounded<T>(future: impl Future<Output = T>) -> Result<T, &'static str> {
    tokio::time::timeout(Duration::from_secs(300), future)
        .await
        .map_err(|_| "operation timed out")
}

async fn collect_provider(
    provider: &Arc<dyn Provider>,
    request: ProviderRequest,
    cancel: p1_contracts::CancellationToken,
) -> Result<Vec<StreamEvent>, String> {
    let mut stream = bounded(provider.stream(request, cancel))
        .await
        .map_err(str::to_string)?
        .map_err(|error| format!("setup error ({:?})", error.kind))?;
    bounded(async {
        let mut events = Vec::new();
        while let Some(event) = stream.next().await {
            events.push(event);
        }
        events
    })
    .await
    .map_err(str::to_string)
}

fn collect(route: &RouteUnderTest, response: ScriptedResponse) -> Result<Vec<StreamEvent>, String> {
    runtime().block_on(async {
        let provider = (route.build)(ScriptedTransport::new(responses(response)));
        collect_provider(&provider, request(), p1_contracts::CancellationToken::new()).await
    })
}

fn ok(route: &RouteUnderTest, name: &str, body: &str) -> Vec<StreamEvent> {
    match collect(route, ScriptedResponse::ok_sse(body)) {
        Ok(events) => events,
        Err(seen) => panic!("[{}] {name}: expected a stream, saw {seen}", route.name),
    }
}

fn terminal(events: &[StreamEvent]) -> Option<&Outcome> {
    match events.last() {
        Some(StreamEvent::Finished(outcome)) => Some(outcome),
        _ => None,
    }
}

fn completed(events: &[StreamEvent]) -> Option<&p1_contracts::CompletedResponse> {
    match terminal(events) {
        Some(Outcome::Completed(response)) => Some(response),
        _ => None,
    }
}

fn finished_count(events: &[StreamEvent]) -> usize {
    events
        .iter()
        .filter(|event| matches!(event, StreamEvent::Finished(_)))
        .count()
}

pub fn text_deltas_then_single_terminal(route: &RouteUnderTest) {
    const NAME: &str = "text_deltas_then_single_terminal";
    let events = ok(route, NAME, route.fixtures.text_turn);
    let deltas: String = events
        .iter()
        .filter_map(|event| match event {
            StreamEvent::TextDelta { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    check!(
        route,
        NAME,
        deltas == "Hello world",
        "expected ordered deltas `Hello world`, saw {deltas:?}"
    );
    check!(
        route,
        NAME,
        finished_count(&events) == 1,
        "expected exactly one Finished, saw {}",
        finished_count(&events)
    );
    let Some(response) = completed(&events) else {
        panic!(
            "[{}] {NAME}: expected a final completed outcome, saw {events:?}",
            route.name
        );
    };
    check!(
        route,
        NAME,
        response.item.text() == "Hello world",
        "expected completed text `Hello world`, saw {:?}",
        response.item.text()
    );
}

pub fn tool_call_is_complete_and_only_in_terminal(route: &RouteUnderTest) {
    const NAME: &str = "tool_call_is_complete_and_only_in_terminal";
    let events = ok(route, NAME, route.fixtures.tool_call_turn);
    let Some(response) = completed(&events) else {
        panic!(
            "[{}] {NAME}: expected completion, saw {events:?}",
            route.name
        );
    };
    let calls: Vec<_> = response.item.tool_calls().collect();
    check!(
        route,
        NAME,
        calls.len() == 1,
        "expected one terminal tool call, saw {}",
        calls.len()
    );
    let call = calls[0];
    check!(
        route,
        NAME,
        call.call_id == "call_1"
            && call.name == "read"
            && call.input == ToolInput::Json("{\"path\":\"a.txt\"}".to_string()),
        "expected call_1/read with exact raw arguments, saw {call:?}"
    );
    check!(
        route,
        NAME,
        response.stop == StopReason::ToolUse,
        "expected ToolUse stop, saw {:?}",
        response.stop
    );
}

pub fn tool_call_order_is_preserved(route: &RouteUnderTest) {
    const NAME: &str = "tool_call_order_is_preserved";
    let events = ok(route, NAME, route.fixtures.two_tool_calls);
    let Some(response) = completed(&events) else {
        panic!(
            "[{}] {NAME}: expected completion, saw {events:?}",
            route.name
        );
    };
    let calls: Vec<_> = response
        .item
        .tool_calls()
        .map(|call| (call.call_id.as_str(), call.name.as_str()))
        .collect();
    check!(
        route,
        NAME,
        calls == [("call_1", "read"), ("call_2", "grep")],
        "expected call_1/read then call_2/grep, saw {calls:?}"
    );
}

pub fn truncated_stream_is_failure_not_completion(route: &RouteUnderTest) {
    const NAME: &str = "truncated_stream_is_failure_not_completion";
    let events = ok(route, NAME, route.fixtures.truncated_tool_call);
    check!(
        route,
        NAME,
        matches!(terminal(&events), Some(Outcome::Failed(error)) if error.kind == ProviderErrorKind::Transport),
        "expected terminal Transport failure, saw {events:?}"
    );
    let surfaced = events.iter().any(|event| matches!(event, StreamEvent::Finished(Outcome::Completed(response)) if response.item.tool_calls().next().is_some()));
    check!(
        route,
        NAME,
        !surfaced,
        "expected no incomplete call to be surfaced, saw {events:?}"
    );
}

pub fn invalid_tool_json_is_preserved_raw(route: &RouteUnderTest) {
    const NAME: &str = "invalid_tool_json_is_preserved_raw";
    let events = ok(route, NAME, route.fixtures.invalid_tool_json);
    let call = completed(&events).and_then(|response| response.item.tool_calls().next());
    check!(
        route,
        NAME,
        matches!(call, Some(call) if call.input == ToolInput::Json("{\"path\": ".to_string())),
        "expected exact invalid raw JSON, saw {call:?}"
    );
}

pub fn error_event_is_single_failed_terminal(route: &RouteUnderTest) {
    const NAME: &str = "error_event_is_single_failed_terminal";
    let events = ok(route, NAME, route.fixtures.error_event);
    check!(
        route,
        NAME,
        finished_count(&events) == 1,
        "expected one terminal, saw {}",
        finished_count(&events)
    );
    let Some(Outcome::Failed(error)) = terminal(&events) else {
        panic!(
            "[{}] {NAME}: expected failed terminal, saw {events:?}",
            route.name
        );
    };
    let mut decoder = SseDecoder::new();
    let mut native_events = decoder.push(route.fixtures.error_event.as_bytes());
    if let Some(event) = decoder.finish() {
        native_events.push(event);
    }
    check!(
        route,
        NAME,
        native_events
            .iter()
            .all(|event| !error.message.contains(&event.data)),
        "expected diagnostic without response body text, saw message length {}",
        error.message.len()
    );
}

pub fn unknown_usage_is_none_not_zero(route: &RouteUnderTest) {
    const NAME: &str = "unknown_usage_is_none_not_zero";
    let events = ok(route, NAME, route.fixtures.no_usage);
    let usage = completed(&events).and_then(|response| response.usage);
    check!(
        route,
        NAME,
        usage.is_none(),
        "expected absent usage to remain None, saw {usage:?}"
    );
}

fn string_leaves(value: &serde_json::Value, output: &mut Vec<String>) {
    match value {
        serde_json::Value::String(value) => output.push(value.clone()),
        serde_json::Value::Array(values) => {
            values.iter().for_each(|value| string_leaves(value, output))
        }
        serde_json::Value::Object(values) => values
            .values()
            .for_each(|value| string_leaves(value, output)),
        _ => {}
    }
}

pub fn reasoning_replay_round_trips(route: &RouteUnderTest) {
    const NAME: &str = "reasoning_replay_round_trips";
    let events = ok(route, NAME, route.fixtures.reasoning_turn);
    let Some(response) = completed(&events) else {
        panic!(
            "[{}] {NAME}: expected completion, saw {events:?}",
            route.name
        );
    };
    let replay = response.item.blocks.iter().find_map(|block| match block {
        AssistantBlock::Reasoning { replay, .. } => replay.clone(),
        _ => None,
    });
    let Some(replay) = replay else {
        panic!(
            "[{}] {NAME}: expected reasoning replay data, saw none",
            route.name
        );
    };
    let mut follow_up = request();
    follow_up
        .history
        .push(Item::Assistant(response.item.clone()));
    let native = (route.follow_up_request)(&follow_up).to_string();
    let mut leaves = Vec::new();
    string_leaves(&replay.payload, &mut leaves);
    check!(
        route,
        NAME,
        !leaves.is_empty() && leaves.iter().all(|leaf| native.contains(leaf)),
        "expected every replay payload string leaf byte-exact in follow-up request, saw {} of {}",
        leaves.iter().filter(|leaf| native.contains(*leaf)).count(),
        leaves.len()
    );

    let mut foreign_item = response.item.clone();
    for block in &mut foreign_item.blocks {
        if let AssistantBlock::Reasoning {
            replay: Some(data), ..
        } = block
        {
            data.origin.route.push_str("-foreign");
        }
    }
    let mut foreign = request();
    foreign.history.push(Item::Assistant(foreign_item));
    let foreign_native = (route.follow_up_request)(&foreign).to_string();
    check!(
        route,
        NAME,
        leaves.iter().all(|leaf| !foreign_native.contains(leaf)),
        "expected foreign replay leaves to be absent, but at least one was present"
    );
}

pub fn nothing_after_terminal(route: &RouteUnderTest) {
    const NAME: &str = "nothing_after_terminal";
    let events = ok(route, NAME, route.fixtures.events_after_terminal);
    check!(
        route,
        NAME,
        finished_count(&events) == 1 && matches!(events.last(), Some(StreamEvent::Finished(_))),
        "expected exactly one last Finished and no later events, saw {events:?}"
    );
}

pub fn chunking_is_irrelevant(route: &RouteUnderTest) {
    const NAME: &str = "chunking_is_irrelevant";
    let fixtures = [
        route.fixtures.text_turn,
        route.fixtures.tool_call_turn,
        route.fixtures.two_tool_calls,
        route.fixtures.truncated_tool_call,
        route.fixtures.invalid_tool_json,
        route.fixtures.error_event,
        route.fixtures.no_usage,
        route.fixtures.reasoning_turn,
        route.fixtures.events_after_terminal,
    ];
    for (fixture_index, body) in fixtures.into_iter().enumerate() {
        let expected = ok(route, NAME, body);
        for at in 0..=body.len() {
            let bytes = body.as_bytes();
            let split = ScriptedResponse {
                status: 200,
                headers: Vec::new(),
                chunks: vec![bytes[..at].to_vec(), bytes[at..].to_vec()],
                end: BodyEnd::Eof,
            };
            let seen = match collect(route, split) {
                Ok(events) => events,
                Err(error) => panic!(
                    "[{}] {NAME}: fixture {fixture_index}, split {at}: {error}",
                    route.name
                ),
            };
            check!(
                route,
                NAME,
                seen == expected,
                "fixture {fixture_index}, split at byte offset {at}: expected {expected:?}, saw {seen:?}"
            );
        }
    }
}

async fn cancelled_events(
    route: &RouteUnderTest,
    response: ScriptedResponse,
    mid: bool,
) -> Result<Vec<StreamEvent>, String> {
    let provider = (route.build)(ScriptedTransport::new(vec![response]));
    let cancel = p1_contracts::CancellationToken::new();
    let mut stream = bounded(provider.stream(request(), cancel.clone()))
        .await
        .map_err(str::to_string)?
        .map_err(|e| format!("setup error ({:?})", e.kind))?;
    let mut events = Vec::new();
    if mid {
        let first = bounded(stream.next())
            .await
            .map_err(str::to_string)?
            .ok_or("stream ended before mid-stream cancellation")?;
        events.push(first);
    } else {
        let pending = stream.next();
        tokio::pin!(pending);
        tokio::select! {
            biased;
            item = &mut pending => return Err(format!("stream produced before cancellation: {item:?}")),
            _ = tokio::task::yield_now() => {}
        }
    }
    cancel.cancel();
    let rest = bounded(async {
        let mut out = Vec::new();
        while let Some(event) = stream.next().await {
            out.push(event);
        }
        out
    })
    .await
    .map_err(str::to_string)?;
    events.extend(rest);
    Ok(events)
}

pub fn cancel_before_first_byte(route: &RouteUnderTest) {
    const NAME: &str = "cancel_before_first_byte";
    let response = ScriptedResponse {
        status: 200,
        headers: Vec::new(),
        chunks: Vec::new(),
        end: BodyEnd::Hang,
    };
    let seen = runtime().block_on(cancelled_events(route, response, false));
    let events = match seen {
        Ok(events) => events,
        Err(error) => panic!("[{}] {NAME}: {error}", route.name),
    };
    check!(
        route,
        NAME,
        events.len() == 1 && matches!(events[0], StreamEvent::Finished(Outcome::Cancelled)),
        "expected prompt single Cancelled terminal, saw {events:?}"
    );
}

pub fn cancel_mid_stream(route: &RouteUnderTest) {
    const NAME: &str = "cancel_mid_stream";
    let body = route.fixtures.text_turn;
    let at = body.find("\n\n").map_or(body.len(), |index| index + 2);
    let response = ScriptedResponse {
        status: 200,
        headers: Vec::new(),
        chunks: vec![body.as_bytes()[..at].to_vec()],
        end: BodyEnd::Hang,
    };
    let seen = runtime().block_on(cancelled_events(route, response, true));
    let events = match seen {
        Ok(events) => events,
        Err(error) => panic!("[{}] {NAME}: {error}", route.name),
    };
    check!(
        route,
        NAME,
        finished_count(&events) == 1 && matches!(terminal(&events), Some(Outcome::Cancelled)),
        "expected one final Cancelled after visible output, saw {events:?}"
    );
}

fn status(status: u16) -> ScriptedResponse {
    ScriptedResponse {
        status,
        headers: Vec::new(),
        chunks: Vec::new(),
        end: BodyEnd::Eof,
    }
}

fn http_collect(
    route: &RouteUnderTest,
    name: &str,
    script: Vec<ScriptedResponse>,
) -> (Vec<StreamEvent>, ScriptedTransport) {
    let transport = ScriptedTransport::new(script);
    let recorded = transport.clone();
    let result = runtime().block_on(async {
        let provider = (route.build)(transport);
        collect_provider(&provider, request(), p1_contracts::CancellationToken::new()).await
    });
    match result {
        Ok(events) => (events, recorded),
        Err(error) => panic!("[{}] {name}: expected stream, saw {error}", route.name),
    }
}

pub fn http_401_refreshes_once_then_fails_authentication(route: &RouteUnderTest) {
    const NAME: &str = "http_401_refreshes_once_then_fails_authentication";
    let (events, transport) = http_collect(route, NAME, vec![status(401), status(401)]);
    let requests = transport.requests();
    check!(
        route,
        NAME,
        requests.len() == 2,
        "expected exactly two requests, saw {}",
        requests.len()
    );
    let refreshed = format!("{}-refreshed", route.fake_bearer);
    check!(
        route,
        NAME,
        requests.get(1).is_some_and(|request| request
            .headers
            .iter()
            .any(|(_, value)| value.contains(&refreshed))),
        "expected second request to carry refreshed bearer in a header value, saw no matching value"
    );
    check!(
        route,
        NAME,
        matches!(terminal(&events), Some(Outcome::Failed(error)) if error.kind == ProviderErrorKind::Authentication),
        "expected Authentication terminal, saw {events:?}"
    );
}

pub fn http_429_retries_then_succeeds(route: &RouteUnderTest) {
    const NAME: &str = "http_429_retries_then_succeeds";
    let (events, transport) = http_collect(
        route,
        NAME,
        vec![
            status(429),
            ScriptedResponse::ok_sse(route.fixtures.text_turn),
        ],
    );
    check!(
        route,
        NAME,
        transport.requests().len() == 2,
        "expected two requests, saw {}",
        transport.requests().len()
    );
    check!(
        route,
        NAME,
        matches!(terminal(&events), Some(Outcome::Completed(_))),
        "expected successful retry, saw {events:?}"
    );
}

pub fn http_500_exhausts_budget_then_fails_transport(route: &RouteUnderTest) {
    const NAME: &str = "http_500_exhausts_budget_then_fails_transport";
    let (events, transport) = http_collect(
        route,
        NAME,
        vec![status(500), status(500), status(500), status(500)],
    );
    check!(
        route,
        NAME,
        transport.requests().len() == 4,
        "expected four requests, saw {}",
        transport.requests().len()
    );
    check!(
        route,
        NAME,
        matches!(terminal(&events), Some(Outcome::Failed(error)) if error.kind == ProviderErrorKind::Transport),
        "expected Transport terminal, saw {events:?}"
    );
}

pub fn http_400_is_invalid_request_without_retry(route: &RouteUnderTest) {
    const NAME: &str = "http_400_is_invalid_request_without_retry";
    let (events, transport) = http_collect(route, NAME, vec![status(400)]);
    check!(
        route,
        NAME,
        transport.requests().len() == 1,
        "expected one request, saw {}",
        transport.requests().len()
    );
    check!(
        route,
        NAME,
        matches!(terminal(&events), Some(Outcome::Failed(error)) if error.kind == ProviderErrorKind::InvalidRequest),
        "expected InvalidRequest terminal, saw {events:?}"
    );
}

pub fn no_retry_after_visible_output(route: &RouteUnderTest) {
    const NAME: &str = "no_retry_after_visible_output";
    let body = route.fixtures.text_turn;
    let at = body.find("\n\n").map_or(body.len(), |index| index + 2);
    let broken = ScriptedResponse {
        status: 200,
        headers: Vec::new(),
        chunks: vec![body.as_bytes()[..at].to_vec()],
        end: BodyEnd::Error("conformance connection reset".to_string()),
    };
    let (events, transport) =
        http_collect(route, NAME, vec![broken, ScriptedResponse::ok_sse(body)]);
    check!(
        route,
        NAME,
        transport.requests().len() == 1,
        "expected one request after visible output, saw {}",
        transport.requests().len()
    );
    check!(
        route,
        NAME,
        matches!(terminal(&events), Some(Outcome::Failed(_))),
        "expected failed terminal, saw {events:?}"
    );
}

pub fn setup_error_is_only_for_invalid_requests(route: &RouteUnderTest) {
    const NAME: &str = "setup_error_is_only_for_invalid_requests";
    let transport = ScriptedTransport::new(vec![
        ScriptedResponse::connect_error("refused"),
        ScriptedResponse::connect_error("refused"),
        ScriptedResponse::connect_error("refused"),
        ScriptedResponse::connect_error("refused"),
    ]);
    let recorded = transport.clone();
    let result = runtime().block_on(async {
        let provider = (route.build)(transport);
        let validation = provider.validate(&request());
        let stream =
            bounded(provider.stream(request(), p1_contracts::CancellationToken::new())).await;
        (
            validation,
            match stream {
                Ok(Ok(mut stream)) => bounded(async {
                    let mut events = Vec::new();
                    while let Some(event) = stream.next().await {
                        events.push(event);
                    }
                    events
                })
                .await
                .map_err(str::to_string),
                Ok(Err(error)) => Err(format!("setup error ({:?})", error.kind)),
                Err(error) => Err(error.to_string()),
            },
        )
    });
    check!(
        route,
        NAME,
        result.0.is_ok(),
        "expected canonical request to validate, saw {:?}",
        result.0.as_ref().err().map(|e| e.kind)
    );
    let events = match result.1 {
        Ok(events) => events,
        Err(error) => panic!(
            "[{}] {NAME}: expected network failure in stream, saw {error}",
            route.name
        ),
    };
    check!(
        route,
        NAME,
        recorded.requests().len() == 4,
        "expected network retries inside stream, saw {} requests",
        recorded.requests().len()
    );
    check!(
        route,
        NAME,
        matches!(terminal(&events), Some(Outcome::Failed(error)) if error.kind == ProviderErrorKind::Transport),
        "expected terminal Transport failure, saw {events:?}"
    );

    // The positive half: a request the route cannot carry fails validation AND is a
    // setup error of kind InvalidRequest, before any network use.
    let transport = ScriptedTransport::new(vec![]);
    let recorded = transport.clone();
    let (validation, setup) = runtime().block_on(async {
        let provider = (route.build)(transport);
        let invalid = (route.invalid_request)();
        let validation = provider.validate(&invalid).map_err(|error| error.kind);
        let setup =
            match bounded(provider.stream(invalid, p1_contracts::CancellationToken::new())).await {
                Ok(Ok(_)) => Ok(()),
                Ok(Err(error)) => Err(Some(error.kind)),
                Err(_) => Err(None),
            };
        (validation, setup)
    });
    check!(
        route,
        NAME,
        validation == Err(ProviderErrorKind::InvalidRequest),
        "expected validate to reject the invalid request as InvalidRequest, saw {validation:?}"
    );
    check!(
        route,
        NAME,
        setup == Err(Some(ProviderErrorKind::InvalidRequest)),
        "expected stream() to return Err(InvalidRequest) for the invalid request, saw {setup:?}"
    );
    check!(
        route,
        NAME,
        recorded.requests().is_empty(),
        "expected no network use for an invalid request, saw {} requests",
        recorded.requests().len()
    );
}

pub fn credentials_never_leak(route: &RouteUnderTest) {
    const NAME: &str = "credentials_never_leak";
    let (events, _) = http_collect(route, NAME, vec![status(400)]);
    let rendered = format!("{events:?}");
    check!(
        route,
        NAME,
        !rendered.contains(route.fake_bearer),
        "expected bearer absent from StreamEvent Debug, but it was present"
    );
    if let Some(Outcome::Failed(error)) = terminal(&events) {
        let error_text = format!("{error:?} {error}");
        check!(
            route,
            NAME,
            !error_text.contains(route.fake_bearer),
            "expected bearer absent from ProviderError formatting, but it was present"
        );
    }
}

pub fn run_all(route: &RouteUnderTest) {
    text_deltas_then_single_terminal(route);
    tool_call_is_complete_and_only_in_terminal(route);
    tool_call_order_is_preserved(route);
    truncated_stream_is_failure_not_completion(route);
    invalid_tool_json_is_preserved_raw(route);
    error_event_is_single_failed_terminal(route);
    unknown_usage_is_none_not_zero(route);
    reasoning_replay_round_trips(route);
    nothing_after_terminal(route);
    chunking_is_irrelevant(route);
    cancel_before_first_byte(route);
    cancel_mid_stream(route);
    http_401_refreshes_once_then_fails_authentication(route);
    http_429_retries_then_succeeds(route);
    http_500_exhausts_budget_then_fails_transport(route);
    http_400_is_invalid_request_without_retry(route);
    no_retry_after_visible_output(route);
    setup_error_is_only_for_invalid_requests(route);
    credentials_never_leak(route);
}
