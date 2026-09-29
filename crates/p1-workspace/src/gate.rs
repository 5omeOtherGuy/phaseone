//! One writer at a time.
//!
//! Every agent has its own [`crate::ObservedFiles`], and the mutating tools run on
//! blocking threads — so two agents working in one directory can both pass the
//! "unchanged since I read it" check and then both write: a lost update. Agents
//! that may touch the same files therefore share ONE `WriteGate`, and a mutating
//! tool holds it from reading the current contents until its write is recorded.
//! The second writer then checks against what the first one wrote, and is told
//! the file changed instead of silently overwriting it.
//!
//! The gate has two kinds of caller on one lock: the native tools, which wait
//! synchronously on a blocking thread ([`WriteGate::begin_mutation`]), and the host
//! function behind the modules' `workspace-mutation.begin`, which must wait without
//! blocking an executor thread and keep the held gate across import calls (an owned,
//! `'static + Send` guard, see [`crate::Workspace::begin_owned`]). A `std` mutex
//! cannot be awaited, and a tokio mutex cannot be waited on synchronously from a
//! thread that runs a runtime (its blocking and budget checks forbid it), so the lock
//! here is a flag under a `std` mutex with a condition variable for synchronous
//! waiters and a list of wakers for asynchronous ones.
//!
//! What this does NOT cover: a shell command. Its writes are neither gated nor
//! checked (though a later file-tool mutation still notices them). Only a separate
//! working directory isolates agents whose commands rewrite the same files.

use std::future::Future;
use std::marker::PhantomData;
use std::pin::Pin;
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::task::{Context, Poll, Waker};

/// A clonable handle; clones share one gate.
#[derive(Debug, Clone, Default)]
pub struct WriteGate {
    gate: Arc<Gate>,
}

#[derive(Debug, Default)]
struct Gate {
    state: Mutex<State>,
    released: Condvar,
}

#[derive(Debug, Default)]
struct State {
    held: bool,
    /// Asynchronous waiters to wake on release. All of them are woken, because a
    /// waiter whose future was dropped would otherwise swallow the only wake-up.
    wakers: Vec<Waker>,
    /// Synchronous waiters currently parked on the condition variable; lets a test
    /// wait for "the other side is blocked on the gate" without sleeping.
    sync_waiters: usize,
}

/// Held for the whole check-then-write of one mutation.
#[must_use = "the mutation is only serialized while this guard is alive"]
pub struct Mutation<'a> {
    _held: Held,
    _gate: PhantomData<&'a WriteGate>,
}

/// The gate held by an owner that may outlive any borrow of the [`WriteGate`]:
/// `'static + Send`, released on drop.
#[must_use = "the gate is only held while this guard is alive"]
#[derive(Debug)]
pub(crate) struct Held {
    gate: Arc<Gate>,
}

/// The future of [`WriteGate::acquire_owned`].
pub(crate) struct Acquire {
    gate: Arc<Gate>,
}

impl WriteGate {
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether `other` is a handle to this very gate.
    pub fn is_shared_with(&self, other: &WriteGate) -> bool {
        Arc::ptr_eq(&self.gate, &other.gate)
    }

    /// Wait for the gate. Synchronous on purpose: mutating tools run their file
    /// work on a blocking thread, and the critical section is a few file
    /// operations, never a model or network wait.
    pub fn begin_mutation(&self) -> Mutation<'_> {
        let mut state = self.gate.lock();
        while state.held {
            state.sync_waiters += 1;
            state = self
                .gate
                .released
                .wait(state)
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            state.sync_waiters -= 1;
        }
        state.held = true;
        drop(state);
        Mutation {
            _held: Held {
                gate: Arc::clone(&self.gate),
            },
            _gate: PhantomData,
        }
    }

    /// Wait for the gate without blocking the calling thread. The guard owns a
    /// handle to the gate, so it can be kept across calls (in a host's resource
    /// table) and moved between threads.
    pub(crate) fn acquire_owned(&self) -> Acquire {
        Acquire {
            gate: Arc::clone(&self.gate),
        }
    }

    /// How many callers are parked waiting for the gate right now, synchronous and
    /// owned. Read-only observability, chiefly for tests that need to know a waiter is
    /// queued without sleeping.
    pub fn waiting_writers(&self) -> usize {
        let state = self.gate.lock();
        state.sync_waiters + state.wakers.len()
    }
}

