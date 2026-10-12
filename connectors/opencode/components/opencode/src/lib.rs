//! The OpenCode CLI's keyed login, borrowed by API-key routes (`borrow = ["opencode:<key>"]`), as a credential
//! component (ADR-0212): the `credential-method` world's exports over
//! `p1_connector_logins::Opencode`. Pure functions over its own login file; the
//! component imports nothing.
#![forbid(unsafe_code)]

use p1_connector_logins::{Login, Opencode};
use p1_module_sdk::credential_method::Guest;

struct Component;

impl Guest for Component {
    fn describe() -> String {
        Opencode.describe()
    }

    fn read_login(file: Vec<u8>, key: Option<String>) -> Result<String, String> {
        Opencode.read_login(&file, key.as_deref())
    }

    fn refresh_request(refresh_token: String, file: Option<Vec<u8>>) -> Result<String, String> {
        Opencode.refresh_request(&refresh_token, file.as_deref())
    }

    fn refresh_response(body: Vec<u8>, sent_at_ms: u64) -> String {
        Opencode.refresh_response(&body, sent_at_ms)
    }

    fn merge_login(
        file: Vec<u8>,
        body: Vec<u8>,
        sent_at_ms: u64,
        now_ms: u64,
        keep_access: bool,
    ) -> Result<Vec<u8>, String> {
        Opencode.merge_login(&file, &body, sent_at_ms, now_ms, keep_access)
    }
}

p1_module_sdk::credential_method::export!(Component with_types_in p1_module_sdk::credential_method);
