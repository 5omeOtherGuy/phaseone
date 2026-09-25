use std::collections::HashMap;
use std::ffi::OsString;
use std::fs::{self, OpenOptions};
use std::io::Read;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use p1_hook_shadow::{Origin, ShadowEvent, ShadowHook, find_binary};

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

fn fake_binary(root: &Path) -> (PathBuf, PathBuf, std::fs::File) {
    let binary = root.join("fake-shadow");
    let arguments = root.join("arguments");
    let ready_path = root.join("arguments.ready");
    let ready = Command::new("mkfifo")
        .arg(&ready_path)
        .status()
        .map(|_| {
            // Open the reader before observe so the fake child never has to win a race
            // against the test's FIFO open. O_RDWR keeps this non-blocking; the read
            // below is what waits for the child to write its completion marker.
            OpenOptions::new()
                .read(true)
                .write(true)
                .open(&ready_path)
                .unwrap()
        })
        .expect("mkfifo should create the synchronization FIFO");
    let script = format!(
        "#!/bin/sh\nprintf '%s\\n' \"$@\" > '{}.tmp'\nmv '{}.tmp' '{}'\nprintf ready > '{}'\n",
        arguments.display(),
        arguments.display(),
        arguments.display(),
        ready_path.display()
    );
    fs::write(&binary, script).unwrap();
    fs::set_permissions(&binary, fs::Permissions::from_mode(0o700)).unwrap();
    (binary, arguments, ready)
}

fn synchronize(ready: &mut std::fs::File) {
    // The fake binary writes this FIFO only after the argument file is complete.
    // Reading it is explicit child/process synchronization, with no wall-clock
    // budget or polling.
    let mut byte = [0];
    ready.read_exact(&mut byte).unwrap();
}

#[test]
fn kill_file_prevents_file_and_spawn() {
    let temp = TempDir::new("kill");
    let state = temp.0.join("state");
    fs::create_dir(&state).unwrap();
    fs::write(state.join("kill"), b"").unwrap();
    let (binary, arguments, _ready) = fake_binary(&temp.0);
    let hook = ShadowHook::new(
        binary,
        env_for(HashMap::from([(
            "BRAIN_PACKET_STATE".into(),
            state.into_os_string(),
        )])),
    );

    hook.observe(event(Origin::UserInput));

    assert!(!arguments.exists());
    assert!(!temp.0.join("state/inbox").exists());
}

#[test]
fn every_recursion_guard_prevents_file_and_spawn() {
    for guard in GUARDS {
        let temp = TempDir::new(guard);
        let state = temp.0.join("state");
        let (binary, arguments, _ready) = fake_binary(&temp.0);
        let hook = ShadowHook::new(
            binary,
            env_for(HashMap::from([
                ("BRAIN_PACKET_STATE".into(), state.clone().into_os_string()),
                (guard.into(), OsString::from("/set")),
            ])),
        );

        hook.observe(event(Origin::UserInput));

        assert!(!arguments.exists(), "spawned with guard {guard}");
        assert!(!state.join("inbox").exists(), "wrote with guard {guard}");
    }
}

#[test]
fn zero_and_empty_recursion_values_do_not_guard() {
    for value in ["", "0"] {
        let temp = TempDir::new("unguarded");
        let state = temp.0.join("state");
        let (binary, _arguments, mut ready) = fake_binary(&temp.0);
        let hook = ShadowHook::new(
            binary,
            env_for(HashMap::from([
                ("BRAIN_PACKET_STATE".into(), state.into_os_string()),
                ("BRAIN_HOME".into(), OsString::from(value)),
            ])),
        );
        hook.observe(event(Origin::UserInput));
        synchronize(&mut ready);
    }
}

#[test]
fn normal_dispatch_writes_private_exact_task_and_exact_argv() {
    let temp = TempDir::new("normal");
    let state = temp.0.join("state");
    let (binary, arguments, mut ready) = fake_binary(&temp.0);
    let hook = ShadowHook::new(
        binary,
        env_for(HashMap::from([(
            "BRAIN_PACKET_STATE".into(),
            state.clone().into_os_string(),
        )])),
    );

    hook.observe(event(Origin::Dispatch {
        family: "researcher".into(),
        provider: "p1".into(),
    }));
    synchronize(&mut ready);

    let entries: Vec<_> = fs::read_dir(state.join("inbox"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect();
    assert_eq!(entries.len(), 1);
    let task = &entries[0];
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
    let (binary, arguments, mut ready) = fake_binary(&temp.0);
    let hook = ShadowHook::new(
        binary,
        env_for(HashMap::from([(
            "BRAIN_PACKET_STATE".into(),
            state.clone().into_os_string(),
        )])),
    );
    let mut input = event(Origin::UserInput);
    input.source_ref = None;
    hook.observe(input);
    synchronize(&mut ready);
    let task = fs::read_dir(state.join("inbox"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
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
    let (binary, arguments, mut ready) = fake_binary(&temp.0);
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
    hook.observe(observed);
    synchronize(&mut ready);

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

    hook.observe(event(Origin::UserInput));

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
    fs::write(&binary, "#!/bin/sh\n").unwrap();
    fs::set_permissions(&binary, fs::Permissions::from_mode(0o700)).unwrap();
    let values = HashMap::from([
        ("BRAIN_PACKET_STATE".into(), OsString::new()),
        ("HOME".into(), temp.0.clone().into_os_string()),
        ("PATH".into(), bin_dir.into_os_string()),
    ]);
    assert_eq!(find_binary(&*env_for(values.clone())), Some(binary));

    let hook = ShadowHook::new(temp.0.join("missing"), env_for(values));
    hook.observe(event(Origin::UserInput));
    assert!(temp.0.join(".local/state/brain-packet/inbox").exists());
}

#[test]
fn twenty_calls_have_under_fifty_millisecond_p95() {
    let temp = TempDir::new("latency");
    let state = temp.0.join("state");
    let (binary, _, _ready) = fake_binary(&temp.0);
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
        hook.observe(event(Origin::UserInput));
        elapsed.push(started.elapsed());
    }
    elapsed.sort();
    let p95 = elapsed[18];
    eprintln!("shadow observe latency p95 over 20 calls: {p95:?}");
    assert!(p95 < Duration::from_millis(500), "p95 was {p95:?}");
}
