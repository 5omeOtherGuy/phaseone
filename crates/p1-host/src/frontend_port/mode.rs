//! The session's permission mode for a front end (#696): how the parent's and its
//! workers' tool calls are authorized, chosen live from the levels the run's own
//! start-up flags allow (ADR-0038), never wider.
//!
//! - `ask`: the front end's own policy answers every call (it asks the user). The
//!   default: what a front end did before modes existed.
//! - `read-only`: a call whose effect is `ReadOnly` runs; every other call is refused
//!   without asking. The `--ask` policy permits reads unasked too, so no start-up
//!   allows less.
//! - `full-access`: every call runs unasked: the full-access policy ADR-0038 makes the
//!   default without `--ask`. Offered only to a run started without `--ask`.
//!
//! The shell sandbox is not a mode: `--sandbox` fixes it per process, since the shell
//! is assembled with it. Confinement (ADR-0025) and the credential refusal hold in
//! every mode, below the policy.

use std::sync::{Arc, Mutex};

use p1_contracts::frontend::{ConfigChoice, ConfigKind, ConfigValue};
use p1_contracts::{AuthorizationPolicy, AuthorizationRequest, BoxFuture, Decision, Effect};

use crate::cli::SandboxMode;

/// The reason a read-only session refuses a call, shown to the model.
pub const READ_ONLY_DENY: &str =
    "Not permitted in read-only mode: this call writes, executes or delegates.";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Mode {
    Ask,
    ReadOnly,
    FullAccess,
}

impl Mode {
    pub(crate) fn id(self) -> &'static str {
        match self {
            Mode::Ask => "ask",
            Mode::ReadOnly => "read-only",
            Mode::FullAccess => "full-access",
        }
    }

    /// The modes a run may switch between: `full-access` only without `--ask`.
    fn allowed(ask: bool) -> &'static [Mode] {
        if ask {
            &[Mode::Ask, Mode::ReadOnly]
        } else {
            &[Mode::Ask, Mode::ReadOnly, Mode::FullAccess]
        }
    }

    fn description(self, sandbox: SandboxMode) -> String {
        match self {
            Mode::Ask => "Ask before every tool call".to_string(),
            Mode::ReadOnly => {
                "Tools that only read run; every other call is refused without asking".to_string()
            }
            Mode::FullAccess => format!(
                "Every tool call runs without asking; shell commands run {} (fixed by --sandbox)",
                match sandbox {
                    SandboxMode::Off => "without a sandbox",
                    SandboxMode::Workspace => "in the workspace sandbox",
                }
            ),
        }
    }
}

/// The session's mode, shared by the policy that reads it at every call and the
/// session that changes it.
pub(crate) struct ModeCell(Mutex<Mode>);

impl Default for ModeCell {
    fn default() -> Self {
        Self(Mutex::new(Mode::Ask))
    }
}

impl ModeCell {
    pub(crate) fn get(&self) -> Mode {
        *self.0.lock().unwrap()
    }

    /// The mode option, offering what `ask` (the run's `--ask`) allows.
    pub(crate) fn choice(&self, ask: bool, sandbox: SandboxMode) -> ConfigChoice {
        ConfigChoice {
            kind: ConfigKind::Mode,
            current: self.get().id().to_string(),
            values: Mode::allowed(ask)
                .iter()
                .map(|mode| ConfigValue {
                    value: mode.id().to_string(),
                    name: mode.id().to_string(),
                    description: Some(mode.description(sandbox)),
                })
                .collect(),
        }
    }

    /// Switch to `value`; a mode the run's `--ask` does not allow is refused, whatever
    /// the front end checked before.
    pub(crate) fn set(&self, ask: bool, value: &str) -> Result<Mode, String> {
        let mode = Mode::allowed(ask)
            .iter()
            .copied()
            .find(|mode| mode.id() == value)
            .ok_or_else(|| format!("`{value}` is not a mode this run allows"))?;
        *self.0.lock().unwrap() = mode;
        Ok(mode)
    }
}

