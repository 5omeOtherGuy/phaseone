//! The five plans' usage responses, read into the answer's JSON. A vendor string the ledger
//! shows (a plan, a window's name or scope, a reset instant, a currency) is passed through as
//! the vendor wrote it: the ledger keeps only what has its own shape.

use serde_json::{Map, Value, json};

use crate::timestamp;

/// How far apart a `limits` entry's `resetTime` and a window's `reset_time` may be and still
/// be treated as the same instant. The vendor computes the two independently; the saved Kimi
/// body skews by one second. This is only a candidate test: [`unambiguous_detail`] accepts a
/// detail only when that candidate names exactly one window and no other entry names it.
const DETAIL_MATCH_SECONDS: i64 = 60;

/// One quota window of the answer. `kind` is `session`, `weekly`, `weekly-scoped` or `other`,
/// which `label` names.
struct Window<'a> {
    kind: &'a str,
    label: Option<Value>,
    scope: Option<Value>,
    used_percent: Option<f64>,
    resets_at: Option<Value>,
    limit_reached: bool,
    detail: Option<String>,
}

impl<'a> Window<'a> {
    fn new(kind: &'a str) -> Self {
        Self {
            kind,
            label: None,
            scope: None,
            used_percent: None,
            resets_at: None,
            limit_reached: false,
            detail: None,
        }
    }

    /// An `other` window the vendor names `label`.
    fn other(label: impl Into<Value>) -> Self {
        Self {
            label: Some(label.into()),
            ..Self::new("other")
        }
    }

    fn json(self) -> Value {
        let mut window = Map::new();
        window.insert("kind".into(), self.kind.into());
        let mut put = |key: &str, value: Option<Value>| {
            if let Some(value) = value {
                window.insert(key.into(), value);
            }
        };
        put("label", self.label);
        put("scope", self.scope);
        put("used_percent", self.used_percent.map(Value::from));
        put("resets_at", self.resets_at);
        put("detail", self.detail.map(Value::from));
        window.insert("limit_reached".into(), self.limit_reached.into());
        Value::Object(window)
    }
}

/// The answer: the plan, the windows, the credits and the extra usage, each absent when the
/// response has none.
fn usage(
    plan: Option<Value>,
    windows: Vec<Window>,
    credits: Option<Value>,
    extra_usage: Option<Value>,
) -> Value {
    let mut out = Map::new();
    if let Some(plan) = plan {
        out.insert("plan".into(), plan);
    }
    out.insert(
        "windows".into(),
        Value::Array(windows.into_iter().map(Window::json).collect()),
    );
    if let Some(credits) = credits {
        out.insert("credits".into(), credits);
    }
    if let Some(extra_usage) = extra_usage {
        out.insert("extra_usage".into(), extra_usage);
    }
    Value::Object(out)
}

/// A vendor string as written, or nothing when the value is not a string.
fn text(value: Option<&Value>) -> Option<Value> {
    value
        .and_then(Value::as_str)
        .map(|text| Value::from(text.to_owned()))
}

/// `{"limits": [{"kind", "percent", "resets_at", "limit_reached", "scope"}], "extra_usage"}`.
pub(crate) fn claude(data: &Value) -> Option<Value> {
    let limits = data.get("limits").and_then(Value::as_array)?;
    let mut windows = Vec::new();
    for limit in limits {
        let mut window = match limit.get("kind").and_then(Value::as_str)? {
            "session" => Window::new("session"),
            "weekly_all" => Window::new("weekly"),
            "weekly_scoped" => Window::new("weekly-scoped"),
            // An unknown kind is shown by name only when it looks like one.
            other => Window::other(other),
        };
        window.scope = text(limit.pointer("/scope/model/display_name"));
        window.used_percent = limit.get("percent").and_then(Value::as_f64);
        window.resets_at = text(limit.get("resets_at"));
        window.limit_reached = limit
            .get("limit_reached")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        windows.push(window);
    }
    let extra_usage = data
        .get("extra_usage")
        .filter(|v| v.is_object())
        .map(|extra| {
            json!({
                "enabled": extra.get("is_enabled").and_then(Value::as_bool),
                "used": number(extra.get("used_credits")),
                "limit": number(extra.get("monthly_limit")),
                "currency": text(extra.get("currency")),
            })
        });
    Some(usage(None, windows, None, extra_usage))
}

