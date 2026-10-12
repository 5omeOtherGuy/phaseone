//! The subscription connectors' login formats (ADR-0212): the logins of the Claude Code, Codex,
//! OpenCode and Pi CLIs, as the five pure functions of the `credential-method` world.
//!
//! Each [`Login`] reads its own CLI's login file and builds and reads its own token refresh
//! exchange, and nothing else. It holds no file, no lock, no clock and no network: p1's
//! credential broker (`p1-auth`) reads and writes the file through its pinned directory under
//! the login's lock, sends the request over HTTPS to an origin the module's manifest declares,
//! and passes the time in. The credential components (`components/claude-code-oauth`,
//! `codex-oauth`, `opencode`, `pi`) export these functions; the suites in `../tests` drive them
//! natively through `p1-auth`'s chain.
//!
//! Answers are the JSON texts `docs/design/modules/credential.md` defines. No reason names a
//! value of a file or a response.

mod claude_code;
mod codex;
mod keys;

pub use claude_code::ClaudeCode;
pub use codex::Codex;
pub use keys::{Opencode, Pi};

/// One login's format and refresh exchange: the `credential-method` world's exports.
pub trait Login {
    /// What the module provides: its method, its login file, or both.
    fn describe(&self) -> String;

    /// The credential `file` holds, or why it holds none usable. `key` names the entry of a
    /// keyed login.
    fn read_login(&self, file: &[u8], key: Option<&str>) -> Result<String, String>;

    /// The refresh request that spends `refresh_token`; `file` is the borrowed login it was
    /// read from, or none for an entry of p1's own store.
    fn refresh_request(
        &self,
        _refresh_token: &str,
        _file: Option<&[u8]>,
    ) -> Result<String, String> {
        Err("this login is never refreshed".into())
    }

    /// What a refresh response `body` grants, the expiry counted from `sent_at_ms`.
    fn refresh_response(&self, _body: &[u8], _sent_at_ms: u64) -> String {
        refused("this login is never refreshed")
    }

    /// `file` with the refresh response `body` written into it, every other field kept.
    fn merge_login(
        &self,
        _file: &[u8],
        _body: &[u8],
        _sent_at_ms: u64,
        _now_ms: u64,
        _keep_access: bool,
    ) -> Result<Vec<u8>, String> {
        Err("this login is never refreshed".into())
    }
}

/// A refresh response no field of which can be used, for `reason`.
fn refused(reason: &str) -> String {
    serde_json::json!({
        "access": {"err": reason},
        "expires_at_ms": {"err": reason},
    })
    .to_string()
}

/// A non-empty string after trimming; whitespace alone is no token.
fn non_empty(value: Option<&serde_json::Value>) -> Option<String> {
    value
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(str::to_string)
}

/// How long before its recorded expiry an OAuth access token counts as due for refresh, as
/// p1's broker counts it.
const REFRESH_MARGIN_MS: u64 = 300_000;
