//! Newline-delimited JSON-RPC 2.0, independent of sessions and ACP payloads.
//!
//! Inbound lines may contain up to 16 MiB before the LF: enough for large embedded
//! resources, while bounding the reader's allocation. Oversized lines are
//! discarded through the next LF and receive Invalid Request; framing resumes.
//! EOF also terminates a final nonempty line. No payloads are logged here.

use p1_contracts::{BoxFuture, CancellationToken};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    future::Future,
    sync::{Arc, Mutex, Weak},
};
use tokio::{
    io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader},
    sync::{mpsc, oneshot},
    task::{JoinHandle, JoinSet},
};

pub const MAX_MESSAGE_BYTES: usize = 16 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RpcError {
    pub code: i64,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

impl RpcError {
    pub fn new(code: i64, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            data: None,
        }
    }

    pub fn method_not_found() -> Self {
        Self::new(-32601, "Method not found")
    }

    fn connection_closed() -> Self {
        Self::new(-32000, "connection closed")
    }
}

impl std::fmt::Display for RpcError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.message.fmt(f)
    }
}

impl std::error::Error for RpcError {}

/// Callbacks run concurrently so a pending request cannot block notifications,
/// cancellation, or responses to a handler's own outbound requests.
///
/// The order is structural (#733). The reader calls [`Handler::call`] and
/// [`Handler::notified`] itself, one message at a time in the order they arrived, and
/// only the futures they return run concurrently: what a handler does in the body of
/// those calls happens in arrival order. The default bodies run
/// [`Handler::request`] and [`Handler::notification`] in the spawned future, so a
/// handler that needs the order overrides `call` and `notified`. A request's answer
/// goes through its [`Responder`] onto the one writer queue, so whatever is sent after
/// [`Responder::answer`] returns goes out after the answer, on any runtime.
pub trait Handler: Send + Sync + 'static {
    /// Take one request, in arrival order. The default answers with
    /// [`Handler::request`]'s outcome once it is ready.
    fn call(
        self: Arc<Self>,
        peer: Peer,
        method: String,
        params: Value,
        responder: Responder,
    ) -> BoxFuture<'static, ()> {
        Box::pin(async move {
            let outcome = self.request(peer, method, params).await;
            responder.answer(outcome);
        })
    }

    /// Take one notification, in arrival order. The default runs
    /// [`Handler::notification`].
    fn notified(
        self: Arc<Self>,
        peer: Peer,
        method: String,
        params: Value,
    ) -> BoxFuture<'static, ()> {
        Box::pin(async move { self.notification(peer, method, params).await })
    }

    fn request(
        &self,
        _peer: Peer,
        _method: String,
        _params: Value,
    ) -> BoxFuture<'_, Result<Value, RpcError>> {
        Box::pin(async { Err(RpcError::method_not_found()) })
    }

    /// Unknown notifications are ignored by default. `$/cancel_request` is
    /// delivered here unchanged; its meaning belongs to the session handler.
    fn notification(&self, _peer: Peer, _method: String, _params: Value) -> BoxFuture<'_, ()> {
        Box::pin(async {})
    }
}

impl Handler for () {}

enum Write {
    Message(Value),
    Close,
}

type Reply = oneshot::Sender<Result<Value, RpcError>>;

struct State {
    next_id: u64,
    pending: HashMap<u64, Reply>,
    closed: bool,
    input_closed: bool,
    writer: mpsc::UnboundedSender<Write>,
}

struct Shared {
    state: Mutex<State>,
    closed: CancellationToken,
}

impl Shared {
    fn finish_input(&self) {
        let mut state = self.state.lock().unwrap();
        state.input_closed = true;
        // No more responses can arrive; unblock handlers before draining them.
        for (_, reply) in state.pending.drain() {
            let _ = reply.send(Err(RpcError::connection_closed()));
        }
    }

    fn close(&self) {
        let mut state = self.state.lock().unwrap();
        if !state.closed {
            state.closed = true;
            self.closed.cancel();
            for (_, reply) in state.pending.drain() {
                let _ = reply.send(Err(RpcError::connection_closed()));
            }
            // Serialize close with sends: accepted messages flush before EOF.
            let _ = state.writer.send(Write::Close);
        }
    }

    fn send(&self, message: Value) -> Result<(), RpcError> {
        let state = self.state.lock().unwrap();
        if state.closed {
            return Err(RpcError::connection_closed());
        }
        if state.writer.send(Write::Message(message)).is_err() {
            drop(state);
            self.close();
            return Err(RpcError::connection_closed());
        }
        Ok(())
    }
}

/// Cloneable producer handle. Calls enqueue synchronously, in call order;
/// polling the returned request future is not needed to send the request.
#[derive(Clone)]
pub struct Peer(Arc<Shared>);

impl Peer {
    pub fn notify(&self, method: impl Into<String>, params: Value) -> Result<(), RpcError> {
        self.0.send(call(method.into(), params, None)?)
    }

