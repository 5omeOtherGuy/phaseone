//! Parent context usage and cumulative session spend, independent of wire version.

use p1_contracts::Usage;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SessionUsage {
    pub used_tokens: u64,
    pub window_tokens: u64,
    pub cost_micro_usd: Option<u64>,
}

pub(crate) struct UsageState {
    pub(crate) window_tokens: Option<u64>,
    cost_micro_usd: Option<u64>,
}

impl Default for UsageState {
    fn default() -> Self {
        Self {
            window_tokens: None,
            // An empty session has spent nothing. Only a known response cost can
            // expose this balance; a missing cost makes the session total unknown.
            cost_micro_usd: Some(0),
        }
    }
}

impl UsageState {
    pub(crate) fn completed(&mut self, usage: Option<Usage>) -> Option<SessionUsage> {
        self.cost_micro_usd = self
            .cost_micro_usd
            .zip(usage.and_then(|usage| usage.cost_micro_usd))
            .and_then(|(total, cost)| total.checked_add(cost));
        let usage = usage?;
        // Match the host's context numerator: latest input, including reported
        // cache categories, not lifetime tokens or output/reasoning tokens.
        let used_tokens = usage
            .input_uncached?
            .checked_add(usage.cache_read.unwrap_or(0))?
            .checked_add(usage.cache_write.unwrap_or(0))?;
        Some(SessionUsage {
            used_tokens,
            window_tokens: self.window_tokens?,
            cost_micro_usd: self.cost_micro_usd,
        })
    }
}