impl Gate {
    /// Recover from a poisoned lock: the state is a flag and a waker list, and a
    /// holder that panicked still releases the flag when its guard drops during
    /// unwinding, so nothing here can be left inconsistent.
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

impl Future for Acquire {
    type Output = Held;

    fn poll(self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Held> {
        let mut state = self.gate.lock();
        if !state.held {
            state.held = true;
            return Poll::Ready(Held {
                gate: Arc::clone(&self.gate),
            });
        }
        // Register once per waker: a future polled again before the release must not
        // grow the list without bound.
        if !state
            .wakers
            .iter()
            .any(|waker| waker.will_wake(context.waker()))
        {
            state.wakers.push(context.waker().clone());
        }
        Poll::Pending
    }
}

impl Drop for Held {
    fn drop(&mut self) {
        let mut state = self.gate.lock();
        state.held = false;
        let wakers = std::mem::take(&mut state.wakers);
        drop(state);
        // Both kinds of waiter are told; whoever takes the lock first wins and the
        // others wait again, so neither kind can miss a release.
        self.gate.released.notify_one();
        for waker in wakers {
            waker.wake();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::WriteGate;
    use std::future::Future;
    use std::pin::pin;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::task::{Context, Poll, Waker};

    #[test]
    fn the_owned_guard_and_its_future_are_static_and_send() {
        fn assert_static_send<T: Send + 'static>() {}
        assert_static_send::<super::Held>();
        assert_static_send::<super::Acquire>();
    }

    #[test]
    fn a_sync_holder_keeps_an_owned_acquisition_pending_until_it_releases() {
        let gate = WriteGate::new();
        let mut context = Context::from_waker(Waker::noop());

        let sync = gate.begin_mutation();
        let mut acquire = pin!(gate.acquire_owned());
        assert!(acquire.as_mut().poll(&mut context).is_pending());
        drop(sync);

        assert!(matches!(
            acquire.as_mut().poll(&mut context),
            Poll::Ready(_)
        ));
    }

    #[test]
    fn an_owned_holder_blocks_a_sync_waiter_until_it_releases() {
        let gate = WriteGate::new();
        let mut context = Context::from_waker(Waker::noop());
        let Poll::Ready(owned) = pin!(gate.acquire_owned()).poll(&mut context) else {
            panic!("a free gate is acquired at once");
        };
        let entered = Arc::new(AtomicBool::new(false));

        let waiter = {
            let gate = gate.clone();
            let entered = entered.clone();
            std::thread::spawn(move || {
                let _mutation = gate.begin_mutation();
                entered.store(true, Ordering::SeqCst);
            })
        };
        // Explicit synchronization instead of a sleep: the waiter counts itself under
        // the state lock before it parks, and sets `entered` only once it holds the gate.
        while gate.waiting_writers() == 0 {
            std::thread::yield_now();
        }
        assert!(!entered.load(Ordering::SeqCst));

        drop(owned);
        waiter.join().unwrap();
        assert!(entered.load(Ordering::SeqCst));
    }

    #[test]
    fn a_dropped_waiting_future_does_not_swallow_the_release() {
        let gate = WriteGate::new();
        let mut context = Context::from_waker(Waker::noop());
        let sync = gate.begin_mutation();
        {
            let mut abandoned = pin!(gate.acquire_owned());
            assert!(abandoned.as_mut().poll(&mut context).is_pending());
        }
        let mut second = pin!(gate.acquire_owned());
        assert!(second.as_mut().poll(&mut context).is_pending());
        drop(sync);

        let Poll::Ready(held) = second.as_mut().poll(&mut context) else {
            panic!("the release reaches the remaining waiter");
        };
        // And the gate is held again: a third waiter queues behind it.
        let mut third = pin!(gate.acquire_owned());
        assert!(third.as_mut().poll(&mut context).is_pending());
        drop(held);
        assert!(matches!(third.as_mut().poll(&mut context), Poll::Ready(_)));
    }
}
