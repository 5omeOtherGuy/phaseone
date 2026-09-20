//! The p1 composition root.
//!
//! This crate is the ONE place that names concrete provider and tool crates. It
//! builds the [`p1_assembly::Catalog`], loads an environment, assembles an agent,
//! and drives it headless or through a plain terminal prompt loop.
//!
//! Everything the host touches outside the process is injected through
//! [`HostDeps`]: the output writers, the input line source, the provider
//! transport, the credential-bearing provider factories, the date, and the
//! Ctrl-C source. Tests therefore run the WHOLE host against fakes by registering
//! a provider factory through the test-only catalog hook.
//!
//! Behaviour is specified in `docs/design/assembly.md` §Host; that note is
//! authoritative. Where the brief and the spec are silent, the simplest
//! behaviour was chosen and listed in the handoff.

pub mod activity;
pub mod auth;
pub mod catalog;
pub mod cli;
pub mod policy;
pub mod render;
pub mod routes;
pub mod run;
pub mod session;

use std::io::Write;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use catalog::CatalogHook;
use p1_provider_http::Transport;

/// Whether the crate was compiled with the `delegation` cargo feature.
///
/// Integration tests use this to assert the feature-off behaviour (unknown
/// `worker_*` modules) and the feature-on behaviour without separate test files.
pub const DELEGATION_ENABLED: bool = cfg!(feature = "delegation");

/// A writer shared between the driver and the event renderer. The renderer is
/// `Send + Sync`, so the writer needs interior mutability.
pub type SharedWriter = Arc<Mutex<Box<dyn Write + Send>>>;

/// The injected line source (stdin in the real host, a script in tests).
///
/// `next_line` returns `None` at EOF. It takes `&self` so one source can be
/// shared by the prompt loop and the authorization policy.
pub trait LineSource: Send + Sync {
    fn next_line<'a>(
        &'a self,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Option<String>> + Send + 'a>>;
}

/// The injected Ctrl-C source. `recv` resolves on the NEXT interrupt and may be
/// called repeatedly.
pub trait InterruptSource: Send + Sync {
    fn recv<'a>(&'a self) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + 'a>>;
}

/// Lines from any async reader, newline trimmed.
///
/// `next_line` is cancel-safe: the prompt loop drops a pending read whenever the
/// agent has to run an inbox turn, so bytes read so far are kept HERE, not in the
/// future, and the next call continues the same line.
pub struct ReaderLines<R> {
    state: tokio::sync::Mutex<(tokio::io::BufReader<R>, Vec<u8>)>,
}

/// Real stdin.
pub type StdinLines = ReaderLines<tokio::io::Stdin>;

impl<R: tokio::io::AsyncRead + Unpin + Send> ReaderLines<R> {
    pub fn from_reader(reader: R) -> Self {
        Self {
            state: tokio::sync::Mutex::new((tokio::io::BufReader::new(reader), Vec::new())),
        }
    }
}

impl StdinLines {
    pub fn new() -> Self {
        Self::from_reader(tokio::io::stdin())
    }
}

impl Default for StdinLines {
    fn default() -> Self {
        Self::new()
    }
}

impl<R: tokio::io::AsyncRead + Unpin + Send> LineSource for ReaderLines<R> {
    fn next_line<'a>(
        &'a self,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Option<String>> + Send + 'a>> {
        Box::pin(async move {
            use tokio::io::AsyncBufReadExt;
            let mut state = self.state.lock().await;
            let (reader, partial) = &mut *state;
            // `read_until` appends what it has read to `partial` even when this
            // future is dropped mid-line (`read_line` would lose it).
            let read = reader.read_until(b'\n', partial).await;
            if partial.is_empty() || (read.is_err() && !partial.ends_with(b"\n")) {
                return None;
            }
            let line = String::from_utf8_lossy(partial)
                .trim_end_matches(['\n', '\r'])
                .to_string();
            partial.clear();
            Some(line)
        })
    }
}

/// Real Ctrl-C via `tokio::signal`. Each `recv` waits for the next signal.
pub struct SignalInterrupt;

