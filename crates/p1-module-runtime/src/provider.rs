//! `WasmProvider` (`docs/design/modules/adapters.md`): the adapter from one loaded provider
//! component (world `p1:module/provider@1.0.0`) to `p1_contracts::Provider`, with the native
//! transport broker of `p1-provider-http` sending every request (freeze item 9, ADR-0086).
//!
//! One executor owns the component. It is a thread of its own that owns the provider's one
//! Store, its configured instance and every live `decoding.decoder` resource (the connection
//! state), and it serves one command at a time from a channel, so no Store is ever entered
//! concurrently. It is a thread rather than a Tokio task because most callers are
//! synchronous — `Provider::validate` and the broker's `ResponseParser`, whose `on_event`,
//! `on_end` and `on_http_error` run inside the drive loop's poll — and a synchronous wait on
//! a task of the caller's own current-thread runtime would never be answered. Every caller
//! waits for its reply on its own thread, `stream`'s boxed `Send` future (ADR-0015) included:
//! a guest call is bounded computation, as a native adapter's lowering and parsing are, and
//! a future parked on another thread would look idle to a runtime whose clock is paused,
//! which would then jump its timers past a wait that never ends on the runtime.
//!
//! The Store is synchronous: the provider world's granted capabilities (`http`, `websocket`,
//! `credential-control`) are type-only interfaces, so nothing asynchronous is linked and
//! every export runs with wasmtime's synchronous call on the executor thread. Each call gets
//! the full [`ExecutionLimits`] fuel and a deadline on the engine's epoch clock. A failed call
//! (trap, fuel, deadline) leaves an instance that may not be entered again, so it is dropped
//! with every decoder it held; the next command builds and configures a fresh one.
//!
//! - Construction calls `configure` then `describe` once and caches the description; either
//!   failing is a construction error ([`ProviderError`]).
//! - `stream` runs `validate` then `lower` in the component before anything is sent; a
//!   refusal is the setup error a native adapter returns. The lowered request goes through
//!   [`broker_drive`], which keeps retry, backoff, the one refresh after 401/403, the read
//!   bounds and cancellation; each framed SSE event goes to a fresh decoder per attempt, and
//!   a non-2xx response to `classify`. The broker applies its own status policy to the kind
//!   `classify` returns, as it does for a native parser (ADR-0046, ADR-0062).
//! - Exactly one `Finished` per stream holds host side too: whatever a decoder returns after
//!   its terminal event is dropped, and the decoder with it.
//! - A module failure maps as `ModuleFailure::into_provider_outcome` fixes it: a trap, fuel
//!   or invalid output is `Protocol` with a message of this runtime, never guest text read
//!   as a value; a deadline is `Transport`.
//! - Endpoint and path (ADR-0086): a route whose endpoint is a full request URL gets a route
//!   authority without its last path segment exactly when the component lowered that
//!   segment as the path, so the request URL is the endpoint unchanged. The split lives here
//!   because the component crates are guest-only and cannot be a host dependency; it is the
//!   same rule as the components' `http_target`.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, mpsc};
use std::thread;

use p1_contracts::serde_json;
use p1_contracts::{
    BoxFuture, CancellationToken, DeclarationKind, Outcome, Provider,
    ProviderError as ContractError, ProviderErrorKind, ProviderRequest, ProviderStream,
    RouteDescription, StreamEvent, ToolDeclaration,
};
use p1_module_protocol::{
    ModuleFailure, WireItem, WireModelOptions, WireProviderError, WireRouteDescription,
    WireStreamEvent,
};
use p1_provider_http::{
    CredentialScheme, CredentialSource, CredentialUse, LoweredHttpRequest, ResponseParser,
    RetryPolicy, RouteAuthority, SseEvent, Transport, broker_drive,
};
use thiserror::Error;
use wasmtime::component::{
    Component, ComponentExportIndex, Instance, InstancePre, Linker, ResourceAny, Val,
};
use wasmtime::{Engine, Store, Trap, UpdateDeadline};

use crate::executor::ExecutionLimits;
use crate::loader::{EPOCH_TICK, Epochs, LoadedModule, ModuleKind, interface_import};

/// The capabilities a provider component may be granted and this adapter links: the
/// transport vocabulary, type-only. The rest of the provider allocation (`control`, `clock`,
/// `random`, `notices`) would need asynchronous host functions in the one synchronous Store,
/// so a manifest granting one is refused at construction until a provider needs it.
pub const PROVIDER_LINKED: [&str; 3] = ["http", "websocket", "credential-control"];

/// The export instance of the decoder resource.
const DECODING: &str = "p1:module/decoding@1.0.0";

// Constant messages: a value the module returned is never quoted back, since it may carry
// prompt or response text.
const BAD_EVENT: &str = "a decoded stream event is not protocol stream-event JSON";
const BAD_ERROR: &str = "a provider error is not protocol provider-error JSON";
const BAD_DESCRIPTION: &str = "describe did not return a protocol route description";
const BAD_LOWERED: &str = "lower did not return an http request of the transport interface";
const WEBSOCKET_LOWERED: &str =
    "lower chose WebSocket, which this broker does not send yet; nothing was sent";
