//! The Claude subscription usage endpoint as a usage-probe component (ADR-0214): the `usage-probe`
//! world's exports over `p1_subscription_usage::Claude`. Pure functions; the component imports
//! nothing and never sees a credential.
#![forbid(unsafe_code)]

use p1_module_sdk::usage_probe::Guest;
use p1_subscription_usage::{Claude, UsageProbe};

struct Component;

impl Guest for Component {
    fn describe() -> String {
        Claude.describe()
    }

    fn decode(body: Vec<u8>) -> Result<String, String> {
        Claude.decode(&body)
    }
}

p1_module_sdk::usage_probe::export!(Component with_types_in p1_module_sdk::usage_probe);
