//! The host's execution authorization policy.
//!
//! Full access is the DEFAULT: without `--ask` every call is permitted, headless
//! and interactive (ADR-0038). `--ask` opts into the restrictive policy: headless
//! permits `ReadOnly` and denies the rest; interactive permits `ReadOnly` silently
//! and asks on the terminal for anything else, racing the turn's cancellation.
//!
//! "Ask" lives HERE, inside the policy, so the core never sees UI (D18).

use std::collections::HashSet;
use std::io::Write;
use std::sync::{Arc, Mutex};

use p1_contracts::{
    AuthorizationPolicy, AuthorizationRequest, BoxFuture, CancellationToken, Decision, Effect,
    ToolIdentity,
};

use crate::LineSource;
use crate::SharedWriter;
use crate::render::summarize_input;

/// The exact headless refusal.
pub const HEADLESS_DENY: &str = "Not permitted in headless mode with --ask.";
/// The exact interactive refusal for anything but `y`/`a`.
pub const USER_DENY: &str = "Denied by the user.";
/// Returned when the turn is cancelled while an ask is outstanding.
pub const CANCEL_DENY: &str = "Cancelled while awaiting authorization.";

/// The interactive/headless authorization policy.
pub struct HostPolicy {
    ask: bool,
    headless: bool,
    lines: Arc<dyn LineSource>,
    stderr: SharedWriter,
    /// The run's cancellation token; the ask races it.
    cancel: CancellationToken,
    /// Tool NAME + `ToolIdentity` granted with `a`lways for this process.
    always: Mutex<HashSet<(String, ToolIdentity)>>,
}

impl HostPolicy {
    pub fn new(
        ask: bool,
        headless: bool,
        lines: Arc<dyn LineSource>,
        stderr: SharedWriter,
        cancel: CancellationToken,
    ) -> Self {
        Self {
            ask,
            headless,
            lines,
            stderr,
            cancel,
            always: Mutex::new(HashSet::new()),
        }
    }

    fn ask(&self, tool: &str, summary: &str) {
        let prompt = format!("allow {tool} {summary}? [y]es / [n]o / [a]lways for this tool: ");
        let mut writer = self.stderr.lock().unwrap();
        let _ = writer.write_all(prompt.as_bytes());
        let _ = writer.flush();
    }
}

impl AuthorizationPolicy for HostPolicy {
    fn authorize<'a>(&'a self, request: AuthorizationRequest<'a>) -> BoxFuture<'a, Decision> {
        Box::pin(async move {
            if !self.ask {
                return Decision::Permit;
            }
            if request.effect == Effect::ReadOnly {
                return Decision::Permit;
            }
            if self.headless {
                return Decision::Deny {
                    reason: HEADLESS_DENY.to_string(),
                };
            }

            let key = (request.call.name.clone(), request.identity.clone());
            if self.always.lock().unwrap().contains(&key) {
                return Decision::Permit;
            }

            let summary = summarize_input(request.call.input.raw());
            self.ask(&request.call.name, &summary);
            let line = tokio::select! {
                biased;
                _ = self.cancel.cancelled() => {
                    return Decision::Deny { reason: CANCEL_DENY.to_string() };
                }
                line = self.lines.next_line() => line,
            };
            match line.as_deref().map(str::trim) {
                Some("y") | Some("yes") => Decision::Permit,
                Some("a") | Some("always") => {
                    self.always.lock().unwrap().insert(key);
                    Decision::Permit
                }
                _ => Decision::Deny {
                    reason: USER_DENY.to_string(),
                },
            }
        })
    }
}
