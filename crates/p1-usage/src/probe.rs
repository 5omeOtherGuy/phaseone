use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use crate::timestamp;
use futures_util::future::join_all;
use p1_auth::{CredentialKind, Locations};
use p1_provider_http::Transport;
use serde_json::Value;
use time::OffsetDateTime;

use crate::{
    Credits, ExtraUsage, FailKind, Probe, RouteUsage, Snapshot, UsageRoute, Window, WindowKind,
};

type ProbeFuture<'a> = Pin<Box<dyn Future<Output = RouteUsage> + Send + 'a>>;

/// `user-agent` for the API-key probes: the vendor asks for a caller version, and it must
/// not be a route file's business (a file would stale it).
const USER_AGENT: &str = concat!("p1/", env!("CARGO_PKG_VERSION"));

/// The OpenCode Zen usage endpoint asks for a session id; the ledger keeps no session, so
/// one short constant identifies the caller.
const SESSION_ID: &str = "p1-usage";

/// How far apart a `limits` entry's `resetTime` and a window's `reset_time` may be and still
/// be treated as the same instant. The vendor computes the two independently; the saved Kimi
/// body skews by one second. This is only a candidate test: [`unambiguous_detail`] accepts a
/// detail only when that candidate names exactly one window and no other entry names it.
const DETAIL_MATCH_SECONDS: i64 = 60;

/// The vendor wire shape of a route's usage endpoint. A route with no known endpoint is
/// `None`, and probes as `Unsupported`.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Shape {
    Claude,
    Codex,
    OpenCodeGo,
    Kimi,
    Glm,
}

impl Shape {
    fn of(route: &UsageRoute) -> Option<Self> {
        match route.spec.kind {
            CredentialKind::ClaudeCodeOauth => Some(Self::Claude),
            CredentialKind::CodexOauth => Some(Self::Codex),
            // An API-key route is matched by the id the host ships. A key route with no
            // known endpoint stays unsupported: guessing one from its name would probe
            // somebody else's service with the owner's credential.
            CredentialKind::ApiKey => match route.route_id.as_str() {
                // Every OpenCode Go account route speaks the same `/zen/go/v1` surface. The Zen
                // FREE routes (`opencode-zen-*`) deliberately do NOT map here: no Zen usage
                // endpoint is established anywhere (docs/design/usage.md), so they stay
                // `Unsupported` rather than probing a guessed URL with the owner's credential.
                "opencode-go-subscription"
                | "opencode-go-1-subscription"
                | "opencode-go-2-subscription"
                | "opencode-go-3-subscription" => Some(Self::OpenCodeGo),
                "kimi-coding-subscription" => Some(Self::Kimi),
                "glm-subscription" => Some(Self::Glm),
                _ => None,
            },
            // A route that sends no credential (issue #134) cannot be probed: the
            // egress proxy injects the credential, so p1 has none to present.
            CredentialKind::None => None,
        }
    }

    fn url(self) -> &'static str {
        match self {
            Self::Claude => "https://api.anthropic.com/api/oauth/usage",
            Self::Codex => "https://chatgpt.com/backend-api/wham/usage",
            Self::OpenCodeGo => "https://opencode.ai/zen/go/v1/usage",
            // The coding plan's usage host is api.kimi.com, NOT the api.kimi.ai chat host.
            Self::Kimi => "https://api.kimi.com/coding/v1/usages",
            Self::Glm => "https://api.z.ai/api/monitor/usage/quota/limit",
        }
    }

    fn api_key(self) -> bool {
        matches!(self, Self::OpenCodeGo | Self::Kimi | Self::Glm)
    }

    fn failure(self, status: u16, bytes: &[u8]) -> Probe {
        // An API-key route that refuses the key is a credential problem, not a transport
        // one. The OAuth probes keep reporting the status they always did.
        if self.api_key() && matches!(status, 401 | 402) {
            return Probe::Failed {
                kind: FailKind::Credential,
                detail: format!("HTTP {status}"),
            };
        }
        http_failure(status, bytes)
    }
}

/// Injectable probe boundary: each route finishes independently of the others.
pub trait UsageProbe: Send + Sync {
    fn probe<'a>(
        &'a self,
        route: &'a UsageRoute,
        locations: &'a Locations,
        transport: Arc<dyn Transport>,
    ) -> ProbeFuture<'a>;
}

pub struct HttpProbe;

impl UsageProbe for HttpProbe {
    fn probe<'a>(
        &'a self,
        route: &'a UsageRoute,
        locations: &'a Locations,
        transport: Arc<dyn Transport>,
    ) -> ProbeFuture<'a> {
        Box::pin(async move {
            let mut result = RouteUsage::empty(route);
            let Some(shape) = Shape::of(route) else {
                // A route that sends no credential has no endpoint p1 could probe:
                // the egress proxy injects the credential (issue #134).
                let reason = if route.spec.kind == CredentialKind::None {
                    "p1 sends no credential on this route (kind \"none\"), so it has none to \
                     present to a usage endpoint"
                } else {
                    "no usage endpoint known for this route"
                };
                result.probe = Probe::Unsupported {
                    reason: reason.into(),
                };
                return result;
            };
            let source = p1_auth::resolve(&route.route_id, &route.spec, transport, locations);
            let credential = match source.access().await {
                Ok(value) => value,
                Err(error) => {
                    result.probe = Probe::Failed {
                        kind: FailKind::Credential,
                        detail: error.to_string(),
                    };
                    return result;
                }
            };
            if shape == Shape::Codex && credential.account_id.is_none() {
                result.probe = Probe::Failed {
                    kind: FailKind::Credential,
                    detail: "account id unavailable".into(),
                };
                return result;
            }
            let client = match reqwest::Client::builder()
                .timeout(Duration::from_secs(20))
                .redirect(reqwest::redirect::Policy::none())
                .build()
            {
                Ok(client) => client,
                Err(_) => {
                    result.probe = Probe::Failed {
                        kind: FailKind::Network,
                        detail: "client initialization failed".into(),
                    };
                    return result;
                }
            };
            // Secret material is used only in request construction; no response or error carries it onward.
            let mut request = client.get(shape.url()).bearer_auth(&credential.bearer);
            request = match shape {
                Shape::Claude => request.header("anthropic-beta", "oauth-2025-04-20"),
                Shape::Codex => request.header(
                    "ChatGPT-Account-Id",
                    credential.account_id.as_deref().unwrap_or_default(),
                ),
                Shape::OpenCodeGo => request
                    .header("x-opencode-session", SESSION_ID)
                    .header("user-agent", USER_AGENT),
                Shape::Kimi | Shape::Glm => request,
            };
            let response = match request.send().await {
                Ok(response) => response,
                Err(_) => {
                    result.probe = Probe::Failed {
                        kind: FailKind::Network,
                        detail: "request failed".into(),
                    };
                    return result;
                }
            };
            let status = response.status();
            let bytes = match response.bytes().await {
                Ok(bytes) => bytes,
                Err(_) => {
                    result.probe = Probe::Failed {
                        kind: FailKind::Network,
                        detail: "response read failed".into(),
                    };
                    return result;
                }
            };
            if !status.is_success() {
                result.probe = shape.failure(status.as_u16(), &bytes);
                return result;
            }
            decode_response(shape, &bytes, result)
        })
    }
}