const BAD_RESULT: &str = "an export returned a value of the wrong shape";
const NOT_TERMINAL: &str = "the decoder's finish returned an event that is not finished";
const DECODER_LOST: &str = "the decoder was lost with its instance after an earlier failure";
const ENDED_TWICE: &str = "the response ended after its terminal event";
const REBUILD_REFUSED: &str = "configure refused the settings on a rebuilt instance";
const UNSERIALIZABLE: &str = "the request cannot be serialized";

/// The instance a route file and a model profile assemble (`provider-settings`).
#[derive(Debug, Clone, PartialEq)]
pub struct ProviderSettings {
    /// `Origin.route` of the route file.
    pub origin_route: String,
    /// The route file's endpoint, unchanged; the broker's route authority is built from it.
    pub endpoint: String,
    /// The model profile id.
    pub model: String,
    /// The name the route's wire knows the model by.
    pub wire_model: String,
    /// The `adapter-settings` object: the route's `[adapter_settings]` plus the reserved
    /// keys of ADR-0086 (`RouteFile::component_adapter_settings` builds it).
    pub adapter_settings: serde_json::Value,
}

/// Why a loaded module could not become a provider.
#[derive(Debug, Error)]
pub enum ProviderError {
    /// The module is not of the `provider` class.
    #[error("module {name} is a {kind} module, not a provider")]
    NotAProvider {
        /// The module.
        name: String,
        /// Its class.
        kind: &'static str,
    },
    /// The manifest grants a capability this adapter does not link.
    #[error("module {name}: capability {capability} is not linked for a provider")]
    Capability {
        /// The module.
        name: String,
        /// The capability as written.
        capability: String,
    },
    /// The route's endpoint is not one the broker sends to.
    #[error("module {name}: {source}")]
    Endpoint {
        /// The module.
        name: String,
        /// Why.
        source: p1_provider_http::InvalidEndpoint,
    },
    /// The component does not fit the `provider` world as linked, or trapped while it was
    /// set up.
    #[error("module {name} cannot be instantiated: {reason}")]
    Instantiate {
        /// The module.
        name: String,
        /// wasmtime's message or the runtime's.
        reason: String,
    },
    /// `configure` refused the settings: the provider is never built.
    #[error("module {name} refused its settings: {reason}")]
    Configure {
        /// The module.
        name: String,
        /// The module's provider error.
        reason: ContractError,
    },
    /// `describe` failed or returned no valid route description.
    #[error("module {name} has no valid route description: {reason}")]
    Describe {
        /// The module.
        name: String,
        /// Why.
        reason: String,
    },
    /// The executor thread could not start.
    #[error("module {name}: cannot start the provider executor thread: {source}")]
    Thread {
        /// The module.
        name: String,
        /// Why.
        source: std::io::Error,
    },
}

/// The provider adapter over one loaded provider component.
pub struct WasmProvider {
    description: RouteDescription,
    executor: ExecutorHandle,
    /// The route authority over the endpoint as the route file names it.
    authority: RouteAuthority,
    /// The endpoint's last path segment as a path, and the authority without it.
    split: Option<(String, RouteAuthority)>,
    transport: Arc<dyn Transport>,
}

impl WasmProvider {
    /// Builds the provider for `module` configured with `settings`. The broker sends every
    /// request on `transport` to `settings.endpoint`, authenticated from `credentials`; the
    /// component names neither. Calls `configure` and `describe` before it returns, on the
    /// executor thread it starts; needs no Tokio runtime.
    pub fn new(
        module: &LoadedModule,
        settings: ProviderSettings,
        credentials: Arc<dyn CredentialSource>,
        transport: Arc<dyn Transport>,
        limits: ExecutionLimits,
    ) -> Result<Self, ProviderError> {
        let name = module.name().to_owned();
        if module.kind() != ModuleKind::Provider {
            return Err(ProviderError::NotAProvider {
                name,
                kind: module.kind().name(),
            });
        }
        if let Some(capability) = module
            .capabilities()
            .iter()
            .find(|capability| !PROVIDER_LINKED.contains(&capability.as_str()))
        {
            return Err(ProviderError::Capability {
                name,
                capability: capability.clone(),
            });
        }
        let authority =
            RouteAuthority::new(&settings.endpoint, credentials.clone()).map_err(|source| {
                ProviderError::Endpoint {
                    name: name.clone(),
                    source,
                }
            })?;
        let split = split_endpoint(&settings.endpoint).and_then(|(base, path)| {
            RouteAuthority::new(base, credentials)
                .ok()
                .map(|authority| (path, authority))
        });

        let instantiate = |reason: String| ProviderError::Instantiate {
            name: name.clone(),
            reason,
        };
        let mut linker: Linker<()> = Linker::new(&module.engine);
        for interface in module
            .capabilities()
            .iter()
            .map(String::as_str)
            .chain(["types"])
        {
            linker
                .instance(&interface_import(interface))
                .map_err(|error| instantiate(format!("{error:#}")))?;
        }
        let pre = linker
            .instantiate_pre(&module.component)
            .map_err(|error| instantiate(format!("{error:#}")))?;
        let exports = Exports::find(&module.component).map_err(instantiate)?;

        let mut machine = Machine {
            engine: module.engine.clone(),
            pre,
            epochs: module.epochs.clone(),
            limits,
            settings: settings_val(settings),
            exports,
            live: None,
            decoders: HashMap::new(),
        };
        let (commands, receiver) = mpsc::channel();
        let (ready, started) = mpsc::sync_channel(1);
        thread::Builder::new()
            .name(format!("p1-provider {name}"))
            .spawn(move || {
                let start = machine.start();
                let serve = start.is_ok();
                if ready.send(start).is_ok() && serve {
                    machine.serve(&receiver);
                }
            })
            .map_err(|source| ProviderError::Thread {
                name: name.clone(),
                source,
            })?;
        let description = match started.recv() {
            Ok(Ok(description)) => description,
            Ok(Err(Start::Instantiate(reason))) => return Err(instantiate(reason)),
            Ok(Err(Start::Configure(reason))) => {
                return Err(ProviderError::Configure { name, reason });
            }
            Ok(Err(Start::Describe(reason))) => {
                return Err(ProviderError::Describe { name, reason });
            }
            Err(_) => return Err(instantiate("the executor thread ended".to_owned())),
        };
        Ok(Self {
            description,
            executor: ExecutorHandle {
                commands,
                decoders: Arc::new(AtomicU64::new(0)),
            },
            authority,
            split,
            transport,
        })
    }
}