    pub fn request(
        &self,
        method: impl Into<String>,
        params: Value,
    ) -> impl Future<Output = Result<Value, RpcError>> + Send + 'static {
        let (reply, answer) = oneshot::channel();
        let mut state = self.0.state.lock().unwrap();
        let id = state.next_id;
        state.next_id += 1;
        let message = call(method.into(), params, Some(json!(id)));
        if state.closed || state.input_closed {
            let _ = reply.send(Err(RpcError::connection_closed()));
        } else {
            match message {
                Err(error) => {
                    let _ = reply.send(Err(error));
                }
                Ok(message) => {
                    state.pending.insert(id, reply);
                    if state.writer.send(Write::Message(message)).is_err() {
                        drop(state);
                        self.0.close();
                    }
                }
            }
        }
        let pending = Pending {
            id,
            shared: Arc::downgrade(&self.0),
        };
        async move {
            // Capture before polling so dropping an unpolled future also
            // removes its pending entry. A late response is then ignored.
            let _pending = pending;
            answer
                .await
                .unwrap_or_else(|_| Err(RpcError::connection_closed()))
        }
    }

    /// Stop reading, flush already queued writes, and fail pending requests.
    pub fn close(&self) {
        self.0.close();
    }

    fn respond(&self, id: Value, outcome: Result<Value, RpcError>) {
        let message = match outcome {
            Ok(result) => json!({"jsonrpc":"2.0","id":id,"result":result}),
            Err(error) => json!({"jsonrpc":"2.0","id":id,"error":error}),
        };
        let _ = self.0.send(message);
    }
}

/// The answer to one request. [`Responder::answer`] puts it on the connection's one
/// writer queue at once, so what the caller writes afterwards follows it. Dropped
/// unanswered, it answers with an internal error: a client never waits for ever.
pub struct Responder {
    peer: Peer,
    id: Option<Value>,
    /// Keeps the connection open after EOF until this request is answered.
    _alive: mpsc::Sender<()>,
}

impl Responder {
    pub fn answer(mut self, outcome: Result<Value, RpcError>) {
        if let Some(id) = self.id.take() {
            self.peer.respond(id, outcome);
        }
    }
}

impl Drop for Responder {
    fn drop(&mut self) {
        if let Some(id) = self.id.take() {
            self.peer.respond(
                id,
                Err(RpcError::new(-32603, "the request was not answered")),
            );
        }
    }
}

struct Pending {
    id: u64,
    shared: Weak<Shared>,
}

impl Drop for Pending {
    fn drop(&mut self) {
        if let Some(shared) = self.shared.upgrade() {
            shared.state.lock().unwrap().pending.remove(&self.id);
        }
    }
}

struct Connection(Arc<Shared>);

impl Drop for Connection {
    fn drop(&mut self) {
        self.0.close();
    }
}

/// Start one connection and its sole writer task. Dropping the returned join
/// handle detaches the connection as usual for Tokio; Peer::close stops it.
/// Aborting the connection also fails pending requests and stops its writer.
pub fn spawn<R, W>(
    reader: R,
    writer: W,
    handler: Arc<dyn Handler>,
) -> (Peer, JoinHandle<std::io::Result<()>>)
where
    R: AsyncRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
{
    let (tx, rx) = mpsc::unbounded_channel();
    let peer = Peer(Arc::new(Shared {
        state: Mutex::new(State {
            next_id: 0,
            pending: HashMap::new(),
            closed: false,
            input_closed: false,
            writer: tx,
        }),
        closed: CancellationToken::new(),
    }));
    let connection = Connection(peer.0.clone());
    let reader_peer = peer.clone();
    let task = tokio::spawn(async move {
        let _connection = connection;
        // JoinSet aborts the writer if this connection task is aborted while
        // output is backpressured; a dropped JoinHandle would detach it.
        let mut writers = JoinSet::new();
        writers.spawn(write_loop(writer, rx));
        tokio::select! {
            result = read_loop(reader, reader_peer.clone(), handler) => {
                reader_peer.close();
                let written = writers.join_next().await.expect("one writer task").map_err(std::io::Error::other)?;
                result.and(written)
            }
            result = writers.join_next() => result.expect("one writer task").map_err(std::io::Error::other)?,
        }
    });
    (peer, task)
}

fn call(method: String, params: Value, id: Option<Value>) -> Result<Value, RpcError> {
    if !params.is_null() && !params.is_object() && !params.is_array() {
        return Err(RpcError::new(-32602, "Invalid params"));
    }
    let mut message = json!({"jsonrpc":"2.0","method":method});
    if !params.is_null() {
        message["params"] = params;
    }
    if let Some(id) = id {
        message["id"] = id;
    }
    Ok(message)
}

async fn write_loop<W: AsyncWrite + Unpin>(
    mut writer: W,
    mut rx: mpsc::UnboundedReceiver<Write>,
) -> std::io::Result<()> {
    while let Some(command) = rx.recv().await {
        match command {
            Write::Message(message) => {
                let mut line = serde_json::to_vec(&message)?;
                line.push(b'\n');
                writer.write_all(&line).await?;
                writer.flush().await?;
            }
            Write::Close => break,
        }
    }
    writer.shutdown().await
}

