//! Usage mapping of the Messages wire (#80): a field the wire leaves out is `None`
//! ("not reported"), a present field maps as sent, and nothing is estimated (ADR-0019).

use p1_contracts::{Outcome, Usage};
use p1_provider_anthropic::{AnthropicParser, ROUTE};
use p1_provider_http::{ResponseParser, SseEvent};
use serde_json::{Value, json};

/// The usage of one text turn whose `message_start` carries `start` and whose
/// `message_delta` carries `delta`.
fn usage_of(start: Value, delta: Value) -> Option<Usage> {
    let mut parser = AnthropicParser::new(ROUTE, "claude-sonnet-4-6");
    let events = [
        json!({"type": "message_start", "message": {"id": "msg_usage", "usage": start}}),
        json!({"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": ""}}),
        json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": "hi"}}),
        json!({"type": "content_block_stop", "index": 0}),
        json!({"type": "message_delta", "delta": {"stop_reason": "end_turn"}, "usage": delta}),
        json!({"type": "message_stop"}),
    ];
    for value in events {
        parser.on_event(SseEvent {
            event: value["type"].as_str().map(str::to_string),
            data: value.to_string(),
        });
    }
    match parser.on_end() {
        Outcome::Completed(completed) => completed.usage,
        other => panic!("expected a completed response: {other:?}"),
    }
}

#[test]
fn absent_cache_write_is_not_reported_rather_than_zero() {
    let usage = usage_of(
        json!({"input_tokens": 12, "cache_read_input_tokens": 5, "output_tokens": 1}),
        json!({"output_tokens": 7}),
    )
    .expect("usage");
    assert_eq!(usage.cache_write, None);
    assert_eq!(usage.cache_read, Some(5));
    assert_eq!(usage.input_uncached, Some(12));
    assert_eq!(usage.output, Some(7));
}

#[test]
fn null_cache_write_is_not_reported_rather_than_zero() {
    let usage = usage_of(
        json!({"input_tokens": 12, "cache_creation_input_tokens": null, "output_tokens": 1}),
        json!({"output_tokens": 7, "cache_creation_input_tokens": null}),
    )
    .expect("usage");
    assert_eq!(usage.cache_write, None);
}

#[test]
fn present_cache_write_maps_as_sent_and_the_delta_overrides_the_start() {
    let usage = usage_of(
        json!({"input_tokens": 12, "cache_creation_input_tokens": 0, "output_tokens": 1}),
        json!({"output_tokens": 7}),
    )
    .expect("usage");
    assert_eq!(usage.cache_write, Some(0));
    let usage = usage_of(
        json!({"input_tokens": 12, "cache_creation_input_tokens": 40, "output_tokens": 1}),
        json!({"output_tokens": 7, "cache_creation_input_tokens": 44}),
    )
    .expect("usage");
    assert_eq!(usage.cache_write, Some(44));
}

#[test]
fn reported_thinking_tokens_fill_reasoning_output() {
    let usage = usage_of(
        json!({"input_tokens": 12, "output_tokens": 1}),
        json!({"output_tokens": 90, "output_tokens_details": {"thinking_tokens": 60}}),
    )
    .expect("usage");
    assert_eq!(usage.output, Some(90));
    assert_eq!(usage.reasoning_output, Some(60));
    // A reported zero is a measurement, not a missing value.
    let usage = usage_of(
        json!({"input_tokens": 12, "output_tokens": 1}),
        json!({"output_tokens": 9, "output_tokens_details": {"thinking_tokens": 0}}),
    )
    .expect("usage");
    assert_eq!(usage.reasoning_output, Some(0));
}

#[test]
fn thinking_tokens_reported_only_at_start_survive_a_delta_without_them() {
    let usage = usage_of(
        json!({"input_tokens": 12, "output_tokens": 1, "output_tokens_details": {"thinking_tokens": 3}}),
        json!({"output_tokens": 90}),
    )
    .expect("usage");
    assert_eq!(usage.reasoning_output, Some(3));
}

#[test]
fn unitemized_thinking_is_not_reported_and_never_estimated() {
    // `output_tokens` may include thinking; without the itemized count the share is unknown.
    for delta in [
        json!({"output_tokens": 90}),
        json!({"output_tokens": 90, "output_tokens_details": {}}),
        json!({"output_tokens": 90, "output_tokens_details": {"thinking_tokens": null}}),
    ] {
        let usage =
            usage_of(json!({"input_tokens": 12, "output_tokens": 1}), delta).expect("usage");
        assert_eq!(usage.output, Some(90));
        assert_eq!(usage.reasoning_output, None);
    }
}

#[test]
fn cost_is_never_invented_on_this_route() {
    let usage = usage_of(
        json!({"input_tokens": 12, "cache_read_input_tokens": 5, "cache_creation_input_tokens": 4, "output_tokens": 1}),
        json!({"output_tokens": 90, "output_tokens_details": {"thinking_tokens": 60}}),
    )
    .expect("usage");
    assert_eq!(usage.cost_micro_usd, None);
}
