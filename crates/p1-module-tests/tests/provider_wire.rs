//! Provider wire boundary (S4.4, issue #309): replay data and errors cross the module
//! boundary unchanged.
//!
//! A provider component parses the SSE body, hands the host `stream-event` JSON, and
//! lowers its next request from the `history-item` JSON the host gives back. Three
//! promises of the adapters are checked here, for all three of them:
//!
//! 1. a replay payload travels WITHOUT interpretation: the wire value the parser read is
//!    the value the next request sends, byte for byte, including non-ASCII text, JSON
//!    escapes, a trailing space and a 64 KiB value. `WireReplayData.payload` is opaque
//!    (`docs/design/modules/protocol.md`), so nothing between the two may touch it.
//! 2. it is tagged with the CONFIGURED origin, never the dated alias a server echoes
//!    (ADR-0018). `replay` gates on that origin, so the alias would make the session's
//!    own thinking foreign to itself.
//! 3. `validate` over the CURRENT history is the compatibility policy (ADR-0049): our
//!    own replay data at another version is refused by name, a foreign origin is
//!    dropped, and neither decision changes on the way across.
//!
//! Every SSE body is written here; nothing in this file opens a network connection,
//! reads a clock or asserts a duration. The parsers are fed through
//! `p1_provider_http::SseDecoder`, the portable half a component decodes with, and every
//! value crosses the boundary as JSON text exactly as `p1-module-protocol` defines it.
//! The three adapter crates are compiled with `default-features = false`: this file uses
//! their portable halves only, and a native dependency would fail the build here.

use p1_contracts::history::{AssistantBlock, AssistantItem, Item, Origin, ReplayData};
use p1_contracts::serde_json::{self, Value, json};
use p1_contracts::{
    ModelOptions, Outcome, ProviderError, ProviderErrorKind, ProviderRequest, StreamEvent,
};
use p1_model_profile::ModelProfile;
use p1_module_protocol::{WireItem, WireProviderError, WireStreamEvent};
use p1_provider_http::{ResponseParser, SseDecoder, SseEvent};

// ---------------------------------------------------------------------------
// The boundary itself
// ---------------------------------------------------------------------------

/// One SSE event as a body spells it: its `data:` line and the blank line that ends it.
fn sse(data: Value) -> String {
    format!("data: {data}\n\n")
}

/// Parse a body through `parser`, pushing at most at `split` (a byte offset; the whole
/// body at once when `None`), and return the events a host would receive: every event is
/// converted to its `stream-event` JSON text and back, and the stream ends exactly once —
/// with the parser's own terminal when it produced one, else with `on_end`.
///
/// A parser that produced its terminal ends the stream: nothing after it is forwarded,
/// which is the driver's rule for every route.
fn parse(parser: &mut impl ResponseParser, body: &str, split: Option<usize>) -> Vec<StreamEvent> {
    let bytes = body.as_bytes();
    let offsets = match split {
        Some(split) => vec![split, bytes.len()],
        None => vec![bytes.len()],
    };
    let mut decoder = SseDecoder::new();
    let mut events = Vec::new();
    let mut start = 0;
    for end in offsets {
        if let Some(outcome) = feed(parser, decoder.push(&bytes[start..end]), &mut events) {
            events.push(StreamEvent::Finished(outcome));
            return events;
        }
        start = end;
    }
    if let Some(event) = decoder.finish()
        && let Some(outcome) = feed(parser, vec![event], &mut events)
    {
        events.push(StreamEvent::Finished(outcome));
        return events;
    }
    let outcome = parser.on_end();
    events.push(StreamEvent::Finished(outcome));
    events
}

/// Feed decoded SSE events through the parser, converting what it returns to its
/// `stream-event` JSON text and back. The terminal outcome ends the stream.
fn feed(
    parser: &mut impl ResponseParser,
    decoded: Vec<SseEvent>,
    out: &mut Vec<StreamEvent>,
) -> Option<Outcome> {
    for event in decoded {
        for event in parser.on_event(event) {
            let wire = WireStreamEvent::from(event);
            let text = serde_json::to_string(&wire).expect("a stream event encodes");
            let wire: WireStreamEvent =
                serde_json::from_str(&text).expect("a stream event decodes");
            let event = StreamEvent::try_from(wire).expect("a stream event converts");
            if let StreamEvent::Finished(outcome) = &event {
                return Some(outcome.clone());
            }
            out.push(event);
        }
    }
    None
}

/// The stream's one terminal outcome. Every case asserts there is exactly one: a stream
/// that ends twice, or never, is not what the host is promised.
fn finished(events: &[StreamEvent]) -> &Outcome {
    let terminals: Vec<&Outcome> = events
        .iter()
        .filter_map(|event| match event {
            StreamEvent::Finished(outcome) => Some(outcome),
            _ => None,
        })
        .collect();
    assert_eq!(
        terminals.len(),
        1,
        "a stream ends in exactly one terminal event: {events:?}"
    );
    terminals[0]
}

/// The completed item of a stream that must have completed.
fn completed(outcome: &Outcome) -> &AssistantItem {
    match outcome {
        Outcome::Completed(response) => &response.item,
        other => panic!("expected a completion, saw {other:?}"),
    }
}

/// The assistant item of a history item; it is the only carrier of replay data.
fn assistant(item: &Item) -> &AssistantItem {
    match item {
        Item::Assistant(assistant) => assistant,
        other => panic!("expected an assistant item, saw {other:?}"),
    }
}

