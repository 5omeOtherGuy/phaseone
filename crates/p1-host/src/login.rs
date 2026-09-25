//! `p1 login <route>`, `p1 login --list` and `p1 logout <route>` (ADR-0044,
//! spec `docs/design/credentials.md` §6).
//!
//! One pasted API key, read from stdin and written to p1's own store by `p1-auth`.
//! This module is the glue: the route lookup, the hidden-input guard, the key read
//! through the injected line source, and the human-facing lines. Nothing is verified
//! against the network — login stores, the first request verifies.
//!
//! The key is read from STDIN, never from an argument: arguments land in shell
//! history and in `ps`. It never reaches stdout, stderr, an error or a `Debug`; the
//! store file is the only place it is written. An OAuth route is a usage error:
//! a legacy one says which CLI login it borrows, and a `store_only` one says that
//! its credential belongs in p1's store, that `p1 login` reads no OAuth grant from
//! stdin, and that the CLI login is not read (ADR-0061). No OAuth flow is pretended.
//!
//! `p1 login <route> --from-claude-code [DIR]` (ADR-0074) is the one way an OAuth
//! entry gets into p1's store: it copies an existing Claude Code login — the
//! directory's `.credentials.json` — for a `claude-code-oauth` route. `p1-auth` reads
//! the file and writes the store; this module prints paths and routes, never a token.

use std::io::IsTerminal;
use std::process::{Command, Stdio};

use p1_auth::CredentialKind;

use crate::HostDeps;
use crate::SharedWriter;
use crate::routes::{RouteFile, load_all_routes};
use crate::run::{EXIT_CANCELLED, EXIT_FAILURE, EXIT_OK, EXIT_USAGE};

/// `p1 login <route>`: the production entry. The process's own stdin decides whether
/// the key is hidden.
pub async fn login(deps: &HostDeps, route_id: &str) -> i32 {
    let stdin_is_tty = std::io::stdin().is_terminal();
    if stdin_is_tty {
        restore_echo_on_interrupt();
    }
    login_with(deps, route_id, stdin_is_tty, &TerminalEcho).await
}

/// The same, with the terminal decisions injected: a test passes a fake echo control
/// and a fake "is a terminal" flag, and feeds the key through `deps.lines` — the seam
/// the interactive prompt loop reads stdin through.
pub async fn login_with(
    deps: &HostDeps,
    route_id: &str,
    stdin_is_tty: bool,
    echo: &dyn EchoControl,
) -> i32 {
    let routes = match load_all_routes(&deps.environment_dirs) {
        Ok(routes) => routes,
        Err(message) => {
            err(deps, &format!("error: {message}\n"));
            return EXIT_FAILURE;
        }
    };
    let route = match api_key_route(&routes, route_id) {
        Ok(route) => route,
        Err(message) => return usage_error(deps, &message),
    };
    let locations = crate::auth::locations(deps);
    // BEFORE the key is read: a store that must not be written is refused here, so
    // the key is never typed into a terminal for nothing (spec §6).
    if let Err(message) = p1_auth::store::check_writable(&locations) {
        err(deps, &format!("error: {message}\n"));
        return EXIT_FAILURE;
    }
    if stdin_is_tty {
        err(deps, &format!("key for {route_id} (input hidden): "));
    }
    let guard = if stdin_is_tty {
        match echo.disable() {
            Ok(guard) => Some(guard),
            // A key typed where echo cannot be switched off is a key on the screen:
            // refuse rather than read it visibly.
            Err(message) => {
                err(deps, &format!("error: {message}\n"));
                return EXIT_FAILURE;
            }
        }
    } else {
        None
    };
    let line = deps.lines.next_line().await;
    // Echo is back — on the error and the piped paths too — before anything is said.
    drop(guard);
    if stdin_is_tty {
        err(deps, "\n");
    }
    let Some(line) = line else {
        err(
            deps,
            &format!(
                "error: no key was read for route `{route_id}`: stdin ended; nothing was written\n"
            ),
        );
        return EXIT_FAILURE;
    };
    // One line, surrounding whitespace trimmed. The format check (printable ASCII,
    // no spaces, non-empty) lives with the store, which applies the same rule as
    // every other key source.
    if let Err(message) = p1_auth::store::put_api_key(route_id, line.trim(), &locations).await {
        err(deps, &format!("error: {message}\n"));
        return EXIT_FAILURE;
    }
    // Which source answers NOW, never a value (spec §4). An environment variable that
    // still overrides the store is named here, because that is the surprise ADR-0040
    // warned about.
    let report = p1_auth::describe(route_id, &route.credential, &locations);
    out(
        deps,
        &format!("stored for {route_id} · source now: {}\n", report.line()),
    );
    EXIT_OK
}

