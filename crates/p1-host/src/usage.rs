//! The CLI adapter for the provider-neutral usage ledger.
use std::collections::BTreeMap;
use std::io::Write;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use p1_auth::Locations;
use p1_usage::{FailKind, Probe, RouteUsage, Snapshot, UsageRoute};

use crate::HostDeps;
use crate::cli::UsageOptions;

fn label(id: &str) -> String {
    match id {
        "anthropic-subscription" => "claude max".into(),
        "openai-codex-subscription" => "chatgpt pro lite".into(),
        "opencode-go-subscription" => "opencode go".into(),
        "opencode-go-1-subscription" => "opencode go-1".into(),
        "opencode-go-2-subscription" => "opencode go-2".into(),
        "opencode-go-3-subscription" => "opencode go-3".into(),
        // The free Zen accounts have no known usage endpoint (docs/design/usage.md), so their
        // rows are `Unsupported`; the label still names the account rather than the route id.
        "opencode-zen-1" => "opencode zen-1".into(),
        "opencode-zen-2" => "opencode zen-2".into(),
        "opencode-zen-3" => "opencode zen-3".into(),
        "opencode-zen-free" => "opencode zen free".into(),
        "glm-subscription" => "glm".into(),
        _ => id
            .strip_suffix("-subscription")
            .unwrap_or(id)
            .replace('-', " "),
    }
}

/// The live pane: the alternate screen and a hidden cursor, both restored when this drops.
/// Entered only when stdout is a terminal; a pipe or file keeps plain, append-only frames.
struct Screen {
    out: crate::SharedWriter,
    active: bool,
}

impl Screen {
    fn enter(out: crate::SharedWriter) -> Self {
        let mut screen = Self { out, active: false };
        screen.emit("\x1b[?1049h\x1b[?25l");
        screen.active = true;
        screen
    }

    /// One frame (or one control sequence), written whole and flushed so a pane never
    /// shows half a frame.
    fn emit(&self, text: &str) {
        if let Ok(mut out) = self.out.lock() {
            let _ = out.write_all(text.as_bytes());
            let _ = out.flush();
        }
    }

    fn restore(&mut self) {
        if self.active {
            self.emit("\x1b[?25h\x1b[?1049l");
            self.active = false;
        }
    }
}

impl Drop for Screen {
    fn drop(&mut self) {
        self.restore();
    }
}

/// The pane's size for this frame, or the requested grid and a 24-line fallback when the
/// terminal cannot be queried. Re-read every frame, so a resize needs no signal, and the
/// grid is clamped to the pane width so rows never wrap.
fn viewport(grid: usize) -> (usize, usize) {
    match crossterm::terminal::size() {
        Ok((width, height)) => (grid.min(width as usize).max(1), (height as usize).max(1)),
        Err(_) => (grid, 24),
    }
}

/// Resolves when SIGTERM arrives, so a killed pane still restores the terminal.
#[cfg(unix)]
async fn terminate_signal(signal: &mut Option<tokio::signal::unix::Signal>) {
    match signal {
        Some(signal) => {
            signal.recv().await;
        }
        None => std::future::pending().await,
    }
}

#[cfg(not(unix))]
async fn terminate_signal(_signal: &mut Option<()>) {
    std::future::pending().await;
}

/// Resolves on SIGWINCH, so a resize redraws at once instead of waiting for the next probe.
#[cfg(unix)]
async fn window_signal(signal: &mut Option<tokio::signal::unix::Signal>) {
    match signal {
        Some(signal) => {
            signal.recv().await;
        }
        None => std::future::pending().await,
    }
}

#[cfg(not(unix))]
async fn window_signal(_signal: &mut Option<()>) {
    std::future::pending().await;
}

/// The hash of the binary actually running, for the pane's provenance row. Hashing the
/// whole (debug) binary takes seconds, so it runs on a background thread and the status row
/// shows `…` until it lands; the first frame is never delayed by it. `ready` is notified when
/// the hash lands, so the watch loop can redraw at once.
fn build_hash_async(ready: Arc<tokio::sync::Notify>) -> Arc<Mutex<String>> {
    let slot = Arc::new(Mutex::new("…".to_string()));
    let out = slot.clone();
    std::thread::spawn(move || {
        let hash = match std::env::current_exe()
            .ok()
            .and_then(|path| std::fs::read(path).ok())
        {
            Some(bytes) => p1_usage::sha256_hex(&bytes),
            None => "unknown".into(),
        };
        if let Ok(mut slot) = out.lock() {
            *slot = hash;
        }
        // The loop redraws on this notification, so the hash appears without waiting for the
        // next probe (which can be 900 s away).
        ready.notify_one();
    });
    slot
}

fn build_now(build: &Option<Arc<Mutex<String>>>) -> String {
    build
        .as_ref()
        .and_then(|slot| slot.lock().ok().map(|value| value.clone()))
        .unwrap_or_default()
}

/// Resolves when the background build hash lands. `Notify` stores one permit, so a
/// notification that arrives while the loop is probing is not lost, and it never fires again
/// after the permit is consumed (unlike a closed watch channel).
async fn hash_ready(ready: &Arc<tokio::sync::Notify>) {
    ready.notified().await;
}

