//! Session history by default (#62, ADR-0171): an interactive run journals to the
//! session catalog under the state directory, `p1 sessions` lists the catalog, and a
//! saved session continues by id. Every test points `P1_STATE_DIR` at a temporary
//! directory through the injected environment snapshot; none reads a real home.

mod common;

use std::path::{Path, PathBuf};

use common::{Harness, provider_hook, run_args, write_environment};
use p1_contracts::{
    CacheKeySupport, Item, JournalRecord, ModelOptions, Origin, RecordBody, RouteDescription,
};
use p1_testkit::{ScriptedProvider, text_response};
use tempfile::{TempDir, tempdir};

struct Scene {
    workspace: TempDir,
    environments: TempDir,
    state: TempDir,
}

impl Scene {
    fn new() -> Self {
        let environments = tempdir().unwrap();
        write_environment(
            environments.path(),
            "plain",
            "fake",
            "fake-model",
            &["read"],
            "test",
        );
        Self {
            workspace: tempdir().unwrap(),
            environments,
            state: tempdir().unwrap(),
        }
    }

    /// A host whose stdin is a terminal (or not), whose only environment variable is
    /// `P1_STATE_DIR`, answering with `replies`.
    fn harness(&self, lines: &[&str], tty: bool, replies: &[&str]) -> (Harness, ScriptedProvider) {
        let provider =
            ScriptedProvider::new(replies.iter().map(|reply| text_response(reply)).collect());
        let mut harness = Harness::new(vec![self.environments.path().to_path_buf()], lines);
        harness.deps.catalog_hook = Some(provider_hook(vec![("fake", provider.clone())]));
        harness.deps.stdin_is_tty = tty;
        harness.deps.shell_env = Some(vec![(
            "P1_STATE_DIR".into(),
            self.state.path().as_os_str().to_os_string(),
        )]);
        (harness, provider)
    }

    fn workspace_arg(&self) -> &str {
        self.workspace.path().to_str().unwrap()
    }

    fn cli_sessions(&self) -> Vec<PathBuf> {
        sessions_in(&self.state.path().join("sessions/cli"))
    }
}

fn sessions_in(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut paths: Vec<PathBuf> = entries.map(|entry| entry.unwrap().path()).collect();
    paths.sort();
    paths
}

fn files_under(dir: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            found.extend(files_under(&path));
        } else {
            found.push(path);
        }
    }
    found
}

fn environment_session_ids(journal: &Path) -> Vec<Option<String>> {
    p1_journal::load(journal)
        .unwrap()
        .records
        .iter()
        .filter_map(|record| match &record.body {
            RecordBody::Environment { options, .. } => Some(
                options
                    .native
                    .get("session_id")
                    .and_then(|id| id.as_str())
                    .map(str::to_string),
            ),
            _ => None,
        })
        .collect()
}

/// One interactive session in `scene`'s workspace that sends `prompt`; its directory.
async fn interactive_session(scene: &Scene, prompt: &str, reply: &str) -> PathBuf {
    let before = scene.cli_sessions();
    let (mut harness, _) = scene.harness(&[prompt], true, &[reply]);
    let code = run_args(
        &mut harness,
        &["--env", "plain", "--workspace", scene.workspace_arg()],
    )
    .await;
    assert_eq!(code, 0, "stderr: {}", harness.stderr.text());
    scene
        .cli_sessions()
        .into_iter()
        .find(|path| !before.contains(path))
        .expect("the run created a session")
}