/// `p1 login <route> --from-claude-code [DIR]` (ADR-0074): copy the Claude Code login
/// in DIR into p1's store as this route's `oauth` entry. DIR defaults to the directory
/// the route borrows from (its `login_dir`, else the default Claude Code directory); a
/// leading `~` is expanded. A route that is not `claude-code-oauth`, or a directory
/// with no login, is a usage error naming the fix. No token is ever printed.
pub async fn from_claude_code(deps: &HostDeps, route_id: &str, dir: Option<&str>) -> i32 {
    let routes = match load_all_routes(&deps.environment_dirs) {
        Ok(routes) => routes,
        Err(message) => {
            err(deps, &format!("error: {message}\n"));
            return EXIT_FAILURE;
        }
    };
    let route = match find_route(&routes, route_id) {
        Ok(route) => route,
        Err(message) => return usage_error(deps, &message),
    };
    if route.credential.kind != CredentialKind::ClaudeCodeOauth {
        return usage_error(
            deps,
            &format!(
                "route `{route_id}` is a {} route; `--from-claude-code` imports a Claude Code \
                 login, which only a claude-code-oauth route reads",
                route.credential.kind.label()
            ),
        );
    }
    let locations = crate::auth::locations(deps);
    let source = match dir {
        Some(dir) => locations.expand_home(dir),
        None => locations.claude_code_dir(route.credential.login_dir.as_deref()),
    };
    let Some(source) = source else {
        return usage_error(
            deps,
            "cannot locate the Claude Code config directory: set HOME, or name the directory \
             after `--from-claude-code`",
        );
    };
    match p1_auth::store::import_claude_code_login(route_id, &source, &locations).await {
        Ok(()) => {}
        Err(p1_auth::store::ImportError::NoLogin(message)) => {
            return usage_error(deps, &message);
        }
        Err(p1_auth::store::ImportError::Failed(message)) => {
            err(deps, &format!("error: {message}\n"));
            return EXIT_FAILURE;
        }
    }
    let report = p1_auth::describe(route_id, &route.credential, &locations);
    out(
        deps,
        &format!(
            "imported the Claude Code login in {} for {route_id} · source now: {}\n",
            source.display(),
            report.line()
        ),
    );
    EXIT_OK
}

/// `p1 login --list`: every route, its credential kind, and which source its
/// credential comes from right now (spec §4). Never a value.
pub fn list(deps: &HostDeps) -> i32 {
    let routes = match load_all_routes(&deps.environment_dirs) {
        Ok(routes) => routes,
        Err(message) => {
            err(deps, &format!("error: {message}\n"));
            return EXIT_FAILURE;
        }
    };
    let locations = crate::auth::locations(deps);
    let id_width = routes.iter().map(|route| route.id.len()).max().unwrap_or(0);
    let kind_width = routes
        .iter()
        .map(|route| route.credential.kind.label().len())
        .max()
        .unwrap_or(0);
    let mut text = String::new();
    for route in &routes {
        let report = p1_auth::describe(&route.id, &route.credential, &locations);
        text.push_str(&format!(
            "{:<id_width$}  {:<kind_width$}  {}\n",
            route.id,
            route.credential.kind.label(),
            report.line()
        ));
    }
    out(deps, &text);
    EXIT_OK
}

