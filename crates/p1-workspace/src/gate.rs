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
//! What this does NOT cover: a shell command. Its writes are neither gated nor
//! checked (though a later file-tool mutation still notices them). Only a separate
//! working directory isolates agents whose commands rewrite the same files.

use std::sync::{Arc, Mutex, MutexGuard};

/// A clonable handle; clones share one gate.
#[derive(Debug, Clone, Default)]
pub struct WriteGate {
    gate: Arc<Mutex<()>>,
}

/// Held for the whole check-then-write of one mutation.
#[must_use = "the mutation is only serialized while this guard is alive"]
pub struct Mutation<'a> {
    _guard: MutexGuard<'a, ()>,
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
        // A writer that panicked left at worst one atomically-renamed file
        // behind; the gate itself holds no state that could be inconsistent.
        let guard = self
            .gate
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        Mutation { _guard: guard }
    }
}
