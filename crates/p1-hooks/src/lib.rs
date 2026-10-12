//! The hook core (ADR-0213, seam #711): matching, running and merging the commands a hook file
//! attaches to the events of [`p1_contracts::HookEvent`], and the two decorators through which
//! their answers reach the core.
//!
//! - [`Hooks`] is the [`HookRunner`]: the commands of one event whose [`Matcher`] matches, run
//!   in file order through a [`HookProcess`], each answer decoded by the harness's [`HookCodec`]
//!   and merged ([`HookOutput::merge`]: deny over ask over allow, the first stop sticks,
//!   context accumulates). A hook that fails is reported and does not block; one that blocks
//!   says so in its answer.
//! - [`HookedAuthorization`] wraps the session's [`AuthorizationPolicy`]: `PreToolUse` hooks run
//!   first. A deny refuses the call, an ask sends it to the policy that asks the user, and
//!   anything else leaves the session's own authorization to decide. A hook narrows; it never
//!   grants.
//! - [`HookedContext`] wraps the session's [`ContextPolicy`]: `SessionStart` hooks once, then the
//!   `UserPromptSubmit` hooks of each new prompt. Their context reaches the model labelled, as
//!   an inbox notification; a block refuses the prompt.
//!
//! A bridge (`p1-hooks-claude-code`) supplies the hook file's commands ([`HookSet`]) and the
//! [`HookCodec`] of its harness. Nothing here names a harness.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use p1_contracts::serde_json::{Value, json};
use p1_contracts::{
    AuthorizationPolicy, AuthorizationRequest, BoxFuture, CancellationToken, Compaction,
    ContextError, ContextInput, ContextPolicy, Decision, HookDecision, HookEvent, HookOutput,
    HookRunner, InboxKind, Item, Prepared, ToolInput,
};
use regex::Regex;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// The most a hook may print on each stream; the rest is not read.
pub const MAX_HOOK_OUTPUT: usize = 1024 * 1024;

/// Which subjects a hook applies to: a tool name for the tool events, every subject otherwise.
#[derive(Debug, Clone)]
pub enum Matcher {
    /// Every subject (an empty matcher or `*`).
    Any,
    /// One of these names exactly (`Bash|Read`).
    Names(Vec<String>),
    /// A regular expression that must match the whole subject.
    Pattern(Regex),
}

impl Matcher {
    /// The matcher a hook file spells: empty or `*` for any, names joined by `|` for those
    /// names, anything else a regular expression anchored to the whole subject.
    pub fn parse(text: &str) -> Result<Self, String> {
        let text = text.trim();
        if text.is_empty() || text == "*" {
            return Ok(Self::Any);
        }
        let literal = |name: &str| {
            !name.is_empty()
                && name
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
        };
        if text.split('|').all(literal) {
            return Ok(Self::Names(text.split('|').map(str::to_owned).collect()));
        }
        Regex::new(&format!("^(?:{text})$"))
            .map(Self::Pattern)
            .map_err(|error| format!("hook matcher `{text}` is not a regular expression: {error}"))
    }

    /// Whether the hook applies to `subject`; an event without a subject matches every hook.
    pub fn matches(&self, subject: Option<&str>) -> bool {
        let Some(subject) = subject else {
            return true;
        };
        match self {
            Self::Any => true,
            Self::Names(names) => names.iter().any(|name| name == subject),
            Self::Pattern(pattern) => pattern.is_match(subject),
        }
    }
}

/// One command a hook file attaches to an event.
#[derive(Debug, Clone)]
pub struct HookCommand {
    pub matcher: Matcher,
    /// The shell command, run with `sh -c`.
    pub command: String,
    /// How long the command may run.
    pub timeout: Duration,
}

/// The commands of every event, in the order the hook files list them.
#[derive(Debug, Clone, Default)]
pub struct HookSet {
    commands: BTreeMap<HookEvent, Vec<HookCommand>>,
}

impl HookSet {
    /// Appends `command` to `event`'s commands.
    pub fn push(&mut self, event: HookEvent, command: HookCommand) {
        self.commands.entry(event).or_default().push(command);
    }