/// `{"plan_type", "rate_limit": {"primary_window"|"secondary_window": {"used_percent",
/// "limit_window_seconds", "reset_at"}, "limit_reached"}, "credits", "rate_limit_reset_credits"}`.
pub(crate) fn codex(data: &Value) -> Option<Value> {
    let rate = data.get("rate_limit").and_then(Value::as_object)?;
    let reached = rate
        .get("limit_reached")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let mut windows = Vec::new();
    for key in ["primary_window", "secondary_window"] {
        let Some(entry) = rate.get(key).filter(|v| v.is_object()) else {
            continue;
        };
        let seconds = entry.get("limit_window_seconds").and_then(Value::as_u64);
        let mut window = match seconds {
            Some(18_000) => Window::new("session"),
            Some(604_800) => Window::new("weekly"),
            Some(n) => Window::other(format!("{}h", n / 3600)),
            None => Window::other("unknown"),
        };
        window.used_percent = entry.get("used_percent").and_then(Value::as_f64);
        window.resets_at = entry
            .get("reset_at")
            .and_then(Value::as_i64)
            .and_then(timestamp::format)
            .map(Value::from);
        window.limit_reached = reached;
        windows.push(window);
    }
    let balance = number(data.pointer("/credits/balance"));
    let reset_credits = data
        .pointer("/rate_limit_reset_credits/available_count")
        .and_then(Value::as_u64);
    let credits = (balance.is_some() || reset_credits.is_some())
        .then(|| json!({"balance": balance, "reset_credits": reset_credits}));
    Some(usage(text(data.get("plan_type")), windows, credits, None))
}

