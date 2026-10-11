//! The ACP v1 session loop: `initialize`, `session/new`, `session/prompt`,
//! `session/cancel` in, `session/update` and `session/request_permission` out.
//!
//! The transport's handler runs in its own tasks, so it never holds the session: it
//! answers `initialize` and `session/new` itself and hands prompts and cancels to the
//! loop below, which owns the [`SessionHandle`]. The loop polls the running prompt
//! while it forwards updates and permission requests, as `pump` does in
//! `crates/p1-host/src/tui.rs` (frozen donor).

use super::Watched;
use super::front_end::{AcpFrontEnd, Channels};
use super::io::{self, Handler, Peer, Responder, RpcError};
use crate::{
    capabilities,
    codec::Codec,
    commands::{self, Invocation},
    config_options::{self, Refusal},
    extensions::agent_state::AgentState,
    policy::{PermissionReply, PermissionRequest},
    session_title::SessionTitle,
    sink::{Outbound, Update},
    turn::prompt_outcome,
};
use p1_contracts::frontend::{CommandInfo, CommandOutput, ConfigChoice, ConfigKind, SessionHandle};
use p1_contracts::mcp::ClientServer;
use p1_contracts::{AgentEvent, BoxFuture, CancellationToken, StopReason, TurnEnd, UserBlock};
use serde_json::{Value, json};
use std::collections::{HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;

const INVALID_PARAMS: i64 = -32602;
/// JSON-RPC's server-error range: a request that is well formed but not allowed now.
const NOT_ALLOWED: i64 = -32000;
const INTERNAL: i64 = -32603;

/// Where a request's answer goes: straight onto the transport's writer queue (#733).
type Reply = Responder;

enum Command {
    /// `session/new`, `session/load`, `session/resume` or `session/fork` was accepted:
    /// the loop answers it with the session's settings and publishes its commands.
    Open {
        codec: Codec,
        id: String,
        replay: bool,
        /// Load or resume: the client named the session, so it knows the id before
        /// the answer.
        named: bool,
        reply: Reply,
    },
    Prompt {
        prompt: Prompt,
        reply: Reply,
    },
    Steer {
        text: String,
        prompt_required: bool,
        reply: Reply,
    },
    Cancel,
    CancelRun {
        id: String,
        reply: Reply,
    },
    StopWorker {
        id: String,
        reply: Reply,
    },
    SetConfig {
        option: String,
        value: String,
        reply: Reply,
        /// `session/set_mode`, the legacy door to the mode option: answered `{}`.
        legacy: bool,
    },
}

impl Command {
    /// The loop has gone: answer what waits for it.
    fn refuse(self) {
        match self {
            Command::Open { reply, .. }
            | Command::Prompt { reply, .. }
            | Command::CancelRun { reply, .. }
            | Command::StopWorker { reply, .. }
            | Command::Steer { reply, .. }
            | Command::SetConfig { reply, .. } => reply.answer(Err(ended())),
            Command::Cancel => {}
        }
    }
}

#[derive(Default)]
struct Protocol {
    codec: Option<Codec>,
    session: Option<String>,
    workflow_update: bool,
    run_cancel: bool,
    worker_stop: bool,
    worker_transcripts: bool,
    /// The client presents standard `notice` updates (ADR-0190).
    notices: bool,
    /// The loop answered the session request. The transport runs while the host
    /// assembles (#695), so the session has an id before its answer; nothing goes out
    /// under that id until the answer does.
    opened: bool,
}

impl Protocol {
    fn ready(&self) -> Option<(Codec, String)> {
        if !self.opened {
            return None;
        }
        Some((self.codec?, self.session.clone()?))
    }
}

/// The transport's view of the session: everything it can answer without the agent.
struct Inbound {
    worker_transcripts: Arc<std::sync::atomic::AtomicBool>,
    image_prompts: bool,
    workspace: PathBuf,
    protocol: Arc<Mutex<Protocol>>,
    commands: mpsc::UnboundedSender<Command>,
    questions: Arc<crate::questions::Questions>,
    agent_state: Arc<AgentState>,
    /// Where the first opened session's `mcpServers` go: the host awaits them before it
    /// assembles the agent (#695).
    opening: Mutex<Option<oneshot::Sender<Vec<ClientServer>>>>,
}

fn ended() -> RpcError {
    RpcError::new(NOT_ALLOWED, "the session has ended")
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
        let (codec, mut capabilities) =
            capabilities::initialize(u16::try_from(version).unwrap_or(u16::MAX), meta);
        capabilities.image_prompts = self.image_prompts;
        let mut protocol = self.protocol.lock().unwrap();
        protocol.codec = Some(codec);
        protocol.workflow_update = capabilities.workflow_update;
        protocol.run_cancel = capabilities.run_cancel;
        protocol.worker_stop = capabilities.worker_stop;
        protocol.worker_transcripts = capabilities.worker_transcripts;
        protocol.notices = codec.decode_notices(params);
        self.worker_transcripts.store(
            capabilities.worker_transcripts,
            std::sync::atomic::Ordering::Relaxed,
        );
        self.agent_state.enable(capabilities.agent_state);
        self.questions.initialize(params);
        Ok(codec.encode_capabilities(&capabilities))
    }

    /// One session per process. Its `mcpServers` become the session's MCP servers (#695):
    /// the host awaits them before it assembles the agent, and answers the request once the
    /// agent is built. A `session/load` or `session/resume` that names the open session
    /// re-attaches to it: the same agent and journal writer (ADR-0156, ADR-0158), never a
    /// second one, and the servers it opened with.
    fn new_session(&self, method: &str, params: &Value) -> Result<(Codec, String), RpcError> {
        let mut protocol = self.protocol.lock().unwrap();
        if protocol.codec.is_none() {
            return Err(RpcError::new(NOT_ALLOWED, "initialize first"));
        }
        let reattach = match &protocol.session {
            None => false,
            Some(ours)
                if matches!(method, "session/load" | "session/resume")
                    && params.get("sessionId").and_then(Value::as_str) == Some(ours.as_str()) =>
            {
                true
            }
            Some(_) => {
                return Err(RpcError::new(
                    NOT_ALLOWED,
                    "p1 acp serves one session per process",
                ));
            }
        };
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
        let codec = protocol.codec.expect("checked above");
        let servers = codec.decode_mcp_servers(params).map_err(invalid)?;
        if reattach {
            return Ok((codec, protocol.session.clone().expect("checked above")));
        }
        let id = format!("p1-{}", std::process::id());
        protocol.session = Some(id.clone());
        if let Some(opening) = self.opening.lock().unwrap().take() {
            let _ = opening.send(servers);
        }
        Ok((codec, id))
    }

    /// Hand `command` to the loop, in the order the client sent it.
    fn hand(&self, command: Command) {
        if let Err(mpsc::error::SendError(command)) = self.commands.send(command) {
            command.refuse();
        }
    }

    fn set_config_option(&self, params: &Value, reply: Reply) {
        if let Err(error) = self.check_session(params) {
            return reply.answer(Err(error));
        }
        let text = |key: &str| {
            params
                .get(key)
                .and_then(Value::as_str)
                .map(str::to_string)
                .ok_or_else(|| invalid(format!("session/set_config_option needs a string {key}")))
        };
        match (text("configId"), text("value")) {
            (Ok(option), Ok(value)) => self.hand(Command::SetConfig {
                option,
                value,
                reply,
                legacy: false,
            }),
            (Err(error), _) | (_, Err(error)) => reply.answer(Err(error)),
        }
    }

    /// `session/set_mode`: the mode option's own path, under its legacy name.
    fn set_mode(&self, params: &Value, reply: Reply) {
        if let Err(error) = self.check_session(params) {
            return reply.answer(Err(error));
        }
        match params.get("modeId").and_then(Value::as_str) {
            Some(mode) => self.hand(Command::SetConfig {
                option: config_options::id(ConfigKind::Mode).to_string(),
                value: mode.to_string(),
                reply,
                legacy: true,
            }),
            None => reply.answer(Err(invalid("session/set_mode needs a string modeId"))),
        }
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

    /// Text and resource links, the baseline every agent accepts; audio and embedded
    /// resources are refused, as `promptCapabilities` advertises. A link reaches the
    /// model as its name and URI, for its own tools to read. Images only where
    /// `images` (`session/prompt`, #694): the session stores them and refuses them when
    /// its model takes none. A prompt without an image is text, as before.
    fn prompt(params: &Value, images: bool) -> Result<Prompt, RpcError> {
        let blocks = params
            .get("prompt")
            .and_then(Value::as_array)
            .ok_or_else(|| invalid("session/prompt needs a prompt array"))?;
        let mut parts: Vec<Part> = Vec::new();
        let mut links = Vec::new();
        for block in blocks {
            match block.get("type").and_then(Value::as_str) {
                Some("text") => parts.push(Part::Text(
                    block
                        .get("text")
                        .and_then(Value::as_str)
                        .ok_or_else(|| invalid("a text block needs text"))?
                        .to_string(),
                )),
                Some("resource_link") => {
                    let uri = block
                        .get("uri")
                        .and_then(Value::as_str)
                        .ok_or_else(|| invalid("a resource_link block needs uri"))?;
                    let name = block.get("name").and_then(Value::as_str).unwrap_or(uri);
                    links.push(format!("[{name}]({uri})"));
                }
                Some("image") if images => parts.push(Part::Image(image_bytes(block)?)),
                other => {
                    return Err(invalid(format!(
                        "p1 acp accepts {} blocks only, not {}",
                        if images {
                            "text, image and resource_link"
                        } else {
                            "text and resource_link"
                        },
                        other.unwrap_or("an untyped block")
                    )));
                }
            }
        }
        let linked =
            (!links.is_empty()).then(|| format!("\n\nLinked resources:\n{}", links.join("\n")));
        if parts.iter().any(|part| matches!(part, Part::Image(_))) {
            if let Some(linked) = linked {
                parts.push(Part::Text(linked.trim_start().to_string()));
            }
            return Ok(Prompt::Content(parts));
        }
        let texts: Vec<String> = parts
            .into_iter()
            .filter_map(|part| match part {
                Part::Text(text) => Some(text),
                Part::Image(_) => None,
            })
            .collect();
        let mut text = texts.join("\n");
        text.push_str(linked.as_deref().unwrap_or_default());
        Ok(Prompt::Text(text))
    }
}

/// What a `session/prompt` asks for: text, or text and images in the client's order.
enum Prompt {
    Text(String),
    Content(Vec<Part>),
}

enum Part {
    Text(String),
    Image(Vec<u8>),
}

impl Prompt {
    /// The prompt's words: the text of a text prompt, the text blocks of a content
    /// prompt. For the title fallback and the command line.
    fn text(&self) -> String {
        match self {
            Prompt::Text(text) => text.clone(),
            Prompt::Content(parts) => parts
                .iter()
                .filter_map(|part| match part {
                    Part::Text(text) => Some(text.as_str()),
                    Part::Image(_) => None,
                })
                .collect::<Vec<_>>()
                .join("\n"),
        }
    }
}

/// An ACP image block's bytes. The store decides the format by content (ADR-0180); a
/// `mimeType` outside the formats it takes is refused here already.
fn image_bytes(block: &Value) -> Result<Vec<u8>, RpcError> {
    let mime = block
        .get("mimeType")
        .and_then(Value::as_str)
        .ok_or_else(|| invalid("an image block needs mimeType"))?;
    if !["image/png", "image/jpeg", "image/gif", "image/webp"].contains(&mime) {
        return Err(invalid(format!(
            "p1 acp takes PNG, JPEG, GIF and WebP images, not {mime}"
        )));
    }
    let data = block
        .get("data")
        .and_then(Value::as_str)
        .ok_or_else(|| invalid("an image block needs data"))?;
    p1_contracts::ImageData::from_base64(data)
        .decode()
        .ok_or_else(|| invalid("an image block's data is not base64"))
}

/// Everything happens in the call itself, in arrival order; nothing waits in the
/// transport's tasks. The loop answers what needs the session.
impl Handler for Inbound {
    fn call(
        self: Arc<Self>,
        _peer: Peer,
        method: String,
        params: Value,
        reply: Responder,
    ) -> BoxFuture<'static, ()> {
        match method.as_str() {
            "initialize" => reply.answer(self.initialize(&params)),
            "session/new" | "session/load" | "session/resume" | "session/fork" => {
                match self.new_session(&method, &params) {
                    Ok((codec, id)) => self.hand(Command::Open {
                        codec,
                        id,
                        replay: method == "session/load",
                        named: matches!(method.as_str(), "session/load" | "session/resume"),
                        reply,
                    }),
                    Err(error) => reply.answer(Err(error)),
                }
            }
            "session/set_config_option" => self.set_config_option(&params, reply),
            "session/set_mode" => self.set_mode(&params, reply),
            "_p1/run/cancel" | "_p1/worker/stop" => {
                let run = method == "_p1/run/cancel";
                let enabled = {
                    let protocol = self.protocol.lock().unwrap();
                    if run {
                        protocol.run_cancel
                    } else {
                        protocol.worker_stop
                    }
                };
                if !enabled {
                    reply.answer(Err(RpcError::method_not_found()));
                } else {
                    let key = if run { "runId" } else { "workerId" };
                    match self.check_session(&params).and_then(|()| {
                        params
                            .get(key)
                            .and_then(Value::as_str)
                            .map(str::to_string)
                            .ok_or_else(|| invalid(format!("{method} needs a string {key}")))
                    }) {
                        Ok(id) => self.hand(if run {
                            Command::CancelRun { id, reply }
                        } else {
                            Command::StopWorker { id, reply }
                        }),
                        Err(error) => reply.answer(Err(error)),
                    }
                }
            }
            "_session/steering" => {
                match self.check_session(&params).and_then(|()| {
                    let required = crate::steering::prompt_required(&params)?;
                    Ok((Self::prompt(&params, false)?.text(), required))
                }) {
                    Ok((text, prompt_required)) => self.hand(Command::Steer {
                        text,
                        prompt_required,
                        reply,
                    }),
                    Err(error) => reply.answer(Err(error)),
                }
            }
            "session/prompt" => {
                match self
                    .check_session(&params)
                    .and_then(|()| Self::prompt(&params, true))
                {
                    Ok(prompt) => self.hand(Command::Prompt { prompt, reply }),
                    Err(error) => reply.answer(Err(error)),
                }
            }
            _ => reply.answer(Err(RpcError::method_not_found())),
        }
        Box::pin(async {})
    }

    fn notified(
        self: Arc<Self>,
        _peer: Peer,
        method: String,
        params: Value,
    ) -> BoxFuture<'static, ()> {
        if method == "session/cancel" && self.check_session(&params).is_ok() {
            self.hand(Command::Cancel);
        }
        Box::pin(async {})
    }
}