/// The reasoning block's replay data, the value under test.
fn replay_of(item: &AssistantItem) -> &ReplayData {
    item.blocks
        .iter()
        .find_map(|block| match block {
            AssistantBlock::Reasoning {
                replay: Some(data), ..
            } => Some(data),
            _ => None,
        })
        .expect("a reasoning block with replay data")
}

/// One item through the boundary's `history-item` JSON and back.
fn history_item(item: &Item) -> Item {
    let wire = WireItem::from(item.clone());
    let text = serde_json::to_string(&wire).expect("a history item encodes");
    let wire: WireItem = serde_json::from_str(&text).expect("a history item decodes");
    Item::from(wire)
}

/// The same item with its replay data at `version`: what a build at another version of
/// this adapter would have recorded.
fn at_version(item: Item, version: u32) -> Item {
    let mut item = item;
    if let Item::Assistant(assistant) = &mut item {
        for block in &mut assistant.blocks {
            if let AssistantBlock::Reasoning {
                replay: Some(data), ..
            } = block
            {
                data.version = version;
            }
        }
    }
    item
}

/// Set every string in `value` to `sentinel`, whatever key holds it: a foreign payload
/// must carry a value no request of ours may reproduce.
fn stamp(value: &mut Value, sentinel: &str) {
    match value {
        Value::String(text) => *text = sentinel.to_string(),
        Value::Array(values) => values.iter_mut().for_each(|value| stamp(value, sentinel)),
        Value::Object(fields) => fields.values_mut().for_each(|value| stamp(value, sentinel)),
        _ => {}
    }
}

/// The same item recorded on `origin`, carrying `sentinel` as its replay value and
/// `text` as the reasoning the model showed: neither may reach a request (ADR-0018).
fn foreign(item: Item, origin: Origin, sentinel: &str, text: &str) -> Item {
    let mut item = item;
    if let Item::Assistant(assistant) = &mut item {
        assistant.origin = origin.clone();
        for block in &mut assistant.blocks {
            if let AssistantBlock::Reasoning {
                text: shown,
                replay: Some(data),
            } = block
            {
                data.origin = origin.clone();
                stamp(&mut data.payload, sentinel);
                *shown = text.to_string();
            }
        }
    }
    item
}

fn user(text: &str) -> Item {
    Item::User {
        text: text.to_string(),
    }
}

fn request(history: Vec<Item>) -> ProviderRequest {
    ProviderRequest {
        system_prompt: "system".to_string(),
        history,
        tools: Vec::new(),
        options: ModelOptions::default(),
    }
}

/// The first string a fixture body spells under `key`. Every replay case reads its
/// expectation from the body itself (decoding the body's JSON escapes exactly as an
/// adapter does), never from the value the implementation happens to produce.
fn body_value(body: &str, key: &str) -> String {
    fn find(value: &Value, key: &str) -> Option<String> {
        match value {
            Value::Object(fields) => match fields.get(key) {
                Some(Value::String(text)) => Some(text.clone()),
                _ => fields.values().find_map(|value| find(value, key)),
            },
            Value::Array(values) => values.iter().find_map(|value| find(value, key)),
            _ => None,
        }
    }
    for line in body.lines() {
        let Some(data) = line.strip_prefix("data:") else {
            continue;
        };
        let Ok(value) = serde_json::from_str::<Value>(data.trim()) else {
            continue;
        };
        if let Some(text) = find(&value, key) {
            return text;
        }
    }
    panic!("the fixture body spells no {key}");
}

/// A value spelled with `\u` escapes only: a NUL, a two-byte character, a three-byte
/// character and a surrogate pair, then a trailing space. The escapes are JSON spelling,
/// never bytes, so a payload that kept them (or a decoder that lost them) is wrong.
const ESCAPED: &str = r"sig\u0000\u00e9\u2603\ud83d\ude00 ";

/// What [`ESCAPED`] decodes to, written out. The trailing space is part of the value.
const DECODED: &str = "sig\u{0}é☃😀 ";

/// A 64 KiB wire value: carried whole, never truncated, summarized or re-encoded.
fn big_value() -> String {
    format!("{}TAIL", "x".repeat(64 * 1024 - 4))
}

/// A leaf of a foreign replay payload and the reasoning it showed: neither may appear in
/// a lowered request.
const FOREIGN_SENTINEL: &str = "FOREIGN-REPLAY-SENTINEL";
const FOREIGN_TEXT: &str = "foreign reasoning text";

// ---------------------------------------------------------------------------
// Errors: every kind, and each adapter's own status classification
// ---------------------------------------------------------------------------

/// The contract's closed set of error kinds, spelled as the boundary spells them.
/// `kind_name` matches exhaustively: a kind added to the contract breaks this file's
/// build instead of crossing the boundary untested.
const KINDS: [ProviderErrorKind; 9] = [
    ProviderErrorKind::InvalidRequest,
    ProviderErrorKind::Authentication,
    ProviderErrorKind::InsufficientBalance,
    ProviderErrorKind::NotEntitled,
    ProviderErrorKind::UsageLimitExhausted,
    ProviderErrorKind::RateLimited,
    ProviderErrorKind::ContextWindowExceeded,
    ProviderErrorKind::Transport,
    ProviderErrorKind::Protocol,
];

