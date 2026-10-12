//! the Claude Code login (claude-code-oauth) as a credential component (ADR-0212): the `credential-method` world's exports over
//! `p1_subscription_logins::ClaudeCode`. Pure functions over its own login file and refresh
//! exchange; the component imports nothing.
#![forbid(unsafe_code)]

use p1_module_sdk::credential_method::Guest;
use p1_subscription_logins::{ClaudeCode, Login};

struct Component;

impl Guest for Component {
    fn describe() -> String {
        ClaudeCode.describe()
    }

    fn read_login(file: Vec<u8>, key: Option<String>) -> Result<String, String> {
        ClaudeCode.read_login(&file, key.as_deref())
    }

    fn refresh_request(refresh_token: String, file: Option<Vec<u8>>) -> Result<String, String> {
        ClaudeCode.refresh_request(&refresh_token, file.as_deref())
    }

    fn refresh_response(body: Vec<u8>, sent_at_ms: u64) -> String {
        ClaudeCode.refresh_response(&body, sent_at_ms)
    }

    fn merge_login(
        file: Vec<u8>,
        body: Vec<u8>,
        sent_at_ms: u64,
        now_ms: u64,
        keep_access: bool,
    ) -> Result<Vec<u8>, String> {
        ClaudeCode.merge_login(&file, &body, sent_at_ms, now_ms, keep_access)
    }
}

p1_module_sdk::credential_method::export!(Component with_types_in p1_module_sdk::credential_method);
