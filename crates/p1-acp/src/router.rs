//! Several ACP sessions behind one `p1 acp` process (ADR-0156): the router answers
//! `initialize`, starts one session process per `session/new` and relays each
//! session's traffic to it.
//!
//! A session process is the single-session driver of [`crate::driver`] for one
//! workspace: its own agent, cancel token, hold, approvals, workers and workflow
//! runs. The host starts it through a [`SessionLauncher`]; tests start one in
//! process. The router gives every session its own id and rewrites `sessionId` both
//! ways. Each session's lines are relayed in order by one task, so an update its
//! process wrote before a response reaches the client before that response.
//! `session/close` closes the process's input: the process cancels its prompt and its
//! work as on EOF, answers the prompt `cancelled`, and exits; the close then answers.
//! When the client goes away every process is closed the same way, and the router
//! returns only once all of them have exited.

use crate::capabilities;
use crate::driver::io::{self, Handler, Peer, RpcError};
use crate::driver::{Reader, Watched, Writer};
use p1_contracts::{BoxFuture, CancellationToken};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::{mpsc, oneshot};

const INVALID_PARAMS: i64 = -32602;
/// JSON-RPC's server-error range: a request that is well formed but not allowed now.
const NOT_ALLOWED: i64 = -32000;

/// One started session process: the pipe pair it speaks ACP over, and its exit.
pub struct Launched {
    /// What the process writes (its stdout).
    pub reader: Reader,
    /// What the process reads (its stdin).
    pub writer: Writer,
    /// Resolves once the process has exited.
    pub exited: BoxFuture<'static, ()>,
}

/// Starts the session process that serves one workspace.
pub trait SessionLauncher: Send + Sync {
    fn launch(&self, workspace: &Path) -> std::io::Result<Launched>;
}

/// Serve the client on `reader` and `writer` until it goes away and every session
/// process has exited; the process exit code. Like the driver it needs a current-thread
/// runtime: a session's lines keep their order through it only while tasks run one
/// at a time. `default_workspace` is the operator's
/// `--workspace`: the folder of a `session/new` that names no `cwd`.
pub async fn serve(
    reader: Reader,
    writer: Writer,
    launcher: Arc<dyn SessionLauncher>,
    default_workspace: Option<PathBuf>,
) -> i32 {
    let gone = CancellationToken::new();
    let router = Arc::new(Router {
        launcher,
        default_workspace,
        initialize: Mutex::new(None),
        sessions: Mutex::new(HashMap::new()),
        processes: Mutex::new(Processes::default()),
        next: AtomicU64::new(1),
    });
    let (_peer, connection) = io::spawn(Watched::new(reader, gone.clone()), writer, router.clone());
    // The client went away: every session's input closes, so each process cancels
    // its prompt and its work and exits, and the prompts the transport still waits
    // for are answered.
    let closer = {
        let router = router.clone();
        tokio::spawn(async move {
            gone.cancelled().await;
            router.shut_down();
        })
    };
    let code = match connection.await {
        Ok(Ok(())) => 0,
        Ok(Err(error)) => {
            eprintln!("p1 acp: connection failed: {error}");
            1
        }
        Err(error) => {
            eprintln!("p1 acp: connection task failed: {error}");
            1
        }
    };
    // A connection that failed without EOF still ends its sessions. Return only once
    // every session process has exited: a process left behind would be killed with
    // this one while it still stops its work.
    closer.abort();
    for session in router.shut_down() {
        session.ended.cancelled().await;
    }
    code
}

/// Every session process started and not known to have ended, and whether the router
/// is shutting down, so a session that is still starting is closed too.
#[derive(Default)]
struct Processes {
    started: Vec<Arc<Session>>,
    closing: bool,
}

struct Router {
    launcher: Arc<dyn SessionLauncher>,
    default_workspace: Option<PathBuf>,
    /// The client's `initialize` parameters, replayed to every session process so it
    /// negotiates what the client asked for.
    initialize: Mutex<Option<Value>>,
    sessions: Mutex<HashMap<String, Arc<Session>>>,
    processes: Mutex<Processes>,
    next: AtomicU64,
}

fn invalid(message: impl Into<String>) -> RpcError {
    RpcError::new(INVALID_PARAMS, message)
}

fn session_id(params: &Value) -> Option<&str> {
    params.get("sessionId").and_then(Value::as_str)
}

/// `params` with its `sessionId` replaced; ACP names the session there and only there.
fn with_session(mut params: Value, id: &str) -> Value {
    if let Some(field) = params.get_mut("sessionId") {
        *field = json!(id);
    }
    params
}

impl Router {
    /// Close every session process, the ones still starting included, and refuse new
    /// ones; the processes to wait for. All inputs close before any wait, so one slow
    /// process does not hold the others open.
    fn shut_down(&self) -> Vec<Arc<Session>> {
        self.sessions.lock().unwrap().clear();
        let mut processes = self.processes.lock().unwrap();
        processes.closing = true;
        for session in &processes.started {
            session.close();
        }
        processes.started.clone()
    }