fn kind_name(kind: ProviderErrorKind) -> &'static str {
    match kind {
        ProviderErrorKind::InvalidRequest => "invalid_request",
        ProviderErrorKind::Authentication => "authentication",
        ProviderErrorKind::InsufficientBalance => "insufficient_balance",
        ProviderErrorKind::NotEntitled => "not_entitled",
        ProviderErrorKind::UsageLimitExhausted => "usage_limit_exhausted",
        ProviderErrorKind::RateLimited => "rate_limited",
        ProviderErrorKind::ContextWindowExceeded => "context_window_exceeded",
        ProviderErrorKind::Transport => "transport",
        ProviderErrorKind::Protocol => "protocol",
    }
}

#[test]
fn every_error_kind_and_message_crosses_the_boundary_unchanged() {
    for kind in KINDS {
        let name = kind_name(kind);
        let error = ProviderError::new(kind, format!("diagnostic for {name}"));
        let text = serde_json::to_string(&WireProviderError::from(error.clone()))
            .expect("a provider error encodes");
        assert!(text.contains(&format!("\"kind\":\"{name}\"")), "{text}");
        let wire: WireProviderError =
            serde_json::from_str(&text).expect("a provider error decodes");
        assert_eq!(ProviderError::from(wire), error, "{name}");
    }
}

/// One classification before and after the boundary: the kind and the diagnostic a
/// component reports must be the ones the native driver reports for the same response.
fn http_error_survives(
    parser: &impl ResponseParser,
    status: u16,
    body: &[u8],
    expected: ProviderErrorKind,
) {
    let error = parser.on_http_error(
        status,
        &[("x-request-id".to_string(), "req-wire".to_string())],
        body,
    );
    assert_eq!(error.kind, expected, "http {status}: {}", error.message);
    let text = serde_json::to_string(&WireProviderError::from(error.clone()))
        .expect("a provider error encodes");
    let wire: WireProviderError = serde_json::from_str(&text).expect("a provider error decodes");
    assert_eq!(ProviderError::from(wire), error, "http {status}");
}

// ---------------------------------------------------------------------------
// Anthropic Messages
// ---------------------------------------------------------------------------

const ANTHROPIC_ROUTE: &str = "anthropic-messages/claude-subscription";
const ANTHROPIC_MODEL: &str = "claude-sonnet-5";
/// The dated alias the server echoes instead of the model that was configured.
const ANTHROPIC_ECHO: &str = "claude-sonnet-5-20260101";

const ANTHROPIC_PROFILE: &str = r#"
id       = "claude-sonnet-5"
revision = 1
model_id = "claude-sonnet-5"
family   = "claude"
thinking = "effort-level"
efforts  = ["low", "medium", "high", "extra_high", "max"]
"#;

fn anthropic_route() -> p1_provider_anthropic::MessagesRoute {
    p1_provider_anthropic::MessagesRoute {
        origin_route: ANTHROPIC_ROUTE.to_string(),
        endpoint: "https://api.anthropic.com".to_string(),
        account: p1_provider_anthropic::MessagesAccount::ClaudeCodeSubscription,
        long_context: false,
    }
}

fn anthropic_profile() -> ModelProfile {
    ModelProfile::from_toml("claude-sonnet-5", ANTHROPIC_PROFILE)
        .expect("the fixture profile parses")
}

fn anthropic_parser() -> p1_provider_anthropic::AnthropicParser {
    p1_provider_anthropic::AnthropicParser::new(ANTHROPIC_ROUTE, ANTHROPIC_MODEL)
}

/// The `signature_delta` line, with `SIGNATURE` standing in for the value: the template
/// is raw text, so the value's escapes stay the escapes the body spelled.
const ANTHROPIC_SIGNATURE_EVENT: &str = r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"SIGNATURE"}}"#;

/// The events of one reasoning turn, one SSE event each, so a case can also send a
/// PREFIX of them (a stream cut inside the reasoning block).
fn anthropic_events(signature: &str) -> Vec<String> {
    vec![
        sse(json!({
            "type": "message_start",
            "message": {
                "id": "msg_replay",
                "model": ANTHROPIC_ECHO,
                "usage": { "input_tokens": 3 },
            },
        })),
        sse(json!({
            "type": "content_block_start",
            "index": 0,
            "content_block": { "type": "thinking", "thinking": "" },
        })),
        sse(json!({
            "type": "content_block_delta",
            "index": 0,
            "delta": { "type": "thinking_delta", "thinking": "deliberation" },
        })),
        format!(
            "{}\n\n",
            ANTHROPIC_SIGNATURE_EVENT.replace("SIGNATURE", signature)
        ),
        sse(json!({ "type": "content_block_stop", "index": 0 })),
        sse(json!({
            "type": "content_block_start",
            "index": 1,
            "content_block": { "type": "text", "text": "" },
        })),
        sse(json!({
            "type": "content_block_delta",
            "index": 1,
            "delta": { "type": "text_delta", "text": "answer" },
        })),
        sse(json!({ "type": "content_block_stop", "index": 1 })),
        sse(json!({
            "type": "message_delta",
            "delta": { "stop_reason": "end_turn" },
            "usage": { "output_tokens": 9 },
        })),
        sse(json!({ "type": "message_stop" })),
    ]
}

fn anthropic_turn(signature: &str) -> String {
    anthropic_events(signature).concat()
}