/// The running prompt: its turn, the hold after it, and where its answer goes.
struct Active<'a> {
    token: CancellationToken,
    codec: Codec,
    reply: Option<Reply>,
    turn: BoxFuture<'a, TurnEnd>,
    /// A host command: it can change the command list (`/modules reload`).
    command: bool,
}

/// The transport, running before the session is served: it answers `initialize` and
/// takes the session request while the host assembles the agent.
pub(super) struct Connected {
    protocol: Arc<Mutex<Protocol>>,
    inbox: mpsc::UnboundedReceiver<Command>,
    peer: Peer,
    connection: JoinHandle<std::io::Result<()>>,
    gone: CancellationToken,
    updates: mpsc::UnboundedReceiver<crate::sink::Stamped>,
    permissions: mpsc::UnboundedReceiver<PermissionRequest>,
    /// The opened session's `mcpServers`, taken once.
    pub(super) opening: Option<oneshot::Receiver<Vec<ClientServer>>>,
}

/// Start the transport on the session's streams.
pub(super) fn connect(front: &AcpFrontEnd, channels: Channels) -> Connected {
    let Channels {
        reader,
        writer,
        updates,
        permissions,
    } = channels;
    let protocol = Arc::new(Mutex::new(Protocol::default()));
    let (commands, inbox) = mpsc::unbounded_channel();
    let (opened, opening) = oneshot::channel();
    let handler = Arc::new(Inbound {
        worker_transcripts: front.worker_transcripts.clone(),
        image_prompts: front.image_prompts,
        workspace: front.workspace.clone(),
        protocol: protocol.clone(),
        commands,
        questions: front.questions.clone(),
        agent_state: front.agent_state.clone(),
        opening: Mutex::new(Some(opened)),
    });
    let gone = CancellationToken::new();
    let reader = Watched::new(reader, gone.clone());
    let (peer, connection) = io::spawn(reader, writer, handler);
    Connected {
        protocol,
        inbox,
        peer,
        connection,
        gone,
        updates,
        permissions,
        opening: Some(opening),
    }
}