    /// `event`'s commands, in file order.
    pub fn commands(&self, event: HookEvent) -> &[HookCommand] {
        self.commands.get(&event).map_or(&[], Vec::as_slice)
    }

    /// The events with at least one command.
    pub fn events(&self) -> impl Iterator<Item = HookEvent> + '_ {
        self.commands
            .iter()
            .filter(|(_, commands)| !commands.is_empty())
            .map(|(event, _)| *event)
    }

    /// Whether no event has a command.
    pub fn is_empty(&self) -> bool {
        self.events().next().is_none()
    }
}

/// What a hook command did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProcessOutput {
    /// The exit code; `None` when the command was killed by a signal.
    pub status: Option<i32>,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

/// How a hook command runs: the process boundary of the core.
pub trait HookProcess: Send + Sync {
    /// Runs `command` with `stdin` on its standard input, for at most `timeout`; an error
    /// says why it did not finish (it could not start, it ran too long, it was cancelled).
    fn run<'a>(
        &'a self,
        command: &'a str,
        stdin: Vec<u8>,
        timeout: Duration,
        cancel: &'a CancellationToken,
    ) -> BoxFuture<'a, Result<ProcessOutput, String>>;
}

/// A hook command as `sh -c <command>` in a directory, with a cleared environment that holds
/// only the variables given: a hook sees no credential the host was started with.
pub struct ShellProcess {
    cwd: PathBuf,
    env: Vec<(OsString, OsString)>,
}

impl ShellProcess {
    /// Commands run in `cwd` with exactly `env`.
    pub fn new(cwd: PathBuf, env: Vec<(OsString, OsString)>) -> Self {
        Self { cwd, env }
    }
}

impl HookProcess for ShellProcess {
    fn run<'a>(
        &'a self,
        command: &'a str,
        stdin: Vec<u8>,
        timeout: Duration,
        cancel: &'a CancellationToken,
    ) -> BoxFuture<'a, Result<ProcessOutput, String>> {
        Box::pin(async move {
            let mut child = tokio::process::Command::new("sh")
                .arg("-c")
                .arg(command)
                .current_dir(&self.cwd)
                .env_clear()
                .envs(self.env.iter().map(|(key, value)| (key, value)))
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .kill_on_drop(true)
                .spawn()
                .map_err(|error| format!("could not start: {error}"))?;
            let mut input = child.stdin.take().expect("stdin is piped");
            let mut stdout = child.stdout.take().expect("stdout is piped");
            let mut stderr = child.stderr.take().expect("stderr is piped");
            let run = async {
                // A hook that ignores its input must not block on a full pipe.
                let write = async {
                    let _ = input.write_all(&stdin).await;
                    drop(input);
                };
                let (_, out, err, status) = tokio::join!(
                    write,
                    bounded(&mut stdout),
                    bounded(&mut stderr),
                    child.wait()
                );
                (out, err, status)
            };
            tokio::select! {
                (out, err, status) = run => Ok(ProcessOutput {
                    status: status.map_err(|error| format!("could not be waited for: {error}"))?.code(),
                    stdout: out,
                    stderr: err,
                }),
                () = tokio::time::sleep(timeout) => Err(format!("ran longer than {}s and was stopped", timeout.as_secs())),
                () = cancel.cancelled() => Err("was cancelled".to_owned()),
            }
        })
    }
}

/// At most [`MAX_HOOK_OUTPUT`] bytes of `stream`; the rest is drained unread.
async fn bounded(stream: &mut (impl tokio::io::AsyncRead + Unpin)) -> Vec<u8> {
    let mut kept = Vec::new();
    let mut buffer = [0_u8; 8192];
    loop {
        match stream.read(&mut buffer).await {
            Ok(0) | Err(_) => return kept,
            Ok(count) => {
                let room = MAX_HOOK_OUTPUT.saturating_sub(kept.len());
                kept.extend_from_slice(&buffer[..count.min(room)]);
            }
        }
    }
}

