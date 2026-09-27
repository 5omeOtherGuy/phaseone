//! The replay codec: the only place this adapter writes or reads a
//! [`ReplayData::payload`].
//!
//! Replay data is opaque native continuation data: the chat wire takes the reasoning
//! text back as `reasoning_content`, so the payload holds the text exactly as decoded
//! from the SSE body — never trimmed, normalized or redacted. It is meaningful only to
//! the adapter that wrote it, only for the origin that produced it (ADR-0018), and
//! only at the layout version that wrote it (ADR-0049).
//!
//! Keeping both halves in one module is what makes the compatibility policy one
//! rule: the parser cannot write a payload the request builder would refuse, and the
//! request builder cannot read a shape the parser never wrote.

use p1_contracts::history::{Origin, ReplayData};
use serde_json::Value;

/// The payload layout this build writes and reads. It is the whole compatibility
/// policy for stored replay data (ADR-0049): another version of our own origin is
/// unreadable, and unreadable data is refused, never dropped.
pub const REPLAY_VERSION: u32 = 1;

/// What the CONFIGURED origin makes of one stored payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Replay<'a> {
    /// Another origin wrote it: dropped entirely, never rendered as assistant text
    /// (ADR-0018).
    Foreign,
    /// Our origin, at a version this build does not read: refused, because a session
    /// must not silently lose the reasoning it is continuing from (ADR-0049).
    UnsupportedVersion { version: u32 },
    /// Our origin, at [`REPLAY_VERSION`], in a shape this build does not read:
    /// refused for the same reason.
    UnsupportedPayload,
    /// Ours and readable: the reasoning text that goes back byte-exact.
    Carried(&'a str),
}

/// The replay data for one reasoning block this parser completed, tagged with the
/// CONFIGURED origin — never the model name the response echoes (ADR-0018).
pub fn encode(origin: &Origin, reasoning: &str) -> ReplayData {
    ReplayData {
        origin: origin.clone(),
        version: REPLAY_VERSION,
        payload: Value::String(reasoning.to_string()),
    }
}

/// Classify one stored payload against `origin`, the configured origin of the request
/// being built.
pub fn decode<'a>(data: &'a ReplayData, origin: &Origin) -> Replay<'a> {
    if data.origin != *origin {
        return Replay::Foreign;
    }
    if data.version != REPLAY_VERSION {
        return Replay::UnsupportedVersion {
            version: data.version,
        };
    }
    match data.payload.as_str() {
        Some(reasoning) => Replay::Carried(reasoning),
        None => Replay::UnsupportedPayload,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn origin() -> Origin {
        Origin {
            route: "openai-chat/opencode-go-subscription".to_string(),
            model: "configured-model".to_string(),
        }
    }

    #[test]
    fn the_reasoning_text_round_trips_byte_exact() {
        let reasoning = "reasoning \u{0}\u{e9}\u{2603}  ";
        let replay = encode(&origin(), reasoning);
        assert_eq!(replay.origin, origin());
        assert_eq!(replay.version, REPLAY_VERSION);
        assert_eq!(replay.payload, json!(reasoning));
        assert_eq!(decode(&replay, &origin()), Replay::Carried(reasoning));
    }

    /// A foreign route OR a foreign model is another origin: dropped, whatever version
    /// and whatever shape the payload has (ADR-0018).
    #[test]
    fn another_origin_is_foreign_at_any_version_or_shape() {
        for other in [
            Origin {
                route: "other".to_string(),
                ..origin()
            },
            Origin {
                model: "other".to_string(),
                ..origin()
            },
        ] {
            for payload in [json!("reasoning"), json!({ "parts": ["x"] })] {
                let data = ReplayData {
                    origin: other.clone(),
                    version: REPLAY_VERSION + 1,
                    payload,
                };
                assert_eq!(decode(&data, &origin()), Replay::Foreign);
            }
        }
    }

    #[test]
    fn our_origin_at_another_version_or_in_an_unreadable_shape_is_unsupported() {
        let data = ReplayData {
            origin: origin(),
            version: REPLAY_VERSION + 1,
            payload: json!("reasoning"),
        };
        assert_eq!(
            decode(&data, &origin()),
            Replay::UnsupportedVersion {
                version: REPLAY_VERSION + 1
            }
        );
        for payload in [json!({ "parts": ["x"] }), json!([1, 2]), json!(7)] {
            let data = ReplayData {
                origin: origin(),
                version: REPLAY_VERSION,
                payload,
            };
            assert_eq!(decode(&data, &origin()), Replay::UnsupportedPayload);
        }
    }
}
