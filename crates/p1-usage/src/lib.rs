//! Subscription usage snapshots and the palette-independent ledger.
#![forbid(unsafe_code)]

mod probe;
mod render;
mod timestamp;

pub use probe::{HttpProbe, UsageProbe, snapshot, snapshot_with};
pub use render::{Line, Span, Tone, render};

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
}

impl RouteUsage {
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
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExtraUsage {
    pub enabled: Option<bool>,
    pub used: Option<f64>,
    pub limit: Option<f64>,
    pub currency: Option<String>,
}
