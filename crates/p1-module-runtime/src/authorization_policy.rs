//! `WasmAuthorizationPolicy`: the adapter over one loaded `authorization-policy` component
//! (world `p1:module/authorization-policy@1.0.0`, decision S0-R2.1).
//!
//! The component answers `permit`, `deny(reason)` or `ask`; this adapter returns that answer
//! as a native [`Verdict`] and resolves nothing itself. `ask` is the host's to resolve: the
//! native ask bridge (`p1-host`'s `policy.rs`) asks through the current front end, or denies
//! headless, so only `Permit` or `Deny` ever reaches the core (ADR-0024). That is why this is
//! not a `p1_contracts::AuthorizationPolicy`.
//!
//! Built like [`WasmTool`](crate::WasmTool)'s `execute`: every call goes through the executor
//! ([`crate::executor`], ADR-0015) on a fresh Store and instance, bounded by
//! [`ExecutionLimits`], and [`WasmAuthorizationPolicy::verdict`] is a `Send` boxed future.
//!
//! - The call crosses as its `tool-call` wire JSON, the identity as the `tool-identity` record
//!   and the effect as the WIT enum.
//! - A trap, a fuel or deadline stop, or an answer that is not a `verdict` is a `Deny` whose
//!   reason names the policy package, never a `Permit`.
//! - Only what the manifest grants is linked, and a grant outside the class allocation
//!   (`control`, `clock`, `notices`) is refused, so a policy has no filesystem, transport,
//!   credential, process or worker capability. No service is passed to the linker at all.
//! - The adapter has no cancellation parameter: authorization belongs to the active turn's
//!   cancellation scope, which the ask bridge holds and races. Dropping the future of
//!   [`WasmAuthorizationPolicy::verdict`] abandons the call and drops its Store.

use std::sync::Arc;

use p1_contracts::serde_json;
use p1_contracts::{AuthorizationRequest, BoxFuture, CancellationToken, Effect, ToolCall};
use p1_module_protocol::{ModuleFailure, WireToolCall};
use thiserror::Error;
use wasmtime::component::Val;

use crate::capabilities::{LinkError, Services, capability_linker};
use crate::executor::{ExecutionLimits, Executor};
use crate::loader::{Epochs, LoadedModule, ModuleKind};
use crate::manifest::Digest;

/// The capabilities the `authorization-policy` class may be granted (freeze item 13,
/// `modules/capabilities.toml`), besides the type-only `types`.
pub const AUTHORIZATION_POLICY_ALLOCATION: [&str; 3] = ["control", "clock", "notices"];

/// A policy component's answer. Wider than `p1_contracts::Decision` by `Ask`, which the host
/// resolves before anything reaches the core.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// The call may run.
    Permit,
    /// Nothing is executed; the reason is shown to the model.
    Deny(String),
    /// The host decides, through the front end or headless.
    Ask,
}

/// Why a loaded module could not become an authorization policy.
#[derive(Debug, Error)]
pub enum AuthorizationPolicyError {
    /// The module is not of the `authorization-policy` class.
    #[error("module {name} is a {kind} module, not an authorization policy")]
    NotAPolicy {
        /// The module.
        name: String,
        /// Its class.
        kind: &'static str,
    },
    /// The manifest grants a capability outside the class allocation.
    #[error("module {name}: capability {capability} is not in the authorization-policy allocation")]
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
    /// The component does not fit the `authorization-policy` world as linked.
    #[error("module {name} cannot be instantiated: {reason}")]
    Instantiate {
        /// The module.
        name: String,
        /// wasmtime's message.
        reason: String,
    },
    /// No Tokio runtime is current, so the executor has nowhere to run.
    #[error(
        "module {name}: an authorization policy must be built inside a Tokio runtime, which runs its executor"
    )]
    NoRuntime {
        /// The module.
        name: String,
    },
}

/// The adapter over one loaded authorization-policy component.
pub struct WasmAuthorizationPolicy {
    name: String,
    digest: Digest,
    executor: Executor,
    /// Deadlines advance only while the epoch clock lives; the policy may outlive its loader.
    _epochs: Arc<Epochs>,
}