enum Frame {
    Json(Vec<u8>),
    Oversized,
}

async fn read_frame<R: AsyncRead + Unpin>(
    reader: &mut BufReader<R>,
) -> std::io::Result<Option<Frame>> {
    let mut bytes = Vec::new();
    let mut oversized = false;
    loop {
        let chunk = reader.fill_buf().await?;
        if chunk.is_empty() {
            return Ok(if oversized {
                Some(Frame::Oversized)
            } else if bytes.is_empty() {
                None
            } else {
                Some(Frame::Json(bytes))
            });
        }
        let newline = chunk.iter().position(|&byte| byte == b'\n');
        let length = newline.unwrap_or(chunk.len());
        if !oversized {
            if length > MAX_MESSAGE_BYTES - bytes.len() {
                oversized = true;
                bytes.clear();
            } else {
                bytes.extend_from_slice(&chunk[..length]);
            }
        }
        reader.consume(length + usize::from(newline.is_some()));
        if newline.is_some() {
            return Ok(Some(if oversized {
                Frame::Oversized
            } else {
                Frame::Json(bytes)
            }));
        }
    }
}

enum Inbound {
    Call {
        id: Option<Value>,
        method: String,
        params: Value,
    },
    Response {
        id: Value,
        outcome: Result<Value, RpcError>,
    },
}

fn valid_id(id: &Value) -> bool {
    id.is_null() || id.is_string() || id.is_number()
}

fn inbound(value: Value) -> Option<Inbound> {
    let object = value.as_object()?;
    if object.get("jsonrpc")? != "2.0" {
        return None;
    }
    if let Some(method) = object.get("method") {
        let method = method.as_str()?.to_owned();
        let id = object.get("id").cloned();
        if id.as_ref().is_some_and(|id| !valid_id(id)) {
            return None;
        }
        let params = object.get("params").cloned().unwrap_or(Value::Null);
        if object.contains_key("params") && !params.is_object() && !params.is_array() {
            return None;
        }
        Some(Inbound::Call { id, method, params })
    } else {
        let id = object.get("id")?.clone();
        if !valid_id(&id) {
            return None;
        }
        let outcome = match (object.get("result"), object.get("error")) {
            (Some(result), None) => Ok(result.clone()),
            (None, Some(error)) => Err(serde_json::from_value(error.clone()).ok()?),
            _ => return None,
        };
        Some(Inbound::Response { id, outcome })
    }
}

async fn read_loop<R: AsyncRead + Unpin>(
    reader: R,
    peer: Peer,
    handler: Arc<dyn Handler>,
) -> std::io::Result<()> {
    let mut reader = BufReader::new(reader);
    let mut handlers = JoinSet::new();
    // Every Responder holds a sender; the receiver ends once all are answered.
    let (alive, mut unanswered) = mpsc::channel::<()>(1);
    loop {
        let frame = tokio::select! {
            biased;
            _ = peer.0.closed.cancelled() => return Ok(()),
            frame = read_frame(&mut reader) => frame?,
        };
        let Some(frame) = frame else { break };
        while handlers.try_join_next().is_some() {}
        let message = match frame {
            Frame::Oversized => Err(RpcError::new(
                -32600,
                "Invalid Request: message exceeds 16 MiB limit",
            )),
            Frame::Json(bytes) => serde_json::from_slice::<Value>(&bytes)
                .map_err(|_| RpcError::new(-32700, "Parse error"))
                .and_then(|value| {
                    inbound(value).ok_or_else(|| RpcError::new(-32600, "Invalid Request"))
                }),
        };
        match message {
            Err(error) => peer.respond(Value::Null, Err(error)),
            Ok(Inbound::Response { id, outcome }) => {
                if let Some(id) = id.as_u64() {
                    let reply = peer.0.state.lock().unwrap().pending.remove(&id);
                    if let Some(reply) = reply {
                        let _ = reply.send(outcome);
                    }
                }
            }
            Ok(Inbound::Call { id, method, params }) => {
                // Called here, in arrival order; only the returned future runs
                // concurrently.
                let handler = handler.clone();
                let work = match id {
                    Some(id) => {
                        let responder = Responder {
                            peer: peer.clone(),
                            id: Some(id),
                            _alive: alive.clone(),
                        };
                        handler.call(peer.clone(), method, params, responder)
                    }
                    None => handler.notified(peer.clone(), method, params),
                };
                handlers.spawn(work);
            }
        }
    }
    peer.0.finish_input();
    while !handlers.is_empty() {
        tokio::select! {
            biased;
            _ = peer.0.closed.cancelled() => return Ok(()),
            _ = handlers.join_next() => {},
        }
    }
    // A request handed on to its handler's own task (a prompt the session answers
    // when its turn ends) keeps the connection open until it is answered.
    drop(alive);
    tokio::select! {
        biased;
        _ = peer.0.closed.cancelled() => {}
        _ = unanswered.recv() => {}
    }
    Ok(())
}
