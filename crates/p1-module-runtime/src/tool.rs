//! `WasmTool` (freeze item 12): the one generic adapter from a tool component to
//! `p1_contracts::Tool`. Tool modules implement the `tool` world; none writes an adapter.
//!
//! - `declaration` is read once, at construction, through the restricted path, and cached.
//! - `identity` is the loader's, from the release manifest; the module never reports one.
//! - `effect`, `describe` and `describe_result` go through the restricted path
//!   ([`crate::restricted`]); a failure there yields the worst case, never a panic.
//! - `execute` goes through the executor ([`crate::executor`]), on a fresh instance per call,
//!   honouring `ToolContext.cancel`, with every failure mapped through `ModuleFailure`.
//!
//! The host only ever receives the adapter wrapped in `p1_redact::RedactingTool`
//! ([`wasm_tool`] is the only constructor), so no module output reaches history unmasked.

use std::sync::Arc;

use p1_contracts::serde_json;
use p1_contracts::tool::ResultDescription;
use p1_contracts::{
    BoxFuture, CallDescription, DeclarationKind, Effect, Grammar, Item, Tool, ToolCall,
    ToolContext, ToolDeclaration, ToolIdentity, ToolOutcome, ToolResultItem,
};
use p1_module_protocol::{
    ModuleFailure, WireCallDescription, WireItem, WireResultDescription, WireToolCall,
    WireToolOutcome,
};
use p1_redact::{MaskCounter, redacted};
use thiserror::Error;
use wasmtime::component::Val;

use crate::capabilities::{LinkError, Services, capability_linker};
use crate::executor::{ExecutionLimits, Executor};
use crate::loader::{EpochTicker, LoadedModule, ModuleKind};
use crate::restricted::Restricted;

/// Why a loaded module could not become a tool.
#[derive(Debug, Error)]
pub enum ToolError {
    /// The module is not of the `tool` class.
    #[error("module {name} is a {kind} module, not a tool")]
    NotATool {
        /// The module.
        name: String,
        /// Its class.
        kind: &'static str,
    },
    /// Its capabilities could not be linked.
    #[error("module {name}: {source}")]
    Link {
        /// The module.
        name: String,
        /// Why.
        source: LinkError,
    },
    /// The component does not fit the `tool` world as linked.
    #[error("module {name} cannot be instantiated: {reason}")]
    Instantiate {
        /// The module.
        name: String,
        /// wasmtime's message.
        reason: String,
    },
    /// `declaration` trapped on the restricted path or returned an invalid declaration.
    #[error("module {name} has no valid declaration: {reason}")]
    Declaration {
        /// The module.
        name: String,
        /// Why.
        reason: String,
    },
    /// No Tokio runtime is current, so the executor has nowhere to run.
    #[error("module {name}: a tool must be built inside a Tokio runtime, which runs its executor")]
    NoRuntime {
        /// The module.
        name: String,
    },
}

/// The generic tool adapter over one loaded tool component.
pub struct WasmTool {
    declaration: ToolDeclaration,
    identity: ToolIdentity,
    restricted: Restricted,
    executor: Executor,
    /// Deadlines advance only while the ticker runs; the tool may outlive its loader.
    _ticker: Arc<EpochTicker>,
}

/// Builds the tool for `module`, linking the capabilities its manifest grants from
/// `services`, and returns it wrapped by [`p1_redact::redacted`] with `counter`: the host
/// never sees an unwrapped module tool. Must be called inside a Tokio runtime, which runs
/// the tool's executor.
pub fn wasm_tool(
    module: &LoadedModule,
    services: Services,
    limits: ExecutionLimits,
    counter: &Arc<MaskCounter>,
) -> Result<Arc<dyn Tool>, ToolError> {
    let tool = WasmTool::new(module, services, limits)?;
    Ok(redacted(Arc::new(tool), counter))
}

impl WasmTool {
    fn new(
        module: &LoadedModule,
        services: Services,
        limits: ExecutionLimits,
    ) -> Result<Self, ToolError> {
        let name = module.name().to_owned();
        if module.kind() != ModuleKind::Tool {
            return Err(ToolError::NotATool {
                name,
                kind: module.kind().name(),
            });
        }
        let handle = tokio::runtime::Handle::try_current()
            .map_err(|_| ToolError::NoRuntime { name: name.clone() })?;
        let instantiate = |error: wasmtime::Error| ToolError::Instantiate {
            name: name.clone(),
            reason: format!("{error:#}"),
        };
        let linker = capability_linker(&module.engine, module.capabilities(), &services).map_err(
            |source| ToolError::Link {
                name: name.clone(),
                source,
            },
        )?;
        let pre = linker
            .instantiate_pre(&module.component)
            .map_err(instantiate)?;
        let restricted = Restricted::new(&module.engine, &module.component).map_err(instantiate)?;

        let declaration = restricted
            .call("declaration", &[])
            .ok_or_else(|| "the declaration export trapped".to_owned())
            .and_then(|results| declaration(results.first()))
            .map_err(|reason| ToolError::Declaration {
                name: name.clone(),
                reason,
            })?;

        let executor = Executor::start(&handle, module.engine.clone(), pre, services, limits);
        Ok(Self {
            declaration,
            identity: module.identity().clone(),
            restricted,
            executor,
            _ticker: module.ticker.clone(),
        })
    }
}