impl Provider for WasmProvider {
    fn describe(&self) -> RouteDescription {
        self.description.clone()
    }

    fn validate(&self, request: &ProviderRequest) -> Result<(), ContractError> {
        let request = request_val(request).map_err(module_error)?;
        self.executor
            .ask(|reply| Command::Validate { request, reply })
            .map_err(Refusal::from)
            .and_then(|answer| answer)
            .map_err(Refusal::into_error)
    }

    fn stream<'a>(
        &'a self,
        request: ProviderRequest,
        cancel: CancellationToken,
    ) -> BoxFuture<'a, Result<ProviderStream, ContractError>> {
        Box::pin(async move {
            let request = request_val(&request).map_err(module_error)?;
            let lowered = self
                .executor
                .ask(|reply| Command::Prepare { request, reply })
                .map_err(Refusal::from)
                .and_then(|answer| answer)
                .map_err(Refusal::into_error)?;
            let authority = match &self.split {
                Some((path, authority)) if *path == lowered.path => authority,
                _ => &self.authority,
            };
            let executor = self.executor.clone();
            broker_drive(
                authority,
                self.transport.clone(),
                &lowered,
                Box::new(move || Box::new(ComponentParser::new(executor.clone()))),
                RetryPolicy::default(),
                cancel,
            )
        })
    }
}

/// The endpoint's last path segment as a path, and the endpoint without it: `None` for an
/// endpoint with no path segment. Trailing slashes are ignored, as the components'
/// `http_target` ignores them.
fn split_endpoint(endpoint: &str) -> Option<(&str, String)> {
    let (base, last) = endpoint.trim_end_matches('/').rsplit_once('/')?;
    // `https://host` splits at the scheme's `//`: the host is no path segment.
    if last.is_empty() || base.ends_with('/') {
        return None;
    }
    Some((base, format!("/{last}")))
}

/// What a `result<_, provider-error>` export refused with, or how the call failed.
#[derive(Debug)]
enum Refusal {
    Refused(ContractError),
    Failed(ModuleFailure),
}

impl From<ModuleFailure> for Refusal {
    fn from(failure: ModuleFailure) -> Self {
        Self::Failed(failure)
    }
}

impl Refusal {
    fn into_error(self) -> ContractError {
        match self {
            Self::Refused(error) => error,
            Self::Failed(failure) => module_error(failure),
        }
    }
}

/// A module failure as the error of a call that returns one, kinds as
/// `ModuleFailure::into_provider_outcome` maps them. This executor never cancels a guest
/// call (the broker owns cancellation), so the `Cancelled` arm is only for totality.
fn module_error(failure: ModuleFailure) -> ContractError {
    match failure.into_provider_outcome() {
        Outcome::Failed(error) => error,
        _ => ContractError::new(
            ProviderErrorKind::Protocol,
            "provider module call cancelled",
        ),
    }
}

/// Where an answer goes: the caller, blocked on its thread until it arrives.
struct Reply<T>(mpsc::SyncSender<T>);

impl<T> Reply<T> {
    fn send(self, value: T) {
        // A caller that went away no longer wants the answer.
        let _ = self.0.send(value);
    }
}

enum Command {
    Validate {
        request: Val,
        reply: Reply<Result<(), Refusal>>,
    },
    /// `validate`, then `lower` when it passed.
    Prepare {
        request: Val,
        reply: Reply<Result<LoweredHttpRequest, Refusal>>,
    },
    Classify {
        status: u16,
        headers: Vec<(String, String)>,
        body: Vec<u8>,
        reply: Reply<Result<ContractError, ModuleFailure>>,
    },
    /// Feed one event to decoder `key`, constructing it first when `create`.
    Feed {
        key: u64,
        create: bool,
        event: SseEvent,
        reply: Reply<Result<Vec<StreamEvent>, ModuleFailure>>,
    },
    Finish {
        key: u64,
        create: bool,
        reply: Reply<Result<Outcome, ModuleFailure>>,
    },
    Drop {
        key: u64,
    },
}

/// The callers' side of the executor: its channel, and the source of decoder keys.
#[derive(Clone)]
struct ExecutorHandle {
    commands: mpsc::Sender<Command>,
    decoders: Arc<AtomicU64>,
}

fn stopped() -> ModuleFailure {
    ModuleFailure::Trap("the provider executor stopped before the call ended".to_owned())
}