/// The OpenRouter credits key, from the existing brain-tools configuration. Read-only; the
/// value is used as a bearer and never logged or serialized.
fn openrouter_key(deps: &HostDeps) -> Option<String> {
    let home = deps
        .home
        .clone()
        .or_else(|| std::env::var_os("HOME").map(std::path::PathBuf::from))?;
    let key = std::fs::read_to_string(home.join(".config/brain-tools/openrouter.key")).ok()?;
    let key = key.trim();
    (!key.is_empty()).then(|| key.to_string())
}

/// The OpenRouter row when the existing key is missing or empty: an explicit `no access`
/// provider, never a silently absent endpoint.
fn openrouter_no_access() -> RouteUsage {
    let mut route = RouteUsage::new(
        "openrouter-credits",
        "openrouter credits",
        "brain-tools openrouter.key",
    );
    route.probe = Probe::Failed {
        kind: FailKind::Credential,
        detail: "no key".into(),
    };
    route
}

/// The short reason a retained row is stale (kept compact: it rides in the header row).
fn failure_reason(probe: &Probe) -> String {
    match probe {
        Probe::Failed { kind, .. } => match kind {
            FailKind::Credential => "no access".into(),
            FailKind::Http(status) => format!("http {status}"),
            FailKind::Network => "network".into(),
            FailKind::Parse => "parse".into(),
        },
        Probe::Unsupported { .. } => "no usage endpoint".into(),
        Probe::Supported => String::new(),
    }
}

/// Keep the last good values for routes whose current probe failed, so a provider is never
/// silently replaced by an error with no data; mark the retained row stale. A route that
/// has never produced data stays an explicit failure.
fn apply_stale(routes: &mut [RouteUsage], last_good: &mut BTreeMap<String, RouteUsage>) {
    for route in routes.iter_mut() {
        if matches!(route.probe, Probe::Supported) {
            last_good.insert(route.route_id.clone(), route.clone());
            continue;
        }
        if let Some(good) = last_good.get(&route.route_id) {
            let mut kept = good.clone();
            kept.stale = Some(failure_reason(&route.probe));
            *route = kept;
        }
    }
}

fn ansi_line(line: &p1_usage::Line, ansi: bool) -> String {
    let mut text = String::new();
    for span in &line.0 {
        if ansi && !span.text.is_empty() {
            let rgb = match span.tone {
                p1_usage::Tone::Ink => "232;232;232",
                p1_usage::Tone::Dim => "154;154;154",
                p1_usage::Tone::Faint => "106;106;106",
                p1_usage::Tone::Rule => "42;42;42",
            };
            text.push_str(&format!("\x1b[38;2;{rgb}m{}\x1b[0m", span.text));
        } else {
            text.push_str(&span.text);
        }
    }
    text
}

/// Write one frame, fitted to the pane when live, with the provenance/freshness status row.
fn draw(
    deps: &HostDeps,
    screen: Option<&Screen>,
    snapshot: &Snapshot,
    grid: usize,
    plain: bool,
    live: bool,
    build: &str,
) {
    let ansi = deps.stdout_is_tty && !plain;
    let (grid, height) = if live { viewport(grid) } else { (grid, 0) };
    let mut lines = if height == 0 {
        p1_usage::render(snapshot, grid)
    } else {
        // One row is reserved for the status line so the frame still fits the pane exactly.
        p1_usage::render_fitted(snapshot, grid, height.saturating_sub(1))
    };
    if live {
        lines.push(p1_usage::status_line(build, &snapshot.taken_at, grid));
    }
    let output = lines
        .iter()
        .map(|line| ansi_line(line, ansi))
        .collect::<Vec<_>>()
        .join("\n");
    match screen {
        Some(screen) => screen.emit(&format!("\x1b[H\x1b[2J{output}")),
        None => {
            let _ = writeln!(deps.stdout.lock().unwrap(), "{output}");
        }
    }
}

