//! Scratch locations and fake credential files for the chain tests.
//!
//! Every path the chain looks at is built from a scratch home or an explicit
//! variable table, so no test in this crate can reach the real home or the real
//! environment — `tests/no_real_environment.rs` asserts that for the whole suite.

#![allow(dead_code)]

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use p1_auth::Locations;

/// A scratch home. Nothing outside it exists for a test that uses it.
pub struct Scratch {
    root: tempfile::TempDir,
}

impl Scratch {
    pub fn new() -> Self {
        Self {
            root: tempfile::tempdir().unwrap(),
        }
    }

    pub fn home(&self) -> PathBuf {
        self.root.path().to_path_buf()
    }

    pub fn path(&self, relative: &str) -> PathBuf {
        self.root.path().join(relative)
    }

    /// The locations of this scratch home, with nothing in the environment.
    pub fn locations(&self) -> Locations {
        Locations::none().with_home(Some(self.home()))
    }

    /// Write a file 0600, creating its parents 0700 (the modes p1 writes).
    pub fn write(&self, relative: &str, contents: &str) -> PathBuf {
        let path = self.path(relative);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
            set_mode(parent, 0o700);
        }
        write_mode(&path, contents, 0o600);
        path
    }

    pub fn read(&self, relative: &str) -> String {
        std::fs::read_to_string(self.path(relative)).unwrap()
    }

    /// Change the mode of a path this scratch already holds.
    pub fn set_mode(&self, relative: &str, mode: u32) {
        set_mode(&self.path(relative), mode);
    }

    /// Everything from the first occurrence of `marker` on, for a byte-compare of
    /// the part a refresh must not touch.
    pub fn tail_from(&self, relative: &str, marker: &str) -> String {
        let text = self.read(relative);
        let at = text
            .find(marker)
            .unwrap_or_else(|| panic!("{relative} holds no {marker}"));
        text[at..].to_string()
    }
}

pub fn set_mode(path: &Path, mode: u32) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
}

pub fn write_mode(path: &Path, contents: &str, mode: u32) {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true).mode(mode);
    let mut file = options.open(path).unwrap();
    file.write_all(contents.as_bytes()).unwrap();
}

/// An environment a test controls: `set` makes a variable appear for the next
/// lookup, `clear` takes it away again. Nothing here touches the process one.
#[derive(Clone)]
pub struct FakeEnv {
    vars: Arc<Mutex<HashMap<String, String>>>,
}

impl FakeEnv {
    pub fn new() -> Self {
        Self {
            vars: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub fn set(&self, name: &str, value: &str) {
        self.vars
            .lock()
            .unwrap()
            .insert(name.to_string(), value.to_string());
    }

    pub fn clear(&self, name: &str) {
        self.vars.lock().unwrap().remove(name);
    }

    /// The scratch home with this variable table as the WHOLE environment.
    pub fn locations(&self, scratch: &Scratch) -> Locations {
        let vars = self.vars.clone();
        scratch
            .locations()
            .with_env_lookup(move |name| vars.lock().unwrap().get(name).cloned())
    }
}

/// One borrowed login's entry, as the two stores write it.
pub fn login(store: &str, key: &str, value: &str) -> String {
    let kind = if store == "opencode" {
        "api"
    } else {
        "api_key"
    };
    serde_json::json!({ key: { "type": kind, "key": value } }).to_string()
}

/// A `claude-code-oauth` login file, fresh unless `expires_at` says otherwise.
pub fn claude_login(access: &str, refresh: &str, expires_at: u64) -> String {
    serde_json::to_string_pretty(&serde_json::json!({
        "claudeAiOauth": {
            "accessToken": access,
            "refreshToken": refresh,
            "expiresAt": expires_at,
        }
    }))
    .unwrap()
}

/// A far-future millisecond timestamp: a token that needs no refresh.
pub const NEVER_EXPIRES_MS: u64 = 4_102_444_800_000;
/// A millisecond timestamp in the past: a token that must be refreshed.
pub const LONG_EXPIRED_MS: u64 = 1;

/// A JSON document in exactly the layout p1 writes: pretty, and newline-terminated.
/// A file written this way is byte-identical to its own rewrite except where a
/// refresh changed a value, which is what the write-back tests compare.
pub fn document(value: &serde_json::Value) -> String {
    let mut text = serde_json::to_string_pretty(value).unwrap();
    text.push('\n');
    text
}
