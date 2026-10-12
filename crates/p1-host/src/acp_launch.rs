//! `p1 acp` with several sessions (ADR-0156): the host serves the ACP router and
//! starts one session process per `session/new`, this same binary with the same
//! run options, the session's folder as `--workspace`, and `--serve-session`.
//!
//! The session process assembles its own agent for that folder, with p1's sandbox,
//! access and approval rules exactly as for `p1 --workspace <folder>` run where
//! `p1 acp` runs: it inherits the router's directory, so a relative path option means
//! what it meant to the router. Its stderr is this process's stderr; its stdin and
//! stdout are the router's pipe to it.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;

use p1_contracts::frontend::StoredSession;
use p1_contracts::{Launched, SessionLauncher};

use crate::cli::{Options, SERVE_SESSION};

/// Serve `p1 acp` on the process's stdin and stdout; the exit code.
pub async fn serve(options: &Options, image_prompts: bool) -> Result<i32, String> {
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
    // ADR-0171: the host's one state root and session catalog; ACP owns its `acp` part.
    let state = crate::state::root(&p1_auth::Locations::from_process()).ok_or_else(|| {
        "p1 acp needs P1_STATE_DIR, XDG_STATE_HOME or HOME for session journals".to_string()
    })?;
    let state = std::path::absolute(state).map_err(|e| e.to_string())?;
    let archive = Arc::new(crate::session::archive::JournalArchive::new(
        crate::session::catalog::SessionCatalog::at_state(&state)
            .front_end_dir(crate::session::catalog::ACP),
    ));
    Ok(p1_acp::router::serve_with_archive(
        Box::new(tokio::io::stdin()),
        Box::new(tokio::io::stdout()),
        launcher,
        default_workspace,
        Some(archive),
        image_prompts,
    )
    .await)
}

/// Whether the model the run starts with takes image input on its route: `p1 acp`'s
/// `promptCapabilities.image` (#694). `initialize` is answered before any session builds its
/// provider, so this reads the selected profile's `input_modalities` and the route's adapter
/// ([`crate::routes::IMAGE_ADAPTERS`]); a selection that does not load advertises nothing.
/// Each session still checks an image prompt against its live route.
pub(crate) fn image_prompts(environment_dirs: &[PathBuf], choice: &crate::models::Choice) -> bool {
    let Ok(environment) = p1_assembly::load_environment(&choice.environment, environment_dirs)
    else {
        return false;
    };
    let profile = match &choice.profile {
        Some(id) => crate::models::load_profile(environment_dirs, id).ok(),
        None => environment.profile.clone(),
    };
    profile.is_some_and(|profile| {
        profile
            .input_modalities
            .contains(&p1_contracts::InputModality::Image)
    }) && crate::routes::load_route_by_id(environment_dirs, &environment.provider)
        .is_ok_and(|route| crate::routes::IMAGE_ADAPTERS.contains(&route.adapter.as_str()))
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

/// Starts each session as a child process of this same binary (ADR-0156, ADR-0167).
///
/// Front-end neutral: it implements the [`SessionLauncher`] port of p1-contracts and
/// knows nothing of ACP, so a GUI server can reuse it unchanged. The child's stdin and
/// stdout are the pipes; the bytes' meaning is the front end's.
struct ProcessLauncher {
    program: PathBuf,
    args: Vec<OsString>,
}

impl SessionLauncher for ProcessLauncher {
    fn launch(&self, workspace: &Path) -> std::io::Result<Launched> {
        self.start(workspace, None, false)
    }

    fn launch_stored(&self, session: &StoredSession, resume: bool) -> std::io::Result<Launched> {
        self.start(&session.workspace, Some(&session.journal), resume)
    }
}

impl ProcessLauncher {
    fn start(
        &self,
        workspace: &Path,
        journal: Option<&Path>,
        resume: bool,
    ) -> std::io::Result<Launched> {
        let mut command = tokio::process::Command::new(&self.program);
        command.args(&self.args).arg("--workspace").arg(workspace);
        if let Some(journal) = journal {
            command.arg("--session").arg(journal);
        }
        if resume {
            command.arg("--resume");
        }
        let mut child = command
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

    /// A vision profile takes images only on an adapter with an image wire (#694).
    #[test]
    fn image_prompts_follow_the_selected_profile_and_route_adapter() {
        let fixture = p1_testkit::catalog::CatalogFixture::new();
        fixture.adapter("messages", "anthropic-messages", "messages", "vision");
        fixture.adapter("chat", "openai-chat", "chat", "text");
        fixture.write_profile(
            "vision",
            &(p1_testkit::catalog::profile_toml("anthropic-messages", "vision")
                + "input_modalities = [\"text\", \"image\"]\n"),
        );
        let dirs = fixture.environment_dirs();
        let choice = |environment: &str, profile: Option<&str>| crate::models::Choice {
            environment: environment.to_string(),
            profile: profile.map(str::to_string),
            effort: None,
            account: None,
            route: None,
        };
        assert!(image_prompts(&dirs, &choice("messages", None)));
        assert!(image_prompts(&dirs, &choice("messages", Some("vision"))));
        assert!(!image_prompts(&dirs, &choice("chat", None)));
        assert!(!image_prompts(&dirs, &choice("messages", Some("text"))));
        assert!(!image_prompts(&dirs, &choice("no-such-environment", None)));

        fixture.write_route(
            "chatty",
            &p1_testkit::catalog::route_toml("openai-chat", "chatty", "vision"),
        );
        fixture.write_environment(
            "chatty",
            "route = \"chatty\"\nprofile = \"vision\"\n",
            "PROMPT",
        );
        crate::routes::load_route_by_id(&dirs, "chatty").unwrap();
        assert!(!image_prompts(&dirs, &choice("chatty", None)));
    }
}
