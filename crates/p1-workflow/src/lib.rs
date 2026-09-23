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
//! The engine behind it lands separately.

pub mod api;

pub use api::*;