impl InterruptSource for SignalInterrupt {
    fn recv<'a>(&'a self) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + 'a>> {
        Box::pin(async move {
            let _ = tokio::signal::ctrl_c().await;
        })
    }
}

/// Everything the host reaches outside its own process. Injected so tests can run
/// the whole host against fakes.
pub struct HostDeps {
    pub stdout: SharedWriter,
    pub stderr: SharedWriter,
    pub lines: Arc<dyn LineSource>,
    /// The network transport real provider factories use. Never exercised by
    /// `env show` or the fake-provider tests.
    pub transport: Arc<dyn Transport>,
    /// Prompt `{{date}}`, already formatted `YYYY-MM-DD` (UTC).
    pub date: String,
    pub interrupt: Arc<dyn InterruptSource>,
    /// Environment search directories, highest priority first (see `main.rs`).
    pub environment_dirs: Vec<std::path::PathBuf>,
    /// Whether stdout is a terminal (reasoning dimming is only for a TTY).
    pub stdout_is_tty: bool,
    /// The home directory the sandbox hides, from `HOME`. Tests inject a fake one.
    pub home: Option<std::path::PathBuf>,
    /// The runtime directory the sandbox replaces, from `XDG_RUNTIME_DIR`.
    pub runtime_dir: Option<std::path::PathBuf>,
    /// A snapshot of the environment `shell` commands are rebuilt from (the
    /// allow-list still filters it). `None` means the shell tool reads the
    /// process environment at construction; tests inject a fixture here instead
    /// of mutating the process environment.
    pub shell_env: Option<Vec<(std::ffi::OsString, std::ffi::OsString)>>,
    /// Test-only hook: called with the fully built catalog, after the built-in
    /// providers and tools are registered, so a test can add or replace entries.
    pub catalog_hook: Option<CatalogHook>,
    /// The delegation service. `run` sets this before building the catalog; the
    /// catalog registers the `worker_*` tools only when it is present.
    #[cfg(feature = "delegation")]
    pub worker_service: Option<Arc<dyn p1_workers::WorkerService>>,
}

impl HostDeps {
    /// Construct the injected dependencies. `catalog_hook` and, under the
    /// delegation feature, `worker_service` start empty; `home` and `runtime_dir`
    /// are read from `HOME` and `XDG_RUNTIME_DIR` (tests inject fakes). The sandbox
    /// selection is NOT here: it is parsed command-line state on [`cli::Options`].
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        stdout: SharedWriter,
        stderr: SharedWriter,
        lines: Arc<dyn LineSource>,
        transport: Arc<dyn Transport>,
        date: String,
        interrupt: Arc<dyn InterruptSource>,
        environment_dirs: Vec<std::path::PathBuf>,
        stdout_is_tty: bool,
    ) -> Self {
        Self {
            stdout,
            stderr,
            lines,
            transport,
            date,
            interrupt,
            environment_dirs,
            stdout_is_tty,
            home: std::env::var_os("HOME").map(std::path::PathBuf::from),
            runtime_dir: std::env::var_os("XDG_RUNTIME_DIR").map(std::path::PathBuf::from),
            shell_env: None,
            catalog_hook: None,
            #[cfg(feature = "delegation")]
            worker_service: None,
        }
    }
}

/// Format `SystemTime` as `YYYY-MM-DD` UTC, by hand (no date crate).
pub fn format_date(time: SystemTime) -> String {
    let seconds = time
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or(0);
    let days = seconds.div_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    format!("{year:04}-{month:02}-{day:02}")
}

/// Today, UTC.
pub fn today_utc() -> String {
    format_date(SystemTime::now())
}

/// Howard Hinnant's `civil_from_days`: days since the Unix epoch to a civil date.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if month <= 2 { year + 1 } else { year }, month, day)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn formats_the_unix_epoch_and_a_leap_day() {
        assert_eq!(format_date(UNIX_EPOCH), "1970-01-01");
        // 2024-02-29T12:34:56Z
        let leap = UNIX_EPOCH + Duration::from_secs(1_709_210_096);
        assert_eq!(format_date(leap), "2024-02-29");
    }
}