impl ExecutorHandle {
    /// Sends `command` and waits for its answer on this thread.
    fn ask<T>(&self, command: impl FnOnce(Reply<T>) -> Command) -> Result<T, ModuleFailure> {
        let (reply, answer) = mpsc::sync_channel(1);
        self.commands
            .send(command(Reply(reply)))
            .map_err(|_| stopped())?;
        answer.recv().map_err(|_| stopped())
    }

    fn drop_decoder(&self, key: u64) {
        let _ = self.commands.send(Command::Drop { key });
    }
}

/// One response attempt's decoder, as the broker's drive loop sees a parser.
struct ComponentParser {
    executor: ExecutorHandle,
    key: u64,
    /// The component-side decoder exists; it is constructed at the first event, so an
    /// attempt that ends in a non-2xx status never makes one.
    created: bool,
    /// The terminal outcome was returned: nothing more is decoded.
    terminated: bool,
}

impl ComponentParser {
    fn new(executor: ExecutorHandle) -> Self {
        let key = executor.decoders.fetch_add(1, Ordering::Relaxed);
        Self {
            executor,
            key,
            created: false,
            terminated: false,
        }
    }

    /// Whether this call constructs the decoder.
    fn create(&mut self) -> bool {
        !std::mem::replace(&mut self.created, true)
    }

    fn terminate(&mut self) {
        self.terminated = true;
        if self.created {
            self.executor.drop_decoder(self.key);
        }
    }
}

impl Drop for ComponentParser {
    fn drop(&mut self) {
        if !self.terminated && self.created {
            self.executor.drop_decoder(self.key);
        }
    }
}

impl ResponseParser for ComponentParser {
    fn on_event(&mut self, event: SseEvent) -> Vec<StreamEvent> {
        if self.terminated {
            return Vec::new();
        }
        let (key, create) = (self.key, self.create());
        let answer = self
            .executor
            .ask(|reply| Command::Feed {
                key,
                create,
                event,
                reply,
            })
            .and_then(|answer| answer);
        match answer {
            Ok(events) => {
                let (events, finished) = through_terminal(events);
                if finished {
                    self.terminate();
                }
                events
            }
            Err(failure) => {
                self.terminate();
                vec![StreamEvent::Finished(failure.into_provider_outcome())]
            }
        }
    }

    fn on_end(&mut self) -> Outcome {
        if self.terminated {
            return Outcome::Failed(ContractError::new(ProviderErrorKind::Protocol, ENDED_TWICE));
        }
        let (key, create) = (self.key, self.create());
        let answer = self
            .executor
            .ask(|reply| Command::Finish { key, create, reply })
            .and_then(|answer| answer);
        self.terminate();
        answer.unwrap_or_else(ModuleFailure::into_provider_outcome)
    }

    fn on_http_error(
        &self,
        status: u16,
        headers: &[(String, String)],
        body: &[u8],
    ) -> ContractError {
        let (headers, body) = (headers.to_vec(), body.to_vec());
        self.executor
            .ask(|reply| Command::Classify {
                status,
                headers,
                body,
                reply,
            })
            .and_then(|answer| answer)
            .unwrap_or_else(module_error)
    }
}

/// The events up to and including the first `Finished`, and whether there was one: what a
/// decoder returns after its terminal event is dropped.
fn through_terminal(mut events: Vec<StreamEvent>) -> (Vec<StreamEvent>, bool) {
    match events
        .iter()
        .position(|event| matches!(event, StreamEvent::Finished(_)))
    {
        Some(terminal) => {
            events.truncate(terminal + 1);
            (events, true)
        }
        None => (events, false),
    }
}

/// The exports this adapter calls, looked up once on the component.
struct Exports {
    configure: ComponentExportIndex,
    describe: ComponentExportIndex,
    validate: ComponentExportIndex,
    lower: ComponentExportIndex,
    classify: ComponentExportIndex,
    new_decoder: ComponentExportIndex,
    feed: ComponentExportIndex,
    finish: ComponentExportIndex,
}

impl Exports {
    fn find(component: &Component) -> Result<Self, String> {
        let top = |name: &str| {
            component
                .get_export_index(None, name)
                .ok_or_else(|| format!("the component exports no {name}"))
        };
        let decoding = top(DECODING)?;
        let decoder = |name: &str| {
            component
                .get_export_index(Some(&decoding), name)
                .ok_or_else(|| format!("the component's {DECODING} exports no {name}"))
        };
        Ok(Self {
            configure: top("configure")?,
            describe: top("describe")?,
            validate: top("validate")?,
            lower: top("lower")?,
            classify: top("classify")?,
            new_decoder: decoder("[constructor]decoder")?,
            feed: decoder("[method]decoder.feed")?,
            finish: decoder("[method]decoder.finish")?,
        })
    }
}

/// Why the executor could not start the provider.
enum Start {
    Instantiate(String),
    Configure(ContractError),
    Describe(String),
}

/// The configured instance and the Store it lives in.
struct Live {
    store: Store<()>,
    instance: Instance,
}

/// The executor: the one owner of the Store, the instance and the decoders.
struct Machine {
    engine: Engine,
    pre: InstancePre<()>,
    epochs: Arc<Epochs>,
    limits: ExecutionLimits,
    settings: Val,
    exports: Exports,
    live: Option<Live>,
    /// The decoders of the live instance, by the key their parser holds.
    decoders: HashMap<u64, ResourceAny>,
}