impl WasmAuthorizationPolicy {
    /// Builds the policy for `module`, linking only the capabilities its manifest grants.
    /// Must be called inside a Tokio runtime, which runs the policy's executor.
    pub fn new(
        module: &LoadedModule,
        limits: ExecutionLimits,
    ) -> Result<Self, AuthorizationPolicyError> {
        let name = module.name().to_owned();
        if module.kind() != ModuleKind::AuthorizationPolicy {
            return Err(AuthorizationPolicyError::NotAPolicy {
                name,
                kind: module.kind().name(),
            });
        }
        if let Some(capability) = module
            .capabilities()
            .iter()
            .find(|capability| !AUTHORIZATION_POLICY_ALLOCATION.contains(&capability.as_str()))
        {
            return Err(AuthorizationPolicyError::Capability {
                name,
                capability: capability.clone(),
            });
        }
        let handle = tokio::runtime::Handle::try_current()
            .map_err(|_| AuthorizationPolicyError::NoRuntime { name: name.clone() })?;
        // No service: a policy is granted nothing a service backs.
        let services = Services::default();
        let linker = capability_linker(&module.engine, module.capabilities(), &services).map_err(
            |source| AuthorizationPolicyError::Link {
                name: name.clone(),
                source,
            },
        )?;
        let pre = linker.instantiate_pre(&module.component).map_err(|error| {
            AuthorizationPolicyError::Instantiate {
                name: name.clone(),
                reason: format!("{error:#}"),
            }
        })?;
        let executor = Executor::start(
            &handle,
            module.engine.clone(),
            module.epochs.clone(),
            pre,
            services,
            limits,
        );
        Ok(Self {
            name,
            digest: module.digest(),
            executor,
            _epochs: module.epochs.clone(),
        })
    }

    /// The policy package's manifest name, e.g. `p1/policy/ask`.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The digest of the policy's verified component bytes: its identity.
    pub fn digest(&self) -> Digest {
        self.digest
    }

    /// The component's answer for `request`. Every failure is a `Deny` naming the policy.
    pub fn verdict<'a>(&'a self, request: AuthorizationRequest<'a>) -> BoxFuture<'a, Verdict> {
        Box::pin(async move {
            let Some(call) = wire_call(request.call) else {
                return self.failed(&ModuleFailure::InvalidOutput(
                    "the call cannot be serialized".to_owned(),
                ));
            };
            let identity = Val::Record(vec![
                (
                    "implementation".to_owned(),
                    Val::String(request.identity.implementation.clone()),
                ),
                (
                    "variant".to_owned(),
                    Val::String(request.identity.variant.clone()),
                ),
            ]);
            let effect = Val::Enum(effect_case(request.effect).to_owned());
            // Never cancelled: the turn's scope is the caller's to race, and dropping this
            // future abandons the call.
            let results = self
                .executor
                .call(
                    "authorize",
                    vec![Val::String(call), identity, effect],
                    CancellationToken::new(),
                )
                .await;
            match results {
                Ok(results) => verdict_of(results.into_iter().next())
                    .unwrap_or_else(|failure| self.failed(&failure)),
                Err(failure) => self.failed(&failure),
            }
        })
    }

    fn failed(&self, failure: &ModuleFailure) -> Verdict {
        Verdict::Deny(format!(
            "authorization policy {} failed: {failure}",
            self.name
        ))
    }
}

/// The wire text of a call, as the module reads it.
fn wire_call(call: &ToolCall) -> Option<String> {
    serde_json::to_string(&WireToolCall::from(call.clone())).ok()
}

/// The `effect` enum case of `effect`.
fn effect_case(effect: Effect) -> &'static str {
    match effect {
        Effect::ReadOnly => "read-only",
        Effect::WritesFiles => "writes-files",
        Effect::Executes => "executes",
        Effect::Delegates => "delegates",
    }
}

