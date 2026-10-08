//! Issue #456 (ADR-0123): a foreground `shell` command that outlives its
//! `timeout_seconds` is handed over to the session's job registry instead of being
//! killed. The call returns at once with the job id and the output so far, the command
//! keeps running as a background job, and its completion wakes the run as one notice.
//! A handed-over command that never ends is killed when the run ends. The provider is a
//! scripted fake: no network, tempdirs only.

mod common;

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use common::{Harness, provider_hook, provider_hook_arc, run_args, write_environment};
use p1_contracts::{
    BoxFuture, CancellationToken, InboxKind, Item, Provider, ProviderError, ProviderRequest,
    ProviderStream, RouteDescription,
};
use p1_testkit::{ScriptedProvider, json_call, text_response, tool_call_response};
use tempfile::tempdir;

/// A scripted provider that also records the `Instant` of every `stream` call. The times
/// are taken on the run's own thread, so a starved observer thread cannot inflate them.
struct TimedProvider {
    inner: ScriptedProvider,
    at: Arc<Mutex<Vec<Instant>>>,
}

impl Provider for TimedProvider {
    fn describe(&self) -> RouteDescription {
        self.inner.describe()
    }

    fn validate(&self, request: &ProviderRequest) -> Result<(), ProviderError> {
        self.inner.validate(request)
    }

    fn stream<'a>(
        &'a self,
        request: ProviderRequest,
        cancel: CancellationToken,
    ) -> BoxFuture<'a, Result<ProviderStream, ProviderError>> {
        self.at.lock().unwrap().push(Instant::now());
        self.inner.stream(request, cancel)
    }
}