/// Why the epoch callback stopped a call.
#[derive(Debug, Error)]
#[error("the provider module call ran past its deadline")]
struct DeadlineStop;

impl Machine {
    /// Builds the instance, configures it and reads its description.
    fn start(&mut self) -> Result<RouteDescription, Start> {
        let live = match self.instantiate() {
            Ok(Ok(live)) => live,
            Ok(Err(refused)) => return Err(Start::Configure(refused)),
            Err(failure) => return Err(Start::Instantiate(failure.to_string())),
        };
        self.live = Some(live);
        let results = self
            .call(|exports| &exports.describe, &[])
            .map_err(|failure| Start::Describe(failure.to_string()))?;
        match results.into_iter().next() {
            Some(Val::String(text)) => serde_json::from_str::<WireRouteDescription>(&text)
                .map(RouteDescription::from)
                .map_err(|_| Start::Describe(BAD_DESCRIPTION.to_owned())),
            _ => Err(Start::Describe(BAD_DESCRIPTION.to_owned())),
        }
    }

    fn serve(&mut self, commands: &mpsc::Receiver<Command>) {
        // Ends when the provider and every parser holding a handle are gone; the Store,
        // the instance and the decoders are dropped with the machine.
        while let Ok(command) = commands.recv() {
            self.handle(command);
        }
    }

    fn handle(&mut self, command: Command) {
        match command {
            Command::Validate { request, reply } => reply.send(self.validate(request)),
            Command::Prepare { request, reply } => {
                let answer = self
                    .validate(request.clone())
                    .and_then(|()| self.lower(request));
                reply.send(answer);
            }
            Command::Classify {
                status,
                headers,
                body,
                reply,
            } => reply.send(self.classify(status, headers, body)),
            Command::Feed {
                key,
                create,
                event,
                reply,
            } => reply.send(self.feed(key, create, event)),
            Command::Finish { key, create, reply } => reply.send(self.finish(key, create)),
            Command::Drop { key } => {
                if let (Some(decoder), Some(live)) = (self.decoders.remove(&key), &mut self.live)
                    && decoder.resource_drop(&mut live.store).is_err()
                {
                    self.poison();
                }
            }
        }
    }

    fn validate(&mut self, request: Val) -> Result<(), Refusal> {
        let results = self.call(|exports| &exports.validate, &[request])?;
        match results.into_iter().next() {
            Some(Val::Result(Ok(_))) => Ok(()),
            Some(Val::Result(Err(Some(error)))) => Err(Refusal::Refused(provider_error(*error)?)),
            _ => Err(invalid(BAD_RESULT).into()),
        }
    }

    fn lower(&mut self, request: Val) -> Result<LoweredHttpRequest, Refusal> {
        let results = self.call(|exports| &exports.lower, &[request, connection_state()])?;
        match results.into_iter().next() {
            Some(Val::Result(Ok(Some(lowered)))) => Ok(lowered_request(*lowered)?),
            Some(Val::Result(Err(Some(error)))) => Err(Refusal::Refused(provider_error(*error)?)),
            _ => Err(invalid(BAD_RESULT).into()),
        }
    }

    fn classify(
        &mut self,
        status: u16,
        headers: Vec<(String, String)>,
        body: Vec<u8>,
    ) -> Result<ContractError, ModuleFailure> {
        let head = Val::Record(vec![
            ("status".to_owned(), Val::U16(status)),
            ("headers".to_owned(), headers_val(headers)),
        ]);
        let body = Val::List(body.into_iter().map(Val::U8).collect());
        let results = self.call(|exports| &exports.classify, &[head, body])?;
        match results.into_iter().next() {
            Some(error) => provider_error(error),
            None => Err(invalid(BAD_RESULT)),
        }
    }

    fn feed(
        &mut self,
        key: u64,
        create: bool,
        event: SseEvent,
    ) -> Result<Vec<StreamEvent>, ModuleFailure> {
        let decoder = self.decoder(key, create)?;
        let event = Val::Record(vec![
            (
                "name".to_owned(),
                Val::Option(event.event.map(|name| Box::new(Val::String(name)))),
            ),
            ("data".to_owned(), Val::String(event.data)),
        ]);
        let results = self.call(|exports| &exports.feed, &[Val::Resource(decoder), event])?;
        match results.into_iter().next() {
            Some(Val::List(events)) => events.into_iter().map(stream_event).collect(),
            _ => Err(invalid(BAD_RESULT)),
        }
    }

    fn finish(&mut self, key: u64, create: bool) -> Result<Outcome, ModuleFailure> {
        let decoder = self.decoder(key, create)?;
        let results = self.call(|exports| &exports.finish, &[Val::Resource(decoder)])?;
        match results.into_iter().next().map(stream_event) {
            Some(Ok(StreamEvent::Finished(outcome))) => Ok(outcome),
            Some(Ok(_)) => Err(invalid(NOT_TERMINAL)),
            Some(Err(failure)) => Err(failure),
            None => Err(invalid(BAD_RESULT)),
        }
    }

