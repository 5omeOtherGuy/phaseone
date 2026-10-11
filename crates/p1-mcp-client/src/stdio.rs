//! The stdio transport: a child process speaking newline-delimited JSON-RPC.
//!
//! The child gets exactly the environment the [`Transport`](p1_contracts::mcp::Transport)
//! names and nothing it would otherwise inherit from p1; its stderr is discarded, so a
//! server's log can never carry a value into p1's output. A reader task owns stdout: it
//! routes each response to the request waiting for it, answers the server's own requests
//! and fails every waiting request once the child's output ends.

use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use p1_contracts::mcp::McpError;
use p1_contracts::{BoxFuture, CancellationToken};
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

use crate::rpc::{self, Incoming};
use crate::{Channel, MAX_MESSAGE_BYTES, NOTIFY_TIMEOUT};

type Waiter = oneshot::Sender<Result<Value, McpError>>;

/// Requests in flight and, once the child's output ended, why. One lock for both, so a
/// request registered after the reader failed everything sees the reason instead of waiting
/// forever.
#[derive(Default)]
struct State {
    waiting: HashMap<u64, Waiter>,
    closed: Option<String>,
}

#[derive(Default)]
struct Shared {
    state: Mutex<State>,
}

impl Shared {
    fn state(&self) -> MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Fail every waiting request and every later one with `reason`.
    fn close(&self, reason: String) {
        let mut state = self.state();
        if state.closed.is_none() {
            state.closed = Some(reason.clone());
        }
        for (_, waiter) in state.waiting.drain() {
            let _ = waiter.send(Err(McpError::Closed(reason.clone())));
        }
    }
}

/// Removes one request's entry from the map when the request ends.
struct Waiting<'a> {
    shared: &'a Shared,
    id: u64,
}

impl Drop for Waiting<'_> {
    fn drop(&mut self) {
        self.shared.state().waiting.remove(&self.id);
    }
}

pub(crate) struct StdioChannel {
    shared: Arc<Shared>,
    stdin: Arc<tokio::sync::Mutex<ChildStdin>>,
    next_id: AtomicU64,
    reader: JoinHandle<()>,
    /// Held so the child lives as long as the channel; `kill_on_drop` ends it with it.
    _child: Child,
}

impl Drop for StdioChannel {
    fn drop(&mut self) {
        self.reader.abort();
    }
}

impl StdioChannel {
    pub(crate) fn spawn(
        command: &str,
        args: &[String],
        env: &BTreeMap<String, String>,
        cwd: Option<&Path>,
    ) -> Result<Self, McpError> {
        let mut process = Command::new(command);
        process
            .args(args)
            .env_clear()
            .envs(env)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            // Its own process group, as a shell command gets: a Ctrl-C that interrupts a turn
            // reaches p1's foreground group and must not end the server for the session.
            .process_group(0)
            .kill_on_drop(true);
        if let Some(cwd) = cwd {
            process.current_dir(cwd);
        }
        let mut child = process
            .spawn()
            .map_err(|error| McpError::Connect(format!("cannot start `{command}`: {error}")))?;
        let (Some(stdin), Some(stdout)) = (child.stdin.take(), child.stdout.take()) else {
            return Err(McpError::Connect(format!(
                "`{command}` started without piped stdin and stdout"
            )));
        };
        let shared = Arc::new(Shared::default());
        let stdin = Arc::new(tokio::sync::Mutex::new(stdin));
        let reader = tokio::spawn(read_loop(stdout, shared.clone(), stdin.clone()));
        Ok(Self {
            shared,
            stdin,
            next_id: AtomicU64::new(1),
            reader,
            _child: child,
        })
    }

    async fn send(&self, message: &Value) -> Result<(), McpError> {
        write_message(&self.stdin, message).await
    }

