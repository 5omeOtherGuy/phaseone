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
use super::io::{self, Handler, Peer, RpcError};
use crate::{
    capabilities,
    codec::Codec,
    commands::{self, Invocation},
    config_options::{self, Refusal},
    policy::{PermissionReply, PermissionRequest},
    sink::{Outbound, Update},
    turn::prompt_outcome,
};
use p1_contracts::frontend::{CommandInfo, CommandOutput, ConfigChoice, ConfigKind, SessionHandle};
use p1_contracts::{AgentEvent, BoxFuture, CancellationToken, StopReason, TurnEnd};
use serde_json::{Value, json};
use std::collections::{HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use tokio::sync::{mpsc, oneshot};

const INVALID_PARAMS: i64 = -32602;
/// JSON-RPC's server-error range: a request that is well formed but not allowed now.
const NOT_ALLOWED: i64 = -32000;
const INTERNAL: i64 = -32603;

type Reply = oneshot::Sender<Result<Value, RpcError>>;

enum Command {
    Prompt {
        text: String,
        reply: Reply,
    },
    Cancel,
    /// The settings `session/new` announces.
    Config {
        reply: oneshot::Sender<Vec<ConfigChoice>>,
    },
    SetConfig {
        option: String,
        value: String,
        reply: Reply,
    },
    /// `session/new` answered: publish the session's commands.
    Opened,
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

    /// The session's settings travel with its id: the loop asks the session for them.
    async fn opened(&self, codec: Codec, id: String) -> Value {
        let (reply, answer) = oneshot::channel();
        let choices = match self.commands.send(Command::Config { reply }) {
            Ok(()) => answer.await.unwrap_or_default(),
            Err(_) => Vec::new(),
        };
        let mut result = json!({ "sessionId": id });
        if !choices.is_empty() {
            result["configOptions"] = codec.encode_config_options(&choices);
        }
        result
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
                "session/new" => {
                    let (codec, id) = self.new_session(&params)?;
                    let opened = self.opened(codec, id).await;
                    // The transport writes this answer before the loop runs again (one
                    // thread), so the command list follows the session's id.
                    let _ = self.commands.send(Command::Opened);
                    Ok(opened)
                }
                "session/set_config_option" => {
                    self.check_session(&params)?;
                    let text = |key: &str| {
                        params
                            .get(key)
                            .and_then(Value::as_str)
                            .map(str::to_string)
                            .ok_or_else(|| {
                                invalid(format!("session/set_config_option needs a string {key}"))
                            })
                    };
                    let (option, value) = (text("configId")?, text("value")?);
                    let (reply, answer) = oneshot::channel();
                    self.commands
                        .send(Command::SetConfig {
                            option,
                            value,
                            reply,
                        })
                        .map_err(|_| RpcError::new(NOT_ALLOWED, "the session has ended"))?;
                    answer.await.unwrap_or_else(|_| {
                        Err(RpcError::new(NOT_ALLOWED, "the session has ended"))
                    })
                }
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
            let turn: BoxFuture<'_, TurnEnd> = match commands::parse(&text, &published) {
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
                queued.clear();
                pending.clear();
                front.hold.release();
                session.cancel_runs().await;
                session.stop_workers().await;
            }
            command = inbox.recv() => match command {
                // Its handler answers that the session has ended.
                Some(Command::Prompt { .. }) if client_gone => {}
                Some(Command::Prompt { text, reply }) => {
                    // The next prompt releases a held one (D4).
                    front.hold.release();
                    queued.push_back((text, reply));
                }
                Some(Command::Config { reply }) => {
                    let _ = reply.send(session.config().await);
                }
                Some(Command::SetConfig { option, value, reply }) => {
                    let idle = active.is_none() && queued.is_empty();
                    let outcome =
                        set_config(&peer, &protocol, session, &mut pending, idle, &option, &value).await;
                    let applied = idle && outcome.is_ok();
                    let _ = reply.send(outcome);
                    if applied {
                        publish_commands(&peer, &protocol, session, &mut published).await;
                    }
                }
                Some(Command::Opened) => {
                    publish_commands(&peer, &protocol, session, &mut published).await;
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
                answer(done.codec, done.reply, end);
            }
        }
    }

    // Every handler has finished, so no prompt is running; close the connection.
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

/// A `session/set_config_option`. Idle, the change applies now; while a prompt runs or
/// waits it applies before the next one, and the answer already names it. Either way
/// the answer is the complete option list, and an applied change is also announced as
/// a `config_option_update`.
async fn set_config(
    peer: &Peer,
    protocol: &Mutex<Protocol>,
    session: &dyn SessionHandle,
    pending: &mut Vec<(ConfigKind, String)>,
    idle: bool,
    option: &str,
    value: &str,
) -> Result<Value, RpcError> {
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
    // `idle` is what keeps the agent lock `set_config` takes free: during a turn the
    // turn holds it and is not polled while this arm waits.
    let choices = if idle {
        session
            .set_config(kind, value)
            .await
            .map_err(|reason| RpcError::new(INTERNAL, reason))?;
        let choices = session.config().await;
        announce_config(peer, protocol, &choices);
        choices
    } else {
        config_options::queue(pending, kind, value);
        config_options::waiting(choices, pending)
    };
    Ok(json!({ "configOptions": codec.encode_config_options(&choices) }))
}

/// Apply the changes a prompt held back, in the order they were asked for. A change
/// that fails now is reported on stderr; the update then shows what the session runs.
async fn apply_pending(
    peer: &Peer,
    protocol: &Mutex<Protocol>,
    session: &dyn SessionHandle,
    pending: Vec<(ConfigKind, String)>,
) {
    for (kind, value) in pending {
        if let Err(reason) = session.set_config(kind, &value).await {
            eprintln!("p1 acp: {} stays: {reason}", config_options::id(kind));
        }
    }
    announce_config(peer, protocol, &session.config().await);
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
    match set_config(
        peer,
        protocol,
        session,
        &mut Vec::new(),
        true,
        option,
        argument,
    )
    .await
    {
        Ok(_) => format!("{}: {argument}\n", commands::name(kind)),
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

fn announce_config(peer: &Peer, protocol: &Mutex<Protocol>, choices: &[ConfigChoice]) {
    let Some((codec, session)) = protocol.lock().unwrap().ready() else {
        return;
    };
    let _ = peer.notify(
        "session/update",
        json!({ "sessionId": session, "update": codec.encode_config_update(choices) }),
    );
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
