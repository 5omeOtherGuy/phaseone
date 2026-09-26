//! `WasmWorkflowDecisions`: the adapter from one loaded `workflow-decision` component (world
//! `p1:module/workflow-decision@1.0.0`, S0-R1.1) to `p1_workflow::Decisions`, so the native
//! substrate of `p1-workflow` asks the component what a step does next while it keeps every
//! piece of state itself.
//!
//! Why this adapter calls synchronously and not through the executor: `Decisions` is a
//! synchronous seam, called on the engine's own OS threads (the script thread and the
//! `parallel`/`pipeline` thunk threads), not on a Tokio worker. Blocking such a thread on the
//! async executor would need a runtime handle there and would tie a decision to the host's
//! scheduler; a decision is pure, short and imports nothing that waits, so it runs as one
//! synchronous wasmtime call on the calling thread, the way the restricted path runs its
//! inspections ([`crate::restricted`]).
//!
//! - **One instance per call.** Every `plan-step` and `accept-step` builds a fresh Store and
//!   instance from one `InstancePre`, calls the export once and drops both. No instance is
//!   ever shared between threads, entered twice, or held while the substrate returns into
//!   the script, so a Rhai callback can never run inside one and a trap poisons nothing for
//!   the next call. [`WasmWorkflowDecisions::instances`] counts them, so a test can see that
//!   each call had its own.
//! - **Bounded.** Each call gets [`ExecutionLimits::fuel`] and a deadline of
//!   [`ExecutionLimits::deadline`] on the engine's epoch clock; running out is the call's
//!   error, naming the module.
//! - **Only `control` and `clock`.** A grant outside that class allocation is refused. Both
//!   are linked as synchronous host functions (a Store with an async definition would refuse
//!   a synchronous call): `control.cancelled` is always false, because a decision call has no
//!   cancellation of its own to forward — the substrate checks the run's cancellation between
//!   calls — and `clock` answers like the runtime's own capability. Anything else the
//!   component could import is already refused by the loader.
//! - The snapshot, request and outcome cross as the JSON of `p1_workflow::decision`'s
//!   contract types, which the component deserializes with the same type definitions (it
//!   compiles them from `p1-workflow` by path); an `err` from the component, a trap or an
//!   answer that is not a transition is the call's `Err`, which the substrate reports.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use p1_contracts::serde_json;
use p1_workflow::Decisions;
use p1_workflow::decision::{AttemptOutcome, PlanRequest, Snapshot, Transition};
use thiserror::Error;
use wasmtime::component::{Func, InstancePre, Linker, Val};
use wasmtime::{Engine, Store};

use crate::executor::ExecutionLimits;
use crate::loader::{EPOCH_TICK, Epochs, LoadedModule, ModuleKind, interface_import};
use crate::manifest::Digest;

/// The capabilities the `workflow-decision` class may be granted (freeze item 13,
/// `modules/capabilities.toml`), besides the type-only `types`.
pub const WORKFLOW_DECISION_ALLOCATION: [&str; 2] = ["control", "clock"];

/// Why a loaded module could not become the workflow's decisions.
#[derive(Debug, Error)]
pub enum WorkflowDecisionError {
    /// The module is not of the `workflow-decision` class.
    #[error("module {name} is a {kind} module, not a workflow decision")]
    NotADecision {
        /// The module.
        name: String,
        /// Its class.
        kind: &'static str,
    },
    /// The manifest grants a capability outside the class allocation.
    #[error("module {name}: capability {capability} is not in the workflow-decision allocation")]
    Capability {
        /// The module.
        name: String,
        /// The capability as written.
        capability: String,
    },
    /// The component does not fit the `workflow-decision` world as linked.
    #[error("module {name} cannot be instantiated: {reason}")]
    Instantiate {
        /// The module.
        name: String,
        /// wasmtime's message.
        reason: String,
    },
}

/// What one call's Store holds: the origin of `clock.monotonic-now`, fixed per instance as
/// `clock` documents.
struct CallData {
    origin: Instant,
}

/// The adapter over one loaded workflow-decision component.
pub struct WasmWorkflowDecisions {
    name: String,
    digest: Digest,
    engine: Engine,
    pre: InstancePre<CallData>,
    fuel: u64,
    deadline_ticks: u64,
    instances: AtomicU64,
    /// Deadlines advance only while the epoch clock lives; the adapter may outlive its loader.
    _epochs: Arc<Epochs>,
}

