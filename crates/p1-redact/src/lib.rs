//! Mask credentials in tool output.
//!
//! Issue #142: a tool result (a `cat` of an auth file, `env`, a provider key
//! printed by a command) entered the model's history, the journal and every later
//! request verbatim. This crate owns the ONE credential matcher and the
//! [`RedactingTool`] decorator the composition root wraps every assembled tool in,
//! so the text a tool returns is masked BEFORE `p1-core` turns it into a
//! [`ToolResultItem`] — history, journal and request then only ever see the masked
//! form, because they are all built from the same `ToolOutcome`.
//!
//! # Two matchers (issue #484)
//!
//! **Exact values.** A [`SecretSet`] holds the credential values p1 actually handles
//! (the host registers every credential it resolves). Any occurrence of a registered
//! value is masked wherever it appears — bare, inside a word, in any context — so a
//! credential needs no recognizable shape to be caught. Values shorter than
//! [`MIN_SECRET_LEN`] are not registered: masking every occurrence of a short string
//! would destroy ordinary text, and the explicit contexts below still catch them.
//! The set is an explicit handle the host composes and passes down (inside the
//! agent's [`MaskCounter`]), never a process-wide registry.
//!
//! **Shapes.** Values in these contexts are masked whether or not they are registered:
//!
//! - `sk-` keys: the fleet pattern (PR #51) finds the start — `sk-` followed by 20+
//!   alphanumerics, or one of the `ant`/`proj`/`or`/`svcacct`/`admin` modifiers, a dash
//!   and 20+ `[A-Za-z0-9_-]` — and the match is then extended over every following
//!   `[A-Za-z0-9_-]`, so no suffix of the key survives;
//! - `Authorization:` (any scheme): the complete header value, to the end of the line
//!   (or to the closing quote when the header text is itself quoted), whatever
//!   printable characters it holds;
//! - `Bearer <token>`: every nonempty token of printable non-space ASCII up to a quote,
//!   comma, backslash, backtick or closing bracket. A template placeholder
//!   (`{token}`, `${TOKEN}`, `$TOKEN`, `%s`) is not a credential and is left alone;
//! - both of the above may be separated from their value by whitespace that includes
//!   ONE line break (`Bearer\n<token>`);
//! - a JSON string value whose field name — JSON-unescaped, compared case-insensitively
//!   — is an auth field ([`AUTH_FIELDS`]: `key`, `access`, `refresh`, `token`,
//!   `api_key`, `access_token`, `refreshToken`, `id_token`, `client_secret`,
//!   `OPENAI_API_KEY`, …). Every NONEMPTY value is masked; the scanner is escape-aware,
//!   so an escaped quote or backslash inside the value cannot end the match early;
//! - URL query, fragment and form parameters `access_token`, `refresh_token`,
//!   `id_token`, `token`, `api_key`, `apikey`, `key`, `client_secret` (and `code` in a
//!   URL): the complete encoded value, up to `&`, `#`, whitespace or a quote.
//!
//! A matched value is replaced with `<redacted:family:N chars>`, keeping only the
//! family (`sk-`, `sk-ant-`, `Bearer`, `Authorization`, the JSON field or URL parameter
//! name, or `secret` for a registered value) and the masked value's length in bytes,
//! never a character of the secret itself. A line break inside a masked span is kept
//! after the marker, so a masked text has as many lines as the original.
//!
//! A long base64 blob is NOT a credential by shape: it is masked only when it is a
//! registered value or sits in one of the contexts above. A provider's thinking
//! signature is opaque continuation data (`ReplayData`), never tool-result text, so it
//! is never passed to [`redact`] and stays byte-for-byte intact (PLAN.md §9).
//!
//! Masking is idempotent: only a complete, well-formed `<redacted:family:N chars>`
//! marker counts as already masked, so a value that merely starts with `<` is masked.
//!
//! ADR-0083 §4 extends the layer (S3.6) to everything else a component produces that
//! reaches the model, the journal or the UI: the [`RedactingTool`] decorator masks the
//! declaration's description once at construction (the trait returns a reference, so the
//! masked value is stored), and the `target` and `edit` of every `describe` and the
//! `summary` and every string of the detail of every `describe_result`. A command's tail
//! is masked as ONE text, so a credential split between two tail lines is still found.
//!
//! The declaration's name and `kind` and the tool's identity are machine-consumed:
//! masking them would corrupt a schema, a grammar or a journalled identity. They are
//! CHECKED instead ([`check_declaration`]): a tool whose name, input schema, grammar or
//! identity carries a registered value or an unambiguous credential shape is refused at
//! assembly. The JSON field rule is not applied to schemas — a property named `token`
//! is ordinary schema text.

use std::borrow::Cow;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock, RwLock};

use p1_contracts::serde_json::Value;
use p1_contracts::tool::{EditPreview, ResultDescription, ResultDetail};
use p1_contracts::{
    BoxFuture, CallDescription, DeclarationKind, Effect, Tool, ToolCall, ToolContext,
    ToolDeclaration, ToolIdentity, ToolOutcome, ToolResultItem,
};
use regex::Regex;

/// The shortest value a [`SecretSet`] registers. Every occurrence of a registered value
/// is masked, so a very short one would mask ordinary words; explicit contexts (a JSON
/// auth field, a header, a URL parameter) still mask short values by position.
pub const MIN_SECRET_LEN: usize = 8;

/// The JSON field names whose string values are credentials, compared case-insensitively
/// after the name is JSON-unescaped.
pub const AUTH_FIELDS: [&str; 22] = [
    "key",
    "access",
    "refresh",
    "token",
    "api_key",
    "apikey",
    "access_token",
    "accesstoken",
    "refresh_token",
    "refreshtoken",
    "id_token",
    "idtoken",
    "client_secret",
    "clientsecret",
    "password",
    "secret_key",
    "authorization",
    "x-api-key",
    "openai_api_key",
    "anthropic_api_key",
    "auth_token",
    "session_token",
];

/// One regex for every positional shape. The fleet's `sk-` alternation (PR #51) finds
/// where a key starts; `Bearer` and `Authorization:` match only their prefix (the value is
/// taken by hand, see [`bearer_token_end`] and [`authorization_value_end`]). A complete
/// marker is matched first, so nothing is ever found inside one.
const SHAPES: &str = concat!(
    r"(?m)(?P<marker><redacted:[A-Za-z0-9_.-]+:[0-9]+ chars>)",
    r"|(?P<sk>sk-(?:[A-Za-z0-9]{20,}|(?:ant|proj|or|svcacct|admin)-[A-Za-z0-9_-]{20,})",
    r"|\bsk-[A-Za-z0-9_-]{20,})",
    r"|(?P<bearer>(?i:\bbearer)[ \t]*(?:\r?\n)?[ \t]*)",
    r"|(?P<authz>(?i:\bauthorization)[ \t]*:[ \t]*(?:\r?\n[ \t]*)?)",
    r#"|(?:^|[?&#;\s"'])(?P<uname>(?i:access_token|refresh_token|id_token|client_secret|api_key|apikey|token|key))=(?P<uvalue>[^&#\s"'<>]+)"#,
    r#"|[?&#](?P<cname>(?i:code))=(?P<cvalue>[^&#\s"'<>]+)"#,
);

/// The modifier families whose `sk-<modifier>-` prefix is kept in the marker.
const MODIFIERS: [&str; 5] = ["ant", "proj", "or", "svcacct", "admin"];

/// A value this long is unambiguous enough to refuse a machine-consumed text for (a
/// declaration's name, schema or grammar): below it, `Bearer token` in a schema's prose
/// is ordinary text.
const DECLARATION_MIN_VALUE: usize = 16;

fn shapes() -> &'static Regex {
    static COMPILED: OnceLock<Regex> = OnceLock::new();
    COMPILED.get_or_init(|| Regex::new(SHAPES).expect("the credential-shape pattern compiles"))
}

