//! The CLI side of the session catalog (ADR-0171): where a run's journal goes when the
//! command line names no file, and `p1 sessions` / `p1 sessions prune`, which also
//! collects the attachments no remaining session references (ADR-0185).
//!
//! - an interactive run (no prompt, stdin a terminal) without `--session` journals to a
//!   new [`catalog::CLI`] session; a headless one stays in memory, because workers and
//!   scripts name their files;
//! - `--resume-id ID` continues catalog session `ID`, any front end;
//! - an interactive `--resume` without `--session` continues the workspace's newest one.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::HostDeps;
use crate::cli::{Command, Options, SessionsOptions};
use crate::run::{EXIT_FAILURE, EXIT_OK, RunError, write_stderr, write_stdout};
use crate::session::catalog::{self, SessionCatalog, Summary};

/// A session the host placed in the catalog for this run.
pub(crate) struct Placed {
    pub id: String,
    /// The session directory, when this run created it (and owns cleaning it up).
    pub created: Option<PathBuf>,
    pub journal: PathBuf,
}

/// The catalog under this host's state directory.
fn open_catalog(deps: &HostDeps) -> Result<SessionCatalog, String> {
    let state = crate::state::root(&crate::auth::locations(deps))
        .ok_or("no state directory for sessions: set P1_STATE_DIR, XDG_STATE_HOME or HOME")?;
    let state =
        std::path::absolute(&state).map_err(|error| format!("{}: {error}", state.display()))?;
    Ok(SessionCatalog::at_state(&state))
}

fn workspace_of(options: &Options) -> Result<PathBuf, String> {
    match &options.workspace {
        Some(path) => Ok(path.clone()),
        None => std::env::current_dir()
            .map_err(|error| format!("cannot determine the current directory: {error}")),
    }
}

/// Place this run's journal in the catalog when the command line names no file: set
/// `options.session` (and `resume`) to the catalog's journal and `options.session_id` to
/// its id. `Ok(None)` leaves the run as it was: an explicit `--session`, a headless run,
/// a non-terminal stdin, or an ACP process (whose router passes `--session`).
pub(crate) fn place(deps: &HostDeps, options: &mut Options) -> Result<Option<Placed>, RunError> {
    let Command::Run { prompt } = &options.command else {
        return Ok(None);
    };
    let headless = prompt.is_some();
    if options.session.is_some() {
        return Ok(None);
    }
    if let Some(id) = options.session_id.clone() {
        let entry = open_catalog(deps)
            .and_then(|catalog| catalog.find(&id))
            .map_err(RunError::usage)?;
        // A session continues in the workspace it was started in, not wherever the
        // command happens to run.
        match &options.workspace {
            None => options.workspace = Some(entry.workspace.clone()),
            Some(given) => {
                let canonical = given.canonicalize().map_err(|error| {
                    RunError::usage(format!("--workspace {}: {error}", given.display()))
                })?;
                if canonical != entry.workspace {
                    return Err(RunError::usage(format!(
                        "session `{id}` belongs to the workspace {}, not {}",
                        entry.workspace.display(),
                        canonical.display()
                    )));
                }
            }
        }
        options.session = Some(entry.journal.clone());
        options.resume = true;
        return Ok(Some(Placed {
            id,
            created: None,
            journal: entry.journal,
        }));
    }
    if options.resume {
        // A headless `--resume`, or one whose stdin is a script, must name its file:
        // `run` reports it.
        if headless || !deps.stdin_is_tty {
            return Ok(None);
        }
        let workspace = workspace_of(options).map_err(RunError::usage)?;
        let newest = open_catalog(deps)
            .and_then(|catalog| catalog.newest_of(&workspace))
            .map_err(RunError::usage)?
            .ok_or_else(|| {
                RunError::usage(format!(
                    "no saved session of {} that no other process runs (`p1 sessions` lists them)",
                    workspace.display()
                ))
            })?;
        options.session = Some(newest.journal.clone());
        options.session_id = Some(newest.id.clone());
        return Ok(Some(Placed {
            id: newest.id,
            created: None,
            journal: newest.journal,
        }));
    }
    if headless || !deps.stdin_is_tty {
        return Ok(None);
    }
    let created = workspace_of(options)
        .and_then(|workspace| open_catalog(deps)?.create(catalog::CLI, &workspace));
    match created {
        Ok(entry) => {
            options.session = Some(entry.journal.clone());
            options.session_id = Some(entry.id.clone());
            Ok(Some(Placed {
                id: entry.id,
                created: Some(entry.directory),
                journal: entry.journal,
            }))
        }
        // Journaling by default must not stop a run the operator did not ask to journal.
        Err(error) => {
            write_stderr(
                deps,
                &format!("warning: this session is not saved: {error}\n"),
            );
            Ok(None)
        }
    }
}

