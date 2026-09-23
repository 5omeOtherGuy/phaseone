//! Statusline data the driver cannot get from `p1-tui` (handoff §10): pure,
//! testable pieces the driver assembles into `Screen::statusbar` every frame.
//!
//! `ctx`/`ctx_warn` genuinely need the assembled context window and its
//! summarize threshold; nothing on the `FrontEnd` seam carries them from
//! `run_with_front_end`'s assembly to `TuiFrontEnd` (`parent_assembled` only
//! passes `route`/`model` strings — frontend.rs is not an owned path for this
//! task). The driver therefore always calls [`ctx_status`] with `window: None`
//! today, so `ctx` renders `—`; the function itself is exercised directly
//! below with a real window, so the math is proven ahead of that seam.

use std::path::Path;

use p1_contracts::Usage;

/// §10 `ctx`: last response's input total ÷ the configured context window, as a
/// whole percent; `warn` fires at or above the summarize threshold. Either input
/// missing, or a zero window, is unknown — never a guessed percentage.
pub fn ctx_status(
    input_total: Option<u64>,
    window: Option<u64>,
    warn_at: Option<u64>,
) -> (Option<String>, bool) {
    let (Some(total), Some(window)) = (input_total, window) else {
        return (None, false);
    };
    if window == 0 {
        return (None, false);
    }
    let percent = total.saturating_mul(100) / window;
    let warn = warn_at.is_some_and(|at| total >= at);
    (Some(format!("{percent}%")), warn)
}

/// The input side of one response's usage, the way `ctx` measures it
/// (uncached + cache read + cache write — the same total `Spend::record`
/// sums for the session, but for exactly ONE response). `None` when the
/// response reported no usage at all: unknown, never zero.
pub fn usage_input_total(usage: Option<&Usage>) -> Option<u64> {
    let usage = usage?;
    let base = usage.input_uncached?;
    Some(base + usage.cache_read.unwrap_or(0) + usage.cache_write.unwrap_or(0))
}

/// §10 `spend`: `Spend.cost_micro_usd` as a compact `$0.00`. `None` stays `None`
/// (a subscription route never reports a cost, so `Spend` poisons to `None` at
/// the first response and stays there — `StatusBar` already renders that `—`).
pub fn spend_string(cost_micro_usd: Option<u64>) -> Option<String> {
    cost_micro_usd
        .map(|micro| format!("${}.{:02}", micro / 1_000_000, (micro % 1_000_000) / 10_000))
}

/// §10 `clock`: wall time since the session started, `{h}h{mm}`.
pub fn clock(elapsed_ms: u64) -> String {
    let minutes_total = elapsed_ms / 60_000;
    format!("{}h{:02}", minutes_total / 60, minutes_total % 60)
}

/// §10 `repo`: the workspace root basename.
pub fn repo_name(workspace: &Path) -> Option<String> {
    workspace
        .file_name()
        .and_then(|name| name.to_str())
        .map(str::to_string)
}

/// §10 `branch`: `git rev-parse --abbrev-ref HEAD` in `workspace`; a detached
/// HEAD reports its 7-char short sha instead; outside a git repo (or no `git`
/// on `PATH`) the field stays omitted. Runs `git` synchronously — callers off
/// the render loop (a blocking task), never from `draw`.
pub fn git_branch(workspace: &Path) -> Option<String> {
    let head = run_git(workspace, &["rev-parse", "--abbrev-ref", "HEAD"])?;
    if head != "HEAD" {
        return Some(head);
    }
    run_git(workspace, &["rev-parse", "--short=7", "HEAD"])
}

