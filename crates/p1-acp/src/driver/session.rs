//! The ACP v1 session loop: `initialize`, `session/new`, `session/prompt`,
//! `session/cancel` in, `session/update` and `session/request_permission` out.
//!
//! The transport's handler runs in its own tasks, so it never holds the session: it
//! answers `initialize` and `session/new` itself and hands prompts and cancels to the
//! loop below, which owns the [`SessionHandle`]. The loop polls the running prompt
//! while it forwards updates and permission requests, as `pump` does in
//! `crates/p1-host/src/tui.rs` (frozen donor).

use super::front_end::{AcpFrontEnd, Channels};
use super::io::{self, Handler, Peer, RpcError};
use crate::{
    capabilities,
    codec::Codec,
    policy::{PermissionReply, PermissionRequest},
    sink::{Outbound, Update},
    turn::prompt_outcome,
};
use p1_contracts::frontend::SessionHandle;
use p1_contracts::{AgentEvent, BoxFuture, CancellationToken, TurnEnd};
use serde_json::{Value, json};
use std::collections::{HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use tokio::sync::{mpsc, oneshot};

const INVALID_PARAMS: i64 = -32602;
/// JSON-RPC's server-error range: a request that is well formed but not allowed now.
const NOT_ALLOWED: i64 = -32000;

type Reply = oneshot::Sender<Result<Value, RpcError>>;

enum Command {
    Prompt { text: String, reply: Reply },
    Cancel,
}

#[derive(Default)]
struct Protocol {
    codec: Option<Codec>,
    session: Option<String>,
}

impl Protocol {
    fn ready(&self) -> Option<(Codec, String)> {
        Some((self.codec?, self.session.clone()?))
    }
}

/// The transport's view of the session: everything it can answer without the agent.
struct Inbound {
    workspace: PathBuf,
    protocol: Arc<Mutex<Protocol>>,
    commands: mpsc::UnboundedSender<Command>,
}

fn invalid(message: impl Into<String>) -> RpcError {
    RpcError::new(INVALID_PARAMS, message)
}

/// Two spellings of one directory are the same workspace.
fn same_directory(a: &Path, b: &Path) -> bool {
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => a == b,
    }
}

impl Inbound {
    fn initialize(&self, params: &Value) -> Result<Value, RpcError> {
        let version = params
            .get("protocolVersion")
            .and_then(Value::as_u64)
            .ok_or_else(|| invalid("initialize needs a numeric protocolVersion"))?;
        let meta = params
            .pointer("/clientCapabilities/_meta")
            .and_then(Value::as_object);
        let (codec, capabilities) =
            capabilities::initialize(u16::try_from(version).unwrap_or(u16::MAX), meta);
        self.protocol.lock().unwrap().codec = Some(codec);
        Ok(codec.encode_capabilities(&capabilities))
    }

    /// One session per process; `mcpServers` is accepted and ignored (no client MCP
    /// servers in this slice, #695).
    fn new_session(&self, params: &Value) -> Result<Value, RpcError> {
        let mut protocol = self.protocol.lock().unwrap();
        if protocol.codec.is_none() {
            return Err(RpcError::new(NOT_ALLOWED, "initialize first"));
        }
        if protocol.session.is_some() {
            return Err(RpcError::new(
                NOT_ALLOWED,
                "p1 acp serves one session per process",
            ));
        }
        let cwd = params
            .get("cwd")
            .and_then(Value::as_str)
            .ok_or_else(|| invalid("session/new needs cwd"))?;
        if !same_directory(Path::new(cwd), &self.workspace) {
            return Err(invalid(format!(
                "cwd {cwd} is not the workspace {} this p1 acp serves (pass --workspace)",
                self.workspace.display()
            )));
        }
        let id = format!("p1-{}", std::process::id());
        protocol.session = Some(id.clone());
        Ok(json!({ "sessionId": id }))
    }

    fn check_session(&self, params: &Value) -> Result<(), RpcError> {
        let protocol = self.protocol.lock().unwrap();
        match (
            &protocol.session,
            params.get("sessionId").and_then(Value::as_str),
        ) {
            (Some(ours), Some(theirs)) if ours == theirs => Ok(()),
            _ => Err(invalid("unknown sessionId")),
        }
    }

