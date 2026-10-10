//! The hold rule (D4): a prompt stays pending while background work it started is
//! live, so the parent's inbox turn about that work runs inside the same prompt.
//!
//! Work counts as started by a prompt when its start arrives while the prompt is
//! pending. `session/cancel` or the next `session/prompt` releases the hold. A shell
//! job is never signalled, so it never holds.
//!
//! A background worker's end reaches the port before its notice reaches the parent's
//! inbox: the worker settles its own jobs in between (`run_child`,
//! `crates/p1-workers/src/lib.rs`). A held worker that ends while no turn of the
//! prompt runs therefore leaves a notice owed, and the prompt waits for it. A worker
//! that ends inside a turn is one that turn waited for, or one whose notice the next
//! drain finds. A workflow run queues its notice before its end signal, so it owes
//! none.

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
    /// A turn of the pending prompt is running.
    in_turn: bool,
    /// A held worker ended outside a turn and its notice has not been drained yet.
    owed: bool,
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
                if state.live.remove(&key) == Some(true)
                    && signal.kind == BackgroundKind::Worker
                    && !state.in_turn
                {
                    state.owed = true;
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
        state.in_turn = false;
        state.owed = false;
    }

    /// Marks a turn of the pending prompt as running or done.
    pub(crate) fn turn(&self, running: bool) {
        self.state.lock().unwrap().in_turn = running;
    }

    /// An inbox turn ran: the notices it drained are no longer owed.
    pub(crate) fn settle(&self) {
        self.state.lock().unwrap().owed = false;
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

    /// Whether the pending prompt holds on live work, or on a notice its work owes.
    pub(crate) fn holding(&self) -> bool {
        let state = self.state.lock().unwrap();
        state.owed || state.live.values().any(|held| *held)
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
        assert!(hold.holding(), "w2 still holds; the unheld w1 owes nothing");
        hold.signal(&signal(BackgroundPhase::Ended, "w2"));
        assert!(hold.holding(), "the ended worker's notice is owed");
        hold.settle();
        assert!(!hold.holding());
        hold.end_prompt();
    }

    #[test]
    fn a_worker_that_ends_inside_a_turn_owes_no_notice() {
        let hold = Hold::default();
        hold.begin_prompt();
        hold.turn(true);
        hold.signal(&signal(BackgroundPhase::Started, "w1"));
        hold.signal(&signal(BackgroundPhase::Ended, "w1"));
        hold.turn(false);
        assert!(!hold.holding(), "a foreground worker sends no notice");
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
