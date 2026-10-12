//! The Kimi coding plan usage endpoint as a usage-probe component (ADR-0214): the `usage-probe`
//! world's exports over `p1_connector_usage::Kimi`. Pure functions; the component imports
//! nothing and never sees a credential.
#![forbid(unsafe_code)]

use p1_connector_usage::{Kimi, UsageProbe};
use p1_module_sdk::usage_probe::Guest;

struct Component;

impl Guest for Component {
    fn describe() -> String {
        Kimi.describe()
    }

    fn decode(body: Vec<u8>) -> Result<String, String> {
        Kimi.decode(&body)
    }
}

p1_module_sdk::usage_probe::export!(Component with_types_in p1_module_sdk::usage_probe);