/// `{"usage": {"rolling"|"weekly"|"monthly": {"percent", "resetsAt", "status"}}}`.
/// `rolling` is the vendor's five-hour window, so it keeps that name; `monthly` is 30 days.
pub(crate) fn opencode_go(data: &Value) -> Option<Value> {
    let entries = data.get("usage").and_then(Value::as_object)?;
    let mut windows = Vec::new();
    for key in ["rolling", "weekly", "monthly"] {
        let Some(entry) = entries.get(key).and_then(Value::as_object) else {
            continue;
        };
        let mut window = match key {
            "weekly" => Window::new("weekly"),
            "monthly" => Window::other("30d"),
            _ => Window::other("rolling"),
        };
        // A window the vendor marks rate-limited is exhausted whatever percent it reports,
        // and the ledger shows the full bar the vendor's own UI shows.
        let rate_limited = entry.get("status").and_then(Value::as_str) == Some("rate-limited");
        window.used_percent = if rate_limited {
            Some(100.0)
        } else {
            entry.get("percent").and_then(Value::as_f64)
        };
        window.resets_at = text(entry.get("resetsAt"));
        window.limit_reached = rate_limited;
        windows.push(window);
    }
    Some(usage(None, windows, None, None))
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
pub(crate) fn kimi(data: &Value) -> Option<Value> {
    let entries = data.get("usages").and_then(Value::as_object)?;
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
                (count(detail.get("remaining")), count(detail.get("limit")))
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
    let keys = ["limit_5h", "limit_month_total", "limit_month_code"];
    // Every present window's reset instant is needed before any detail is placed: a month
    // window can share another's reset, and a 5h reset can coincide with a month's.
    let mut window_resets: Vec<Option<i64>> = Vec::new();
    let mut five_hour: Option<i64> = None;
    for key in keys {
        let Some(entry) = entries.get(key).and_then(Value::as_object) else {
            continue;
        };
        let at = entry
            .get("reset_time")
            .and_then(Value::as_str)
            .and_then(timestamp::parse);
        if key == "limit_5h" {
            five_hour = at;
        }
        window_resets.push(at);
    }
    let five_hour_detail = unambiguous_detail(&window_resets, five_hour, &details);
    let mut windows = Vec::new();
    for key in keys {
        let Some(entry) = entries.get(key).and_then(Value::as_object) else {
            continue;
        };
        let mut window = match key {
            "limit_5h" => Window::new("session"),
            "limit_month_total" => Window::other("month"),
            _ => Window::other("month code"),
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
        window.used_percent = detail.and_then(|detail| detail.used_percent).or(used_ratio);
        window.resets_at = text(entry.get("reset_time"));
        window.limit_reached = exhausted(window.used_percent);
        window.detail = detail.map(|detail| detail.text.clone());
        windows.push(window);
    }
    Some(usage(None, windows, None, None))
}

/// `{"data": {"limits": [{"type": "CREDIT_LIMIT", "unit", "number", "percentage",
/// "nextResetTime"}]}}`. Only `CREDIT_LIMIT` entries are quota windows; `(3, 5)` is the
/// five-hour window and `(6, 1)` the seven-day one, and any other pair keeps its identity
/// rather than being forced into a window it may not be. `"success": false` is the vendor
/// refusing the credential, whatever else the body says.
pub(crate) fn glm(data: &Value) -> Option<Value> {
    if data.get("success").and_then(Value::as_bool) == Some(false) {
        return Some(json!({"credential_refused": true}));
    }
    let limits = data.pointer("/data/limits").and_then(Value::as_array)?;
    let mut windows = Vec::new();
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
        let mut window = match (unit, number) {
            (3, 5) => Window::new("session"),
            (6, 1) => Window::new("weekly"),
            _ => Window::other(format!("u{unit}n{number}")),
        };
        window.used_percent = limit.get("percentage").and_then(Value::as_f64);
        window.resets_at = epoch_millis(limit.get("nextResetTime"))
            .and_then(|millis| timestamp::format(millis.div_euclid(1000)))
            .map(Value::from);
        window.limit_reached = exhausted(window.used_percent);
        windows.push(window);
    }
    Some(usage(None, windows, None, None))
}

/// A vendor count that is a string in one response and a number in the next, as text. Only
/// a short finite number is kept: this text is shown, so it must never be an echoed value.
fn count(value: Option<&Value>) -> Option<String> {
    let text = match value? {
        Value::String(text) => text.trim().to_string(),
        Value::Number(number) => number.to_string(),
        _ => return None,
    };
    (text.len() <= 20 && text.parse::<f64>().is_ok_and(f64::is_finite)).then_some(text)
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

/// Epoch milliseconds, as the GLM quota endpoint reports reset times.
fn epoch_millis(value: Option<&Value>) -> Option<i64> {
    match value? {
        Value::Number(number) => number.as_i64(),
        Value::String(text) => text.parse().ok(),
        _ => None,
    }
}

/// A `limits` entry's request-count detail: the reset instant it names (seconds since the
/// epoch), the `N/M left` text, and the used share the count implies.
struct Detail {
    at: i64,
    text: String,
    used_percent: Option<f64>,
}

/// Whether a `limits` entry's reset instant names this window, i.e. the two are within
/// [`DETAIL_MATCH_SECONDS`].
fn within(at: i64, window: Option<i64>) -> bool {
    window.is_some_and(|window| (at - window).abs() <= DETAIL_MATCH_SECONDS)
}

/// The 5h request detail, only when the mapping is unambiguous: exactly one `limits` entry
/// names the 5h window, and that entry names no other window. Month windows can share a
/// reset instant and a 5h reset can coincide with a month's, so an ambiguous entry is
/// dropped rather than guessed at.
fn unambiguous_detail<'a>(
    windows: &[Option<i64>],
    five_hour: Option<i64>,
    details: &'a [Detail],
) -> Option<&'a Detail> {
    let five_hour = five_hour?;
    let naming_5h: Vec<&Detail> = details
        .iter()
        .filter(|entry| within(entry.at, Some(five_hour)))
        .collect();
    let [entry] = naming_5h.as_slice() else {
        return None;
    };
    let named = windows
        .iter()
        .filter(|window| within(entry.at, **window))
        .count();
    (named == 1).then_some(*entry)
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
