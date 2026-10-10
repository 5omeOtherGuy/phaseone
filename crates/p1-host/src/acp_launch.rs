//! `p1 acp` with several sessions (ADR-0156): the host serves the ACP router and
//! starts one session process per `session/new`, this same binary with the same
//! run options, the session's folder as `--workspace`, and `--serve-session`.
//!
//! The session process assembles its own agent for that folder, with p1's sandbox,
//! access and approval rules exactly as for `p1 --workspace <folder>`. Its stderr is
//! this process's stderr; its stdin and stdout are the router's pipe to it.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;

use p1_acp::router::{Launched, SessionLauncher};

use crate::cli::{Options, SERVE_SESSION};

/// Serve `p1 acp` on the process's stdin and stdout; the exit code.
pub async fn serve(options: &Options) -> Result<i32, String> {
    let program = std::env::current_exe()
        .map_err(|error| format!("p1 acp cannot find its own executable: {error}"))?;
    let launcher = Arc::new(ProcessLauncher {
        program,
        args: session_args(std::env::args_os().skip(1)),
    });
    // The operator's `--workspace` is the folder of a `session/new` that names none.
    let default_workspace = match &options.workspace {
        Some(path) => Some(
            std::path::absolute(path)
                .map_err(|error| format!("--workspace {}: {error}", path.display()))?,
        ),
        None => None,
    };
    Ok(p1_acp::router::serve(
        Box::new(tokio::io::stdin()),
        Box::new(tokio::io::stdout()),
        launcher,
        default_workspace,
    )
    .await)
}

/// The session process's arguments: the router's own (`acp` and its run options)
/// without `--workspace`, which each session sets to its folder, plus the flag that
/// makes the process one session.
fn session_args(args: impl IntoIterator<Item = OsString>) -> Vec<OsString> {
    let mut kept = Vec::new();
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        if arg == "--workspace" {
            args.next();
        } else if !arg.to_string_lossy().starts_with("--workspace=") {
            kept.push(arg);
        }
    }
    kept.push(SERVE_SESSION.into());
    kept
}

struct ProcessLauncher {
    program: PathBuf,
    args: Vec<OsString>,
}

impl SessionLauncher for ProcessLauncher {
    fn launch(&self, workspace: &Path) -> std::io::Result<Launched> {
        let mut child = tokio::process::Command::new(&self.program)
            .args(&self.args)
            .arg("--workspace")
            .arg(workspace)
            .current_dir(workspace)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            // A router that is itself killed takes its sessions with it.
            .kill_on_drop(true)
            .spawn()?;
        let stdin = child.stdin.take().expect("stdin is piped");
        let stdout = child.stdout.take().expect("stdout is piped");
        Ok(Launched {
            reader: Box::new(stdout),
            writer: Box::new(stdin),
            exited: Box::pin(async move {
                let _ = child.wait().await;
            }),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn os(parts: &[&str]) -> Vec<OsString> {
        parts.iter().map(OsString::from).collect()
    }

    #[test]
    fn a_session_process_keeps_the_run_options_and_drops_the_workspace() {
        assert_eq!(
            session_args(os(&[
                "acp",
                "--env",
                "deepseek",
                "--workspace",
                "/w",
                "--ask",
                "--workspace=/x",
            ])),
            os(&["acp", "--env", "deepseek", "--ask", SERVE_SESSION])
        );
    }
}