    /// Decoder `key` of the live instance, constructed when `create`.
    fn decoder(&mut self, key: u64, create: bool) -> Result<ResourceAny, ModuleFailure> {
        if !create {
            return self
                .decoders
                .get(&key)
                .copied()
                .ok_or_else(|| ModuleFailure::Trap(DECODER_LOST.to_owned()));
        }
        let results = self.call(|exports| &exports.new_decoder, &[])?;
        match results.into_iter().next() {
            Some(Val::Resource(decoder)) => {
                self.decoders.insert(key, decoder);
                Ok(decoder)
            }
            _ => Err(invalid(BAD_RESULT)),
        }
    }

    /// Calls one export on the live instance, building a configured one first when the
    /// last call failed. A failed call drops the instance and its decoders.
    fn call(
        &mut self,
        export: impl Fn(&Exports) -> &ComponentExportIndex,
        params: &[Val],
    ) -> Result<Vec<Val>, ModuleFailure> {
        if self.live.is_none() {
            match self.instantiate()? {
                Ok(live) => self.live = Some(live),
                Err(_) => return Err(ModuleFailure::Trap(REBUILD_REFUSED.to_owned())),
            }
        }
        let live = self.live.as_mut().expect("an instance was just built");
        let result = invoke(
            live,
            &self.epochs,
            self.limits,
            export(&self.exports),
            params,
        );
        if result.is_err() {
            self.poison();
        }
        result
    }

    fn poison(&mut self) {
        self.live = None;
        self.decoders.clear();
    }

    /// A fresh instance with `configure` called: the outer error is a failure, the inner
    /// one `configure`'s refusal.
    fn instantiate(&self) -> Result<Result<Live, ContractError>, ModuleFailure> {
        let mut store = Store::new(&self.engine, ());
        // Instantiation runs guest code too, under the same limits as a call.
        arm(&mut store, &self.epochs, self.limits)?;
        let instance = self
            .pre
            .instantiate(&mut store)
            .map_err(|error| failure(&error))?;
        let mut live = Live { store, instance };
        let results = invoke(
            &mut live,
            &self.epochs,
            self.limits,
            &self.exports.configure,
            std::slice::from_ref(&self.settings),
        )?;
        match results.into_iter().next() {
            Some(Val::Result(Ok(_))) => Ok(Ok(live)),
            Some(Val::Result(Err(Some(error)))) => Ok(Err(provider_error(*error)?)),
            _ => Err(invalid(BAD_RESULT)),
        }
    }
}

/// Gives the Store the limits of one call: full fuel, and a deadline on the engine's epoch
/// clock that the epoch callback enforces while the guest runs.
fn arm(
    store: &mut Store<()>,
    epochs: &Epochs,
    limits: ExecutionLimits,
) -> Result<(), ModuleFailure> {
    store
        .set_fuel(limits.fuel)
        .map_err(|error| failure(&error))?;
    let clock = epochs.subscribe();
    let ticks = u64::try_from(limits.deadline.as_nanos() / EPOCH_TICK.as_nanos())
        .unwrap_or(u64::MAX)
        .max(1);
    let deadline = clock.borrow().saturating_add(ticks);
    store.set_epoch_deadline(1);
    store.epoch_deadline_callback(move |_| {
        if *clock.borrow() >= deadline {
            return Err(wasmtime::Error::new(DeadlineStop));
        }
        Ok(UpdateDeadline::Continue(1))
    });
    Ok(())
}

/// One synchronous call of `export` under the limits.
fn invoke(
    live: &mut Live,
    epochs: &Epochs,
    limits: ExecutionLimits,
    export: &ComponentExportIndex,
    params: &[Val],
) -> Result<Vec<Val>, ModuleFailure> {
    let store = &mut live.store;
    arm(store, epochs, limits)?;
    let func = live
        .instance
        .get_func(&mut *store, export)
        .ok_or_else(|| ModuleFailure::Trap("the component lost an export".to_owned()))?;
    let mut results = vec![Val::Bool(false); func.ty(&*store).results().len()];
    func.call(&mut *store, params, &mut results)
        .map_err(|error| failure(&error))?;
    Ok(results)
}

/// Maps a wasmtime error into the closed failure shapes; the text is wasmtime's or this
/// runtime's, never guest memory.
fn failure(error: &wasmtime::Error) -> ModuleFailure {
    if error.downcast_ref::<DeadlineStop>().is_some() {
        return ModuleFailure::DeadlineExceeded;
    }
    match error.downcast_ref::<Trap>() {
        Some(Trap::OutOfFuel) => ModuleFailure::FuelExhausted,
        Some(Trap::Interrupt) => ModuleFailure::DeadlineExceeded,
        Some(trap) => ModuleFailure::Trap(trap.to_string()),
        None => ModuleFailure::Trap(format!("{error:#}")),
    }
}

fn invalid(message: &str) -> ModuleFailure {
    ModuleFailure::InvalidOutput(message.to_owned())
}

/// A `provider-error` value: protocol JSON of one of the closed kinds.
fn provider_error(value: Val) -> Result<ContractError, ModuleFailure> {
    match value {
        Val::String(text) => serde_json::from_str::<WireProviderError>(&text)
            .map(ContractError::from)
            .map_err(|_| invalid(BAD_ERROR)),
        _ => Err(invalid(BAD_ERROR)),
    }
}

