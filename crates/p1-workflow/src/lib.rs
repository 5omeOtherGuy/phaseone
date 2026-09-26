//! Optional workflows (ADR-0053): a sandboxed script — `agent(prompt, opts)`, `parallel`,
//! `pipeline`, `phase`, `log`, `args` — orchestrates workers under roles and caps.
//!
//! This crate owns the script engine, the roles/caps resolution, the step envelope, the
//! run journal with its prefix replay, cancellation and the [`WorkflowService`] trait. It
//! knows no provider, no tool and no worker implementation: a step is started through the
//! [`StepRunner`] seam the host implements over its worker service, and models are named
//! by `environment/profile[:effort]` references the host resolves through [`ModelResolver`].
//!
//! `api` is the frozen public surface: `p1-tool-workflow` depends on [`WorkflowService`],
//! the host on [`StepRunner`], [`ModelResolver`], [`WorkflowSettings`] and [`WorkflowObserver`].
//! [`InProcessWorkflows`] is the engine behind it.
//!
//! The engine is split (S6.3): [`decision`] holds the step decisions behind the
//! [`Decisions`] seam and their JSON contract; the engine is the native substrate that owns
//! every piece of state and applies the checked transitions.

pub mod api;
mod caps;
mod check;
pub mod decision;
mod engine;
mod error;
mod journal;
mod service;

pub use api::*;
pub use decision::{Decisions, NativeDecisions};
// The one piece of the journal module the host needs: it reads a run's journal to
// reserve worker ids on resume (issue #98), with this crate's exact parse semantics.
pub use journal::read_journal;
pub use service::InProcessWorkflows;
