//! The host side of the `tool-outputs` import (`modules/wit/outputs.wit`, ADR-0109): the
//! values that cross it, the [`ToolOutputsService`] a caller passes in
//! ([`Services::tool_outputs`](crate::Services::tool_outputs)), and the store behind it.
//!
//! The store is written by the host alone: a [`ProcessCapability`] given [`CallOutputs`]
//! tees every process it starts through an [`OutputRecorder`](store::OutputRecorder), which
//! masks each chunk and writes it before the process stream cuts the output to its head and
//! tail. The same [`CallOutputs`] serves the call's `tool-outputs`, so `produced` names
//! exactly the outputs of the current export call (a call-scoped service, ADR-0092).
//!
//! [`ProcessCapability`]: crate::process::ProcessCapability

mod redact;
mod store;

use std::sync::{Arc, Mutex};

use p1_redact::SecretSet;
use wasmtime::bail;
use wasmtime::component::{Linker, Val};

use crate::capabilities::{CallState, check_arity};
use crate::loader::interface_import;

#[cfg(test)]
pub(crate) use redact::MAX_HELD as MAX_HELD_BYTES;
pub(crate) use store::{Entry, OutputRecorder};
pub use store::{MAX_PAGE_BYTES, OutputCaps, OutputStore};

/// How much of an output the store holds (`tool-outputs.capture`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Capture {
    /// Everything the command printed, masked.
    Complete,
    /// A cap stopped the store; what it holds is exact up to there.
    StoredCapReached,
    /// The store could not keep up with the command (its disk stalled or was too slow) and
    /// stopped; what it holds is exact up to there.
    StorageIncomplete,
    /// Nothing is recoverable.
    StorageFailed,
}

/// One stored output (`tool-outputs.output-info`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutputInfo {
    /// The opaque host id.
    pub handle: String,
    /// Stored bytes of masked UTF-8 text.
    pub stored_bytes: u64,
    /// How much of the output that is.
    pub capture: Capture,
}

/// One page of a stored output (`tool-outputs.output-page`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutputPage {
    /// The text from the requested offset.
    pub text: String,
    /// The requested offset plus the bytes of `text`.
    pub next_offset: u64,
    /// `next_offset` is the end of the output.
    pub at_end: bool,
}

/// Why a stored output could not be read (`tool-outputs.output-error`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OutputError {
    /// No output under this handle.
    UnknownOutput,
    /// The limit is smaller than the character at the offset.
    LimitTooSmall,
    /// The offset lies past the end; the output's stored bytes.
    OffsetPastEnd(u64),
    /// The offset lies inside a character.
    OffsetInsideCharacter,
    /// The store could not be read.
    ReadFailed(String),
}

/// The native service a module's `tool-outputs` capability is linked to.
pub trait ToolOutputsService: Send + Sync {
    /// The outputs stored during the current export call, oldest first (`produced`).
    fn produced(&self) -> Vec<OutputInfo>;
    /// What the store holds under `handle` (`describe`).
    fn describe(&self, handle: &str) -> Result<OutputInfo, OutputError>;
    /// One page of the output under `handle` (`page`).
    fn page(&self, handle: &str, offset: u64, limit: u32) -> Result<OutputPage, OutputError>;
}

/// The outputs of ONE export call over a session's [`OutputStore`]: the tee the call's
/// process capability writes through, and the call's `tool-outputs` service. Build one per
/// call ([`Services::call_scoped`](crate::Services::call_scoped)); a service that is not
/// call-scoped simply reports no `produced` output.
#[derive(Clone)]
pub struct CallOutputs {
    store: Arc<OutputStore>,
    /// The agent's registered credentials, masked with every credential shape.
    secrets: SecretSet,
    produced: Arc<Mutex<Vec<Arc<Entry>>>>,
}

