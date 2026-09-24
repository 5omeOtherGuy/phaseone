use crate::timestamp;
use unicode_width::UnicodeWidthStr;

use crate::{FailKind, Probe, RouteUsage, Snapshot, Window, WindowKind};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tone {
    Ink,
    Dim,
    Faint,
    Rule,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Span {
    pub text: String,
    pub tone: Tone,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Line(pub Vec<Span>);

impl Line {
    pub fn text(&self) -> String {
        self.0.iter().map(|span| span.text.as_str()).collect()
    }
}

fn width(s: &str) -> usize {
    UnicodeWidthStr::width(s)
}
fn clip(text: &str, max: usize) -> String {
    let mut result = String::new();
    for ch in text.chars() {
        if width(&result) + unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0) > max {
            break;
        }
        result.push(ch);
    }
    result
}
fn row(label: &str, value: &str, label_tone: Tone, grid: usize) -> Line {
    let value = clip(value, grid);
    let label = clip(label, grid.saturating_sub(width(&value) + 1));
    let padding = grid.saturating_sub(width(&label) + width(&value));
    Line(vec![
        Span {
            text: label,
            tone: label_tone,
        },
        Span {
            text: " ".repeat(padding),
            tone: Tone::Dim,
        },
        Span {
            text: value,
            tone: Tone::Ink,
        },
    ])
}
fn single(text: &str, tone: Tone, grid: usize) -> Line {
    Line(vec![Span {
        text: clip(text, grid),
        tone,
    }])
}
fn percent(value: Option<f64>) -> String {
    match value {
        Some(value) if value.is_finite() => format!("{value:.0}%"),
        _ => "—".into(),
    }
}
fn amount(value: Option<f64>) -> String {
    match value {
        Some(value) if value.is_finite() => {
            if value.fract() == 0.0 {
                format!("{value:.0}")
            } else {
                format!("{value:.2}")
            }
        }
        _ => "—".into(),
    }
}
fn reset(at: Option<&str>, taken: &str) -> String {
    let (Some(at), Some(now)) = (at, timestamp::parse(taken)) else {
        return "—".into();
    };
    let Some(end) = timestamp::parse(at) else {
        return "—".into();
    };
    let secs = (end - now).whole_seconds();
    if secs <= 0 {
        return "due".into();
    }
    let mins = (secs + 30) / 60;
    let hours = mins / 60;
    let days = hours / 24;
    if days > 0 {
        format!("{days}d {}h", hours % 24)
    } else if hours > 0 {
        format!("{hours}h{}m", mins % 60)
    } else {
        format!("{mins}m")
    }
}
fn window_label(window: &Window) -> String {
    let label = match &window.kind {
        WindowKind::Session => "5h".into(),
        WindowKind::Weekly => "7d".into(),
        WindowKind::WeeklyScoped => format!(
            "7d {}",
            window.scope.as_deref().unwrap_or("—").to_lowercase()
        ),
        WindowKind::Other(label) => label.clone(),
    };
    if window.limit_reached {
        format!("!{label}")
    } else {
        label
    }
}
fn bar(window: &Window, grid: usize) -> Line {
    let cells = grid.saturating_sub(6);
    let filled = window
        .used_percent
        .filter(|v| v.is_finite())
        .map(|v| ((v.clamp(0.0, 100.0) / 100.0) * cells as f64).round() as usize)
        .unwrap_or(0);
    let pct = percent(window.used_percent);
    Line(vec![
        Span {
            text: "█".repeat(filled),
            tone: Tone::Ink,
        },
        Span {
            text: "█".repeat(cells - filled),
            tone: Tone::Rule,
        },
        Span {
            text: " ".repeat(grid.saturating_sub(cells + width(&pct))),
            tone: Tone::Dim,
        },
        Span {
            text: pct,
            tone: Tone::Ink,
        },
    ])
}
fn priority(route: &RouteUsage) -> u8 {
    match route.probe {
        Probe::Supported => 0,
        Probe::Failed { .. } => 1,
        Probe::Unsupported { .. } => 2,
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Density {
    /// A blank line between routes.
    Separated,
    /// No blank lines.
    Compact,
    /// No blank lines, and paired credit rows merged onto one row.
    Dense,
}

struct Rendered {
    lines: Vec<Line>,
    /// `(start, end, label)` of every route block, in render order.
    spans: Vec<(usize, usize, String)>,
}

/// Pure, palette-independent ledger on a `grid`-column content grid.
pub fn render(snapshot: &Snapshot, grid: usize) -> Vec<Line> {
    render_parts(snapshot, grid, Density::Separated).lines
}

fn render_parts(snapshot: &Snapshot, grid: usize, density: Density) -> Rendered {
    let mut routes: Vec<&RouteUsage> = snapshot.routes.iter().collect();
    routes.sort_by(|a, b| {
        priority(a)
            .cmp(&priority(b))
            .then_with(|| {
                let max = |r: &RouteUsage| {
                    r.windows
                        .iter()
                        .filter_map(|w| w.used_percent)
                        .filter(|v| v.is_finite())
                        .fold(0.0_f64, f64::max)
                };
                max(b).total_cmp(&max(a))
            })
            .then_with(|| a.route_id.cmp(&b.route_id))
    });
    let mut lines = Vec::new();
    let mut spans = Vec::new();
    for route in routes {
        if density == Density::Separated && !lines.is_empty() {
            lines.push(Line(vec![]));
        }
        let start = lines.len();
        let state = route_state(route, &snapshot.taken_at);
        lines.push(row(&route.label.to_uppercase(), &state, Tone::Dim, grid));
        match &route.probe {
            Probe::Unsupported { .. } => lines.push(single("no usage endpoint", Tone::Faint, grid)),
            Probe::Failed { kind, detail } => {
                let label = match kind {
                    FailKind::Credential => "no access".to_string(),
                    _ => format!("error · {detail}"),
                };
                lines.push(single(&label, Tone::Dim, grid));
            }
            Probe::Supported => {
                if route.windows.is_empty()
                    && route.credits.is_none()
                    && route.extra_usage.is_none()
                {
                    lines.push(row("usage", "—", Tone::Dim, grid));
                }
                for window in &route.windows {
                    let label = window_label(window);
                    let time = reset(window.resets_at.as_deref(), &snapshot.taken_at);
                    // A vendor detail says more than the percentage does; the bar below
                    // still carries the percentage.
                    let head = window
                        .detail
                        .clone()
                        .unwrap_or_else(|| percent(window.used_percent));
                    let value = if time == "—" {
                        format!("{head} · resets —")
                    } else if time == "due" {
                        format!("{head} · resets due")
                    } else {
                        format!("{head} · resets {time}")
                    };
                    lines.push(row(
                        &label,
                        &value,
                        if window.limit_reached {
                            Tone::Ink
                        } else {
                            Tone::Dim
                        },
                        grid,
                    ));
                    // The bar is the used fraction, so it is drawn only when that fraction is
                    // known: an empty bar beside the unknown marker would read as `0 % used`
                    // and contradict it.
                    if window.used_percent.is_some_and(f64::is_finite) {
                        lines.push(bar(window, grid));
                    }
                }
                if let Some(credits) = &route.credits {
                    let currency = credits.currency.as_deref().unwrap_or("");
                    if let (Some(used), Some(limit)) = (credits.used, credits.limit) {
                        lines.push(row(
                            "credits used",
                            &format!(
                                "{} / {} {currency}",
                                amount(Some(used)),
                                amount(Some(limit))
                            ),
                            Tone::Dim,
                            grid,
                        ));
                        if let Some(balance) = credits.balance {
                            lines.push(row(
                                "credits left",
                                &format!("{} {currency}", amount(Some(balance))),
                                Tone::Dim,
                                grid,
                            ));
                        }
                    } else if density == Density::Dense {
                        let reset = credits
                            .reset_credits
                            .map(|value| value.to_string())
                            .unwrap_or_else(|| "—".into());
                        lines.push(row(
                            "credits",
                            &format!("{} · reset {reset}", amount(credits.balance)),
                            Tone::Dim,
                            grid,
                        ));
                    } else {
                        lines.push(row("credits", &amount(credits.balance), Tone::Dim, grid));
                        if let Some(reset) = credits.reset_credits {
                            lines.push(row("reset credits", &reset.to_string(), Tone::Dim, grid));
                        }
                    }
                }
                if let Some(extra) = &route.extra_usage {
                    let value = if extra.enabled == Some(false) {
                        "disabled".to_string()
                    } else {
                        format!(
                            "{} / {} {}",
                            amount(extra.used),
                            amount(extra.limit),
                            extra.currency.as_deref().unwrap_or("—")
                        )
                    };
                    lines.push(row("extra usage", &value, Tone::Dim, grid));
                }
            }
        }
        spans.push((start, lines.len(), route.label.to_uppercase()));
    }
    Rendered { lines, spans }
}

/// The route header's value: the plan (or the probe state) plus this route's data age. A
/// retained last-good value says `stale`; a route that never produced data says `no data`.
fn route_state(route: &RouteUsage, taken_at: &str) -> String {
    let plan = match &route.probe {
        // A provider that reports usage but no plan name: the value is unknown, so it
        // renders as `—` (SPEC §5), never as a word that reads like a probe state.
        Probe::Supported => route.plan.as_deref().unwrap_or("—"),
        Probe::Unsupported { .. } => "unknown",
        Probe::Failed { .. } => "error",
    };
    match freshness(
        taken_at,
        route.observed_at.as_deref(),
        route.stale.as_deref(),
    ) {
        Some(age) => format!("{plan} · {age}"),
        None if matches!(route.probe, Probe::Failed { .. }) => "error · no data".to_string(),
        None => plan.to_string(),
    }
}

/// `None` when the route has never produced data; otherwise its age at the frame's time,
/// marked `stale` when the values are a retained last-good snapshot.
fn freshness(taken_at: &str, observed_at: Option<&str>, stale: Option<&str>) -> Option<String> {
    let at = timestamp::parse(observed_at?)?;
    let now = timestamp::parse(taken_at)?;
    let age = age_label((now - at).whole_seconds().max(0));
    Some(match stale {
        Some(reason) => format!("stale {age} ({reason})"),
        None => age,
    })
}

fn age_label(seconds: i64) -> String {
    if seconds < 60 {
        format!("{seconds}s")
    } else if seconds < 3600 {
        format!("{}m", seconds / 60)
    } else if seconds < 86_400 {
        format!("{}h{}m", seconds / 3600, (seconds % 3600) / 60)
    } else {
        format!("{}d{}h", seconds / 86_400, (seconds % 86_400) / 3600)
    }
}

/// The live pane's status row: the build hash of the binary actually running and the
/// snapshot time. A full sha256 is shown when the grid can hold it; otherwise it is a
/// clearly-marked 16-hex prefix. The hash makes a stale binary visible; the time makes a
/// stale frame visible. It is clipped like any other row.
pub fn status_line(build: &str, taken_at: &str, grid: usize) -> Line {
    let time = taken_at.get(11..16).unwrap_or(taken_at);
    let full = format!("sha256 {build} · {time}Z");
    if width(&full) <= grid {
        return single(&full, Tone::Faint, grid);
    }
    let prefix: String = build.chars().take(16).collect();
    single(&format!("sha256 {prefix}… · {time}Z"), Tone::Faint, grid)
}

/// Render for a viewport of `height` lines. The separated form is tried first, then the
/// compact form without blank separators, then a dense form that merges paired credit rows;
/// only if none fits is the tail clipped. The clipped footer names every hidden route, so a
/// provider never disappears silently. `height == 0` means no viewport is known and the full
/// ledger is returned.
pub fn render_fitted(snapshot: &Snapshot, grid: usize, height: usize) -> Vec<Line> {
    if height == 0 {
        return render(snapshot, grid);
    }
    for density in [Density::Separated, Density::Compact, Density::Dense] {
        let rendered = render_parts(snapshot, grid, density);
        if rendered.lines.len() <= height {
            return rendered.lines;
        }
    }
    let rendered = render_parts(snapshot, grid, Density::Dense);
    let total = rendered.lines.len();
    let mut shown = height.saturating_sub(1).min(total);
    let mut footer;
    loop {
        let hidden: Vec<String> = rendered
            .spans
            .iter()
            .filter(|(_, end, _)| *end > shown)
            .map(|(_, _, label)| label.clone())
            .collect();
        footer = hidden_footer(&hidden, grid, total - shown);
        if footer.len() > height {
            footer.truncate(height);
        }
        if shown + footer.len() <= height {
            break;
        }
        let next = height.saturating_sub(footer.len());
        if next >= shown {
            break;
        }
        shown = next;
    }
    let mut lines = rendered.lines[..shown].to_vec();
    lines.extend(footer);
    lines
}

/// The clipped tail's footer: every hidden route named, wrapped at route boundaries, or a
/// plain line count when only blank separators were dropped.
fn hidden_footer(names: &[String], grid: usize, hidden_lines: usize) -> Vec<Line> {
    if names.is_empty() {
        return vec![single(
            &format!("+{hidden_lines} more lines"),
            Tone::Faint,
            grid,
        )];
    }
    let text = format!("hidden: {}", names.join(", "));
    let mut lines = Vec::new();
    let mut current = String::new();
    for token in text.split(", ") {
        let candidate = if current.is_empty() {
            token.to_string()
        } else {
            format!("{current}, {token}")
        };
        if !current.is_empty() && width(&candidate) > grid {
            lines.push(single(&current, Tone::Faint, grid));
            current = token.to_string();
        } else {
            current = candidate;
        }
    }
    if !current.is_empty() {
        lines.push(single(&current, Tone::Faint, grid));
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Credits, ExtraUsage};

    fn route(id: &str, probe: Probe, windows: Vec<Window>) -> RouteUsage {
        RouteUsage {
            route_id: id.into(),
            label: id.into(),
            credential: "fixture source".into(),
            probe,
            plan: None,
            windows,
            credits: None,
            extra_usage: None,
            observed_at: None,
            stale: None,
        }
    }
    fn window(pct: Option<f64>, reached: bool) -> Window {
        Window {
            kind: WindowKind::Session,
            scope: None,
            used_percent: pct,
            resets_at: Some("2026-09-22T23:00:00Z".into()),
            limit_reached: reached,
            detail: None,
        }
    }

    /// The real 7-provider ledger shape (42 compact lines): the six p1 routes plus the
    /// OpenRouter credits row, with the live window counts.
    fn seven_providers() -> Snapshot {
        let mut claude = route(
            "claude max",
            Probe::Supported,
            vec![
                window(Some(97.0), false),
                window(Some(78.0), false),
                window(Some(7.0), false),
            ],
        );
        claude.extra_usage = Some(ExtraUsage {
            enabled: Some(false),
            used: Some(0.0),
            limit: Some(5000.0),
            currency: Some("EUR".into()),
        });
        let glm = route(
            "glm",
            Probe::Supported,
            vec![window(Some(0.0), false), window(Some(100.0), true)],
        );
        let kimi = route(
            "kimi coding",
            Probe::Supported,
            vec![
                // The live 5h row: the vendor reports the request count left (`15/100`), and
                // the bar is the used share that count implies (85 %).
                Window {
                    used_percent: Some(85.0),
                    detail: Some("15/100 left".into()),
                    ..window(None, false)
                },
                window(Some(28.0), false),
                window(Some(0.0), false),
            ],
        );
        let mut codex = route(
            "chatgpt pro lite",
            Probe::Supported,
            vec![window(Some(19.0), false)],
        );
        codex.credits = Some(Credits {
            balance: Some(0.0),
            reset_credits: Some(0),
            used: None,
            limit: None,
            currency: None,
        });
        let go2 = route(
            "opencode go-2",
            Probe::Supported,
            vec![
                window(Some(0.0), false),
                window(Some(100.0), true),
                window(Some(58.0), false),
            ],
        );
        let go = route(
            "opencode go",
            Probe::Supported,
            vec![
                window(Some(20.0), false),
                window(Some(27.0), false),
                window(Some(13.0), false),
            ],
        );
        let mut openrouter = route("openrouter credits", Probe::Supported, vec![]);
        openrouter.credits = Some(Credits {
            balance: Some(17.46),
            reset_credits: None,
            used: Some(12.54),
            limit: Some(30.0),
            currency: Some("USD".into()),
        });
        Snapshot {
            taken_at: "2026-09-24T06:14:00Z".into(),
            routes: vec![claude, glm, kimi, codex, go2, go, openrouter],
        }
    }
    /// The four API-key routes now report windows, in the shapes the probes produce.
    fn api_key_routes() -> Vec<RouteUsage> {
        let go = route(
            "opencode go",
            Probe::Supported,
            vec![
                Window {
                    kind: WindowKind::Other("rolling".into()),
                    used_percent: Some(0.0),
                    resets_at: Some("2026-09-24T00:51:16Z".into()),
                    ..window(None, false)
                },
                Window {
                    kind: WindowKind::Weekly,
                    used_percent: Some(100.0),
                    resets_at: Some("2026-09-28T00:00:00Z".into()),
                    limit_reached: true,
                    ..window(None, false)
                },
                Window {
                    kind: WindowKind::Other("30d".into()),
                    used_percent: Some(58.0),
                    resets_at: Some("2026-10-20T18:12:09Z".into()),
                    ..window(None, false)
                },
            ],
        );
        let go2 = route(
            "opencode go-2",
            Probe::Supported,
            vec![
                Window {
                    kind: WindowKind::Other("rolling".into()),
                    used_percent: Some(12.0),
                    resets_at: Some("2026-09-23T05:00:00Z".into()),
                    ..window(None, false)
                },
                Window {
                    kind: WindowKind::Weekly,
                    used_percent: Some(100.0),
                    resets_at: Some("2026-09-28T00:00:00Z".into()),
                    limit_reached: true,
                    ..window(None, false)
                },
                Window {
                    kind: WindowKind::Other("30d".into()),
                    used_percent: Some(5.0),
                    resets_at: Some("2026-10-20T18:12:09Z".into()),
                    ..window(None, false)
                },
            ],
        );
        let kimi = route(
            "kimi coding",
            Probe::Supported,
            vec![
                Window {
                    used_percent: Some(13.0),
                    resets_at: Some("2026-09-24T00:05:01Z".into()),
                    detail: Some("87/100 left".into()),
                    ..window(None, false)
                },
                Window {
                    kind: WindowKind::Other("month".into()),
                    used_percent: Some(20.0),
                    resets_at: Some("2026-10-19T00:00:00Z".into()),
                    ..window(None, false)
                },
                Window {
                    kind: WindowKind::Other("month code".into()),
                    used_percent: Some(0.0),
                    resets_at: Some("2026-10-19T00:00:00Z".into()),
                    ..window(None, false)
                },
            ],
        );
        let glm = route(
            "glm",
            Probe::Supported,
            vec![
                Window {
                    used_percent: Some(0.0),
                    resets_at: Some("2026-09-23T02:00:00Z".into()),
                    ..window(None, false)
                },
                Window {
                    kind: WindowKind::Weekly,
                    used_percent: Some(100.0),
                    resets_at: Some("2026-09-28T12:00:00Z".into()),
                    limit_reached: true,
                    ..window(None, false)
                },
                Window {
                    kind: WindowKind::Other("u9n2".into()),
                    used_percent: Some(42.0),
                    resets_at: Some("2026-10-01T00:00:00Z".into()),
                    ..window(None, false)
                },
            ],
        );
        vec![go, go2, kimi, glm]
    }
    fn sample() -> Snapshot {
        let mut claude = route(
            "claude max",
            Probe::Supported,
            vec![
                window(Some(67.0), false),
                Window {
                    kind: WindowKind::WeeklyScoped,
                    scope: Some("Fable".into()),
                    used_percent: Some(7.0),
                    resets_at: Some("2026-09-24T16:36:00Z".into()),
                    limit_reached: false,
                    detail: None,
                },
            ],
        );
        claude.plan = Some("max".into());
        claude.extra_usage = Some(ExtraUsage {
            enabled: Some(true),
            used: Some(0.0),
            limit: Some(5000.0),
            currency: Some("EUR".into()),
        });
        let mut chatgpt = route(
            "chatgpt pro lite",
            Probe::Supported,
            vec![Window {
                kind: WindowKind::Weekly,
                scope: None,
                used_percent: Some(69.0),
                resets_at: Some("2026-09-26T11:36:00Z".into()),
                limit_reached: false,
                detail: None,
            }],
        );
        chatgpt.plan = Some("prolite".into());
        chatgpt.credits = Some(Credits {
            balance: Some(0.0),
            reset_credits: Some(1),
            used: None,
            limit: None,
            currency: None,
        });
        // Ranking is by percentage, not input order or label.
        let mut routes = api_key_routes();
        routes.push(claude);
        routes.push(chatgpt);
        routes.push(route(
            "failed",
            Probe::Failed {
                kind: FailKind::Http(500),
                detail: "HTTP 500".into(),
            },
            vec![],
        ));
        routes.push(route(
            "key refused",
            Probe::Failed {
                kind: FailKind::Credential,
                detail: "HTTP 401".into(),
            },
            vec![],
        ));
        Snapshot {
            taken_at: "2026-09-22T22:36:00Z".into(),
            routes,
        }
    }
    fn check(grid: usize) {
        let lines = render(&sample(), grid);
        for line in &lines {
            assert!(width(&line.text()) <= grid, "{:?}", line.text());
            assert!(
                !line
                    .text()
                    .chars()
                    .any(|ch| ('\u{2500}'..='\u{257f}').contains(&ch))
            );
        }
        let text = lines.iter().map(Line::text).collect::<Vec<_>>().join("\n");
        let expected = if grid == 32 {
            include_str!("../tests/golden32.txt")
        } else {
            include_str!("../tests/golden48.txt")
        };
        let expected = expected.trim_end_matches('\n');
        assert_eq!(text, expected);
        // Every window renders one bar, filled to its own fraction of the grid.
        let fills: Vec<usize> = lines
            .iter()
            .filter(|line| line.0.iter().any(|span| span.tone == Tone::Rule))
            .map(|line| line.0[0].text.chars().count())
            .collect();
        assert_eq!(fills.len(), 15);
        for fraction in [0.0, 0.12, 0.13, 0.42, 0.69, 1.0] {
            let filled = ((grid - 6) as f64 * fraction).round() as usize;
            assert!(fills.contains(&filled), "no bar at {fraction} in {fills:?}");
        }
    }
    #[test]
    fn render_golden_grid_32() {
        check(32);
    }
    #[test]
    fn render_golden_grid_48() {
        check(48);
    }
    /// R1: at the narrow grid the `5h` label stays readable and the compacted request
    /// count leaves it room instead of being clipped to a single digit.
    #[test]
    fn kimi_5h_label_is_readable_at_grid_32() {
        let lines = render(&sample(), 32);
        let texts: Vec<String> = lines.iter().map(Line::text).collect();
        assert!(
            texts
                .iter()
                .any(|t| t.starts_with("5h") && t.contains("87/100 left")),
            "{texts:#?}"
        );
        assert!(
            !texts.iter().any(|t| t.starts_with("5 87/100")),
            "{texts:#?}"
        );
        assert!(lines.iter().all(|l| width(&l.text()) <= 32));
    }

    /// A viewport shorter than the ledger keeps the head and ends with a faint footer that
    /// names the hidden routes, so a watching pane never writes past its last row.
    #[test]
    fn render_fitted_clips_and_names_hidden_routes() {
        let full = render(&sample(), 32);
        let fitted = render_fitted(&sample(), 32, 10);
        assert!(fitted.len() <= 10, "{} lines", fitted.len());
        assert_eq!(fitted[0].text(), full[0].text());
        let text = fitted.iter().map(Line::text).collect::<Vec<_>>().join("\n");
        assert!(text.contains("hidden:"), "{text}");
        assert!(fitted.iter().all(|line| width(&line.text()) <= 32));
        assert_eq!(fitted.last().unwrap().0[0].tone, Tone::Faint);
    }

    /// A viewport between the compact and the separated length drops the blank separators
    /// instead of hiding a route: the owner's 42-line pane fits the dense ledger.
    #[test]
    fn render_fitted_compacts_before_clipping() {
        let full = render(&sample(), 32);
        let compact = render_parts(&sample(), 32, Density::Compact).lines;
        assert!(compact.len() < full.len(), "no separators to drop");
        let fitted = render_fitted(&sample(), 32, full.len() - 1);
        assert!(fitted.len() < full.len());
        assert!(!fitted.iter().any(|line| line.text().is_empty()));
        let text = fitted.iter().map(Line::text).collect::<Vec<_>>().join("\n");
        assert!(!text.contains("hidden:"), "{text}");
    }

    /// The owner's real pane is 56x42 and the host reserves one row for the status line, so
    /// the ledger must fit 41 rows with the credits rows visible.
    #[test]
    fn seven_providers_fit_at_the_owners_pane_with_credits_visible() {
        let snapshot = seven_providers();
        assert_eq!(
            render_parts(&snapshot, 48, Density::Compact).lines.len(),
            42
        );
        let fitted = render_fitted(&snapshot, 48, 41);
        assert!(fitted.len() <= 41, "{} lines", fitted.len());
        let text: Vec<String> = fitted.iter().map(Line::text).collect();
        assert!(text.iter().any(|t| t.contains("credits used")), "{text:#?}");
        assert!(text.iter().any(|t| t.contains("credits left")), "{text:#?}");
        assert!(
            !text.iter().any(|t| t.contains("hidden:")),
            "a route was hidden: {text:#?}"
        );
    }

    /// Clipping names every hidden provider, at 34x10 (the host's reserve makes it 9).
    #[test]
    fn clipping_names_every_hidden_provider() {
        let snapshot = seven_providers();
        let fitted = render_fitted(&snapshot, 34, 9);
        assert!(fitted.len() <= 9, "{} lines", fitted.len());
        assert!(fitted.iter().all(|line| width(&line.text()) <= 34));
        let text = fitted.iter().map(Line::text).collect::<Vec<_>>().join("\n");
        for label in ["OPENCODE GO-2", "OPENCODE GO", "OPENROUTER CREDITS"] {
            assert!(text.contains(label), "hidden {label} not named: {text}");
        }
        assert!(text.contains("hidden:"), "{text}");
    }

    /// A wide grid shows the full sha256; a narrow one shows a marked 16-hex prefix.
    #[test]
    fn status_line_shows_full_hash_or_a_marked_prefix() {
        let full = "a".repeat(64);
        let wide = status_line(&full, "2026-09-24T06:14:00Z", 96);
        assert!(wide.text().contains(&full), "{}", wide.text());
        let narrow = status_line(&full, "2026-09-24T06:14:00Z", 48);
        assert!(narrow.text().contains("sha256"), "{}", narrow.text());
        assert!(narrow.text().contains('…'), "{}", narrow.text());
    }

    #[test]
    fn render_fitted_leaves_a_fitting_ledger_alone() {
        let full = render(&sample(), 32);
        let fitted = render_fitted(&sample(), 32, full.len() + 3);
        assert_eq!(fitted.len(), full.len());
        assert_eq!(fitted.last().unwrap().text(), full.last().unwrap().text());
        assert_eq!(render_fitted(&sample(), 32, 0).len(), full.len());
    }

    /// Freshness is on every provider row: a fresh age, a retained last-good value marked
    /// stale with its age, and `no data` when a probe failed with nothing to keep.
    #[test]
    fn route_rows_show_age_and_stale_last_good() {
        let mut fresh = route("fresh", Probe::Supported, vec![window(Some(10.0), false)]);
        fresh.observed_at = Some("2026-09-22T22:36:00Z".into());
        let mut stale = route("stale", Probe::Supported, vec![window(Some(20.0), false)]);
        stale.observed_at = Some("2026-09-22T22:31:00Z".into());
        stale.stale = Some("http 500".into());
        let failed = route(
            "failed",
            Probe::Failed {
                kind: FailKind::Http(500),
                detail: "HTTP 500".into(),
            },
            vec![],
        );
        let snapshot = Snapshot {
            taken_at: "2026-09-22T22:36:00Z".into(),
            routes: vec![fresh, stale, failed],
        };
        let text: Vec<String> = render(&snapshot, 48).iter().map(Line::text).collect();
        assert!(
            text.iter()
                .any(|t| t.starts_with("FRESH") && t.ends_with("0s")),
            "{text:#?}"
        );
        assert!(
            text.iter()
                .any(|t| t.starts_with("STALE") && t.contains("stale 5m (http 500)")),
            "{text:#?}"
        );
        assert!(
            text.iter()
                .any(|t| t.starts_with("FAILED") && t.contains("no data")),
            "{text:#?}"
        );
        assert!(
            text.iter()
                .any(|t| t.trim_start().starts_with("error · HTTP 500")),
            "{text:#?}"
        );
    }

    /// A credits endpoint with a total renders used/left; a disabled extra-usage plan says so.
    #[test]
    fn credits_used_left_and_disabled_extra_usage_render() {
        let mut openrouter = route("openrouter credits", Probe::Supported, vec![]);
        openrouter.credits = Some(Credits {
            balance: Some(17.46),
            reset_credits: None,
            used: Some(12.54),
            limit: Some(30.0),
            currency: Some("USD".into()),
        });
        let mut claude = route("claude max", Probe::Supported, vec![]);
        claude.extra_usage = Some(ExtraUsage {
            enabled: Some(false),
            used: Some(0.0),
            limit: Some(5000.0),
            currency: Some("EUR".into()),
        });
        let snapshot = Snapshot {
            taken_at: "2026-09-22T22:36:00Z".into(),
            routes: vec![openrouter, claude],
        };
        let text: Vec<String> = render(&snapshot, 48).iter().map(Line::text).collect();
        assert!(
            text.iter()
                .any(|t| t.contains("credits used") && t.contains("12.54 / 30 USD")),
            "{text:#?}"
        );
        assert!(
            text.iter()
                .any(|t| t.contains("credits left") && t.contains("17.46 USD")),
            "{text:#?}"
        );
        assert!(
            text.iter()
                .any(|t| t.contains("extra usage") && t.ends_with("disabled")),
            "{text:#?}"
        );
    }

    #[test]
    fn credential_failure_says_no_access() {
        let snapshot = Snapshot {
            taken_at: "2026-09-22T22:36:00Z".into(),
            routes: vec![route(
                "kimi",
                Probe::Failed {
                    kind: FailKind::Credential,
                    detail: "HTTP 401".into(),
                },
                vec![],
            )],
        };
        let lines = render(&snapshot, 32);
        assert!(lines.iter().any(|l| l.text().trim() == "no access"));
    }

    #[test]
    fn render_unsupported_says_so() {
        let snapshot = Snapshot {
            taken_at: "2026-09-22T22:36:00Z".into(),
            routes: vec![route(
                "unknown key",
                Probe::Unsupported {
                    reason: "no usage endpoint known for this route".into(),
                },
                vec![],
            )],
        };
        let lines = render(&snapshot, 32);
        assert_eq!(lines[0].text(), "UNKNOWN KEY              unknown");
        assert_eq!(lines[1].text(), "no usage endpoint");
        assert_eq!(lines[1].0[0].tone, Tone::Faint);
    }
    #[test]
    fn render_zero_full_unknown_and_due() {
        let snapshot = Snapshot {
            taken_at: "2026-09-22T22:36:00Z".into(),
            routes: vec![route(
                "edge",
                Probe::Supported,
                vec![
                    window(Some(0.0), false),
                    window(Some(100.0), true),
                    Window {
                        resets_at: Some("2026-09-22T20:00:00Z".into()),
                        ..window(None, false)
                    },
                ],
            )],
        };
        let lines = render(&snapshot, 32);
        assert!(lines.iter().any(|l| l.text().contains("!5h")));
        assert!(lines.iter().any(|l| l.text().contains("resets due")));
        assert!(lines.iter().any(|l| l.text().contains("— · resets")));
        for (line, count) in [(2, 0), (4, 26)] {
            assert_eq!(lines[line].0[0].text.chars().count(), count);
        }
        assert!(lines.iter().all(|l| width(&l.text()) <= 32));
    }

    /// An unknown used share shows the `—` marker and no bar: an empty bar beside it would
    /// read as `0 % used` and contradict the marker. A known sibling window keeps its bar.
    #[test]
    fn unknown_usage_draws_the_marker_and_no_bar() {
        let snapshot = Snapshot {
            taken_at: "2026-09-22T22:36:00Z".into(),
            routes: vec![route(
                "edge",
                Probe::Supported,
                vec![window(None, false), window(Some(42.0), false)],
            )],
        };
        let lines = render(&snapshot, 32);
        let texts: Vec<String> = lines.iter().map(Line::text).collect();
        assert!(
            texts
                .iter()
                .any(|t| t.starts_with("5h") && t.contains("— · resets")),
            "{texts:#?}"
        );
        let bars: Vec<&Line> = lines
            .iter()
            .filter(|line| line.0.iter().any(|span| span.tone == Tone::Rule))
            .collect();
        assert_eq!(
            bars.len(),
            1,
            "only the known window draws a bar: {texts:#?}"
        );
        assert!(
            !texts.iter().any(|t| t.contains("0%")),
            "no 0 % bar for the unknown window: {texts:#?}"
        );
    }

    /// A used-based window is unchanged: its label states the used percentage and its bar is
    /// that same fraction.
    #[test]
    fn used_based_row_bar_and_percent_agree() {
        let snapshot = Snapshot {
            taken_at: "2026-09-22T22:36:00Z".into(),
            routes: vec![route(
                "used",
                Probe::Supported,
                vec![window(Some(42.0), false)],
            )],
        };
        let grid = 32;
        let lines = render(&snapshot, grid);
        let texts: Vec<String> = lines.iter().map(Line::text).collect();
        assert!(
            texts
                .iter()
                .any(|t| t.starts_with("5h") && t.contains("42% · resets")),
            "{texts:#?}"
        );
        let bar = lines
            .iter()
            .find(|line| line.0.iter().any(|span| span.tone == Tone::Rule))
            .expect("a bar");
        let cells = grid - 6;
        assert_eq!(
            bar.0[0].text.chars().count(),
            (0.42 * cells as f64).round() as usize
        );
        assert!(bar.text().trim_end().ends_with("42%"), "{}", bar.text());
    }

    /// The owner's live Kimi row: `15/100 left` beside a bar at the 85 % used the count
    /// implies, at the pane's 48-column grid and 42-row height. The bar used to sit at 0 %
    /// while the text said 15 of 100 were left.
    #[test]
    fn seven_providers_kimi_row_bar_agrees_with_the_left_text() {
        let snapshot = seven_providers();
        let grid = 48;
        let lines = render(&snapshot, grid);
        let texts: Vec<String> = lines.iter().map(Line::text).collect();
        let row = texts
            .iter()
            .find(|t| t.starts_with("5h") && t.contains("15/100 left"))
            .unwrap_or_else(|| panic!("no kimi 5h row: {texts:#?}"));
        assert!(!row.contains("0%"), "contradictory row: {row}");
        let bar = lines
            .iter()
            .find(|line| line.text().contains("85%"))
            .expect("the 85 % bar");
        let cells = grid - 6;
        assert_eq!(
            bar.0[0].text.chars().count(),
            (0.85 * cells as f64).round() as usize
        );
        assert!(bar.0[0].text.chars().all(|c| c == '█'));
        // It still fits the owner's 56x42 pane with the status row reserved.
        assert!(render_fitted(&snapshot, grid, 41).len() <= 41);
    }
}
