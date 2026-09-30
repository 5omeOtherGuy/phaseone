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
//! What is held is bounded: a line longer than [`MAX_HELD`] is cut without waiting for its
//! end, so memory stays at most [`MAX_HELD`] plus one chunk however long a line is. Such a
//! forced cut never stores a piece of a credential:
//! - it is made at a boundary (not between two credential-shaped characters, see [`is_run`])
//!   at least [`LOOKAHEAD`] bytes before the end of what is held, and only where masking the
//!   two sides apart gives exactly what masking them together gives: a context and its value
//!   (`"api_key":"…"`, `Bearer …`) that the cut would part are masked together, so the cut is
//!   refused there. [`LOOKAHEAD`] is longer than any credential value, so a value that begins
//!   after the cut is complete in what the check sees;
//! - when no such boundary is found (one run of credential-shaped characters fills the reach,
//!   or [`CUTS_TRIED`] boundaries are all refused), the cut is made anyway and whatever could
//!   hold a credential across it is masked on both sides: the run it falls in, from the run's
//!   start and on after the cut until the run ends, however many chunks that takes (only a
//!   count is kept); when the cut falls between runs, the [`GUARD`] bytes on each side,
//!   extended to whole runs, and to the end of the line when a context before the cut would
//!   take what follows it (`Authorization:` values may hold blanks).
//!
//! A long line with no credential is therefore stored as printed, save a run of
//! credential-shaped characters longer than the reach of a cut, which is masked.

use p1_redact::{SecretSet, redact_with};

/// The most text held back before a cut is forced (see the module documentation).
pub(crate) const MAX_HELD: usize = 64 * 1024;

/// How many boundaries a forced cut checks before it cuts and masks.
const CUTS_TRIED: usize = 16;

/// The text a forced cut leaves held after it: longer than any credential value, so a
/// value that starts after the cut is whole in what the cut is checked against.
const LOOKAHEAD: usize = 16 * 1024;

/// Bytes masked on each side of a forced cut that falls between runs (see the module
/// documentation): longer than any credential context and its separator.
const GUARD: usize = 128;

/// Whether `character` can be part of a credential value: what the credential shapes of
/// `p1_redact` take (keys, tokens, base64 and URL-encoded values).
fn is_run(character: char) -> bool {
    character.is_ascii_alphanumeric() || "-_./+=%~".contains(character)
}

/// The marker of text a forced cut masked; the family names why.
fn cut_marker(bytes: usize) -> String {
    format!("<redacted:cut:{bytes} chars>")
}

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
    /// A forced cut is still masking what follows it.
    masking: Option<Masking>,
}

/// What a forced cut still masks after itself.
struct Masking {
    /// Bytes masked so far after the cut.
    masked: usize,
    /// Mask at least this many bytes (the guard, or the rest of a registered value the cut
    /// fell in), then up to `until`.
    at_least: usize,
    until: Until,
}

/// Where the masking after a forced cut ends: before the first character that ends it.
#[derive(Clone, Copy)]
enum Until {
    /// A character that cannot be in a credential value ([`is_run`]).
    RunEnd,
    /// The end of the line: an open `Authorization` value, which may hold blanks.
    LineEnd,
    /// A blank, a quote or the end of the line: an open token (`Bearer …`).
    TokenEnd,
    /// The closing, unescaped quote of an open JSON string, or the end of the line.
    StringEnd { escaped: bool },
}

impl Until {
    /// Whether `character` is still masked, updating the escape state.
    fn takes(&mut self, character: char) -> bool {
        if character == '\n' {
            return false;
        }
        match self {
            Until::RunEnd => is_run(character),
            Until::LineEnd => true,
            Until::TokenEnd => !character.is_whitespace() && !"\"'`".contains(character),
            Until::StringEnd { escaped } => {
                if *escaped {
                    *escaped = false;
                    true
                } else if character == '\\' {
                    *escaped = true;
                    true
                } else {
                    character != '"'
                }
            }
        }
    }
}

/// A token-shaped value that no credential rule takes on its own (see [`StreamRedactor::open`]).
const OPEN_PROBE: &str = "p1probe0value0that0any0token0rule0takes";

/// Where the pending text is cut.
enum Cut {
    /// Mask and show `pending[..n]`.
    At(usize),
    /// A forced cut at the second index: `pending[first..second]` is replaced by a marker,
    /// and the [`Masking`] goes on after the cut.
    Masking(usize, usize, Masking),
}