/// The reasoning block's own events only: the stream ends there, with no terminal event.
fn anthropic_cut_turn(signature: &str) -> String {
    anthropic_events(signature)[..4].concat()
}

/// One Messages turn across the boundary: the parser's terminal item (already through
/// `stream-event` JSON) into a history, that history through `history-item` JSON, and
/// the follow-up request lowered. Returns the body, the item the host holds and the
/// lowered request body.
fn anthropic_round_trip(signature: &str) -> (String, Item, Value) {
    let body = anthropic_turn(signature);
    let events = parse(&mut anthropic_parser(), &body, None);
    let item = history_item(&Item::Assistant(completed(finished(&events)).clone()));
    let lowered = p1_provider_anthropic::lower_request(
        &anthropic_route(),
        ANTHROPIC_MODEL,
        &anthropic_profile(),
        &request(vec![user("go"), item.clone()]),
    )
    .expect("the follow-up request lowers");
    let lowered = serde_json::from_slice::<Value>(&lowered.body).expect("the body is JSON");
    (body, item, lowered)
}

/// The lowered `thinking` block: what went back to the provider.
fn anthropic_thinking_block(lowered: &Value) -> &Value {
    lowered["messages"]
        .as_array()
        .expect("a message array")
        .iter()
        .flat_map(|message| message["content"].as_array().into_iter().flatten())
        .find(|block| block["type"] == "thinking")
        .expect("a replayed thinking block")
}

fn anthropic_validate(request: &ProviderRequest) -> Result<(), ProviderError> {
    p1_provider_anthropic::validate_request(
        &anthropic_route(),
        ANTHROPIC_MODEL,
        &anthropic_profile(),
        request,
    )
}

fn anthropic_lower(request: &ProviderRequest) -> Result<Value, ProviderError> {
    p1_provider_anthropic::lower_request(
        &anthropic_route(),
        ANTHROPIC_MODEL,
        &anthropic_profile(),
        request,
    )
    .map(|lowered| serde_json::from_slice(&lowered.body).expect("the body is JSON"))
}

#[test]
fn anthropic_replay_bytes_travel_unchanged() {
    // The fixture spells escapes and ends with a space; these are the bytes they mean.
    assert_eq!(body_value(&anthropic_turn(ESCAPED), "signature"), DECODED);

    for signature in [ESCAPED.to_string(), big_value()] {
        let (body, item, lowered) = anthropic_round_trip(&signature);
        let expected = body_value(&body, "signature");
        if signature == ESCAPED {
            assert_eq!(expected, DECODED);
        } else {
            assert_eq!(expected.len(), 64 * 1024, "the fixture is 64 KiB");
        }

        let replay = replay_of(assistant(&item));
        assert_eq!(replay.version, p1_provider_anthropic::REPLAY_VERSION);
        assert_eq!(
            replay
                .payload
                .get("signature")
                .and_then(Value::as_str)
                .expect("a signature")
                .as_bytes(),
            expected.as_bytes(),
            "the payload holds the wire value byte for byte"
        );
        // The configured origin is kept; the alias the server echoed never becomes it.
        assert_eq!(replay.origin.route, ANTHROPIC_ROUTE);
        assert_eq!(replay.origin.model, ANTHROPIC_MODEL);
        assert_ne!(replay.origin.model, ANTHROPIC_ECHO);
        assert_eq!(assistant(&item).origin, replay.origin);

        let replayed = anthropic_thinking_block(&lowered);
        assert_eq!(
            replayed["signature"]
                .as_str()
                .expect("a signature")
                .as_bytes(),
            expected.as_bytes(),
            "the replayed value is the payload's value"
        );
        assert_eq!(replayed["thinking"], json!("deliberation"));
        let text = serde_json::to_string(&lowered).expect("the body encodes");
        assert!(
            !text.contains(ANTHROPIC_ECHO),
            "the echoed alias must not reach a request"
        );
    }
}

/// ADR-0049 and ADR-0018 at the boundary: our own replay data at another version is
/// refused by name, a foreign origin is dropped and contributes nothing.
#[test]
fn anthropic_refuses_an_own_replay_from_another_version_and_drops_a_foreign_one() {
    let (_, recorded, _) = anthropic_round_trip(ESCAPED);
    anthropic_validate(&request(vec![user("go"), recorded.clone()]))
        .expect("the recorded replay is this build's version");

    let stale = at_version(recorded.clone(), p1_provider_anthropic::REPLAY_VERSION + 1);
    let stale_request = request(vec![user("go"), stale]);
    let error = anthropic_validate(&stale_request).expect_err("another version is refused");
    assert_eq!(error.kind, ProviderErrorKind::InvalidRequest);
    for part in [ANTHROPIC_ROUTE, ANTHROPIC_MODEL, "version 2", "version 1"] {
        assert!(error.message.contains(part), "{part}: {}", error.message);
    }
    // `lower` shares the one history mapping, so activation fails with the same error.
    assert!(anthropic_lower(&stale_request).is_err());

    for origin in [
        Origin {
            route: "elsewhere/foreign-route".to_string(),
            model: ANTHROPIC_MODEL.to_string(),
        },
        Origin {
            route: ANTHROPIC_ROUTE.to_string(),
            model: "claude-opus-5".to_string(),
        },
    ] {
        let shown = foreign(recorded.clone(), origin, FOREIGN_SENTINEL, FOREIGN_TEXT);
        // The foreign block at another version is still foreign: dropped, not refused.
        let carried = request(vec![user("go"), at_version(shown.clone(), 2)]);
        anthropic_validate(&carried).expect("a foreign history is carried, never refused");
        let lowered = anthropic_lower(&carried).expect("the foreign turn still lowers");
        let text = serde_json::to_string(&lowered).expect("the body encodes");
        for part in [FOREIGN_SENTINEL, FOREIGN_TEXT] {
            assert!(!text.contains(part), "{part} reached {text}");
        }
    }
}

