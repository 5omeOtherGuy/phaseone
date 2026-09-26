//! `WasmContextPolicy`: the adapter from one loaded `context-policy` component (world
//! `p1:module/context-policy@1.0.0`) to `p1_contracts::ContextPolicy`, and the host side of
//! the component's `summary` import.
//!
//! Built like [`WasmTool`](crate::WasmTool)'s `execute`: every call goes through the executor
//! ([`crate::executor`], ADR-0015) on a fresh Store and instance, bounded by
//! [`ExecutionLimits`], and both trait methods are `Send` boxed futures.
//!
//! - `configure(settings)` is called once when the adapter is built, on the restricted path
//!   ([`crate::restricted`]): an `err` or a trap there is an assembly error
//!   ([`ContextPolicyError::Configure`]). Each fresh instance of a call then gets the same
//!   `configure` as its executor prelude before any other export runs, so the component
//!   keeps no state between calls (ADR-0036) and never runs unconfigured.
//! - History items and usage cross as their `p1-module-protocol` wire JSON; an answer that
//!   is not the world's shape, or whose JSON is not its family's, is the module's invalid
//!   output. A trap, a fuel or deadline stop or invalid output is `ContextError::Failed`
//!   naming the module, and the core applies its own rules to it; the component's own
//!   `failed(reason)` is passed on verbatim.
//! - `summary.summarize` is answered by a native [`SummaryService`] the caller passes in, so
//!   this file holds no provider code. The service's future runs as its own Tokio task on the
//!   host runtime while the guest call waits in the import: the task that owns the context
//!   Store only awaits it, and the service has no handle to the component, so the component
//!   is never re-entered and no `prepare` runs inside a summary. The answer is masked here
//!   too before the guest sees it (wit.md: summaries are masked natively), whatever the
//!   service did.
//! - Only the capabilities of the class allocation may be granted (`modules/capabilities.toml`).

use std::sync::Arc;

use p1_contracts::serde_json;
use p1_contracts::{
    BoxFuture, CancellationToken, Compaction, ContextError, ContextInput, ContextPolicy, Item,
    Prepared, ProviderError, StopReason, Usage,
};
use p1_module_protocol::{ModuleFailure, WireItem, WireProviderError, WireUsage};
use thiserror::Error;
use tokio::task::JoinHandle;
use wasmtime::bail;
use wasmtime::component::{Linker, Val};

use crate::capabilities::{CallState, LinkError, Services, capability_linker};
use crate::executor::{ExecutionLimits, Executor, Prelude};
use crate::loader::{Epochs, LoadedModule, ModuleKind, interface_import};
use crate::manifest::Digest;
use crate::restricted::Restricted;

/// The capabilities the `context-policy` class may be granted (freeze item 13,
/// `modules/capabilities.toml`), besides the type-only `types`.
pub const CONTEXT_POLICY_ALLOCATION: [&str; 5] =
    ["control", "clock", "notices", "summary", "completion"];

/// One summary request as the component sends it (`summary.summary-request`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SummaryRequest {
    /// The rendered part of the history to summarize.
    pub transcript: String,
    /// The output cap, sent exactly as given; `None` sends no cap.
    pub max_output_tokens: Option<u32>,
}

/// A completed summary response (`summary.summary-response`).
#[derive(Debug, Clone, PartialEq)]
pub struct SummaryResponse {
    /// The text of the completed response, masked.
    pub text: String,
    pub stop: StopReason,
    /// Unknown usage is `None`, never zero.
    pub usage: Option<Usage>,
}

/// Why no summary response came back (`summary.summary-error`).
#[derive(Debug, Clone, PartialEq)]
pub enum SummaryError {
    /// The route refused the request before anything was sent.
    Refused(ProviderError),
    /// The request was sent and failed.
    Failed(ProviderError),
    /// The call was cancelled while the request was in flight.
    Cancelled,
}

