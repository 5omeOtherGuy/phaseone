//! Golden payloads adapted from ACP v1 docs (Apache-2.0):
//! https://agentclientprotocol.com/protocol/v1/prompt-turn
//! https://agentclientprotocol.com/protocol/v1/tool-calls
//! https://agentclientprotocol.com/protocol/v1/initialization
//! Envelopes belong to the driver. Optional fields are limited to this slice;
//! execution starts in_progress and finishes completed rather than pending.

use p1_acp::{
    capabilities::initialize,
    codec::Codec,
    policy::{PermissionPrompt, PermissionReply},
    sink::{ToolCategory, ToolDisplay, Update},
    turn::{TurnError, TurnStop},
};
use serde_json::json;

fn tool() -> ToolDisplay {
    ToolDisplay {
        id: "call_001".into(),
        name: "read_file".into(),
        title: "Reading configuration file".into(),
        category: ToolCategory::Read,
        input: json!({"path":"config.json"}),
    }
}

#[test]
fn v1_outbound_payloads() {
    let codec = Codec::negotiate(1);
    let text = "I'll analyze your code for potential issues. Let me examine it...";
    assert_eq!(
        codec.encode_update(&Update::Message(text.into())),
        json!({
            "sessionUpdate":"agent_message_chunk", "content":{"type":"text","text":text}
        })
    );
    assert_eq!(
        codec.encode_update(&Update::Thought(text.into())),
        json!({
            "sessionUpdate":"agent_thought_chunk", "content":{"type":"text","text":text}
        })
    );
    assert_eq!(
        codec.encode_update(&Update::ToolStarted(tool())),
        json!({
            "sessionUpdate":"tool_call", "toolCallId":"call_001", "name":"read_file",
            "title":"Reading configuration file", "kind":"read", "status":"in_progress",
            "rawInput":{"path":"config.json"}
        })
    );
    assert_eq!(
        codec.encode_update(&Update::ToolFinished {
            id: "call_001".into(),
            succeeded: true,
            text: "Analysis complete. Found 3 issues.".into()
        }),
        json!({
            "sessionUpdate":"tool_call_update", "toolCallId":"call_001", "status":"completed",
            "content":[{"type":"content","content":{"type":"text","text":"Analysis complete. Found 3 issues."}}]
        })
    );
    assert_eq!(
        codec.encode_stop(TurnStop::Finished),
        json!({"stopReason":"end_turn"})
    );
    assert_eq!(
        codec.encode_error(&TurnError {
            message: "Internal error".into()
        }),
        json!({"code":-32603,"message":"Internal error"})
    );
    assert_eq!(
        codec.encode_permission("sess_abc123def456", &PermissionPrompt { tool: tool() }),
        json!({
            "sessionId":"sess_abc123def456",
            "toolCall":{"toolCallId":"call_001","name":"read_file","title":"Reading configuration file","kind":"read","status":"pending","rawInput":{"path":"config.json"}},
            "options":[
                {"optionId":"allow_once","name":"Allow once","kind":"allow_once"},
                {"optionId":"allow_always","name":"Always allow","kind":"allow_always"},
                {"optionId":"reject_once","name":"Reject","kind":"reject_once"}
            ]
        })
    );
}

#[test]
fn initialization_selects_supported_codec_without_enabling_extensions() {
    for requested in [0, 1, 2, u16::MAX] {
        let (codec, capabilities) = initialize(requested, None);
        assert_eq!(codec.version(), 1);
        assert_eq!(
            codec.encode_capabilities(&capabilities),
            json!({
                "protocolVersion":1,"authMethods":[],"agentCapabilities":{
                    "loadSession":false,"promptCapabilities":{"image":false,"audio":false,"embeddedContext":false}
                }
            })
        );
        assert_eq!(
            codec.encode_stop(TurnStop::Finished),
            json!({"stopReason":"end_turn"})
        );
    }
}

#[test]
fn permission_replies_validate_the_discriminator_and_option_id() {
    let codec = Codec::negotiate(1);
    for (id, expected) in [
        ("allow_once", PermissionReply::AllowOnce),
        ("allow_always", PermissionReply::AllowAlways),
        ("reject_once", PermissionReply::Reject),
        ("allow-once", PermissionReply::Reject),
    ] {
        assert_eq!(
            codec
                .decode_permission(json!({"outcome":{"outcome":"selected","optionId":id}}))
                .unwrap(),
            expected
        );
    }
    assert_eq!(
        codec
            .decode_permission(json!({"outcome":{"outcome":"cancelled"}}))
            .unwrap(),
        PermissionReply::Cancelled
    );
    for malformed in [
        json!({}),
        json!({"outcome":"cancelled"}),
        json!({"outcome":{"outcome":"selected"}}),
        json!({"outcome":{"outcome":"selected","optionId":7}}),
        json!({"outcome":{"outcome":"allow_always"}}),
    ] {
        assert!(codec.decode_permission(malformed).is_err());
    }
}
