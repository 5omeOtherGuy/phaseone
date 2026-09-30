//! Issue #484: the refresh lock holds across PROCESSES, not only between two sources
//! in one runtime. A helper process (this test binary, run again for one ignored
//! test) holds a Claude Code refresh open; this process refreshes the same login and
//! must wait for it, then use its rotation instead of sending a second one.
//!
//! The helper learns its scratch directory from stdin, never from the environment,
//! and the two processes synchronize through marker files, never through a clock.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::Duration;

use p1_auth::ClaudeCodeCredentials;
use p1_contracts::BoxFuture;
use p1_provider_http::testing::{BodyEnd, ScriptedResponse, ScriptedTransport};
use p1_provider_http::{CredentialSource, HttpRequest, HttpResponse, Transport, TransportError};
use serde_json::json;

const HELPER: &str = "helper_holds_a_refresh_until_released";

/// Says "entered" by creating a marker file, then holds the request until the other
/// process creates "release".
struct FileGate {
    inner: ScriptedTransport,
    dir: PathBuf,
}

impl Transport for FileGate {
    fn post<'a>(
        &'a self,
        request: HttpRequest,
    ) -> BoxFuture<'a, Result<HttpResponse, TransportError>> {
        Box::pin(async move {
            std::fs::write(self.dir.join("entered"), "").unwrap();
            while !self.dir.join("release").exists() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            self.inner.post(request).await
        })
    }
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .unwrap()
}

fn login(dir: &Path) -> PathBuf {
    dir.join("login/.credentials.json")
}

/// Wait for a marker file the other process creates (explicit synchronization).
fn wait_for(path: &Path) {
    for _ in 0..6000 {
        if path.exists() {
            return;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    panic!("{} never appeared", path.display());
}

#[test]
#[ignore = "a helper process of two_processes_refreshing_one_login_rotate_it_once"]
fn helper_holds_a_refresh_until_released() {
    let mut line = String::new();
    std::io::stdin().read_line(&mut line).unwrap();
    let dir = PathBuf::from(line.trim());
    let inner = ScriptedTransport::new(vec![ScriptedResponse {
        status: 200,
        headers: Vec::new(),
        chunks: vec![
            json!({
                "access_token": "FAKE-NEW", "refresh_token": "FAKE-NEW-REFRESH", "expires_in": 3600,
            })
            .to_string()
            .into_bytes(),
        ],
        end: BodyEnd::Eof,
    }]);
    let source = ClaudeCodeCredentials::at(login(&dir), Arc::new(FileGate { inner, dir }));
    let credential = runtime().block_on(source.access()).unwrap();
    assert_eq!(credential.bearer, "FAKE-NEW");
}

#[test]
fn two_processes_refreshing_one_login_rotate_it_once() {
    let scratch = tempfile::tempdir().unwrap();
    let dir = scratch.path().to_path_buf();
    std::fs::create_dir(dir.join("login")).unwrap();
    std::fs::write(
        login(&dir),
        json!({ "claudeAiOauth": {
            "accessToken": "FAKE-OLD", "refreshToken": "FAKE-OLD-REFRESH", "expiresAt": 1,
        }})
        .to_string(),
    )
    .unwrap();

    let mut helper = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            HELPER,
            "--ignored",
            "--nocapture",
            "--test-threads=1",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    writeln!(helper.stdin.take().unwrap(), "{}", dir.display()).unwrap();
    wait_for(&dir.join("entered"));

    // This process has NO scripted response: a second rotation would fail it.
    let source =
        ClaudeCodeCredentials::at(login(&dir), Arc::new(ScriptedTransport::new(Vec::new())));
    let credential = runtime().block_on(async {
        let access = source.access();
        tokio::pin!(access);
        tokio::select! {
            biased;
            result = &mut access => panic!("the other process holds the lock: {result:?}"),
            () = tokio::task::yield_now() => {}
        }
        std::fs::write(dir.join("release"), "").unwrap();
        access.await
    });

    let output = helper.wait_with_output().unwrap();
    let mut log = String::new();
    log.push_str(&String::from_utf8_lossy(&output.stdout));
    log.push_str(&String::from_utf8_lossy(&output.stderr));
    assert!(output.status.success(), "the helper failed:\n{log}");
    assert!(
        log.contains("1 passed"),
        "the helper test did not run:\n{log}"
    );
    assert_eq!(credential.unwrap().bearer, "FAKE-NEW");
    let mut written = String::new();
    std::fs::File::open(login(&dir))
        .unwrap()
        .read_to_string(&mut written)
        .unwrap();
    assert!(written.contains("FAKE-NEW-REFRESH"), "{written}");
}