/// The native summary operation a context policy's `summary` capability is linked to: one
/// request through the agent's provider, streamed to its end. `p1-context`'s
/// `ProviderSummary` is the real one. It must never call back into a context policy.
pub trait SummaryService: Send + Sync {
    /// Sends `request` for a call whose cancellation is `cancel`.
    fn summarize(
        &self,
        request: SummaryRequest,
        cancel: CancellationToken,
    ) -> BoxFuture<'_, Result<SummaryResponse, SummaryError>>;
}

/// Why a loaded module could not become a context policy.
#[derive(Debug, Error)]
pub enum ContextPolicyError {
    /// The module is not of the `context-policy` class.
    #[error("module {name} is a {kind} module, not a context policy")]
    NotAContextPolicy {
        /// The module.
        name: String,
        /// Its class.
        kind: &'static str,
    },
    /// The manifest grants a capability outside the class allocation.
    #[error("module {name}: capability {capability} is not in the context-policy allocation")]
    Capability {
        /// The module.
        name: String,
        /// The capability as written.
        capability: String,
    },
    /// Its capabilities could not be linked.
    #[error("module {name}: {source}")]
    Link {
        /// The module.
        name: String,
        /// Why.
        source: LinkError,
    },
    /// The component does not fit the `context-policy` world as linked.
    #[error("module {name} cannot be instantiated: {reason}")]
    Instantiate {
        /// The module.
        name: String,
        /// wasmtime's message.
        reason: String,
    },
    /// `configure` refused the settings or trapped: the assembly is refused.
    #[error("module {name} refused its settings: {reason}")]
    Configure {
        /// The module.
        name: String,
        /// The module's reason, or that it trapped.
        reason: String,
    },
    /// No Tokio runtime is current, so the executor has nowhere to run.
    #[error(
        "module {name}: a context policy must be built inside a Tokio runtime, which runs its executor"
    )]
    NoRuntime {
        /// The module.
        name: String,
    },
}

/// The adapter over one loaded context-policy component.
pub struct WasmContextPolicy {
    name: String,
    digest: Digest,
    executor: Executor,
    /// Deadlines advance only while the epoch clock lives; the policy may outlive its loader.
    _epochs: Arc<Epochs>,
}

impl WasmContextPolicy {
    /// Builds the policy for `module` with `settings` (the JSON object `configure` takes),
    /// linking its `summary` import to `summary`. Must be called inside a Tokio runtime,
    /// which runs the policy's executor.
    pub fn new(
        module: &LoadedModule,
        settings: &str,
        summary: Arc<dyn SummaryService>,
        limits: ExecutionLimits,
    ) -> Result<Self, ContextPolicyError> {
        let name = module.name().to_owned();
        if module.kind() != ModuleKind::ContextPolicy {
            return Err(ContextPolicyError::NotAContextPolicy {
                name,
                kind: module.kind().name(),
            });
        }
        if let Some(capability) = module
            .capabilities()
            .iter()
            .find(|capability| !CONTEXT_POLICY_ALLOCATION.contains(&capability.as_str()))
        {
            return Err(ContextPolicyError::Capability {
                name,
                capability: capability.clone(),
            });
        }
        let handle = tokio::runtime::Handle::try_current()
            .map_err(|_| ContextPolicyError::NoRuntime { name: name.clone() })?;
        let instantiate = |error: wasmtime::Error| ContextPolicyError::Instantiate {
            name: name.clone(),
            reason: format!("{error:#}"),
        };
        let services = Services {
            summary: Some(summary),
            ..Services::default()
        };
        let linker = capability_linker(&module.engine, module.capabilities(), &services).map_err(
            |source| ContextPolicyError::Link {
                name: name.clone(),
                source,
            },
        )?;
        let pre = linker
            .instantiate_pre(&module.component)
            .map_err(instantiate)?;

        let configure = Prelude {
            export: "configure",
            params: vec![Val::String(settings.to_owned())],
        };
        let restricted = Restricted::new(&module.engine, &module.component).map_err(instantiate)?;
        restricted
            .call(configure.export, &configure.params)
            .ok_or_else(|| "configure trapped".to_owned())
            .and_then(|results| configured(results.first()))
            .map_err(|reason| ContextPolicyError::Configure {
                name: name.clone(),
                reason,
            })?;

        let executor = Executor::start_with_prelude(
            &handle,
            module.engine.clone(),
            module.epochs.clone(),
            pre,
            services,
            limits,
            Some(configure),
        );
        Ok(Self {
            name,
            digest: module.digest(),
            executor,
            _epochs: module.epochs.clone(),
        })
    }