#[test]
fn anthropic_partial_input_is_equivalent_and_a_cut_reasoning_turn_fails() {
    let body = anthropic_turn(ESCAPED);
    let whole = parse(&mut anthropic_parser(), &body, None);
    for split in 0..=body.len() {
        assert_eq!(
            parse(&mut anthropic_parser(), &body, Some(split)),
            whole,
            "split at byte offset {split}"
        );
    }

    // A stream cut inside the reasoning block is one failure, never a completed item
    // carrying a half payload.
    let events = parse(&mut anthropic_parser(), &anthropic_cut_turn(ESCAPED), None);
    assert!(matches!(finished(&events), Outcome::Failed(_)));
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, StreamEvent::Finished(Outcome::Completed(_)))),
        "{events:?}"
    );
}

#[test]
fn anthropic_http_errors_cross_unchanged() {
    let parser = anthropic_parser();
    let context_window = br#"{"error":{"type":"invalid_request_error","message":"prompt is too long for the context window"}}"#;
    for (status, body, expected) in [
        (
            400u16,
            context_window.as_slice(),
            ProviderErrorKind::ContextWindowExceeded,
        ),
        (
            400,
            br#"{"error":{"type":"invalid_request_error"}}"#,
            ProviderErrorKind::InvalidRequest,
        ),
        (
            401,
            br#"{"error":{"type":"authentication_error"}}"#,
            ProviderErrorKind::Authentication,
        ),
        (
            429,
            br#"{"error":{"type":"rate_limit_error"}}"#,
            ProviderErrorKind::RateLimited,
        ),
        (529, b"overloaded".as_slice(), ProviderErrorKind::Transport),
    ] {
        http_error_survives(&parser, status, body, expected);
    }
}

// ---------------------------------------------------------------------------
// OpenAI Responses (Codex subscription)
// ---------------------------------------------------------------------------

const OPENAI_ROUTE: &str = "openai-responses/codex-subscription";
const OPENAI_MODEL: &str = "gpt-6-sol";
/// The dated alias the server echoes instead of the model that was configured.
const OPENAI_ECHO: &str = "gpt-6-sol-20260101";

const OPENAI_PROFILE: &str = r#"
id       = "gpt-6-sol"
revision = 1
model_id = "gpt-6-sol"
family   = "gpt"
thinking = "effort-level"
efforts  = ["low", "medium", "high", "extra_high", "max"]
"#;

fn openai_route() -> p1_provider_openai::ResponsesRoute {
    p1_provider_openai::ResponsesRoute {
        origin_route: OPENAI_ROUTE.to_string(),
        endpoint: "https://chatgpt.com/backend-api".to_string(),
        account: p1_provider_openai::ResponsesAccount::CodexSubscription,
        transport: p1_provider_openai::ResponsesTransport::Sse,
    }
}

fn openai_profile() -> ModelProfile {
    ModelProfile::from_toml("gpt-6-sol", OPENAI_PROFILE).expect("the fixture profile parses")
}

fn openai_parser() -> p1_provider_openai::CodexResponseParser {
    p1_provider_openai::CodexResponseParser::new(OPENAI_ROUTE, OPENAI_MODEL)
}

/// The reasoning output item, with `ENCRYPTED` standing in for the encrypted content.
const OPENAI_REASONING_ITEM: &str = r#"data: {"type":"response.output_item.done","output_index":0,"item":{"id":"rs_1","type":"reasoning","encrypted_content":"ENCRYPTED","summary":[{"type":"summary_text","text":"first"}]}}"#;

fn openai_events(encrypted_content: &str) -> Vec<String> {
    vec![
        sse(json!({
            "type": "response.created",
            "response": { "id": "resp_replay", "model": OPENAI_ECHO },
        })),
        format!(
            "{}\n\n",
            OPENAI_REASONING_ITEM.replace("ENCRYPTED", encrypted_content)
        ),
        sse(json!({
            "type": "response.output_item.done",
            "output_index": 1,
            "item": {
                "id": "msg_1",
                "type": "message",
                "role": "assistant",
                "content": [{ "type": "output_text", "text": "answer" }],
            },
        })),
        sse(json!({
            "type": "response.completed",
            "response": { "id": "resp_replay" },
        })),
    ]
}

fn openai_turn(encrypted_content: &str) -> String {
    openai_events(encrypted_content).concat()
}

/// The reasoning item only: the stream ends there, with no terminal event.
fn openai_cut_turn(encrypted_content: &str) -> String {
    openai_events(encrypted_content)[..2].concat()
}