    /// Track a started process; `false` when the router is shutting down, and the
    /// process is then closed at once.
    fn track(&self, session: &Arc<Session>) -> bool {
        let mut processes = self.processes.lock().unwrap();
        processes
            .started
            .retain(|started| !started.ended.is_cancelled());
        processes.started.push(session.clone());
        if processes.closing {
            session.close();
        }
        !processes.closing
    }

    fn session(&self, params: &Value) -> Result<Arc<Session>, RpcError> {
        session_id(params)
            .and_then(|id| self.sessions.lock().unwrap().get(id).cloned())
            .ok_or_else(|| invalid("unknown sessionId"))
    }

    fn initialize(&self, params: Value) -> Result<Value, RpcError> {
        let version = params
            .get("protocolVersion")
            .and_then(Value::as_u64)
            .ok_or_else(|| invalid("initialize needs a numeric protocolVersion"))?;
        let meta = params
            .pointer("/clientCapabilities/_meta")
            .and_then(Value::as_object);
        let (codec, mut capabilities) =
            capabilities::initialize(u16::try_from(version).unwrap_or(u16::MAX), meta);
        capabilities.close_sessions = true;
        *self.initialize.lock().unwrap() = Some(params);
        Ok(codec.encode_capabilities(&capabilities))
    }

    /// The session's folder: the client's `cwd`, an existing absolute directory, or the
    /// operator's `--workspace` when the client names none.
    fn workspace(&self, params: &Value) -> Result<PathBuf, RpcError> {
        match params.get("cwd") {
            None | Some(Value::Null) => self
                .default_workspace
                .clone()
                .ok_or_else(|| invalid("session/new needs cwd")),
            Some(Value::String(cwd)) => {
                let path = PathBuf::from(cwd);
                if !path.is_absolute() {
                    return Err(invalid(format!("cwd {cwd} is not an absolute path")));
                }
                if !path.is_dir() {
                    return Err(invalid(format!("cwd {cwd} is not an existing directory")));
                }
                Ok(path)
            }
            Some(_) => Err(invalid("cwd must be a string")),
        }
    }

    async fn new_session(&self, client: Peer, mut params: Value) -> Result<Value, RpcError> {
        let Some(initialize) = self.initialize.lock().unwrap().clone() else {
            return Err(RpcError::new(NOT_ALLOWED, "initialize first"));
        };
        if !params.is_object() {
            return Err(invalid("session/new needs an object of params"));
        }
        let workspace = self.workspace(&params)?;
        params["cwd"] = json!(workspace);
        let launched = self.launcher.launch(&workspace).map_err(|error| {
            RpcError::new(
                NOT_ALLOWED,
                format!(
                    "could not start a session for {}: {error}",
                    workspace.display()
                ),
            )
        })?;
        let id = format!(
            "p1-{}-{}",
            std::process::id(),
            self.next.fetch_add(1, Ordering::Relaxed)
        );
        let session = Session::start(id.clone(), launched, client);
        if !self.track(&session) {
            return Err(RpcError::new(NOT_ALLOWED, "p1 acp is shutting down"));
        }
        let opened = async {
            session.request("initialize", initialize).await?;
            session.request("session/new", params).await
        };
        let mut result = match opened.await {
            Ok(result) => result,
            Err(error) => {
                session.close();
                session.ended.cancelled().await;
                return Err(error);
            }
        };
        let Some(inner) = session_id(&result).map(str::to_string) else {
            session.close();
            session.ended.cancelled().await;
            return Err(RpcError::new(
                -32603,
                "the session process answered without a sessionId",
            ));
        };
        *session.inner.lock().unwrap() = inner;
        result["sessionId"] = json!(id);
        if self.processes.lock().unwrap().closing {
            return Err(RpcError::new(NOT_ALLOWED, "p1 acp is shutting down"));
        }
        self.sessions.lock().unwrap().insert(id, session);
        Ok(result)
    }

    async fn close_session(&self, params: Value) -> Result<Value, RpcError> {
        let session = session_id(&params)
            .and_then(|id| self.sessions.lock().unwrap().remove(id))
            .ok_or_else(|| invalid("unknown sessionId"))?;
        session.close();
        session.ended.cancelled().await;
        Ok(json!({}))
    }
}

impl Handler for Router {
    fn request(
        &self,
        peer: Peer,
        method: String,
        params: Value,
    ) -> BoxFuture<'_, Result<Value, RpcError>> {
        Box::pin(async move {
            match method.as_str() {
                "initialize" => self.initialize(params),
                "session/new" => self.new_session(peer, params).await,
                "session/close" => self.close_session(params).await,
                _ if session_id(&params).is_some() => {
                    let session = self.session(&params)?;
                    let inner = session.inner.lock().unwrap().clone();
                    session.request(&method, with_session(params, &inner)).await
                }
                _ => Err(RpcError::method_not_found()),
            }
        })
    }

    fn notification(&self, _peer: Peer, method: String, params: Value) -> BoxFuture<'_, ()> {
        Box::pin(async move {
            if let Ok(session) = self.session(&params) {
                let inner = session.inner.lock().unwrap().clone();
                session.notify(&method, with_session(params, &inner));
            }
        })
    }
}