/// Poll `condition` for up to `limit`, then return whether it held.
fn within<T>(limit: Duration, mut poll: impl FnMut() -> Option<T>) -> Option<T> {
    let deadline = Instant::now() + limit;
    loop {
        if let Some(value) = poll() {
            return Some(value);
        }
        if Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// Any test helper that must observe the run while it happens runs on a real thread:
/// a task spawned into the run's own runtime could be starved by it.
const WATCH_LIMIT: Duration = Duration::from_secs(30);

/// A call that reaches `timeout_seconds` while its command still runs is handed to the
/// job registry: the call returns after about a second with the job's id and the output
/// so far, the model ends its turn, the run waits, and the job's own end arrives as one
/// notification. The command ran exactly once — it is never restarted.
#[tokio::test]
async fn a_timed_out_foreground_command_returns_at_once_and_finishes_as_a_job() {
    let workspace = tempdir().unwrap();
    let environments = tempdir().unwrap();
    write_environment(
        environments.path(),
        "plain",
        "fake",
        "fake-model",
        &["shell"],
        "test",
    );
    let counter = workspace.path().join("counter.txt");
    let command = format!(
        "echo run >> '{}'; sleep 3; echo ok-marker",
        counter.display()
    );
    // Request 1 starts the command; request 2 is the turn that ends while the job runs;
    // request 3 is the turn the completion notice opens. Nothing else may reach the model.
    let inner = ScriptedProvider::new(vec![
        tool_call_response(vec![json_call(
            "c1",
            "shell",
            &format!(r#"{{"command":"{command}","timeout_seconds":1}}"#),
        )]),
        text_response("waiting for the job"),
        text_response("the job is done"),
    ]);
    let handle = inner.clone();
    let at: Arc<Mutex<Vec<Instant>>> = Arc::new(Mutex::new(Vec::new()));
    let timed: Arc<dyn Provider> = Arc::new(TimedProvider {
        inner,
        at: at.clone(),
    });
    let mut harness = Harness::new(vec![environments.path().to_path_buf()], &[]);
    harness.deps.catalog_hook = Some(provider_hook_arc(vec![("fake", timed)]));

    let started = Instant::now();
    let code = tokio::time::timeout(
        Duration::from_secs(60),
        run_args(
            &mut harness,
            &[
                "--yes",
                "--env",
                "plain",
                "--workspace",
                workspace.path().to_str().unwrap(),
                "go",
            ],
        ),
    )
    .await
    .expect("the run must end on its own");
    let runaway = started.elapsed();
    assert_eq!(code, 0, "stderr: {}", harness.stderr.text());

    let requests = handle.requests();
    assert_eq!(
        requests.len(),
        3,
        "one request per turn, none while waiting"
    );

    // The call returned at the deadline, not at the command's end. The provider stamped
    // request 2 on the run's own thread, so this is the tool call's duration, not an
    // observer thread's wake-up time.
    let times = at.lock().unwrap().clone();
    assert_eq!(times.len(), 3, "one stamp per request");
    let tool_call = times[1].duration_since(times[0]);
    assert!(
        tool_call < Duration::from_millis(2500),
        "the tool call took {tool_call:?}"
    );
    assert!(
        tool_call >= Duration::from_millis(900),
        "the tool call returned before its deadline: {tool_call:?}"
    );
    // The run then waited for the job, so it lasted at least the command's sleep.
    assert!(
        runaway >= Duration::from_secs(3),
        "the run lasted {runaway:?}"
    );

    // The start call answered with the job id, not with the command's output.
    let start_result = requests[1]
        .history
        .iter()
        .find_map(|item| match item {
            Item::ToolResult(result) => Some(result.content.clone()),
            _ => None,
        })
        .expect("the start call's result");
    assert!(
        start_result.starts_with("still running as background job j1 after 1 s"),
        "{start_result}"
    );
    assert!(!start_result.contains("ok-marker"), "{start_result}");

    // The third request carries the completion notice: exit 0 and the job's output.
    let notice = requests[2]
        .history
        .iter()
        .find_map(|item| match item {
            Item::Inbox { kind, text } if *kind == InboxKind::Notification => Some(text.clone()),
            _ => None,
        })
        .expect("the job's completion notice");
    assert!(notice.contains("Background job j1 ended"), "{notice}");
    assert!(notice.contains("Code(0)"), "{notice}");
    assert!(notice.contains("ok-marker"), "{notice}");

    // The command ran exactly once: the file it appends to holds one line.
    let counted = std::fs::read_to_string(&counter).expect("the counter file");
    assert_eq!(counted.lines().count(), 1, "{counted:?}");
}

/// The state field of `/proc/<pid>/stat`, or `None` once the process is gone.
fn process_state(pid: i32) -> Option<u8> {
    let stat = std::fs::read(format!("/proc/{pid}/stat")).ok()?;
    let close = stat.iter().rposition(|byte| *byte == b')')?;
    stat.get(close + 2).copied()
}

/// A handed-over command that never ends is killed when the run ends: the run is
/// interrupted while it waits for the job, and the job's process group — whose leader
/// wrote its own pid — is gone.
#[tokio::test]
async fn a_handed_over_job_that_never_ends_is_killed_when_the_run_ends() {
    let workspace = tempdir().unwrap();
    let environments = tempdir().unwrap();
    write_environment(
        environments.path(),
        "plain",
        "fake",
        "fake-model",
        &["shell"],
        "test",
    );
    let pid_file = workspace.path().join("child.pid");
    // `exec` keeps the pid the group leader published, so the file holds the pgid.
    let command = "echo $$ > child.pid; exec sleep 86402";
    let provider = ScriptedProvider::new(vec![
        tool_call_response(vec![json_call(
            "c1",
            "shell",
            &format!(r#"{{"command":"{command}","timeout_seconds":1}}"#),
        )]),
        text_response("waiting for the job"),
    ]);
    let handle = provider.clone();
    let mut harness = Harness::new(vec![environments.path().to_path_buf()], &[]);
    harness.deps.catalog_hook = Some(provider_hook(vec![("fake", provider)]));

    let interrupt = harness.interrupt.clone();
    let watcher = std::thread::spawn({
        let watched = handle.clone();
        move || {
            // The second request is only made once the command was handed over, so by
            // then the job really runs; then the run is told to stop.
            let deadline = Instant::now() + WATCH_LIMIT;
            while watched.requests().len() < 2 {
                if Instant::now() >= deadline {
                    return;
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            interrupt.fire();
        }
    });

    let code = tokio::time::timeout(
        Duration::from_secs(60),
        run_args(
            &mut harness,
            &[
                "--yes",
                "--env",
                "plain",
                "--workspace",
                workspace.path().to_str().unwrap(),
                "go",
            ],
        ),
    )
    .await
    .expect("the run must end on its own");
    watcher.join().unwrap();
    assert_eq!(
        code,
        p1_host::run::EXIT_CANCELLED,
        "stderr: {}",
        harness.stderr.text()
    );

    let requests = handle.requests();
    let start_result = requests[1]
        .history
        .iter()
        .find_map(|item| match item {
            Item::ToolResult(result) => Some(result.content.clone()),
            _ => None,
        })
        .expect("the start call's result");
    assert!(
        start_result.starts_with("still running as background job j1 after 1 s"),
        "{start_result}"
    );

    let pid: i32 = std::fs::read_to_string(&pid_file)
        .expect("the group leader's pid file")
        .trim()
        .parse()
        .unwrap();
    // A zombie is already dead; the pid token disappears once it is reaped.
    let gone = within(Duration::from_secs(10), || match process_state(pid) {
        None | Some(b'Z') => Some(()),
        Some(_) => None,
    });
    assert!(
        gone.is_some(),
        "the handed-over job {pid} survived the session (state {:?})",
        process_state(pid)
    );
}