fn openai_round_trip(encrypted_content: &str) -> (String, Item, Value) {
    let body = openai_turn(encrypted_content);
    let events = parse(&mut openai_parser(), &body, None);
    let item = history_item(&Item::Assistant(completed(finished(&events)).clone()));
    let lowered = p1_provider_openai::lower_request(
        &openai_route(),
        OPENAI_MODEL,
        &openai_profile(),
        &request(vec![user("go"), item.clone()]),
    )
    .expect("the follow-up request lowers");
    let lowered = serde_json::from_slice::<Value>(&lowered.body).expect("the body is JSON");
    (body, item, lowered)
}

/// The replayed reasoning input item: what went back to the provider.
fn openai_reasoning_item(lowered: &Value) -> &Value {
    lowered["input"]
        .as_array()
        .expect("an input array")
        .iter()
        .find(|item| item["type"] == "reasoning")
        .expect("a replayed reasoning item")
}

/// The replay half of the history check, which needs the configured wire model: the
/// portable `validate_request` takes the route, profile and request only (the shape
/// S4.3.2's provider component calls it with).
fn openai_validate(request: &ProviderRequest) -> Result<(), ProviderError> {
    p1_provider_openai::validate_history(&openai_route(), OPENAI_MODEL, request)
}

fn openai_lower(request: &ProviderRequest) -> Result<Value, ProviderError> {
    p1_provider_openai::lower_request(&openai_route(), OPENAI_MODEL, &openai_profile(), request)
        .map(|lowered| serde_json::from_slice(&lowered.body).expect("the body is JSON"))
}

#[test]
fn openai_replay_bytes_travel_unchanged() {
    // The fixture spells escapes and ends with a space; these are the bytes they mean.
    assert_eq!(
        body_value(&openai_turn(ESCAPED), "encrypted_content"),
        DECODED
    );

    for encrypted_content in [ESCAPED.to_string(), big_value()] {
        let (body, item, lowered) = openai_round_trip(&encrypted_content);
        let expected = body_value(&body, "encrypted_content");
        if encrypted_content == ESCAPED {
            assert_eq!(expected, DECODED);
        } else {
            assert_eq!(expected.len(), 64 * 1024, "the fixture is 64 KiB");
        }

        let replay = replay_of(assistant(&item));
        assert_eq!(replay.version, p1_provider_openai::REPLAY_VERSION);
        assert_eq!(
            replay
                .payload
                .get("encrypted_content")
                .and_then(Value::as_str)
                .expect("an encrypted content")
                .as_bytes(),
            expected.as_bytes(),
            "the payload holds the wire value byte for byte"
        );
        assert_eq!(replay.origin.route, OPENAI_ROUTE);
        assert_eq!(replay.origin.model, OPENAI_MODEL);
        assert_ne!(replay.origin.model, OPENAI_ECHO);
        assert_eq!(assistant(&item).origin, replay.origin);

        let replayed = openai_reasoning_item(&lowered);
        assert_eq!(
            replayed["encrypted_content"]
                .as_str()
                .expect("an encrypted content")
                .as_bytes(),
            expected.as_bytes(),
            "the replayed value is the payload's value"
        );
        let text = serde_json::to_string(&lowered).expect("the body encodes");
        assert!(
            !text.contains(OPENAI_ECHO),
            "the echoed alias must not reach a request"
        );
    }
}

#[test]
fn openai_refuses_an_own_replay_from_another_version_and_drops_a_foreign_one() {
    let (_, recorded, _) = openai_round_trip(ESCAPED);
    openai_validate(&request(vec![user("go"), recorded.clone()]))
        .expect("the recorded replay is this build's version");

    let stale = at_version(recorded.clone(), p1_provider_openai::REPLAY_VERSION + 1);
    let stale_request = request(vec![user("go"), stale]);
    let error = openai_validate(&stale_request).expect_err("another version is refused");
    assert_eq!(error.kind, ProviderErrorKind::InvalidRequest);
    for part in [OPENAI_ROUTE, OPENAI_MODEL, "version 2", "version 1"] {
        assert!(error.message.contains(part), "{part}: {}", error.message);
    }
    // `lower_request` runs the same lowering, so nothing sends what `validate` refuses.
    assert!(openai_lower(&stale_request).is_err());

    for origin in [
        Origin {
            route: "elsewhere/foreign-route".to_string(),
            model: OPENAI_MODEL.to_string(),
        },
        Origin {
            route: OPENAI_ROUTE.to_string(),
            model: "gpt-6-sol-mini".to_string(),
        },
    ] {
        let shown = foreign(recorded.clone(), origin, FOREIGN_SENTINEL, FOREIGN_TEXT);
        let carried = request(vec![user("go"), at_version(shown, 2)]);
        openai_validate(&carried).expect("a foreign history is carried, never refused");
        let lowered = openai_lower(&carried).expect("the foreign turn still lowers");
        let text = serde_json::to_string(&lowered).expect("the body encodes");
        for part in [FOREIGN_SENTINEL, FOREIGN_TEXT] {
            assert!(!text.contains(part), "{part} reached {text}");
        }
    }
}

#[test]
fn openai_partial_input_is_equivalent_and_a_cut_reasoning_turn_fails() {
    let body = openai_turn(ESCAPED);
    let whole = parse(&mut openai_parser(), &body, None);
    for split in 0..=body.len() {
        assert_eq!(
            parse(&mut openai_parser(), &body, Some(split)),
            whole,
            "split at byte offset {split}"
        );
    }

    let events = parse(&mut openai_parser(), &openai_cut_turn(ESCAPED), None);
    assert!(matches!(finished(&events), Outcome::Failed(_)));
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, StreamEvent::Finished(Outcome::Completed(_)))),
        "{events:?}"
    );
}

