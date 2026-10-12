//! The subscription connectors' usage probes (ADR-0214): each plan's response, decoded by the
//! connectors' own decoder and kept by p1's usage ledger exactly as a session keeps it. The
//! decoders run natively here, the same functions the `usage-*` components export; the
//! ledger's own rules (origins, credentials, echoes) are `p1-usage`'s suites.

use p1_subscription_usage::{Claude, Codex, Glm, Kimi, OpencodeGo, UsageProbe};
use p1_usage::{FailKind, Probe, ProbeModule, RouteUsage, Snapshot, UsageRoute, WindowKind};

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

/// A connector probe called natively, as the ledger calls a component.
struct Native<'a>(&'a (dyn UsageProbe + Sync));

impl ProbeModule for Native<'_> {
    fn describe(&self) -> Result<String, String> {
        Ok(self.0.describe())
    }

    fn decode(&self, body: &[u8]) -> Result<String, String> {
        self.0.decode(body)
    }

    fn token_origins(&self) -> &[String] {
        &[]
    }
}

fn fixture_route(id: &str) -> UsageRoute {
    UsageRoute {
        route_id: id.into(),
        label: id.into(),
        credential: "test source".into(),
        store_id: None,
        probe: None,
        compiled_origins: None,
        spec: p1_auth::CredentialSpec {
            kind: p1_auth::CredentialKind::API_KEY,
            env: None,
            borrow: vec![],
            store_only: false,
            login_dir: None,
        },
    }
}

/// `body` as the ledger keeps it after `probe` decoded it.
fn decoded(probe: &(dyn UsageProbe + Sync), route: &str, body: &[u8]) -> RouteUsage {
    p1_usage::decode_usage(&fixture_route(route), &Native(probe), body)
}

/// `body`, which the probe must read.
fn parsed(probe: &(dyn UsageProbe + Sync), route: &str, body: &[u8]) -> RouteUsage {
    let usage = decoded(probe, route, body);
    assert!(
        matches!(usage.probe, Probe::Supported),
        "fixture body parses: {:?}",
        usage.probe
    );
    usage
}

fn close(actual: Option<f64>, expected: f64) {
    let actual = actual.expect("a percentage");
    assert!((actual - expected).abs() < 1e-9, "{actual} != {expected}");
}

/// Each probe describes its endpoint at the origin its component's manifest declares
/// (`components/usage-*/Cargo.toml`), and takes the credential method of its plan.
#[test]
fn each_probe_describes_its_endpoint_and_method() {
    let probes: [(&(dyn UsageProbe + Sync), &str, &str, &str); 5] = [
        (
            &Claude,
            "usage-claude",
            "https://api.anthropic.com/",
            "claude-code-oauth",
        ),
        (&Codex, "usage-codex", "https://chatgpt.com/", "codex-oauth"),
        (
            &OpencodeGo,
            "usage-opencode-go",
            "https://opencode.ai/",
            "api-key",
        ),
        (&Kimi, "usage-kimi", "https://api.kimi.ai/", "api-key"),
        (&Glm, "usage-glm", "https://api.z.ai/", "api-key"),
    ];
    let components = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../components");
    for (probe, component, origin, method) in probes {
        let description: serde_json::Value = serde_json::from_str(&probe.describe()).unwrap();
        let url = description["url"].as_str().unwrap();
        assert!(url.starts_with(origin), "{component}: {url}");
        assert_eq!(description["method"], method, "{component}");
        let manifest =
            std::fs::read_to_string(components.join(component).join("Cargo.toml")).unwrap();
        let declared = format!("token_origins = [\"{}\"]", origin.trim_end_matches('/'));
        assert!(manifest.contains(&declared), "{component}: {declared}");
    }
}

#[test]
fn probe_fixtures() {
    let claude = parsed(
        &Claude,
        "anthropic-subscription",
        br#"{"limits":[{"kind":"session","percent":67,"resets_at":"2026-09-22T23:00:00Z"},{"kind":"weekly_scoped","percent":7,"scope":{"model":{"display_name":"Fable"}}}],"extra_usage":{"is_enabled":true,"used_credits":12.4,"monthly_limit":5000,"currency":"EUR"}}"#,
    );
    assert!(matches!(claude.windows[1].kind, WindowKind::WeeklyScoped));
    assert_eq!(claude.windows[1].scope.as_deref(), Some("Fable"));
    assert_eq!(claude.extra_usage.as_ref().unwrap().used, Some(12.4));
    let codex = parsed(
        &Codex,
        "openai-codex-subscription",
        br#"{"plan_type":"prolite","rate_limit":{"primary_window":{"used_percent":69,"limit_window_seconds":604800,"reset_at":1790451360},"limit_reached":true},"credits":{"balance":0},"rate_limit_reset_credits":{"available_count":1}}"#,
    );
    assert_eq!(codex.windows.len(), 1);
    assert!(codex.windows[0].limit_reached);
    assert!(matches!(codex.windows[0].kind, WindowKind::Weekly));
    assert_eq!(
        codex.windows[0].resets_at.as_deref(),
        Some("2026-09-26T19:36:00Z")
    );
    assert_eq!(codex.plan.as_deref(), Some("prolite"));
    assert_eq!(codex.credits.as_ref().unwrap().reset_credits, Some(1));
}
#[test]
fn opencode_go_windows() {
    let go = parsed(&OpencodeGo, "opencode-go-subscription", OPENCODE_GO_BODY);
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
        &OpencodeGo,
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
    let kimi = parsed(&Kimi, "kimi-coding-subscription", KIMI_BODY);
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
        let kimi = parsed(&Kimi, "kimi-coding-subscription", body);
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
    let kimi = parsed(&Kimi, "kimi-coding-subscription", body);
    let snapshot = Snapshot {
        taken_at: "2026-09-24T00:06:00Z".into(),
        routes: vec![kimi],
    };
    let grid = 32;
    let lines = p1_usage::render(&snapshot, grid);
    let texts: Vec<String> = lines.iter().map(|line| line.text()).collect();
    assert!(
        texts
            .iter()
            .any(|t| t.starts_with("5h") && t.contains("15/100 left")),
        "{texts:#?}"
    );
    let bars: Vec<&p1_usage::Line> = lines
        .iter()
        .filter(|line| line.0.iter().any(|span| span.tone == p1_usage::Tone::Rule))
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
        &Kimi,
        "kimi-coding-subscription",
        br#"{"usages":{"limit_5h":{"reset_time":"2026-09-24T00:05:01Z"}}}"#,
    );
    assert_eq!(kimi.windows[0].used_percent, None);
    assert_eq!(kimi.windows[0].detail, None);
    let snapshot = Snapshot {
        taken_at: "2026-09-24T00:06:00Z".into(),
        routes: vec![kimi],
    };
    let lines = p1_usage::render(&snapshot, 32);
    let texts: Vec<String> = lines.iter().map(|line| line.text()).collect();
    assert!(
        texts
            .iter()
            .any(|t| t.starts_with("5h") && t.contains("— · resets")),
        "{texts:#?}"
    );
    assert!(
        !lines
            .iter()
            .any(|line| { line.0.iter().any(|span| span.tone == p1_usage::Tone::Rule) }),
        "{texts:#?}"
    );
}