    /// The policy package's manifest name, e.g. `p1/context/summarizing`.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The digest of the policy's verified component bytes: its identity.
    pub fn digest(&self) -> Digest {
        self.digest
    }

    /// Calls `export` with the input's history and usage and reads its one result.
    async fn call<T>(
        &self,
        export: &'static str,
        input: ContextInput<'_>,
        read: fn(Option<Val>) -> Answer<T>,
    ) -> Result<T, ContextError> {
        let results = self
            .executor
            .call(
                export,
                params(input.history, input.last_usage),
                input.cancel.clone(),
            )
            .await;
        let answer = results.and_then(|results| read(results.into_iter().next()));
        match answer {
            Ok(answer) => answer,
            Err(ModuleFailure::Cancelled) => Err(ContextError::Cancelled),
            Err(failure) => Err(ContextError::Failed(format!(
                "context policy {} failed: {failure}",
                self.name
            ))),
        }
    }
}

impl ContextPolicy for WasmContextPolicy {
    fn prepare<'a>(
        &'a self,
        input: ContextInput<'a>,
    ) -> BoxFuture<'a, Result<Option<Prepared>, ContextError>> {
        Box::pin(self.call("prepare", input, prepare_answer))
    }

    fn compact_now<'a>(
        &'a self,
        input: ContextInput<'a>,
    ) -> BoxFuture<'a, Result<Compaction, ContextError>> {
        Box::pin(self.call("compact-now", input, compaction_answer))
    }
}

/// `configure`'s `result<_, string>`.
fn configured(value: Option<&Val>) -> Result<(), String> {
    match value {
        Some(Val::Result(Ok(None))) => Ok(()),
        Some(Val::Result(Err(Some(reason)))) => match reason.as_ref() {
            Val::String(reason) => Err(reason.clone()),
            _ => Err("configure refused with a reason that is not a string".to_owned()),
        },
        _ => Err("configure did not return result<_, string>".to_owned()),
    }
}

/// The history and last usage as the world's `list<history-item>` and `option<usage>`.
fn params(history: &[Item], last_usage: Option<&Usage>) -> Vec<Val> {
    let items = history
        .iter()
        .map(|item| Val::String(json(&WireItem::from(item.clone()))))
        .collect();
    let usage = last_usage.map(|usage| Box::new(Val::String(json(&WireUsage::from(*usage)))));
    vec![Val::List(items), Val::Option(usage)]
}

/// The wire types serialize to JSON without a failure case (no maps with non-string keys, no
/// non-finite numbers).
fn json(value: &impl WireJson) -> String {
    value.to_json()
}

/// The wire values this adapter writes.
trait WireJson {
    fn to_json(&self) -> String;
}

macro_rules! wire_json {
    ($($wire:ty),*) => {$(
        impl WireJson for $wire {
            fn to_json(&self) -> String {
                serde_json::to_string(self).expect("a wire value always serializes")
            }
        }
    )*};
}

wire_json!(WireItem, WireUsage, WireProviderError);

fn invalid(what: &str) -> ModuleFailure {
    ModuleFailure::InvalidOutput(what.to_owned())
}

/// An export's answer read from its result: the policy's own answer, or the module's
/// failure to give one.
type Answer<T> = Result<Result<T, ContextError>, ModuleFailure>;

/// The `result<T, context-error>` of both exports: the `ok` payload for `read`, the error as
/// the component gave it.
fn context_result(value: Option<Val>, export: &str) -> Answer<Val> {
    match value {
        Some(Val::Result(Ok(Some(payload)))) => Ok(Ok(*payload)),
        Some(Val::Result(Err(Some(error)))) => match *error {
            Val::Variant(case, None) if case == "cancelled" => Ok(Err(ContextError::Cancelled)),
            Val::Variant(case, Some(reason)) if case == "failed" => match *reason {
                Val::String(reason) => Ok(Err(ContextError::Failed(reason))),
                _ => Err(invalid(&format!("{export} failed without a text reason"))),
            },
            _ => Err(invalid(&format!(
                "{export} returned an unknown context-error"
            ))),
        },
        _ => Err(invalid(&format!(
            "{export} did not return its result shape"
        ))),
    }
}