/// After the run: remove a session this run created that holds no record (an assembly
/// error before the journal existed, or a run quit before its first prompt), else name
/// the saved session when the run got as far as its loop (`ran`).
pub(crate) fn finished(deps: &HostDeps, placed: &Placed, ran: bool) {
    if let Some(directory) = &placed.created {
        let empty = match p1_journal::load(&placed.journal) {
            Ok(loaded) => loaded.records.is_empty(),
            Err(_) => !placed.journal.exists(),
        };
        if empty {
            let _ = std::fs::remove_dir_all(directory);
            return;
        }
    }
    if ran && placed.journal.is_file() {
        write_stderr(
            deps,
            &format!(
                "session {} saved; continue it with: p1 --resume-id {}\n",
                placed.id, placed.id
            ),
        );
    }
}

/// Run one `p1 sessions` command.
pub fn sessions(deps: &HostDeps, options: &Options, sessions: &SessionsOptions) -> i32 {
    match run_sessions(deps, options, sessions) {
        Ok(code) => code,
        Err(message) => {
            write_stderr(deps, &format!("error: {message}\n"));
            EXIT_FAILURE
        }
    }
}

fn run_sessions(
    deps: &HostDeps,
    options: &Options,
    sessions: &SessionsOptions,
) -> Result<i32, String> {
    let catalog = open_catalog(deps)?;
    let listed = catalog.summaries()?;
    let mut summaries = listed.clone();
    if let Some(workspace) = &options.workspace {
        let workspace = workspace
            .canonicalize()
            .map_err(|error| format!("--workspace {}: {error}", workspace.display()))?;
        summaries.retain(|summary| summary.workspace.as_deref() == Some(workspace.as_path()));
    }
    match sessions {
        SessionsOptions::List { json: true } => {
            let rows: Vec<serde_json::Value> = summaries.iter().map(json_row).collect();
            let text = serde_json::to_string_pretty(&rows).map_err(|error| error.to_string())?;
            write_stdout(deps, &(text + "\n"));
        }
        SessionsOptions::List { json: false } => {
            write_stdout(deps, &table(&summaries));
        }
        SessionsOptions::Prune {
            older_than_days,
            yes,
        } => {
            let older_than = Duration::from_secs(older_than_days.saturating_mul(86_400));
            let now = SystemTime::now();
            let candidates = catalog::prune_candidates(&summaries, older_than, now);
            let pruned = prune(deps, &catalog, &candidates, *older_than_days, *yes);
            // The attachments are the whole store's, whatever `--workspace` chose.
            let cutoff = now.checked_sub(older_than).unwrap_or(UNIX_EPOCH);
            let collected = collect_attachments(deps, &catalog, &listed, &candidates, cutoff, *yes);
            return Ok(pruned.max(collected));
        }
    }
    Ok(EXIT_OK)
}