/// A `stream-event` value.
fn stream_event(value: Val) -> Result<StreamEvent, ModuleFailure> {
    let Val::String(text) = value else {
        return Err(invalid(BAD_EVENT));
    };
    serde_json::from_str::<WireStreamEvent>(&text)
        .ok()
        .and_then(|event| StreamEvent::try_from(event).ok())
        .ok_or_else(|| invalid(BAD_EVENT))
}

fn field<'v>(fields: &'v [(String, Val)], key: &str) -> Option<&'v Val> {
    fields
        .iter()
        .find(|(name, _)| name == key)
        .map(|(_, value)| value)
}

/// A `lowered-request`: only its `http` case is sent here.
fn lowered_request(value: Val) -> Result<LoweredHttpRequest, ModuleFailure> {
    let fields = match value {
        Val::Variant(case, Some(request)) if case == "http" => match *request {
            Val::Record(fields) => fields,
            _ => return Err(invalid(BAD_LOWERED)),
        },
        Val::Variant(case, _) if case == "websocket" => return Err(invalid(WEBSOCKET_LOWERED)),
        _ => return Err(invalid(BAD_LOWERED)),
    };
    let path = match field(&fields, "path") {
        Some(Val::String(path)) => path.clone(),
        _ => return Err(invalid(BAD_LOWERED)),
    };
    let headers = match field(&fields, "headers") {
        Some(Val::List(headers)) => headers
            .iter()
            .map(|header| match header {
                Val::Tuple(pair) => match pair.as_slice() {
                    [Val::String(name), Val::String(value)] => Ok((name.clone(), value.clone())),
                    _ => Err(invalid(BAD_LOWERED)),
                },
                _ => Err(invalid(BAD_LOWERED)),
            })
            .collect::<Result<Vec<_>, _>>()?,
        _ => return Err(invalid(BAD_LOWERED)),
    };
    let credential = match field(&fields, "credential") {
        Some(Val::Record(credential)) => {
            let scheme = match field(credential, "scheme") {
                Some(Val::Enum(scheme)) if scheme == "bearer" => CredentialScheme::Bearer,
                _ => return Err(invalid(BAD_LOWERED)),
            };
            let account_id_header = match field(credential, "account-id-header") {
                Some(Val::Option(None)) => None,
                Some(Val::Option(Some(name))) => match name.as_ref() {
                    Val::String(name) => Some(name.clone()),
                    _ => return Err(invalid(BAD_LOWERED)),
                },
                _ => return Err(invalid(BAD_LOWERED)),
            };
            CredentialUse {
                scheme,
                account_id_header,
            }
        }
        _ => return Err(invalid(BAD_LOWERED)),
    };
    let body = match field(&fields, "body") {
        Some(Val::List(bytes)) => bytes
            .iter()
            .map(|byte| match byte {
                Val::U8(byte) => Ok(*byte),
                _ => Err(invalid(BAD_LOWERED)),
            })
            .collect::<Result<Vec<_>, _>>()?,
        _ => return Err(invalid(BAD_LOWERED)),
    };
    Ok(LoweredHttpRequest {
        path,
        headers,
        credential,
        body,
    })
}

fn settings_val(settings: ProviderSettings) -> Val {
    Val::Record(vec![
        (
            "origin-route".to_owned(),
            Val::String(settings.origin_route),
        ),
        ("endpoint".to_owned(), Val::String(settings.endpoint)),
        ("model".to_owned(), Val::String(settings.model)),
        ("wire-model".to_owned(), Val::String(settings.wire_model)),
        (
            "adapter-settings".to_owned(),
            Val::String(settings.adapter_settings.to_string()),
        ),
    ])
}

/// A `provider-request`: history and options as their protocol JSON, tools as records.
fn request_val(request: &ProviderRequest) -> Result<Val, ModuleFailure> {
    let unserializable = |_| invalid(UNSERIALIZABLE);
    let history = request
        .history
        .iter()
        .map(|item| serde_json::to_string(&WireItem::from(item.clone())).map(Val::String))
        .collect::<Result<Vec<_>, _>>()
        .map_err(unserializable)?;
    let tools = request
        .tools
        .iter()
        .map(declaration_val)
        .collect::<Result<Vec<_>, _>>()
        .map_err(unserializable)?;
    let options = serde_json::to_string(&WireModelOptions::from(request.options.clone()))
        .map_err(unserializable)?;
    Ok(Val::Record(vec![
        (
            "system-prompt".to_owned(),
            Val::String(request.system_prompt.clone()),
        ),
        ("history".to_owned(), Val::List(history)),
        ("tools".to_owned(), Val::List(tools)),
        ("options".to_owned(), Val::String(options)),
    ]))
}

fn declaration_val(declaration: &ToolDeclaration) -> Result<Val, serde_json::Error> {
    let kind = match &declaration.kind {
        DeclarationKind::Function { input_schema } => Val::Variant(
            "function".to_owned(),
            Some(Box::new(Val::String(serde_json::to_string(input_schema)?))),
        ),
        DeclarationKind::Freeform { grammar } => Val::Variant(
            "freeform".to_owned(),
            Some(Box::new(Val::Option(grammar.as_ref().map(|grammar| {
                Box::new(Val::Record(vec![
                    ("syntax".to_owned(), Val::String(grammar.syntax.clone())),
                    (
                        "definition".to_owned(),
                        Val::String(grammar.definition.clone()),
                    ),
                ]))
            })))),
        ),
    };
    Ok(Val::Record(vec![
        ("name".to_owned(), Val::String(declaration.name.clone())),
        (
            "description".to_owned(),
            Val::String(declaration.description.clone()),
        ),
        ("kind".to_owned(), kind),
    ]))
}