/// How one harness's hooks are fed and read.
pub trait HookCodec: Send + Sync {
    /// What a hook's matcher is tested against for `event` (a tool name), or `None` for an
    /// event every hook of it applies to.
    fn subject(&self, event: HookEvent, payload: &Value) -> Option<String>;
    /// The bytes a hook command reads on its standard input.
    fn encode(&self, event: HookEvent, payload: &Value) -> Vec<u8>;
    /// The answer a command gave, or why it failed: a failure is reported and blocks nothing.
    fn decode(&self, event: HookEvent, output: &ProcessOutput) -> Result<HookOutput, String>;
}

/// The hook core: one hook set, its harness's codec and the process its commands run as.
pub struct Hooks {
    set: HookSet,
    codec: Arc<dyn HookCodec>,
    process: Arc<dyn HookProcess>,
    report: Arc<dyn Fn(String) + Send + Sync>,
}

impl Hooks {
    /// The runner of `set`. `report` receives one line for each hook that failed.
    pub fn new(
        set: HookSet,
        codec: Arc<dyn HookCodec>,
        process: Arc<dyn HookProcess>,
        report: Arc<dyn Fn(String) + Send + Sync>,
    ) -> Self {
        Self {
            set,
            codec,
            process,
            report,
        }
    }
}

impl HookRunner for Hooks {
    fn run<'a>(
        &'a self,
        event: HookEvent,
        payload: Value,
        cancel: &'a CancellationToken,
    ) -> BoxFuture<'a, HookOutput> {
        Box::pin(async move {
            let mut merged = HookOutput::default();
            // An event no decision point serves is read and not run (ADR-0213).
            if !event.is_served() {
                return merged;
            }
            let subject = self.codec.subject(event, &payload);
            let stdin = self.codec.encode(event, &payload);
            for hook in self.set.commands(event) {
                if !hook.matcher.matches(subject.as_deref()) {
                    continue;
                }
                let answer = self
                    .process
                    .run(&hook.command, stdin.clone(), hook.timeout, cancel)
                    .await
                    .and_then(|output| self.codec.decode(event, &output));
                match answer {
                    Ok(answer) => merged = merged.merge(answer),
                    Err(reason) => (self.report)(format!(
                        "{} hook `{}` {reason}; it does not block",
                        event.name(),
                        hook.command
                    )),
                }
            }
            merged
        })
    }
}

/// The session's authorization with `PreToolUse` hooks in front (ADR-0213).
pub struct HookedAuthorization {
    inner: Arc<dyn AuthorizationPolicy>,
    hooks: Arc<dyn HookRunner>,
    asker: Option<Arc<dyn AuthorizationPolicy>>,
}

impl HookedAuthorization {
    /// `inner` decides whatever the hooks leave open; `asker`, when the session can ask the
    /// user, answers a hook's ask. Without one, a hook's ask refuses the call.
    pub fn new(
        inner: Arc<dyn AuthorizationPolicy>,
        hooks: Arc<dyn HookRunner>,
        asker: Option<Arc<dyn AuthorizationPolicy>>,
    ) -> Self {
        Self {
            inner,
            hooks,
            asker,
        }
    }
}

/// The payload of a tool event: the call and its input as JSON when it is JSON.
pub fn tool_payload(request: &AuthorizationRequest<'_>) -> Value {
    let input = match &request.call.input {
        ToolInput::Json(raw) => {
            p1_contracts::serde_json::from_str(raw).unwrap_or(Value::String(raw.clone()))
        }
        ToolInput::Text(text) => Value::String(text.clone()),
    };
    json!({
        "tool_name": request.call.name,
        "tool_input": input,
        "call_id": request.call.call_id,
        "implementation": request.identity.implementation,
    })
}