impl CallOutputs {
    /// A call's outputs in `store`, masked with `secrets` (the agent's
    /// `ToolServices.mask.secrets()`).
    pub fn new(store: Arc<OutputStore>, secrets: SecretSet) -> Self {
        Self {
            store,
            secrets,
            produced: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// Starts one output of this call.
    pub(crate) fn record(&self) -> OutputRecorder {
        let recorder = self.store.start(self.secrets.clone());
        self.produced
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push(recorder.entry());
        recorder
    }
}

impl ToolOutputsService for CallOutputs {
    fn produced(&self) -> Vec<OutputInfo> {
        self.produced
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
            .iter()
            .map(|entry| entry.report())
            .collect()
    }

    fn describe(&self, handle: &str) -> Result<OutputInfo, OutputError> {
        self.store.describe(handle)
    }

    fn page(&self, handle: &str, offset: u64, limit: u32) -> Result<OutputPage, OutputError> {
        self.store.page(handle, offset, limit)
    }
}

/// Links the `tool-outputs` interface to the call's [`ToolOutputsService`]. Called by
/// [`capability_linker`](crate::capabilities::capability_linker) only when the manifest grants
/// `tool-outputs` and a service was given. `describe` and `page` read files, so they run on
/// Tokio's blocking pool rather than the module's executor.
pub(crate) fn link_tool_outputs(linker: &mut Linker<CallState>) -> wasmtime::Result<()> {
    let mut outputs = linker.instance(&interface_import("tool-outputs"))?;
    outputs.func_new_async("produced", |store, _ty, params, results| {
        let shape = check_arity("tool-outputs.produced", params, results, 0, 1);
        let service = store.data().tool_outputs.clone();
        Box::new(async move {
            shape?;
            let Some(service) = service else {
                bail!("tool-outputs.produced called without a tool-outputs service");
            };
            // It may wait, bounded, for the writer of an output whose command just ended.
            let produced = tokio::task::spawn_blocking(move || service.produced()).await?;
            results[0] = Val::List(produced.into_iter().map(info_val).collect());
            Ok(())
        })
    })?;
    outputs.func_new_async("describe", |store, _ty, params, results| {
        let service = store.data().tool_outputs.clone();
        let handle = check_arity("tool-outputs.describe", params, results, 1, 1)
            .and_then(|()| string(&params[0], "tool-outputs.describe: handle"));
        Box::new(async move {
            let handle = handle?;
            let Some(service) = service else {
                bail!("tool-outputs.describe called without a tool-outputs service");
            };
            let described = tokio::task::spawn_blocking(move || service.describe(&handle)).await?;
            results[0] = match described {
                Ok(info) => Val::Result(Ok(Some(Box::new(info_val(info))))),
                Err(error) => Val::Result(Err(Some(Box::new(error_val(error))))),
            };
            Ok(())
        })
    })?;
    outputs.func_new_async("page", |store, _ty, params, results| {
        let service = store.data().tool_outputs.clone();
        let request = check_arity("tool-outputs.page", params, results, 3, 1).and_then(|()| {
            let handle = string(&params[0], "tool-outputs.page: handle")?;
            let (Val::U64(offset), Val::U32(limit)) = (&params[1], &params[2]) else {
                bail!("tool-outputs.page: offset or limit has the wrong type");
            };
            Ok((handle, *offset, *limit))
        });
        Box::new(async move {
            let (handle, offset, limit) = request?;
            let Some(service) = service else {
                bail!("tool-outputs.page called without a tool-outputs service");
            };
            let page =
                tokio::task::spawn_blocking(move || service.page(&handle, offset, limit)).await?;
            results[0] = match page {
                Ok(page) => Val::Result(Ok(Some(Box::new(page_val(page))))),
                Err(error) => Val::Result(Err(Some(Box::new(error_val(error))))),
            };
            Ok(())
        })
    })
}

fn string(value: &Val, what: &str) -> wasmtime::Result<String> {
    match value {
        Val::String(text) => Ok(text.clone()),
        _ => bail!("{what} is not a string"),
    }
}

fn info_val(info: OutputInfo) -> Val {
    Val::Record(vec![
        ("handle".to_owned(), Val::String(info.handle)),
        ("stored-bytes".to_owned(), Val::U64(info.stored_bytes)),
        (
            "capture".to_owned(),
            Val::Enum(
                match info.capture {
                    Capture::Complete => "complete",
                    Capture::StoredCapReached => "stored-cap-reached",
                    Capture::StorageIncomplete => "storage-incomplete",
                    Capture::StorageFailed => "storage-failed",
                }
                .to_owned(),
            ),
        ),
    ])
}

fn page_val(page: OutputPage) -> Val {
    Val::Record(vec![
        ("text".to_owned(), Val::String(page.text)),
        ("next-offset".to_owned(), Val::U64(page.next_offset)),
        ("at-end".to_owned(), Val::Bool(page.at_end)),
    ])
}

fn error_val(error: OutputError) -> Val {
    let (case, payload) = match error {
        OutputError::UnknownOutput => ("unknown-output", None),
        OutputError::LimitTooSmall => ("limit-too-small", None),
        OutputError::OffsetPastEnd(stored) => ("offset-past-end", Some(Val::U64(stored))),
        OutputError::OffsetInsideCharacter => ("offset-inside-character", None),
        OutputError::ReadFailed(reason) => ("read-failed", Some(Val::String(reason))),
    };
    Val::Variant(case.to_owned(), payload.map(Box::new))
}

#[cfg(test)]
mod tests;
