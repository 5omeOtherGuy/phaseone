//! The keyed logins (`opencode`, `pi`): one API key another CLI keeps in its own login file,
//! borrowed read-only by an API-key route (`borrow = ["opencode:go"]`, ADR-0040).
//!
//! OpenCode writes `{"<provider>":{"type":"api","key":…}}` and Pi `{"<provider>":{"type":
//! "api_key","key":…}}`. Nothing is refreshed and nothing written: the file is re-read on every
//! access, so a key the CLI rotated is picked up without a restart. Pi permits command-backed
//! keys (`"!command"`); executing configuration is outside a credential source, so such an entry
//! is refused rather than run.

use serde_json::{Value, json};

use crate::Login;

/// The OpenCode CLI's login file.
pub struct Opencode;

/// The Pi CLI's login file.
pub struct Pi;

impl Login for Opencode {
    fn describe(&self) -> String {
        json!({
            "login": {
                "title": "opencode login",
                "guidance": "log in with opencode",
                "dir_env": "XDG_DATA_HOME",
                "dir_env_subdir": "opencode",
                "home_dir": ".local/share/opencode",
                "file": "auth.json",
                "keyed": true,
            },
        })
        .to_string()
    }

    fn read_login(&self, file: &[u8], key: Option<&str>) -> Result<String, String> {
        read_key(file, key, "opencode", "api")
    }
}

impl Login for Pi {
    fn describe(&self) -> String {
        json!({
            "login": {
                "title": "pi login",
                "guidance": "log in with pi",
                "dir_env": "PI_CODING_AGENT_DIR",
                "home_dir": ".pi/agent",
                "file": "auth.json",
                "keyed": true,
            },
        })
        .to_string()
    }

    fn read_login(&self, file: &[u8], key: Option<&str>) -> Result<String, String> {
        read_key(file, key, "pi", "api_key")
    }
}

/// The key the entry `key` holds, `{"access": null}` when the file has no such entry, and the
/// reason when the entry exists but cannot be used.
fn read_key(
    file: &[u8],
    key: Option<&str>,
    store: &str,
    entry_type: &str,
) -> Result<String, String> {
    let key = key.ok_or_else(|| format!("the {store} login is read by key"))?;
    let data: Value = serde_json::from_slice(file)
        .map_err(|_| format!("the {store} credential file is not valid JSON"))?;
    let Some(entry) = data.get(key) else {
        return Ok(json!({ "access": null }).to_string());
    };
    if entry.get("type").and_then(Value::as_str) != Some(entry_type) {
        return Err(format!("the {store} login holds no API key for \"{key}\""));
    }
    let value = entry
        .get("key")
        .and_then(Value::as_str)
        .ok_or_else(|| format!("the {store} login entry has no key"))?;
    if value.starts_with('!') {
        return Err(
            "command-backed keys are unsupported; set the documented key environment variable"
                .into(),
        );
    }
    Ok(json!({ "access": value }).to_string())
}