impl AuthorizationPolicy for HookedAuthorization {
    fn authorize<'a>(&'a self, request: AuthorizationRequest<'a>) -> BoxFuture<'a, Decision> {
        Box::pin(async move {
            // The hooks' own timeouts bound them; an authorization has no cancellation.
            let cancel = CancellationToken::new();
            let answer = self
                .hooks
                .run(HookEvent::PreToolUse, tool_payload(&request), &cancel)
                .await;
            if let Some(reason) = answer.stop {
                return Decision::Deny {
                    reason: format!("a hook stopped the session: {reason}"),
                };
            }
            match answer.decision {
                Some(HookDecision::Deny { reason }) => Decision::Deny {
                    reason: format!("a hook refused this call: {reason}"),
                },
                Some(HookDecision::Ask) => match &self.asker {
                    Some(asker) => asker.authorize(request).await,
                    None => Decision::Deny {
                        reason: "a hook asked for confirmation, and this session cannot ask"
                            .to_owned(),
                    },
                },
                // An allow grants nothing: the session's authorization decides.
                Some(HookDecision::Allow) | None => self.inner.authorize(request).await,
            }
        })
    }
}

/// The session's context policy with `SessionStart` and `UserPromptSubmit` hooks behind it
/// (ADR-0213).
pub struct HookedContext {
    inner: Arc<dyn ContextPolicy>,
    hooks: Arc<dyn HookRunner>,
    state: Mutex<ContextHookState>,
}

#[derive(Default)]
struct ContextHookState {
    started: bool,
    /// How many items the history had when its last prompt's hooks ran: a later preparation
    /// of the same prompt does not run them again.
    prompt_seen_at: Option<usize>,
}

/// The label hook text reaches the model under.
pub fn labelled_context(event: HookEvent, context: &str) -> String {
    format!(
        "<hook-context event=\"{}\">\n{context}\n</hook-context>",
        event.name()
    )
}

impl HookedContext {
    pub fn new(inner: Arc<dyn ContextPolicy>, hooks: Arc<dyn HookRunner>) -> Self {
        Self {
            inner,
            hooks,
            state: Mutex::new(ContextHookState::default()),
        }
    }

    /// The events this preparation runs, with their payloads.
    fn due(&self, history: &[Item]) -> Vec<(HookEvent, Value)> {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        let mut due = Vec::new();
        if !state.started {
            state.started = true;
            due.push((HookEvent::SessionStart, json!({ "source": "startup" })));
        }
        if let Some(Item::User { text }) = history.last()
            && state.prompt_seen_at != Some(history.len())
        {
            state.prompt_seen_at = Some(history.len());
            due.push((HookEvent::UserPromptSubmit, json!({ "prompt": text })));
        }
        due
    }
}

impl ContextPolicy for HookedContext {
    fn prepare<'a>(
        &'a self,
        input: ContextInput<'a>,
    ) -> BoxFuture<'a, Result<Option<Prepared>, ContextError>> {
        Box::pin(async move {
            let due = self.due(input.history);
            let mut contexts = Vec::new();
            for (event, payload) in due {
                let answer = self.hooks.run(event, payload, input.cancel).await;
                if let Some(reason) = answer.stop.or(match answer.decision {
                    Some(HookDecision::Deny { reason }) => Some(reason),
                    _ => None,
                }) {
                    return Err(ContextError::Failed(format!(
                        "a {} hook blocked the prompt: {reason}",
                        event.name()
                    )));
                }
                if let Some(context) = answer.additional_context {
                    contexts.push(Item::Inbox {
                        kind: InboxKind::Notification,
                        text: labelled_context(event, &context),
                    });
                }
            }
            let history = input.history;
            let cancel = input.cancel;
            let prepared = self
                .inner
                .prepare(ContextInput {
                    history,
                    last_usage: input.last_usage,
                    cancel,
                })
                .await?;
            if contexts.is_empty() {
                return Ok(prepared);
            }
            let (mut items, usage) = match prepared {
                Some(prepared) => (prepared.items, prepared.usage),
                None => (history.to_vec(), None),
            };
            items.extend(contexts);
            Ok(Some(Prepared { items, usage }))
        })
    }

    fn compact_now<'a>(
        &'a self,
        input: ContextInput<'a>,
    ) -> BoxFuture<'a, Result<Compaction, ContextError>> {
        self.inner.compact_now(input)
    }
}
