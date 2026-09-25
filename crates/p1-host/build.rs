//! Version metadata for `p1 --version`.
//!
//! The release job hands the built commit's short sha and date in through the
//! environment (`P1_GIT_SHA`, `P1_BUILD_DATE`, ADR-0065); a local build falls back to
//! `git rev-parse` for the sha and `SOURCE_DATE_EPOCH` for the date. There is no
//! wall-clock fallback: two builds of one commit must print the same string, and
//! `unknown` is the honest answer when neither source exists. A git-sourced sha is
//! baked into the binary until cargo reruns this script, so the local case also names
//! the git files that move HEAD (see `rerun_when_head_changes`).
//!
//! Standard library only — a build script that needed a crate would have to be
//! vendored into every build.

use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    for name in ["P1_GIT_SHA", "P1_BUILD_DATE", "SOURCE_DATE_EPOCH"] {
        println!("cargo:rerun-if-env-changed={name}");
    }
    println!("cargo:rustc-env=P1_GIT_SHA={}", git_sha());
    println!("cargo:rustc-env=P1_BUILD_DATE={}", build_date());
}

/// Short sha from the environment, else from `git` in this checkout, else `unknown`.
fn git_sha() -> String {
    if let Some(sha) = env_value("P1_GIT_SHA") {
        return sha;
    }
    match git(&["rev-parse", "--short=12", "HEAD"]) {
        Some(sha) if !sha.is_empty() => {
            // The rerun-if-env-changed lines above switch off cargo's default "rerun
            // when a package file changes"; without a rerun-if-changed a local rebuild
            // after a new commit would keep this sha forever.
            rerun_when_head_changes();
            sha
        }
        // No git, no repository, or a broken one: the sha is not knowable here.
        _ => "unknown".to_string(),
    }
}

/// Name the files that hold the local HEAD sha, so cargo reruns this script when they
/// change: the HEAD file (under the worktree's git dir for a linked worktree), the
/// branch HEAD points to, and `packed-refs`. Only existing paths are named — cargo
/// treats a missing rerun-if-changed path as permanently stale — and a missing or
/// failing git contributes nothing.
fn rerun_when_head_changes() {
    let mut paths: Vec<String> = Vec::new();
    if let Some(path) = git(&["rev-parse", "--git-path", "HEAD"]) {
        paths.push(path);
    }
    if let Some(name) = git(&["rev-parse", "--symbolic-full-name", "HEAD"])
        && name.starts_with("refs/heads/")
        && let Some(path) = git(&["rev-parse", "--git-path", &name])
    {
        paths.push(path);
    }
    if let Some(path) = git(&["rev-parse", "--git-path", "packed-refs"]) {
        paths.push(path);
    }
    for path in paths {
        if let Some(path) = absolute(Path::new(&path))
            && path.exists()
        {
            println!("cargo:rerun-if-changed={}", path.display());
        }
    }
}

/// `path` made absolute against the manifest directory git ran in: `--git-path` is
/// relative to that directory in an ordinary checkout and already absolute in a linked
/// worktree.
fn absolute(path: &Path) -> Option<PathBuf> {
    if path.is_absolute() {
        Some(path.to_path_buf())
    } else {
        Some(manifest_dir()?.join(path))
    }
}

/// Trimmed stdout of `git <args>` run in the crate directory, or `None` when git is
/// missing or the command fails.
fn git(args: &[&str]) -> Option<String> {
    let out = Command::new("git")
        .args(args)
        .current_dir(manifest_dir()?)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// The crate directory cargo runs this script in.
fn manifest_dir() -> Option<PathBuf> {
    std::env::var_os("CARGO_MANIFEST_DIR").map(PathBuf::from)
}

/// UTC date from the environment, else from `SOURCE_DATE_EPOCH`, else `unknown`.
fn build_date() -> String {
    if let Some(date) = env_value("P1_BUILD_DATE") {
        return date;
    }
    match env_value("SOURCE_DATE_EPOCH").and_then(|epoch| epoch.parse::<i64>().ok()) {
        Some(epoch) => utc_date(epoch),
        None => "unknown".to_string(),
    }
}

/// The trimmed value of `name`, or `None` when it is unset or blank.
fn env_value(name: &str) -> Option<String> {
    match std::env::var(name) {
        Ok(value) if !value.trim().is_empty() => Some(value.trim().to_string()),
        _ => None,
    }
}

/// `YYYY-MM-DD` of a Unix timestamp (UTC), by the civil-from-days algorithm: no
/// calendar crate, no local timezone, no ceiling on which dates it can name.
fn utc_date(epoch_seconds: i64) -> String {
    let days = epoch_seconds.div_euclid(86_400);
    // Day 0 is 1970-01-01; the algorithm counts days from 0000-03-01.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if month <= 2 {
        yoe + era * 400 + 1
    } else {
        yoe + era * 400
    };
    format!("{year:04}-{month:02}-{day:02}")
}