/// The wire text of a call, as the module reads it.
fn wire_call(call: &ToolCall) -> Option<String> {
    serde_json::to_string(&WireToolCall::from(call.clone())).ok()
}

/// The single string result of a restricted call, if it returned one.
fn string_result(results: Option<Vec<Val>>) -> Option<String> {
    match results?.into_iter().next()? {
        Val::String(text) => Some(text),
        _ => None,
    }
}

/// Reads the `declaration` record.
fn declaration(value: Option<&Val>) -> Result<ToolDeclaration, String> {
    let Some(Val::Record(fields)) = value else {
        return Err("declaration did not return a record".to_owned());
    };
    let field = |key: &str| {
        fields
            .iter()
            .find(|(name, _)| name == key)
            .map(|(_, value)| value)
    };
    let text = |key: &str| match field(key) {
        Some(Val::String(text)) => Ok(text.clone()),
        _ => Err(format!("declaration.{key} is not a string")),
    };
    let kind = match field("kind") {
        Some(Val::Variant(case, payload)) => match (case.as_str(), payload.as_deref()) {
            ("function", Some(Val::String(schema))) => DeclarationKind::Function {
                input_schema: serde_json::from_str(schema)
                    .map_err(|error| format!("the input schema is not JSON: {error}"))?,
            },
            ("freeform", Some(Val::Option(grammar))) => DeclarationKind::Freeform {
                grammar: match grammar.as_deref() {
                    None => None,
                    Some(Val::Record(grammar)) => {
                        let part = |key: &str| {
                            grammar
                                .iter()
                                .find_map(|(name, value)| match value {
                                    Val::String(text) if name == key => Some(text.clone()),
                                    _ => None,
                                })
                                .ok_or_else(|| format!("grammar.{key} is not a string"))
                        };
                        Some(Grammar {
                            syntax: part("syntax")?,
                            definition: part("definition")?,
                        })
                    }
                    Some(_) => return Err("the grammar is not a record".to_owned()),
                },
            },
            _ => return Err(format!("declaration.kind {case} is not a known kind")),
        },
        _ => return Err("declaration.kind is not a variant".to_owned()),
    };
    Ok(ToolDeclaration {
        name: text("name")?,
        description: text("description")?,
        kind,
    })
}

/// What a failed `describe` yields: the neutral verb and nothing the module said.
fn empty_description() -> CallDescription {
    CallDescription {
        verb: "call",
        target: None,
        edit: None,
        destructive: false,
    }
}

impl Tool for WasmTool {
    fn declaration(&self) -> &ToolDeclaration {
        &self.declaration
    }

    fn identity(&self) -> &ToolIdentity {
        &self.identity
    }

    fn effect(&self, call: &ToolCall) -> Effect {
        let Some(call) = wire_call(call) else {
            return Effect::Executes;
        };
        let results = self.restricted.call("effect", &[Val::String(call)]);
        match results.and_then(|results| results.into_iter().next()) {
            Some(Val::Enum(case)) => match case.as_str() {
                "read-only" => Effect::ReadOnly,
                "writes-files" => Effect::WritesFiles,
                "delegates" => Effect::Delegates,
                // `executes`, and the worst case for anything unreadable.
                _ => Effect::Executes,
            },
            _ => Effect::Executes,
        }
    }

    fn describe(&self, call: &ToolCall) -> CallDescription {
        wire_call(call)
            .and_then(|call| string_result(self.restricted.call("describe", &[Val::String(call)])))
            .and_then(|text| serde_json::from_str::<WireCallDescription>(&text).ok())
            .map(CallDescription::from)
            .unwrap_or_else(empty_description)
    }

    fn describe_result(&self, call: &ToolCall, result: &ToolResultItem) -> ResultDescription {
        let item = serde_json::to_string(&WireItem::from(Item::ToolResult(result.clone()))).ok();
        let described = wire_call(call)
            .zip(item)
            .and_then(|(call, item)| {
                string_result(
                    self.restricted
                        .call("describe-result", &[Val::String(call), Val::String(item)]),
                )
            })
            .and_then(|text| serde_json::from_str::<WireResultDescription>(&text).ok())
            .and_then(|wire| ResultDescription::try_from(wire).ok());
        // The host's own summary when the module's failed: the first line of what the model
        // was shown, as the trait's default gives.
        described.unwrap_or_else(|| ResultDescription {
            summary: result.content.lines().next().unwrap_or_default().to_owned(),
            detail: None,
        })
    }

    fn execute<'a>(
        &'a self,
        call: &'a ToolCall,
        context: ToolContext,
    ) -> BoxFuture<'a, ToolOutcome> {
        Box::pin(async move {
            let Some(call) = wire_call(call) else {
                return ModuleFailure::InvalidOutput("the call cannot be serialized".to_owned())
                    .into_tool_outcome();
            };
            let results = self
                .executor
                .call("execute", vec![Val::String(call)], context.cancel)
                .await;
            let outcome = results.and_then(|results| {
                let text = string_result(Some(results)).ok_or_else(|| {
                    ModuleFailure::InvalidOutput("execute returned no text".to_owned())
                })?;
                serde_json::from_str::<WireToolOutcome>(&text)
                    .map(ToolOutcome::from)
                    .map_err(|error| ModuleFailure::InvalidOutput(error.to_string()))
            });
            outcome.unwrap_or_else(ModuleFailure::into_tool_outcome)
        })
    }
}