#[tokio::test]
async fn an_interactive_start_without_session_journals_one_private_file_that_names_its_id() {
    let scene = Scene::new();
    let (mut harness, _) = scene.harness(&["hello"], true, &["hi"]);
    let code = run_args(
        &mut harness,
        &["--env", "plain", "--workspace", scene.workspace_arg()],
    )
    .await;
    assert_eq!(code, 0, "stderr: {}", harness.stderr.text());

    let sessions = scene.cli_sessions();
    assert_eq!(sessions.len(), 1, "{sessions:?}");
    let id = sessions[0]
        .file_name()
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();
    // `<yyyymmdd-hhmmss>-<6 hex>`
    let parts: Vec<&str> = id.split('-').collect();
    assert_eq!(
        parts.iter().map(|part| part.len()).collect::<Vec<_>>(),
        [8, 6, 6],
        "{id}"
    );
    assert!(parts[2].bytes().all(|b| b.is_ascii_hexdigit()), "{id}");

    // The journal and its workspace record are the session's only files, both private.
    let journal = sessions[0].join("journal.jsonl");
    let mut files = files_under(scene.state.path());
    files.sort();
    assert_eq!(files, [journal.clone(), sessions[0].join("workspace.json")]);
    for file in &files {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(file).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "{} mode {mode:o}", file.display());
    }
    assert_eq!(environment_session_ids(&journal), [Some(id.clone())]);
    assert!(
        harness
            .stderr
            .text()
            .contains(&format!("p1 --resume-id {id}")),
        "stderr: {}",
        harness.stderr.text()
    );
}

#[tokio::test]
async fn a_headless_run_without_session_writes_nothing_under_the_state_dir() {
    let scene = Scene::new();
    let (mut harness, _) = scene.harness(&[], true, &["done"]);
    let code = run_args(
        &mut harness,
        &["--env", "plain", "--workspace", scene.workspace_arg(), "go"],
    )
    .await;
    assert_eq!(code, 0, "stderr: {}", harness.stderr.text());
    assert!(files_under(scene.state.path()).is_empty());

    // Nor does an interactive run whose stdin is not a terminal (a pipe, a script).
    let (mut harness, _) = scene.harness(&["go"], false, &["done"]);
    let code = run_args(
        &mut harness,
        &["--env", "plain", "--workspace", scene.workspace_arg()],
    )
    .await;
    assert_eq!(code, 0, "stderr: {}", harness.stderr.text());
    assert!(files_under(scene.state.path()).is_empty());
}

#[tokio::test]
async fn resume_id_on_a_free_session_replays_the_projection_and_keeps_its_id() {
    let scene = Scene::new();
    let directory = interactive_session(&scene, "first", "one").await;
    let id = directory.file_name().unwrap().to_str().unwrap().to_string();

    // A session continues in its own workspace, never in another one.
    let elsewhere = tempdir().unwrap();
    let (mut harness, provider) = scene.harness(&["second"], true, &["two"]);
    let code = run_args(
        &mut harness,
        &[
            "--env",
            "plain",
            "--workspace",
            elsewhere.path().to_str().unwrap(),
            "--resume-id",
            &id,
        ],
    )
    .await;
    assert_eq!(code, 2);
    assert!(
        harness.stderr.text().contains("belongs to the workspace"),
        "{}",
        harness.stderr.text()
    );
    assert!(provider.requests().is_empty());

    let (mut harness, second) = scene.harness(&["second"], true, &["two"]);
    let code = run_args(
        &mut harness,
        &[
            "--env",
            "plain",
            "--workspace",
            scene.workspace_arg(),
            "--resume-id",
            &id,
        ],
    )
    .await;
    assert_eq!(code, 0, "stderr: {}", harness.stderr.text());
    assert_eq!(scene.cli_sessions(), std::slice::from_ref(&directory), "no new session");

    let history = &second.requests()[0].history;
    for text in ["first", "second"] {
        assert!(
            history
                .iter()
                .any(|item| matches!(item, Item::User { text: user } if user == text)),
            "{text} in {history:?}"
        );
    }
    assert!(
        history
            .iter()
            .any(|item| matches!(item, Item::Assistant(a) if a.text() == "one"))
    );
    let journal = directory.join("journal.jsonl");
    let loaded = p1_journal::load(&journal).unwrap();
    for (index, record) in loaded.records.iter().enumerate() {
        assert_eq!(record.seq, index as u64, "seq must be dense");
    }
    // The same id on the same environment: nothing changed, nothing re-committed.
    assert_eq!(environment_session_ids(&journal), [Some(id)]);
}