/// A `limits` entry names its window by the reset instant in its detail, never by array
/// position: the 5h detail sits behind a month detail and a far-away unrelated entry.
/// Only the 5h window carries the text; the month window is not given one.
#[test]
fn kimi_detail_follows_the_reset_instant_not_the_array_position() {
    let kimi = parsed(
        &Kimi,
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
        &Kimi,
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
        &Kimi,
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
        &Kimi,
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
        &Kimi,
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
        &Kimi,
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
        &Glm,
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
        &Kimi,
        "kimi-coding-subscription",
        br#"{"usages":{"limit_5h":{"reset_time":"2026-09-24T00:05:01Z"}}}"#,
    );
    assert_eq!(kimi.windows[0].used_percent, None);
    assert!(!kimi.windows[0].limit_reached);
    let glm = parsed(
        &Glm,
        "glm-subscription",
        br#"{"data":{"limits":[{"type":"CREDIT_LIMIT","unit":3,"number":5,"nextResetTime":1758700800000}]}}"#,
    );
    assert_eq!(glm.windows[0].used_percent, None);
    assert!(!glm.windows[0].limit_reached);
}

#[test]
fn kimi_without_a_limits_detail_has_no_value_text() {
    let kimi = parsed(
        &Kimi,
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
    let glm = parsed(&Glm, "glm-subscription", GLM_BODY);
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

#[test]
fn glm_success_false_is_a_fixed_credential_failure_even_with_limits() {
    for body in [
        br#"{"success":false,"message":"SENTINEL_PRIVATE_VALUE"}"#.as_slice(),
        br#"{"success":false,"data":{"limits":[{"type":"CREDIT_LIMIT","unit":3,"number":5,"percentage":5}]},"message":"SENTINEL_PRIVATE_VALUE"}"#.as_slice(),
    ] {
        let result = decoded(&Glm, "glm-subscription", body);
        assert!(matches!(result.probe, Probe::Failed {
            kind: FailKind::Credential,
            ref detail,
        } if detail == "usage credential refused"));
        assert!(result.windows.is_empty());
        assert!(!serde_json::to_string(&result).unwrap().contains("SENTINEL_PRIVATE_VALUE"));
    }
}

#[test]
fn malformed_bodies_are_parse_failures() {
    let probes: [(&(dyn UsageProbe + Sync), &str); 5] = [
        (&Claude, "anthropic-subscription"),
        (&Codex, "openai-codex-subscription"),
        (&OpencodeGo, "opencode-go-subscription"),
        (&Kimi, "kimi-coding-subscription"),
        (&Glm, "glm-subscription"),
    ];
    for (probe, route) in probes {
        for body in [&b"not json"[..], br#"{"unexpected":true}"#] {
            let out = decoded(probe, route, body);
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
            assert!(out.windows.is_empty());
        }
    }
}

/// Review #484: a vendor string that is not shaped like what it claims to be is dropped by
/// the ledger, whatever the decoder passed through.
#[test]
fn vendor_strings_keep_only_their_own_shape() {
    let shaped = format!("Kx{}", "9aQ".repeat(10));
    let claude = parsed(
        &Claude,
        "anthropic-subscription",
        format!(
            r#"{{"limits":[{{"kind":"{shaped}","percent":5,"resets_at":"tomorrow","scope":{{"model":{{"display_name":"{shaped}"}}}}}}],"extra_usage":{{"is_enabled":true,"currency":"EURO"}}}}"#
        )
        .as_bytes(),
    );
    assert!(matches!(&claude.windows[0].kind, WindowKind::Other(name) if name == "other"));
    assert_eq!(claude.windows[0].scope, None);
    assert_eq!(claude.windows[0].resets_at, None);
    assert_eq!(claude.extra_usage.as_ref().unwrap().currency, None);
}
