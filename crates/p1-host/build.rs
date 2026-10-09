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
//! Shipped route TOML is parsed here too, so credential trust never depends on
//! installation-prefix files (ADR-0110).

use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    compile_shipped_routes();
    for name in ["P1_GIT_SHA", "P1_BUILD_DATE", "SOURCE_DATE_EPOCH"] {
        println!("cargo:rerun-if-env-changed={name}");
    }
    println!("cargo:rustc-env=P1_GIT_SHA={}", git_sha());
    println!("cargo:rustc-env=P1_BUILD_DATE={}", build_date());
}

fn compile_shipped_routes() {
    let root = std::env::var_os("CARGO_MANIFEST_DIR").unwrap();
    let routes = Path::new(&root).join("../../routes");
    println!("cargo:rerun-if-changed={}", routes.display());
    println!(
        "cargo:rerun-if-changed={}",
        Path::new(&root).join("../../accounts").display()
    );
    let table = shipped_route_table(&routes);
    let out = std::env::var_os("OUT_DIR").unwrap();
    std::fs::write(Path::new(&out).join("shipped_routes.rs"), table).unwrap();
}

/// The `*.toml` files of `dir`, sorted; a missing directory holds none.
fn toml_files(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut files: Vec<_> = entries
        .map(|entry| entry.expect("shipped file entry").path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "toml"))
        .collect();
    files.sort();
    files
}

fn parse(path: &Path) -> toml::Value {
    let text = std::fs::read_to_string(path).expect("read shipped file");
    toml::from_str(&text).unwrap_or_else(|error| panic!("{}: {error}", path.display()))
}

/// The credential method of a shipped `[credential]` table (`kind` or `method`).
fn method(table: &toml::Value) -> &str {
    let method = table
        .get("credential")
        .and_then(|value| value.get("kind").or_else(|| value.get("method")))
        .and_then(toml::Value::as_str)
        .expect("shipped credential method");
    assert!(["api-key", "claude-code-oauth", "codex-oauth", "none"].contains(&method));
    method
}

/// The compiled credential anchor (ADR-0110, ADR-0139 §2): one row per id that names a
/// shipped credential — a route with its inline credential, a route by its default
/// account, and a shipped account by its id, its `store_id` and each legacy route id —
/// with an endpoint whose origin that credential may reach and its method. The accounts
/// are the ones beside the routes: `<routes>/../accounts`.
pub(crate) fn shipped_route_table(routes: &Path) -> String {
    let accounts: Vec<(PathBuf, toml::Value)> = toml_files(&routes.join("../accounts"))
        .into_iter()
        .map(|path| {
            let account = parse(&path);
            (path, account)
        })
        .collect();
    let account = |id: &str| {
        accounts
            .iter()
            .find(|(_, account)| account.get("id").and_then(toml::Value::as_str) == Some(id))
            .map(|(_, account)| account)
            .unwrap_or_else(|| panic!("shipped account `{id}` is missing"))
    };
    let files = toml_files(routes);
    assert!(!files.is_empty(), "no shipped routes to anchor credentials");
    let mut rows: Vec<(String, String, String)> = Vec::new();
    for path in files {
        let route = parse(&path);
        let field = |name: &str| {
            route
                .get(name)
                .and_then(toml::Value::as_str)
                .unwrap_or_else(|| panic!("{}: missing string {name}", path.display()))
        };
        let id = field("id");
        assert_eq!(Some(id), path.file_stem().and_then(|stem| stem.to_str()));
        let endpoint = field("endpoint");
        assert!(
            endpoint.starts_with("https://"),
            "shipped endpoint must be HTTPS"
        );
        // A route without its own credential is anchored by its default account's method.
        let kind = match route.get("account").and_then(toml::Value::as_str) {
            Some(default) => method(account(default)),
            None => method(&route),
        };
        rows.push((id.into(), endpoint.into(), kind.into()));
    }
    for (path, account) in &accounts {
        let field = |name: &str| account.get(name).and_then(toml::Value::as_str);
        let id = field("id").unwrap_or_else(|| panic!("{}: missing id", path.display()));
        assert_eq!(Some(id), path.file_stem().and_then(|stem| stem.to_str()));
        let kind = method(account);
        let mut ids = vec![id, field("store_id").unwrap_or(id)];
        if let Some(legacy) = account.get("legacy_routes").and_then(toml::Value::as_table) {
            ids.extend(legacy.keys().map(String::as_str));
        }
        let origins = account
            .get("origins")
            .and_then(toml::Value::as_array)
            .unwrap_or_else(|| panic!("{}: missing origins", path.display()));
        for origin in origins {
            let origin = origin.as_str().expect("an origin string");
            assert!(
                origin.starts_with("https://"),
                "shipped origin must be HTTPS"
            );
            for id in &ids {
                rows.push(((*id).into(), origin.into(), kind.into()));
            }
        }
    }
    rows.sort();
    rows.dedup();
    let mut table = String::from("const SHIPPED_ROUTES: &[(&str, &str, &str)] = &[\n");
    for (id, endpoint, kind) in rows {
        table.push_str(&format!("    ({id:?}, {endpoint:?}, {kind:?}),\n"));
    }
    table.push_str("];\n");
    table
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