fn prepare_answer(value: Option<Val>) -> Answer<Option<Prepared>> {
    Ok(match context_result(value, "prepare")? {
        Ok(Val::Option(None)) => Ok(None),
        Ok(Val::Option(Some(prepared))) => Ok(Some(prepared_of(*prepared)?)),
        Ok(_) => return Err(invalid("prepare did not return option<prepared>")),
        Err(error) => Err(error),
    })
}

fn compaction_answer(value: Option<Val>) -> Answer<Compaction> {
    Ok(match context_result(value, "compact-now")? {
        Ok(Val::Variant(case, Some(payload))) if case == "unchanged" => match *payload {
            Val::U64(tokens) => Ok(Compaction::Unchanged { tokens }),
            _ => return Err(invalid("compact-now: unchanged carries no token count")),
        },
        Ok(Val::Variant(case, Some(payload))) if case == "replaced" => {
            let Val::Record(fields) = *payload else {
                return Err(invalid("compact-now: replaced is not a replacement record"));
            };
            let mut prepared = None;
            let mut tokens_before = None;
            let mut tokens_after = None;
            for (name, value) in fields {
                match (name.as_str(), value) {
                    ("prepared", value) => prepared = Some(prepared_of(value)?),
                    ("tokens-before", Val::U64(tokens)) => tokens_before = Some(tokens),
                    ("tokens-after", Val::U64(tokens)) => tokens_after = Some(tokens),
                    _ => return Err(invalid(&format!("compact-now: unexpected field {name}"))),
                }
            }
            match (prepared, tokens_before, tokens_after) {
                (Some(prepared), Some(tokens_before), Some(tokens_after)) => {
                    Ok(Compaction::Replaced {
                        prepared,
                        tokens_before,
                        tokens_after,
                    })
                }
                _ => return Err(invalid("compact-now: the replacement is missing a field")),
            }
        }
        Ok(_) => return Err(invalid("compact-now did not return a compaction")),
        Err(error) => Err(error),
    })
}