fn run_git(workspace: &Path, args: &[&str]) -> Option<String> {
    let output = std::process::Command::new("git")
        .args(args)
        .current_dir(workspace)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8(output.stdout).ok()?;
    let trimmed = text.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

/// §10 `▪ N workers`: the running count from the worker snapshot.
pub fn running_workers(rows: &[p1_tui::render::workers::WorkerBlock]) -> usize {
    rows.iter()
        .filter(|row| row.state == p1_tui::render::workers::BlockState::Running)
        .count()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ctx_percent_needs_both_the_usage_and_the_window() {
        assert_eq!(ctx_status(None, Some(120_000), Some(90_000)), (None, false));
        assert_eq!(ctx_status(Some(1_000), None, Some(90_000)), (None, false));
        assert_eq!(
            ctx_status(Some(60_000), Some(120_000), Some(90_000)),
            (Some("50%".into()), false)
        );
    }

    #[test]
    fn ctx_warn_fires_at_the_summarize_threshold() {
        let (percent, warn) = ctx_status(Some(90_000), Some(120_000), Some(90_000));
        assert_eq!(percent.as_deref(), Some("75%"));
        assert!(warn);
        let (_, warn) = ctx_status(Some(89_999), Some(120_000), Some(90_000));
        assert!(!warn);
        // No configured threshold: never warns, however full.
        let (_, warn) = ctx_status(Some(120_000), Some(120_000), None);
        assert!(!warn);
    }

    #[test]
    fn usage_input_total_sums_uncached_and_cache_and_is_none_without_usage() {
        assert_eq!(usage_input_total(None), None);
        assert_eq!(
            usage_input_total(Some(&Usage {
                input_uncached: Some(100),
                cache_read: Some(40),
                cache_write: Some(10),
                ..Usage::default()
            })),
            Some(150)
        );
        // A reported usage with no input part at all: still unknown, not zero.
        assert_eq!(
            usage_input_total(Some(&Usage {
                input_uncached: None,
                ..Usage::default()
            })),
            None
        );
    }

    #[test]
    fn spend_is_none_for_a_subscription_route_and_a_price_otherwise() {
        assert_eq!(spend_string(None), None);
        assert_eq!(spend_string(Some(1_234_500)), Some("$1.23".into()));
    }

    #[test]
    fn clock_formats_hours_and_minutes() {
        assert_eq!(clock(0), "0h00");
        assert_eq!(clock(125_000), "0h02");
        assert_eq!(clock(3_661_000), "1h01");
    }

    #[test]
    fn repo_name_is_the_workspace_basename() {
        assert_eq!(
            repo_name(Path::new("/home/op/projects/p1")),
            Some("p1".into())
        );
    }

    #[test]
    fn git_branch_reads_a_real_repo_and_is_none_outside_one() {
        let dir = tempfile::tempdir().unwrap();
        // Not a repo yet: omitted, not a guess.
        assert_eq!(git_branch(dir.path()), None);
        let run = |args: &[&str]| {
            assert!(
                std::process::Command::new("git")
                    .args(args)
                    .current_dir(dir.path())
                    .status()
                    .unwrap()
                    .success()
            );
        };
        run(&["init", "-q", "-b", "trunk"]);
        run(&["config", "user.email", "p1@example.invalid"]);
        run(&["config", "user.name", "p1"]);
        std::fs::write(dir.path().join("f"), b"x").unwrap();
        run(&["add", "f"]);
        run(&["commit", "-q", "-m", "first"]);
        assert_eq!(git_branch(dir.path()), Some("trunk".into()));
        // Detached HEAD: the short sha, not the literal `HEAD`.
        run(&["checkout", "-q", "--detach", "HEAD"]);
        let sha = git_branch(dir.path()).expect("a detached head still resolves");
        assert_eq!(sha.len(), 7);
        assert_ne!(sha, "HEAD");
    }

    #[test]
    fn running_workers_counts_only_the_running_state() {
        use p1_tui::render::workers::{BlockState, WorkerBlock};
        let row = |state| WorkerBlock {
            id: "w1".into(),
            task: String::new(),
            route: String::new(),
            state,
            elapsed: None,
            cost_micro_usd: None,
            grants: String::new(),
            activity: String::new(),
        };
        let rows = vec![
            row(BlockState::Running),
            row(BlockState::Done),
            row(BlockState::Running),
        ];
        assert_eq!(running_workers(&rows), 2);
    }
}