#[tokio::test]
async fn resume_id_on_a_locked_session_fails_naming_the_file() {
    let scene = Scene::new();
    let directory = interactive_session(&scene, "first", "one").await;
    let id = directory.file_name().unwrap().to_str().unwrap().to_string();
    let journal = directory.join("journal.jsonl");
    let before = std::fs::read(&journal).unwrap();
    // Another p1 owns the session: it holds the writer lock for its whole life.
    let (_holder, _) =
        p1_journal::JsonlJournal::resume(&journal, p1_journal::SyncPolicy::EveryRecord).unwrap();

    let (mut harness, provider) = scene.harness(&["second"], true, &["two"]);
    let code = run_args(
        &mut harness,
        &[
            "--env",
            "plain",
            "--workspace",
            scene.workspace_arg(),
            "--resume-id",
            &id,
        ],
    )
    .await;
    assert_eq!(code, 1);
    let stderr = harness.stderr.text();
    assert!(stderr.contains(journal.to_str().unwrap()), "{stderr}");
    assert!(stderr.contains("in use"), "{stderr}");
    assert!(!stderr.contains("saved"), "{stderr}");
    assert!(provider.requests().is_empty());
    assert_eq!(std::fs::read(&journal).unwrap(), before);
}

#[tokio::test]
async fn an_unknown_resume_id_is_a_usage_error() {
    let scene = Scene::new();
    let (mut harness, _) = scene.harness(&[], true, &[]);
    let code = run_args(
        &mut harness,
        &[
            "--env",
            "plain",
            "--workspace",
            scene.workspace_arg(),
            "--resume-id",
            "20260101-000000-abcdef",
        ],
    )
    .await;
    assert_eq!(code, 2);
    assert!(
        harness
            .stderr
            .text()
            .contains("no session `20260101-000000-abcdef`"),
        "{}",
        harness.stderr.text()
    );
}