impl WasmWorkflowDecisions {
    /// Builds the decisions for `module`, linking only the capabilities its manifest grants.
    pub fn new(
        module: &LoadedModule,
        limits: ExecutionLimits,
    ) -> Result<Self, WorkflowDecisionError> {
        let name = module.name().to_owned();
        if module.kind() != ModuleKind::WorkflowDecision {
            return Err(WorkflowDecisionError::NotADecision {
                name,
                kind: module.kind().name(),
            });
        }
        if let Some(capability) = module
            .capabilities()
            .iter()
            .find(|capability| !WORKFLOW_DECISION_ALLOCATION.contains(&capability.as_str()))
        {
            return Err(WorkflowDecisionError::Capability {
                name,
                capability: capability.clone(),
            });
        }
        let instantiate = |error: wasmtime::Error| WorkflowDecisionError::Instantiate {
            name: name.clone(),
            reason: format!("{error:#}"),
        };
        let linker = linker(&module.engine, module.capabilities()).map_err(instantiate)?;
        let pre = linker
            .instantiate_pre(&module.component)
            .map_err(instantiate)?;
        // At least one tick: a zero deadline would stop every call before it starts.
        let deadline_ticks = u64::try_from(limits.deadline.as_millis() / EPOCH_TICK.as_millis())
            .unwrap_or(u64::MAX)
            .max(1);
        Ok(Self {
            name,
            digest: module.digest(),
            engine: module.engine.clone(),
            pre,
            fuel: limits.fuel,
            deadline_ticks,
            instances: AtomicU64::new(0),
            _epochs: module.epochs.clone(),
        })
    }

    /// The decision package's manifest name, e.g. `p1/workflow-decision`.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The digest of the component's verified bytes: its identity.
    pub fn digest(&self) -> Digest {
        self.digest
    }

    /// How many instances the adapter has built: one per call, never reused.
    pub fn instances(&self) -> u64 {
        self.instances.load(Ordering::SeqCst)
    }

    /// Calls export `export` with two JSON texts on a fresh instance and reads the
    /// transition it answers.
    fn decide(&self, export: &str, first: String, second: String) -> Result<Transition, String> {
        let failed = |why: String| format!("workflow decision {} failed: {why}", self.name);
        let answer = self
            .call(export, first, second)
            .map_err(|error| failed(format!("{export}: {error:#}")))?;
        let text = match answer {
            Val::Result(Ok(Some(text))) => match *text {
                Val::String(text) => text,
                _ => return Err(failed(format!("{export} returned no JSON text"))),
            },
            // The component's own refusal is passed on as it wrote it.
            Val::Result(Err(Some(reason))) => match *reason {
                Val::String(reason) => return Err(reason),
                _ => {
                    return Err(failed(format!(
                        "{export} returned an error that is not text"
                    )));
                }
            },
            _ => {
                return Err(failed(format!(
                    "{export} returned something other than a result"
                )));
            }
        };
        serde_json::from_str(&text).map_err(|error| failed(format!("{export} answer: {error}")))
    }

    fn call(&self, export: &str, first: String, second: String) -> wasmtime::Result<Val> {
        let mut store = Store::new(
            &self.engine,
            CallData {
                origin: Instant::now(),
            },
        );
        store.set_fuel(self.fuel)?;
        store.epoch_deadline_trap();
        store.set_epoch_deadline(self.deadline_ticks);
        self.instances.fetch_add(1, Ordering::SeqCst);
        let instance = self.pre.instantiate(&mut store)?;
        let func: Func = instance
            .get_func(&mut store, export)
            .ok_or_else(|| wasmtime::format_err!("the module exports no {export}"))?;
        let mut results = [Val::Bool(false)];
        func.call(
            &mut store,
            &[Val::String(first), Val::String(second)],
            &mut results,
        )?;
        let [result] = results;
        Ok(result)
    }
}

impl Decisions for WasmWorkflowDecisions {
    fn plan_step(&self, snapshot: &Snapshot, request: &PlanRequest) -> Result<Transition, String> {
        self.decide(
            "plan-step",
            serde_json::to_string(snapshot).map_err(input)?,
            serde_json::to_string(request).map_err(input)?,
        )
    }

    fn accept_step(
        &self,
        snapshot: &Snapshot,
        outcome: &AttemptOutcome,
    ) -> Result<Transition, String> {
        self.decide(
            "accept-step",
            serde_json::to_string(snapshot).map_err(input)?,
            serde_json::to_string(outcome).map_err(input)?,
        )
    }
}

/// The contract types always serialize; an error here is a bug, reported rather than hidden.
fn input(error: serde_json::Error) -> String {
    format!("decision input: {error}")
}