/// The host could not start the session: the session request that waits for it is
/// answered with `reason`, everything else the client sends is refused, and the
/// connection closes once its answers are written. `requested`: the host took a session
/// request's servers, so that request is in the inbox or about to be; otherwise none is
/// waited for.
pub(super) async fn refuse_start(connected: Connected, reason: &str, requested: bool) {
    let Connected {
        mut inbox,
        peer,
        connection,
        gone,
        ..
    } = connected;
    // Before the session request nothing reaches the loop, so the first command is it;
    // each arm says whether the wait is over.
    let mut waiting = requested;
    while waiting {
        waiting = !tokio::select! {
            biased;
            command = inbox.recv() => match command {
                Some(Command::Open { reply, .. }) => {
                    reply.answer(Err(RpcError::new(
                        INTERNAL,
                        format!("p1 could not start the session: {reason}"),
                    )));
                    true
                }
                Some(command) => {
                    command.refuse();
                    false
                }
                None => true,
            },
            () = gone.cancelled() => true,
        };
    }
    peer.close();
    drop(inbox);
    if let Ok(Err(error)) = connection.await {
        eprintln!("p1 acp: connection failed: {error}");
    }
}

pub(super) async fn serve(
    front: &AcpFrontEnd,
    session: &dyn SessionHandle,
    connected: Connected,
) -> i32 {
    let Connected {
        protocol,
        mut inbox,
        peer,
        connection,
        gone,
        mut updates,
        mut permissions,
        opening: _,
    } = connected;
    let mut client_gone = false;
    // Calls announced `pending` before their permission request; their start is then
    // an `in_progress` update, not a second `tool_call`.
    let mut announced = HashSet::new();
    let mut active: Option<Active<'_>> = None;
    let mut queued: VecDeque<(Prompt, Option<Reply>)> = VecDeque::new();
    // Setting changes asked for while a prompt ran; they apply before the next one.
    let mut pending: Vec<(ConfigKind, String)> = Vec::new();
    // The commands last published; a prompt naming one runs it (#676).
    let mut published: Vec<CommandInfo> = Vec::new();
    let mut title = SessionTitle::default();
    // Inbox turns between prompts start with the client's input: not after a load,
    // whose notices about the earlier process wait for the first prompt, and not
    // after a cancel, whose notices about the stopped work wait for the next one,
    // as they do under the hold.
    let mut drain_between = false;

    loop {
        if active.is_none() && !pending.is_empty() {
            apply_pending(&peer, &protocol, session, std::mem::take(&mut pending)).await;
            publish_commands(&peer, &protocol, session, &mut published).await;
        }
        if active.is_none()
            && let Some((prompt, reply)) = queued.pop_front()
        {
            // A prompt reaches the loop only after session/new, so the codec exists.
            let codec = protocol
                .lock()
                .unwrap()
                .codec
                .unwrap_or(Codec::negotiate(1));
            let token = CancellationToken::new();
            let words = prompt.text();
            let input = match prompt {
                Prompt::Text(text) => Input::Text(text),
                Prompt::Content(parts) => match store_images(session, parts).await {
                    Ok(blocks) => Input::Content(blocks),
                    Err(error) => {
                        if let Some(reply) = reply {
                            reply.answer(Err(error));
                        }
                        continue;
                    }
                },
            };
            title.prompt(&words);
            // An image prompt is a turn, never a command line.
            let parsed = match &input {
                Input::Text(text) => commands::parse(text, &published),
                Input::Content(_) => None,
            };
            let command = matches!(parsed, Some(Invocation::Host { .. }));
            front.agent_state.turn_started();
            let turn: BoxFuture<'_, TurnEnd> = match parsed {
                // Nothing runs, so the change applies now, exactly as an idle
                // `session/set_config_option` does.
                Some(Invocation::Setting { kind, argument }) => {
                    let report = setting_command(&peer, &protocol, session, kind, &argument).await;
                    message(&peer, &protocol, &report);
                    publish_commands(&peer, &protocol, session, &mut published).await;
                    let early = !front.agent_state.enabled();
                    if early {
                        publish_title(&peer, &protocol, session, &mut title).await;
                    }
                    answer(
                        front,
                        codec,
                        reply,
                        TurnEnd::Completed {
                            stop: StopReason::EndTurn,
                        },
                    );
                    if !early {
                        publish_title(&peer, &protocol, session, &mut title).await;
                    }
                    continue;
                }
                Some(Invocation::Host { name, argument }) => Box::pin(host_command(
                    front,
                    &peer,
                    &protocol,
                    session,
                    name,
                    argument,
                    token.clone(),
                )),
                None => Box::pin(prompt_turn(front, session, input, token.clone())),
            };
            active = Some(Active {
                token,
                codec,
                reply,
                turn,
                command,
            });
        }
        let running = active.is_some();
        // A client that opted out of the hold gets the parent's inbox turns between
        // prompts, as the line loop runs them (#683).
        let idle = !running
            && queued.is_empty()
            && !client_gone
            && drain_between
            && front.agent_state.enabled()
            && protocol.lock().unwrap().ready().is_some();
        tokio::select! {
            biased;
            Some(stamped) = updates.recv() => {
                forward(&peer, &protocol, &mut announced, stamped.item);
            }
            Some(request) = permissions.recv() => {
                ask(&peer, &protocol, &front.agent_state, &mut announced, request);
            }
            _ = gone.cancelled(), if !client_gone => {
                // The client went away: stop the running prompt and its work, refuse
                // the queued prompts, and deny every permission request from now on.
                client_gone = true;
                front.connection.cancel();
                if let Some(active) = &active {
                    active.token.cancel();
                }
                for (_, reply) in queued.drain(..) {
                    if let Some(reply) = reply {
                        reply.answer(Err(ended()));
                    }
                }
                pending.clear();
                front.hold.release();
                session.cancel_runs().await;
                session.stop_workers().await;
            }
            command = inbox.recv() => match command {
                Some(Command::Prompt { reply, .. }) if client_gone => {
                    reply.answer(Err(ended()));
                }
                Some(Command::Prompt { prompt, reply }) => {
                    // The next prompt releases a held one (D4).
                    front.hold.release();
                    drain_between = true;
                    queued.push_back((prompt, Some(reply)));
                }
                Some(Command::Steer { reply, .. }) if client_gone => {
                    reply.answer(Err(ended()));
                }
                Some(Command::Steer { text, prompt_required, reply }) => {
                    drain_between |= !prompt_required;
                    if active.as_ref().is_some_and(|active| !active.command)
                        || front.hold.prompting()
                        || !queued.is_empty()
                    {
                        // No release: even a held prompt owns this input. The host
                        // sender never takes the running turn's agent lock.
                        reply.answer(session.steer(text)
                            .map(|()| json!({"outcome":"injected"}))
                            .map_err(|reason| RpcError::new(INTERNAL, reason)));
                    } else if prompt_required {
                        reply.answer(Ok(json!({"outcome":"promptRequired","reason":"noRunningTurn"})));
                    } else {
                        // Legacy idle fallback: stream a detached turn, not a
                        // second response to the already answered steer request.
                        queued.push_back((Prompt::Text(text), None));
                        reply.answer(Ok(json!({"outcome":"startedNewTurn"})));
                    }
                }
                Some(Command::Open { codec, id, replay, named, reply }) => {
                    protocol.lock().unwrap().opened = true;
                    // The settings travel with the id. A re-attach (load or resume of
                    // the open session) is told the list again, and its inbox turns
                    // wait for a prompt as after any load.
                    drain_between = false;
                    published.clear();
                    front.questions.opened(peer.clone(), id.clone());
                    front.agent_state.opened(peer.clone(), id.clone());
                    if replay {
                        let records = front.history.lock().unwrap().clone();
                        for update in crate::replay::updates(&records, session).await {
                            let _ = peer.notify("session/update", json!({
                                "sessionId": id, "update": codec.encode_update(&update),
                            }));
                        }
                    }
                    let choices = session.config().await;
                    let mut result = json!({ "sessionId": id });
                    if !choices.is_empty() {
                        result["configOptions"] = codec.encode_config_options(&choices);
                    }
                    if let Some(mode) = choices.iter().find(|choice| choice.kind == ConfigKind::Mode) {
                        result["modes"] = codec.encode_modes(mode);
                    }
                    // The command list goes on the same writer queue. A load or resume
                    // names a session the client knows, so its list precedes the
                    // answer, as the replay does: no update follows a `session/load`
                    // answer (ACP-LOAD-002). A new session's id reaches the client
                    // with the answer, so its list follows it. A client that opted out
                    // of the hold (#683) keeps the order it was given: the answer, then
                    // the list.
                    let early = named && !front.agent_state.enabled();
                    if early {
                        publish_commands(&peer, &protocol, session, &mut published).await;
                    }
                    reply.answer(Ok(result));
                    if !early {
                        publish_commands(&peer, &protocol, session, &mut published).await;
                    }
                }
                Some(Command::SetConfig { option, value, reply, legacy }) => {
                    let idle = active.is_none() && queued.is_empty();
                    match set_config(&protocol, session, &mut pending, idle, &option, &value).await {
                        Ok(set) => {
                            // The answer first, then what announces the change.
                            reply.answer(Ok(if legacy { json!({}) } else { set.answer }));
                            if let Some((choices, kind)) = set.applied {
                                announce_config(&peer, &protocol, &choices, kind);
                                publish_commands(&peer, &protocol, session, &mut published).await;
                            }
                        }
                        Err(error) => reply.answer(Err(error)),
                    }
                }
                Some(Command::CancelRun { id, reply }) => {
                    reply.answer(Ok(json!(crate::extensions::cancel::RunCancelResult {
                        cancelled: session.cancel_run(&id).await,
                    })));
                }
                Some(Command::StopWorker { id, reply }) => {
                    reply.answer(Ok(json!(crate::extensions::cancel::WorkerStopResult {
                        stopped: session.stop_worker(&id).await,
                    })));
                }
                Some(Command::Cancel) => {
                    drain_between = false;
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
                // Every update of the turn goes out before its answer; `try_recv` is
                // not held back by the runtime's poll budget.
                while let Ok(stamped) = updates.try_recv() {
                    forward(&peer, &protocol, &mut announced, stamped.item);
                }
                // The turn's metadata too (ACP-CANCEL-002): the title, and the list a
                // reload changed; the answer follows, so nothing of the turn comes after
                // it and the next prompt's lines do. A client that opted out of the
                // hold (#683) takes updates after the answer by design and keeps the
                // order it was given: the answer, then the metadata.
                let early = !front.agent_state.enabled();
                if early {
                    turn_metadata(
                        &peer, &protocol, session, &mut title, &mut published, done.command,
                    )
                    .await;
                }
                answer(front, done.codec, done.reply, end);
                if !early {
                    turn_metadata(
                        &peer, &protocol, session, &mut title, &mut published, done.command,
                    )
                    .await;
                }
            }
            // Last: a request that arrived meanwhile goes first. The future holds the
            // session while it waits and is dropped before any arm above runs.
            () = session.inbox_ready(), if idle => {
                let token = CancellationToken::new();
                front.agent_state.turn_started();
                active = Some(Active {
                    turn: Box::pin(inbox_turn(front, session, token.clone())),
                    token,
                    codec: protocol.lock().unwrap().codec.unwrap_or(Codec::negotiate(1)),
                    reply: None,
                    command: false,
                });
            }
        }
    }

    // Every request was answered (the transport waits for that before it ends), so no
    // prompt is running; close the connection.
    front.connection.cancel();
    if let Some(done) = active {
        done.token.cancel();
        let end = done.turn.await;
        answer(front, done.codec, done.reply, end);
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

/// What a `session/set_config_option` did: its answer, and the settings to announce
/// when the change applied now.
struct Set {
    answer: Value,
    applied: Option<(Vec<ConfigChoice>, ConfigKind)>,
}

/// A `session/set_config_option`. Idle, the change applies now; while a prompt runs or
/// waits it applies before the next one, and the answer already names it. The mode
/// applies now in either case: it is read at the next tool call and needs nothing of
/// the running turn. Either way the answer is the complete option list.
async fn set_config(
    protocol: &Mutex<Protocol>,
    session: &dyn SessionHandle,
    pending: &mut Vec<(ConfigKind, String)>,
    idle: bool,
    option: &str,
    value: &str,
) -> Result<Set, RpcError> {
    let Some((codec, _)) = protocol.lock().unwrap().ready() else {
        return Err(RpcError::new(NOT_ALLOWED, "session/new first"));
    };
    let choices = session.config().await;
    // Checked against what the session will run once the changes already waiting for
    // the running prompt apply, so an answer never contradicts an earlier one.
    let view = config_options::waiting(choices.clone(), pending);
    let kind = config_options::validate(&view, option, value).map_err(|refusal| match refusal {
        Refusal::UnknownOption(_) if choices.len() > view.len() => invalid(format!(
            "`{option}` can be set once the model switch waiting for this prompt has run"
        )),
        refusal => invalid(refusal.to_string()),
    })?;
    // `idle` is what keeps the agent lock a switch takes free: during a turn the turn
    // holds it and is not polled while this arm waits. The mode takes no lock.
    let (choices, applied) = if idle || kind == ConfigKind::Mode {
        session
            .set_config(kind, value)
            .await
            .map_err(|reason| RpcError::new(INTERNAL, reason))?;
        let now = config_options::waiting(session.config().await, pending);
        (now.clone(), Some((now, kind)))
    } else {
        config_options::queue(pending, kind, value);
        (config_options::waiting(choices, pending), None)
    };
    Ok(Set {
        answer: json!({ "configOptions": codec.encode_config_options(&choices) }),
        applied,
    })
}

/// Apply the changes a prompt held back, in the order they were asked for. A change
/// that fails now is reported on stderr; the update then shows what the session runs.
async fn apply_pending(
    peer: &Peer,
    protocol: &Mutex<Protocol>,
    session: &dyn SessionHandle,
    pending: Vec<(ConfigKind, String)>,
) {
    // The mode never waits, so what waits is the model and its effort.
    let mut last = ConfigKind::Model;
    for (kind, value) in pending {
        if let Err(reason) = session.set_config(kind, &value).await {
            eprintln!("p1 acp: {} stays: {reason}", config_options::id(kind));
        }
        last = kind;
    }
    announce_config(peer, protocol, &session.config().await, last);
}

/// What a finished turn may have changed: the title, and the command list after a host
/// command (`/modules reload`).
async fn turn_metadata(
    peer: &Peer,
    protocol: &Mutex<Protocol>,
    session: &dyn SessionHandle,
    title: &mut SessionTitle,
    published: &mut Vec<CommandInfo>,
    command: bool,
) {
    publish_title(peer, protocol, session, title).await;
    if command {
        publish_commands(peer, protocol, session, published).await;
    }
}

/// The title, when it changed, on the writer queue: before the prompt's answer, since
/// no update of a turn follows its answer (ACP-CANCEL-002), unless the client opted
/// out of the hold (#683).
async fn publish_title(
    peer: &Peer,
    protocol: &Mutex<Protocol>,
    session: &dyn SessionHandle,
    title: &mut SessionTitle,
) {
    if let Some(title) = title.changed(session.title().await)
        && let Some((codec, id)) = protocol.lock().unwrap().ready()
    {
        let _ = peer.notify(
            "session/update",
            json!({ "sessionId": id, "update": codec.encode_session_title(&title) }),
        );
    }
}

/// Publish the session's commands when they differ from `published`: once as the
/// session opens (after `session/new`'s answer, before a load's or resume's), then
/// after a switch that changed them (another environment can
/// bring other skills).
async fn publish_commands(
    peer: &Peer,
    protocol: &Mutex<Protocol>,
    session: &dyn SessionHandle,
    published: &mut Vec<CommandInfo>,
) {
    let Some((codec, id)) = protocol.lock().unwrap().ready() else {
        return;
    };
    let now = commands::list(&session.config().await, session.commands().await);
    // A session without commands publishes none.
    if now == *published {
        return;
    }
    let _ = peer.notify(
        "session/update",
        json!({ "sessionId": id, "update": codec.encode_commands_update(&now) }),
    );
    *published = now;
}

/// `/model` and `/effort`: without an argument the setting's values, with one the
/// same change `session/set_config_option` makes. The report is for the user.
async fn setting_command(
    peer: &Peer,
    protocol: &Mutex<Protocol>,
    session: &dyn SessionHandle,
    kind: ConfigKind,
    argument: &str,
) -> String {
    let option = config_options::id(kind);
    if argument.is_empty() {
        return match session
            .config()
            .await
            .iter()
            .find(|choice| choice.kind == kind)
        {
            Some(choice) => commands::describe(choice),
            None => format!("this session has no {} to list\n", commands::name(kind)),
        };
    }
    // The loop applied every waiting change before it took this prompt.
    match set_config(protocol, session, &mut Vec::new(), true, option, argument).await {
        Ok(set) => {
            if let Some((choices, changed)) = set.applied {
                announce_config(peer, protocol, &choices, changed);
            }
            format!("{}: {argument}\n", commands::name(kind))
        }
        Err(error) => format!("{} not changed: {}\n", commands::name(kind), error.message),
    }
}

/// One of the host's commands, run as the prompt's turn: its report is the answer's
/// text, or its text is a turn for the model (a skill).
async fn host_command<'a>(
    front: &'a AcpFrontEnd,
    peer: &'a Peer,
    protocol: &'a Mutex<Protocol>,
    session: &'a dyn SessionHandle,
    name: String,
    argument: String,
    token: CancellationToken,
) -> TurnEnd {
    let outcome = session.command(&name, &argument, token.clone()).await;
    if token.is_cancelled() {
        return TurnEnd::Cancelled;
    }
    match outcome {
        Ok(CommandOutput::Prompt(text)) => {
            return prompt_turn(front, session, Input::Text(text), token).await;
        }
        Ok(CommandOutput::Text(text)) => message(peer, protocol, &text),
        Err(reason) => message(peer, protocol, &format!("/{name}: {reason}\n")),
    }
    TurnEnd::Completed {
        stop: StopReason::EndTurn,
    }
}

/// Text for the user, as the agent's message.
fn message(peer: &Peer, protocol: &Mutex<Protocol>, text: &str) {
    if let Some((codec, session)) = protocol.lock().unwrap().ready() {
        notify_update(peer, codec, &session, &Update::Message(text.to_string()));
    }
}

/// The settings after a change of `changed`: the option list, and for the mode also
/// the legacy `current_mode_update`.
fn announce_config(
    peer: &Peer,
    protocol: &Mutex<Protocol>,
    choices: &[ConfigChoice],
    changed: ConfigKind,
) {
    let Some((codec, session)) = protocol.lock().unwrap().ready() else {
        return;
    };
    let _ = peer.notify(
        "session/update",
        json!({ "sessionId": session, "update": codec.encode_config_update(choices) }),
    );
    if changed == ConfigKind::Mode
        && let Some(mode) = choices
            .iter()
            .find(|choice| choice.kind == ConfigKind::Mode)
    {
        let _ = peer.notify(
            "session/update",
            json!({ "sessionId": session, "update": codec.encode_mode_update(&mode.current) }),
        );
    }
}

/// The turn's end: its idle state first, then the prompt's answer.
fn answer(front: &AcpFrontEnd, codec: Codec, reply: Option<Reply>, end: TurnEnd) {
    let outcome = match prompt_outcome(end) {
        Ok(stop) => Ok(codec.encode_stop(stop)),
        Err(error) => Err(serde_json::from_value(codec.encode_error(&error))
            .unwrap_or_else(|_| RpcError::new(-32603, error.message))),
    };
    front.agent_state.turn_ended(match &outcome {
        Ok(stop) => stop["stopReason"].as_str().unwrap_or("end_turn"),
        Err(_) => "error",
    });
    if let Some(reply) = reply {
        reply.answer(outcome);
    } else if let Err(error) = outcome {
        eprintln!("p1 acp: turn without a prompt failed: {}", error.message);
    }
}

/// What a prompt's turn runs on: text, or content with stored images.
enum Input {
    Text(String),
    Content(Vec<UserBlock>),
}

/// A content prompt's images go into the session's store, in order, before its turn;
/// the first refusal (a model without image input, an invalid image) refuses the prompt
/// with invalid params, never dropping the image (#694). Images stored before a refusal
/// stay in the content-addressed store (ADR-0180) until `p1 sessions prune` collects them
/// as referenced by no remaining session (ADR-0185).
async fn store_images(
    session: &dyn SessionHandle,
    parts: Vec<Part>,
) -> Result<Vec<UserBlock>, RpcError> {
    let mut blocks = Vec::with_capacity(parts.len());
    for part in parts {
        blocks.push(match part {
            Part::Text(text) => UserBlock::text(text),
            Part::Image(bytes) => {
                UserBlock::image(session.save_image(bytes).await.map_err(invalid)?)
            }
        });
    }
    Ok(blocks)
}

/// One prompt: its turn, then the hold (D4) with the inbox turns it waits for, as the
/// line loop drains the inbox after a turn (`run_interactive`, `crates/p1-host/src/run.rs`).
async fn prompt_turn(
    front: &AcpFrontEnd,
    session: &dyn SessionHandle,
    input: Input,
    token: CancellationToken,
) -> TurnEnd {
    let hold = &front.hold;
    hold.begin_prompt();
    front.policy.set_turn(Some(token.clone()));
    hold.turn(true);
    let mut end = match input {
        Input::Text(text) => session.prompt(text, token.clone()).await,
        Input::Content(blocks) => session.prompt_content(blocks, token.clone()).await,
    };
    hold.turn(false);
    // A client that opted out (#683) gets its answer now; the loop runs the inbox
    // turns between prompts.
    let held = !front.agent_state.enabled();
    while held && !matches!(end, TurnEnd::Cancelled) && !token.is_cancelled() {
        hold.turn(true);
        let drained = session.drain_inbox(token.clone()).await;
        hold.turn(false);
        if let Some(inbox) = drained {
            hold.settle();
            end = inbox;
            continue;
        }
        if hold.released() || !hold.holding() {
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

/// The parent's inbox turns between prompts, for a client that opted out of the hold.
async fn inbox_turn(
    front: &AcpFrontEnd,
    session: &dyn SessionHandle,
    token: CancellationToken,
) -> TurnEnd {
    front.policy.set_turn(Some(token.clone()));
    let end = session.drain_inbox(token.clone()).await;
    front.policy.set_turn(None);
    match end {
        _ if token.is_cancelled() => TurnEnd::Cancelled,
        Some(end) => end,
        None => TurnEnd::Completed {
            stop: StopReason::EndTurn,
        },
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
                // Already announced with its permission request.
                Update::ToolPending(tool) if !announced.insert(tool.id.clone()) => return,
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
        Outbound::WorkerTranscript { worker, update } => {
            if protocol.lock().unwrap().worker_transcripts {
                let mut update = codec.encode_update(&update);
                update["_meta"] = json!({"p1.dev": {"worker": worker}});
                let _ = peer.notify(
                    "session/update",
                    json!({"sessionId":session,"update":update}),
                );
            }
        }
        Outbound::Worker(update) => {
            if protocol.lock().unwrap().worker_transcripts {
                let _ = peer.notify(
                    crate::extensions::workers::METHOD,
                    json!({
                        "sessionId":session,"worker":update.worker,"state":update.state,
                        "description":update.description,"note":update.note,
                    }),
                );
            }
        }
        Outbound::Workflow(event) => {
            if protocol.lock().unwrap().workflow_update {
                let _ = peer.notify(
                    crate::extensions::workflow::METHOD,
                    json!({"sessionId": session, "event": event}),
                );
            }
        }
        // The prompt's answer comes from its turn; an inbox turn's end is not one.
        Outbound::Turn(_) => {}
        // The operator log always; the client too when it presents notices (ADR-0190), never
        // as assistant content (ADR-0048).
        Outbound::Operator(AgentEvent::ProviderNotice { text }) => {
            eprintln!("p1 acp: {text}");
            if protocol.lock().unwrap().notices {
                let _ = peer.notify(
                    "session/update",
                    json!({ "sessionId": session, "update": codec.encode_notice(&text) }),
                );
            }
        }
        Outbound::Operator(_) => {}
    }
}

/// The `tool_call` always goes out before its permission request.
fn ask(
    peer: &Peer,
    protocol: &Mutex<Protocol>,
    agent_state: &Arc<AgentState>,
    announced: &mut HashSet<String>,
    request: PermissionRequest,
) {
    let Some((codec, session)) = protocol.lock().unwrap().ready() else {
        request.answer(PermissionReply::Cancelled);
        return;
    };
    // Its turn was cancelled before the loop reached it: nothing to put to the client.
    if request.is_closed() {
        return;
    }
    let prompt = request.prompt();
    if announced.insert(prompt.tool.id.clone()) {
        notify_update(
            peer,
            codec,
            &session,
            &Update::ToolPending(prompt.tool.clone()),
        );
    }
    // Released by the asking turn itself, when the client answers or the turn is
    // cancelled: the state is current before the turn goes on or ends.
    request.keep_while_waiting(agent_state.waiting());
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