#[test]
fn openai_http_errors_cross_unchanged() {
    let parser = openai_parser();
    for (status, body, expected) in [
        (
            400u16,
            br#"{"error":{"code":"context_length_exceeded"}}"#.as_slice(),
            ProviderErrorKind::ContextWindowExceeded,
        ),
        (
            401,
            br#"{"error":{"code":"invalid_api_key"}}"#,
            ProviderErrorKind::Authentication,
        ),
        (
            404,
            br#"{"error":{"code":"model_not_found"}}"#,
            ProviderErrorKind::InvalidRequest,
        ),
        (
            429,
            br#"{"error":{"code":"rate_limit_exceeded"}}"#,
            ProviderErrorKind::RateLimited,
        ),
        (
            503,
            b"upstream unavailable".as_slice(),
            ProviderErrorKind::Transport,
        ),
    ] {
        http_error_survives(&parser, status, body, expected);
    }
}

// ---------------------------------------------------------------------------
// OpenAI Chat Completions
// ---------------------------------------------------------------------------

const CHAT_ROUTE: &str = "openai-chat/opencode-go-subscription";
const CHAT_MODEL: &str = "configured-model";
/// The dated alias the server echoes instead of the model that was configured.
const CHAT_ECHO: &str = "dated-server-alias";

const CHAT_PROFILE: &str = r#"
id             = "chat-reasoning"
revision       = 1
model_id       = "chat-reasoning"
family         = "deepseek"
thinking       = "enabled"
efforts        = ["high", "max"]
default_effort = "high"
"#;

fn chat_route() -> p1_provider_openai_chat::ChatRoute {
    p1_provider_openai_chat::ChatRoute {
        origin_route: CHAT_ROUTE.to_string(),
        endpoint: "https://opencode.ai/zen/go/v1/chat/completions".to_string(),
        headers: vec![("user-agent".to_string(), "p1/test".to_string())],
        session_header: Some("x-opencode-session".to_string()),
        client_identity: None,
        dialect: p1_provider_openai_chat::ChatDialect::ThinkingWithReasoningAlias,
        limits: p1_provider_openai_chat::ChatLimits::default(),
    }
}

fn chat_profile() -> ModelProfile {
    ModelProfile::from_toml("chat-reasoning", CHAT_PROFILE).expect("the fixture profile parses")
}

fn chat_parser() -> p1_provider_openai_chat::ChatParser {
    p1_provider_openai_chat::ChatParser::new(
        chat_route().origin(CHAT_MODEL),
        p1_provider_openai_chat::ChatDialect::ThinkingWithReasoningAlias,
    )
}

/// The reasoning delta, with `REASONING` standing in for the value.
const CHAT_REASONING_DELTA: &str = r#"data: {"id":"chatcmpl-replay","model":"ECHO","choices":[{"index":0,"delta":{"reasoning_content":"REASONING"},"finish_reason":null}]}"#;

fn chat_events(reasoning: &str) -> Vec<String> {
    vec![
        format!(
            "{}\n\n",
            CHAT_REASONING_DELTA
                .replace("ECHO", CHAT_ECHO)
                .replace("REASONING", reasoning)
        ),
        sse(json!({
            "id": "chatcmpl-replay",
            "model": CHAT_ECHO,
            "choices": [{ "index": 0, "delta": { "content": "answer" }, "finish_reason": "stop" }],
        })),
        // The chat wire's terminal sentinel.
        "data: [DONE]\n\n".to_string(),
    ]
}

fn chat_turn(reasoning: &str) -> String {
    chat_events(reasoning).concat()
}

/// The reasoning delta only: the stream ends there, before the sentinel.
fn chat_cut_turn(reasoning: &str) -> String {
    chat_events(reasoning)[..1].concat()
}

fn chat_round_trip(reasoning: &str) -> (String, Item, Value) {
    let body = chat_turn(reasoning);
    let events = parse(&mut chat_parser(), &body, None);
    let item = history_item(&Item::Assistant(completed(finished(&events)).clone()));
    let lowered = p1_provider_openai_chat::lower_request(
        &chat_route(),
        CHAT_MODEL,
        &chat_profile(),
        &request(vec![user("go"), item.clone()]),
    )
    .expect("the follow-up request lowers");
    let lowered = serde_json::from_slice::<Value>(&lowered.body).expect("the body is JSON");
    (body, item, lowered)
}

/// The replayed `reasoning_content`: what went back to the provider.
fn chat_reasoning_content(lowered: &Value) -> &str {
    lowered["messages"]
        .as_array()
        .expect("a message array")
        .iter()
        .find_map(|message| message["reasoning_content"].as_str())
        .expect("a replayed reasoning_content")
}

fn chat_validate(request: &ProviderRequest) -> Result<(), ProviderError> {
    p1_provider_openai_chat::validate_request(&chat_route(), CHAT_MODEL, &chat_profile(), request)
}

fn chat_lower(request: &ProviderRequest) -> Result<Value, ProviderError> {
    p1_provider_openai_chat::lower_request(&chat_route(), CHAT_MODEL, &chat_profile(), request)
        .map(|lowered| serde_json::from_slice(&lowered.body).expect("the body is JSON"))
}