/// After the sessions, the attachments no remaining session references, last saved
/// before `cutoff`, the sessions' own age limit (ADR-0185); the dry run counts the
/// sessions it would delete as gone. Nothing is collected while any session is live: a
/// running session can save an attachment before its journal names it.
fn collect_attachments(
    deps: &HostDeps,
    catalog: &SessionCatalog,
    listed: &[Summary],
    pruned: &[Summary],
    cutoff: SystemTime,
    yes: bool,
) -> i32 {
    let Some(store) = crate::attachments::local(deps) else {
        return EXIT_OK;
    };
    let remaining = if yes {
        // Listed again: a session whose deletion failed still references its attachments.
        match catalog.summaries() {
            Ok(remaining) => remaining,
            Err(error) => {
                write_stderr(
                    deps,
                    &format!("error: attachments not collected: {error}\n"),
                );
                return EXIT_FAILURE;
            }
        }
    } else {
        listed
            .iter()
            .filter(|summary| {
                !pruned
                    .iter()
                    .any(|gone| gone.directory == summary.directory)
            })
            .cloned()
            .collect()
    };
    if remaining.iter().any(|summary| summary.live) {
        write_stdout(
            deps,
            "attachments are not collected while a session is live (`p1 sessions` shows it)\n",
        );
        return EXIT_OK;
    }
    let collected = catalog::attachment_references(&remaining).and_then(|referenced| {
        store
            .collect(&referenced, cutoff, yes)
            .map_err(|error| error.to_string())
    });
    let objects = match collected {
        Ok(objects) => objects,
        Err(error) => {
            write_stderr(
                deps,
                &format!("error: attachments not collected: {error}\n"),
            );
            return EXIT_FAILURE;
        }
    };
    if objects.is_empty() {
        return EXIT_OK;
    }
    let verb = if yes { "deleted" } else { "would delete" };
    for object in &objects {
        write_stdout(
            deps,
            &format!("{verb} attachment {}  {} bytes\n", object.id, object.bytes),
        );
    }
    let bytes: u64 = objects.iter().map(|object| object.bytes).sum();
    let count = objects.len();
    write_stdout(
        deps,
        &if yes {
            format!("deleted {count} attachments ({bytes} bytes) no remaining session references\n")
        } else {
            format!(
                "{count} attachments ({bytes} bytes) no remaining session references; pass --yes to delete them\n"
            )
        },
    );
    EXIT_OK
}

/// The dry run lists what would go; `--yes` deletes it. A failed deletion is reported
/// and the rest still go; the exit code then says something failed.
fn prune(
    deps: &HostDeps,
    catalog: &SessionCatalog,
    candidates: &[Summary],
    days: u64,
    yes: bool,
) -> i32 {
    if candidates.is_empty() {
        write_stdout(
            deps,
            &format!("no saved session is idle for more than {days} days\n"),
        );
        return EXIT_OK;
    }
    let mut code = EXIT_OK;
    for summary in candidates {
        let line = format!("{}  {}", summary.id, summary.directory.display());
        if !yes {
            write_stdout(deps, &format!("would delete {line}\n"));
            continue;
        }
        match catalog.remove(summary) {
            Ok(()) => write_stdout(deps, &format!("deleted {line}\n")),
            Err(error) => {
                write_stderr(deps, &format!("error: {}: {error}\n", summary.id));
                code = EXIT_FAILURE;
            }
        }
    }
    if !yes {
        write_stdout(
            deps,
            &format!(
                "{} sessions idle for more than {days} days; pass --yes to delete them\n",
                candidates.len()
            ),
        );
    }
    code
}

fn json_row(summary: &Summary) -> serde_json::Value {
    serde_json::json!({
        "id": summary.id,
        "front_end": summary.front_end,
        "started": summary.started_ms.map(utc),
        "started_ms": summary.started_ms,
        "workspace": summary.workspace,
        "route": summary.route,
        "model": summary.model,
        "first_prompt": summary.first_prompt,
        "records": summary.records,
        "live": summary.live,
        "journal": summary.journal,
        "warning": summary.warning,
    })
}

/// One block per session, newest first.
fn table(summaries: &[Summary]) -> String {
    let mut out = String::new();
    if summaries.is_empty() {
        out.push_str("no saved sessions\n");
        return out;
    }
    for summary in summaries {
        let _ = writeln!(
            out,
            "{}  {}  {}  {} records{}",
            summary.id,
            summary
                .started_ms
                .map(utc)
                .unwrap_or_else(|| "-".to_string()),
            summary.front_end,
            summary.records,
            if summary.live { "  live" } else { "" }
        );
        let _ = writeln!(
            out,
            "    {}",
            summary
                .workspace
                .as_deref()
                .map(Path::display)
                .map(|path| path.to_string())
                .unwrap_or_else(|| "-".to_string())
        );
        if let (Some(route), Some(model)) = (&summary.route, &summary.model) {
            let _ = writeln!(out, "    {route} {model}");
        }
        if let Some(prompt) = &summary.first_prompt {
            let _ = writeln!(out, "    > {prompt}");
        }
        if let Some(warning) = &summary.warning {
            let _ = writeln!(out, "    warning: {warning}");
        }
    }
    out
}

/// Unix milliseconds as `YYYY-MM-DD HH:MM:SSZ`.
fn utc(ms: u64) -> String {
    let (year, month, day, hour, minute, second) = crate::civil_utc((ms / 1000) as i64);
    format!("{year:04}-{month:02}-{day:02} {hour:02}:{minute:02}:{second:02}Z")
}