    /// Text and resource links, the baseline every agent accepts; image, audio and
    /// embedded resources are refused, as `promptCapabilities` advertises. A link
    /// reaches the model as its name and URI, for its own tools to read.
    fn prompt_text(params: &Value) -> Result<String, RpcError> {
        let blocks = params
            .get("prompt")
            .and_then(Value::as_array)
            .ok_or_else(|| invalid("session/prompt needs a prompt array"))?;
        let mut texts: Vec<String> = Vec::new();
        let mut links = Vec::new();
        for block in blocks {
            match block.get("type").and_then(Value::as_str) {
                Some("text") => texts.push(
                    block
                        .get("text")
                        .and_then(Value::as_str)
                        .ok_or_else(|| invalid("a text block needs text"))?
                        .to_string(),
                ),
                Some("resource_link") => {
                    let uri = block
                        .get("uri")
                        .and_then(Value::as_str)
                        .ok_or_else(|| invalid("a resource_link block needs uri"))?;
                    let name = block.get("name").and_then(Value::as_str).unwrap_or(uri);
                    links.push(format!("[{name}]({uri})"));
                }
                other => {
                    return Err(invalid(format!(
                        "p1 acp accepts text and resource_link blocks only, not {}",
                        other.unwrap_or("an untyped block")
                    )));
                }
            }
        }
        let mut text = texts.join("\n");
        if !links.is_empty() {
            text.push_str("\n\nLinked resources:\n");
            text.push_str(&links.join("\n"));
        }
        Ok(text)
    }
}

impl Handler for Inbound {
    fn request(
        &self,
        _peer: Peer,
        method: String,
        params: Value,
    ) -> BoxFuture<'_, Result<Value, RpcError>> {
        Box::pin(async move {
            match method.as_str() {
                "initialize" => self.initialize(&params),
                "session/new" => self.new_session(&params),
                "session/prompt" => {
                    self.check_session(&params)?;
                    let text = Self::prompt_text(&params)?;
                    let (reply, answer) = oneshot::channel();
                    self.commands
                        .send(Command::Prompt { text, reply })
                        .map_err(|_| RpcError::new(NOT_ALLOWED, "the session has ended"))?;
                    answer.await.unwrap_or_else(|_| {
                        Err(RpcError::new(NOT_ALLOWED, "the session has ended"))
                    })
                }
                _ => Err(RpcError::method_not_found()),
            }
        })
    }

    fn notification(&self, _peer: Peer, method: String, params: Value) -> BoxFuture<'_, ()> {
        Box::pin(async move {
            if method == "session/cancel" && self.check_session(&params).is_ok() {
                let _ = self.commands.send(Command::Cancel);
            }
        })
    }
}

/// The running prompt: its turn, the hold after it, and where its answer goes.
struct Active<'a> {
    token: CancellationToken,
    codec: Codec,
    reply: Reply,
    turn: BoxFuture<'a, TurnEnd>,
}

pub(super) async fn serve(
    front: &AcpFrontEnd,
    session: &dyn SessionHandle,
    channels: Channels,
) -> i32 {
    let Channels {
        reader,
        writer,
        mut updates,
        mut permissions,
    } = channels;
    let protocol = Arc::new(Mutex::new(Protocol::default()));
    let (commands, mut inbox) = mpsc::unbounded_channel();
    let handler = Arc::new(Inbound {
        workspace: front.workspace.clone(),
        protocol: protocol.clone(),
        commands,
    });
    let (peer, connection) = io::spawn(reader, writer, handler);
    // Calls announced `pending` before their permission request; their start is then
    // an `in_progress` update, not a second `tool_call`.
    let mut announced = HashSet::new();
    let mut active: Option<Active<'_>> = None;
    let mut queued: VecDeque<(String, Reply)> = VecDeque::new();

    loop {
        if active.is_none()
            && let Some((text, reply)) = queued.pop_front()
        {
            // A prompt reaches the loop only after session/new, so the codec exists.
            let codec = protocol
                .lock()
                .unwrap()
                .codec
                .unwrap_or(Codec::negotiate(1));
            let token = CancellationToken::new();
            active = Some(Active {
                token: token.clone(),
                codec,
                reply,
                turn: Box::pin(prompt_turn(front, session, text, token)),
            });
        }
        let running = active.is_some();
        tokio::select! {
            biased;
            Some(stamped) = updates.recv() => {
                forward(&peer, &protocol, &mut announced, stamped.item);
            }
            Some(request) = permissions.recv() => {
                ask(&peer, &protocol, &mut announced, request);
            }
            command = inbox.recv() => match command {
                Some(Command::Prompt { text, reply }) => {
                    // The next prompt releases a held one (D4).
                    front.hold.release();
                    queued.push_back((text, reply));
                }
                Some(Command::Cancel) => {
                    if let Some(active) = &active {
                        active.token.cancel();
                    }
                    front.hold.release();
                    session.cancel_runs().await;
                    session.stop_workers().await;
                }
                None => break,
            },
            end = async { active.as_mut().expect("guarded by running").turn.as_mut().await }, if running => {
                let done = active.take().expect("guarded by running");
                answer(done.codec, done.reply, end);
            }
        }
    }

    // The client went away: end the running prompt cleanly, then the connection.
    front.connection.cancel();
    if let Some(done) = active {
        done.token.cancel();
        let end = done.turn.await;
        answer(done.codec, done.reply, end);
    }
    peer.close();
    match connection.await {
        Ok(Ok(())) => 0,
        Ok(Err(error)) => {
            eprintln!("p1 acp: connection failed: {error}");
            1
        }
        Err(error) => {
            eprintln!("p1 acp: connection task failed: {error}");
            1
        }
    }
}

