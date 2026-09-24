//! Subscription usage snapshots and the palette-independent ledger.
#![forbid(unsafe_code)]

mod hash;
mod probe;
mod render;
mod timestamp;

pub use hash::sha256_hex;
pub use probe::{HttpProbe, UsageProbe, openrouter_credits, snapshot, snapshot_with};
pub use render::{Line, Span, Tone, render, render_fitted, status_line};

use p1_auth::CredentialSpec;
use serde::{Deserialize, Serialize};

/// Host-provided route metadata: a reference to credentials, never a credential.
#[derive(Clone)]
pub struct UsageRoute {
    pub route_id: String,
    pub label: String,
    pub credential: String,
    pub spec: CredentialSpec,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Snapshot {
    pub taken_at: String,
    pub routes: Vec<RouteUsage>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RouteUsage {
    pub route_id: String,
    pub label: String,
    pub credential: String,
    pub probe: Probe,
    pub plan: Option<String>,
    pub windows: Vec<Window>,
    pub credits: Option<Credits>,
    pub extra_usage: Option<ExtraUsage>,
    /// RFC 3339 time these values were fetched. `None` when the route has never produced
    /// data (an unsupported route, or a failure with no last good snapshot).
    #[serde(default)]
    pub observed_at: Option<String>,
    /// Set when these are the last good values because the latest probe failed; carries the
    /// sanitized failure label (`no access`, `error · HTTP 500`, …) so a stale row is never
    /// mistaken for fresh.
    #[serde(default)]
    pub stale: Option<String>,
}

impl RouteUsage {
    /// A route entry the host builds itself (a credits-only endpoint with no route file).
    pub fn new(route_id: &str, label: &str, credential: &str) -> Self {
        Self {
            route_id: route_id.to_string(),
            label: label.to_string(),
            credential: credential.to_string(),
            probe: Probe::Supported,
            plan: None,
            windows: Vec::new(),
            credits: None,
            extra_usage: None,
            observed_at: None,
            stale: None,
        }
    }

    fn empty(route: &UsageRoute) -> Self {
        Self {
            route_id: route.route_id.clone(),
            label: route.label.clone(),
            credential: route.credential.clone(),
            probe: Probe::Supported,
            plan: None,
            windows: Vec::new(),
            credits: None,
            extra_usage: None,
            observed_at: None,
            stale: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Window {
    pub kind: WindowKind,
    pub scope: Option<String>,
    pub used_percent: Option<f64>,
    pub resets_at: Option<String>,
    pub limit_reached: bool,
    /// Vendor text that says more than the percentage does, shown as the row's value
    /// (e.g. `87/100 left`, the remaining count); the bar still carries the percentage.
    pub detail: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum WindowKind {
    Session,
    Weekly,
    WeeklyScoped,
    Other(String),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Probe {
    Supported,
    Unsupported { reason: String },
    Failed { kind: FailKind, detail: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum FailKind {
    Credential,
    Http(u16),
    Network,
    Parse,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Credits {
    pub balance: Option<f64>,
    pub reset_credits: Option<u64>,
    /// Credits consumed and the total purchased, when the vendor reports both (OpenRouter);
    /// a route with these renders `credits used` / `credits left` instead of a bare balance.
    #[serde(default)]
    pub used: Option<f64>,
    #[serde(default)]
    pub limit: Option<f64>,
    #[serde(default)]
    pub currency: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExtraUsage {
    pub enabled: Option<bool>,
    pub used: Option<f64>,
    pub limit: Option<f64>,
    pub currency: Option<String>,
}
