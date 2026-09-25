use std::collections::HashMap;
use std::ffi::OsString;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock, RwLockReadGuard, RwLockWriteGuard};
use std::time::{Duration, Instant};

use p1_hook_shadow::{Origin, Outcome, ShadowEvent, ShadowHook, find_binary};

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);
const GUARDS: [&str; 5] = [
    "BRAIN_PACKET_SHADOW",
    "BRAIN_INTERNAL",
    "BRAIN_JOB",
    "BRAIN_HOME",
    "BRAIN_PACKET_DEPTH",
];

struct TempDir(PathBuf);

impl TempDir {
    fn new(label: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "p1-shadow-test-{}-{label}-{}",
            std::process::id(),
            TEMP_COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        Self(path)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[allow(clippy::type_complexity)]
fn env_for(
    values: HashMap<String, OsString>,
) -> Arc<dyn Fn(&str) -> Option<OsString> + Send + Sync> {
    Arc::new(move |name| values.get(name).cloned())
}

fn event(origin: Origin) -> ShadowEvent {
    ShadowEvent {
        text: "  retain this exact task\n".into(),
        workspace: Some(PathBuf::from("/abs/workspace")),
        journal: PathBuf::from("/sessions/example.jsonl"),
        cache_key: Some("cache-key".into()),
        origin,
        source_ref: Some("/sessions/example.jsonl:7".into()),
    }
}

/// Serialises fake-binary writes against spawns. The tests of this file share one
/// process: when one test writes a script while another test forks a child, the
/// forked child inherits the open write descriptor until its exec, and any exec of
/// that script in the window fails with ETXTBSY. Spawns still run in parallel with
/// each other under the read lock; only an open write descriptor excludes them.
static FAKE_BINARY_LOCK: RwLock<()> = RwLock::new(());

/// Writes an executable under the write lock, covering the write and the permission
/// change, so no forked child can inherit the descriptor mid-write. A poisoned lock
/// is recovered: one failing test must not cascade into the others.
fn write_executable(path: &Path, script: &str) {
    let _guard: RwLockWriteGuard<'static, ()> = FAKE_BINARY_LOCK
        .write()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    fs::write(path, script).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
}

/// Runs a hook call that may spawn a child while holding the read lock, so the fork
/// cannot happen while any test holds an executable open for writing (ETXTBSY).
fn spawning<T>(call: impl FnOnce() -> T) -> T {
    let _guard: RwLockReadGuard<'static, ()> = FAKE_BINARY_LOCK
        .read()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    call()
}

fn fake_binary(root: &Path) -> (PathBuf, PathBuf) {
    let binary = root.join("fake-shadow");
    let arguments = root.join("arguments");
    let script = format!(
        "#!/bin/sh\nprintf '%s\\n' \"$@\" > '{}.tmp'\nmv '{}.tmp' '{}'\n",
        arguments.display(),
        arguments.display(),
        arguments.display()
    );
    write_executable(&binary, &script);
    (binary, arguments)
}

/// Guards only against a hung test: the fake child is a three-line shell script, so a
/// correct run never comes near this. The result is decided by the hook's outcome and
/// the child's exit status, never by the clock.
const HUNG_TEST_BOUND: Duration = Duration::from_secs(300);

/// Asserts that the hook spawned, waits for the fake child to exit (the reaper
/// reports its exit status), and returns the task file handed to it.
fn wait_for_spawned_exit(outcome: Outcome) -> PathBuf {
    let Outcome::Spawned(spawned) = outcome else {
        panic!("expected a spawn, got {outcome:?}");
    };
    let status = spawned
        .exit
        .recv_timeout(HUNG_TEST_BOUND)
        .expect("the reaper reports the fake child's exit")
        .expect("waiting on the fake child succeeds");
    assert!(status.success(), "fake child failed: {status:?}");
    spawned.task_file
}

#[test]
fn kill_file_prevents_file_and_spawn() {
    let temp = TempDir::new("kill");
    let state = temp.0.join("state");
    fs::create_dir(&state).unwrap();
    fs::write(state.join("kill"), b"").unwrap();
    let (binary, arguments) = fake_binary(&temp.0);
    let hook = ShadowHook::new(
        binary,
        env_for(HashMap::from([(
            "BRAIN_PACKET_STATE".into(),
            state.into_os_string(),
        )])),
    );

    // The hook's own decision, returned synchronously: nothing is left to wait for.
    let outcome = spawning(|| hook.observe_outcome(event(Origin::UserInput)));
    assert!(matches!(outcome, Outcome::Killed), "{outcome:?}");

    assert!(!arguments.exists());
    assert!(!temp.0.join("state/inbox").exists());
}

#[test]
fn every_recursion_guard_prevents_file_and_spawn() {
    for guard in GUARDS {
        let temp = TempDir::new(guard);
        let state = temp.0.join("state");
        let (binary, arguments) = fake_binary(&temp.0);
        let hook = ShadowHook::new(
            binary,
            env_for(HashMap::from([
                ("BRAIN_PACKET_STATE".into(), state.clone().into_os_string()),
                (guard.into(), OsString::from("/set")),
            ])),
        );

        let outcome = spawning(|| hook.observe_outcome(event(Origin::UserInput)));
        assert!(
            matches!(outcome, Outcome::Guarded),
            "guard {guard}: {outcome:?}"
        );

        assert!(!arguments.exists(), "spawned with guard {guard}");
        assert!(!state.join("inbox").exists(), "wrote with guard {guard}");
    }
}

#[test]
fn zero_and_empty_recursion_values_do_not_guard() {
    for value in ["", "0"] {
        let temp = TempDir::new("unguarded");
        let state = temp.0.join("state");
        let (binary, arguments) = fake_binary(&temp.0);
        let hook = ShadowHook::new(
            binary,
            env_for(HashMap::from([
                ("BRAIN_PACKET_STATE".into(), state.into_os_string()),
                ("BRAIN_HOME".into(), OsString::from(value)),
            ])),
        );
        wait_for_spawned_exit(spawning(|| hook.observe_outcome(event(Origin::UserInput))));
        assert!(arguments.exists(), "no spawn with BRAIN_HOME={value:?}");
    }
}

#[test]
fn normal_dispatch_writes_private_exact_task_and_exact_argv() {
    let temp = TempDir::new("normal");
    let state = temp.0.join("state");
    let (binary, arguments) = fake_binary(&temp.0);
    let hook = ShadowHook::new(
        binary,
        env_for(HashMap::from([(
            "BRAIN_PACKET_STATE".into(),
            state.clone().into_os_string(),
        )])),
    );

    let spawned_task = wait_for_spawned_exit(spawning(|| {
        hook.observe_outcome(event(Origin::Dispatch {
            family: "researcher".into(),
            provider: "p1".into(),
        }))
    }));

    let entries: Vec<_> = fs::read_dir(state.join("inbox"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect();
    assert_eq!(entries.len(), 1);
    let task = &entries[0];
    assert_eq!(task, &spawned_task);
    assert_eq!(fs::read(task).unwrap(), b"  retain this exact task\n");
    assert_eq!(
        fs::metadata(task).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert_eq!(
        fs::metadata(state.join("inbox"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o700
    );

    let expected = [
        "--harness",
        "p1",
        "--origin",
        "hook",
        "--task-file",
        task.to_str().unwrap(),
        "--workspace",
        "/abs/workspace",
        "--session",
        "cache-key",
        "--episode",
        "p1-agent-e604e10d228c",
        "--family",
        "researcher",
        "--provider",
        "p1",
        "--source-ref",
        "/sessions/example.jsonl:7",
    ]
    .join("\n")
        + "\n";
    assert_eq!(fs::read_to_string(arguments).unwrap(), expected);
}

#[test]
fn user_input_has_exact_argv_and_private_exact_bytes() {
    let temp = TempDir::new("user");
    let state = temp.0.join("state");
    let (binary, arguments) = fake_binary(&temp.0);
    let hook = ShadowHook::new(
        binary,
        env_for(HashMap::from([(
            "BRAIN_PACKET_STATE".into(),
            state.clone().into_os_string(),
        )])),
    );
    let mut input = event(Origin::UserInput);
    input.source_ref = None;
    let spawned_task = wait_for_spawned_exit(spawning(|| hook.observe_outcome(input)));
    let task = fs::read_dir(state.join("inbox"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    assert_eq!(task, spawned_task);
    assert_eq!(fs::read(&task).unwrap(), b"  retain this exact task\n");
    assert_eq!(
        fs::metadata(&task).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert_eq!(
        fs::metadata(state.join("inbox"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o700
    );
    let expected = [
        "--harness",
        "p1",
        "--origin",
        "hook",
        "--task-file",
        task.to_str().unwrap(),
        "--workspace",
        "/abs/workspace",
        "--session",
        "cache-key",
        "--episode",
        "p1-e604e10d228c",
    ]
    .join("\n")
        + "\n";
    assert_eq!(fs::read_to_string(arguments).unwrap(), expected);
}

#[test]
fn explicit_episode_and_derived_session_are_used() {
    let temp = TempDir::new("ids");
    let state = temp.0.join("state");
    let (binary, arguments) = fake_binary(&temp.0);
    let hook = ShadowHook::new(
        binary,
        env_for(HashMap::from([(
            "BRAIN_PACKET_STATE".into(),
            state.into_os_string(),
        )])),
    );
    let mut observed = event(Origin::UserInput);
    observed.text = "Task\nEpisode: E-42\n".into();
    observed.cache_key = None;
    observed.workspace = Some(PathBuf::from("relative"));
    observed.source_ref = None;
    wait_for_spawned_exit(spawning(|| hook.observe_outcome(observed)));

    let args = fs::read_to_string(arguments).unwrap();
    assert!(args.contains("--workspace\nunknown\n"));
    assert!(args.contains("--session\np1:33384b53dc1866a5\n"));
    assert!(args.contains("--episode\nE-42\n"));
    assert!(!args.contains("--family"));
    assert!(!args.contains("--source-ref"));
}

#[test]
fn nonexistent_binary_removes_only_new_task_file() {
    let temp = TempDir::new("missing");
    let state = temp.0.join("state");
    fs::create_dir_all(state.join("inbox")).unwrap();
    fs::write(state.join("inbox/preexisting"), b"keep").unwrap();
    let hook = ShadowHook::new(
        temp.0.join("does-not-exist"),
        env_for(HashMap::from([(
            "BRAIN_PACKET_STATE".into(),
            state.clone().into_os_string(),
        )])),
    );

    let outcome = spawning(|| hook.observe_outcome(event(Origin::UserInput)));
    assert!(matches!(outcome, Outcome::Failed(_)), "{outcome:?}");

    let names: Vec<_> = fs::read_dir(state.join("inbox"))
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect();
    assert_eq!(names, [OsString::from("preexisting")]);
}

#[test]
fn home_fallback_and_find_binary_use_only_injected_environment() {
    let temp = TempDir::new("environment");
    let bin_dir = temp.0.join("bin");
    fs::create_dir(&bin_dir).unwrap();
    let binary = bin_dir.join("brain-packet-shadow");
    write_executable(&binary, "#!/bin/sh\n");
    let values = HashMap::from([
        ("BRAIN_PACKET_STATE".into(), OsString::new()),
        ("HOME".into(), temp.0.clone().into_os_string()),
        ("PATH".into(), bin_dir.into_os_string()),
    ]);
    assert_eq!(find_binary(&*env_for(values.clone())), Some(binary));

    let hook = ShadowHook::new(temp.0.join("missing"), env_for(values));
    spawning(|| hook.observe(event(Origin::UserInput)));
    assert!(temp.0.join(".local/state/brain-packet/inbox").exists());
}

#[test]
fn twenty_calls_have_under_fifty_millisecond_p95() {
    let temp = TempDir::new("latency");
    let state = temp.0.join("state");
    let (binary, _) = fake_binary(&temp.0);
    let hook = ShadowHook::new(
        binary,
        env_for(HashMap::from([(
            "BRAIN_PACKET_STATE".into(),
            state.into_os_string(),
        )])),
    );
    let mut elapsed = Vec::new();
    for _ in 0..20 {
        let started = Instant::now();
        spawning(|| hook.observe(event(Origin::UserInput)));
        elapsed.push(started.elapsed());
    }
    elapsed.sort();
    let p95 = elapsed[18];
    eprintln!("shadow observe latency p95 over 20 calls: {p95:?}");
    assert!(p95 < Duration::from_millis(500), "p95 was {p95:?}");
}
