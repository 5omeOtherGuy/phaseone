//! The ChatGPT/Codex subscription usage endpoint as a usage-probe component (ADR-0214): the `usage-probe`
//! world's exports over `p1_connector_usage::Codex`. Pure functions; the component imports
//! nothing and never sees a credential.
#![forbid(unsafe_code)]

use p1_connector_usage::{Codex, UsageProbe};
use p1_module_sdk::usage_probe::Guest;

struct Component;

impl Guest for Component {
    fn describe() -> String {
        Codex.describe()
    }

    fn decode(body: Vec<u8>) -> Result<String, String> {
        Codex.decode(&body)
    }
}

p1_module_sdk::usage_probe::export!(Component with_types_in p1_module_sdk::usage_probe);
