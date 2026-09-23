//! The `p1` binary: parse the arguments, build a current-thread runtime, and hand
//! the injected dependencies to [`p1_host::run`].

use std::io::IsTerminal;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use p1_host::{HostDeps, SharedWriter, SignalInterrupt, StdinLines, cli, run};

fn main() -> std::process::ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let options = match cli::parse(&args) {
        Ok(options) => options,
        Err(error) => {
            eprintln!("error: {error}\n\n{}", cli::usage());
            return std::process::ExitCode::from(2);
        }
    };

    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("error: could not start the async runtime: {error}");
            return std::process::ExitCode::from(1);
        }
    };

    let stdout: SharedWriter = Arc::new(Mutex::new(Box::new(std::io::stdout())));
    let stderr: SharedWriter = Arc::new(Mutex::new(Box::new(std::io::stderr())));
    let stdout_is_tty = std::io::stdout().is_terminal();
    let mut deps = HostDeps::new(
        stdout,
        stderr,
        Arc::new(StdinLines::new()),
        Arc::new(p1_provider_http::ReqwestTransport::new()),
        p1_host::today_utc(),
        Arc::new(SignalInterrupt),
        environment_dirs(),
        stdout_is_tty,
    );
    #[cfg(feature = "shadow-hook")]
    {
        let env = Arc::new(|name: &str| std::env::var_os(name));
        let configured = p1_host::models::load_settings(&p1_host::auth::locations(&deps))
            .ok()
            .and_then(|settings| settings.shadow)
            .and_then(|shadow| shadow.brain_packet_shadow);
        if let Some(binary) = configured.or_else(|| p1_hook_shadow::find_binary(env.as_ref())) {
            deps.shadow = Some(Arc::new(p1_hook_shadow::ShadowHook::new(binary, env)));
        }
    }

    let code = runtime.block_on(run::run(&mut deps, options));
    std::process::ExitCode::from(code as u8)
}

/// Environment search directories, highest priority first:
/// `$P1_CONFIG_DIR/environments` (default `~/.config/p1/environments`), then
/// `$P1_ENVIRONMENTS_DIR`, else `<exe dir>/../share/p1/environments`; in debug
/// builds the source-tree `environments/` directory is appended so `cargo run`
/// works without an install step.
fn environment_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    let config_base = std::env::var_os("P1_CONFIG_DIR")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config/p1")));
    if let Some(base) = config_base {
        dirs.push(base.join("environments"));
    }
    if let Some(dir) = std::env::var_os("P1_ENVIRONMENTS_DIR") {
        dirs.push(PathBuf::from(dir));
    } else {
        if let Ok(exe) = std::env::current_exe()
            && let Some(bin) = exe.parent()
        {
            dirs.push(bin.join("../share/p1/environments"));
        }
        if cfg!(debug_assertions) {
            dirs.push(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../environments"));
        }
    }
    dirs
}
