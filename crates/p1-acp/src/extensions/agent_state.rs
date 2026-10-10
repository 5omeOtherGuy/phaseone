//! `_p1/agent_state` v1 (#683): the session's turn state, for a client that opted out
//! of the hold rule (D4). The shape follows ACP v2's `state_update`, its successor: a
//! turn of the session, prompt or inbox, makes it `running`; a request waiting for
//! the client makes it `requires_action`; background work alone leaves it `idle`.
//! The idle that ends a turn carries the turn's stop reason.

use crate::driver::io::Peer;
use serde::Serialize;
use std::sync::{Arc, Mutex};

pub(crate) const CAPABILITY: &str = "agent_state";
pub(crate) const METHOD: &str = "_p1/agent_state";

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum State {
    #[default]
    Idle,
    Running,
    RequiresAction,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Notification<'a> {
    session_id: &'a str,
    state: State,
    #[serde(skip_serializing_if = "Option::is_none")]
    stop_reason: Option<&'a str>,
}

#[derive(Default)]
struct Inner {
    enabled: bool,
    target: Option<(Peer, String)>,
    turn: bool,
    waiting: usize,
    reported: State,
    /// The last turn's stop reason, until an idle reports it: a request still
    /// waiting at the turn's end delays that idle.
    stop: Option<String>,
}

impl Inner {
    /// Writes the state when it changed, under the lock, so concurrent reporters
    /// reach the writer queue in the order their changes happened.
    fn report(&mut self) {
        let state = if self.waiting > 0 {
            State::RequiresAction
        } else if self.turn {
            State::Running
        } else {
            State::Idle
        };
        if !self.enabled || state == self.reported {
            return;
        }
        let Some((peer, session)) = &self.target else {
            return;
        };
        self.reported = state;
        let stop = if state == State::Idle {
            self.stop.take()
        } else {
            None
        };
        let notification = Notification {
            session_id: session,
            state,
            stop_reason: stop.as_deref(),
        };
        let _ = peer.notify(
            METHOD,
            serde_json::to_value(notification).expect("wire serialization is infallible"),
        );
    }
}

/// The state one session reports; silent unless the client declared the capability.
#[derive(Default)]
pub(crate) struct AgentState {
    inner: Mutex<Inner>,
}

impl AgentState {
    pub(crate) fn enable(&self, enabled: bool) {
        self.inner.lock().unwrap().enabled = enabled;
    }

    /// The client opted out of the hold: its prompt answers at the turn's end.
    pub(crate) fn enabled(&self) -> bool {
        self.inner.lock().unwrap().enabled
    }

    pub(crate) fn opened(&self, peer: Peer, session: String) {
        self.inner.lock().unwrap().target = Some((peer, session));
    }

    pub(crate) fn turn_started(&self) {
        let mut inner = self.inner.lock().unwrap();
        inner.turn = true;
        inner.stop = None;
        inner.report();
    }

    /// `stop` is the ACP stop reason, or `error` for a failed turn, as in v2.
    pub(crate) fn turn_ended(&self, stop: &str) {
        let mut inner = self.inner.lock().unwrap();
        inner.turn = false;
        inner.stop = Some(stop.to_string());
        inner.report();
    }

    /// The session waits for the client until the guard drops.
    pub(crate) fn waiting(self: &Arc<Self>) -> Waiting {
        let mut inner = self.inner.lock().unwrap();
        inner.waiting += 1;
        inner.report();
        Waiting(self.clone())
    }
}

pub(crate) struct Waiting(Arc<AgentState>);

impl Drop for Waiting {
    fn drop(&mut self) {
        let mut inner = self.0.inner.lock().unwrap();
        inner.waiting -= 1;
        inner.report();
    }
}