impl StreamRedactor {
    pub(crate) fn new(secrets: SecretSet) -> Self {
        Self {
            secrets,
            undecoded: Vec::new(),
            pending: String::new(),
            masking: None,
        }
    }

    /// Take `chunk` in and return the masked text that no later chunk can change. Bytes that
    /// are not UTF-8 become U+FFFD, as the tool result shows them.
    pub(crate) fn push(&mut self, chunk: &[u8]) -> String {
        self.decode(chunk);
        let mut shown = self.continue_masking();
        if self.masking.is_some() {
            return shown;
        }
        match self.cut() {
            Cut::At(0) => {}
            Cut::At(cut) => {
                shown.push_str(&redact_with(&self.pending[..cut], &self.secrets).text);
                self.pending.drain(..cut);
            }
            Cut::Masking(start, cut, masking) => {
                shown.push_str(&redact_with(&self.pending[..start], &self.secrets).text);
                shown.push_str(&cut_marker(cut - start));
                self.pending.drain(..cut);
                self.masking = Some(masking);
                shown.push_str(&self.continue_masking());
            }
        }
        shown
    }

    /// Masks what a forced cut still owes from the front of `pending`, and its marker once
    /// the masked stretch ends.
    fn continue_masking(&mut self) -> String {
        let Some(masking) = self.masking.as_mut() else {
            return String::new();
        };
        let mut end = None;
        let mut taken = 0;
        for (at, character) in self.pending.char_indices() {
            let masked = masking.masked + at;
            // The end condition sees every character, so a string's escapes are tracked
            // through the guard too.
            let until = masking.until.takes(character);
            let takes = character != '\n' && masked < masking.at_least || until;
            if !takes {
                end = Some(at);
                break;
            }
            taken = at + character.len_utf8();
        }
        let taken = end.unwrap_or(taken);
        masking.masked += taken;
        self.pending.drain(..taken);
        if end.is_none() {
            return String::new();
        }
        let masked = masking.masked;
        self.masking = None;
        cut_marker(masked)
    }

