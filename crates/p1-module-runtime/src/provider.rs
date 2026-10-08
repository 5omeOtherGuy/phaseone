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
//! a task of the caller's own current-thread runtime would never be answered. Synchronous
//! callers wait on their thread; `stream` awaits Prepare/Lower replies without blocking
//! Tokio's worker, and cancellation drops its reply receiver (ADR-0015).
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
//!   refusal is the setup error a native adapter returns. A lowered HTTP request goes through
//!   [`broker_drive`], which keeps retry, backoff, the one refresh after 401/403, the read
//!   bounds and cancellation; each framed SSE event goes to a fresh decoder per attempt, and
//!   a non-2xx response to `classify`. The broker applies its own status policy to the kind
//!   `classify` returns, as it does for a native parser (ADR-0046, ADR-0062).
//! - WebSocket (ADR-0078 §1–§2): a provider given a session ([`WasmProvider::with_websocket`])
//!   leases it for each request — waiting while another request's response holds it, since the
//!   frozen `connection-state` has no fact for a busy session and a second socket is never
//!   opened — and passes the session's facts to `lower`. A lowered `websocket-send` goes
//!   through [`ws_drive`], which keeps the connection, the handshake with the credential, the
//!   read bounds, §5's retry and refresh, and feeds every text frame to a fresh decoder per
//!   attempt as an event with no name; for each retry before any output it calls `lower`
//!   again, and an HTTP answer then is the fallback it sends through [`broker_drive`]. A
//!   decoder's `response-id` goes back to the session with a cleanly completed response.
//! - Exactly one `Finished` per stream holds host side too: whatever a decoder returns after
//!   its terminal event is dropped, and the decoder with it.
//! - A completed response is checked before it leaves the adapter, as the native parsers check
//!   theirs: every tool call has a name and an id no other call of the item has, and the item
//!   and every replay it carries name the configured origin (`origin-route`, `wire-model`).
//!   `describe` must name that origin too. A guest cannot attribute output to another route,
//!   whose replay decoder would then read it as its own, and the core never sees two calls it
//!   cannot tell apart by id. A response that fails is invalid output, so `Protocol`.
//! - A module failure maps as `ModuleFailure::into_provider_outcome` fixes it: a trap, fuel
//!   or invalid output is `Protocol` with a message of this runtime, never guest text read
//!   as a value; a deadline is `Transport`.
//! - Endpoint and path (ADR-0086): a route whose endpoint is a full request URL gets a route
//!   authority without its last path segment exactly when the component lowered that
//!   segment as the path, so the request URL is the endpoint unchanged. The split lives here
//!   because the component crates are guest-only and cannot be a host dependency; it is the
//!   same rule as the components' `http_target`.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, mpsc};
use std::thread;

use p1_contracts::serde_json;
use p1_contracts::{
    AssistantBlock, BoxFuture, CancellationToken, DeclarationKind, Origin, Outcome, Provider,
    ProviderError as ContractError, ProviderErrorKind, ProviderRequest, ProviderStream,
    RouteDescription, StreamEvent, ToolDeclaration,
};
use p1_module_protocol::{
    ModuleFailure, WireItem, WireModelOptions, WireProviderError, WireRouteDescription,
    WireStreamEvent,
};
use p1_provider_http::ws::WsConnector;
use p1_provider_http::ws_session::{Clock, ConnectionState, WsHead, WsLease, WsSend, WsSession};
use p1_provider_http::{
    CredentialScheme, CredentialSource, CredentialUse, LoweredHttpRequest, ResponseParser,
    RetryPolicy, RouteAuthority, SseEvent, Transport, WsDriveRequest, WsLowered, broker_drive,
    cancelled_stream, ws_drive, ws_lease,
};
use thiserror::Error;
use wasmtime::component::{
    Component, ComponentExportIndex, Instance, InstancePre, Linker, ResourceAny, Val,
};
use wasmtime::{Engine, Store, Trap};

use crate::executor::{BareStore, ExecutionLimits, module_store};
use crate::loader::{EPOCH_TICK, Epochs, LoadedModule, ModuleKind, interface_import};
use crate::restricted::Restricted;

/// The capabilities a provider component may be granted and this adapter links: the
/// transport vocabulary, type-only. The rest of the provider allocation (`control`, `clock`,
/// `random`, `notices`) would need asynchronous host functions in the one synchronous Store,
/// so a manifest granting one is refused at construction until a provider needs it.
pub const PROVIDER_LINKED: [&str; 3] = ["http", "websocket", "credential-control"];

/// The capabilities a provider component cannot work without: the transport it lowers for
/// (`http`) and the credential placement it names (`credential-control`). A component granted
/// neither would configure a provider that can never send, so activation refuses it before
/// the first turn rather than building it (ADR-0086, S4.9).
pub const PROVIDER_REQUIRED: [&str; 2] = ["http", "credential-control"];

/// The export instance of the decoder resource.
const DECODING: &str = "p1:module/decoding@1.0.0";

// Constant messages: a value the module returned is never quoted back, since it may carry
// prompt or response text.
const BAD_EVENT: &str = "a decoded stream event is not protocol stream-event JSON";
const BAD_ERROR: &str = "a provider error is not protocol provider-error JSON";
const BAD_DESCRIPTION: &str = "describe did not return a protocol route description";
const FOREIGN_DESCRIPTION: &str =
    "describe named another origin than the configured route and wire model";
const FOREIGN_ORIGIN: &str = "a completed response or its replay names another origin than the configured route and wire model";
const BAD_TOOL_IDENTITY: &str =
    "a completed response holds a tool call with an empty name or id, or an id used twice";
const BAD_LOWERED: &str = "lower did not return a request of the transport interfaces";
const WEBSOCKET_LOWERED: &str =
    "lower chose WebSocket, but this provider has no WebSocket session; nothing was sent";
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
    /// The manifest does not grant a capability every provider needs.
    #[error(
        "module {name}: capability {capability} is not granted, and a provider cannot work without it"
    )]
    MissingCapability {
        /// The module.
        name: String,
        /// The capability.
        capability: &'static str,
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
    retry: RetryPolicy,
    /// This instance's one WebSocket connection, when the host gave it one.
    websocket: Option<WsSession>,
}