/// A `prepared` record: every item and the usage must be their family's wire JSON.
fn prepared_of(value: Val) -> Result<Prepared, ModuleFailure> {
    let Val::Record(fields) = value else {
        return Err(invalid("prepared is not a record"));
    };
    let mut items = None;
    let mut usage = None;
    for (name, value) in fields {
        match (name.as_str(), value) {
            ("items", Val::List(list)) => {
                let parsed = list
                    .into_iter()
                    .enumerate()
                    .map(|(index, item)| match item {
                        Val::String(text) => serde_json::from_str::<WireItem>(&text)
                            .map(Item::from)
                            .map_err(|error| {
                                invalid(&format!("replacement item {index}: {error}"))
                            }),
                        _ => Err(invalid(&format!("replacement item {index} is not text"))),
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                items = Some(parsed);
            }
            ("usage", Val::Option(value)) => {
                usage = Some(match value.map(|value| *value) {
                    None => None,
                    Some(Val::String(text)) => Some(
                        serde_json::from_str::<WireUsage>(&text)
                            .map(Usage::from)
                            .map_err(|error| invalid(&format!("the usage: {error}")))?,
                    ),
                    Some(_) => return Err(invalid("the usage is not text")),
                });
            }
            _ => return Err(invalid(&format!("prepared: unexpected field {name}"))),
        }
    }
    match (items, usage) {
        (Some(items), Some(usage)) => Ok(Prepared { items, usage }),
        _ => Err(invalid("prepared is missing a field")),
    }
}

/// Links `summary.summarize` to the call's [`SummaryService`]. Called by
/// [`capability_linker`] for a module granted `summary`, only when a service was given.
pub(crate) fn link_summary(linker: &mut Linker<CallState>) -> wasmtime::Result<()> {
    let mut summary = linker.instance(&interface_import("summary"))?;
    summary.func_new_async("summarize", |store, _ty, params, results| {
        Box::new(async move {
            let request = summary_request(&params[0])?;
            let Some(service) = store.data().summary.clone() else {
                bail!("summary.summarize called without a summary service");
            };
            let cancel = store.data().cancel.clone();
            let outcome = summarize_outside(service, request, cancel).await?;
            results[0] = outcome_val(outcome);
            Ok(())
        })
    })
}

/// Aborts the summary task when the guest call ends without waiting for it: a cancelled or
/// abandoned call leaves no provider stream behind.
struct SummaryTask(JoinHandle<Result<SummaryResponse, SummaryError>>);

impl Drop for SummaryTask {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Runs the service as its own task on the host runtime and waits for it, or for the
/// call's cancellation, whichever comes first. The service gets no handle to the component.
async fn summarize_outside(
    service: Arc<dyn SummaryService>,
    request: SummaryRequest,
    cancel: CancellationToken,
) -> wasmtime::Result<Result<SummaryResponse, SummaryError>> {
    if cancel.is_cancelled() {
        return Ok(Err(SummaryError::Cancelled));
    }
    let service_cancel = cancel.clone();
    let mut task = SummaryTask(tokio::spawn(async move {
        service.summarize(request, service_cancel).await
    }));
    tokio::select! {
        biased;
        () = cancel.cancelled() => Ok(Err(SummaryError::Cancelled)),
        joined = &mut task.0 => {
            joined.map_err(|error| wasmtime::format_err!("the summary service failed: {error}"))
        }
    }
}

fn summary_request(value: &Val) -> wasmtime::Result<SummaryRequest> {
    let Val::Record(fields) = value else {
        bail!("summary.summarize: the request is not a record");
    };
    let mut transcript = None;
    let mut max_output_tokens = None;
    for (name, value) in fields {
        match (name.as_str(), value) {
            ("transcript", Val::String(text)) => transcript = Some(text.clone()),
            ("max-output-tokens", Val::Option(cap)) => {
                max_output_tokens = Some(match cap.as_deref() {
                    None => None,
                    Some(Val::U32(cap)) => Some(*cap),
                    Some(_) => bail!("summary.summarize: max-output-tokens is not a u32"),
                });
            }
            _ => bail!("summary.summarize: unexpected request field {name}"),
        }
    }
    match (transcript, max_output_tokens) {
        (Some(transcript), Some(max_output_tokens)) => Ok(SummaryRequest {
            transcript,
            max_output_tokens,
        }),
        _ => bail!("summary.summarize: the request is missing a field"),
    }
}

fn outcome_val(outcome: Result<SummaryResponse, SummaryError>) -> Val {
    let error = |case: &str, payload: Option<Val>| {
        Val::Result(Err(Some(Box::new(Val::Variant(
            case.to_owned(),
            payload.map(Box::new),
        )))))
    };
    let provider_error = |error: ProviderError| Val::String(json(&WireProviderError::from(error)));
    match outcome {
        Ok(response) => Val::Result(Ok(Some(Box::new(Val::Record(vec![
            (
                "text".to_owned(),
                Val::String(p1_redact::redact(&response.text).text),
            ),
            (
                "stop".to_owned(),
                Val::Enum(stop_case(response.stop).to_owned()),
            ),
            (
                "usage".to_owned(),
                Val::Option(
                    response
                        .usage
                        .map(|usage| Box::new(Val::String(json(&WireUsage::from(usage))))),
                ),
            ),
        ]))))),
        Err(SummaryError::Refused(reason)) => error("refused", Some(provider_error(reason))),
        Err(SummaryError::Failed(reason)) => error("failed", Some(provider_error(reason))),
        Err(SummaryError::Cancelled) => error("cancelled", None),
    }
}

/// The `stop-reason` enum case of `stop`.
fn stop_case(stop: StopReason) -> &'static str {
    match stop {
        StopReason::EndTurn => "end-turn",
        StopReason::ToolUse => "tool-use",
        StopReason::MaxOutputTokens => "max-output-tokens",
        StopReason::ContextWindowExceeded => "context-window-exceeded",
        StopReason::Refusal => "refusal",
        StopReason::Paused => "paused",
        StopReason::Other => "other",
    }
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use p1_contracts::serde_json::{Value, json};

    use super::*;
    use crate::{Loader, ReleaseManifest};

    const PACKAGE: (&str, &str) = ("p1-module-context", "p1/context/summarizing");

    /// Where `scripts/build-modules.sh` publishes the packages.
    fn built() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../modules/target/p1-modules")
    }

    fn output(file: &str) -> String {
        let path = built().join(PACKAGE.0).join(file);
        std::fs::read_to_string(&path).unwrap_or_else(|error| {
            panic!(
                "the build output {} is missing ({error}): run scripts/build-modules.sh first",
                path.display()
            )
        })
    }

    fn module(capabilities: Option<Value>) -> LoadedModule {
        let manifest: Value =
            serde_json::from_str(&output(&format!("{}.manifest.json", PACKAGE.0)))
                .expect("the package manifest is JSON");
        let entry = json!({
            "name": manifest["name"],
            "digest": manifest["digest"],
            "path": format!("{0}/{0}.wasm", PACKAGE.0),
            "kind": manifest["kind"],
            "world": manifest["world"],
            "protocol": manifest["protocol"],
            "capabilities": capabilities.unwrap_or_else(|| manifest["capabilities"].clone()),
            "variant": manifest["variant"],
        });
        let release = json!({ "format": "p1-release-manifest/1", "components": [entry] });
        let release = ReleaseManifest::parse(&release.to_string()).expect("release manifest");
        Loader::new(release, built())
            .expect("loader")
            .load(PACKAGE.1)
            .expect("the built context policy loads")
    }

    fn settings() -> Value {
        json!({
            "window_tokens": 10_000,
            "output_headroom_tokens": 1_000,
            "summarize_at_tokens": 500,
            "keep_recent_tokens": 80,
            "user_verbatim_tokens": 100,
            "tool_result_excerpt_chars": 2_000,
            "summary_output_tokens": 4_000,
        })
    }

    /// A service no test below may reach: nothing here is over the threshold.
    struct Unreachable;

    impl SummaryService for Unreachable {
        fn summarize(
            &self,
            _request: SummaryRequest,
            _cancel: CancellationToken,
        ) -> BoxFuture<'_, Result<SummaryResponse, SummaryError>> {
            panic!("no summary is due")
        }
    }

    fn policy(settings: &Value) -> Result<WasmContextPolicy, ContextPolicyError> {
        WasmContextPolicy::new(
            &module(None),
            &settings.to_string(),
            Arc::new(Unreachable),
            ExecutionLimits::default(),
        )
    }

    #[tokio::test]
    async fn every_call_runs_configured_and_below_the_threshold_changes_nothing() {
        let policy = policy(&settings()).expect("the settings are accepted");
        assert_eq!(policy.name(), PACKAGE.1);
        let history = vec![Item::User {
            text: "hello".to_owned(),
        }];
        // Twice: each call gets a fresh instance, and the prelude configures each.
        for _ in 0..2 {
            let prepared = policy
                .prepare(ContextInput {
                    history: &history,
                    last_usage: None,
                    cancel: &CancellationToken::new(),
                })
                .await
                .expect("prepare runs");
            assert!(prepared.is_none());
        }
    }

    #[tokio::test]
    async fn refused_settings_are_an_assembly_error() {
        let mut unknown = settings();
        unknown["surprise"] = json!(1);
        let mut missing = settings();
        missing
            .as_object_mut()
            .expect("an object")
            .remove("keep_recent_tokens");
        let mut too_high = settings();
        too_high["summarize_at_tokens"] = json!(9_000);
        for (case, settings) in [
            ("unknown", unknown),
            ("missing", missing),
            ("wall", too_high),
        ] {
            match policy(&settings) {
                Err(ContextPolicyError::Configure { name, reason }) => {
                    assert_eq!(name, PACKAGE.1, "{case}");
                    assert!(!reason.is_empty(), "{case}");
                }
                Err(other) => panic!("{case}: wrong refusal {other}"),
                Ok(_) => panic!("{case}: the settings must be refused"),
            }
        }
    }

    #[tokio::test]
    async fn a_grant_outside_the_allocation_is_refused() {
        for capability in ["random", "process"] {
            match WasmContextPolicy::new(
                &module(Some(json!(["control", "summary", capability]))),
                &settings().to_string(),
                Arc::new(Unreachable),
                ExecutionLimits::default(),
            ) {
                Err(ContextPolicyError::Capability {
                    capability: refused,
                    ..
                }) => assert_eq!(refused, capability),
                Err(other) => panic!("{capability}: wrong refusal {other}"),
                Ok(_) => panic!("{capability}: a context policy must not be granted it"),
            }
        }
    }

    #[tokio::test]
    async fn a_fuel_stop_is_a_failure_naming_the_policy() {
        let policy = WasmContextPolicy::new(
            &module(None),
            &settings().to_string(),
            Arc::new(Unreachable),
            ExecutionLimits {
                fuel: 1,
                ..ExecutionLimits::default()
            },
        )
        .expect("configure runs on the restricted path, with its own fuel");
        let history = vec![Item::User {
            text: "hello".to_owned(),
        }];
        match policy
            .prepare(ContextInput {
                history: &history,
                last_usage: None,
                cancel: &CancellationToken::new(),
            })
            .await
        {
            Err(ContextError::Failed(reason)) => assert!(
                reason.starts_with(&format!("context policy {} failed: ", PACKAGE.1)),
                "{reason}"
            ),
            Err(ContextError::Cancelled) => panic!("a fuel stop is not a cancellation"),
            Ok(_) => panic!("a stopped call must fail"),
        }
    }

    #[test]
    fn answers_outside_the_world_are_invalid_output() {
        let ok = |payload: Val| Some(Val::Result(Ok(Some(Box::new(payload)))));
        assert!(matches!(
            prepare_answer(ok(Val::Option(None))),
            Ok(Ok(None))
        ));
        let failed = Some(Val::Result(Err(Some(Box::new(Val::Variant(
            "failed".to_owned(),
            Some(Box::new(Val::String("full".to_owned()))),
        ))))));
        assert!(matches!(
            prepare_answer(failed),
            Ok(Err(ContextError::Failed(reason))) if reason == "full"
        ));
        let bad_item = ok(Val::Option(Some(Box::new(Val::Record(vec![
            (
                "items".to_owned(),
                Val::List(vec![Val::String("{\"not\":\"an item\"}".to_owned())]),
            ),
            ("usage".to_owned(), Val::Option(None)),
        ])))));
        for answer in [bad_item, ok(Val::Bool(true)), Some(Val::Bool(true)), None] {
            assert!(matches!(
                prepare_answer(answer),
                Err(ModuleFailure::InvalidOutput(_))
            ));
        }
        assert!(matches!(
            compaction_answer(ok(Val::Variant(
                "unchanged".to_owned(),
                Some(Box::new(Val::U64(7)))
            ))),
            Ok(Ok(Compaction::Unchanged { tokens: 7 }))
        ));
        assert!(matches!(
            compaction_answer(ok(Val::Variant("unchanged".to_owned(), None))),
            Err(ModuleFailure::InvalidOutput(_))
        ));
    }

    #[test]
    fn a_summary_answer_is_masked_before_the_guest_sees_it() {
        let secret = format!("sk-proj-{}", "a".repeat(40));
        let Val::Result(Ok(Some(record))) = outcome_val(Ok(SummaryResponse {
            text: format!("keep {secret}"),
            stop: StopReason::EndTurn,
            usage: None,
        })) else {
            panic!("a response is ok");
        };
        let Val::Record(fields) = *record else {
            panic!("a response is a record");
        };
        let Some((_, Val::String(text))) = fields.iter().find(|(name, _)| name == "text") else {
            panic!("the response has text");
        };
        assert!(!text.contains(&secret), "{text}");
    }
}