fn http_failure(status: u16, bytes: &[u8]) -> Probe {
    let parsed: Option<Value> = serde_json::from_slice(bytes).ok();
    let code = parsed
        .as_ref()
        .and_then(|v| {
            v.pointer("/error/type")
                .or_else(|| v.pointer("/error/code"))
                .or_else(|| v.get("type"))
                .or_else(|| v.get("code"))
        })
        .and_then(Value::as_str)
        .filter(|s| {
            s.len() <= 40
                && s.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
        });
    Probe::Failed {
        kind: FailKind::Http(status),
        detail: match code {
            Some(code) => format!("HTTP {status} · {code}"),
            None => format!("HTTP {status}"),
        },
    }
}

fn decode_response(shape: Shape, bytes: &[u8], mut out: RouteUsage) -> RouteUsage {
    if parse_response(shape, bytes, &mut out).is_err() {
        out.probe = Probe::Failed {
            kind: FailKind::Parse,
            detail: "invalid usage response".into(),
        };
        out.windows.clear();
        out.credits = None;
        out.extra_usage = None;
        out.plan = None;
    }
    out
}

fn parse_response(shape: Shape, bytes: &[u8], out: &mut RouteUsage) -> Result<(), ()> {
    let data: Value = serde_json::from_slice(bytes).map_err(|_| ())?;
    match shape {
        Shape::Claude => parse_claude(&data, out),
        Shape::Codex => parse_codex(&data, out),
        Shape::OpenCodeGo => parse_opencode_go(&data, out),
        Shape::Kimi => parse_kimi(&data, out),
        Shape::Glm => parse_glm(&data, out),
    }
}

fn parse_claude(data: &Value, out: &mut RouteUsage) -> Result<(), ()> {
    let limits = data.get("limits").and_then(Value::as_array).ok_or(())?;
    for limit in limits {
        let kind = match limit.get("kind").and_then(Value::as_str).ok_or(())? {
            "session" => WindowKind::Session,
            "weekly_all" => WindowKind::Weekly,
            "weekly_scoped" => WindowKind::WeeklyScoped,
            other => WindowKind::Other(other.to_string()),
        };
        out.windows.push(Window {
            kind,
            scope: limit
                .pointer("/scope/model/display_name")
                .and_then(Value::as_str)
                .map(str::to_string),
            used_percent: limit.get("percent").and_then(Value::as_f64),
            resets_at: limit
                .get("resets_at")
                .and_then(Value::as_str)
                .map(str::to_string),
            limit_reached: limit
                .get("limit_reached")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            detail: None,
        });
    }
    if let Some(extra) = data.get("extra_usage").filter(|v| v.is_object()) {
        out.extra_usage = Some(ExtraUsage {
            enabled: extra.get("is_enabled").and_then(Value::as_bool),
            used: number(extra.get("used_credits")),
            limit: number(extra.get("monthly_limit")),
            currency: extra
                .get("currency")
                .and_then(Value::as_str)
                .map(str::to_string),
        });
    }
    Ok(())
}

fn parse_codex(data: &Value, out: &mut RouteUsage) -> Result<(), ()> {
    out.plan = data
        .get("plan_type")
        .and_then(Value::as_str)
        .map(str::to_string);
    let rate = data
        .get("rate_limit")
        .and_then(Value::as_object)
        .ok_or(())?;
    let reached = rate
        .get("limit_reached")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    for key in ["primary_window", "secondary_window"] {
        let Some(window) = rate.get(key).filter(|v| v.is_object()) else {
            continue;
        };
        let seconds = window.get("limit_window_seconds").and_then(Value::as_u64);
        let kind = match seconds {
            Some(18_000) => WindowKind::Session,
            Some(604_800) => WindowKind::Weekly,
            Some(n) => WindowKind::Other(format!("{}h", n / 3600)),
            None => WindowKind::Other("unknown".into()),
        };
        let resets_at = window
            .get("reset_at")
            .and_then(Value::as_i64)
            .and_then(|n| OffsetDateTime::from_unix_timestamp(n).ok())
            .map(timestamp::format);
        out.windows.push(Window {
            kind,
            scope: None,
            used_percent: window.get("used_percent").and_then(Value::as_f64),
            resets_at,
            limit_reached: reached,
            detail: None,
        });
    }
    let balance = number(data.pointer("/credits/balance"));
    let reset_credits = data
        .pointer("/rate_limit_reset_credits/available_count")
        .and_then(Value::as_u64);
    if balance.is_some() || reset_credits.is_some() {
        out.credits = Some(Credits {
            balance,
            reset_credits,
            used: None,
            limit: None,
            currency: None,
        });
    }
    Ok(())
}

/// `{"usage": {"rolling"|"weekly"|"monthly": {"percent", "resetsAt", "status"}}}`.
/// `rolling` is the vendor's five-hour window, so it keeps that name; `monthly` is 30 days.
fn parse_opencode_go(data: &Value, out: &mut RouteUsage) -> Result<(), ()> {
    let usage = data.get("usage").and_then(Value::as_object).ok_or(())?;
    let kinds = [
        ("rolling", WindowKind::Other("rolling".into())),
        ("weekly", WindowKind::Weekly),
        ("monthly", WindowKind::Other("30d".into())),
    ];
    for (key, kind) in kinds {
        let Some(window) = usage.get(key).and_then(Value::as_object) else {
            continue;
        };
        // A window the vendor marks rate-limited is exhausted whatever percent it reports,
        // and the ledger shows the full bar the vendor's own UI shows.
        let rate_limited = window.get("status").and_then(Value::as_str) == Some("rate-limited");
        out.windows.push(Window {
            kind,
            scope: None,
            used_percent: if rate_limited {
                Some(100.0)
            } else {
                window.get("percent").and_then(Value::as_f64)
            },
            resets_at: window
                .get("resetsAt")
                .and_then(Value::as_str)
                .map(str::to_string),
            limit_reached: rate_limited,
            detail: None,
        });
    }
    Ok(())
}

