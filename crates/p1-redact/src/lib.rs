//! Mask credential-shaped strings in tool output.
//!
//! Issue #142: a tool result (a `cat` of an auth file, `env`, a provider key
//! printed by a command) entered the model's history, the journal and every later
//! request verbatim. This crate owns the ONE matcher for credential shapes and the
//! [`RedactingTool`] decorator the composition root wraps every assembled tool in,
//! so the text a tool returns is masked BEFORE `p1-core` turns it into a
//! [`ToolResultItem`] — history, journal and request then only ever see the masked
//! form, because they are all built from the same `ToolOutcome`.
//!
//! # Patterns (fleet PR #51, dash-safe)
//!
//! - `sk-` followed by 20+ alphanumerics, or by one of the `ant`/`proj`/`or`/
//!   `svcacct`/`admin` modifiers and a dash and 20+ `[A-Za-z0-9_-]`;
//! - the token after `Authorization:` or `Bearer `, when it is 16+ token
//!   characters (auth-store headers);
//! - a JSON string value of the keys `key`, `access`, `refresh`, `api_key` or
//!   `token` when it is 16+ characters (auth-store shapes).
//!
//! A matched value is replaced with `<redacted:family:N chars>`, keeping only the
//! family prefix (`sk-`, `sk-ant-`, `Bearer`, `Authorization`, the JSON key) and
//! never a character of the secret itself.
//!
//! A long base64 blob is NOT a credential: it is masked only when it matches one of
//! the patterns above. A provider's thinking signature is opaque continuation data
//! (`ReplayData`), never tool-result text, so it is never passed to [`redact`] and
//! stays byte-for-byte intact.
//!
//! Masking is idempotent: a replaced `<redacted:…>` marker cannot match any of the
//! patterns again.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};

use p1_contracts::tool::ResultDescription;
use p1_contracts::{
    BoxFuture, CallDescription, Effect, Tool, ToolCall, ToolContext, ToolDeclaration, ToolIdentity,
    ToolOutcome, ToolResultItem,
};
use regex::{Captures, Regex};

/// The canonical credential-shape pattern (fleet PR #51). The `sk-` branch keeps
/// the exact alternation the fleet lead published.
const PATTERNS: &str = concat!(
    r"(?P<sk>sk-(?:[A-Za-z0-9]{20,}|(?:ant|proj|or|svcacct|admin)-[A-Za-z0-9_-]{20,})",
    r"|\bsk-[A-Za-z0-9_-]{20,})",
    r"|(?P<bearer>(?i:Bearer)[ \t]+(?P<btoken>[A-Za-z0-9._~+/=-]{16,}))",
    r"|(?P<authz>(?i:Authorization):[ \t]*(?P<atoken>[A-Za-z0-9._~+/=-]{16,}))",
    // The opening quote anchors the key name (`"monkey"` is not `"key"`); a value that
    // starts with `<` is an existing marker, which keeps masking idempotent.
    r#"|(?:"(?P<jkey>api_key|access|refresh|token|key)"[ \t]*:[ \t]*"(?P<jtoken>[^"<][^"]{15,})")"#,
);

/// The modifier families whose `sk-<modifier>-` prefix is kept in the marker.
const MODIFIERS: [&str; 5] = ["ant", "proj", "or", "svcacct", "admin"];

fn patterns() -> &'static Regex {
    static COMPILED: OnceLock<Regex> = OnceLock::new();
    COMPILED.get_or_init(|| Regex::new(PATTERNS).expect("the credential-shape pattern compiles"))
}

/// One masked text: the result plus how many values were replaced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Redaction {
    pub text: String,
    /// Number of credential-shaped values replaced. Never the values themselves.
    pub masked: usize,
}

