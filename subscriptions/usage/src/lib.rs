//! The subscription connectors' usage probes (ADR-0214): the usage endpoints of the Claude,
//! Codex, OpenCode Go, Kimi and GLM plans, as the two pure functions of the `usage-probe` world.
//!
//! Each [`UsageProbe`] says where its plan's usage endpoint is and reads that endpoint's answer,
//! and nothing else. It never sees a credential and holds no network, file or clock: p1's usage
//! ledger (`p1-usage`) builds the request from [`UsageProbe::describe`], attaches the credential
//! natively, sends it over HTTPS to an origin the module's manifest declares and hands
//! [`UsageProbe::decode`] the body of a 2xx response. The ledger checks every string of the
//! answer again before it shows it (a label, a reset instant, a currency), so the decoders pass
//! the vendor's text through and only shape it.
//!
//! Answers are the JSON texts `docs/design/modules/usage-probe.md` defines.

mod decode;
mod timestamp;

use serde_json::{Value, json};

/// One plan's usage endpoint and its response decoding: the `usage-probe` world's exports.
pub trait UsageProbe {
    /// The endpoint: its url, the credential method it takes and the headers it sends.
    fn describe(&self) -> String;

    /// What a 2xx response `body` says, or why it says nothing readable.
    fn decode(&self, body: &[u8]) -> Result<String, String> {
        let data: Value = serde_json::from_slice(body)
            .map_err(|_| "the usage response body is not JSON".to_string())?;
        self.read(&data)
            .map(|usage| usage.to_string())
            .ok_or_else(|| "the body is not this plan's usage response".to_string())
    }

    /// The usage `data` holds, as the answer's JSON, or `None` when it is not this plan's
    /// response.
    fn read(&self, data: &Value) -> Option<Value>;
}

/// The Claude subscription's usage (`claude-code-oauth`).
pub struct Claude;
/// The ChatGPT/Codex subscription's usage (`codex-oauth`).
pub struct Codex;
/// The OpenCode Go plan's usage (an API key).
pub struct OpencodeGo;
/// The Kimi coding plan's usage (an API key).
pub struct Kimi;
/// The Z.ai GLM coding plan's usage (an API key).
pub struct Glm;

/// The OpenCode Go usage endpoint asks for a session id; the ledger keeps no session, so one
/// short constant identifies the caller.
const SESSION_ID: &str = "p1-usage";

/// A description: `url` and `method`, the static `headers`, the header the credential's
/// account id travels in, whether p1's own `user-agent` is sent, and whether a credential the
/// method borrows from another tool's login may be sent here without the account's recorded
/// approval.
fn described(
    url: &str,
    method: &str,
    headers: &[(&str, &str)],
    account_id_header: Option<&str>,
    user_agent: bool,
    borrowed: bool,
) -> String {
    json!({
        "url": url,
        "method": method,
        "headers": headers,
        "account_id_header": account_id_header,
        "user_agent": user_agent,
        "borrowed": borrowed,
    })
    .to_string()
}

impl UsageProbe for Claude {
    fn describe(&self) -> String {
        described(
            "https://api.anthropic.com/api/oauth/usage",
            "claude-code-oauth",
            &[("anthropic-beta", "oauth-2025-04-20")],
            None,
            false,
            true,
        )
    }

    fn read(&self, data: &Value) -> Option<Value> {
        decode::claude(data)
    }
}

impl UsageProbe for Codex {
    fn describe(&self) -> String {
        described(
            "https://chatgpt.com/backend-api/wham/usage",
            "codex-oauth",
            &[],
            Some("ChatGPT-Account-Id"),
            false,
            true,
        )
    }

    fn read(&self, data: &Value) -> Option<Value> {
        decode::codex(data)
    }
}

impl UsageProbe for OpencodeGo {
    fn describe(&self) -> String {
        described(
            "https://opencode.ai/zen/go/v1/usage",
            "api-key",
            &[("x-opencode-session", SESSION_ID)],
            None,
            true,
            true,
        )
    }

    fn read(&self, data: &Value) -> Option<Value> {
        decode::opencode_go(data)
    }
}

impl UsageProbe for Kimi {
    /// The overseas chat host (#645 R3): `api.kimi.com` is the China host of the same plan and
    /// answers with the same body, but the shipped account approves `.ai` only. No Kimi login
    /// another tool keeps is trusted (the opencode `kimi-for-coding` entry was stale,
    /// ADR-0061), so a borrowed credential never authorizes this probe.
    fn describe(&self) -> String {
        described(
            "https://api.kimi.ai/coding/v1/usages",
            "api-key",
            &[],
            None,
            false,
            false,
        )
    }

    fn read(&self, data: &Value) -> Option<Value> {
        decode::kimi(data)
    }
}

impl UsageProbe for Glm {
    fn describe(&self) -> String {
        described(
            "https://api.z.ai/api/monitor/usage/quota/limit",
            "api-key",
            &[],
            None,
            false,
            true,
        )
    }

    fn read(&self, data: &Value) -> Option<Value> {
        decode::glm(data)
    }
}