/// Reads the `verdict` variant; anything else is the module's invalid output.
fn verdict_of(value: Option<Val>) -> Result<Verdict, ModuleFailure> {
    let invalid = |what: &str| ModuleFailure::InvalidOutput(format!("authorize returned {what}"));
    match value {
        Some(Val::Variant(case, payload)) => match (case.as_str(), payload.map(|value| *value)) {
            ("permit", None) => Ok(Verdict::Permit),
            ("deny", Some(Val::String(reason))) => Ok(Verdict::Deny(reason)),
            ("ask", None) => Ok(Verdict::Ask),
            _ => Err(invalid("an unknown verdict")),
        },
        Some(_) => Err(invalid("something other than a verdict")),
        None => Err(invalid("nothing")),
    }
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use p1_contracts::serde_json::{Value, json};
    use p1_contracts::{ToolIdentity, ToolInput};

    use super::*;
    use crate::{Loader, ReleaseManifest};

    const FULL_ACCESS: (&str, &str) = ("p1-module-policy-full-access", "p1/policy/full-access");
    const ASK: (&str, &str) = ("p1-module-policy-ask", "p1/policy/ask");
    const EFFECTS: [Effect; 4] = [
        Effect::ReadOnly,
        Effect::WritesFiles,
        Effect::Executes,
        Effect::Delegates,
    ];

    /// Where `scripts/build-modules.sh` publishes the packages.
    fn built() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../modules/target/p1-modules")
    }

    /// Reads build output `file` of `package`, or fails the case with how to build it.
    fn output(package: &str, file: &str) -> String {
        let path = built().join(package).join(file);
        std::fs::read_to_string(&path).unwrap_or_else(|error| {
            panic!(
                "the build output {} is missing ({error}): run scripts/build-modules.sh first",
                path.display()
            )
        })
    }

    /// The built package manifest of `package`.
    fn package_manifest(package: &str) -> Value {
        serde_json::from_str(&output(package, &format!("{package}.manifest.json")))
            .expect("the package manifest is JSON")
    }

    /// A release manifest entry for the built `package`, with `capabilities` if given.
    fn entry(package: &str, capabilities: Option<Value>) -> Value {
        let manifest = package_manifest(package);
        json!({
            "name": manifest["name"],
            "digest": manifest["digest"],
            "path": format!("{package}/{package}.wasm"),
            "kind": manifest["kind"],
            "world": manifest["world"],
            "protocol": manifest["protocol"],
            "capabilities": capabilities.unwrap_or_else(|| manifest["capabilities"].clone()),
            "variant": manifest["variant"],
        })
    }

    /// A loader over the build outputs, as a release lays them out.
    fn loader(entries: Vec<Value>) -> Loader {
        let manifest = json!({ "format": "p1-release-manifest/1", "components": entries });
        let manifest = ReleaseManifest::parse(&manifest.to_string()).expect("release manifest");
        Loader::new(manifest, built()).expect("loader")
    }

    fn policy(package: (&str, &str), limits: ExecutionLimits) -> WasmAuthorizationPolicy {
        let module = loader(vec![entry(package.0, None)])
            .load(package.1)
            .expect("the built policy loads");
        WasmAuthorizationPolicy::new(&module, limits).expect("the policy adapter builds")
    }

    fn call() -> ToolCall {
        ToolCall {
            call_id: "c1".to_owned(),
            name: "shell".to_owned(),
            input: ToolInput::Text("ls".to_owned()),
        }
    }

    fn identity() -> ToolIdentity {
        ToolIdentity {
            implementation: "p1/shell".to_owned(),
            variant: "default".to_owned(),
        }
    }

    async fn ask(policy: &WasmAuthorizationPolicy, effect: Effect) -> Verdict {
        let call = call();
        let identity = identity();
        policy
            .verdict(AuthorizationRequest {
                call: &call,
                identity: &identity,
                effect,
            })
            .await
    }

    #[tokio::test]
    async fn full_access_permits_every_effect() {
        let policy = policy(FULL_ACCESS, ExecutionLimits::default());
        assert_eq!(policy.name(), "p1/policy/full-access");
        for effect in EFFECTS {
            assert_eq!(ask(&policy, effect).await, Verdict::Permit, "{effect:?}");
        }
    }

    #[tokio::test]
    async fn ask_permits_read_only_and_asks_for_the_rest() {
        let policy = policy(ASK, ExecutionLimits::default());
        assert_eq!(policy.name(), "p1/policy/ask");
        for effect in EFFECTS {
            let expected = if effect == Effect::ReadOnly {
                Verdict::Permit
            } else {
                Verdict::Ask
            };
            assert_eq!(ask(&policy, effect).await, expected, "{effect:?}");
        }
    }

    #[tokio::test]
    async fn a_fuel_stop_is_a_deny_naming_the_policy() {
        for package in [FULL_ACCESS, ASK] {
            let policy = policy(
                package,
                ExecutionLimits {
                    fuel: 1,
                    ..ExecutionLimits::default()
                },
            );
            match ask(&policy, Effect::Executes).await {
                Verdict::Deny(reason) => assert!(
                    reason.starts_with(&format!("authorization policy {} failed: ", package.1)),
                    "{reason}"
                ),
                other => panic!("{}: a stopped call must deny, got {other:?}", package.1),
            }
        }
    }

    #[test]
    fn an_answer_that_is_not_a_verdict_is_invalid_output() {
        let variant = |case: &str, payload: Option<Val>| {
            Some(Val::Variant(case.to_owned(), payload.map(Box::new)))
        };
        assert_eq!(verdict_of(variant("permit", None)), Ok(Verdict::Permit));
        assert_eq!(verdict_of(variant("ask", None)), Ok(Verdict::Ask));
        assert_eq!(
            verdict_of(variant("deny", Some(Val::String("no".to_owned())))),
            Ok(Verdict::Deny("no".to_owned()))
        );
        for unknown in [
            variant("maybe", None),
            variant("deny", None),
            variant("permit", Some(Val::Bool(true))),
            Some(Val::Bool(true)),
            None,
        ] {
            assert!(
                matches!(
                    verdict_of(unknown.clone()),
                    Err(ModuleFailure::InvalidOutput(_))
                ),
                "{unknown:?}"
            );
        }
    }

    #[tokio::test]
    async fn a_grant_outside_the_allocation_is_refused() {
        for capability in ["random", "process"] {
            let module = loader(vec![entry(
                FULL_ACCESS.0,
                Some(json!(["control", capability])),
            )])
            .load(FULL_ACCESS.1)
            .expect("the loader links what the runtime can link");
            match WasmAuthorizationPolicy::new(&module, ExecutionLimits::default()) {
                Err(AuthorizationPolicyError::Capability {
                    capability: refused,
                    ..
                }) => assert_eq!(refused, capability),
                Err(other) => panic!("{capability}: wrong refusal {other}"),
                Ok(_) => panic!("{capability}: a policy must not be granted it"),
            }
        }
    }

    #[test]
    fn the_built_policies_import_only_types_and_granted_interfaces() {
        for (package, _) in [FULL_ACCESS, ASK] {
            let manifest = package_manifest(package);
            let granted: Vec<&str> = manifest["capabilities"]
                .as_array()
                .expect("capabilities is a list")
                .iter()
                .map(|capability| capability.as_str().expect("a capability name"))
                .collect();
            for capability in &granted {
                assert!(
                    AUTHORIZATION_POLICY_ALLOCATION.contains(capability),
                    "{package} is granted {capability}"
                );
            }
            let imports = output(package, &format!("{package}.imports"));
            for import in imports.lines().filter(|line| !line.trim().is_empty()) {
                let interface = import
                    .strip_prefix("p1:module/")
                    .and_then(|rest| rest.strip_suffix("@1.0.0"))
                    .unwrap_or_else(|| panic!("{package} imports {import}"));
                assert!(
                    interface == "types" || granted.contains(&interface),
                    "{package} imports {import}, which its manifest does not grant"
                );
            }
        }
    }
}