/// `p1 logout <route>`: remove that route's entry from p1's store. A missing entry
/// is reported, not an error.
pub async fn logout(deps: &HostDeps, route_id: &str) -> i32 {
    let routes = match load_all_routes(&deps.environment_dirs) {
        Ok(routes) => routes,
        Err(message) => {
            err(deps, &format!("error: {message}\n"));
            return EXIT_FAILURE;
        }
    };
    if let Err(message) = api_key_route(&routes, route_id) {
        return usage_error(deps, &message);
    }
    let locations = crate::auth::locations(deps);
    match p1_auth::store::remove(route_id, &locations).await {
        Ok(true) => {
            out(deps, &format!("removed {route_id} from p1's store\n"));
            EXIT_OK
        }
        Ok(false) => {
            out(deps, &format!("no {route_id} entry in p1's store\n"));
            EXIT_OK
        }
        Err(message) => {
            err(deps, &format!("error: {message}\n"));
            EXIT_FAILURE
        }
    }
}

/// The one route a login or a logout names: it must be a loaded route file whose
/// credential kind is `api-key`. Anything else is a usage error that says where that
/// login comes from (spec §6).
fn api_key_route<'a>(routes: &'a [RouteFile], route_id: &str) -> Result<&'a RouteFile, String> {
    let route = find_route(routes, route_id)?;
    match route.credential.kind {
        CredentialKind::ApiKey => Ok(route),
        // A route that sends no credential has nothing to log in to (issue #134):
        // the egress proxy injects the provider's credential, and p1 stores none.
        CredentialKind::None => Err(format!(
            "route `{route_id}` declares `kind = \"none\"`: p1 sends no credential on this \
             route, so there is nothing to log in to; the egress proxy injects the proxy \
             credential"
        )),
        // A self-contained OAuth route reads p1's own store only (ADR-0061). Sending
        // the operator to that CLI's login would be wrong: this route does not read
        // it. p1 has no OAuth flow, so the error says exactly that and never
        // pretends one exists; a Claude Code login can be imported (ADR-0074).
        kind if route.credential.store_only => Err(format!(
            "route `{route_id}` is a {} route with `store_only`: its credential is read from \
             p1's own store, and `p1 login` reads no OAuth grant from stdin. p1 has no \
             independent OAuth flow, so the grant has to come from elsewhere; the {} login is \
             NOT read by this route{}",
            kind.name(),
            login_owner(kind),
            import_hint(kind, route_id)
        )),
        kind => Err(format!(
            "route `{route_id}` is a {} route; its login comes from {}, not from p1's store{}",
            kind.name(),
            login_owner(kind),
            import_hint(kind, route_id)
        )),
    }
}

/// The import a `claude-code-oauth` route offers instead of a pasted key (ADR-0074).
fn import_hint(kind: CredentialKind, route_id: &str) -> String {
    if kind == CredentialKind::ClaudeCodeOauth {
        format!(
            "; to copy a Claude Code login into p1's store, run `p1 login {route_id} \
             --from-claude-code [DIR]`"
        )
    } else {
        String::new()
    }
}

/// The route a login names, or the usage error that lists the loaded ones.
fn find_route<'a>(routes: &'a [RouteFile], route_id: &str) -> Result<&'a RouteFile, String> {
    routes
        .iter()
        .find(|route| route.id == route_id)
        .ok_or_else(|| {
            let available = if routes.is_empty() {
                "none".to_string()
            } else {
                routes
                    .iter()
                    .map(|route| route.id.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            };
            format!("route `{route_id}` was not found; available: {available}")
        })
}