impl WasmProvider {
    /// Builds the provider for `module` configured with `settings`. The broker sends every
    /// request on `transport` to `settings.endpoint`, authenticated from `credentials`; the
    /// component names neither. Runs `describe` on the restricted configured instance,
    /// then starts the executor thread with its own configured instance; needs no Tokio runtime.
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
        if let Some(capability) = missing_required(module.capabilities()) {
            return Err(ProviderError::MissingCapability { name, capability });
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
        let mut linker: Linker<BareStore> = Linker::new(&module.engine);
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

        // Every value a guest attributes to an origin must name this one (G1-05).
        let origin = Origin {
            route: settings.origin_route.clone(),
            model: settings.wire_model.clone(),
        };
        let configured_settings = settings_val(settings);
        let restricted = Restricted::new(&module.engine, &module.epochs, &module.component)
            .map_err(|error| instantiate(format!("{error:#}")))?;
        let configured = restricted.call("configure", std::slice::from_ref(&configured_settings));
        let configured = configured.as_deref().and_then(|values| match values {
            [Val::Result(Ok(_))] => Some(Ok(())),
            [Val::Result(Err(Some(error)))] => Some(Err(
                provider_error((**error).clone()).unwrap_or_else(module_error)
            )),
            _ => None,
        });
        match configured {
            Some(Ok(())) => {}
            Some(Err(reason)) => return Err(ProviderError::Configure { name, reason }),
            None => {
                return Err(ProviderError::Configure {
                    name,
                    reason: ContractError::new(
                        ProviderErrorKind::Protocol,
                        "restricted configure failed",
                    ),
                });
            }
        }
        let description = restricted
            .call("describe", &[])
            .and_then(|values| values.into_iter().next())
            .and_then(|value| match value {
                Val::String(text) => serde_json::from_str::<WireRouteDescription>(&text).ok(),
                _ => None,
            })
            .map(RouteDescription::from)
            .ok_or(BAD_DESCRIPTION)
            .and_then(|description| bound_description(description, &origin))
            .map_err(|reason| ProviderError::Describe {
                name: name.clone(),
                reason: reason.to_owned(),
            })?;

        let mut machine = Machine {
            engine: module.engine.clone(),
            pre,
            epochs: module.epochs.clone(),
            limits,
            settings: configured_settings,
            origin,
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
        match started.recv() {
            Ok(Ok(())) => {}
            Ok(Err(Start::Instantiate(reason))) => return Err(instantiate(reason)),
            Ok(Err(Start::Configure(reason))) => {
                return Err(ProviderError::Configure { name, reason });
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
            retry: RetryPolicy::default(),
            websocket: None,
        })
    }

    /// Selects the native broker policy without changing the component's wire settings.
    pub fn with_retry(mut self, retry: RetryPolicy) -> Self {
        self.retry = retry;
        self
    }

    /// Gives the provider its WebSocket session: the one route-bound connection its component
    /// may lower a request onto (ADR-0078 §1), opened through `connector` and reused under the
    /// bounds of `docs/design/websocket.md` §4 on `clock`. Composing it opens no socket; only a
    /// request the component lowers to WebSocket connects. Without a session a WebSocket
    /// lowering is refused and nothing is sent.
    pub fn with_websocket(mut self, connector: Arc<dyn WsConnector>, clock: Clock) -> Self {
        self.websocket = Some(WsSession::new(connector, clock));
        self
    }

    /// The route authority a lowered `path` goes to (ADR-0086).
    fn authority_for(&self, path: &str) -> &RouteAuthority {
        authority_for(&self.authority, self.split.as_ref(), path)
    }
}

/// `authority`, or the split one when `path` is the endpoint's own last segment: the request
/// URL is then the endpoint unchanged.
fn authority_for<'a>(
    authority: &'a RouteAuthority,
    split: Option<&'a (String, RouteAuthority)>,
    path: &str,
) -> &'a RouteAuthority {
    match split {
        Some((split_path, split_authority)) if split_path == path => split_authority,
        _ => authority,
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
            let mut lease = match &self.websocket {
                Some(session) => match ws_lease(session, &cancel).await {
                    Ok(lease) => Some(lease),
                    Err(cancelled) => return Ok(cancelled),
                },
                None => None,
            };
            let connection = lease.as_mut().map(WsLease::state).unwrap_or_default();
            let lowered = match self
                .executor
                .prepare(request.clone(), connection, &cancel)
                .await
            {
                Ok(lowered) => lowered,
                // A token that fired before `Prepare` answered is the contract's own
                // ending: the caller gets a stream that settles as cancelled, not a setup
                // error. Nothing was lowered and no connection was opened, so the lease is
                // released by returning.
                Err(Refusal::Failed(ModuleFailure::Cancelled)) => {
                    return Ok(cancelled_stream());
                }
                Err(refusal) => {
                    // A failed call poisoned the instance; the next request rebuilds it and
                    // must not reuse a connection opened for this one (ADR-0078 §3).
                    if let (Refusal::Failed(_), Some(lease)) = (&refusal, lease.as_mut()) {
                        lease.drop_session_connection();
                    }
                    return Err(refusal.into_error());
                }
            };
            let send = match lowered {
                WsLowered::Http(lowered) => {
                    // No connection is needed: the session is free for another request.
                    drop(lease);
                    let executor = self.executor.clone();
                    return broker_drive(
                        self.authority_for(&lowered.path),
                        self.transport.clone(),
                        &lowered,
                        Box::new(move || Box::new(ComponentParser::new(executor.clone()))),
                        self.retry,
                        cancel,
                    );
                }
                WsLowered::WebSocket(send) => send,
            };
            let Some(lease) = lease else {
                return Err(module_error(invalid(WEBSOCKET_LOWERED)));
            };
            let lower = self.executor.clone();
            let retry_cancel = cancel.clone();
            let parsers = self.executor.clone();
            let (authority, split) = (self.authority.clone(), self.split.clone());
            Ok(ws_drive(WsDriveRequest {
                lease,
                send,
                lower: Box::new(move |connection| {
                    let lower = lower.clone();
                    let request = request.clone();
                    let cancel = retry_cancel.clone();
                    Box::pin(async move {
                        lower
                            .lower(request, connection, &cancel)
                            .await
                            .map_err(Refusal::into_error)
                    })
                }),
                authority: Box::new(move |path| {
                    authority_for(&authority, split.as_ref(), path).clone()
                }),
                transport: self.transport.clone(),
                new_parser: Arc::new(move || Box::new(ComponentParser::new(parsers.clone()))),
                retry: self.retry,
                cancel,
            }))
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

/// The capability of [`PROVIDER_REQUIRED`] that `granted` does not have, or `None`: a
/// component that cannot reach the transport, or cannot name where a credential goes, would
/// configure a provider that can never send a request.
fn missing_required(granted: &[String]) -> Option<&'static str> {
    PROVIDER_REQUIRED
        .into_iter()
        .find(|required| !granted.iter().any(|granted| granted == required))
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

/// What one decoder `feed` answers: the events it produced and, when this attempt's
/// response completed, the id a continuation may name (`decoding.decoder.response-id`).
type FeedReply = (Vec<StreamEvent>, DecodedId);

/// What one decoder `finish` answers: this attempt's terminal outcome and, when that
/// completed the response, the id a continuation may name.
type FinishReply = (Outcome, DecodedId);

/// What `decoding.decoder.response-id` answered for a completed response.
#[derive(Debug, Clone, PartialEq, Eq)]
enum DecodedId {
    /// No completed response yet, or the decoder named no id.
    Unknown,
    Known(String),
    /// Asking failed: the instance was dropped with its decoders, and so must the
    /// connection it used be (ADR-0078 §3).
    InstanceLost,
}

enum Command {
    Validate {
        request: Val,
        reply: Reply<Result<(), Refusal>>,
    },
    /// `validate`, then `lower` when it passed.
    Prepare {
        request: Val,
        connection: ConnectionState,
        reply: tokio::sync::oneshot::Sender<Result<WsLowered, Refusal>>,
    },
    /// `lower` again, for a retry after a WebSocket failure before any output.
    Lower {
        request: Val,
        connection: ConnectionState,
        reply: tokio::sync::oneshot::Sender<Result<WsLowered, Refusal>>,
    },
    Classify {
        status: u16,
        headers: Vec<(String, String)>,
        body: Vec<u8>,
        reply: Reply<Result<ContractError, ModuleFailure>>,
    },
    /// Feed one event to decoder `key`, constructing it first when `create`. A completed
    /// response comes back with the decoder's response id.
    Feed {
        key: u64,
        create: bool,
        event: SseEvent,
        reply: Reply<Result<FeedReply, ModuleFailure>>,
    },
    Finish {
        key: u64,
        create: bool,
        reply: Reply<Result<FinishReply, ModuleFailure>>,
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

    async fn prepare(
        &self,
        request: Val,
        connection: ConnectionState,
        cancel: &CancellationToken,
    ) -> Result<WsLowered, Refusal> {
        let (reply, answer) = tokio::sync::oneshot::channel();
        self.commands
            .send(Command::Prepare {
                request,
                connection,
                reply,
            })
            .map_err(|_| Refusal::Failed(stopped()))?;
        await_reply(answer, cancel).await
    }

    async fn lower(
        &self,
        request: Val,
        connection: ConnectionState,
        cancel: &CancellationToken,
    ) -> Result<WsLowered, Refusal> {
        let (reply, answer) = tokio::sync::oneshot::channel();
        self.commands
            .send(Command::Lower {
                request,
                connection,
                reply,
            })
            .map_err(|_| Refusal::Failed(stopped()))?;
        await_reply(answer, cancel).await
    }

    fn drop_decoder(&self, key: u64) {
        let _ = self.commands.send(Command::Drop { key });
    }
}

/// Await a thread-owned guest call without blocking the caller's runtime, observing
/// cancellation: whichever settles first ends the wait. The reply arrives on the waker the
/// executor thread signals, so the runtime parks instead of polling in a loop.
///
/// A blocked wait is invisible to the runtime, so a paused test clock would otherwise
/// auto-advance an enclosing `timeout` to its deadline before the external reply lands. Tokio
/// suspends that auto-advance only while a blocking task is outstanding, so a guard task is
/// spawned for the duration of the wait. It is detached, never awaited: its completion does
/// not depend on a blocking thread being free, so a saturated blocking pool cannot stall
/// provider setup. Dropping `stop` ends it as soon as the wait does, whether on the reply, on
/// cancellation or on the executor's death.
async fn await_reply<T, F>(reply: F, cancel: &CancellationToken) -> Result<T, Refusal>
where
    F: std::future::Future<
            Output = Result<Result<T, Refusal>, tokio::sync::oneshot::error::RecvError>,
        >,
{
    tokio::pin!(reply);
    let (stop, inhibited) = tokio::sync::oneshot::channel::<()>();
    let _guard = tokio::task::spawn_blocking(move || {
        let _ = inhibited.blocking_recv();
    });
    let result = tokio::select! {
        biased;
        () = cancel.cancelled() => Err(Refusal::Failed(ModuleFailure::Cancelled)),
        reply = &mut reply => match reply {
            Ok(value) => value,
            Err(_) => Err(Refusal::Failed(stopped())),
        },
    };
    drop(stop);
    result
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
    /// The response id of a response the decoder completed (`decoder.response-id`).
    response_id: DecodedId,
}

impl ComponentParser {
    fn new(executor: ExecutorHandle) -> Self {
        let key = executor.decoders.fetch_add(1, Ordering::Relaxed);
        Self {
            executor,
            key,
            created: false,
            terminated: false,
            response_id: DecodedId::Unknown,
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
            Ok((events, response_id)) => {
                self.response_id = response_id;
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
        match answer {
            Ok((outcome, response_id)) => {
                self.response_id = response_id;
                outcome
            }
            Err(failure) => failure.into_provider_outcome(),
        }
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

    fn response_id(&self) -> Option<String> {
        match &self.response_id {
            DecodedId::Known(id) => Some(id.clone()),
            DecodedId::Unknown | DecodedId::InstanceLost => None,
        }
    }

    fn instance_lost(&self) -> bool {
        self.response_id == DecodedId::InstanceLost
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
    validate: ComponentExportIndex,
    lower: ComponentExportIndex,
    classify: ComponentExportIndex,
    new_decoder: ComponentExportIndex,
    feed: ComponentExportIndex,
    finish: ComponentExportIndex,
    response_id: ComponentExportIndex,
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
            validate: top("validate")?,
            lower: top("lower")?,
            classify: top("classify")?,
            new_decoder: decoder("[constructor]decoder")?,
            feed: decoder("[method]decoder.feed")?,
            finish: decoder("[method]decoder.finish")?,
            response_id: decoder("[method]decoder.response-id")?,
        })
    }
}

/// Why the executor could not start the provider.
enum Start {
    Instantiate(String),
    Configure(ContractError),
}

/// The configured instance and the Store it lives in.
struct Live {
    store: Store<BareStore>,
    instance: Instance,
}

/// The executor: the one owner of the Store, the instance and the decoders.
struct Machine {
    engine: Engine,
    pre: InstancePre<BareStore>,
    epochs: Arc<Epochs>,
    limits: ExecutionLimits,
    settings: Val,
    /// The configured origin every completed response must name.
    origin: Origin,
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
    /// Builds and configures the executor's instance (description ran restricted already).
    fn start(&mut self) -> Result<(), Start> {
        let live = match self.instantiate() {
            Ok(Ok(live)) => live,
            Ok(Err(refused)) => return Err(Start::Configure(refused)),
            Err(failure) => return Err(Start::Instantiate(failure.to_string())),
        };
        self.live = Some(live);
        Ok(())
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
            Command::Prepare {
                request,
                connection,
                reply,
            } => {
                let answer = self
                    .validate(request.clone())
                    .and_then(|()| self.lower(request, &connection));
                let _ = reply.send(answer);
            }
            Command::Lower {
                request,
                connection,
                reply,
            } => {
                let _ = reply.send(self.lower(request, &connection));
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

    fn lower(&mut self, request: Val, connection: &ConnectionState) -> Result<WsLowered, Refusal> {
        let results = self.call(
            |exports| &exports.lower,
            &[request, connection_state(connection)],
        )?;
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
    ) -> Result<FeedReply, ModuleFailure> {
        let decoder = self.decoder(key, create)?;
        let event = Val::Record(vec![
            (
                "name".to_owned(),
                Val::Option(event.event.map(|name| Box::new(Val::String(name)))),
            ),
            ("data".to_owned(), Val::String(event.data)),
        ]);
        let results = self.call(|exports| &exports.feed, &[Val::Resource(decoder), event])?;
        let events = match results.into_iter().next() {
            Some(Val::List(events)) => events
                .into_iter()
                .map(|event| stream_event(event).and_then(|event| checked(event, &self.origin)))
                .collect::<Result<Vec<_>, _>>()?,
            _ => return Err(invalid(BAD_RESULT)),
        };
        let completed = events
            .iter()
            .any(|event| matches!(event, StreamEvent::Finished(Outcome::Completed(_))));
        let response_id = if completed {
            self.response_id(decoder)
        } else {
            DecodedId::Unknown
        };
        Ok((events, response_id))
    }

    fn finish(&mut self, key: u64, create: bool) -> Result<FinishReply, ModuleFailure> {
        let decoder = self.decoder(key, create)?;
        let results = self.call(|exports| &exports.finish, &[Val::Resource(decoder)])?;
        let outcome = match results
            .into_iter()
            .next()
            .map(|event| stream_event(event).and_then(|event| checked(event, &self.origin)))
        {
            Some(Ok(StreamEvent::Finished(outcome))) => outcome,
            Some(Ok(_)) => return Err(invalid(NOT_TERMINAL)),
            Some(Err(failure)) => return Err(failure),
            None => return Err(invalid(BAD_RESULT)),
        };
        let response_id = if matches!(outcome, Outcome::Completed(_)) {
            self.response_id(decoder)
        } else {
            DecodedId::Unknown
        };
        Ok((outcome, response_id))
    }

    /// The response id `decoder` saw. It only lets the next request continue this response on
    /// its connection, so a call that fails does not fail a response that completed; it
    /// reports the instance lost, since the failed call dropped it.
    fn response_id(&mut self, decoder: ResourceAny) -> DecodedId {
        let Ok(results) = self.call(|exports| &exports.response_id, &[Val::Resource(decoder)])
        else {
            return DecodedId::InstanceLost;
        };
        match results.into_iter().next() {
            Some(Val::Option(Some(id))) => match *id {
                Val::String(id) => DecodedId::Known(id),
                _ => DecodedId::Unknown,
            },
            _ => DecodedId::Unknown,
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
        let mut store = module_store(&self.engine, BareStore::default());
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
    store: &mut Store<BareStore>,
    epochs: &Epochs,
    limits: ExecutionLimits,
) -> Result<(), ModuleFailure> {
    store
        .set_fuel(limits.fuel)
        .map_err(|error| failure(&error))?;
    let ticks = u64::try_from(limits.deadline.as_nanos() / EPOCH_TICK.as_nanos())
        .unwrap_or(u64::MAX)
        .max(1);
    epochs.arm_deadline(store, ticks, || wasmtime::Error::new(DeadlineStop));
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
            .map(|wire| {
                let error = ContractError::from(wire);
                ContractError::new(error.kind, p1_redact::redact(&error.message).text)
            })
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

/// `event`, when it is not a completed response the native parsers would refuse: the
/// response's item and every replay in it name `origin`, and every tool call has a name and an
/// id of its own. Serde checks only the shape; these are the semantic rules of a completed
/// response, applied here because a guest's output is not trusted to follow them.
fn checked(event: StreamEvent, origin: &Origin) -> Result<StreamEvent, ModuleFailure> {
    let StreamEvent::Finished(Outcome::Completed(response)) = &event else {
        return Ok(event);
    };
    let item = &response.item;
    let foreign_replay = item.blocks.iter().any(|block| {
        matches!(block, AssistantBlock::Reasoning { replay: Some(replay), .. }
            if replay.origin != *origin)
    });
    if item.origin != *origin || foreign_replay {
        return Err(invalid(FOREIGN_ORIGIN));
    }
    let mut ids = HashSet::new();
    if item.tool_calls().any(|call| {
        call.call_id.is_empty() || call.name.is_empty() || !ids.insert(call.call_id.as_str())
    }) {
        return Err(invalid(BAD_TOOL_IDENTITY));
    }
    Ok(event)
}

/// `description`, when it names the configured `origin`: the route identity the host was
/// configured with, never one the guest chose.
fn bound_description(
    description: RouteDescription,
    origin: &Origin,
) -> Result<RouteDescription, &'static str> {
    if description.origin == *origin {
        Ok(description)
    } else {
        Err(FOREIGN_DESCRIPTION)
    }
}

fn field<'v>(fields: &'v [(String, Val)], key: &str) -> Option<&'v Val> {
    fields
        .iter()
        .find(|(name, _)| name == key)
        .map(|(_, value)| value)
}

/// A `lowered-request`: an `http-request` or a `websocket-send`.
fn lowered_request(value: Val) -> Result<WsLowered, ModuleFailure> {
    match value {
        Val::Variant(case, Some(request)) if case == "http" => match *request {
            Val::Record(fields) => http_request(&fields).map(WsLowered::Http),
            _ => Err(invalid(BAD_LOWERED)),
        },
        Val::Variant(case, Some(send)) if case == "websocket" => match *send {
            Val::Record(fields) => websocket_send(&fields).map(WsLowered::WebSocket),
            _ => Err(invalid(BAD_LOWERED)),
        },
        _ => Err(invalid(BAD_LOWERED)),
    }
}

fn http_request(fields: &[(String, Val)]) -> Result<LoweredHttpRequest, ModuleFailure> {
    let body = match field(fields, "body") {
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
        path: string(field(fields, "path"))?,
        headers: headers(field(fields, "headers"))?,
        credential: credential_use(field(fields, "credential"))?,
        body,
    })
}

fn websocket_send(fields: &[(String, Val)]) -> Result<WsSend, ModuleFailure> {
    let handshake = match field(fields, "handshake") {
        Some(Val::Option(None)) => None,
        Some(Val::Option(Some(head))) => match head.as_ref() {
            Val::Record(head) => Some(WsHead {
                path: string(field(head, "path"))?,
                headers: headers(field(head, "headers"))?,
                credential: credential_use(field(head, "credential"))?,
            }),
            _ => return Err(invalid(BAD_LOWERED)),
        },
        _ => return Err(invalid(BAD_LOWERED)),
    };
    Ok(WsSend {
        handshake,
        frame: string(field(fields, "frame"))?,
    })
}

fn string(value: Option<&Val>) -> Result<String, ModuleFailure> {
    match value {
        Some(Val::String(text)) => Ok(text.clone()),
        _ => Err(invalid(BAD_LOWERED)),
    }
}

fn headers(value: Option<&Val>) -> Result<Vec<(String, String)>, ModuleFailure> {
    match value {
        Some(Val::List(headers)) => headers
            .iter()
            .map(|header| match header {
                Val::Tuple(pair) => match pair.as_slice() {
                    [Val::String(name), Val::String(value)] => Ok((name.clone(), value.clone())),
                    _ => Err(invalid(BAD_LOWERED)),
                },
                _ => Err(invalid(BAD_LOWERED)),
            })
            .collect(),
        _ => Err(invalid(BAD_LOWERED)),
    }
}

fn credential_use(value: Option<&Val>) -> Result<CredentialUse, ModuleFailure> {
    let Some(Val::Record(credential)) = value else {
        return Err(invalid(BAD_LOWERED));
    };
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
    Ok(CredentialUse {
        scheme,
        account_id_header,
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

/// The broker's report on `lower`: the session's facts, or nothing open and nothing failed
/// for a provider without a session.
fn connection_state(state: &ConnectionState) -> Val {
    Val::Record(vec![
        ("open".to_owned(), Val::Bool(state.open)),
        (
            "last-clean-response".to_owned(),
            Val::Option(
                state
                    .last_clean_response
                    .clone()
                    .map(|id| Box::new(Val::String(id))),
            ),
        ),
        (
            "failed-before-output".to_owned(),
            Val::Bool(state.failed_before_output),
        ),
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
    fn provider_store_can_lift_more_than_three_megabytes_of_request_body() {
        let store = module_store(&crate::engine().unwrap(), BareStore::default());
        let size = 4 << 20;
        assert!(store.hostcall_fuel() >= size * std::mem::size_of::<Val>());
        let fields = vec![
            ("path".to_owned(), Val::String("/v1/messages".to_owned())),
            ("headers".to_owned(), headers_val(Vec::new())),
            ("credential".to_owned(), credential_val(None)),
            ("body".to_owned(), Val::List(vec![Val::U8(7); size])),
        ];
        // Dynamic lifting charges each byte as a Val; decoding retains the full body.
        assert_eq!(http_request(&fields).unwrap().body.len(), size);
    }

    /// A oneshot receiver that counts how often its future is polled. A parking wait polls
    /// it once and then waits for the waker; the removed busy-yield polled it once per
    /// reschedule, without bound.
    struct CountingReply {
        inner: tokio::sync::oneshot::Receiver<Result<u8, Refusal>>,
        polls: Arc<std::sync::atomic::AtomicUsize>,
    }

    impl std::future::Future for CountingReply {
        type Output = Result<Result<u8, Refusal>, tokio::sync::oneshot::error::RecvError>;

        fn poll(
            self: std::pin::Pin<&mut Self>,
            cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<Self::Output> {
            let this = self.get_mut();
            this.polls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            std::future::Future::poll(std::pin::Pin::new(&mut this.inner), cx)
        }
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn paused_current_thread_runtime_waits_for_external_provider_reply() {
        let cancel = CancellationToken::new();
        let (reply, answer) = tokio::sync::oneshot::channel();
        let thread = std::thread::spawn(move || {
            let _ = reply.send(Ok::<_, Refusal>(42));
        });
        // No timer is registered before the reply, so the paused clock has nothing to
        // advance to: the wait must end on the reply's waker, not on a poll loop.
        let result = await_reply(answer, &cancel).await;
        assert!(matches!(result, Ok(42)));
        thread.join().unwrap();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn awaiting_an_external_reply_parks_instead_of_polling() {
        let wait_cancel = CancellationToken::new();
        let (reply, answer) = tokio::sync::oneshot::channel();
        let polls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counted = CountingReply {
            inner: answer,
            polls: Arc::clone(&polls),
        };
        let waiter = tokio::spawn(async move { await_reply(counted, &wait_cancel).await });
        // Yield without a reply. A parking wait is polled once and then woken by the
        // sender; the busy-yield loop rescheduled itself on every yield, so this drove its
        // poll count up instead of leaving it at one.
        for _ in 0..64 {
            tokio::task::yield_now().await;
        }
        assert_eq!(
            polls.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "the wait repolled while no reply was available"
        );
        reply.send(Ok(42)).unwrap();
        assert!(matches!(waiter.await.unwrap(), Ok(42)));
    }

    /// A reply that is ready at once must return even while the blocking pool's only thread
    /// is busy: the paused-clock guard is detached, not awaited, so a saturated pool cannot
    /// stall provider setup.
    #[test]
    fn a_provider_reply_does_not_wait_for_the_blocking_guard() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .max_blocking_threads(1)
            .build()
            .expect("a runtime");
        let (started, started_at) = std::sync::mpsc::channel();
        let (release, release_from) = std::sync::mpsc::channel::<()>();
        runtime.block_on(async {
            // Hold the blocking pool's one thread, and do not proceed until it is held.
            let blocker = tokio::task::spawn_blocking(move || {
                let _ = started.send(());
                let _ = release_from.recv();
            });
            started_at.recv().expect("the blocker started");
            let (reply, answer) = tokio::sync::oneshot::channel();
            reply.send(Ok::<u8, Refusal>(7)).expect("the reply");
            let cancel = CancellationToken::new();
            let result = tokio::time::timeout(
                std::time::Duration::from_secs(5),
                await_reply(answer, &cancel),
            )
            .await
            .expect("await_reply waited for the queued blocking guard");
            assert!(matches!(result, Ok(7)));
            let _ = release.send(());
            let _ = blocker.await;
        });
    }

    #[test]
    fn guest_provider_error_text_is_masked() {
        let synthetic = format!("sk-{}", "a".repeat(24));
        let wire = WireProviderError::from(ContractError::new(
            ProviderErrorKind::RateLimited,
            format!("provider rejected {synthetic}"),
        ));
        let text = serde_json::to_string(&wire).unwrap();
        let result = provider_error(Val::String(text)).unwrap();
        assert_eq!(result.kind, ProviderErrorKind::RateLimited);
        assert!(!result.message.contains(&synthetic));
        assert!(result.message.contains("provider rejected"));
    }

    #[test]
    fn a_provider_component_without_a_required_capability_is_refused() {
        let granted = |names: &[&str]| {
            names
                .iter()
                .map(|name| (*name).to_owned())
                .collect::<Vec<_>>()
        };
        assert_eq!(
            missing_required(&granted(&["http", "credential-control"])),
            None
        );
        assert_eq!(
            missing_required(&granted(&["http", "websocket", "credential-control"])),
            None
        );
        assert_eq!(missing_required(&granted(&["websocket"])), Some("http"));
        assert_eq!(
            missing_required(&granted(&["http", "websocket"])),
            Some("credential-control")
        );
        assert_eq!(missing_required(&granted(&[])), Some("http"));
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

    fn credential_val(account_id_header: Option<&str>) -> Val {
        Val::Record(vec![
            ("scheme".to_owned(), Val::Enum("bearer".to_owned())),
            (
                "account-id-header".to_owned(),
                Val::Option(account_id_header.map(|name| Box::new(Val::String(name.to_owned())))),
            ),
        ])
    }

    #[test]
    fn a_lowered_request_of_either_transport_is_read() {
        let http = Val::Variant(
            "http".to_owned(),
            Some(Box::new(Val::Record(vec![
                ("method".to_owned(), Val::Enum("post".to_owned())),
                ("path".to_owned(), Val::String("/v1/messages".to_owned())),
                (
                    "headers".to_owned(),
                    headers_val(vec![("x-a".to_owned(), "1".to_owned())]),
                ),
                ("credential".to_owned(), credential_val(None)),
                (
                    "body".to_owned(),
                    Val::List(vec![Val::U8(b'{'), Val::U8(b'}')]),
                ),
            ]))),
        );
        let Ok(WsLowered::Http(lowered)) = lowered_request(http) else {
            panic!("an http request");
        };
        assert_eq!(lowered.path, "/v1/messages");
        assert_eq!(lowered.headers, vec![("x-a".to_owned(), "1".to_owned())]);
        assert_eq!(lowered.body, b"{}".to_vec());
        assert_eq!(lowered.credential.account_id_header, None);

        let send = |handshake: Option<Val>| {
            Val::Variant(
                "websocket".to_owned(),
                Some(Box::new(Val::Record(vec![
                    ("handshake".to_owned(), Val::Option(handshake.map(Box::new))),
                    ("frame".to_owned(), Val::String("{}".to_owned())),
                ]))),
            )
        };
        let head = Val::Record(vec![
            (
                "path".to_owned(),
                Val::String("/codex/responses".to_owned()),
            ),
            (
                "headers".to_owned(),
                headers_val(vec![("originator".to_owned(), "p1".to_owned())]),
            ),
            (
                "credential".to_owned(),
                credential_val(Some("chatgpt-account-id")),
            ),
        ]);
        let Ok(WsLowered::WebSocket(opened)) = lowered_request(send(Some(head))) else {
            panic!("a websocket send");
        };
        let head = opened.handshake.expect("a handshake head");
        assert_eq!(head.path, "/codex/responses");
        assert_eq!(
            head.headers,
            vec![("originator".to_owned(), "p1".to_owned())]
        );
        assert_eq!(
            head.credential.account_id_header.as_deref(),
            Some("chatgpt-account-id")
        );
        assert_eq!(opened.frame, "{}");
        let Ok(WsLowered::WebSocket(continued)) = lowered_request(send(None)) else {
            panic!("a websocket send on the open connection");
        };
        assert_eq!(continued.handshake, None);

        for malformed in [
            Val::Variant("websocket".to_owned(), None),
            Val::Variant("http".to_owned(), Some(Box::new(Val::Bool(true)))),
            Val::Variant("smtp".to_owned(), None),
        ] {
            assert_eq!(
                lowered_request(malformed).unwrap_err(),
                invalid(BAD_LOWERED)
            );
        }
    }

    fn origin() -> Origin {
        Origin {
            route: "openai-chat/glm-subscription".to_owned(),
            model: "glm-5.3".to_owned(),
        }
    }

    fn completed(item_origin: Origin, blocks: Vec<AssistantBlock>) -> StreamEvent {
        StreamEvent::Finished(Outcome::Completed(p1_contracts::CompletedResponse {
            item: p1_contracts::AssistantItem {
                origin: item_origin,
                blocks,
            },
            stop: StopReason::ToolUse,
            usage: None,
        }))
    }

    fn call(call_id: &str, name: &str) -> AssistantBlock {
        AssistantBlock::ToolCall(p1_contracts::ToolCall {
            call_id: call_id.to_owned(),
            name: name.to_owned(),
            input: p1_contracts::ToolInput::Json("{}".to_owned()),
        })
    }

    fn reasoning(replay_origin: Origin) -> AssistantBlock {
        AssistantBlock::Reasoning {
            text: "thought".to_owned(),
            replay: Some(p1_contracts::ReplayData {
                origin: replay_origin,
                version: 1,
                payload: serde_json::json!({"signature": "s"}),
            }),
        }
    }

    #[test]
    fn a_completed_response_with_an_empty_or_duplicate_tool_identity_is_invalid_output() {
        let valid = completed(origin(), vec![call("c1", "read"), call("c2", "read")]);
        assert_eq!(checked(valid.clone(), &origin()), Ok(valid));
        for blocks in [
            vec![call("", "read")],
            vec![call("c1", "")],
            vec![call("c1", "read"), call("c1", "write")],
        ] {
            assert_eq!(
                checked(completed(origin(), blocks), &origin()),
                Err(invalid(BAD_TOOL_IDENTITY))
            );
        }
        // The rule is the completed item's: a delta, a failure or a cancellation passes.
        let delta = StreamEvent::TextDelta {
            block: 0,
            text: "t".to_owned(),
        };
        assert_eq!(checked(delta.clone(), &origin()), Ok(delta));
        assert_eq!(checked(finished(), &origin()), Ok(finished()));
        // Invalid output is the closed `Protocol` failure, before anything is journalled.
        assert_eq!(
            module_error(invalid(BAD_TOOL_IDENTITY)).kind,
            ProviderErrorKind::Protocol
        );
    }

    #[test]
    fn a_completed_response_attributed_to_another_origin_is_invalid_output() {
        let other_route = Origin {
            route: "anthropic-messages/claude-subscription".to_owned(),
            ..origin()
        };
        let other_model = Origin {
            model: "glm-4".to_owned(),
            ..origin()
        };
        let own = completed(origin(), vec![reasoning(origin())]);
        assert_eq!(checked(own.clone(), &origin()), Ok(own));
        for foreign in [other_route, other_model] {
            assert_eq!(
                checked(completed(foreign.clone(), Vec::new()), &origin()),
                Err(invalid(FOREIGN_ORIGIN))
            );
            // The item names the configured origin, but its replay names another: a later
            // request on that route would read the payload as its own.
            assert_eq!(
                checked(completed(origin(), vec![reasoning(foreign)]), &origin()),
                Err(invalid(FOREIGN_ORIGIN))
            );
        }
    }

    #[test]
    fn a_description_of_another_origin_is_refused() {
        let description = |origin: Origin| RouteDescription {
            origin,
            supports_freeform_tools: false,
            mandatory_prompt_prefix: None,
            reports_cost: false,
            cache_key: p1_contracts::CacheKeySupport::Unsupported,
        };
        assert_eq!(
            bound_description(description(origin()), &origin()),
            Ok(description(origin()))
        );
        let foreign = Origin {
            route: "openai-chat/other".to_owned(),
            ..origin()
        };
        assert_eq!(
            bound_description(description(foreign), &origin()),
            Err(FOREIGN_DESCRIPTION)
        );
    }

    // ---- guest-driven cases over the shipped openai-chat component (G3b-02, G3b-10) ----

    const CHAT_PACKAGE: (&str, &str) =
        ("p1-module-provider-openai-chat", "p1/provider-openai-chat");

    /// Where `scripts/build-modules.sh` publishes the packages.
    fn built() -> std::path::PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../modules/target/p1-modules")
    }

    fn chat_release() -> crate::ReleaseManifest {
        let (package, name) = CHAT_PACKAGE;
        let path = built()
            .join(package)
            .join(format!("{package}.manifest.json"));
        let text = std::fs::read_to_string(&path).unwrap_or_else(|error| {
            panic!(
                "the build output {} is missing ({error}): run scripts/build-modules.sh first",
                path.display()
            )
        });
        let manifest: serde_json::Value = serde_json::from_str(&text).expect("a package manifest");
        assert_eq!(manifest["name"], name);
        let entry = serde_json::json!({
            "name": manifest["name"],
            "digest": manifest["digest"],
            "path": format!("{package}/{package}.wasm"),
            "kind": manifest["kind"],
            "world": manifest["world"],
            "protocol": manifest["protocol"],
            "capabilities": manifest["capabilities"],
            "variant": manifest["variant"],
        });
        let release =
            serde_json::json!({ "format": "p1-release-manifest/1", "components": [entry] });
        crate::ReleaseManifest::parse(&release.to_string()).expect("release manifest")
    }

    /// The shipped chat component on its own ticking clock.
    fn chat_module() -> LoadedModule {
        crate::Loader::new(chat_release(), built())
            .expect("loader")
            .load(CHAT_PACKAGE.1)
            .expect("the built chat provider loads")
    }

    /// The settings the host composes for `routes/glm-subscription.toml` and the `glm-5.3`
    /// profile (`RouteFile::component_adapter_settings`, which this crate cannot call).
    fn chat_settings() -> ProviderSettings {
        let profile =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../profiles/glm-5.3.toml");
        let toml = std::fs::read_to_string(&profile)
            .unwrap_or_else(|error| panic!("{}: {error}", profile.display()));
        ProviderSettings {
            origin_route: origin().route,
            endpoint: "https://api.z.ai/api/coding/paas/v4/chat/completions".to_owned(),
            model: "glm-5.3".to_owned(),
            wire_model: origin().model,
            adapter_settings: serde_json::json!({
                "dialect": "retained-thinking",
                "model_profile": { "stem": "glm-5.3", "toml": toml },
                "route_headers": {},
                "model_binding": {},
            }),
        }
    }

    /// No case here sends: they call the executor directly.
    struct Unreachable;

    const UNSENT: &str = "no case sends a request";

    fn unsent() -> ContractError {
        ContractError::new(ProviderErrorKind::Transport, UNSENT)
    }

    impl CredentialSource for Unreachable {
        fn access<'a>(
            &'a self,
        ) -> BoxFuture<'a, Result<p1_provider_http::Credential, ContractError>> {
            Box::pin(std::future::ready(Err(unsent())))
        }

        fn refresh<'a>(
            &'a self,
            _rejected: &'a p1_provider_http::Credential,
        ) -> BoxFuture<'a, Result<p1_provider_http::Credential, ContractError>> {
            Box::pin(std::future::ready(Err(unsent())))
        }
    }

    impl Transport for Unreachable {
        fn post<'a>(
            &'a self,
            _request: p1_provider_http::HttpRequest,
        ) -> BoxFuture<'a, Result<p1_provider_http::HttpResponse, p1_provider_http::TransportError>>
        {
            Box::pin(std::future::ready(Err(p1_provider_http::TransportError(
                UNSENT.to_owned(),
            ))))
        }
    }

    fn chat_provider(module: &LoadedModule, limits: ExecutionLimits) -> WasmProvider {
        WasmProvider::new(
            module,
            chat_settings(),
            Arc::new(Unreachable),
            Arc::new(Unreachable),
            limits,
        )
        .unwrap_or_else(|error| panic!("{error}"))
    }

    fn request(user_text: String) -> ProviderRequest {
        ProviderRequest {
            system_prompt: "prompt".to_owned(),
            history: vec![p1_contracts::Item::User { text: user_text }],
            tools: Vec::new(),
            options: p1_contracts::ModelOptions::default(),
        }
    }

    fn chunk(data: serde_json::Value) -> SseEvent {
        SseEvent {
            event: None,
            data: data.to_string(),
        }
    }

    fn text_chunk(text: &str) -> SseEvent {
        chunk(
            serde_json::json!({"choices":[{"index":0,"delta":{"content":text},"finish_reason":null}]}),
        )
    }

    /// A whole chat answer `text`, fed to a fresh decoder: what the decoder completed.
    fn answer(executor: &ExecutorHandle, text: &str) -> Vec<StreamEvent> {
        let mut parser = ComponentParser::new(executor.clone());
        let mut events = parser.on_event(text_chunk(text));
        events.extend(parser.on_event(chunk(
            serde_json::json!({"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}),
        )));
        events.extend(parser.on_event(SseEvent {
            event: None,
            data: "[DONE]".to_owned(),
        }));
        if !matches!(events.last(), Some(StreamEvent::Finished(_))) {
            events.push(StreamEvent::Finished(parser.on_end()));
        }
        events
    }

    fn completed_text(events: &[StreamEvent]) -> Option<String> {
        match events.last() {
            Some(StreamEvent::Finished(Outcome::Completed(response))) => {
                assert_eq!(response.item.origin, origin());
                Some(
                    response
                        .item
                        .blocks
                        .iter()
                        .filter_map(|block| match block {
                            AssistantBlock::Text { text } => Some(text.as_str()),
                            _ => None,
                        })
                        .collect(),
                )
            }
            _ => None,
        }
    }

    /// G3b-02: a request body of more than three megabytes comes back from the component's
    /// `lower` through the real export call, so Wasmtime lifts every byte under the Store's
    /// hostcall fuel; a budget below that lift fails here, not only in a configuration check.
    #[tokio::test(flavor = "current_thread")]
    async fn a_guest_lowers_and_the_host_lifts_more_than_three_megabytes_of_body() {
        let module = chat_module();
        let provider = chat_provider(&module, ExecutionLimits::default());
        let text = "x".repeat(4 << 20);
        let request = request_val(&request(text.clone())).unwrap();
        let lowered = provider
            .executor
            .prepare(
                request,
                ConnectionState::default(),
                &CancellationToken::new(),
            )
            .await
            .unwrap_or_else(|refusal| panic!("{:?}", refusal.into_error()));
        let WsLowered::Http(lowered) = lowered else {
            panic!("the chat route lowers an HTTP request");
        };
        assert!(lowered.body.len() > 4 << 20, "{}", lowered.body.len());
        let body: serde_json::Value = serde_json::from_slice(&lowered.body).expect("a JSON body");
        let user = body["messages"]
            .as_array()
            .and_then(|messages| messages.iter().find(|message| message["role"] == "user"))
            .expect("the user message");
        assert_eq!(user["content"], text.as_str());
    }

    /// Fuel for one call: the chat component's configure and each call of a short answer fit
    /// in a third of it, and validating a request of eight megabytes needs more than ten times
    /// it (measured on the shipped component: both fit in 1M; the validation fails at 30M and
    /// passes at 100M).
    const CALIBRATED_FUEL: u64 = 3_000_000;

    /// G3b-10: a real call that runs out of fuel fails with the closed kind, drops the
    /// instance with every decoder it held, and the next request completes on a rebuilt one.
    #[test]
    fn a_guest_out_of_fuel_loses_its_decoders_and_the_next_request_rebuilds() {
        let module = chat_module();
        let provider = chat_provider(
            &module,
            ExecutionLimits {
                fuel: CALIBRATED_FUEL,
                ..ExecutionLimits::default()
            },
        );
        let executor = &provider.executor;
        assert_eq!(
            completed_text(&answer(executor, "first")).as_deref(),
            Some("first")
        );

        // A decoder of the live instance, part-way through its response.
        let mut held = ComponentParser::new(executor.clone());
        assert!(!through_terminal(held.on_event(text_chunk("held"))).1);
        let error = provider
            .validate(&request("x".repeat(8 << 20)))
            .unwrap_err();
        assert_eq!(error.kind, ProviderErrorKind::Protocol);
        assert!(error.message.contains("fuel"), "{}", error.message);

        // The failed call dropped the instance: the held decoder is gone with it.
        match held.on_event(text_chunk("more")).as_slice() {
            [StreamEvent::Finished(Outcome::Failed(error))] => {
                assert_eq!(error.kind, ProviderErrorKind::Protocol);
                assert!(error.message.contains(DECODER_LOST), "{}", error.message);
            }
            other => panic!("expected the lost decoder's failure, got {other:?}"),
        }
        provider
            .validate(&request("small".to_owned()))
            .expect("a rebuilt instance validates");
        assert_eq!(
            completed_text(&answer(executor, "after")).as_deref(),
            Some("after")
        );
    }

    /// G3b-10: a real call past its deadline fails as `Transport`, drops its decoders, and a
    /// rebuilt instance answers once the clock stops. The clock is manual: it only advances
    /// while the long call is asked for.
    #[test]
    fn a_guest_past_its_deadline_loses_its_decoders_and_the_next_request_rebuilds() {
        let (loader, epochs) =
            crate::Loader::with_manual_epochs(chat_release(), built()).expect("loader");
        let module = loader
            .load(CHAT_PACKAGE.1)
            .expect("the built chat provider loads");
        let provider = chat_provider(
            &module,
            ExecutionLimits {
                deadline: EPOCH_TICK,
                ..ExecutionLimits::default()
            },
        );
        let executor = &provider.executor;
        let mut held = ComponentParser::new(executor.clone());
        assert!(!through_terminal(held.on_event(text_chunk("held"))).1);

        let running = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let ticker = {
            let running = running.clone();
            std::thread::spawn(move || {
                while running.load(Ordering::SeqCst) {
                    epochs.advance(1);
                    std::thread::yield_now();
                }
            })
        };
        let error = provider
            .validate(&request("x".repeat(8 << 20)))
            .unwrap_err();
        running.store(false, Ordering::SeqCst);
        ticker.join().unwrap();
        assert_eq!(error.kind, ProviderErrorKind::Transport);
        assert!(error.message.contains("deadline"), "{}", error.message);

        match held.on_event(text_chunk("more")).as_slice() {
            [StreamEvent::Finished(Outcome::Failed(error))] => {
                assert!(error.message.contains(DECODER_LOST), "{}", error.message);
            }
            other => panic!("expected the lost decoder's failure, got {other:?}"),
        }
        assert_eq!(
            completed_text(&answer(executor, "after")).as_deref(),
            Some("after")
        );
    }

    #[test]
    fn the_connection_state_carries_the_sessions_facts() {
        let state = ConnectionState {
            open: true,
            last_clean_response: Some("resp_1".to_owned()),
            failed_before_output: true,
        };
        assert_eq!(
            connection_state(&state),
            Val::Record(vec![
                ("open".to_owned(), Val::Bool(true)),
                (
                    "last-clean-response".to_owned(),
                    Val::Option(Some(Box::new(Val::String("resp_1".to_owned())))),
                ),
                ("failed-before-output".to_owned(), Val::Bool(true)),
            ])
        );
    }
}