#[test]
fn chat_replay_bytes_travel_unchanged() {
    // The fixture spells escapes and ends with a space; these are the bytes they mean.
    assert_eq!(
        body_value(&chat_turn(ESCAPED), "reasoning_content"),
        DECODED
    );

    for reasoning in [ESCAPED.to_string(), big_value()] {
        let (body, item, lowered) = chat_round_trip(&reasoning);
        let expected = body_value(&body, "reasoning_content");
        if reasoning == ESCAPED {
            assert_eq!(expected, DECODED);
        } else {
            assert_eq!(expected.len(), 64 * 1024, "the fixture is 64 KiB");
        }

        let replay = replay_of(assistant(&item));
        assert_eq!(replay.version, p1_provider_openai_chat::REPLAY_VERSION);
        assert_eq!(
            replay
                .payload
                .as_str()
                .expect("a reasoning payload")
                .as_bytes(),
            expected.as_bytes(),
            "the payload holds the wire value byte for byte"
        );
        assert_eq!(replay.origin.route, CHAT_ROUTE);
        assert_eq!(replay.origin.model, CHAT_MODEL);
        assert_ne!(replay.origin.model, CHAT_ECHO);
        assert_eq!(assistant(&item).origin, replay.origin);

        assert_eq!(
            chat_reasoning_content(&lowered).as_bytes(),
            expected.as_bytes(),
            "the replayed value is the payload's value"
        );
        let text = serde_json::to_string(&lowered).expect("the body encodes");
        assert!(
            !text.contains(CHAT_ECHO),
            "the echoed alias must not reach a request"
        );
    }
}

#[test]
fn chat_refuses_an_own_replay_from_another_version_and_drops_a_foreign_one() {
    let (_, recorded, _) = chat_round_trip(ESCAPED);
    chat_validate(&request(vec![user("go"), recorded.clone()]))
        .expect("the recorded replay is this build's version");

    let stale = at_version(
        recorded.clone(),
        p1_provider_openai_chat::REPLAY_VERSION + 1,
    );
    let stale_request = request(vec![user("go"), stale]);
    let error = chat_validate(&stale_request).expect_err("another version is refused");
    assert_eq!(error.kind, ProviderErrorKind::InvalidRequest);
    for part in [CHAT_ROUTE, CHAT_MODEL, "version 2", "version 1"] {
        assert!(error.message.contains(part), "{part}: {}", error.message);
    }
    assert!(chat_lower(&stale_request).is_err());

    for origin in [
        Origin {
            route: "elsewhere/foreign-route".to_string(),
            model: CHAT_MODEL.to_string(),
        },
        Origin {
            route: CHAT_ROUTE.to_string(),
            model: "another-model".to_string(),
        },
    ] {
        let shown = foreign(recorded.clone(), origin, FOREIGN_SENTINEL, FOREIGN_TEXT);
        let carried = request(vec![user("go"), at_version(shown, 2)]);
        chat_validate(&carried).expect("a foreign history is carried, never refused");
        let lowered = chat_lower(&carried).expect("the foreign turn still lowers");
        let text = serde_json::to_string(&lowered).expect("the body encodes");
        for part in [FOREIGN_SENTINEL, FOREIGN_TEXT] {
            assert!(!text.contains(part), "{part} reached {text}");
        }
    }
}

#[test]
fn chat_partial_input_is_equivalent_and_a_cut_reasoning_turn_fails() {
    let body = chat_turn(ESCAPED);
    let whole = parse(&mut chat_parser(), &body, None);
    for split in 0..=body.len() {
        assert_eq!(
            parse(&mut chat_parser(), &body, Some(split)),
            whole,
            "split at byte offset {split}"
        );
    }

    let events = parse(&mut chat_parser(), &chat_cut_turn(ESCAPED), None);
    assert!(matches!(finished(&events), Outcome::Failed(_)));
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, StreamEvent::Finished(Outcome::Completed(_)))),
        "{events:?}"
    );
}

#[test]
fn chat_http_errors_cross_unchanged() {
    let parser = chat_parser();
    let usage_limit =
        br#"{"error":{"type":"GoUsageLimitError","message":"quota for this plan is exhausted"}}"#;
    let no_balance = br#"{"error":{"type":"insufficient_balance","message":"please top up"}}"#;
    let plan_refusal = br#"{"error":{"type":"FreeTierError","message":"this model is only available in the OpenCode client"}}"#;
    let context_window = br#"{"error":{"code":"context_length_exceeded"}}"#;
    for (status, body, expected) in [
        (
            429u16,
            usage_limit.as_slice(),
            ProviderErrorKind::UsageLimitExhausted,
        ),
        (
            402,
            usage_limit.as_slice(),
            ProviderErrorKind::UsageLimitExhausted,
        ),
        (
            401,
            no_balance.as_slice(),
            ProviderErrorKind::InsufficientBalance,
        ),
        (403, plan_refusal.as_slice(), ProviderErrorKind::NotEntitled),
        (
            400,
            context_window.as_slice(),
            ProviderErrorKind::ContextWindowExceeded,
        ),
        (
            400,
            b"bad request".as_slice(),
            ProviderErrorKind::InvalidRequest,
        ),
        (
            500,
            b"server error".as_slice(),
            ProviderErrorKind::Transport,
        ),
    ] {
        http_error_survives(&parser, status, body, expected);
    }
}