type Reply = oneshot::Sender<Result<Value, RpcError>>;

enum Write {
    Line(Value),
    Close,
}

/// One session process as the router sees it.
struct Session {
    /// The router's id for the session, the one the client sees.
    id: String,
    /// The session process's own id for it.
    inner: Mutex<String>,
    to_process: mpsc::UnboundedSender<Write>,
    /// The router's requests to the process that await their response; `None` once
    /// the process's output ended, so nothing asked later waits for ever.
    pending: Mutex<Option<HashMap<u64, Reply>>>,
    next: AtomicU64,
    /// Cancelled once the process's output ended and the process exited.
    ended: CancellationToken,
}

impl Session {
    fn start(id: String, launched: Launched, client: Peer) -> Arc<Self> {
        let Launched {
            reader,
            writer,
            exited,
        } = launched;
        let (to_process, writes) = mpsc::unbounded_channel();
        let session = Arc::new(Self {
            id,
            inner: Mutex::new(String::new()),
            to_process,
            pending: Mutex::new(Some(HashMap::new())),
            next: AtomicU64::new(0),
            ended: CancellationToken::new(),
        });
        tokio::spawn(write_loop(writer, writes));
        let relay = session.clone();
        tokio::spawn(async move {
            relay.relay(reader, client).await;
            exited.await;
            relay.ended.cancel();
        });
        session
    }

    fn send(&self, message: Value) {
        let _ = self.to_process.send(Write::Line(message));
    }

    fn notify(&self, method: &str, params: Value) {
        self.send(json!({"jsonrpc":"2.0","method":method,"params":params}));
    }

    async fn request(&self, method: &str, params: Value) -> Result<Value, RpcError> {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let (reply, answer) = oneshot::channel();
        match self.pending.lock().unwrap().as_mut() {
            Some(pending) => pending.insert(id, reply),
            None => return Err(RpcError::new(NOT_ALLOWED, "the session has ended")),
        };
        self.send(json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}));
        answer
            .await
            .unwrap_or_else(|_| Err(RpcError::new(NOT_ALLOWED, "the session has ended")))
    }

    /// Close the process's input; it ends its session as on EOF.
    fn close(&self) {
        let _ = self.to_process.send(Write::Close);
    }

    /// Relay the process's lines to the client in the order it wrote them: its
    /// notifications and requests under the router's session id, its responses to the
    /// router's requests that await them.
    async fn relay(&self, reader: Reader, client: Peer) {
        let mut lines = BufReader::new(reader).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            let Ok(message) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            if let Some(method) = message.get("method").and_then(Value::as_str) {
                let params = with_session(
                    message.get("params").cloned().unwrap_or(Value::Null),
                    &self.id,
                );
                match message.get("id").cloned() {
                    None => {
                        let _ = client.notify(method, params);
                    }
                    // A permission request: the client's answer goes back under the
                    // process's own request id.
                    Some(id) => {
                        let asked = client.request(method, params);
                        let to_process = self.to_process.clone();
                        tokio::spawn(async move {
                            let response = match asked.await {
                                Ok(result) => json!({"jsonrpc":"2.0","id":id,"result":result}),
                                Err(error) => json!({"jsonrpc":"2.0","id":id,"error":error}),
                            };
                            let _ = to_process.send(Write::Line(response));
                        });
                    }
                }
            } else if let Some(id) = message.get("id").and_then(Value::as_u64) {
                let outcome = match (message.get("result"), message.get("error")) {
                    (Some(result), _) => Ok(result.clone()),
                    (None, Some(error)) => Err(serde_json::from_value(error.clone())
                        .unwrap_or_else(|_| RpcError::new(-32603, "malformed error"))),
                    (None, None) => Err(RpcError::new(-32603, "malformed response")),
                };
                let reply = self
                    .pending
                    .lock()
                    .unwrap()
                    .as_mut()
                    .and_then(|pending| pending.remove(&id));
                if let Some(reply) = reply {
                    let _ = reply.send(outcome);
                    // Let the waiting handler answer the client before the next line
                    // goes out (one thread, tasks in turn): the command list the
                    // process sends after its `session/new` answer must follow the
                    // router's answer too (#676).
                    tokio::task::yield_now().await;
                }
            }
        }
        // The process is gone: nothing it was asked will be answered.
        self.pending.lock().unwrap().take();
    }
}

async fn write_loop(mut writer: Writer, mut writes: mpsc::UnboundedReceiver<Write>) {
    while let Some(write) = writes.recv().await {
        match write {
            Write::Line(message) => {
                let mut line = serde_json::to_vec(&message).expect("a JSON value serializes");
                line.push(b'\n');
                if writer.write_all(&line).await.is_err() || writer.flush().await.is_err() {
                    break;
                }
            }
            Write::Close => break,
        }
    }
    let _ = writer.shutdown().await;
}