/// `{"usages": {"limit_5h"|"limit_month_total"|"limit_month_code": {"used_ratio",
/// "reset_time"}}, "limits": [{"detail": {"limit", "remaining", "resetTime"}}]}`.
/// The saved body names no window explicitly, so a `limits` entry can only be tied to the 5h
/// window by the reset instant in its detail. That tie is accepted only when it is
/// unambiguous across every window and entry (see [`unambiguous_detail`]), and only the 5h
/// window receives the text: its request-count meaning is the one the saved body proves.
///
/// The 5h window's `detail` is a remaining count, not a used one: `15/100 left` is 85 % used.
/// That count is a direct measurement, so it — and never a disagreeing or absent `used_ratio`
/// — decides the bar; the two must agree or the row contradicts itself.
fn parse_kimi(data: &Value, out: &mut RouteUsage) -> Result<(), ()> {
    let usages = data.get("usages").and_then(Value::as_object).ok_or(())?;
    // Each `limits` entry's `detail` is a request count; `remaining` is what is left, so the
    // text says `left` rather than implying the count was used, and the used share it implies
    // is what the bar must show.
    let mut details: Vec<Detail> = Vec::new();
    if let Some(limits) = data.get("limits").and_then(Value::as_array) {
        for limit in limits {
            let Some(detail) = limit.get("detail") else {
                continue;
            };
            let (Some(remaining), Some(limit_count)) =
                (text(detail.get("remaining")), text(detail.get("limit")))
            else {
                continue;
            };
            // Without the reset instant the entry cannot be tied to a window: omit it.
            let Some(at) = detail
                .get("resetTime")
                .and_then(Value::as_str)
                .and_then(timestamp::parse)
            else {
                continue;
            };
            details.push(Detail {
                at,
                text: format!("{remaining}/{limit_count} left"),
                used_percent: used_percent_of_counts(&remaining, &limit_count),
            });
        }
    }
    let kinds = [
        ("limit_5h", WindowKind::Session),
        ("limit_month_total", WindowKind::Other("month".into())),
        ("limit_month_code", WindowKind::Other("month code".into())),
    ];
    // Every present window's reset instant is needed before any detail is placed: a month
    // window can share another's reset, and a 5h reset can coincide with a month's.
    let mut window_resets: Vec<Option<OffsetDateTime>> = Vec::new();
    let mut five_hour: Option<OffsetDateTime> = None;
    for (key, _) in &kinds {
        let Some(entry) = usages.get(*key).and_then(Value::as_object) else {
            continue;
        };
        let at = entry
            .get("reset_time")
            .and_then(Value::as_str)
            .and_then(timestamp::parse);
        if *key == "limit_5h" {
            five_hour = at;
        }
        window_resets.push(at);
    }
    let five_hour_detail = unambiguous_detail(&window_resets, five_hour, &details);
    for (key, kind) in kinds {
        let Some(entry) = usages.get(key).and_then(Value::as_object) else {
            continue;
        };
        let used_ratio = entry
            .get("used_ratio")
            .and_then(Value::as_f64)
            .map(|ratio| ratio * 100.0);
        // Only the 5h window receives the count, so only it can override `used_ratio`.
        let detail = if key == "limit_5h" {
            five_hour_detail
        } else {
            None
        };
        let used_percent = detail.and_then(|detail| detail.used_percent).or(used_ratio);
        out.windows.push(Window {
            kind,
            scope: None,
            used_percent,
            resets_at: entry
                .get("reset_time")
                .and_then(Value::as_str)
                .map(str::to_string),
            limit_reached: exhausted(used_percent),
            detail: detail.map(|detail| detail.text.clone()),
        });
    }
    Ok(())
}

/// `{"data": {"limits": [{"type": "CREDIT_LIMIT", "unit", "number", "percentage",
/// "nextResetTime"}]}}`. Only `CREDIT_LIMIT` entries are quota windows; `(3, 5)` is the
/// five-hour window and `(6, 1)` the seven-day one, and any other pair keeps its identity
/// rather than being forced into a window it may not be.
fn parse_glm(data: &Value, out: &mut RouteUsage) -> Result<(), ()> {
    let limits = data
        .pointer("/data/limits")
        .and_then(Value::as_array)
        .ok_or(())?;
    for limit in limits {
        if limit.get("type").and_then(Value::as_str) != Some("CREDIT_LIMIT") {
            continue;
        }
        let (Some(unit), Some(number)) = (
            limit.get("unit").and_then(Value::as_u64),
            limit.get("number").and_then(Value::as_u64),
        ) else {
            continue;
        };
        let kind = match (unit, number) {
            (3, 5) => WindowKind::Session,
            (6, 1) => WindowKind::Weekly,
            _ => WindowKind::Other(format!("u{unit}n{number}")),
        };
        let used_percent = limit.get("percentage").and_then(Value::as_f64);
        out.windows.push(Window {
            kind,
            scope: None,
            used_percent,
            resets_at: epoch_millis(limit.get("nextResetTime")).map(timestamp::format),
            limit_reached: exhausted(used_percent),
            detail: None,
        });
    }
    Ok(())
}

/// A vendor field that is a string in one response and a number in the next, as text.
fn text(value: Option<&Value>) -> Option<String> {
    match value? {
        Value::String(text) => Some(text.clone()),
        Value::Number(number) => Some(number.to_string()),
        _ => None,
    }
}

/// The used share a `remaining`/`limit` count pair implies. `remaining` is what is left, so
/// the used share is `(limit - remaining) / limit`. `None` when either is not a finite number
/// or the limit is not positive — an unreadable count decides nothing, and no bar is drawn.
fn used_percent_of_counts(remaining: &str, limit: &str) -> Option<f64> {
    let remaining: f64 = remaining.trim().parse().ok()?;
    let limit: f64 = limit.trim().parse().ok()?;
    if !remaining.is_finite() || !limit.is_finite() || limit <= 0.0 {
        return None;
    }
    Some(((limit - remaining) / limit * 100.0).clamp(0.0, 100.0))
}

/// Epoch milliseconds, UTC, as the GLM quota endpoint reports reset times.
fn epoch_millis(value: Option<&Value>) -> Option<OffsetDateTime> {
    let millis = match value? {
        Value::Number(number) => number.as_i64()?,
        Value::String(text) => text.parse().ok()?,
        _ => return None,
    };
    OffsetDateTime::from_unix_timestamp_nanos(i128::from(millis) * 1_000_000).ok()
}

/// A `limits` entry's request-count detail: the reset instant it names, the `N/M left` text,
/// and the used share the count implies.
struct Detail {
    at: OffsetDateTime,
    text: String,
    used_percent: Option<f64>,
}

