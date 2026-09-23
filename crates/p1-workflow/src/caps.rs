//! Per-run attempt caps keyed by the resolved wire model (ADR-0053 item 4).

use std::collections::BTreeMap;
use std::sync::Mutex;

/// Counts attempts (starts and repairs) per wire model for ONE run. Keyed by the wire
/// model, not the role or profile, so no renaming in settings can multiply the scarce
/// model's budget.
pub(crate) struct CapCounter {
    caps: BTreeMap<String, u32>,
    used: Mutex<BTreeMap<String, u32>>,
}

impl CapCounter {
    /// `charged` is what a resumed run's predecessor already spent.
    pub(crate) fn new(caps: BTreeMap<String, u32>, charged: BTreeMap<String, u32>) -> Self {
        Self {
            caps,
            used: Mutex::new(charged),
        }
    }

    /// Check and spend under one lock, so two concurrent thunks can never both take the
    /// last attempt. `Err((used, limit))` when the cap is reached.
    pub(crate) fn try_spend(&self, wire_model: &str) -> Result<(), (u32, u32)> {
        let mut used = self
            .used
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let count = used.entry(wire_model.to_string()).or_insert(0);
        if let Some(limit) = self.caps.get(wire_model)
            && *count >= *limit
        {
            return Err((*count, *limit));
        }
        *count += 1;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_cap_counts_charged_attempts_and_refuses_at_the_limit() {
        let caps = BTreeMap::from([("fable".to_string(), 3)]);
        let counter = CapCounter::new(caps, BTreeMap::from([("fable".to_string(), 2)]));
        assert_eq!(counter.try_spend("fable"), Ok(()));
        assert_eq!(counter.try_spend("fable"), Err((3, 3)));
        for _ in 0..10 {
            assert_eq!(counter.try_spend("opus"), Ok(()), "uncapped");
        }
    }
}
