//! Fire-and-forget observation hook for the brain packet shadow process.
//!
//! The hook deliberately has no response path: it writes a private task file,
//! starts the configured process, and hands the child to a reaper thread.

use std::ffi::{OsStr, OsString};
use std::fs::{self, DirBuilder, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, mpsc};
use std::thread;

const RECURSION_GUARDS: [&str; 5] = [
    "BRAIN_PACKET_SHADOW",
    "BRAIN_INTERNAL",
    "BRAIN_JOB",
    "BRAIN_HOME",
    "BRAIN_PACKET_DEPTH",
];

/// The source of text observed by the hook.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Origin {
    /// Text committed as user input.
    UserInput,
    /// A brief dispatched to a worker.
    Dispatch { family: String, provider: String },
}

/// One prompt or worker brief to pass to the shadow process.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ShadowEvent {
    pub text: String,
    pub workspace: Option<PathBuf>,
    pub journal: PathBuf,
    pub cache_key: Option<String>,
    pub origin: Origin,
    pub source_ref: Option<String>,
}

/// What one [`ShadowHook::observe_outcome`] call decided.
#[derive(Debug)]
pub enum Outcome {
    /// A recursion guard variable was set: no task file, no process.
    Guarded,
    /// No state directory could be derived: no task file, no process.
    NoState,
    /// `STATE/kill` exists: no task file, no process.
    Killed,
    /// The task file was written and the process started, detached.
    Spawned(Spawned),
    /// A filesystem or spawn error; any task file this call created was removed.
    Failed(io::Error),
}

/// A started shadow process.
#[derive(Debug)]
pub struct Spawned {
    /// The task file handed to the process.
    pub task_file: PathBuf,
    /// Receives the child's wait result from the reaper thread once it exits;
    /// disconnects without a value if no reaper could be started.
    pub exit: mpsc::Receiver<io::Result<ExitStatus>>,
}

/// A configured, fail-open shadow hook.
#[allow(clippy::type_complexity)]
pub struct ShadowHook {
    binary: PathBuf,
    env: Arc<dyn Fn(&str) -> Option<OsString> + Send + Sync>,
    counter: AtomicU64,
}

impl ShadowHook {
    /// Constructs a hook without inspecting the process environment.
    #[allow(clippy::type_complexity)]
    pub fn new(binary: PathBuf, env: Arc<dyn Fn(&str) -> Option<OsString> + Send + Sync>) -> Self {
        Self {
            binary,
            env,
            counter: AtomicU64::new(0),
        }
    }

    /// Observes an event. All filesystem and process errors are intentionally ignored.
    pub fn observe(&self, event: ShadowEvent) {
        let _ = self.observe_outcome(event);
    }

    /// Observes an event like [`ShadowHook::observe`] and reports what the hook decided.
    ///
    /// The call itself never waits for the child. Dropping the outcome keeps the
    /// fire-and-forget behaviour; holding a [`Spawned`] lets a caller (the tests)
    /// synchronise on the child's exit instead of on wall-clock time.
    pub fn observe_outcome(&self, event: ShadowEvent) -> Outcome {
        match self.observe_inner(event) {
            Ok(outcome) => outcome,
            Err(error) => Outcome::Failed(error),
        }
    }

    fn observe_inner(&self, event: ShadowEvent) -> io::Result<Outcome> {
        if RECURSION_GUARDS
            .iter()
            .any(|name| guarded((self.env)(name)))
        {
            return Ok(Outcome::Guarded);
        }

        let Some(state) = state_dir(&*self.env) else {
            return Ok(Outcome::NoState);
        };
        if state.join("kill").exists() {
            return Ok(Outcome::Killed);
        }

        let inbox = state.join("inbox");
        let mut builder = DirBuilder::new();
        builder.recursive(true).mode(0o700);
        builder.create(&inbox)?;

        let (task_path, mut task_file) = self.create_task_file(&inbox)?;
        if let Err(error) = task_file.write_all(event.text.as_bytes()) {
            drop(task_file);
            let _ = fs::remove_file(&task_path);
            return Err(error);
        }
        drop(task_file);

        let session = event
            .cache_key
            .unwrap_or_else(|| format!("p1:{}", &hex_sha256(path_bytes(&event.journal))[..16]));
        let episode = episode(&event.text, &event.origin);
        let workspace = event
            .workspace
            .filter(|path| path.is_absolute())
            .unwrap_or_else(|| PathBuf::from("unknown"));

        let mut command = Command::new(&self.binary);
        command
            .arg("--harness")
            .arg("p1")
            .arg("--origin")
            .arg("hook")
            .arg("--task-file")
            .arg(&task_path)
            .arg("--workspace")
            .arg(workspace)
            .arg("--session")
            .arg(session)
            .arg("--episode")
            .arg(episode);
        if let Origin::Dispatch { family, provider } = event.origin {
            command
                .arg("--family")
                .arg(family)
                .arg("--provider")
                .arg(provider);
        }
        if let Some(source_ref) = event.source_ref {
            command.arg("--source-ref").arg(source_ref);
        }
        command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .process_group(0);

        match command.spawn() {
            Ok(mut child) => {
                // Builder::spawn is fallible, unlike thread::spawn. If the reaper
                // cannot be created, dropping Child still does not delay this path;
                // the dropped closure drops the sender, so `exit` disconnects.
                let (sender, exit) = mpsc::channel();
                let _ = thread::Builder::new()
                    .name("p1-shadow-reaper".into())
                    .spawn(move || {
                        // Nobody listens when the caller dropped the outcome.
                        let _ = sender.send(child.wait());
                    });
                Ok(Outcome::Spawned(Spawned {
                    task_file: task_path,
                    exit,
                }))
            }
            Err(error) => {
                let _ = fs::remove_file(task_path);
                Err(error)
            }
        }
    }