/// Where a route whose credential is not an API key gets its login (spec §6).
fn login_owner(kind: CredentialKind) -> &'static str {
    match kind {
        CredentialKind::ApiKey => "the documented key environment variable",
        CredentialKind::ClaudeCodeOauth => "the Claude Code CLI (`claude`)",
        CredentialKind::CodexOauth => "the Codex CLI (`codex login`)",
        // Unreachable through `api_key_route`, which answers for `none` first: the
        // credential is the egress proxy's (issue #134), not a login p1 could store.
        CredentialKind::None => "the egress proxy that injects it",
    }
}

/// A usage error: the message on stderr, exit 2 (the process contract).
fn usage_error(deps: &HostDeps, message: &str) -> i32 {
    err(deps, &format!("error: {message}\n"));
    EXIT_USAGE
}

/// Switches the terminal's input echo off for one hidden read (spec §6).
///
/// A trait, so the read is testable without a terminal: a test records what was
/// switched off and when it was put back.
pub trait EchoControl: Send + Sync {
    /// Echo is off from the moment this returns, and back on when the guard is
    /// dropped — on every path, including a read that fails or a run that is
    /// cancelled.
    fn disable(&self) -> Result<Guard, String>;
}

/// Puts back whatever an [`EchoControl`] switched off. `Drop` is the only exit from a
/// cancelled or failed read, so restoring there covers error and cancel alike.
pub struct Guard {
    restore: Option<Box<dyn FnOnce() + Send>>,
}

impl Guard {
    /// A guard that runs `restore` when it is dropped.
    pub fn from_restore(restore: impl FnOnce() + Send + 'static) -> Self {
        Self {
            restore: Some(Box::new(restore)),
        }
    }
}

impl Drop for Guard {
    fn drop(&mut self) {
        if let Some(restore) = self.restore.take() {
            restore();
        }
    }
}

/// The real terminal: `stty` on the terminal behind stdin.
///
/// `stty` and not a crate: `crossterm` (already a dependency, for the TUI) only
/// offers raw mode through a global state machine, and raw mode also takes the signal
/// keys and line editing away for the read; `stty -echo` changes ECHO alone and is the
/// POSIX tool for exactly this flag, so the dependency rule stays intact.
pub struct TerminalEcho;

impl EchoControl for TerminalEcho {
    fn disable(&self) -> Result<Guard, String> {
        stty(&["-echo"])?;
        Ok(Guard::from_restore(|| {
            // Nothing better to do if this fails: the run is over either way, and the
            // message would be about a terminal the user can already see.
            let _ = stty(&["echo"]);
        }))
    }
}

/// Run `stty` with these arguments on the terminal behind stdin (fd 0), silently.
fn stty(args: &[&str]) -> Result<(), String> {
    let status = Command::new("stty")
        .args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    match status {
        Ok(status) if status.success() => Ok(()),
        Ok(status) => Err(format!(
            "`stty {}` exited with {status}: this terminal cannot hide the key; pipe the key in \
             instead",
            args.join(" ")
        )),
        Err(error) => Err(format!(
            "`stty` could not be run ({error}): this terminal cannot hide the key; pipe the key \
             in instead"
        )),
    }
}

/// Echo is off while the key is typed, and `Drop` never runs on a signal: a task puts
/// echo back and gives up the run with the interrupt exit code, so a Ctrl-C cannot
/// leave the terminal without echo.
fn restore_echo_on_interrupt() {
    tokio::spawn(async {
        // A host whose signals cannot be watched keeps the ordinary path: the guard
        // still restores echo on every code path.
        if tokio::signal::ctrl_c().await.is_err() {
            return;
        }
        let _ = stty(&["echo"]);
        std::process::exit(EXIT_CANCELLED);
    });
}

/// One line to the host's injected stdout.
fn out(deps: &HostDeps, text: &str) {
    write_to(&deps.stdout, text);
}

/// One line to the host's injected stderr.
fn err(deps: &HostDeps, text: &str) {
    write_to(&deps.stderr, text);
}

fn write_to(stream: &SharedWriter, text: &str) {
    use std::io::Write;
    let mut writer = stream.lock().unwrap();
    let _ = writer.write_all(text.as_bytes());
    let _ = writer.flush();
}