/// An `sk-` key at the very start of a text (the fleet alternation, anchored).
fn sk_key() -> &'static Regex {
    static COMPILED: OnceLock<Regex> = OnceLock::new();
    COMPILED.get_or_init(|| {
        Regex::new(concat!(
            r"^(?:sk-(?:[A-Za-z0-9]{20,}|(?:ant|proj|or|svcacct|admin)-[A-Za-z0-9_-]{20,})",
            r"|sk-[A-Za-z0-9_-]{20,})",
        ))
        .expect("the sk-key pattern compiles")
    })
}

fn marker_pattern() -> &'static Regex {
    static COMPILED: OnceLock<Regex> = OnceLock::new();
    COMPILED.get_or_init(|| {
        Regex::new(r"^<redacted:[A-Za-z0-9_.-]+:[0-9]+ chars>$").expect("the marker pattern")
    })
}

/// Whether `text` is exactly one complete marker.
fn is_marker(text: &str) -> bool {
    marker_pattern().is_match(text)
}

/// One masked text: the result plus how many values were replaced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Redaction {
    pub text: String,
    /// Number of credential values replaced. Never the values themselves.
    pub masked: usize,
}

/// The credential values p1 handles, masked wherever they appear (see the crate docs).
/// Cloning shares the set: the host registers into the handle it composed, and every
/// clone sees the value. `Debug` prints the count only.
#[derive(Clone, Default)]
pub struct SecretSet {
    /// Longest first, so a value that contains another is replaced whole.
    values: Arc<RwLock<Vec<String>>>,
}

impl std::fmt::Debug for SecretSet {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "SecretSet {{ {} value(s) }}", self.read().len())
    }
}

impl SecretSet {
    pub fn new() -> Self {
        Self::default()
    }

    fn read(&self) -> std::sync::RwLockReadGuard<'_, Vec<String>> {
        self.values
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Register one credential value. A value shorter than [`MIN_SECRET_LEN`] (or one
    /// already registered) is ignored.
    pub fn register(&self, value: &str) {
        if value.len() < MIN_SECRET_LEN {
            return;
        }
        let mut values = self
            .values
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if values.iter().any(|known| known == value) {
            return;
        }
        values.push(value.to_owned());
        values.sort_by(|a, b| b.len().cmp(&a.len()));
    }

    pub fn is_empty(&self) -> bool {
        self.read().is_empty()
    }

    /// [`redact_with`] this set.
    pub fn redact(&self, text: &str) -> Redaction {
        redact_with(text, self)
    }

    /// Mask every registered value in `text` and nothing else: no credential shape is
    /// looked for. For text a model produced (its answer, its reasoning, the arguments
    /// of a tool call), where a shape rule would corrupt ordinary content but an exact
    /// credential p1 handles must still never be shown, stored or sent on.
    pub fn mask(&self, text: &str) -> Redaction {
        let (replaced, masked) = replace_spans(text, find_spans(text, self, Mode::Registered));
        Redaction {
            text: replaced.into_owned(),
            masked,
        }
    }

    /// Whether `text` contains any registered value.
    pub fn contains_secret(&self, text: &str) -> bool {
        self.read()
            .iter()
            .any(|value| text.contains(value.as_str()))
    }

    /// The length in bytes of the longest suffix of `text` that is a proper prefix of a
    /// registered value: a streaming masker holds that much back until the next chunk
    /// shows whether a value starts there. Always on a char boundary of `text`.
    pub fn held_suffix_len(&self, text: &str) -> usize {
        let mut held = 0;
        for value in self.read().iter() {
            let longest = (value.len() - 1).min(text.len());
            for length in (held + 1..=longest).rev() {
                if value.is_char_boundary(length) && text.ends_with(&value[..length]) {
                    held = length;
                    break;
                }
            }
        }
        held
    }

    /// Every occurrence of every registered value, as spans.
    fn spans(&self, text: &str, spans: &mut Vec<Span>) {
        for value in self.read().iter() {
            for (start, found) in text.match_indices(value.as_str()) {
                spans.push(Span {
                    start,
                    end: start + found.len(),
                    value_start: start,
                    family: "secret".to_owned(),
                });
            }
        }
    }
}

/// Mask every credential-shaped string in `text` (shapes only: no registered values).
/// A text with no match is returned unchanged (`masked == 0`).
pub fn redact(text: &str) -> Redaction {
    redact_with(text, &SecretSet::new())
}

/// Mask every registered value of `secrets` and every credential shape in `text`.
pub fn redact_with(text: &str, secrets: &SecretSet) -> Redaction {
    let (replaced, masked) = redact_cow(text, secrets);
    Redaction {
        text: replaced.into_owned(),
        masked,
    }
}

/// [`redact_with`] without the copy of a clean text: the text is borrowed back unless a
/// value was replaced. A tool outcome can be a whole history (tens of MiB), almost always
/// clean, and copying it only to drop the copy was most of the wrapper's cost.
fn redact_cow<'a>(text: &'a str, secrets: &SecretSet) -> (Cow<'a, str>, usize) {
    replace_spans(text, find_spans(text, secrets, Mode::Output))
}

/// `text` with every span replaced by its marker; borrowed back when there is none.
fn replace_spans(text: &str, spans: Vec<Span>) -> (Cow<'_, str>, usize) {
    if spans.is_empty() {
        return (Cow::Borrowed(text), 0);
    }
    let mut out = String::with_capacity(text.len());
    let mut position = 0;
    for span in &spans {
        out.push_str(&text[position..span.start]);
        out.push_str(&format!(
            "<redacted:{}:{} chars>",
            span.family,
            span.end - span.value_start
        ));
        // Keep the line count: a span may swallow the one line break after `Bearer`.
        for _ in text[span.start..span.end].matches('\n') {
            out.push('\n');
        }
        position = span.end;
    }
    out.push_str(&text[position..]);
    (Cow::Owned(out), spans.len())
}

/// What the spans are for: masking output, or refusing a machine-consumed declaration
/// text (unambiguous shapes only, see [`check_declaration`]).
#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Output,
    Declaration,
    /// Registered values only ([`SecretSet::mask`]).
    Registered,
}

/// One text range to replace: `[start, end)` is replaced; `[value_start, end)` is the
/// value whose length the marker reports; `family` is the marker's family.
struct Span {
    start: usize,
    end: usize,
    value_start: usize,
    family: String,
}