pub async fn usage(deps: &HostDeps, options: &UsageOptions) -> i32 {
    let UsageOptions {
        json,
        watch,
        plain,
        grid,
        search,
    } = options;
    let (json, watch, plain, grid, search) = (*json, *watch, *plain, *grid, search.as_deref());
    let locations = Locations::from_process();
    let routes = match crate::routes::load_all_routes(&deps.environment_dirs) {
        Ok(routes) => routes,
        Err(error) => {
            let _ = writeln!(deps.stderr.lock().unwrap(), "{error}");
            return 2;
        }
    };
    let search = search.map(str::to_lowercase);
    let routes: Vec<UsageRoute> = routes
        .into_iter()
        .filter(|r| {
            search
                .as_ref()
                .is_none_or(|s| r.id.to_lowercase().contains(s) || label(&r.id).contains(s))
        })
        .map(|route| UsageRoute {
            label: label(&route.id),
            credential: p1_auth::describe(&route.id, &route.credential, &locations).line(),
            route_id: route.id,
            spec: route.credential,
        })
        .collect();
    let interrupt = deps.interrupt.recv();
    tokio::pin!(interrupt);
    #[cfg(unix)]
    let mut terminate =
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).ok();
    #[cfg(unix)]
    let mut winch =
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::window_change()).ok();
    #[cfg(not(unix))]
    let mut terminate: Option<()> = None;
    #[cfg(not(unix))]
    let mut winch: Option<()> = None;
    // A live pane owns the alternate screen: repeated frames never reach scrollback, and
    // the terminal is restored when this returns, including on interrupt.
    let live = watch.is_some() && deps.stdout_is_tty;
    let screen = live.then(|| Screen::enter(deps.stdout.clone()));
    let hash_ready_signal = Arc::new(tokio::sync::Notify::new());
    let build = live.then(|| build_hash_async(hash_ready_signal.clone()));
    let openrouter = openrouter_key(deps);
    let mut last_good: BTreeMap<String, RouteUsage> = BTreeMap::new();
    // The last rendered snapshot, so a resize can redraw immediately without a new probe.
    let mut current: Option<Snapshot> = None;
    loop {
        let gather = async {
            let snapshot = p1_usage::snapshot(&routes, &locations, deps.transport.clone());
            let credits = async {
                match &openrouter {
                    Some(key) => p1_usage::openrouter_credits(key).await,
                    None => openrouter_no_access(),
                }
            };
            tokio::join!(snapshot, credits)
        };
        tokio::pin!(gather);
        let (mut snapshot, mut credits) = loop {
            tokio::select! {
                _ = &mut interrupt => return 0,
                _ = terminate_signal(&mut terminate) => return 0,
                _ = window_signal(&mut winch) => {
                    if let Some(snapshot) = &current {
                        draw(deps, screen.as_ref(), snapshot, grid, plain, live, &build_now(&build));
                    }
                }
                _ = hash_ready(&hash_ready_signal) => {
                    if let Some(snapshot) = &current {
                        draw(deps, screen.as_ref(), snapshot, grid, plain, live, &build_now(&build));
                    }
                }
                pair = &mut gather => break pair,
            }
        };
        if matches!(credits.probe, Probe::Supported) {
            credits.observed_at = Some(snapshot.taken_at.clone());
        }
        snapshot.routes.push(credits);
        apply_stale(&mut snapshot.routes, &mut last_good);
        if json {
            let text = serde_json::to_string_pretty(&snapshot).unwrap_or_default();
            let _ = writeln!(deps.stdout.lock().unwrap(), "{text}");
            return 0;
        }
        current = Some(snapshot.clone());
        draw(
            deps,
            screen.as_ref(),
            &snapshot,
            grid,
            plain,
            live,
            &build_now(&build),
        );
        let Some(seconds) = watch else { return 0 };
        let sleep = tokio::time::sleep(Duration::from_secs(seconds));
        tokio::pin!(sleep);
        loop {
            tokio::select! {
                _ = &mut interrupt => return 0,
                _ = terminate_signal(&mut terminate) => return 0,
                _ = window_signal(&mut winch) => {
                    if let Some(snapshot) = &current {
                        draw(deps, screen.as_ref(), snapshot, grid, plain, live, &build_now(&build));
                    }
                }
                _ = hash_ready(&hash_ready_signal) => {
                    if let Some(snapshot) = &current {
                        draw(deps, screen.as_ref(), snapshot, grid, plain, live, &build_now(&build));
                    }
                }
                _ = &mut sleep => break,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use p1_usage::{Window, WindowKind};

    fn good(id: &str, observed: &str) -> RouteUsage {
        let mut route = RouteUsage::new(id, id, "fixture");
        route.observed_at = Some(observed.into());
        route.windows.push(Window {
            kind: WindowKind::Session,
            scope: None,
            used_percent: Some(10.0),
            resets_at: None,
            limit_reached: false,
            detail: None,
        });
        route
    }

    fn failed(id: &str) -> RouteUsage {
        let mut route = RouteUsage::new(id, id, "fixture");
        route.probe = Probe::Failed {
            kind: FailKind::Credential,
            detail: "HTTP 401".into(),
        };
        route
    }

    #[test]
    fn a_failed_probe_keeps_the_last_good_values_and_marks_them_stale() {
        let mut last = BTreeMap::new();
        let mut first = vec![good("kimi", "2026-09-24T05:00:00Z")];
        apply_stale(&mut first, &mut last);
        assert!(last.contains_key("kimi"));
        let mut second = vec![failed("kimi")];
        apply_stale(&mut second, &mut last);
        assert!(matches!(second[0].probe, Probe::Supported));
        assert_eq!(second[0].windows.len(), 1);
        assert_eq!(
            second[0].observed_at.as_deref(),
            Some("2026-09-24T05:00:00Z")
        );
        assert_eq!(second[0].stale.as_deref(), Some("no access"));
    }

    #[test]
    fn a_never_good_route_stays_an_explicit_failure() {
        let mut last = BTreeMap::new();
        let mut routes = vec![failed("kimi")];
        apply_stale(&mut routes, &mut last);
        assert!(matches!(routes[0].probe, Probe::Failed { .. }));
        assert_eq!(routes[0].observed_at, None);
        assert_eq!(routes[0].stale, None);
    }
}
