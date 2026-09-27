//! The replay codec: the only place this adapter writes or reads a
//! [`ReplayData::payload`].
//!
//! Replay data is opaque native continuation data (`routes.md` §B "Replay"): the
//! provider takes the encrypted reasoning item back verbatim, so the payload holds
//! the `encrypted_content` exactly as the SSE body carried it — never trimmed,
//! normalized or redacted. It is meaningful only to the adapter that wrote it, only
//! for the origin that produced it (ADR-0018), and only at the layout version that
//! wrote it (ADR-0049).
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
    /// Ours and readable: the encrypted reasoning item that goes back byte-exact.
    Carried(&'a str),
}

/// The replay data for one reasoning output item this parser saw, tagged with the
/// CONFIGURED origin — never the model name the response echoes (ADR-0018).
pub fn encode(origin: &Origin, encrypted_content: &str) -> ReplayData {
    ReplayData {
        origin: origin.clone(),
        version: REPLAY_VERSION,
        payload: json!({ "type": "reasoning", "encrypted_content": encrypted_content }),
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
    match data
        .payload
        .get("encrypted_content")
        .and_then(Value::as_str)
    {
        Some(encrypted_content) => Replay::Carried(encrypted_content),
        None => Replay::UnsupportedPayload,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn origin() -> Origin {
        Origin {
            route: "openai-responses/codex-subscription".to_string(),
            model: "gpt-6-sol".to_string(),
        }
    }

    #[test]
    fn the_encrypted_content_round_trips_byte_exact() {
        let encrypted_content = "enc \u{0}\u{e9}\u{2603}  ";
        let replay = encode(&origin(), encrypted_content);
        assert_eq!(replay.origin, origin());
        assert_eq!(replay.version, REPLAY_VERSION);
        assert_eq!(
            replay.payload,
            json!({ "type": "reasoning", "encrypted_content": encrypted_content })
        );
        assert_eq!(
            decode(&replay, &origin()),
            Replay::Carried(encrypted_content)
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
                json!({ "type": "reasoning", "encrypted_content": "enc" }),
                json!({ "type": "reasoning" }),
                json!("not a reasoning payload"),
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
    fn our_origin_at_another_version_or_in_an_unreadable_shape_is_unsupported() {
        let data = ReplayData {
            origin: origin(),
            version: REPLAY_VERSION + 1,
            payload: json!({ "type": "reasoning", "encrypted_content": "enc" }),
        };
        assert_eq!(
            decode(&data, &origin()),
            Replay::UnsupportedVersion {
                version: REPLAY_VERSION + 1
            }
        );
        for payload in [
            json!({ "type": "reasoning" }),
            json!({ "encrypted_content": 7 }),
            json!(["enc"]),
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