/// Every range of `text` to mask, sorted and merged so that overlapping matches (a
/// registered value inside a `Bearer` token, a `Bearer` inside an `Authorization` value)
/// become one replacement that covers them all.
fn find_spans(text: &str, secrets: &SecretSet, mode: Mode) -> Vec<Span> {
    let mut spans = Vec::new();
    let mut markers: Vec<(usize, usize)> = Vec::new();
    let long_enough = |value: &str| mode == Mode::Output || value.len() >= DECLARATION_MIN_VALUE;
    // Registered values only: no shape is looked for.
    let shaped = if mode == Mode::Registered { "" } else { text };
    for captures in shapes().captures_iter(shaped) {
        if let Some(found) = captures.name("marker") {
            markers.push((found.start(), found.end()));
        } else if let Some(found) = captures.name("sk") {
            let end = key_end(text, found.end());
            let family = sk_family(&text[found.start()..end]);
            spans.push(Span {
                start: found.start(),
                end,
                value_start: found.start() + family.len(),
                family: family.to_owned(),
            });
        } else if let Some(found) = captures.name("bearer") {
            // `bearer` glued to the next word (`bearerToken`) is not the scheme.
            if found.as_str().len() == "bearer".len() {
                continue;
            }
            let value_start = found.end();
            let end = bearer_token_end(text, value_start);
            let token = &text[value_start..end];
            if token.is_empty() || is_placeholder(token) || !long_enough(token) {
                continue;
            }
            spans.push(Span {
                start: found.start(),
                end,
                value_start,
                family: "Bearer".to_owned(),
            });
        } else if let Some(found) = captures.name("authz") {
            let value_start = found.end();
            let Some(end) = authorization_value_end(text, found.start(), value_start) else {
                continue;
            };
            let value = &text[value_start..end];
            if already_masked_header(value) || !long_enough(value) {
                continue;
            }
            spans.push(Span {
                start: found.start(),
                end,
                value_start,
                family: "Authorization".to_owned(),
            });
        } else if let (Some(name), Some(value)) = (
            captures.name("uname").or_else(|| captures.name("cname")),
            captures.name("uvalue").or_else(|| captures.name("cvalue")),
        ) {
            if !long_enough(value.as_str()) {
                continue;
            }
            // The parameter match consumed the value, so an `sk-` key in it (`KEY=sk-…`)
            // is found here; pushed first, it names the family when it is the whole value.
            if let Some(key) = sk_key().find(value.as_str()) {
                let start = value.start() + key.start();
                let end = key_end(text, value.start() + key.end());
                let family = sk_family(&text[start..end]);
                spans.push(Span {
                    start,
                    end,
                    value_start: start + family.len(),
                    family: family.to_owned(),
                });
            }
            spans.push(Span {
                start: value.start(),
                end: value.end(),
                value_start: value.start(),
                family: name.as_str().to_ascii_lowercase(),
            });
        }
    }
    if mode == Mode::Output {
        json_field_spans(text, &mut spans);
    }
    secrets.spans(text, &mut spans);

    // Nothing starts inside an existing marker or lies wholly within one: that keeps
    // masking idempotent. A span that starts at a marker and runs past it (`<marker> more`)
    // is kept, so text after a marker never escapes.
    spans.retain(|span| {
        !markers.iter().any(|(start, end)| {
            (*start < span.start && span.start < *end) || (*start <= span.start && span.end <= *end)
        })
    });
    spans.sort_by(|a, b| a.start.cmp(&b.start).then(b.end.cmp(&a.end)));
    let mut merged: Vec<Span> = Vec::with_capacity(spans.len());
    for span in spans {
        match merged.last_mut() {
            Some(last) if span.start < last.end => last.end = last.end.max(span.end),
            _ => merged.push(span),
        }
    }
    merged
}

/// The end of an `sk-` key: the fleet pattern found its start, and every following
/// `[A-Za-z0-9_-]` still belongs to it.
fn key_end(text: &str, from: usize) -> usize {
    from + text[from..]
        .bytes()
        .take_while(|byte| byte.is_ascii_alphanumeric() || *byte == b'_' || *byte == b'-')
        .count()
}

/// The family prefix of an `sk-` token: `sk-` alone, or `sk-<modifier>-`. A pure
/// prefix of the match, so no secret character is kept.
fn sk_family(token: &str) -> &str {
    let Some(rest) = token.strip_prefix("sk-") else {
        return "sk-";
    };
    for modifier in MODIFIERS {
        if let Some(after) = rest.strip_prefix(modifier)
            && after.starts_with('-')
        {
            return &token[.."sk-".len() + modifier.len() + 1];
        }
    }
    "sk-"
}

/// A byte a `Bearer` token may hold: printable non-space ASCII except the delimiters that
/// end a token in running text (quotes, comma, backslash, backtick, closing brackets).
fn is_bearer_byte(byte: u8) -> bool {
    (33..=126).contains(&byte)
        && !matches!(
            byte,
            b'"' | b'\'' | b',' | b'\\' | b'`' | b')' | b']' | b'}' | b'>'
        )
}

/// The end of the token after `Bearer `. A complete marker inside the run ends it, so an
/// already masked value is never swallowed into a new one.
fn bearer_token_end(text: &str, from: usize) -> usize {
    let run = text[from..]
        .bytes()
        .take_while(|byte| is_bearer_byte(*byte) || *byte == b'>');
    let mut end = from + run.count();
    // `>` is only allowed so a marker can be recognized; a token never ends inside one.
    if let Some(offset) = text[from..end].find("<redacted:") {
        end = from + offset;
    }
    // A trailing `>` that is not part of a marker is a closing bracket.
    while end > from && text.as_bytes()[end - 1] == b'>' {
        end -= 1;
    }
    end
}

/// A template placeholder in source text (`Bearer {token}`, `Bearer ${TOKEN}`,
/// `Bearer $TOKEN`, `Bearer %s`), not a credential.
fn is_placeholder(token: &str) -> bool {
    if token == "%s" || token == "{" {
        return true;
    }
    let Some(rest) = token
        .strip_prefix("${")
        .or_else(|| token.strip_prefix('{'))
        .or_else(|| token.strip_prefix('$'))
    else {
        return false;
    };
    !rest.is_empty()
        && rest
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'.' | b':'))
}

/// The end of an `Authorization:` value: the end of the line, or — when the header text
/// itself sits in quotes (`-H "Authorization: …"`) — the closing quote on that line.
/// Trailing blanks are not part of the value. `None` for an empty value.
fn authorization_value_end(text: &str, prefix_start: usize, value_start: usize) -> Option<usize> {
    let bytes = text.as_bytes();
    let line_end = text[value_start..]
        .find(['\n', '\r'])
        .map_or(text.len(), |offset| value_start + offset);
    let mut end = line_end;
    if let Some(quote) = prefix_start
        .checked_sub(1)
        .map(|index| bytes[index])
        .filter(|byte| matches!(byte, b'"' | b'\''))
    {
        let mut index = value_start;
        while index < line_end {
            match bytes[index] {
                b'\\' => index += 2,
                byte if byte == quote => {
                    end = index;
                    break;
                }
                _ => index += 1,
            }
        }
    }
    while end > value_start && matches!(bytes[end - 1], b' ' | b'\t') {
        end -= 1;
    }
    (end > value_start).then_some(end)
}

/// An `Authorization` value that is already only a marker, with or without its scheme.
fn already_masked_header(value: &str) -> bool {
    if is_marker(value) {
        return true;
    }
    match value.split_once([' ', '\t']) {
        Some((scheme, rest)) => {
            scheme.bytes().all(|byte| byte.is_ascii_alphabetic()) && is_marker(rest.trim())
        }
        None => false,
    }
}

/// Whether a (JSON-unescaped) field name is an auth field.
fn is_auth_field(name: &str) -> bool {
    AUTH_FIELDS
        .iter()
        .any(|field| field.eq_ignore_ascii_case(name))
}

/// The value of every `"<auth field>": "<value>"` pair: an escape-aware scan over JSON
/// string literals, so an escaped quote cannot end a value early and an escaped name
/// (`"\u0074oken"`) is still recognized. Only the value's contents are replaced; the
/// quotes and the name stay. Text that is not JSON is scanned the same way: a quote that
/// opens no complete string on its line is skipped. Every quote is tried as an opener at
/// most once more, so the scan stays linear.
fn json_field_spans(text: &str, spans: &mut Vec<Span>) {
    let bytes = text.as_bytes();
    let mut index = 0;
    while let Some(offset) = bytes[index..].iter().position(|byte| *byte == b'"') {
        let open = index + offset;
        let Some((close, name)) = json_string(bytes, open, true) else {
            index = open + 1;
            continue;
        };
        let colon = skip_blank(bytes, close + 1);
        if bytes.get(colon) != Some(&b':') {
            // Not a field name. Its closing quote may really open the next string (a stray
            // quote earlier on the line shifts the pairing), so it is tried as an opener.
            index = close;
            continue;
        }
        let value_open = skip_blank(bytes, colon + 1);
        if bytes.get(value_open) != Some(&b'"') {
            index = close + 1;
            continue;
        }
        let Some((value_close, _)) = json_string(bytes, value_open, false) else {
            index = value_open + 1;
            continue;
        };
        if let Some(name) = name.filter(|name| is_auth_field(name)) {
            let (start, end) = (value_open + 1, value_close);
            if end > start && !is_marker(&text[start..end]) {
                spans.push(Span {
                    start,
                    end,
                    value_start: start,
                    family: name,
                });
            }
        }
        index = value_close + 1;
    }
}