    /// The stream ended: mask and return everything still held.
    pub(crate) fn finish(&mut self) -> String {
        if !self.undecoded.is_empty() {
            self.undecoded.clear();
            self.pending.push(char::REPLACEMENT_CHARACTER);
        }
        let mut shown = self.continue_masking();
        if let Some(masking) = self.masking.take() {
            // The stream ended inside the masked stretch.
            shown.push_str(&cut_marker(masking.masked));
            return shown;
        }
        let rest = std::mem::take(&mut self.pending);
        if !rest.is_empty() {
            shown.push_str(&redact_with(&rest, &self.secrets).text);
        }
        shown
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
    fn cut(&self) -> Cut {
        let text = self.pending.as_str();
        let mut cut = text.rfind('\n').map_or(0, |end| end + 1);
        // The last finished line may be a context whose value is on the next line, and a
        // registered value may hold a line break: neither is ever cut.
        while cut > 0 && (self.continues(&text[..cut]) || self.splits_registered(text, cut)) {
            cut = text[..cut - 1].rfind('\n').map_or(0, |end| end + 1);
        }
        if cut == 0 && text.len() > MAX_HELD {
            return self.forced_cut(text);
        }
        // A suffix that begins a registered value waits for the rest of it.
        Cut::At(cut.min(self.secrets.shown_len(text)))
    }

    /// Whether a cut at `cut` parts a registered value found whole in `text`.
    fn splits_registered(&self, text: &str, cut: usize) -> bool {
        if self.secrets.is_empty() {
            return false;
        }
        let mut apart = self.secrets.mask(&text[..cut]).text;
        apart.push_str(&self.secrets.mask(&text[cut..]).text);
        apart != self.secrets.mask(text).text
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

    /// Where a credential context left open at the end of `left` ends, if one is: asked of
    /// `p1_redact` itself with a probe value after `left`, so the rule follows the matcher.
    /// An `Authorization` value takes a blank, a token does not, and a JSON string value is
    /// masked only once its closing quote arrives.
    fn open(&self, left: &str) -> Option<Until> {
        let takes = |follows: &str| {
            !redact_with(&format!("{left}{follows}"), &self.secrets)
                .text
                .contains(OPEN_PROBE)
        };
        if takes(&format!("x {OPEN_PROBE}")) {
            Some(Until::LineEnd)
        } else if takes(OPEN_PROBE) {
            Some(Until::TokenEnd)
        } else if takes(&format!("{OPEN_PROBE}\"")) {
            Some(Until::StringEnd { escaped: false })
        } else {
            None
        }
    }

    /// The last position before `cut` where `holds` is false and the first after it where it
    /// is true, given it is false at 0 and true at `cut`: where a stretch that is open at `cut`
    /// began (a binary search, so a bounded number of checks).
    fn start_of(text: &str, cut: usize, holds: impl Fn(usize) -> bool) -> (usize, usize) {
        let (mut low, mut high) = (0, cut);
        while high - low > 1 {
            let mut middle = low + (high - low) / 2;
            while !text.is_char_boundary(middle) {
                middle -= 1;
            }
            if middle <= low {
                break;
            }
            if holds(middle) {
                high = middle;
            } else {
                low = middle;
            }
        }
        (low, high)
    }

    /// A cut in a line longer than [`MAX_HELD`] (see the module documentation).
    fn forced_cut(&self, text: &str) -> Cut {
        let lowest = text.len() - MAX_HELD;
        let mut highest = text.len() - LOOKAHEAD;
        while !text.is_char_boundary(highest) {
            highest -= 1;
        }
        let whole = redact_with(text, &self.secrets).text;
        let mut tried = 0;
        let mut previous: Option<char> = text[highest..].chars().next();
        for (at, character) in text[..highest].char_indices().rev() {
            if at + character.len_utf8() < lowest || tried == CUTS_TRIED {
                break;
            }
            let cut = at + character.len_utf8();
            let boundary = !(is_run(character) && previous.is_some_and(is_run));
            previous = Some(character);
            if !boundary || cut < lowest {
                continue;
            }
            tried += 1;
            let mut apart = redact_with(&text[..cut], &self.secrets).text;
            apart.push_str(&redact_with(&text[cut..], &self.secrets).text);
            if apart == whole
                && cut.min(self.secrets.shown_len(text)) == cut
                && self.open(&text[..cut]).is_none()
            {
                return Cut::At(cut);
            }
        }
        self.masked_cut(text, highest)
    }

    /// The cut no safe boundary allowed: at `cut`, masking on both sides whatever could hold a
    /// credential across it (see the module documentation).
    fn masked_cut(&self, text: &str, cut: usize) -> Cut {
        let before = text[..cut].chars().next_back();
        let after = text[cut..].chars().next();
        let in_run = before.is_some_and(is_run) && after.is_some_and(is_run);
        let mut start = if in_run {
            cut
        } else {
            cut.saturating_sub(GUARD)
        };
        while !text.is_char_boundary(start) {
            start -= 1;
        }
        // Back to the start of the run `start` is in.
        start = text[..start]
            .char_indices()
            .rev()
            .take_while(|(_, character)| is_run(*character))
            .last()
            .map_or(start, |(at, _)| at);
        let mut until = Until::RunEnd;
        let mut at_least = if in_run { 0 } else { GUARD };
        // A credential value left open at the cut: masked from where it began to where it ends.
        if let Some(open) = self.open(&text[..cut]) {
            until = open;
            // The first position the value is open at: its opening quote or context is shown.
            let (_, open_at) = Self::start_of(text, cut, |at| self.open(&text[..at]).is_some());
            start = start.min(open_at);
        }
        // A registered value the cut falls in: masked whole, on both sides.
        if self.splits_registered(text, cut) {
            // The last position a cut would part no registered value: the value's start.
            let (value_at, _) = Self::start_of(text, cut, |at| self.splits_registered(text, at));
            start = start.min(value_at);
            // The first position after the cut that parts no registered value: the value's
            // end, found by the same bounded search from the other side.
            let (mut low, mut high) = (cut, text.len());
            while high - low > 1 {
                let mut middle = low + (high - low) / 2;
                while !text.is_char_boundary(middle) {
                    middle += 1;
                }
                if middle >= high {
                    break;
                }
                if self.splits_registered(text, middle) {
                    low = middle;
                } else {
                    high = middle;
                }
            }
            at_least = at_least.max(high - cut);
        }
        Cut::Masking(
            start,
            cut,
            Masking {
                masked: 0,
                at_least,
                until,
            },
        )
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
        // One run of a MiB crosses every forced cut, so all of it is masked.
        assert!(shown < 200, "{shown}");
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

    // --- round 2: a forced cut never stores a piece of a credential (ADR-0109 item 2) ---------

    /// Offset the forced cut is measured against: where [`MAX_HELD`] falls in the stream.
    const STRADDLED: usize = 64 * 1024;

    /// Minified JSON with no blank: many boundaries, no credential.
    fn json_filler(bytes: usize) -> String {
        let mut text = String::new();
        let mut id = 0;
        while text.len() < bytes {
            text.push_str(&format!(
                r#"{{"id":{id},"name":"item-{id}","tags":["a","b"]}},"#
            ));
            id += 1;
        }
        text.truncate(bytes);
        text
    }

    /// One run of credential-shaped characters: no boundary at all.
    fn run_filler(bytes: usize) -> String {
        "x".repeat(bytes)
    }

    /// `prefix` padded with `filler` so `secret` starts `before` bytes ahead of
    /// [`STRADDLED`], then `suffix` and filler to `total` bytes, with no line break.
    fn line_around(
        filler: fn(usize) -> String,
        prefix: &str,
        secret: &str,
        suffix: &str,
        before: usize,
        total: usize,
    ) -> String {
        let lead = STRADDLED - before - prefix.len();
        let mut line = filler(lead);
        line.push_str(prefix);
        line.push_str(secret);
        line.push_str(suffix);
        let rest = total - line.len();
        line.push_str(&filler(rest));
        assert!(!line.contains(['\n', ' ']));
        line
    }

    /// Feeds `text` in chunks of `size` bytes and returns everything shown.
    fn stream(text: &str, size: usize) -> String {
        let mut redactor = StreamRedactor::new(SecretSet::new());
        let mut out = String::new();
        for chunk in text.as_bytes().chunks(size) {
            out.push_str(&redactor.push(chunk));
            assert!(redactor.held() <= MAX_HELD + size, "{}", redactor.held());
        }
        out.push_str(&redactor.finish());
        out
    }

    /// Neither half of `secret` is in `out`: every 12-byte window of it is absent.
    fn assert_no_piece(out: &str, secret: &str, case: &str) {
        for start in 0..=secret.len() - 12 {
            let piece = &secret[start..start + 12];
            assert!(!out.contains(piece), "{case}: {piece} is shown");
        }
    }

    const CHUNKS: [usize; 4] = [16 * 1024, 7_000, 4_096, STRADDLED + 5];

    /// (a) A 200 KiB line with no blank, carrying an `sk-` key across the forced cut.
    #[test]
    fn a_key_across_a_forced_cut_in_a_line_without_blanks_is_never_shown() {
        let key = format!("sk-{}", "a1B2c3D4e5F6g7H8".repeat(4));
        for filler in [json_filler as fn(usize) -> String, run_filler] {
            for before in [1, 30, key.len() - 1] {
                let line = line_around(filler, ",", &key, ",", before, 200 * 1024);
                for size in CHUNKS {
                    let out = stream(&line, size);
                    assert_no_piece(&out, &key, &format!("before {before}, chunk {size}"));
                }
            }
        }
    }

    /// (b) The same with a credential only its JSON field name makes one.
    #[test]
    fn a_json_field_credential_across_a_forced_cut_is_never_shown() {
        let value = "Zq9Wm4Lp7Tr2Vx8Kd5Hn".repeat(3);
        let long_value = "Pq7Rs2Tu9Vw4".repeat(9 * 1024);
        for filler in [json_filler as fn(usize) -> String, run_filler] {
            for (secret, before) in [
                (value.as_str(), 1),
                (value.as_str(), 25),
                (value.as_str(), value.len() + 2),
                (value.as_str(), value.len() + 12),
                (long_value.as_str(), 50 * 1024),
            ] {
                let line = line_around(
                    filler,
                    r#"{"api_key":""#,
                    secret,
                    r#""},"#,
                    before,
                    300 * 1024,
                );
                for size in CHUNKS {
                    let out = stream(&line, size);
                    assert_no_piece(
                        &out,
                        &secret[..60.min(secret.len())],
                        &format!("before {before}, chunk {size}"),
                    );
                    assert_no_piece(
                        &out,
                        &secret[secret.len() - 60..],
                        &format!("before {before}, chunk {size}"),
                    );
                }
            }
        }
    }

    /// (c) A long line with no credential is stored exactly as printed; only a run of
    /// credential-shaped characters that crosses a forced cut is masked, and only that run.
    #[test]
    fn a_long_line_without_credentials_is_stored_as_printed_save_runs_across_a_cut() {
        let line = json_filler(300 * 1024);
        for size in CHUNKS {
            assert_eq!(stream(&line, size), line, "chunk {size}");
        }
        let head = json_filler(50 * 1024);
        let tail = json_filler(50 * 1024);
        let line = format!("{head},{},{tail}", run_filler(100 * 1024));
        for size in CHUNKS {
            let out = stream(&line, size);
            let start = format!("{head},");
            let end = format!(",{tail}");
            assert!(
                out.starts_with(&start) && out.ends_with(&end),
                "chunk {size}"
            );
            let middle = &out[start.len()..out.len() - end.len()];
            let masked: usize = middle
                .split_inclusive(" chars>")
                .map(|marker| {
                    let count = marker
                        .strip_prefix("<redacted:")
                        .and_then(|rest| rest.strip_suffix(" chars>"))
                        .and_then(|rest| rest.rsplit_once(':'))
                        .unwrap_or_else(|| panic!("chunk {size}: not a marker: {marker}"));
                    count.1.parse::<usize>().unwrap()
                })
                .sum();
            assert_eq!(masked, 100 * 1024, "chunk {size}: {middle}");
        }
    }

    // --- round 3: an open credential context and a multi-line registered value ---------------

    /// Review finding 1: a JSON auth field whose value holds non-credential characters and has
    /// not closed yet is masked across every forced cut, up to its closing quote.
    #[test]
    fn an_unfinished_json_credential_is_never_shown_across_a_cut() {
        let value = "Ab9!".repeat(30_000);
        let text = format!(r#"{{"api_key":"{value}"}},{}"#, json_filler(1024));
        for size in CHUNKS {
            let out = stream(&text, size);
            assert!(!out.contains("Ab9!Ab9!Ab9!"), "chunk {size}");
            assert!(out.starts_with(r#"{"api_key":""#), "chunk {size}");
            assert!(out.ends_with(&json_filler(1024)), "chunk {size}");
        }
    }

    /// The same for the two contexts whose values run on: an `Authorization` header (to the
    /// end of its line, blanks included) and a `Bearer` token.
    #[test]
    fn an_open_header_or_bearer_value_is_never_shown_across_a_cut() {
        let header = format!("Authorization: Basic {}\nnext\n", "ab cd!".repeat(20_000));
        let bearer = format!("x Bearer {} y\nnext\n", "Ab9!".repeat(30_000));
        for size in CHUNKS {
            let out = stream(&header, size);
            assert!(!out.contains("ab cd!ab cd!"), "header, chunk {size}");
            assert!(out.ends_with("next\n"), "header, chunk {size}");
            let out = stream(&bearer, size);
            assert!(!out.contains("Ab9!Ab9!Ab9!"), "bearer, chunk {size}");
            assert!(out.ends_with("next\n"), "bearer, chunk {size}");
        }
    }

    /// Review finding 2: a registered value holding a line break is never cut at that break.
    #[test]
    fn a_registered_value_with_a_line_break_is_masked_whole() {
        let secrets = SecretSet::new();
        let value = "opaque-first-half\nopaque-second-half";
        secrets.register(value);
        let text = format!("before\n{value}");
        let bytes = text.as_bytes();
        for split in [bytes.len(), 10, 20, 24, 30] {
            let out = run(&secrets, &[&bytes[..split], &bytes[split..]]);
            assert!(!out.contains("opaque-first"), "split {split}: {out}");
            assert!(!out.contains("opaque-second"), "split {split}: {out}");
            assert!(out.starts_with("before\n"), "split {split}: {out}");
        }
        // A PEM-sized value across a forced cut in a long line is masked whole too.
        let pem = format!("-----BEGIN-----\n{}\n-----END-----", "MIIEvQ".repeat(500));
        secrets.register(&pem);
        let line = format!("{}{pem}{}", json_filler(62 * 1024), json_filler(100 * 1024));
        let mut redactor = StreamRedactor::new(secrets.clone());
        let mut out = String::new();
        for chunk in line.as_bytes().chunks(16 * 1024) {
            out.push_str(&redactor.push(chunk));
        }
        out.push_str(&redactor.finish());
        assert!(!out.contains("MIIEvQMIIEvQ"), "the PEM body is shown");
    }
}
