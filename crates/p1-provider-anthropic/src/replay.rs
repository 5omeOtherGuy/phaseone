//! The replay codec: the only place this adapter writes or reads a
//! [`ReplayData::payload`].
//!
//! Replay data is opaque native continuation data (`routes.md` §A "Replay"): the
//! provider matches the signature and the redacted data byte for byte, so the
//! payload holds the wire values exactly as the SSE body carried them — never
//! trimmed, normalized or redacted. It is meaningful only to the adapter that wrote
//! it, only for the origin that produced it (ADR-0018), and only at the layout
//! version that wrote it (ADR-0049).
//!
//! Keeping both halves in one module is what makes the compatibility policy one
//! rule: the parser cannot write a payload the request builder would refuse, and the
//! request builder cannot read a field the parser never wrote.

use p1_contracts::history::{Origin, ReplayData};
use serde_json::{Value, json};

/// The payload layout this build writes and reads. It is the whole compatibility
/// policy for stored replay data (ADR-0049): another version of our own origin is
/// unreadable, and unreadable data is refused, never dropped.
pub const REPLAY_VERSION: u32 = 1;

/// The wire values one replayable Messages block carries. The visible thinking text
/// is NOT here: it travels in the item's `text`, where the model sees it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WireBlock<'a> {
    /// A `thinking` block's signature.
    Thinking { signature: &'a str },
    /// A `redacted_thinking` block's opaque data.
    Redacted { data: &'a str },
}

/// What the CONFIGURED origin makes of one stored payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Replay<'a> {
    /// Another origin wrote it: dropped entirely, never rendered as assistant text
    /// (ADR-0018).
    Foreign,
    /// Our origin, at a version this build does not read: refused, because a session
    /// must not silently lose the thinking it is continuing from (ADR-0049).
    UnsupportedVersion { version: u32 },
    /// Our origin, at [`REPLAY_VERSION`], in a shape this build does not read:
    /// refused for the same reason.
    UnsupportedPayload,
    /// Ours and readable.
    Carried(WireBlock<'a>),
}

/// The replay data for one block this parser saw, tagged with the CONFIGURED origin —
/// never the model name the response echoes (ADR-0018).
pub fn encode(origin: &Origin, block: WireBlock<'_>) -> ReplayData {
    let payload = match block {
        WireBlock::Thinking { signature } => json!({ "type": "thinking", "signature": signature }),
        WireBlock::Redacted { data } => json!({ "type": "redacted_thinking", "data": data }),
    };
    ReplayData {
        origin: origin.clone(),
        version: REPLAY_VERSION,
        payload,
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
    let field = |name: &str| data.payload.get(name).and_then(Value::as_str);
    match data.payload.get("type").and_then(Value::as_str) {
        Some("thinking") => match field("signature") {
            Some(signature) => Replay::Carried(WireBlock::Thinking { signature }),
            None => Replay::UnsupportedPayload,
        },
        Some("redacted_thinking") => match field("data") {
            Some(data) => Replay::Carried(WireBlock::Redacted { data }),
            None => Replay::UnsupportedPayload,
        },
        _ => Replay::UnsupportedPayload,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn origin() -> Origin {
        Origin {
            route: "anthropic-messages/claude-subscription".to_string(),
            model: "claude-sonnet-5".to_string(),
        }
    }

    /// The signature is carried as decoded from the SSE JSON: non-ASCII text, a NUL
    /// and trailing spaces all survive, because the provider compares bytes.
    #[test]
    fn a_signed_thinking_block_round_trips_byte_exact() {
        let signature = "sig \u{0}\u{e9}\u{2603}  ";
        let replay = encode(&origin(), WireBlock::Thinking { signature });
        assert_eq!(replay.origin, origin());
        assert_eq!(replay.version, REPLAY_VERSION);
        assert_eq!(
            replay.payload,
            json!({ "type": "thinking", "signature": signature })
        );
        assert_eq!(
            decode(&replay, &origin()),
            Replay::Carried(WireBlock::Thinking { signature })
        );
    }

    #[test]
    fn a_redacted_block_round_trips() {
        let replay = encode(&origin(), WireBlock::Redacted { data: "opaque" });
        assert_eq!(
            decode(&replay, &origin()),
            Replay::Carried(WireBlock::Redacted { data: "opaque" })
        );
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
            for payload in [
                json!({ "type": "thinking", "signature": "sig" }),
                json!({ "type": "thinking" }),
                json!("not a thinking block"),
            ] {
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
    fn our_origin_at_another_version_is_unsupported() {
        let data = ReplayData {
            origin: origin(),
            version: REPLAY_VERSION + 1,
            payload: json!({ "type": "thinking", "signature": "sig" }),
        };
        assert_eq!(
            decode(&data, &origin()),
            Replay::UnsupportedVersion {
                version: REPLAY_VERSION + 1
            }
        );
    }

    #[test]
    fn our_origin_in_an_unreadable_shape_is_unsupported() {
        for payload in [
            json!("text"),
            json!({ "type": "thinking" }),
            json!({ "type": "redacted_thinking", "data": 7 }),
            json!({ "type": "summary" }),
        ] {
            let data = ReplayData {
                origin: origin(),
                version: REPLAY_VERSION,
                payload,
            };
            assert_eq!(decode(&data, &origin()), Replay::UnsupportedPayload);
        }
    }
}