fn skip_blank(bytes: &[u8], mut index: usize) -> usize {
    while bytes
        .get(index)
        .is_some_and(|byte| matches!(byte, b' ' | b'\t' | b'\r' | b'\n'))
    {
        index += 1;
    }
    index
}

/// The JSON string literal opening at `open`: the index of its closing quote and, when
/// `decode` is set and the literal is short enough to be a field name, its decoded text.
/// `None` when the literal does not close before a raw control character or the end.
fn json_string(bytes: &[u8], open: usize, decode: bool) -> Option<(usize, Option<String>)> {
    const NAME_LIMIT: usize = 64;
    let mut decoded = decode.then(String::new);
    let mut index = open + 1;
    while index < bytes.len() {
        let byte = bytes[index];
        match byte {
            b'"' => return Some((index, decoded.filter(|name| name.len() <= NAME_LIMIT))),
            byte if byte < 0x20 => return None,
            b'\\' => {
                let escaped = *bytes.get(index + 1)?;
                let (character, width) = match escaped {
                    b'u' => {
                        let hex = std::str::from_utf8(bytes.get(index + 2..index + 6)?).ok()?;
                        let code = u32::from_str_radix(hex, 16).ok()?;
                        (char::from_u32(code).unwrap_or('\u{fffd}'), 6)
                    }
                    b'n' => ('\n', 2),
                    b't' => ('\t', 2),
                    b'r' => ('\r', 2),
                    b'b' => ('\u{8}', 2),
                    b'f' => ('\u{c}', 2),
                    other => (char::from(other), 2),
                };
                if let Some(name) = decoded.as_mut()
                    && name.len() <= NAME_LIMIT
                {
                    name.push(character);
                }
                index += width;
            }
            _ => {
                if let Some(name) = decoded.as_mut()
                    && name.len() <= NAME_LIMIT
                {
                    // Names are compared with ASCII fields; any other byte just keeps the
                    // name from matching.
                    name.push(if byte.is_ascii() {
                        char::from(byte)
                    } else {
                        '\u{fffd}'
                    });
                }
                index += 1;
            }
        }
    }
    None
}

/// Refuse a tool whose machine-consumed declaration text carries a credential: its name,
/// its input schema (every key and string value) or grammar, or its identity. Masking
/// these would corrupt a schema, a grammar or a journalled identity, so the tool is
/// refused instead. Only registered values and unambiguous shapes count here (`sk-` keys,
/// `Bearer`/`Authorization` values and URL credential parameters of 16+ characters); the
/// JSON field rule is not applied, since a schema property named `token` is ordinary
/// schema text. The error names the field, never the value.
pub fn check_declaration(
    declaration: &ToolDeclaration,
    identity: &ToolIdentity,
    secrets: &SecretSet,
) -> Result<(), String> {
    let carries = |text: &str| !find_spans(text, secrets, Mode::Declaration).is_empty();
    let refuse = |field: &str| -> Result<(), String> {
        Err(format!(
            "the tool's {field} carries a credential value; the tool is refused"
        ))
    };
    if carries(&declaration.name) {
        return refuse("name");
    }
    match &declaration.kind {
        DeclarationKind::Function { input_schema } => {
            if schema_carries(input_schema, &carries) {
                return refuse("input schema");
            }
        }
        DeclarationKind::Freeform {
            grammar: Some(grammar),
        } => {
            if carries(&grammar.syntax) || carries(&grammar.definition) {
                return refuse("grammar");
            }
        }
        DeclarationKind::Freeform { grammar: None } => {}
    }
    if carries(&identity.implementation) || carries(&identity.variant) {
        return refuse("identity");
    }
    Ok(())
}

fn schema_carries(value: &Value, carries: &dyn Fn(&str) -> bool) -> bool {
    match value {
        Value::String(text) => carries(text),
        Value::Array(items) => items.iter().any(|item| schema_carries(item, carries)),
        Value::Object(fields) => fields
            .iter()
            .any(|(name, field)| carries(name) || schema_carries(field, carries)),
        _ => false,
    }
}

/// Values masked since the last [`MaskCounter::take`]. One per agent, so the host
/// can report a per-turn count through an existing notice path without ever
/// touching a value. It also carries the agent's [`SecretSet`], so every tool the
/// agent assembles masks the registered values too.
#[derive(Debug, Default)]
pub struct MaskCounter {
    masked: AtomicUsize,
    secrets: SecretSet,
}

impl MaskCounter {
    /// A counter with an empty [`SecretSet`]: shapes only.
    pub fn new() -> Self {
        Self::default()
    }

    /// A counter whose tools also mask every value registered in `secrets`.
    pub fn with_secrets(secrets: SecretSet) -> Self {
        Self {
            masked: AtomicUsize::new(0),
            secrets,
        }
    }

    /// The registered values this counter's tools mask.
    pub fn secrets(&self) -> &SecretSet {
        &self.secrets
    }

    /// Count `masked` more replaced values.
    pub fn add(&self, masked: usize) {
        if masked > 0 {
            self.masked.fetch_add(masked, Ordering::SeqCst);
        }
    }

    /// The count so far, reset to zero.
    pub fn take(&self) -> usize {
        self.masked.swap(0, Ordering::SeqCst)
    }
}

/// A [`Tool`] decorator: it forwards identity and effect to the tool it wraps, masks
/// its declaration description once at construction, and rewrites the text of every
/// [`ToolOutcome`], `describe` and `describe_result` through [`redact_with`] and the
/// counter's [`SecretSet`] before the core or the UI can see it. Every masked value is
/// added to `counter`.
pub struct RedactingTool {
    inner: Arc<dyn Tool>,
    /// The declaration with its description already masked. `Tool::declaration` returns a
    /// reference, so the masked value has to be stored at construction (ADR-0083 §4).
    declaration: ToolDeclaration,
    counter: Arc<MaskCounter>,
}

impl RedactingTool {
    pub fn new(inner: Arc<dyn Tool>, counter: Arc<MaskCounter>) -> Self {
        let declared = inner.declaration();
        let description = redact_with(&declared.description, counter.secrets());
        counter.add(description.masked);
        let declaration = ToolDeclaration {
            name: declared.name.clone(),
            description: description.text,
            // `kind` passes through: its schema/grammar text is machine-consumed, so it is
            // checked at assembly ([`check_declaration`]) instead of masked.
            kind: declared.kind.clone(),
        };
        Self {
            inner,
            declaration,
            counter,
        }
    }

    /// Mask one text and count what was replaced.
    fn mask(&self, text: &str) -> String {
        let redaction = redact_with(text, self.counter.secrets());
        self.counter.add(redaction.masked);
        redaction.text
    }

    /// Mask a list of lines as ONE text, so a credential split between two lines (a
    /// `Bearer` at the end of one, its token on the next) is still found. The masked
    /// text keeps every line break, so the list keeps its length.
    fn mask_lines(&self, lines: &[String]) -> Vec<String> {
        if lines.is_empty() {
            return Vec::new();
        }
        self.mask(&lines.join("\n"))
            .split('\n')
            .map(str::to_owned)
            .collect()
    }

    /// Mask every string of a call description: its `target` and an edit preview's
    /// `path`, `old` and `new`. The verb and the destructive flag are closed values.
    fn mask_call_description(&self, mut description: CallDescription) -> CallDescription {
        description.target = description.target.map(|target| self.mask(&target));
        if let Some(edit) = description.edit.take() {
            description.edit = Some(EditPreview {
                path: self.mask(&edit.path),
                old: self.mask(&edit.old),
                new: self.mask(&edit.new),
            });
        }
        description
    }

    /// Mask every string of a result description: its `summary` and every string of its
    /// detail (a diff's path and sides, a command's tail, match and file paths, free text).
    fn mask_result_description(&self, mut description: ResultDescription) -> ResultDescription {
        description.summary = self.mask(&description.summary);
        description.detail = description
            .detail
            .take()
            .map(|detail| self.mask_detail(detail));
        description
    }