    async fn request_inner(
        &self,
        method: &str,
        params: Value,
        cancel: CancellationToken,
    ) -> Result<Value, McpError> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (sender, receiver) = oneshot::channel();
        {
            let mut state = self.shared.state();
            if let Some(reason) = &state.closed {
                return Err(McpError::Closed(reason.clone()));
            }
            state.waiting.insert(id, sender);
        }
        // However this request ends (answered, failed, cancelled or dropped by its caller),
        // its entry leaves the map.
        let _waiting = Waiting {
            shared: &self.shared,
            id,
        };
        self.send(&rpc::request(id, method, params)).await?;
        tokio::select! {
            answer = receiver => answer.unwrap_or_else(|_| {
                Err(McpError::Closed("the server's output ended".into()))
            }),
            () = cancel.cancelled() => {
                // Best effort and bounded: the server may already have answered, gone, or
                // stopped reading.
                let _ = tokio::time::timeout(
                    NOTIFY_TIMEOUT,
                    self.send(&rpc::notification(
                        "notifications/cancelled",
                        serde_json::json!({ "requestId": id, "reason": "cancelled by p1" }),
                    )),
                )
                .await;
                Err(McpError::Cancelled)
            }
        }
    }
}

impl Channel for StdioChannel {
    fn request<'a>(
        &'a self,
        method: &'a str,
        params: Value,
        cancel: CancellationToken,
    ) -> BoxFuture<'a, Result<Value, McpError>> {
        Box::pin(self.request_inner(method, params, cancel))
    }

    fn notify<'a>(&'a self, method: &'a str, params: Value) -> BoxFuture<'a, Result<(), McpError>> {
        Box::pin(async move { self.send(&rpc::notification(method, params)).await })
    }
}

async fn write_message(
    stdin: &tokio::sync::Mutex<ChildStdin>,
    message: &Value,
) -> Result<(), McpError> {
    // `to_string` never emits a raw newline, so one line is exactly one message.
    let mut line = message.to_string();
    line.push('\n');
    let mut stdin = stdin.lock().await;
    stdin
        .write_all(line.as_bytes())
        .await
        .and(stdin.flush().await)
        .map_err(|error| McpError::Closed(format!("cannot write to the server: {error}")))
}

async fn read_loop(
    stdout: ChildStdout,
    shared: Arc<Shared>,
    stdin: Arc<tokio::sync::Mutex<ChildStdin>>,
) {
    let mut reader = BufReader::new(stdout);
    let mut line = Vec::new();
    let reason = loop {
        line.clear();
        match read_line(&mut reader, &mut line).await {
            Ok(false) => break "the server's output ended".to_owned(),
            Ok(true) => {}
            Err(reason) => break reason,
        }
        if line.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        let message = match serde_json::from_slice::<Value>(&line) {
            Ok(message) => message,
            Err(error) => break format!("the server wrote a line that is not JSON: {error}"),
        };
        match rpc::classify(message) {
            Incoming::Response { id, result } => {
                let waiter = shared.state().waiting.remove(&id);
                if let Some(waiter) = waiter {
                    let _ = waiter.send(result);
                }
            }
            Incoming::Request { id, method } => {
                if write_message(&stdin, &rpc::answer(id, &method))
                    .await
                    .is_err()
                {
                    break "cannot answer the server".to_owned();
                }
            }
            Incoming::Notification => {}
            Incoming::Invalid(reason) => break reason,
        }
    };
    shared.close(reason);
}

/// Read one line into `line`, without its terminator. `Ok(false)` at end of output; a line
/// longer than [`MAX_MESSAGE_BYTES`] ends the connection rather than growing without bound.
async fn read_line(
    reader: &mut BufReader<ChildStdout>,
    line: &mut Vec<u8>,
) -> Result<bool, String> {
    loop {
        let buffer = reader
            .fill_buf()
            .await
            .map_err(|error| format!("cannot read from the server: {error}"))?;
        if buffer.is_empty() {
            return Ok(!line.is_empty());
        }
        let (taken, done) = match buffer.iter().position(|&byte| byte == b'\n') {
            Some(end) => (end + 1, true),
            None => (buffer.len(), false),
        };
        let end = if done { taken - 1 } else { taken };
        if line.len() + end > MAX_MESSAGE_BYTES {
            return Err(format!(
                "the server wrote a message larger than {MAX_MESSAGE_BYTES} bytes"
            ));
        }
        line.extend_from_slice(&buffer[..end]);
        reader.consume(taken);
        if done {
            return Ok(true);
        }
    }
}
