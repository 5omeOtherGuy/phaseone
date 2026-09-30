//! The masking of a stream before it is stored (ADR-0109 item 2): chunks come in as a process
//! prints them, masked text comes out, and a credential split across two chunks is masked
//! whole.
//!
//! `p1_redact` masks a text; a stream has no text, only chunks whose boundaries fall anywhere,
//! including inside a character, inside an `sk-` key, or between `Bearer` and its token on the
//! next line. So the redactor holds back what a later chunk could still complete, and masks
//! only what no later chunk can change:
//! - the bytes of a character the chunk cut, until the rest arrives;
//! - the last, unfinished line: every credential shape ends at a line end, save the two that
//!   may carry ONE line break between their context and the value (`Bearer`, `Authorization:`);
//! - so also the last finished line whenever its end is such a context: whether it is, is
//!   asked of `p1_redact` itself (a token-shaped probe after the line is masked or not), so the
//!   rule follows the matcher instead of copying its shapes;
//! - and a suffix that begins a registered value ([`SecretSet::shown_len`], as the model-output
//!   mask in `p1-host/src/secret_mask.rs` does).
//!
//! What is held is bounded: a line longer than [`MAX_HELD`] is cut at its last blank that no
//! context spans, or, when none is left, at [`MAX_HELD`] itself, so memory stays at most
//! [`MAX_HELD`] plus one chunk however long a line is. A credential inside one line longer
//! than [`MAX_HELD`] with no blank in reach is the one case the bound can split.

use p1_redact::{SecretSet, redact_with};

/// The most text held back before a cut is forced (see the module documentation).
pub(crate) const MAX_HELD: usize = 64 * 1024;

/// How many blanks a forced cut tries before it cuts at [`MAX_HELD`].
const BLANKS_TRIED: usize = 16;

/// Token-shaped text that the matcher masks after a context and leaves alone anywhere else;
/// the quoted form covers a context whose value is a quoted string.
const PROBES: [&str; 2] = [
    "p1probe0value0that0any0token0rule0takes",
    "\"p1probe0value0that0any0token0rule0takes\"",
];

/// Masks a byte stream chunk by chunk (see the module documentation).
pub(crate) struct StreamRedactor {
    secrets: SecretSet,
    /// The leading bytes of a character the last chunk cut; at most three.
    undecoded: Vec<u8>,
    /// Decoded text not masked yet.
    pending: String,
}

impl StreamRedactor {
    pub(crate) fn new(secrets: SecretSet) -> Self {
        Self {
            secrets,
            undecoded: Vec::new(),
            pending: String::new(),
        }
    }

    /// Take `chunk` in and return the masked text that no later chunk can change. Bytes that
    /// are not UTF-8 become U+FFFD, as the tool result shows them.
    pub(crate) fn push(&mut self, chunk: &[u8]) -> String {
        self.decode(chunk);
        let cut = self.cut();
        if cut == 0 {
            return String::new();
        }
        let shown = redact_with(&self.pending[..cut], &self.secrets).text;
        self.pending.drain(..cut);
        shown
    }

    /// The stream ended: mask and return everything still held.
    pub(crate) fn finish(&mut self) -> String {
        if !self.undecoded.is_empty() {
            self.undecoded.clear();
            self.pending.push(char::REPLACEMENT_CHARACTER);
        }
        let rest = std::mem::take(&mut self.pending);
        if rest.is_empty() {
            return rest;
        }
        redact_with(&rest, &self.secrets).text
    }

    /// Bytes held back right now: the memory the redactor adds to a run.
    #[cfg(test)]
    pub(crate) fn held(&self) -> usize {
        self.pending.len() + self.undecoded.len()
    }

    fn decode(&mut self, chunk: &[u8]) {
        let joined;
        let mut bytes = if self.undecoded.is_empty() {
            chunk
        } else {
            let mut start = std::mem::take(&mut self.undecoded);
            start.extend_from_slice(chunk);
            joined = start;
            &joined[..]
        };
        loop {
            match std::str::from_utf8(bytes) {
                Ok(text) => {
                    self.pending.push_str(text);
                    return;
                }
                Err(error) => {
                    let valid = error.valid_up_to();
                    // Checked by `from_utf8` up to `valid`.
                    self.pending
                        .push_str(std::str::from_utf8(&bytes[..valid]).unwrap_or_default());
                    match error.error_len() {
                        Some(invalid) => {
                            self.pending.push(char::REPLACEMENT_CHARACTER);
                            bytes = &bytes[valid + invalid..];
                        }
                        None => {
                            self.undecoded = bytes[valid..].to_vec();
                            return;
                        }
                    }
                }
            }
        }
    }

    /// How much of `pending` can be masked now (see the module documentation).
    fn cut(&self) -> usize {
        let text = self.pending.as_str();
        let mut cut = text.rfind('\n').map_or(0, |end| end + 1);
        // The last finished line may be a context whose value is on the next line.
        while cut > 0 && self.continues(&text[..cut]) {
            cut = text[..cut - 1].rfind('\n').map_or(0, |end| end + 1);
        }
        if cut == 0 && text.len() > MAX_HELD {
            cut = self.forced_cut(text);
        }
        // A suffix that begins a registered value waits for the rest of it.
        cut.min(self.secrets.shown_len(text))
    }

