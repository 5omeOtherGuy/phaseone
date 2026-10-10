//! The hold rule (D4): a prompt stays pending while background work it started is
//! live, so the parent's inbox turn about that work runs inside the same prompt.
//!
//! Work counts as started by a prompt when its start arrives while the prompt is
//! pending. `session/cancel` or the next `session/prompt` releases the hold. A shell
//! job is never signalled, so it never holds.

use p1_contracts::frontend::{BackgroundKind, BackgroundPhase, BackgroundSignal};
use std::collections::HashMap;
use std::sync::Mutex;
use tokio::sync::Notify;

#[derive(Default)]
struct State {
    prompting: bool,
    released: bool,
    /// Live work, and whether the pending prompt holds on it.
    live: HashMap<(BackgroundKind, String), bool>,
    /// A held piece of work ended since the last [`Hold::take_ended`]: its notice
    /// may still be on its way to the inbox.
    ended: bool,
}

#[derive(Default)]
pub(crate) struct Hold {
    state: Mutex<State>,
    changed: Notify,
}

impl Hold {
    pub(crate) fn signal(&self, signal: &BackgroundSignal) {
        let mut state = self.state.lock().unwrap();
        let key = (signal.kind, signal.id.clone());
        match signal.phase {
            BackgroundPhase::Started => {
                let held = state.prompting;
                state.live.insert(key, held);
            }
            BackgroundPhase::Ended => {
                if state.live.remove(&key) == Some(true) {
                    state.ended = true;
                }
            }
        }
        drop(state);
        self.changed.notify_one();
    }

    pub(crate) fn begin_prompt(&self) {
        let mut state = self.state.lock().unwrap();
        state.prompting = true;
        state.released = false;
        state.ended = false;
    }

    /// Work the finished prompt held on no longer holds a later one.
    pub(crate) fn end_prompt(&self) {
        let mut state = self.state.lock().unwrap();
        state.prompting = false;
        state.live.values_mut().for_each(|held| *held = false);
    }

    pub(crate) fn release(&self) {
        self.state.lock().unwrap().released = true;
        self.changed.notify_one();
    }

    pub(crate) fn released(&self) -> bool {
        self.state.lock().unwrap().released
    }

    /// Whether the pending prompt holds on live work.
    pub(crate) fn holding(&self) -> bool {
        self.state.lock().unwrap().live.values().any(|held| *held)
    }

    pub(crate) fn take_ended(&self) -> bool {
        std::mem::take(&mut self.state.lock().unwrap().ended)
    }

    /// Resolves after the next signal or release. `notify_one` keeps a permit, so a
    /// change between a check and this wait is not lost.
    pub(crate) async fn changed(&self) {
        self.changed.notified().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn signal(phase: BackgroundPhase, id: &str) -> BackgroundSignal {
        BackgroundSignal {
            phase,
            kind: BackgroundKind::Worker,
            id: id.to_string(),
            turn: Some(1),
        }
    }

    #[test]
    fn only_work_started_during_the_prompt_holds_it() {
        let hold = Hold::default();
        hold.signal(&signal(BackgroundPhase::Started, "w1"));
        hold.begin_prompt();
        assert!(!hold.holding(), "work started at idle does not hold");
        hold.signal(&signal(BackgroundPhase::Started, "w2"));
        assert!(hold.holding());
        hold.signal(&signal(BackgroundPhase::Ended, "w1"));
        assert!(!hold.take_ended(), "an unheld end is no notice to wait for");
        hold.signal(&signal(BackgroundPhase::Ended, "w2"));
        assert!(!hold.holding());
        assert!(hold.take_ended());
        hold.end_prompt();
    }

    #[test]
    fn a_later_prompt_does_not_hold_on_an_earlier_prompts_work() {
        let hold = Hold::default();
        hold.begin_prompt();
        hold.signal(&signal(BackgroundPhase::Started, "w1"));
        hold.release();
        assert!(hold.released());
        hold.end_prompt();
        hold.begin_prompt();
        assert!(!hold.released());
        assert!(!hold.holding());
    }
}