#[tokio::test]
async fn resume_without_session_continues_the_newest_session_of_the_workspace() {
    let scene = Scene::new();
    let older = interactive_session(&scene, "alpha", "a").await;
    let newer = interactive_session(&scene, "beta", "b").await;
    // Make the order a fact of the files rather than of the clock: the first session
    // started at the epoch.
    restamp(&older.join("journal.jsonl"), 1);
    set_modified(
        &older.join("journal.jsonl"),
        std::time::UNIX_EPOCH + std::time::Duration::from_secs(1),
    );
    // A newer session of ANOTHER workspace is not this workspace's.
    let other_workspace = tempdir().unwrap();
    write_fabricated(
        &scene.state.path().join("sessions/cli"),
        "29991231-235959-ffffff",
        other_workspace.path(),
        &[(user_input(0, "elsewhere"), Some(9_999_999_999_999))],
    );

    let (mut harness, provider) = scene.harness(&["gamma"], true, &["c"]);
    let code = run_args(
        &mut harness,
        &[
            "--env",
            "plain",
            "--workspace",
            scene.workspace_arg(),
            "--resume",
        ],
    )
    .await;
    assert_eq!(code, 0, "stderr: {}", harness.stderr.text());
    let history = &provider.requests()[0].history;
    let users: Vec<&str> = history
        .iter()
        .filter_map(|item| match item {
            Item::User { text } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(users, ["beta", "gamma"]);
    assert!(
        harness
            .stderr
            .text()
            .contains(newer.file_name().unwrap().to_str().unwrap())
    );
}

#[tokio::test]
async fn a_run_quit_before_its_first_prompt_leaves_no_session() {
    let scene = Scene::new();
    let (mut harness, _) = scene.harness(&[], true, &[]);
    let code = run_args(
        &mut harness,
        &["--env", "plain", "--workspace", scene.workspace_arg()],
    )
    .await;
    assert_eq!(code, 0, "stderr: {}", harness.stderr.text());
    assert!(scene.cli_sessions().is_empty());
    assert!(!harness.stderr.text().contains("--resume-id"));
}

#[tokio::test]
async fn a_piped_bare_resume_still_needs_its_file() {
    let scene = Scene::new();
    interactive_session(&scene, "first", "one").await;
    let (mut harness, provider) = scene.harness(&["second"], false, &["two"]);
    let code = run_args(
        &mut harness,
        &[
            "--env",
            "plain",
            "--workspace",
            scene.workspace_arg(),
            "--resume",
        ],
    )
    .await;
    assert_eq!(code, 2);
    assert!(
        harness
            .stderr
            .text()
            .contains("--resume requires --session")
    );
    assert!(provider.requests().is_empty());
}

#[tokio::test]
async fn a_headless_resume_still_needs_its_file() {
    let scene = Scene::new();
    let (mut harness, _) = scene.harness(&[], true, &[]);
    let options = p1_host::cli::Options {
        command: p1_host::cli::Command::Run {
            prompt: Some("go".to_string()),
        },
        resume: true,
        ..p1_host::cli::parse(&["--env".into(), "plain".into()]).unwrap()
    };
    assert_eq!(p1_host::run::run(&mut harness.deps, options).await, 2);
    assert!(
        harness
            .stderr
            .text()
            .contains("--resume requires --session")
    );
}

// ------------------------------------------------------------- p1 sessions

fn user_input(seq: u64, text: &str) -> JournalRecord {
    JournalRecord {
        seq,
        body: RecordBody::UserInput { text: text.into() },
    }
}

fn environment(seq: u64, route: &str, model: &str) -> JournalRecord {
    JournalRecord {
        seq,
        body: RecordBody::Environment {
            route: RouteDescription {
                origin: Origin {
                    route: route.into(),
                    model: model.into(),
                },
                supports_freeform_tools: false,
                mandatory_prompt_prefix: None,
                reports_cost: false,
                cache_key: CacheKeySupport::Unsupported,
            },
            system_prompt: "test".into(),
            tools: Vec::new(),
            options: ModelOptions::default(),
        },
    }
}

/// A session directory `dir/id` exactly as the catalog writes one, with `records`
/// (each with its `at_ms`) in a version-3 journal.
fn write_fabricated(
    dir: &Path,
    id: &str,
    workspace: &Path,
    records: &[(JournalRecord, Option<u64>)],
) -> PathBuf {
    let directory = dir.join(id);
    std::fs::create_dir_all(&directory).unwrap();
    std::fs::write(
        directory.join("workspace.json"),
        serde_json::to_vec(&workspace.canonicalize().unwrap()).unwrap(),
    )
    .unwrap();
    let mut text = String::from("{\"p1_journal\":3}\n");
    for (record, at_ms) in records {
        let mut value = serde_json::to_value(record).unwrap();
        if let Some(at_ms) = at_ms {
            value["at_ms"] = serde_json::json!(at_ms);
        }
        text.push_str(&format!("{value}\n"));
    }
    let journal = directory.join("journal.jsonl");
    std::fs::write(&journal, text).unwrap();
    journal
}

fn set_modified(path: &Path, time: std::time::SystemTime) {
    std::fs::File::options()
        .write(true)
        .open(path)
        .unwrap()
        .set_modified(time)
        .unwrap();
}

/// Rewrite every record's `at_ms` in `journal` to `at_ms`.
fn restamp(journal: &Path, at_ms: u64) {
    let text = std::fs::read_to_string(journal).unwrap();
    let mut out = String::new();
    for line in text.lines() {
        let mut value: serde_json::Value = serde_json::from_str(line).unwrap();
        if value.get("at_ms").is_some() {
            value["at_ms"] = serde_json::json!(at_ms);
        }
        out.push_str(&format!("{value}\n"));
    }
    std::fs::write(journal, out).unwrap();
}

#[tokio::test]
async fn sessions_json_orders_by_start_and_lists_a_truncated_journal_with_a_warning() {
    let scene = Scene::new();
    let cli = scene.state.path().join("sessions/cli");
    let workspace = scene.workspace.path();
    let long_prompt = format!("explain\n{}", "x".repeat(200));
    write_fabricated(
        &cli,
        "20260101-000000-aaaaaa",
        workspace,
        &[
            (environment(0, "fake-route/a", "model-a"), Some(1_000)),
            (user_input(1, &long_prompt), Some(1_001)),
        ],
    );
    let truncated = write_fabricated(
        &cli,
        "20260103-000000-cccccc",
        workspace,
        &[
            (environment(0, "fake-route/c", "model-c"), Some(3_000)),
            (user_input(1, "third"), Some(3_001)),
            (user_input(2, "fourth"), Some(3_002)),
        ],
    );
    // A crash mid-record leaves a torn last line.
    {
        use std::io::Write;
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&truncated)
            .unwrap();
        file.write_all(b"{\"seq\":3,\"record\":\"user_in").unwrap();
    }
    // `p1 acp` sessions are part of the same catalog.
    let acp =
        p1_host::session::archive::JournalArchive::new(scene.state.path().join("sessions/acp"));
    let stored = p1_contracts::frontend::SessionArchive::create(&acp, workspace).unwrap();
    std::fs::remove_dir_all(stored.journal.parent().unwrap()).unwrap();
    write_fabricated(
        &scene.state.path().join("sessions/acp"),
        &stored.id,
        workspace,
        &[(user_input(0, "from the editor"), Some(2_000))],
    );
    // Unpublished (no journal yet): not a session.
    std::fs::create_dir_all(cli.join("20260109-000000-dddddd")).unwrap();

    let (mut harness, _) = scene.harness(&[], true, &[]);
    let code = run_args(&mut harness, &["sessions", "--json"]).await;
    assert_eq!(code, 0, "stderr: {}", harness.stderr.text());
    let rows: serde_json::Value = serde_json::from_str(&harness.stdout.text()).unwrap();
    let rows = rows.as_array().unwrap();
    let ids: Vec<&str> = rows.iter().map(|row| row["id"].as_str().unwrap()).collect();
    assert_eq!(
        ids,
        [
            "20260103-000000-cccccc",
            stored.id.as_str(),
            "20260101-000000-aaaaaa"
        ]
    );

    let newest = &rows[0];
    assert_eq!(newest["front_end"], "cli");
    assert_eq!(newest["records"], 3);
    assert_eq!(newest["first_prompt"], "third");
    assert_eq!(newest["route"], "fake-route/c");
    assert_eq!(newest["model"], "model-c");
    assert_eq!(newest["started_ms"], 3_000);
    assert_eq!(newest["started"], "1970-01-01 00:00:03Z");
    assert_eq!(newest["live"], false);
    assert!(
        newest["warning"]
            .as_str()
            .unwrap()
            .contains("incomplete last record"),
        "{newest}"
    );
    assert_eq!(
        newest["workspace"],
        workspace.canonicalize().unwrap().to_str().unwrap()
    );

    assert_eq!(rows[1]["front_end"], "acp");
    assert_eq!(rows[1]["first_prompt"], "from the editor");
    assert_eq!(rows[1]["warning"], serde_json::Value::Null);

    let oldest = &rows[2];
    assert_eq!(oldest["records"], 2);
    let prompt = oldest["first_prompt"].as_str().unwrap();
    assert_eq!(prompt.chars().count(), 80);
    assert!(prompt.starts_with("explain xxx"), "{prompt}");

    // The text form lists the same sessions, the warning included.
    let (mut harness, _) = scene.harness(&[], true, &[]);
    assert_eq!(run_args(&mut harness, &["sessions"]).await, 0);
    let text = harness.stdout.text();
    for id in ids {
        assert!(text.contains(id), "{text}");
    }
    assert!(text.contains("warning: incomplete last record"), "{text}");
}

#[tokio::test]
async fn sessions_reports_a_live_session_and_filters_by_workspace() {
    let scene = Scene::new();
    let cli = scene.state.path().join("sessions/cli");
    let other = tempdir().unwrap();
    let mine = write_fabricated(
        &cli,
        "20260101-000000-aaaaaa",
        scene.workspace.path(),
        &[(user_input(0, "mine"), Some(1_000))],
    );
    write_fabricated(
        &cli,
        "20260102-000000-bbbbbb",
        other.path(),
        &[(user_input(0, "theirs"), Some(2_000))],
    );
    let (_holder, _) =
        p1_journal::JsonlJournal::resume(&mine, p1_journal::SyncPolicy::EveryRecord).unwrap();

    let (mut harness, _) = scene.harness(&[], true, &[]);
    let code = run_args(
        &mut harness,
        &["sessions", "--workspace", scene.workspace_arg(), "--json"],
    )
    .await;
    assert_eq!(code, 0, "stderr: {}", harness.stderr.text());
    let rows: serde_json::Value = serde_json::from_str(&harness.stdout.text()).unwrap();
    let rows = rows.as_array().unwrap();
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0]["id"], "20260101-000000-aaaaaa");
    assert_eq!(rows[0]["live"], true);
}