    /// Whether a credential context at the end of `shown` would take text that follows it.
    fn continues(&self, shown: &str) -> bool {
        let line_start = shown[..shown.len().saturating_sub(1)]
            .rfind('\n')
            .map_or(0, |end| end + 1);
        self.takes_what_follows(&shown[line_start..])
    }

    fn takes_what_follows(&self, text: &str) -> bool {
        PROBES.iter().any(|probe| {
            let probed = format!("{text}{probe}");
            !redact_with(&probed, &self.secrets).text.ends_with(probe)
        })
    }

    /// A cut in a line longer than [`MAX_HELD`]: after its last blank that no context spans,
    /// else at [`MAX_HELD`]. Only the last [`BLANKS_TRIED`] blanks are tried, so a long line
    /// of blanks inside one context costs a bounded number of probes.
    fn forced_cut(&self, text: &str) -> usize {
        let mut end = text.len();
        for _ in 0..BLANKS_TRIED {
            let Some(blank) = text[..end].rfind([' ', '\t', '\r']) else {
                break;
            };
            let cut = blank + 1;
            if !self.takes_what_follows(&text[..cut]) {
                return cut;
            }
            end = blank;
        }
        let mut cut = MAX_HELD;
        while !text.is_char_boundary(cut) {
            cut -= 1;
        }
        cut
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fake key of the `sk-` shape, built at run time: no credential-shaped literal.
    fn fake_key() -> String {
        format!("sk-{}", "a1B2c3D4e5".repeat(3))
    }

    fn run(secrets: &SecretSet, chunks: &[&[u8]]) -> String {
        let mut redactor = StreamRedactor::new(secrets.clone());
        let mut out = String::new();
        for chunk in chunks {
            out.push_str(&redactor.push(chunk));
        }
        out.push_str(&redactor.finish());
        out
    }

    #[test]
    fn a_key_split_across_two_chunks_is_masked_whole() {
        let key = fake_key();
        let text = format!("token {key} end\n");
        let split = text.find("a1B2").unwrap() + 3;
        let out = run(
            &SecretSet::new(),
            &[&text.as_bytes()[..split], &text.as_bytes()[split..]],
        );
        assert!(!out.contains("a1B2c3"), "{out}");
        assert!(out.contains("<redacted:sk-:"), "{out}");
        assert!(
            out.starts_with("token ") && out.ends_with(" end\n"),
            "{out}"
        );
    }

    #[test]
    fn a_bearer_token_on_the_next_line_of_an_earlier_chunk_is_masked() {
        let token = "Zq9".repeat(10);
        let out = run(
            &SecretSet::new(),
            &[
                b"curl -H 'Authorization: Bearer\n",
                format!("{token}' x\nnext\n").as_bytes(),
            ],
        );
        assert!(!out.contains(&token), "{out}");
        assert!(out.ends_with("next\n"), "{out}");
    }

    #[test]
    fn a_registered_value_split_across_chunks_is_masked() {
        let secrets = SecretSet::new();
        let value = "opaque-registered-credential-value-42";
        secrets.register(value);
        let out = run(
            &secrets,
            &[
                b"one\ntwo opaque-regis",
                b"tered-credential-value-42 three\n",
            ],
        );
        assert!(!out.contains("opaque-regis"), "{out}");
        assert!(out.contains("<redacted:secret:"), "{out}");
    }

    #[test]
    fn a_character_cut_by_a_chunk_is_decoded_whole() {
        let text = "grüße ✓\n".as_bytes();
        let chunks: Vec<&[u8]> = text.chunks(1).collect();
        assert_eq!(run(&SecretSet::new(), &chunks), "grüße ✓\n");
        // A byte that is no UTF-8 at all is shown as U+FFFD.
        assert_eq!(run(&SecretSet::new(), &[b"a\xffb\n"]), "a\u{fffd}b\n");
        assert_eq!(run(&SecretSet::new(), &[b"a\xc3"]), "a\u{fffd}");
    }

    #[test]
    fn a_long_line_is_held_at_most_max_held() {
        let mut redactor = StreamRedactor::new(SecretSet::new());
        let chunk = vec![b'x'; 16 * 1024];
        let mut shown = 0;
        for _ in 0..64 {
            shown += redactor.push(&chunk).len();
            assert!(
                redactor.held() <= MAX_HELD + chunk.len(),
                "{}",
                redactor.held()
            );
        }
        shown += redactor.finish().len();
        assert_eq!(shown, 64 * chunk.len());
    }

    #[test]
    fn a_long_line_is_cut_at_a_blank_outside_any_context() {
        let key = fake_key();
        let mut line = "word ".repeat(MAX_HELD / 5 + 10);
        line.push_str(&key);
        let split = line.len() - 10;
        let out = run(
            &SecretSet::new(),
            &[&line.as_bytes()[..split], &line.as_bytes()[split..]],
        );
        assert!(!out.contains("a1B2c3"), "{}", &out[out.len() - 80..]);
    }
}
