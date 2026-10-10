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
    policy::{PermissionReply, PermissionRequest},
    session_title::SessionTitle,
    sink::{Outbound, Update},
    turn::prompt_outcome,
};
use p1_contracts::frontend::{CommandInfo, CommandOutput, ConfigChoice, ConfigKind, SessionHandle};
use p1_contracts::{AgentEvent, BoxFuture, CancellationToken, StopReason, TurnEnd};
use serde_json::{Value, json};
use std::collections::{HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;

const INVALID_PARAMS: i64 = -32602;
/// JSON-RPC's server-error range: a request that is well formed but not allowed now.
const NOT_ALLOWED: i64 = -32000;
const INTERNAL: i64 = -32603;

/// Where a request's answer goes: straight onto the transport's writer queue (#733).
type Reply = Responder;

enum Command {
    /// `session/new` was accepted: the loop answers it with the session's settings,
    /// then publishes its commands, in that order.
    Open {
        codec: Codec,
        id: String,
        reply: Reply,
    },
    Prompt {
        text: String,
        reply: Reply,
    },
    Cancel,
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
            | Command::SetConfig { reply, .. } => reply.answer(Err(ended())),
            Command::Cancel => {}
        }
    }
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
        let (codec, capabilities) =
            capabilities::initialize(u16::try_from(version).unwrap_or(u16::MAX), meta);
        self.protocol.lock().unwrap().codec = Some(codec);
        Ok(codec.encode_capabilities(&capabilities))
    }

    /// One session per process; `mcpServers` is accepted and ignored (no client MCP
    /// servers in this slice, #695).
    fn new_session(&self, params: &Value) -> Result<(Codec, String), RpcError> {
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
        Ok((protocol.codec.expect("checked above"), id))
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
            "session/new" => match self.new_session(&params) {
                Ok((codec, id)) => self.hand(Command::Open { codec, id, reply }),
                Err(error) => reply.answer(Err(error)),
            },
            "session/set_config_option" => self.set_config_option(&params, reply),
            "session/set_mode" => self.set_mode(&params, reply),
            "session/prompt" => {
                match self
                    .check_session(&params)
                    .and_then(|()| Self::prompt_text(&params))
                {
                    Ok(text) => self.hand(Command::Prompt { text, reply }),
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
    reply: Reply,
    turn: BoxFuture<'a, TurnEnd>,
    /// A host command: it can change the command list (`/modules reload`).
    command: bool,
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
    let gone = CancellationToken::new();
    let reader = Watched::new(reader, gone.clone());
    let (peer, connection) = io::spawn(reader, writer, handler);
    let mut client_gone = false;
    // Calls announced `pending` before their permission request; their start is then
    // an `in_progress` update, not a second `tool_call`.
    let mut announced = HashSet::new();
    let mut active: Option<Active<'_>> = None;
    let mut queued: VecDeque<(String, Reply)> = VecDeque::new();
    // Setting changes asked for while a prompt ran; they apply before the next one.
    let mut pending: Vec<(ConfigKind, String)> = Vec::new();
    // The commands last published; a prompt naming one runs it (#676).
    let mut published: Vec<CommandInfo> = Vec::new();
    let mut title = SessionTitle::default();

    loop {
        if active.is_none() && !pending.is_empty() {
            apply_pending(&peer, &protocol, session, std::mem::take(&mut pending)).await;
            publish_commands(&peer, &protocol, session, &mut published).await;
        }
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
            title.prompt(&text);
            let parsed = commands::parse(&text, &published);
            let command = matches!(parsed, Some(Invocation::Host { .. }));
            let turn: BoxFuture<'_, TurnEnd> = match parsed {
                // Nothing runs, so the change applies now, exactly as an idle
                // `session/set_config_option` does.
                Some(Invocation::Setting { kind, argument }) => {
                    let report = setting_command(&peer, &protocol, session, kind, &argument).await;
                    message(&peer, &protocol, &report);
                    publish_commands(&peer, &protocol, session, &mut published).await;
                    answer(
                        codec,
                        reply,
                        TurnEnd::Completed {
                            stop: StopReason::EndTurn,
                        },
                    );
                    publish_title(&peer, &protocol, session, &mut title).await;
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
                None => Box::pin(prompt_turn(front, session, text, token.clone())),
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
        tokio::select! {
            biased;
            Some(stamped) = updates.recv() => {
                forward(&peer, &protocol, &mut announced, stamped.item);
            }
            Some(request) = permissions.recv() => {
                ask(&peer, &protocol, &mut announced, request);
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
                    reply.answer(Err(ended()));
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
                Some(Command::Prompt { text, reply }) => {
                    // The next prompt releases a held one (D4).
                    front.hold.release();
                    queued.push_back((text, reply));
                }
                Some(Command::Open { codec, id, reply }) => {
                    // The settings travel with the id; the command list follows the
                    // answer on the same writer queue.
                    let choices = session.config().await;
                    let mut result = json!({ "sessionId": id });
                    if !choices.is_empty() {
                        result["configOptions"] = codec.encode_config_options(&choices);
                    }
                    if let Some(mode) = choices.iter().find(|choice| choice.kind == ConfigKind::Mode) {
                        result["modes"] = codec.encode_modes(mode);
                    }
                    reply.answer(Ok(result));
                    publish_commands(&peer, &protocol, session, &mut published).await;
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
                // Every update of the turn goes out before its answer; `try_recv` is
                // not held back by the runtime's poll budget.
                while let Ok(stamped) = updates.try_recv() {
                    forward(&peer, &protocol, &mut announced, stamped.item);
                }
                // Written now: the next prompt's lines, or the list a reload changed,
                // follow it.
                answer(done.codec, done.reply, end);
                publish_title(&peer, &protocol, session, &mut title).await;
                if done.command {
                    publish_commands(&peer, &protocol, session, &mut published).await;
                }
            }
        }
    }

    // Every request was answered (the transport waits for that before it ends), so no
    // prompt is running; close the connection.
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

/// Metadata follows the prompt answer on the same writer queue, before the next turn.
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

/// Publish the session's commands when they differ from `published`: once after
/// `session/new`, then after a switch that changed them (another environment can
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
        Ok(CommandOutput::Prompt(text)) => return prompt_turn(front, session, text, token).await,
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

fn answer(codec: Codec, reply: Reply, end: TurnEnd) {
    let outcome = match prompt_outcome(end) {
        Ok(stop) => Ok(codec.encode_stop(stop)),
        Err(error) => Err(serde_json::from_value(codec.encode_error(&error))
            .unwrap_or_else(|_| RpcError::new(-32603, error.message))),
    };
    reply.answer(outcome);
}

/// One prompt: its turn, then the hold (D4) with the inbox turns it waits for, as the
/// line loop drains the inbox after a turn (`run_interactive`, `crates/p1-host/src/run.rs`).
async fn prompt_turn(
    front: &AcpFrontEnd,
    session: &dyn SessionHandle,
    text: String,
    token: CancellationToken,
) -> TurnEnd {
    let hold = &front.hold;
    hold.begin_prompt();
    front.policy.set_turn(Some(token.clone()));
    hold.turn(true);
    let mut end = session.prompt(text, token.clone()).await;
    hold.turn(false);
    while !matches!(end, TurnEnd::Cancelled) && !token.is_cancelled() {
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