    fn mask_detail(&self, detail: ResultDetail) -> ResultDetail {
        match detail {
            ResultDetail::Diff {
                path,
                before,
                after,
            } => ResultDetail::Diff {
                path: self.mask(&path),
                before: self.mask(&before),
                after: self.mask(&after),
            },
            ResultDetail::Command {
                exit_code,
                elapsed_ms,
                tail,
            } => ResultDetail::Command {
                exit_code,
                elapsed_ms,
                tail: self.mask_lines(&tail),
            },
            ResultDetail::Matches { count, files } => ResultDetail::Matches {
                count,
                files: files.iter().map(|file| self.mask(file)).collect(),
            },
            ResultDetail::Files { paths } => ResultDetail::Files {
                paths: paths.iter().map(|path| self.mask(path)).collect(),
            },
            ResultDetail::Text(text) => ResultDetail::Text(self.mask(&text)),
        }
    }
}

/// Wrap one assembled tool. A non-zero mask count is added to `counter`, and the
/// counter's [`SecretSet`] is masked too.
pub fn redacted(tool: Arc<dyn Tool>, counter: &Arc<MaskCounter>) -> Arc<dyn Tool> {
    Arc::new(RedactingTool::new(tool, counter.clone()))
}

impl Tool for RedactingTool {
    fn declaration(&self) -> &ToolDeclaration {
        &self.declaration
    }

    fn identity(&self) -> &ToolIdentity {
        self.inner.identity()
    }

    fn effect(&self, call: &ToolCall) -> Effect {
        self.inner.effect(call)
    }

    fn take_command_exit_code(&self, call_id: &str) -> Option<i32> {
        self.inner.take_command_exit_code(call_id)
    }

    fn command_exit_code(&self, call_id: &str) -> Option<i32> {
        self.inner.command_exit_code(call_id)
    }

    fn synthetic_command_result(&self) -> bool {
        self.inner.synthetic_command_result()
    }

    fn describe(&self, call: &ToolCall) -> CallDescription {
        self.mask_call_description(self.inner.describe(call))
    }

    fn describe_result(&self, call: &ToolCall, result: &ToolResultItem) -> ResultDescription {
        self.mask_result_description(self.inner.describe_result(call, result))
    }