#[tokio::test]
async fn prune_prints_what_it_would_delete_and_deletes_only_with_yes() {
    let scene = Scene::new();
    let cli = scene.state.path().join("sessions/cli");
    let workspace = scene.workspace.path();
    let long_ago = std::time::SystemTime::now() - std::time::Duration::from_secs(90 * 86_400);
    let mut journals = Vec::new();
    for id in [
        "20260101-000000-aaaaaa",
        "20260102-000000-bbbbbb",
        "20260103-000000-cccccc",
    ] {
        let journal = write_fabricated(&cli, id, workspace, &[(user_input(0, id), Some(1_000))]);
        journals.push(journal);
    }
    // `aaaaaa` and `bbbbbb` have been idle for 90 days; `cccccc` was written just now.
    for journal in &journals[..2] {
        std::fs::File::options()
            .write(true)
            .open(journal)
            .unwrap()
            .set_modified(long_ago)
            .unwrap();
    }
    // A worker journal beside it goes with its session.
    std::fs::write(
        cli.join("20260101-000000-aaaaaa/journal.jsonl.w1.jsonl"),
        "",
    )
    .unwrap();
    // A live session is never pruned, however old.
    let (_holder, _) =
        p1_journal::JsonlJournal::resume(&journals[1], p1_journal::SyncPolicy::EveryRecord)
            .unwrap();
    std::fs::File::options()
        .write(true)
        .open(&journals[1])
        .unwrap()
        .set_modified(long_ago)
        .unwrap();

    let (mut harness, _) = scene.harness(&[], true, &[]);
    let code = run_args(&mut harness, &["sessions", "prune", "--older-than", "30"]).await;
    assert_eq!(code, 0, "stderr: {}", harness.stderr.text());
    let text = harness.stdout.text();
    assert!(
        text.contains("would delete 20260101-000000-aaaaaa"),
        "{text}"
    );
    assert!(!text.contains("bbbbbb"), "{text}");
    assert!(!text.contains("cccccc"), "{text}");
    assert!(text.contains("pass --yes"), "{text}");
    assert_eq!(sessions_in(&cli).len(), 3, "a dry run deletes nothing");

    let (mut harness, _) = scene.harness(&[], true, &[]);
    let code = run_args(
        &mut harness,
        &["sessions", "prune", "--older-than", "30", "--yes"],
    )
    .await;
    assert_eq!(code, 0, "stderr: {}", harness.stderr.text());
    assert!(
        harness
            .stdout
            .text()
            .contains("deleted 20260101-000000-aaaaaa")
    );
    assert_eq!(
        sessions_in(&cli),
        [
            cli.join("20260102-000000-bbbbbb"),
            cli.join("20260103-000000-cccccc")
        ]
    );
}