/// The broker's report on `lower`: this broker keeps no WebSocket connection (S5's), so
/// none is open and no attempt failed over one.
fn connection_state() -> Val {
    Val::Record(vec![
        ("open".to_owned(), Val::Bool(false)),
        ("last-clean-response".to_owned(), Val::Option(None)),
        ("failed-before-output".to_owned(), Val::Bool(false)),
    ])
}

fn headers_val(headers: Vec<(String, String)>) -> Val {
    Val::List(
        headers
            .into_iter()
            .map(|(name, value)| Val::Tuple(vec![Val::String(name), Val::String(value)]))
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use p1_contracts::{Origin, StopReason};

    use super::*;

    #[test]
    fn the_split_is_the_last_path_segment_and_never_the_host() {
        assert_eq!(
            split_endpoint("https://opencode.ai/zen/go/v1/chat/completions"),
            Some((
                "https://opencode.ai/zen/go/v1/chat",
                "/completions".to_owned()
            ))
        );
        assert_eq!(
            split_endpoint("https://chatgpt.com/backend-api/codex/responses/"),
            Some((
                "https://chatgpt.com/backend-api/codex",
                "/responses".to_owned()
            ))
        );
        assert_eq!(
            split_endpoint("https://api.anthropic.com/v1"),
            Some(("https://api.anthropic.com", "/v1".to_owned()))
        );
        for endpoint in ["https://api.anthropic.com", "https://api.anthropic.com/"] {
            assert_eq!(split_endpoint(endpoint), None, "{endpoint}");
        }
    }

    fn finished() -> StreamEvent {
        StreamEvent::Finished(Outcome::Failed(ContractError::new(
            ProviderErrorKind::Transport,
            "ended",
        )))
    }

    #[test]
    fn events_after_the_terminal_one_are_dropped() {
        let text = StreamEvent::TextDelta {
            block: 0,
            text: "hi".to_owned(),
        };
        let (events, ended) =
            through_terminal(vec![text.clone(), finished(), text.clone(), finished()]);
        assert!(ended);
        assert_eq!(events, vec![text.clone(), finished()]);
        let (events, ended) = through_terminal(vec![text.clone()]);
        assert!(!ended);
        assert_eq!(events, vec![text]);
    }

    #[test]
    fn a_module_failure_maps_to_the_closed_kinds() {
        assert_eq!(module_error(stopped()).kind, ProviderErrorKind::Protocol);
        assert_eq!(
            module_error(ModuleFailure::DeadlineExceeded).kind,
            ProviderErrorKind::Transport
        );
        assert_eq!(
            module_error(ModuleFailure::FuelExhausted).kind,
            ProviderErrorKind::Protocol
        );
        assert_eq!(
            failure(&wasmtime::Error::new(DeadlineStop)),
            ModuleFailure::DeadlineExceeded
        );
    }

    #[test]
    fn a_stream_event_is_protocol_json_or_invalid_output() {
        let event = StreamEvent::Finished(Outcome::Completed(p1_contracts::CompletedResponse {
            item: p1_contracts::AssistantItem {
                origin: Origin {
                    route: "r".to_owned(),
                    model: "m".to_owned(),
                },
                blocks: Vec::new(),
            },
            stop: StopReason::EndTurn,
            usage: None,
        }));
        let text = serde_json::to_string(&WireStreamEvent::from(event.clone())).unwrap();
        assert_eq!(stream_event(Val::String(text)), Ok(event));
        let secret = "not json SECRET-RESPONSE-TEXT".to_owned();
        match stream_event(Val::String(secret)) {
            Err(ModuleFailure::InvalidOutput(message)) => {
                assert!(!message.contains("SECRET"), "{message}");
            }
            other => panic!("expected invalid output, got {other:?}"),
        }
    }

    #[test]
    fn a_lowered_websocket_request_is_refused_and_http_is_read() {
        let http = Val::Variant(
            "http".to_owned(),
            Some(Box::new(Val::Record(vec![
                ("method".to_owned(), Val::Enum("post".to_owned())),
                ("path".to_owned(), Val::String("/v1/messages".to_owned())),
                (
                    "headers".to_owned(),
                    headers_val(vec![("x-a".to_owned(), "1".to_owned())]),
                ),
                (
                    "credential".to_owned(),
                    Val::Record(vec![
                        ("scheme".to_owned(), Val::Enum("bearer".to_owned())),
                        ("account-id-header".to_owned(), Val::Option(None)),
                    ]),
                ),
                (
                    "body".to_owned(),
                    Val::List(vec![Val::U8(b'{'), Val::U8(b'}')]),
                ),
            ]))),
        );
        let lowered = lowered_request(http).unwrap();
        assert_eq!(lowered.path, "/v1/messages");
        assert_eq!(lowered.headers, vec![("x-a".to_owned(), "1".to_owned())]);
        assert_eq!(lowered.body, b"{}".to_vec());
        assert_eq!(lowered.credential.account_id_header, None);
        let websocket = Val::Variant("websocket".to_owned(), None);
        assert_eq!(
            lowered_request(websocket).unwrap_err(),
            invalid(WEBSOCKET_LOWERED)
        );
    }
}