    fn execute<'a>(
        &'a self,
        call: &'a ToolCall,
        context: ToolContext,
    ) -> BoxFuture<'a, ToolOutcome> {
        Box::pin(async move {
            let outcome = self.inner.execute(call, context).await;
            let (text, masked) = redact_cow(&outcome.content, self.counter.secrets());
            let Cow::Owned(content) = text else {
                return outcome;
            };
            self.counter.add(masked);
            ToolOutcome {
                status: outcome.status,
                content,
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A key-shaped value built at runtime: never a literal in the tree.
    fn key(prefix: &str, length: usize) -> String {
        format!("{prefix}{}", "A".repeat(length))
    }

    #[test]
    fn each_pattern_family_is_masked_and_keeps_only_the_family() {
        let plain = key("sk-", 24);
        let anthropic = key("sk-ant-", 24);
        let project = key("sk-proj-", 24);
        let bearer = key("", 40);
        let authorization = key("", 32);
        let json = key("", 20);
        let text = format!(
            "plain={plain}\nanthropic={anthropic}\nproject={project}\n\
             header: Bearer {bearer}\nauth: Authorization: {authorization}\n\
             {{\"api_key\": \"{json}\"}}"
        );

        let redaction = redact(&text);

        assert_eq!(redaction.masked, 6, "{}", redaction.text);
        assert_eq!(
            redaction.text,
            "plain=<redacted:sk-:24 chars>\nanthropic=<redacted:sk-ant-:24 chars>\n\
             project=<redacted:sk-proj-:24 chars>\n\
             header: <redacted:Bearer:40 chars>\nauth: <redacted:Authorization:32 chars>\n\
             {\"api_key\": \"<redacted:api_key:20 chars>\"}"
        );
        // No character of any secret survives.
        for secret in [&plain, &anthropic, &project, &bearer, &authorization, &json] {
            assert!(!redaction.text.contains(secret.as_str()));
        }
        assert!(!redaction.text.contains(&"A".repeat(20)));
    }

    #[test]
    fn the_json_key_families_are_named_in_the_marker() {
        for json_key in [
            "key",
            "access",
            "refresh",
            "api_key",
            "token",
            "accessToken",
            "refreshToken",
            "access_token",
            "refresh_token",
            "id_token",
            "idToken",
            "apiKey",
            "OPENAI_API_KEY",
            "ANTHROPIC_API_KEY",
            "client_secret",
            "password",
        ] {
            let value = key("", 18);
            let text = format!("{{\"{json_key}\": \"{value}\"}}");
            let redaction = redact(&text);
            assert_eq!(redaction.masked, 1, "{text}");
            assert_eq!(
                redaction.text,
                format!("{{\"{json_key}\": \"<redacted:{json_key}:18 chars>\"}}")
            );
        }
    }

    #[test]
    fn a_key_name_that_only_ends_in_a_family_word_is_left_alone() {
        let text = format!("{{\"monkey\": \"{}\"}}", key("", 18));
        assert_eq!(redact(&text).masked, 0, "{text}");
    }

    #[test]
    fn short_or_unrelated_values_are_left_alone() {
        // 19 characters after `sk-` is one short of the bare-key pattern.
        let short = key("sk-", 19);
        // A plain long base64 blob (a thinking-signature shape) is not a key.
        let base64 = format!("{}{}", "QWxsQWJj".repeat(6), "ZGVm");
        // A field that is not an auth field, and source-code placeholders after `Bearer`.
        let text = format!(
            "{short}\n{base64}\nplain sentence\n{{\"name\": \"short\"}}\n\
             format!(\"Bearer {{token}}\") Bearer ${{TOKEN}} Bearer $TOKEN\n\
             exit code=1 and a bearerToken field"
        );

        let redaction = redact(&text);

        assert_eq!(redaction.masked, 0, "{}", redaction.text);
        assert_eq!(redaction.text, text);
    }

    #[test]
    fn masking_is_idempotent() {
        let text = format!(
            "{} {} {} {{\"refresh\": \"{}\"}}",
            key("sk-", 24),
            key("sk-ant-", 30),
            key("sk-proj-", 22),
            key("", 24)
        );
        let once = redact(&text);
        assert_eq!(once.masked, 4, "{}", once.text);

        let twice = redact(&once.text);

        assert_eq!(twice.masked, 0, "{}", twice.text);
        assert_eq!(twice.text, once.text);
    }

    #[test]
    fn masking_every_context_is_idempotent() {
        let secrets = SecretSet::new();
        secrets.register(&key("opaque-", 12));
        let text = format!(
            "Authorization: Basic {}\nBearer {}\n{{\"accessToken\": \"{}\"}}\n\
             https://x/cb?access_token={}&state=1\nbare {} here\n\"Authorization: Bearer x\"",
            key("", 30),
            key("", 3),
            key("", 4),
            key("", 5),
            key("opaque-", 12),
        );
        let once = redact_with(&text, &secrets);
        assert_eq!(once.masked, 6, "{}", once.text);

        let twice = redact_with(&once.text, &secrets);

        assert_eq!(twice.masked, 0, "{}", twice.text);
        assert_eq!(twice.text, once.text);
    }

    #[test]
    fn explicit_contexts_mask_values_of_any_length() {
        // Finding 17: a short key p1-auth accepts is masked wherever its context says
        // it is a credential.
        for (text, family) in [
            ("{\"token\": \"abc\"}".to_owned(), "token"),
            ("Bearer abc".to_owned(), "Bearer"),
            ("Authorization: abc".to_owned(), "Authorization"),
            (
                "GET /x?access_token=abc HTTP/1.1".to_owned(),
                "access_token",
            ),
        ] {
            let redaction = redact(&text);
            assert_eq!(redaction.masked, 1, "{text} -> {}", redaction.text);
            assert!(
                redaction
                    .text
                    .contains(&format!("<redacted:{family}:3 chars>")),
                "{}",
                redaction.text
            );
            assert!(!redaction.text.contains("abc"), "{}", redaction.text);
        }
        // An empty value is not a credential.
        assert_eq!(redact("{\"token\": \"\"}").masked, 0);
    }

    #[test]
    fn an_escaped_field_name_is_recognized() {
        let value = key("", 10);
        let text = format!("{{\"\\u0061ccess_token\": \"{value}\"}}");
        let redaction = redact(&text);
        assert_eq!(redaction.masked, 1, "{}", redaction.text);
        assert!(!redaction.text.contains(&value));
    }

    #[test]
    fn a_json_pair_after_a_stray_quote_is_still_found() {
        let value = key("", 10);
        let text = format!("it\"s here: {{\"key\": \"{value}\"}}");
        let redaction = redact(&text);
        assert!(!redaction.text.contains(&value), "{}", redaction.text);
    }

    #[test]
    fn an_sk_key_is_masked_to_its_last_character() {
        // Finding 18: a separator after a long alphanumeric run does not end the key.
        for suffix in ["_tail", "-tail", "_tail-more_x", "--tail__"] {
            let token = format!("{}{suffix}", key("sk-", 24));
            for context in ["value", "KEY="] {
                let separator = if context.ends_with('=') { "" } else { " " };
                let redaction = redact(&format!("{context}{separator}{token} next"));
                assert_eq!(redaction.masked, 1, "{}", redaction.text);
                assert_eq!(
                    redaction.text,
                    format!(
                        "{context}{separator}<redacted:sk-:{} chars> next",
                        token.len() - 3
                    )
                );
            }
        }
    }

    /// Every punctuation byte `p1-auth`'s `usable_key` accepts (33..=126 without the
    /// alphanumerics).
    fn punctuation() -> impl Iterator<Item = char> {
        (33u8..=126)
            .filter(|byte| !byte.is_ascii_alphanumeric())
            .map(char::from)
    }

    #[test]
    fn an_authorization_value_is_masked_whole_whatever_it_holds() {
        // Finding 19: any scheme, any printable punctuation after a long alphanumeric run.
        for scheme in ["Bearer ", "Basic ", "Digest ", ""] {
            for mark in punctuation() {
                let value = format!("{scheme}{}{mark}{}", key("", 20), "B".repeat(6));
                let text = format!("Authorization: {value}\nnext line");
                let redaction = redact(&text);
                assert_eq!(redaction.masked, 1, "{text:?} -> {:?}", redaction.text);
                assert_eq!(
                    redaction.text,
                    format!("<redacted:Authorization:{} chars>\nnext line", value.len()),
                    "{text:?}"
                );
            }
        }
        // A quoted header ends at its closing quote, not at the end of the line.
        let redaction = redact(&format!(
            "curl -H \"Authorization: Bearer {}\" https://example.test",
            key("", 20)
        ));
        assert_eq!(
            redaction.text,
            "curl -H \"<redacted:Authorization:27 chars>\" https://example.test"
        );
    }

    #[test]
    fn a_bearer_token_is_masked_through_its_punctuation() {
        let delimiters = ['"', '\'', ',', '\\', '`', ')', ']', '}', '>'];
        for mark in punctuation().filter(|mark| !delimiters.contains(mark)) {
            let token = format!("{}{mark}{}", key("", 20), "B".repeat(6));
            let redaction = redact(&format!("token Bearer {token} end"));
            assert!(
                !redaction.text.contains("BBBBBB"),
                "{mark:?}: {}",
                redaction.text
            );
        }
    }

    #[test]
    fn escaped_json_values_are_masked_whole() {
        // Finding 20: an escaped quote or backslash inside the value does not end it.
        let text = "{\"key\": \"AAAAAAAA\\\"BBBBBBBB\\\\CCCCCCCC\\u0041DDDDDDDD\"}";
        let redaction = redact(text);
        assert_eq!(redaction.masked, 1, "{}", redaction.text);
        for part in ["BBBBBBBB", "CCCCCCCC", "DDDDDDDD"] {
            assert!(!redaction.text.contains(part), "{}", redaction.text);
        }
    }

    #[test]
    fn only_a_complete_marker_counts_as_masked() {
        // Finding 21: a value that merely starts with `<` is masked.
        for value in [
            format!("<{}", key("", 20)),
            "<redacted:key:5 chars".to_owned(),
            format!("<redacted:key:5 chars> {}", key("", 20)),
        ] {
            let text = format!("{{\"key\": \"{value}\"}}");
            let redaction = redact(&text);
            assert_eq!(redaction.masked, 1, "{text} -> {}", redaction.text);
            assert!(!redaction.text.contains("AAAA"), "{}", redaction.text);
        }
        let marked = "{\"key\": \"<redacted:key:20 chars>\"}";
        assert_eq!(redact(marked).masked, 0);
    }

    #[test]
    fn url_and_form_credentials_are_masked() {
        // Finding 22: query, fragment and form parameters, encoded values included.
        let text = "https://example.test/cb?code=XYZ123&state=keep#access_token=ABCDEF%2Fghij\n\
             grant_type=refresh_token&refresh_token=RRRRRRRR&client_id=keep2\n\
             https://example.test/?api_key=KKKK&apikey=LLLL;token=TTTT";
        let redaction = redact(text);
        for secret in [
            "XYZ123", "ABCDEF", "ghij", "RRRRRRRR", "KKKK", "LLLL", "TTTT",
        ] {
            assert!(!redaction.text.contains(secret), "{}", redaction.text);
        }
        for kept in ["state=keep", "client_id=keep2", "grant_type=refresh_token"] {
            assert!(redaction.text.contains(kept), "{}", redaction.text);
        }
        assert!(
            redaction
                .text
                .contains("access_token=<redacted:access_token:13 chars>")
        );
    }

    #[test]
    fn a_line_break_after_the_scheme_does_not_hide_the_token() {
        let token = key("", 18);
        let redaction = redact(&format!("Bearer\n{token}\nnext"));
        assert_eq!(redaction.text, "<redacted:Bearer:18 chars>\n\nnext");
        let redaction = redact(&format!("Authorization:\n  {token}\nnext"));
        assert_eq!(redaction.text, "<redacted:Authorization:18 chars>\n\nnext");
    }

    #[test]
    fn registered_values_are_masked_anywhere() {
        // Finding 46: a credential with no recognizable shape is masked because it is
        // registered, bare and inside other text.
        let secrets = SecretSet::new();
        let secret = format!("{}9z", "opaque.jwt.".repeat(3));
        secrets.register(&secret);
        // Too short to register: ordinary words must survive.
        secrets.register("abc");
        assert!(format!("{secrets:?}").contains("1 value"));
        assert!(!format!("{secrets:?}").contains("opaque"));

        let text = format!("env: X={secret}\nglued{secret}glued");
        let redaction = redact_with(&text, &secrets);
        assert_eq!(redaction.masked, 2, "{}", redaction.text);
        assert!(!redaction.text.contains(&secret));
        assert!(
            redaction
                .text
                .contains(&format!("<redacted:secret:{} chars>", secret.len()))
        );
        assert!(secrets.contains_secret(&text));
        assert!(!secrets.contains_secret(&redaction.text));
        // `redact` alone knows no registered value.
        assert_eq!(redact(&text).masked, 0);
    }

    #[test]
    fn mask_replaces_registered_values_and_no_shape() {
        let secrets = SecretSet::new();
        let secret = format!("{}9z", "opaque.jwt.".repeat(3));
        secrets.register(&secret);
        let shaped = format!(
            "{{\"token\":\"plain-value\"}} Bearer abcdefghij {}",
            key("sk-", 24)
        );
        let text = format!("{shaped} {secret}");
        let masked = secrets.mask(&text);
        assert_eq!(masked.masked, 1);
        assert_eq!(
            masked.text,
            format!("{shaped} <redacted:secret:{} chars>", secret.len())
        );
        assert_eq!(secrets.mask(&shaped).text, shaped);
    }

    #[test]
    fn a_held_suffix_is_the_longest_possible_secret_start() {
        let secrets = SecretSet::new();
        secrets.register("abcdefgh-secret");
        assert_eq!(secrets.held_suffix_len("text ending in abc"), 3);
        assert_eq!(secrets.held_suffix_len("abcdefgh-secre"), 14);
        assert_eq!(secrets.held_suffix_len("no start here"), 0);
        // The whole secret is not a proper prefix: nothing is held for it.
        assert_eq!(secrets.held_suffix_len("abcdefgh-secret"), 0);
        assert_eq!(SecretSet::new().held_suffix_len("abc"), 0);
    }

    #[test]
    fn a_clone_of_the_set_sees_every_registration() {
        let secrets = SecretSet::new();
        let counter = MaskCounter::with_secrets(secrets.clone());
        secrets.register(&key("late-", 10));
        assert!(counter.secrets().contains_secret(&key("late-", 10)));
        assert!(MaskCounter::new().secrets().is_empty());
    }

    #[tokio::test]
    async fn the_decorator_masks_the_result_and_counts_it() {
        use p1_testkit::FakeTool;

        let secret = key("sk-", 26);
        let inner: Arc<dyn Tool> = Arc::new(
            FakeTool::new("shell").returning(ToolOutcome::ok(format!("export KEY={secret}\n"))),
        );
        let counter = Arc::new(MaskCounter::new());
        let tool = redacted(inner, &counter);

        assert_eq!(tool.declaration().name, "shell");
        assert_eq!(tool.identity().implementation, "fake-shell");

        let call = p1_testkit::json_call("c1", "shell", "{}");
        let context = ToolContext {
            cancel: p1_contracts::CancellationToken::new(),
        };
        let outcome = tool.execute(&call, context).await;

        assert!(!outcome.content.contains(&secret));
        assert!(outcome.content.contains("<redacted:sk-:26 chars>"));
        assert_eq!(counter.take(), 1);
        assert_eq!(counter.take(), 0);
    }

    #[tokio::test]
    async fn the_decorator_leaves_a_clean_result_untouched() {
        use p1_testkit::FakeTool;

        let inner: Arc<dyn Tool> =
            Arc::new(FakeTool::new("read").returning(ToolOutcome::ok("     1\tfn main() {}\n")));
        let counter = Arc::new(MaskCounter::new());
        let tool = redacted(inner, &counter);
        let call = p1_testkit::json_call("c1", "read", "{}");
        let context = ToolContext {
            cancel: p1_contracts::CancellationToken::new(),
        };

        let outcome = tool.execute(&call, context).await;

        assert_eq!(outcome.content, "     1\tfn main() {}\n");
        assert_eq!(counter.take(), 0);
    }

    use p1_contracts::{DeclarationKind, ToolStatus};

    /// A native tool the masking tests control completely: its declaration and both
    /// descriptions are whatever the test builds, so masking is observed field by field.
    struct StubTool {
        declaration: ToolDeclaration,
        identity: ToolIdentity,
        call: CallDescription,
        result: ResultDescription,
    }

    impl StubTool {
        fn new(description: &str) -> Self {
            Self {
                declaration: ToolDeclaration {
                    name: "stub".to_owned(),
                    description: description.to_owned(),
                    kind: DeclarationKind::Freeform { grammar: None },
                },
                identity: ToolIdentity {
                    implementation: "stub".to_owned(),
                    variant: "test".to_owned(),
                },
                call: CallDescription {
                    verb: "call",
                    target: None,
                    edit: None,
                    destructive: false,
                },
                result: ResultDescription {
                    summary: String::new(),
                    detail: None,
                },
            }
        }
    }

    impl Tool for StubTool {
        fn declaration(&self) -> &ToolDeclaration {
            &self.declaration
        }

        fn identity(&self) -> &ToolIdentity {
            &self.identity
        }

        fn effect(&self, _call: &ToolCall) -> Effect {
            Effect::ReadOnly
        }

        fn describe(&self, _call: &ToolCall) -> CallDescription {
            self.call.clone()
        }

        fn describe_result(&self, _call: &ToolCall, _result: &ToolResultItem) -> ResultDescription {
            self.result.clone()
        }

        fn execute<'a>(
            &'a self,
            _call: &'a ToolCall,
            _context: ToolContext,
        ) -> BoxFuture<'a, ToolOutcome> {
            Box::pin(async move { ToolOutcome::ok("unused") })
        }
    }

    fn stub_call() -> ToolCall {
        p1_testkit::json_call("c1", "stub", "{}")
    }

    fn stub_result_item() -> ToolResultItem {
        ToolResultItem {
            call_id: "c1".to_owned(),
            name: "stub".to_owned(),
            status: ToolStatus::Ok,
            content: String::new(),
        }
    }

    /// Mask one result description through the decorator and return it with the count.
    fn masked_result(summary: &str, detail: Option<ResultDetail>) -> (ResultDescription, usize) {
        let counter = Arc::new(MaskCounter::new());
        let mut stub = StubTool::new("stub");
        stub.result = ResultDescription {
            summary: summary.to_owned(),
            detail,
        };
        let tool = RedactingTool::new(Arc::new(stub), counter.clone());
        let masked = tool.describe_result(&stub_call(), &stub_result_item());
        (masked, counter.take())
    }

    #[test]
    fn the_declaration_description_is_masked_at_construction() {
        let secret = key("sk-", 24);
        let counter = Arc::new(MaskCounter::new());
        let tool = RedactingTool::new(
            Arc::new(StubTool::new(&format!("reads {secret} for you"))),
            counter.clone(),
        );

        assert_eq!(tool.declaration().name, "stub");
        assert!(!tool.declaration().description.contains(&secret));
        assert_eq!(
            tool.declaration().description,
            "reads <redacted:sk-:24 chars> for you"
        );
        // Construction counts what it masked.
        assert_eq!(counter.take(), 1);
    }

    #[test]
    fn an_already_masked_declaration_is_left_as_it_is() {
        let counter = Arc::new(MaskCounter::new());
        let tool = RedactingTool::new(
            Arc::new(StubTool::new("reads <redacted:sk-:24 chars>")),
            counter.clone(),
        );

        assert_eq!(
            tool.declaration().description,
            "reads <redacted:sk-:24 chars>"
        );
        assert_eq!(counter.take(), 0);
    }

    #[test]
    fn a_call_description_masks_the_target_and_the_edit_preview() {
        let target = key("sk-ant-", 24);
        let old = key("sk-proj-", 22);
        let new = key("sk-", 26);
        let counter = Arc::new(MaskCounter::new());
        let mut stub = StubTool::new("stub");
        stub.call = CallDescription {
            verb: "edit",
            target: Some(format!("edit {target}")),
            edit: Some(EditPreview {
                path: format!("/tmp/{target}"),
                old: old.clone(),
                new: new.clone(),
            }),
            destructive: false,
        };
        let tool = RedactingTool::new(Arc::new(stub), counter.clone());

        let described = tool.describe(&stub_call());

        assert_eq!(described.verb, "edit");
        assert_eq!(
            described.target.as_deref(),
            Some("edit <redacted:sk-ant-:24 chars>")
        );
        let edit = described.edit.expect("the edit preview");
        assert_eq!(edit.path, "/tmp/<redacted:sk-ant-:24 chars>");
        assert_eq!(edit.old, "<redacted:sk-proj-:22 chars>");
        assert_eq!(edit.new, "<redacted:sk-:26 chars>");
        // The target and the path each carry the same key, and the two edit sides one each.
        assert_eq!(counter.take(), 4);
    }

    #[test]
    fn a_result_description_masks_the_summary_and_every_detail_field() {
        let summary = key("sk-", 24);
        let (masked, count) = masked_result(&summary, None);
        assert_eq!(masked.summary, "<redacted:sk-:24 chars>");
        assert!(masked.detail.is_none());
        assert_eq!(count, 1);

        let diff = key("sk-ant-", 24);
        let (masked, count) = masked_result(
            "",
            Some(ResultDetail::Diff {
                path: format!("/tmp/{diff}"),
                before: diff.clone(),
                after: "clean".to_owned(),
            }),
        );
        assert_eq!(
            masked.detail,
            Some(ResultDetail::Diff {
                path: "/tmp/<redacted:sk-ant-:24 chars>".to_owned(),
                before: "<redacted:sk-ant-:24 chars>".to_owned(),
                after: "clean".to_owned(),
            })
        );
        assert_eq!(count, 2);

        let tail = key("sk-proj-", 22);
        let (masked, count) = masked_result(
            "",
            Some(ResultDetail::Command {
                exit_code: Some(1),
                elapsed_ms: Some(5),
                tail: vec!["ok".to_owned(), tail],
            }),
        );
        match masked.detail {
            Some(ResultDetail::Command {
                exit_code,
                elapsed_ms,
                tail,
            }) => {
                assert_eq!(exit_code, Some(1));
                assert_eq!(elapsed_ms, Some(5));
                assert_eq!(
                    tail,
                    vec!["ok".to_owned(), "<redacted:sk-proj-:22 chars>".to_owned()]
                );
            }
            other => panic!("expected a command detail, got {other:?}"),
        }
        assert_eq!(count, 1);

        let file = key("sk-or-", 24);
        let (masked, count) = masked_result(
            "",
            Some(ResultDetail::Matches {
                count: 1,
                files: vec![format!("/tmp/{file}")],
            }),
        );
        assert_eq!(
            masked.detail,
            Some(ResultDetail::Matches {
                count: 1,
                files: vec!["/tmp/<redacted:sk-or-:24 chars>".to_owned()],
            })
        );
        assert_eq!(count, 1);

        let path = key("sk-svcacct-", 24);
        let (masked, count) = masked_result(
            "",
            Some(ResultDetail::Files {
                paths: vec![path.clone()],
            }),
        );
        assert_eq!(
            masked.detail,
            Some(ResultDetail::Files {
                paths: vec!["<redacted:sk-svcacct-:24 chars>".to_owned()],
            })
        );
        assert_eq!(count, 1);

        let text = key("sk-admin-", 24);
        let (masked, count) = masked_result("", Some(ResultDetail::Text(format!("see {text}"))));
        assert_eq!(
            masked.detail,
            Some(ResultDetail::Text(
                "see <redacted:sk-admin-:24 chars>".to_owned()
            ))
        );
        assert_eq!(count, 1);
    }

    #[tokio::test]
    async fn the_decorator_masks_registered_values_through_its_counter() {
        use p1_testkit::FakeTool;

        let secret = key("opaque-", 16);
        let secrets = SecretSet::new();
        secrets.register(&secret);
        let inner: Arc<dyn Tool> = Arc::new(
            FakeTool::new("shell").returning(ToolOutcome::ok(format!("TOKEN={secret}\n"))),
        );
        let counter = Arc::new(MaskCounter::with_secrets(secrets));
        let tool = redacted(inner, &counter);
        let call = p1_testkit::json_call("c1", "shell", "{}");
        let context = ToolContext {
            cancel: p1_contracts::CancellationToken::new(),
        };

        let outcome = tool.execute(&call, context).await;

        assert!(!outcome.content.contains(&secret), "{}", outcome.content);
        assert_eq!(counter.take(), 1);
    }

    #[test]
    fn a_command_tail_is_masked_as_one_text() {
        // Finding 23: `Bearer` ends one line, its token starts the next.
        let token = key("", 18);
        let (masked, count) = masked_result(
            "",
            Some(ResultDetail::Command {
                exit_code: Some(0),
                elapsed_ms: None,
                tail: vec![
                    "curl -H Bearer".to_owned(),
                    token.clone(),
                    "done".to_owned(),
                ],
            }),
        );
        match masked.detail {
            Some(ResultDetail::Command { tail, .. }) => {
                assert_eq!(tail.len(), 3, "{tail:?}");
                assert!(tail.iter().all(|line| !line.contains(&token)), "{tail:?}");
                assert_eq!(tail[2], "done");
            }
            other => panic!("expected a command detail, got {other:?}"),
        }
        assert_eq!(count, 1);
    }

    fn declaration(name: &str, kind: DeclarationKind) -> ToolDeclaration {
        ToolDeclaration {
            name: name.to_owned(),
            description: "d".to_owned(),
            kind,
        }
    }

    fn identity(variant: &str) -> ToolIdentity {
        ToolIdentity {
            implementation: "stub".to_owned(),
            variant: variant.to_owned(),
        }
    }

    #[test]
    fn a_declaration_carrying_a_credential_is_refused() {
        // Finding 47: name, schema, grammar and identity are checked, never masked.
        let secret = key("sk-", 24);
        let secrets = SecretSet::new();
        let registered = key("opaque-", 12);
        secrets.register(&registered);
        let schema = |value: &str| DeclarationKind::Function {
            input_schema: p1_contracts::serde_json::json!({
                "type": "object",
                "properties": { "mode": { "type": "string", "enum": ["a", value] } }
            }),
        };
        let clean = schema("b");
        let cases = [
            (
                declaration(&format!("t{secret}"), clean.clone()),
                identity("v"),
            ),
            (declaration("t", schema(&secret)), identity("v")),
            (declaration("t", schema(&registered)), identity("v")),
            (
                declaration(
                    "t",
                    DeclarationKind::Freeform {
                        grammar: Some(p1_contracts::tool::Grammar {
                            syntax: "lark".to_owned(),
                            definition: format!("start: \"Bearer {}\"", key("", 20)),
                        }),
                    },
                ),
                identity("v"),
            ),
            (declaration("t", clean.clone()), identity(&registered)),
        ];
        for (declared, identified) in &cases {
            let error = check_declaration(declared, identified, &secrets).unwrap_err();
            assert!(
                !error.contains(&secret) && !error.contains(&registered),
                "{error}"
            );
        }
    }

    #[test]
    fn an_ordinary_schema_is_not_refused() {
        // A property named `token` and prose about bearer tokens are schema text.
        let kind = DeclarationKind::Function {
            input_schema: p1_contracts::serde_json::json!({
                "type": "object",
                "properties": {
                    "token": { "type": "string", "description": "the Bearer token to send" },
                    "key": { "type": "string", "default": "Enter" }
                }
            }),
        };
        assert_eq!(
            check_declaration(
                &declaration("fetch", kind),
                &identity("v"),
                &SecretSet::new()
            ),
            Ok(())
        );
    }

    #[test]
    fn clean_declarations_and_descriptions_are_left_alone() {
        let counter = Arc::new(MaskCounter::new());
        let mut stub = StubTool::new("plain description");
        stub.call = CallDescription {
            verb: "read",
            target: Some("src/lib.rs".to_owned()),
            edit: None,
            destructive: false,
        };
        stub.result = ResultDescription {
            summary: "1 file".to_owned(),
            detail: Some(ResultDetail::Files {
                paths: vec!["src/lib.rs".to_owned()],
            }),
        };
        let tool = RedactingTool::new(Arc::new(stub), counter.clone());

        assert_eq!(tool.declaration().description, "plain description");
        assert_eq!(
            tool.describe(&stub_call()).target.as_deref(),
            Some("src/lib.rs")
        );
        assert_eq!(
            tool.describe_result(&stub_call(), &stub_result_item())
                .summary,
            "1 file"
        );
        assert_eq!(counter.take(), 0);
    }

    #[test]
    fn the_decorator_forwards_effect_and_identity() {
        let counter = Arc::new(MaskCounter::new());
        let tool = RedactingTool::new(Arc::new(StubTool::new("stub")), counter);

        assert_eq!(tool.effect(&stub_call()), Effect::ReadOnly);
        assert_eq!(tool.identity().implementation, "stub");
        assert_eq!(tool.identity().variant, "test");
    }
}