/// Mask every credential-shaped string in `text`. A text with no match is returned
/// unchanged (`masked == 0`).
pub fn redact(text: &str) -> Redaction {
    let mut masked = 0usize;
    let replaced = patterns().replace_all(text, |captures: &Captures| {
        masked += 1;
        marker(captures)
    });
    Redaction {
        text: replaced.into_owned(),
        masked,
    }
}

/// The `<redacted:family:N chars>` marker for one match. `N` is the length of the
/// masked value (the JSON value, the bearer token, or the whole `sk-` token); the
/// family is the only part of the match kept.
fn marker(captures: &Captures) -> String {
    if let Some(found) = captures.name("sk") {
        let token = found.as_str();
        let family = sk_family(token);
        // The length of the secret part only, like the other families' token lengths.
        return format!("<redacted:{family}:{} chars>", token.len() - family.len());
    }
    if let Some(found) = captures.name("btoken") {
        return format!("<redacted:Bearer:{} chars>", found.as_str().len());
    }
    if let Some(found) = captures.name("atoken") {
        return format!("<redacted:Authorization:{} chars>", found.as_str().len());
    }
    let key = captures.name("jkey").map_or("key", |found| found.as_str());
    let length = captures
        .name("jtoken")
        .map_or(0, |found| found.as_str().len());
    // The match spans the whole `"key": "value"` pair, so the key is written back.
    format!("\"{key}\": \"<redacted:{key}:{length} chars>\"")
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

/// Values masked since the last [`MaskCounter::take`]. One per agent, so the host
/// can report a per-turn count through an existing notice path without ever
/// touching a value.
#[derive(Debug, Default)]
pub struct MaskCounter {
    masked: AtomicUsize,
}

impl MaskCounter {
    pub fn new() -> Self {
        Self::default()
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

/// A [`Tool`] decorator: it forwards declaration, identity, effect and description
/// to the tool it wraps and rewrites the text of every `ToolOutcome` through
/// [`redact`] before the core can see it.
pub struct RedactingTool {
    inner: Arc<dyn Tool>,
    counter: Arc<MaskCounter>,
}

impl RedactingTool {
    pub fn new(inner: Arc<dyn Tool>, counter: Arc<MaskCounter>) -> Self {
        Self { inner, counter }
    }
}

/// Wrap one assembled tool. A non-zero mask count is added to `counter`.
pub fn redacted(tool: Arc<dyn Tool>, counter: &Arc<MaskCounter>) -> Arc<dyn Tool> {
    Arc::new(RedactingTool::new(tool, counter.clone()))
}

impl Tool for RedactingTool {
    fn declaration(&self) -> &ToolDeclaration {
        self.inner.declaration()
    }

    fn identity(&self) -> &ToolIdentity {
        self.inner.identity()
    }

    fn effect(&self, call: &ToolCall) -> Effect {
        self.inner.effect(call)
    }

    fn describe(&self, call: &ToolCall) -> CallDescription {
        self.inner.describe(call)
    }

    fn describe_result(&self, call: &ToolCall, result: &ToolResultItem) -> ResultDescription {
        self.inner.describe_result(call, result)
    }

    fn execute<'a>(
        &'a self,
        call: &'a ToolCall,
        context: ToolContext,
    ) -> BoxFuture<'a, ToolOutcome> {
        Box::pin(async move {
            let outcome = self.inner.execute(call, context).await;
            let redaction = redact(&outcome.content);
            if redaction.masked == 0 {
                return outcome;
            }
            self.counter.add(redaction.masked);
            ToolOutcome {
                status: outcome.status,
                content: redaction.text,
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
        for json_key in ["key", "access", "refresh", "api_key", "token"] {
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
        // 19 characters after `sk-` is one short of the pattern.
        let short = key("sk-", 19);
        // A plain long base64 blob (a thinking-signature shape) is not a key.
        let base64 = format!("{}{}", "QWxsQWJj".repeat(6), "ZGVm");
        let text = format!("{short}\n{base64}\nplain sentence\n{{\"token\": \"short\"}}");

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
}