/// A linker with the granted `control` and `clock`, both synchronous (see the module
/// documentation).
fn linker(engine: &Engine, granted: &[String]) -> wasmtime::Result<Linker<CallData>> {
    let mut linker: Linker<CallData> = Linker::new(engine);
    for capability in granted {
        match capability.as_str() {
            "control" => {
                let mut control = linker.instance(&interface_import("control"))?;
                control.func_wrap("cancelled", |_store, (): ()| Ok((false,)))?;
            }
            "clock" => {
                let mut clock = linker.instance(&interface_import("clock"))?;
                clock.func_new("now", |_store, _ty, _params, results| {
                    // A wall clock before 1970 is a host misconfiguration; zero says
                    // "unknown" without failing the decision.
                    let since = SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .unwrap_or_default();
                    results[0] = Val::Record(vec![
                        ("seconds".to_owned(), Val::U64(since.as_secs())),
                        ("nanoseconds".to_owned(), Val::U32(since.subsec_nanos())),
                    ]);
                    Ok(())
                })?;
                clock.func_wrap("monotonic-now", |store, (): ()| {
                    let elapsed = store.data().origin.elapsed().as_nanos();
                    Ok((u64::try_from(elapsed).unwrap_or(u64::MAX),))
                })?;
            }
            // `new` refused every other grant before linking.
            _ => {}
        }
    }
    Ok(linker)
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use serde_json::{Value, json};

    use super::*;
    use crate::{Loader, ReleaseManifest};

    const PACKAGE: (&str, &str) = ("p1-module-workflow-decision", "p1/workflow-decision");

    /// Where `scripts/build-modules.sh` publishes the packages.
    fn built() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../modules/target/p1-modules")
    }

    /// Reads build output `file` of the package, or fails the case with how to build it.
    fn output(file: &str) -> String {
        let path = built().join(PACKAGE.0).join(file);
        std::fs::read_to_string(&path).unwrap_or_else(|error| {
            panic!(
                "the build output {} is missing ({error}): run scripts/build-modules.sh first",
                path.display()
            )
        })
    }

    fn package_manifest() -> Value {
        serde_json::from_str(&output(&format!("{}.manifest.json", PACKAGE.0)))
            .expect("the package manifest is JSON")
    }

    fn module(capabilities: Option<Value>) -> LoadedModule {
        let manifest = package_manifest();
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
        let manifest = json!({ "format": "p1-release-manifest/1", "components": [entry] });
        let manifest = ReleaseManifest::parse(&manifest.to_string()).expect("release manifest");
        Loader::new(manifest, built())
            .expect("loader")
            .load(PACKAGE.1)
            .expect("the built decision component loads")
    }

    #[test]
    fn the_package_is_the_workflow_decision_class_with_control_and_clock() {
        let manifest = package_manifest();
        assert_eq!(manifest["kind"], "workflow-decision");
        assert_eq!(manifest["world"], "p1:module/workflow-decision@1.0.0");
        assert_eq!(manifest["capabilities"], json!(["control", "clock"]));
        let decisions = WasmWorkflowDecisions::new(&module(None), ExecutionLimits::default())
            .expect("the adapter builds");
        assert_eq!(decisions.name(), PACKAGE.1);
        assert_eq!(decisions.instances(), 0);
    }

    #[test]
    fn a_grant_outside_the_allocation_is_refused() {
        for capability in ["random", "process"] {
            match WasmWorkflowDecisions::new(
                &module(Some(json!(["control", capability]))),
                ExecutionLimits::default(),
            ) {
                Err(WorkflowDecisionError::Capability {
                    capability: refused,
                    ..
                }) => assert_eq!(refused, capability),
                Err(other) => panic!("{capability}: wrong refusal {other}"),
                Ok(_) => panic!("{capability}: a decision must not be granted it"),
            }
        }
    }

    #[test]
    fn a_fuel_stop_is_an_error_naming_the_module_and_the_next_call_is_fresh() {
        let starved = WasmWorkflowDecisions::new(
            &module(None),
            ExecutionLimits {
                fuel: 1,
                ..ExecutionLimits::default()
            },
        )
        .expect("the adapter builds");
        let error = starved
            .decide("plan-step", "{}".to_owned(), "{}".to_owned())
            .unwrap_err();
        assert!(
            error.starts_with("workflow decision p1/workflow-decision failed: plan-step"),
            "{error}"
        );
        let error = starved
            .decide("accept-step", "{}".to_owned(), "{}".to_owned())
            .unwrap_err();
        assert!(error.contains("accept-step"), "{error}");
        assert_eq!(starved.instances(), 2, "each call built its own instance");
    }

    #[test]
    fn the_components_own_refusal_is_passed_on() {
        let decisions = WasmWorkflowDecisions::new(&module(None), ExecutionLimits::default())
            .expect("the adapter builds");
        // Not a snapshot: the shared decision code refuses it, and that text comes back as
        // the component wrote it.
        let error = decisions
            .decide("plan-step", "{}".to_owned(), "{}".to_owned())
            .unwrap_err();
        assert!(error.starts_with("snapshot: "), "{error}");
        assert_eq!(decisions.instances(), 1);
    }

    #[test]
    fn the_built_component_imports_only_granted_interfaces() {
        let granted: Vec<String> =
            serde_json::from_value(package_manifest()["capabilities"].clone())
                .expect("capabilities is a list of names");
        let imports = output(&format!("{}.imports", PACKAGE.0));
        for import in imports.lines().filter(|line| !line.trim().is_empty()) {
            let interface = import
                .strip_prefix("p1:module/")
                .and_then(|rest| rest.strip_suffix("@1.0.0"))
                .unwrap_or_else(|| panic!("the component imports {import}"));
            assert!(
                interface == "types" || granted.iter().any(|granted| granted == interface),
                "the component imports {import}, which its manifest does not grant"
            );
        }
    }
}