/// Whether a `limits` entry's reset instant names this window, i.e. the two are within
/// [`DETAIL_MATCH_SECONDS`].
fn within(at: &OffsetDateTime, window: &Option<OffsetDateTime>) -> bool {
    window.is_some_and(|window| (*at - window).whole_seconds().abs() <= DETAIL_MATCH_SECONDS)
}

/// The 5h request detail, only when the mapping is unambiguous: exactly one `limits` entry
/// names the 5h window, and that entry names no other window. Month windows can share a
/// reset instant and a 5h reset can coincide with a month's, so an ambiguous entry is
/// dropped rather than guessed at.
fn unambiguous_detail<'a>(
    windows: &[Option<OffsetDateTime>],
    five_hour: Option<OffsetDateTime>,
    details: &'a [Detail],
) -> Option<&'a Detail> {
    let five_hour = five_hour?;
    let naming_5h: Vec<&Detail> = details
        .iter()
        .filter(|entry| within(&entry.at, &Some(five_hour)))
        .collect();
    if naming_5h.len() != 1 {
        return None;
    }
    let entry = naming_5h[0];
    let mut named = 0usize;
    for window in windows {
        if within(&entry.at, window) {
            named += 1;
        }
    }
    if named != 1 {
        return None;
    }
    Some(entry)
}

/// A window is spent once the vendor reports at least the full quota used. An unknown
/// percentage is not exhaustion: an absent measurement must never become a `!` bar.
fn exhausted(used_percent: Option<f64>) -> bool {
    used_percent.is_some_and(|percent| percent.is_finite() && percent >= 100.0)
}

fn number(value: Option<&Value>) -> Option<f64> {
    value.and_then(|v| {
        v.as_f64()
            .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
    })
}

pub async fn snapshot(
    routes: &[UsageRoute],
    locations: &Locations,
    transport: Arc<dyn Transport>,
) -> Snapshot {
    snapshot_with(routes, locations, transport, &HttpProbe).await
}

pub async fn snapshot_with<P: UsageProbe + ?Sized>(
    routes: &[UsageRoute],
    locations: &Locations,
    transport: Arc<dyn Transport>,
    probe: &P,
) -> Snapshot {
    let taken_at = timestamp::format(OffsetDateTime::now_utc());
    let mut routes = join_all(
        routes
            .iter()
            .map(|route| probe.probe(route, locations, transport.clone())),
    )
    .await;
    // Fresh data is timestamped here; the host replaces this with the last good time when it
    // keeps a stale row (see the live watch loop).
    for route in &mut routes {
        if matches!(route.probe, Probe::Supported) {
            route.observed_at = Some(taken_at.clone());
        }
    }
    Snapshot { taken_at, routes }
}

/// The one non-route source the dashboard shows: OpenRouter credits. OpenRouter has no model
/// route in p1 and no quota windows; the host supplies the existing configured key (never
/// logged) and this makes one bounded read-only request, or an explicit failure.
pub async fn openrouter_credits(bearer: &str) -> RouteUsage {
    let mut out = RouteUsage::new(
        "openrouter-credits",
        "openrouter credits",
        "brain-tools openrouter.key",
    );
    let client = match reqwest::Client::builder()
        .timeout(Duration::from_secs(20))
        .redirect(reqwest::redirect::Policy::none())
        .build()
    {
        Ok(client) => client,
        Err(_) => {
            out.probe = Probe::Failed {
                kind: FailKind::Network,
                detail: "client initialization failed".into(),
            };
            return out;
        }
    };
    let response = match client
        .get("https://openrouter.ai/api/v1/credits")
        .bearer_auth(bearer)
        .header("user-agent", USER_AGENT)
        .send()
        .await
    {
        Ok(response) => response,
        Err(_) => {
            out.probe = Probe::Failed {
                kind: FailKind::Network,
                detail: "request failed".into(),
            };
            return out;
        }
    };
    let status = response.status();
    let bytes = match response.bytes().await {
        Ok(bytes) => bytes,
        Err(_) => {
            out.probe = Probe::Failed {
                kind: FailKind::Network,
                detail: "response read failed".into(),
            };
            return out;
        }
    };
    if !status.is_success() {
        // A refused key is `no access`; every other status keeps the HTTP kind.
        let refused = matches!(status.as_u16(), 401 | 402);
        out.probe = match http_failure(status.as_u16(), &bytes) {
            Probe::Failed { kind, detail } => Probe::Failed {
                kind: if refused { FailKind::Credential } else { kind },
                detail: if refused {
                    format!("HTTP {status}")
                } else {
                    detail
                },
            },
            other => other,
        };
        return out;
    }
    match parse_openrouter_credits(&bytes) {
        Some((used, limit, balance)) => {
            out.credits = Some(Credits {
                balance: Some(balance),
                reset_credits: None,
                used: Some(used),
                limit: Some(limit),
                currency: Some("USD".into()),
            });
        }
        None => {
            out.probe = Probe::Failed {
                kind: FailKind::Parse,
                detail: "invalid credits response".into(),
            };
        }
    }
    out
}

/// `{"data": {"total_credits", "total_usage"}}` as `(used, limit, balance)` in USD. The
/// cents are rounded once and the balance derived from them, so `used + balance == limit` in
/// the display and the three values never disagree by a cent.
fn parse_openrouter_credits(bytes: &[u8]) -> Option<(f64, f64, f64)> {
    let data: Value = serde_json::from_slice(bytes).ok()?;
    let data = data.get("data")?;
    let limit = round_cents(data.get("total_credits").and_then(Value::as_f64)?);
    let used = round_cents(
        data.get("total_usage")
            .and_then(Value::as_f64)
            .unwrap_or(0.0),
    );
    Some((used, limit, round_cents(limit - used)))
}