/// The front end's policy under the session's mode: the mode is read at every call,
/// so a change applies from the next one.
pub(crate) struct ModePolicy {
    pub(crate) inner: Arc<dyn AuthorizationPolicy>,
    pub(crate) mode: Arc<ModeCell>,
}

impl AuthorizationPolicy for ModePolicy {
    fn authorize<'a>(&'a self, request: AuthorizationRequest<'a>) -> BoxFuture<'a, Decision> {
        match self.mode.get() {
            Mode::Ask => self.inner.authorize(request),
            Mode::ReadOnly if request.effect == Effect::ReadOnly => {
                Box::pin(async { Decision::Permit })
            }
            Mode::ReadOnly => Box::pin(async {
                Decision::Deny {
                    reason: READ_ONLY_DENY.to_string(),
                }
            }),
            Mode::FullAccess => Box::pin(async { Decision::Permit }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use p1_contracts::{ToolCall, ToolIdentity, ToolInput};
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Counts the calls it is asked about and denies them.
    #[derive(Default)]
    struct Asking(AtomicUsize);

    impl AuthorizationPolicy for Asking {
        fn authorize<'a>(&'a self, _request: AuthorizationRequest<'a>) -> BoxFuture<'a, Decision> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Box::pin(async {
                Decision::Deny {
                    reason: "asked".to_string(),
                }
            })
        }
    }

    fn decide(policy: &ModePolicy, effect: Effect) -> Decision {
        let call = ToolCall {
            call_id: "c1".into(),
            name: "tool".into(),
            input: ToolInput::Json("{}".into()),
        };
        let identity = ToolIdentity {
            implementation: "tool".into(),
            variant: "default".into(),
        };
        futures_executor_block_on(policy.authorize(AuthorizationRequest {
            call: &call,
            identity: &identity,
            effect,
        }))
    }

    fn futures_executor_block_on<T>(future: BoxFuture<'_, T>) -> T {
        tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap()
            .block_on(future)
    }

    #[test]
    fn the_mode_decides_the_next_call_and_ask_asks() {
        let asking = Arc::new(Asking::default());
        let mode = Arc::new(ModeCell::default());
        let policy = ModePolicy {
            inner: asking.clone(),
            mode: mode.clone(),
        };
        assert!(
            matches!(decide(&policy, Effect::WritesFiles), Decision::Deny { reason } if reason == "asked")
        );
        assert_eq!(asking.0.load(Ordering::SeqCst), 1);

        mode.set(false, "read-only").unwrap();
        assert_eq!(decide(&policy, Effect::ReadOnly), Decision::Permit);
        for effect in [Effect::WritesFiles, Effect::Executes, Effect::Delegates] {
            assert_eq!(
                decide(&policy, effect),
                Decision::Deny {
                    reason: READ_ONLY_DENY.to_string()
                }
            );
        }
        mode.set(false, "full-access").unwrap();
        assert_eq!(decide(&policy, Effect::Executes), Decision::Permit);
        // Neither mode asked the user.
        assert_eq!(asking.0.load(Ordering::SeqCst), 1);

        mode.set(false, "ask").unwrap();
        decide(&policy, Effect::ReadOnly);
        assert_eq!(asking.0.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn a_run_started_with_ask_never_offers_or_takes_full_access() {
        let mode = ModeCell::default();
        let offered: Vec<_> = mode
            .choice(true, SandboxMode::Off)
            .values
            .into_iter()
            .map(|value| value.value)
            .collect();
        assert_eq!(offered, ["ask", "read-only"]);
        assert!(mode.set(true, "full-access").is_err());
        assert_eq!(mode.get(), Mode::Ask, "a refused change changes nothing");
        let offered = mode.choice(false, SandboxMode::Workspace);
        assert_eq!(offered.values.len(), 3);
        assert!(
            offered.values[2]
                .description
                .as_deref()
                .unwrap()
                .contains("workspace sandbox")
        );
    }
}