    fn create_task_file(&self, inbox: &Path) -> io::Result<(PathBuf, fs::File)> {
        // A collision can occur between independently constructed hooks. O_EXCL
        // makes every attempted name safe; advance until an unused one is found.
        loop {
            let number = self.counter.fetch_add(1, Ordering::Relaxed);
            let path = inbox.join(format!("p1-{}-{number}.txt", std::process::id()));
            match OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&path)
            {
                Ok(file) => return Ok((path, file)),
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error),
            }
        }
    }
}

/// Finds an executable named `brain-packet-shadow` on the injected `PATH`.
pub fn find_binary(env: &dyn Fn(&str) -> Option<OsString>) -> Option<PathBuf> {
    let path = env("PATH")?;
    std::env::split_paths(&path)
        .map(|directory| directory.join("brain-packet-shadow"))
        .find(|candidate| {
            candidate.metadata().is_ok_and(|metadata| {
                metadata.is_file() && metadata.permissions().mode() & 0o111 != 0
            })
        })
}

fn guarded(value: Option<OsString>) -> bool {
    value.is_some_and(|value| !value.is_empty() && value != OsStr::new("0"))
}

fn state_dir(env: &dyn Fn(&str) -> Option<OsString>) -> Option<PathBuf> {
    env("BRAIN_PACKET_STATE")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            env("HOME")
                .filter(|value| !value.is_empty())
                .map(PathBuf::from)
                .map(|home| home.join(".local/state/brain-packet"))
        })
}

fn episode(text: &str, origin: &Origin) -> String {
    if let Some(value) = text.lines().find_map(|line| {
        line.trim()
            .strip_prefix("Episode:")
            .map(str::trim)
            .filter(|value| !value.is_empty())
    }) {
        return value.to_owned();
    }

    let prefix = match origin {
        Origin::UserInput => "p1-",
        Origin::Dispatch { .. } => "p1-agent-",
    };
    format!("{prefix}{}", &hex_sha256(text.trim().as_bytes())[..12])
}

#[cfg(unix)]
fn path_bytes(path: &Path) -> &[u8] {
    use std::os::unix::ffi::OsStrExt;
    path.as_os_str().as_bytes()
}

fn hex_sha256(input: &[u8]) -> String {
    let digest = sha256(input);
    let mut output = String::with_capacity(64);
    const HEX: &[u8; 16] = b"0123456789abcdef";
    for byte in digest {
        output.push(HEX[(byte >> 4) as usize] as char);
        output.push(HEX[(byte & 0x0f) as usize] as char);
    }
    output
}

// FIPS PUB 180-4 SHA-256. Kept here to preserve the crate's std-only boundary.
fn sha256(input: &[u8]) -> [u8; 32] {
    const INITIAL: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
        0x5be0cd19,
    ];
    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4,
        0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe,
        0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f,
        0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
        0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc,
        0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
        0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116,
        0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
        0xc67178f2,
    ];

    let bit_len = (input.len() as u64).wrapping_mul(8);
    let padded_len = (input.len() + 9).div_ceil(64) * 64;
    let mut padded = vec![0_u8; padded_len];
    padded[..input.len()].copy_from_slice(input);
    padded[input.len()] = 0x80;
    padded[padded_len - 8..].copy_from_slice(&bit_len.to_be_bytes());

    let mut state = INITIAL;
    for block in padded.as_chunks::<64>().0 {
        let mut words = [0_u32; 64];
        for (index, bytes) in block.as_chunks::<4>().0.iter().enumerate() {
            words[index] = u32::from_be_bytes(*bytes);
        }
        for index in 16..64 {
            let s0 = words[index - 15].rotate_right(7)
                ^ words[index - 15].rotate_right(18)
                ^ (words[index - 15] >> 3);
            let s1 = words[index - 2].rotate_right(17)
                ^ words[index - 2].rotate_right(19)
                ^ (words[index - 2] >> 10);
            words[index] = words[index - 16]
                .wrapping_add(s0)
                .wrapping_add(words[index - 7])
                .wrapping_add(s1);
        }

        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut h] = state;
        for index in 0..64 {
            let sum1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let choice = (e & f) ^ ((!e) & g);
            let temp1 = h
                .wrapping_add(sum1)
                .wrapping_add(choice)
                .wrapping_add(K[index])
                .wrapping_add(words[index]);
            let sum0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let majority = (a & b) ^ (a & c) ^ (b & c);
            let temp2 = sum0.wrapping_add(majority);
            h = g;
            g = f;
            f = e;
            e = d.wrapping_add(temp1);
            d = c;
            c = b;
            b = a;
            a = temp1.wrapping_add(temp2);
        }
        for (slot, value) in state.iter_mut().zip([a, b, c, d, e, f, g, h]) {
            *slot = slot.wrapping_add(value);
        }
    }

    let mut digest = [0_u8; 32];
    for (bytes, value) in digest.as_chunks_mut::<4>().0.iter_mut().zip(state) {
        bytes.copy_from_slice(&value.to_be_bytes());
    }
    digest
}

#[cfg(test)]
mod tests {
    use super::hex_sha256;

    #[test]
    fn sha256_matches_fips_180_4_vectors() {
        assert_eq!(
            hex_sha256(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(
            hex_sha256(b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"),
            "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1"
        );
        assert_eq!(
            hex_sha256(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }
}
