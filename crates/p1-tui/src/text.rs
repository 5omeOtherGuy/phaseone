//! Single-stream terminal-control sanitizer for band text.
//!
//! Source: iris-agent `src/ui/textengine.rs`, commit
//! `5b04a1ad3412ad0bb663b6355f77a024aec0ddfa`, MIT License.
//! Copyright (c) 2026 5omeOtherGuy. SPDX-License-Identifier: MIT
//!
//! Adapted from `transform`, `consume_csi`, and `consume_string_control`.

use crate::band::Seg;

#[derive(Clone, Copy)]
enum State {
    Ground,
    Escape,
    Csi,
    StringControl,
    StringEscape,
}

/// Sanitize one side of a band as a single character stream.
///
/// Segment boundaries carry parser state but not output style: every retained
/// character remains in the segment which supplied it.
pub(crate) fn sanitize_segments(segments: &[Seg]) -> Vec<Seg> {
    // Preserve the existing clone-only allocation path for ordinary text.
    if !segments
        .iter()
        .any(|segment| segment.text.chars().any(char::is_control))
    {
        return segments.to_vec();
    }

    let mut clean = Vec::with_capacity(segments.len());
    let mut state = State::Ground;
    for segment in segments {
        let mut text = String::with_capacity(segment.text.len());
        transform(&segment.text, &mut text, &mut state);
        clean.push(Seg {
            fg: segment.fg,
            bg: segment.bg,
            text,
        });
    }
    clean
}

/// Sanitize one plain transcript string without changing its storage.
///
/// Adapted from `iris-donor/src/ui/tui/text.rs`'s `strip_ansi_for_text` use
/// before wrapping, commit `5b04a1ad3412ad0bb663b6355f77a024aec0ddfa`, MIT License.
pub(crate) fn sanitize_text(input: &str) -> String {
    sanitize_segments(&[Seg::new(crate::palette::INK, input)])
        .pop()
        .expect("one input segment produces one sanitized segment")
        .text
}

fn transform(input: &str, output: &mut String, state: &mut State) {
    for ch in input.chars() {
        match *state {
            State::Ground => match ch {
                '\x1b' => *state = State::Escape,
                '\u{009b}' => *state = State::Csi,
                '\u{0090}' | '\u{0098}' | '\u{009d}' | '\u{009e}' | '\u{009f}' => {
                    *state = State::StringControl
                }
                '\t' => output.push(' '),
                ch if ch.is_control() => {}
                ch => output.push(ch),
            },
            State::Escape => {
                *state = match ch {
                    '[' => State::Csi,
                    ']' | 'P' | 'X' | '^' | '_' => State::StringControl,
                    '\u{009b}' => State::Csi,
                    '\u{0090}' | '\u{0098}' | '\u{009d}' | '\u{009e}' | '\u{009f}' => {
                        State::StringControl
                    }
                    // As in the donor, ESC plus any other character drops both.
                    _ => State::Ground,
                };
            }
            State::Csi => match ch {
                // A VT parser aborts an unfinished CSI as soon as a new
                // introducer arrives.  Ignoring the introducer would let a
                // stray ']' pass as the final byte, so an OSC nested inside a
                // CSI would leak its payload as visible text.
                '\x1b' => *state = State::Escape,
                '\u{009b}' => *state = State::Csi,
                '\u{0090}' | '\u{0098}' | '\u{009d}' | '\u{009e}' | '\u{009f}' => {
                    *state = State::StringControl
                }
                // Parameters and intermediates are consumed inside the
                // sequence, so only a final byte ends it.
                '\u{40}'..='\u{7e}' => *state = State::Ground,
                _ => {}
            },
            State::StringControl => match ch {
                // A string-control terminator ends the sequence.  Keep the
                // state across segment boundaries when it does not arrive.
                '\u{7}' | '\u{009c}' => *state = State::Ground,
                '\x1b' => *state = State::StringEscape,
                _ => {}
            },
            State::StringEscape => match ch {
                // ST may be split at a segment boundary: ESC is pending
                // until the following character is inspected.
                '\\' => *state = State::Ground,
                '\u{7}' | '\u{009c}' => *state = State::Ground,
                '\x1b' => {}
                _ => *state = State::StringControl,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sanitized(raw: &str) -> String {
        let mut segments = sanitize_segments(&[Seg::new(crate::palette::INK, raw)]);
        assert_eq!(segments.len(), 1);
        segments.pop().unwrap().text
    }

    #[test]
    fn strip_ansi_keeps_controls_clean_text_drops_them() {
        // Adapted donor policy: each tab is one space, not a tab stop.
        assert_eq!(sanitized("\x1b[31ma\tb\x1b[0m"), "a b");
        assert_eq!(sanitized("a\0b\nc"), "abc");
        // 8-bit C1 CSI and bracketed-paste markers are removed.
        assert_eq!(sanitized("\u{009b}31mx"), "x");
        assert_eq!(sanitized("\x1b[200~hi\x1b[201~"), "hi");
    }

    #[test]
    fn strip_ansi_handles_osc_with_st_and_bel() {
        assert_eq!(sanitized("\x1b]8;;https://a\x07txt\x1b]8;;\x07"), "txt");
        assert_eq!(sanitized("\x1b]0;title\x1b\\body"), "body");
        // 8-bit C1 ST (U+009C) terminates rather than swallowing visible text.
        assert_eq!(sanitized("\x1b]0;title\u{009c}body"), "body");
    }

    fn sanitized_segments(first: &str, second: &str) -> String {
        let segments = sanitize_segments(&[
            Seg::new(crate::palette::INK, first),
            Seg::new(crate::palette::REF, second),
        ]);
        segments.into_iter().map(|segment| segment.text).collect()
    }

    #[test]
    fn csi_final_byte_is_inspected_exactly_once() {
        assert_eq!(sanitized("\x1b[mempty-sgr"), "empty-sgr");
        assert_eq!(sanitized_segments("x\x1b[", "mHELLO"), "xHELLO");
    }

    #[test]
    fn split_csi_and_osc_sequences_keep_their_payloads_consumed() {
        assert_eq!(sanitized_segments("\x1b[31", "mrest"), "rest");
        assert_eq!(sanitized_segments("\x1b]8;;http", "://x\u{7}link"), "link");
        assert_eq!(sanitized_segments("\x1b]title\x1b", "\\body"), "body");
        assert_eq!(sanitized_segments("\x1b[", "mHELLO"), "HELLO");
    }

    #[test]
    fn split_c1_sequences_and_st_keep_state() {
        assert_eq!(sanitized_segments("\u{009b}31", "mrest"), "rest");
        assert_eq!(
            sanitized_segments("\u{009d}0;title", "body\u{7}extra"),
            "extra"
        );
        assert_eq!(sanitized_segments("\x1b]", "\u{7}vis\u{7}"), "vis");
    }

    #[test]
    fn a_new_introducer_aborts_an_unfinished_csi() {
        // An OSC introducer inside a CSI must not be read as its final byte.
        assert_eq!(sanitized("\x1b[31\x1b]0;title\x07ok"), "ok");
        assert_eq!(sanitized("\x1b[31\u{009d}0;title\x07ok"), "ok");
        // A nested CSI likewise aborts and restarts the sequence.
        assert_eq!(sanitized("a\x1b[31\x1b[32mb"), "ab");
        // ESC may be the last character of a segment before the introducer.
        assert_eq!(sanitized_segments("\x1b[31\x1b", "]0;t\x07ok"), "ok");
    }
}