fn answer(codec: Codec, reply: Reply, end: TurnEnd) {
    let outcome = match prompt_outcome(end) {
        Ok(stop) => Ok(codec.encode_stop(stop)),
        Err(error) => Err(serde_json::from_value(codec.encode_error(&error))
            .unwrap_or_else(|_| RpcError::new(-32603, error.message))),
    };
    let _ = reply.send(outcome);
}

/// One prompt: its turn, then the hold (D4) with the inbox turns it waits for, as the
/// line loop drains the inbox after a turn (`run_interactive`, `crates/p1-host/src/run.rs`).
async fn prompt_turn(
    front: &AcpFrontEnd,
    session: &dyn SessionHandle,
    text: String,
    token: CancellationToken,
) -> TurnEnd {
    front.hold.begin_prompt();
    front.policy.set_turn(Some(token.clone()));
    let mut end = session.prompt(text, token.clone()).await;
    while !matches!(end, TurnEnd::Cancelled) && !token.is_cancelled() {
        let ended = front.hold.take_ended();
        if let Some(inbox) = session.drain_inbox(token.clone()).await {
            end = inbox;
            continue;
        }
        if front.hold.released() {
            break;
        }
        if !front.hold.holding() {
            // The last held work just ended: its notice is queued right behind its end
            // signal. Let that producer finish before the last drain.
            if ended {
                tokio::task::yield_now().await;
                if let Some(inbox) = session.drain_inbox(token.clone()).await {
                    end = inbox;
                }
            }
            break;
        }
        tokio::select! {
            _ = session.inbox_ready() => {}
            _ = front.hold.changed() => {}
            _ = token.cancelled() => {}
        }
    }
    front.policy.set_turn(None);
    front.hold.end_prompt();
    if token.is_cancelled() {
        TurnEnd::Cancelled
    } else {
        end
    }
}

fn notify_update(peer: &Peer, codec: Codec, session: &str, update: &Update) {
    let _ = peer.notify(
        "session/update",
        json!({ "sessionId": session, "update": codec.encode_update(update) }),
    );
}

fn forward(
    peer: &Peer,
    protocol: &Mutex<Protocol>,
    announced: &mut HashSet<String>,
    item: Outbound,
) {
    let Some((codec, session)) = protocol.lock().unwrap().ready() else {
        return;
    };
    match item {
        Outbound::Update(update) => {
            let update = match *update {
                Update::ToolStarted(tool) if announced.remove(&tool.id) => {
                    Update::ToolRunning { id: tool.id }
                }
                Update::ToolFinished {
                    id,
                    succeeded,
                    text,
                } => {
                    announced.remove(&id);
                    Update::ToolFinished {
                        id,
                        succeeded,
                        text,
                    }
                }
                update => update,
            };
            notify_update(peer, codec, &session, &update);
        }
        // The prompt's answer comes from its turn; an inbox turn's end is not one.
        Outbound::Turn(_) => {}
        Outbound::Operator(AgentEvent::ProviderNotice { text }) => eprintln!("p1 acp: {text}"),
        Outbound::Operator(_) => {}
    }
}

/// The `tool_call` always goes out before its permission request.
fn ask(
    peer: &Peer,
    protocol: &Mutex<Protocol>,
    announced: &mut HashSet<String>,
    request: PermissionRequest,
) {
    let Some((codec, session)) = protocol.lock().unwrap().ready() else {
        request.answer(PermissionReply::Cancelled);
        return;
    };
    let prompt = request.prompt();
    if announced.insert(prompt.tool.id.clone()) {
        notify_update(
            peer,
            codec,
            &session,
            &Update::ToolPending(prompt.tool.clone()),
        );
    }
    let asked = peer.request(
        "session/request_permission",
        codec.encode_permission(&session, &prompt),
    );
    tokio::spawn(async move {
        let reply = match asked.await {
            Ok(value) => codec
                .decode_permission(value)
                .unwrap_or(PermissionReply::Reject),
            Err(_) => PermissionReply::Cancelled,
        };
        request.answer(reply);
    });
}
