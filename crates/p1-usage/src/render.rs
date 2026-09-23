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

/// Pure, palette-independent ledger on a `grid`-column content grid.
pub fn render(snapshot: &Snapshot, grid: usize) -> Vec<Line> {
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
    for route in routes {
        if !lines.is_empty() {
            lines.push(Line(vec![]));
        }
        let state = match &route.probe {
            // A provider that reports usage but no plan name: the value is unknown, so
            // it renders as `—` (SPEC §5), never as a word that reads like a probe state.
            Probe::Supported => route.plan.as_deref().unwrap_or("—"),
            Probe::Unsupported { .. } => "unknown",
            Probe::Failed { .. } => "error",
        };
        lines.push(row(&route.label.to_uppercase(), state, Tone::Dim, grid));
        match &route.probe {
            Probe::Unsupported { .. } => lines.push(single("no usage endpoint", Tone::Faint, grid)),
            Probe::Failed { kind, .. } => {
                let label = match kind {
                    FailKind::Credential => "credential",
                    FailKind::Http(_) => "http",
                    FailKind::Network => "network",
                    FailKind::Parse => "parse",
                };
                lines.push(single(label, Tone::Dim, grid));
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
                    let value = if time == "—" {
                        format!("{} · resets —", percent(window.used_percent))
                    } else if time == "due" {
                        format!("{} · resets due", percent(window.used_percent))
                    } else {
                        format!("{} · resets {time}", percent(window.used_percent))
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
                    lines.push(bar(window, grid));
                }
                if let Some(credits) = &route.credits {
                    lines.push(row("credits", &amount(credits.balance), Tone::Dim, grid));
                    lines.push(row(
                        "reset credits",
                        &credits
                            .reset_credits
                            .map(|v| v.to_string())
                            .unwrap_or_else(|| "—".into()),
                        Tone::Dim,
                        grid,
                    ));
                }
                if let Some(extra) = &route.extra_usage {
                    let value = format!(
                        "{} / {} {}",
                        amount(extra.used),
                        amount(extra.limit),
                        extra.currency.as_deref().unwrap_or("—")
                    );
                    lines.push(row("extra usage", &value, Tone::Dim, grid));
                }
            }
        }
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
        }
    }
    fn window(pct: Option<f64>, reached: bool) -> Window {
        Window {
            kind: WindowKind::Session,
            scope: None,
            used_percent: pct,
            resets_at: Some("2026-09-22T23:00:00Z".into()),
            limit_reached: reached,
        }
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
            }],
        );
        chatgpt.plan = Some("prolite".into());
        chatgpt.credits = Some(Credits {
            balance: Some(0.0),
            reset_credits: Some(1),
        });
        // Ranking is by percentage, not input order or label.
        Snapshot {
            taken_at: "2026-09-22T22:36:00Z".into(),
            routes: vec![
                route(
                    "opencode go-2",
                    Probe::Unsupported {
                        reason: "no usage endpoint known for this route".into(),
                    },
                    vec![],
                ),
                route(
                    "failed",
                    Probe::Failed {
                        kind: FailKind::Http(401),
                        detail: "HTTP 401".into(),
                    },
                    vec![],
                ),
                claude,
                chatgpt,
            ],
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
        let bar = lines
            .iter()
            .find(|line| line.0.iter().any(|span| span.tone == Tone::Rule))
            .unwrap();
        assert_eq!(
            bar.0[0].text.chars().count(),
            ((grid - 6) as f64 * 0.69).round() as usize
        );
    }
    #[test]
    fn render_golden_grid_32() {
        check(32);
    }
    #[test]
    fn render_golden_grid_48() {
        check(48);
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
}