fn round_cents(value: f64) -> f64 {
    (value * 100.0).round() / 100.0
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The three API-key usage shapes, exactly as the vendors answered on 2026-09-23
    /// (values illustrative, no credential anywhere).
    const OPENCODE_GO_BODY: &[u8] = br#"{"usage":{
        "rolling":{"percent":0,"resetsAt":"2026-09-24T00:51:16.925Z","status":"ok"},
        "weekly":{"percent":100,"resetsAt":"2026-09-28T00:00:00.000Z","status":"rate-limited"},
        "monthly":{"percent":58,"resetsAt":"2026-10-20T18:12:09.000Z","status":"ok"}}}"#;
    const KIMI_BODY: &[u8] = br#"{"usages":{
        "limit_5h":{"used_ratio":0.13,"reset_time":"2026-09-24T00:05:01Z"},
        "limit_month_total":{"used_ratio":0.2009,"reset_time":"2026-10-19T00:00:00Z"},
        "limit_month_code":{"used_ratio":0.0,"reset_time":"2026-10-19T00:00:00Z"}},
        "limits":[{"detail":{"limit":"100","remaining":"87","resetTime":"2026-09-24T00:05:02Z"}}]}"#;
    const GLM_BODY: &[u8] = br#"{"data":{"limits":[
        {"type":"CREDIT_LIMIT","unit":3,"number":5,"percentage":0,"nextResetTime":1758700800000},
        {"type":"CREDIT_LIMIT","unit":6,"number":1,"percentage":100,"nextResetTime":1758876528000},
        {"type":"TOKENS_LIMIT","unit":3,"number":5,"percentage":50,"nextResetTime":1758876528000},
        {"type":"CREDIT_LIMIT","unit":9,"number":2,"percentage":42,"nextResetTime":1758876528000}]}}"#;

    fn fixture_route(id: &str) -> UsageRoute {
        fixture_route_of(id, CredentialKind::ApiKey)
    }

    fn fixture_route_of(id: &str, kind: CredentialKind) -> UsageRoute {
        UsageRoute {
            route_id: id.into(),
            label: id.into(),
            credential: "test source".into(),
            spec: p1_auth::CredentialSpec {
                kind,
                env: None,
                borrow: vec![],
                store_only: false,
            },
        }
    }

    fn parsed(shape: Shape, route: &str, body: &[u8]) -> RouteUsage {
        let mut out = RouteUsage::empty(&fixture_route(route));
        parse_response(shape, body, &mut out).expect("fixture body parses");
        out
    }

    fn close(actual: Option<f64>, expected: f64) {
        let actual = actual.expect("a percentage");
        assert!((actual - expected).abs() < 1e-9, "{actual} != {expected}");
    }

    #[test]
    fn probe_fixtures() {
        let mut claude = RouteUsage::empty(&fixture_route("anthropic-subscription"));
        parse_response(Shape::Claude, br#"{"limits":[{"kind":"session","percent":67,"resets_at":"2026-09-22T23:00:00Z"},{"kind":"weekly_scoped","percent":7,"scope":{"model":{"display_name":"Fable"}}}],"extra_usage":{"is_enabled":true,"used_credits":12.4,"monthly_limit":5000,"currency":"EUR"}}"#, &mut claude).unwrap();
        assert!(matches!(claude.windows[1].kind, WindowKind::WeeklyScoped));
        assert_eq!(claude.windows[1].scope.as_deref(), Some("Fable"));
        assert_eq!(claude.extra_usage.as_ref().unwrap().used, Some(12.4));
        let mut codex = RouteUsage::empty(&fixture_route("openai-codex-subscription"));
        parse_response(Shape::Codex, br#"{"plan_type":"prolite","rate_limit":{"primary_window":{"used_percent":69,"limit_window_seconds":604800,"reset_at":1790451360},"limit_reached":true},"credits":{"balance":0},"rate_limit_reset_credits":{"available_count":1}}"#, &mut codex).unwrap();
        assert_eq!(codex.windows.len(), 1);
        assert!(codex.windows[0].limit_reached);
        assert!(matches!(codex.windows[0].kind, WindowKind::Weekly));
    }

    /// Every API-key route id the host ships reaches its vendor's usage host — and no other
    /// key route is guessed at.
    #[test]
    fn route_ids_map_to_their_endpoints() {
        let url = |id: &str| Shape::of(&fixture_route(id)).map(Shape::url);
        assert_eq!(
            url("opencode-go-subscription"),
            Some("https://opencode.ai/zen/go/v1/usage")
        );
        assert_eq!(
            url("opencode-go-1-subscription"),
            Some("https://opencode.ai/zen/go/v1/usage")
        );
        assert_eq!(
            url("opencode-go-2-subscription"),
            Some("https://opencode.ai/zen/go/v1/usage")
        );
        assert_eq!(
            url("opencode-go-3-subscription"),
            Some("https://opencode.ai/zen/go/v1/usage")
        );
        assert_eq!(
            url("kimi-coding-subscription"),
            Some("https://api.kimi.com/coding/v1/usages")
        );
        assert_eq!(
            url("glm-subscription"),
            Some("https://api.z.ai/api/monitor/usage/quota/limit")
        );
        assert_eq!(url("some-other-key-route"), None);
        // The Zen FREE accounts have NO established usage endpoint (`https://opencode.ai/zen/v1`
        // documents none), so they must stay unsupported: probing a guessed URL would send the
        // owner's credential to a path nobody verified.
        assert_eq!(url("opencode-zen-1"), None);
        assert_eq!(url("opencode-zen-2"), None);
        assert_eq!(url("opencode-zen-3"), None);
        assert_eq!(url("opencode-zen-free"), None);
        // ClinePass shows usage only on its dashboard; no usage API is documented.
        assert_eq!(url("cline-pass-1"), None);
        assert_eq!(url("cline-pass-2"), None);
        let oauth =
            |kind| Shape::of(&fixture_route_of("anthropic-subscription", kind)).map(Shape::url);
        assert_eq!(
            oauth(CredentialKind::ClaudeCodeOauth),
            Some("https://api.anthropic.com/api/oauth/usage")
        );
        assert_eq!(
            oauth(CredentialKind::CodexOauth),
            Some("https://chatgpt.com/backend-api/wham/usage")
        );
    }

    /// Issue #134: a route that sends no credential has no usage endpoint p1 could
    /// probe — it has no credential to present — so it is never mapped to one.
    #[test]
    fn a_route_that_sends_no_credential_maps_to_no_endpoint() {
        let route = fixture_route_of("proxy-route", CredentialKind::None);
        assert!(Shape::of(&route).is_none());
    }

    /// A transport that fails the test if a probe ever reaches the network.
    struct NoNetwork;

    impl Transport for NoNetwork {
        fn post<'a>(
            &'a self,
            _request: p1_provider_http::HttpRequest,
        ) -> Pin<
            Box<
                dyn Future<
                        Output = Result<
                            p1_provider_http::HttpResponse,
                            p1_provider_http::TransportError,
                        >,
                    > + Send
                    + 'a,
            >,
        > {
            Box::pin(async {
                panic!("a route that sends no credential must not reach the network")
            })
        }
    }

    /// Issue #134: such a route is reported `Unsupported`, with a reason that says p1
    /// has no credential to present — and not a single request is made.
    #[tokio::test]
    async fn a_route_that_sends_no_credential_is_unsupported() {
        let route = fixture_route_of("proxy-route", CredentialKind::None);
        let result = HttpProbe
            .probe(&route, &Locations::none(), Arc::new(NoNetwork))
            .await;
        match result.probe {
            Probe::Unsupported { reason } => {
                for part in ["none", "no credential"] {
                    assert!(reason.contains(part), "{reason}: {part}");
                }
            }
            other => panic!("expected Unsupported, got {other:?}"),
        }
        assert!(result.windows.is_empty());
    }

    #[test]
    fn opencode_go_windows() {
        let go = parsed(
            Shape::OpenCodeGo,
            "opencode-go-subscription",
            OPENCODE_GO_BODY,
        );
        assert_eq!(go.windows.len(), 3);
        // `rolling` is the vendor's own name for its five-hour window.
        assert!(matches!(&go.windows[0].kind, WindowKind::Other(name) if name == "rolling"));
        assert_eq!(go.windows[0].used_percent, Some(0.0));
        assert_eq!(
            go.windows[0].resets_at.as_deref(),
            Some("2026-09-24T00:51:16.925Z")
        );
        assert!(!go.windows[0].limit_reached);
        assert!(matches!(go.windows[1].kind, WindowKind::Weekly));
        assert_eq!(go.windows[1].used_percent, Some(100.0));
        assert!(go.windows[1].limit_reached);
        assert!(matches!(&go.windows[2].kind, WindowKind::Other(name) if name == "30d"));
        assert_eq!(go.windows[2].used_percent, Some(58.0));
        assert_eq!(
            go.windows[2].resets_at.as_deref(),
            Some("2026-10-20T18:12:09.000Z")
        );
    }

    /// `status: "rate-limited"` is a spent window: it renders as the vendor's full bar with
    /// the reset time, whatever percentage the body carries.
    #[test]
    fn opencode_go_rate_limited_is_a_full_bar() {
        let go = parsed(
            Shape::OpenCodeGo,
            "opencode-go-subscription",
            br#"{"usage":{"weekly":{"percent":58,"resetsAt":"2026-09-28T00:00:00.000Z","status":"rate-limited"}}}"#,
        );
        assert_eq!(go.windows.len(), 1);
        assert_eq!(go.windows[0].used_percent, Some(100.0));
        assert!(go.windows[0].limit_reached);
        assert_eq!(
            go.windows[0].resets_at.as_deref(),
            Some("2026-09-28T00:00:00.000Z")
        );
    }

    #[test]
    fn kimi_windows() {
        let kimi = parsed(Shape::Kimi, "kimi-coding-subscription", KIMI_BODY);
        assert_eq!(kimi.windows.len(), 3);
        assert!(matches!(kimi.windows[0].kind, WindowKind::Session));
        close(kimi.windows[0].used_percent, 13.0);
        assert_eq!(
            kimi.windows[0].resets_at.as_deref(),
            Some("2026-09-24T00:05:01Z")
        );
        // The request count is tied to the 5h window by its reset instant (the fixture's
        // one-second skew), not by its position in `limits`.
        assert_eq!(kimi.windows[0].detail.as_deref(), Some("87/100 left"));
        assert!(!kimi.windows[0].limit_reached);
        assert!(matches!(&kimi.windows[1].kind, WindowKind::Other(name) if name == "month"));
        close(kimi.windows[1].used_percent, 20.09);
        assert_eq!(kimi.windows[1].detail, None);
        assert!(matches!(&kimi.windows[2].kind, WindowKind::Other(name) if name == "month code"));
        assert_eq!(kimi.windows[2].used_percent, Some(0.0));
    }

    /// The Kimi 5h window's value is a remaining request count: `15/100 left` is 85 % used, so
    /// no bar may sit at 0 %. The count is a direct measurement and decides the used share even
    /// when `used_ratio` is absent or disagrees (the live row showed `15/100 left` beside 0 %).
    #[test]
    fn kimi_remaining_count_decides_the_used_percent() {
        let cases: &[(&[u8], f64, &str)] = &[
            (
                br#"{"usages":{"limit_5h":{"used_ratio":0.0,"reset_time":"2026-09-24T00:05:01Z"}},"limits":[{"detail":{"limit":"100","remaining":"15","resetTime":"2026-09-24T00:05:02Z"}}]}"#,
                85.0,
                "15/100 left",
            ),
            (
                br#"{"usages":{"limit_5h":{"reset_time":"2026-09-24T00:05:01Z"}},"limits":[{"detail":{"limit":"100","remaining":"15","resetTime":"2026-09-24T00:05:02Z"}}]}"#,
                85.0,
                "15/100 left",
            ),
            (
                br#"{"usages":{"limit_5h":{"reset_time":"2026-09-24T00:05:01Z"}},"limits":[{"detail":{"limit":"100","remaining":"100","resetTime":"2026-09-24T00:05:02Z"}}]}"#,
                0.0,
                "100/100 left",
            ),
            (
                br#"{"usages":{"limit_5h":{"reset_time":"2026-09-24T00:05:01Z"}},"limits":[{"detail":{"limit":"100","remaining":"0","resetTime":"2026-09-24T00:05:02Z"}}]}"#,
                100.0,
                "0/100 left",
            ),
        ];
        for (body, expected, text) in cases {
            let kimi = parsed(Shape::Kimi, "kimi-coding-subscription", body);
            assert_eq!(
                kimi.windows[0].used_percent,
                Some(*expected),
                "{}",
                String::from_utf8_lossy(body)
            );
            assert_eq!(kimi.windows[0].detail.as_deref(), Some(*text));
        }
    }

    /// End to end, no network: a fabricated Kimi body with `15/100 left` renders that text
    /// and a bar at the used fraction it implies (85 %), never at 0 %.
    #[test]
    fn kimi_remaining_row_renders_text_and_bar_consistently() {
        let body = br#"{"usages":{"limit_5h":{"used_ratio":0.0,"reset_time":"2026-09-24T00:05:01Z"}},"limits":[{"detail":{"limit":"100","remaining":"15","resetTime":"2026-09-24T00:05:02Z"}}]}"#;
        let kimi = parsed(Shape::Kimi, "kimi-coding-subscription", body);
        let snapshot = Snapshot {
            taken_at: "2026-09-24T00:06:00Z".into(),
            routes: vec![kimi],
        };
        let grid = 32;
        let lines = crate::render::render(&snapshot, grid);
        let texts: Vec<String> = lines.iter().map(|line| line.text()).collect();
        assert!(
            texts
                .iter()
                .any(|t| t.starts_with("5h") && t.contains("15/100 left")),
            "{texts:#?}"
        );
        let bars: Vec<&crate::render::Line> = lines
            .iter()
            .filter(|line| {
                line.0
                    .iter()
                    .any(|span| span.tone == crate::render::Tone::Rule)
            })
            .collect();
        assert_eq!(bars.len(), 1, "{texts:#?}");
        let cells = grid - 6;
        assert_eq!(
            bars[0].0[0].text.chars().count(),
            (0.85 * cells as f64).round() as usize
        );
        assert!(
            bars[0].text().trim_end().ends_with("85%"),
            "{}",
            bars[0].text()
        );
    }

    /// A Kimi window with neither a `used_ratio` nor a count is unknown: the row shows the
    /// marker and draws no bar at all (an empty bar would read as `0 % used`).
    #[test]
    fn kimi_unknown_window_shows_no_value_and_no_bar() {
        let kimi = parsed(
            Shape::Kimi,
            "kimi-coding-subscription",
            br#"{"usages":{"limit_5h":{"reset_time":"2026-09-24T00:05:01Z"}}}"#,
        );
        assert_eq!(kimi.windows[0].used_percent, None);
        assert_eq!(kimi.windows[0].detail, None);
        let snapshot = Snapshot {
            taken_at: "2026-09-24T00:06:00Z".into(),
            routes: vec![kimi],
        };
        let lines = crate::render::render(&snapshot, 32);
        let texts: Vec<String> = lines.iter().map(|line| line.text()).collect();
        assert!(
            texts
                .iter()
                .any(|t| t.starts_with("5h") && t.contains("— · resets")),
            "{texts:#?}"
        );
        assert!(
            !lines.iter().any(|line| {
                line.0
                    .iter()
                    .any(|span| span.tone == crate::render::Tone::Rule)
            }),
            "{texts:#?}"
        );
    }

    /// A `limits` entry names its window by the reset instant in its detail, never by array
    /// position: the 5h detail sits behind a month detail and a far-away unrelated entry.
    /// Only the 5h window carries the text; the month window is not given one.
    #[test]
    fn kimi_detail_follows_the_reset_instant_not_the_array_position() {
        let kimi = parsed(
            Shape::Kimi,
            "kimi-coding-subscription",
            br#"{"usages":{
                "limit_5h":{"used_ratio":0.13,"reset_time":"2026-09-24T00:05:01Z"},
                "limit_month_total":{"used_ratio":0.2,"reset_time":"2026-10-19T00:00:00Z"}},
                "limits":[
                  {"detail":{"limit":"5000","remaining":"4000","resetTime":"2026-10-19T00:00:00Z"}},
                  {"detail":{"limit":"100","remaining":"87","resetTime":"2026-09-24T00:05:02Z"}},
                  {"detail":{"limit":"9","remaining":"9","resetTime":"2001-01-01T00:00:00Z"}}]}"#,
        );
        assert_eq!(kimi.windows.len(), 2);
        assert_eq!(kimi.windows[0].detail.as_deref(), Some("87/100 left"));
        assert_eq!(kimi.windows[1].detail, None);
    }

    /// Two windows sharing a reset instant (the month pair does) make any entry at that
    /// instant name more than one window: it is dropped, never attached to either.
    #[test]
    fn kimi_detail_is_omitted_when_windows_share_a_reset() {
        let kimi = parsed(
            Shape::Kimi,
            "kimi-coding-subscription",
            br#"{"usages":{
                "limit_5h":{"used_ratio":0.1,"reset_time":"2026-09-24T00:05:01Z"},
                "limit_month_total":{"used_ratio":0.2,"reset_time":"2026-10-19T00:00:00Z"},
                "limit_month_code":{"used_ratio":0.3,"reset_time":"2026-10-19T00:00:00Z"}},
                "limits":[{"detail":{"limit":"100","remaining":"90","resetTime":"2026-10-19T00:00:00Z"}}]}"#,
        );
        assert!(kimi.windows.iter().all(|window| window.detail.is_none()));
    }

    /// A 5h reset that coincides with a month reset makes the entry name two windows: drop.
    #[test]
    fn kimi_detail_is_omitted_when_5h_coincides_with_a_month() {
        let kimi = parsed(
            Shape::Kimi,
            "kimi-coding-subscription",
            br#"{"usages":{
                "limit_5h":{"used_ratio":0.1,"reset_time":"2026-09-24T00:05:01Z"},
                "limit_month_total":{"used_ratio":0.2,"reset_time":"2026-09-24T00:05:30Z"}},
                "limits":[{"detail":{"limit":"100","remaining":"90","resetTime":"2026-09-24T00:05:15Z"}}]}"#,
        );
        assert!(kimi.windows.iter().all(|window| window.detail.is_none()));
    }

    /// Two entries within the match window of the 5h reset are an equal-distance tie: drop.
    #[test]
    fn kimi_detail_is_omitted_on_an_equal_distance_tie() {
        let kimi = parsed(
            Shape::Kimi,
            "kimi-coding-subscription",
            br#"{"usages":{"limit_5h":{"used_ratio":0.1,"reset_time":"2026-09-24T00:05:00Z"}},
                "limits":[
                  {"detail":{"limit":"100","remaining":"90","resetTime":"2026-09-24T00:05:30Z"}},
                  {"detail":{"limit":"100","remaining":"80","resetTime":"2026-09-24T00:04:30Z"}}]}"#,
        );
        assert_eq!(kimi.windows.len(), 1);
        assert_eq!(kimi.windows[0].detail, None);
    }

    /// A detail whose reset instant names no window is dropped, not shown on the first row.
    #[test]
    fn kimi_detail_without_a_matching_window_is_omitted() {
        let kimi = parsed(
            Shape::Kimi,
            "kimi-coding-subscription",
            br#"{"usages":{"limit_5h":{"used_ratio":0.5,"reset_time":"2026-09-24T00:05:01Z"}},
                "limits":[{"detail":{"limit":"100","remaining":"50","resetTime":"2027-01-01T00:00:00Z"}}]}"#,
        );
        assert_eq!(kimi.windows.len(), 1);
        assert_eq!(kimi.windows[0].detail, None);
    }

    /// `>= 100 %` is spent for the two vendors that report a continuous quota, and an
    /// unknown percentage is never exhaustion.
    #[test]
    fn quota_boundaries_flag_exhaustion() {
        let kimi = parsed(
            Shape::Kimi,
            "kimi-coding-subscription",
            br#"{"usages":{
                "limit_5h":{"used_ratio":1.0,"reset_time":"2026-09-24T00:05:01Z"},
                "limit_month_total":{"used_ratio":1.2,"reset_time":"2026-10-19T00:00:00Z"},
                "limit_month_code":{"used_ratio":0.999,"reset_time":"2026-10-19T00:00:00Z"}}}"#,
        );
        assert!(kimi.windows[0].limit_reached, "exactly 100 %");
        assert!(kimi.windows[1].limit_reached, "over 100 %");
        assert!(!kimi.windows[2].limit_reached, "just under 100 %");
        let glm = parsed(
            Shape::Glm,
            "glm-subscription",
            br#"{"data":{"limits":[
                {"type":"CREDIT_LIMIT","unit":3,"number":5,"percentage":100,"nextResetTime":1758700800000},
                {"type":"CREDIT_LIMIT","unit":6,"number":1,"percentage":100.5,"nextResetTime":1758876528000},
                {"type":"CREDIT_LIMIT","unit":9,"number":2,"percentage":99.9,"nextResetTime":1758876528000}]}}"#,
        );
        assert!(glm.windows[0].limit_reached, "exactly 100 %");
        assert!(glm.windows[1].limit_reached, "over 100 %");
        assert!(!glm.windows[2].limit_reached, "just under 100 %");
    }

    #[test]
    fn unknown_usage_is_not_flagged_exhausted() {
        let kimi = parsed(
            Shape::Kimi,
            "kimi-coding-subscription",
            br#"{"usages":{"limit_5h":{"reset_time":"2026-09-24T00:05:01Z"}}}"#,
        );
        assert_eq!(kimi.windows[0].used_percent, None);
        assert!(!kimi.windows[0].limit_reached);
        let glm = parsed(
            Shape::Glm,
            "glm-subscription",
            br#"{"data":{"limits":[{"type":"CREDIT_LIMIT","unit":3,"number":5,"nextResetTime":1758700800000}]}}"#,
        );
        assert_eq!(glm.windows[0].used_percent, None);
        assert!(!glm.windows[0].limit_reached);
    }

    #[test]
    fn kimi_without_a_limits_detail_has_no_value_text() {
        let kimi = parsed(
            Shape::Kimi,
            "kimi-coding-subscription",
            br#"{"usages":{"limit_5h":{"used_ratio":1.0,"reset_time":"2026-09-24T00:05:01Z"}}}"#,
        );
        assert_eq!(kimi.windows.len(), 1);
        assert_eq!(kimi.windows[0].detail, None);
        close(kimi.windows[0].used_percent, 100.0);
        // A spent window with no value text is still flagged as spent.
        assert!(kimi.windows[0].limit_reached);
    }

    #[test]
    fn glm_windows() {
        let glm = parsed(Shape::Glm, "glm-subscription", GLM_BODY);
        // The non-`CREDIT_LIMIT` entry is not a quota window.
        assert_eq!(glm.windows.len(), 3);
        assert!(matches!(glm.windows[0].kind, WindowKind::Session));
        assert_eq!(glm.windows[0].used_percent, Some(0.0));
        assert_eq!(
            glm.windows[0].resets_at.as_deref(),
            Some("2025-09-24T08:00:00Z")
        );
        assert!(matches!(glm.windows[1].kind, WindowKind::Weekly));
        assert_eq!(glm.windows[1].used_percent, Some(100.0));
        assert!(glm.windows[1].limit_reached);
        assert!(matches!(&glm.windows[2].kind, WindowKind::Other(name) if name == "u9n2"));
        assert_eq!(glm.windows[2].used_percent, Some(42.0));
    }

    /// A refused key is a credential failure on a key route and an HTTP status on an OAuth
    /// route, and no failure carries a body.
    #[test]
    fn api_key_statuses_map_to_credential_or_http() {
        let body = br#"{"error":{"code":"unauthorized","detail":"SENTINEL_PRIVATE_VALUE"}}"#;
        for shape in [Shape::OpenCodeGo, Shape::Kimi, Shape::Glm] {
            for status in [401, 402] {
                match shape.failure(status, body) {
                    Probe::Failed {
                        kind: FailKind::Credential,
                        detail,
                    } => assert_eq!(detail, format!("HTTP {status}")),
                    other => panic!("{status} did not map to a credential failure: {other:?}"),
                }
            }
            assert!(matches!(
                shape.failure(500, body),
                Probe::Failed {
                    kind: FailKind::Http(500),
                    ..
                }
            ));
        }
        assert!(matches!(
            Shape::Claude.failure(401, body),
            Probe::Failed {
                kind: FailKind::Http(401),
                ..
            }
        ));
    }

    #[test]
    fn malformed_bodies_are_parse_failures() {
        let shapes = [
            (Shape::OpenCodeGo, "opencode-go-subscription"),
            (Shape::Kimi, "kimi-coding-subscription"),
            (Shape::Glm, "glm-subscription"),
        ];
        for (shape, route) in shapes {
            for body in [&b"not json"[..], br#"{"unexpected":true}"#] {
                let out = decode_response(shape, body, RouteUsage::empty(&fixture_route(route)));
                assert!(
                    matches!(
                        out.probe,
                        Probe::Failed {
                            kind: FailKind::Parse,
                            ..
                        }
                    ),
                    "{route} accepted {body:?}"
                );
            }
        }
    }

    struct FakeProbe {
        barrier: tokio::sync::Barrier,
        bearer: String,
    }
    impl UsageProbe for FakeProbe {
        fn probe<'a>(
            &'a self,
            route: &'a UsageRoute,
            _: &'a Locations,
            _: Arc<dyn Transport>,
        ) -> ProbeFuture<'a> {
            Box::pin(async move {
                self.barrier.wait().await;
                let mut result = RouteUsage::empty(route);
                if route.route_id == "broken" {
                    result.probe = Probe::Failed {
                        kind: FailKind::Network,
                        detail: "request failed".into(),
                    };
                } else {
                    result.windows.push(Window {
                        kind: WindowKind::Weekly,
                        scope: None,
                        used_percent: Some(self.bearer.len() as f64),
                        resets_at: None,
                        limit_reached: false,
                        detail: None,
                    });
                }
                result
            })
        }
    }

    #[tokio::test]
    async fn probe_concurrency_isolation_and_redaction() {
        let sentinel = "SENTINEL_PRIVATE_VALUE";
        let probe = FakeProbe {
            barrier: tokio::sync::Barrier::new(3),
            bearer: sentinel.into(),
        };
        let routes = ["good", "broken", "also-good"].map(|id| UsageRoute {
            route_id: id.into(),
            ..fixture_route("fixture")
        });
        let snapshot = snapshot_with(
            &routes,
            &Locations::none(),
            Arc::new(p1_provider_http::ReqwestTransport::new()),
            &probe,
        )
        .await;
        assert!(matches!(snapshot.routes[1].probe, Probe::Failed { .. }));
        assert!(matches!(snapshot.routes[0].probe, Probe::Supported));
        assert!(matches!(snapshot.routes[2].probe, Probe::Supported));
        assert!(!format!("{snapshot:?}").contains(sentinel));
        assert!(!serde_json::to_string(&snapshot).unwrap().contains(sentinel));
    }
}
