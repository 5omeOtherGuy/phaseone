//! Run-wide data-weighted fuel. Rhai's own data limits apply to one value, not to
//! copies kept in other variables or by concurrent thunks.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use rhai::{Array, Dynamic, Engine, EvalAltResult, FnPtr, ImmutableString, Map, Position};

const DATA_FUEL: usize = 256 * 1024 * 1024;

pub(crate) struct DataFuel(AtomicUsize);

impl DataFuel {
    pub(crate) fn install(engine: &mut Engine) -> Arc<Self> {
        let fuel = Arc::new(Self(AtomicUsize::new(DATA_FUEL)));
        let accessed = fuel.clone();
        // These hooks are volatile APIs, but Rhai is pinned. Return no replacement:
        // variable lookup, mutability and shadowing keep their original semantics.
        #[allow(deprecated)]
        engine.on_var(move |name, _, context| {
            if let Some(value) = context.scope().get(name) {
                accessed.charge(value)?;
            }
            Ok(None)
        });
        let defined = fuel.clone();
        #[allow(deprecated)]
        engine.on_def_var(move |runtime, _, context| {
            if runtime {
                // Account retained data even when a new initializer is a literal or
                // a host call, rather than an access to an existing variable.
                for (_, _, value) in context.scope().iter_raw() {
                    defined.charge(value)?;
                }
            }
            Ok(true)
        });
        fuel
    }

    pub(crate) fn exhausted(&self) -> bool {
        self.0.load(Ordering::Relaxed) == 0
    }

    pub(crate) fn charge(&self, value: &Dynamic) -> Result<(), Box<EvalAltResult>> {
        let weight = data_weight(value, 0, self.0.load(Ordering::Relaxed));
        if self
            .0
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |left| {
                left.checked_sub(weight)
            })
            .is_err()
        {
            self.0.store(0, Ordering::Relaxed);
            // Like cancellation, exhaustion cannot be caught to continue allocating.
            return Err(Box::new(EvalAltResult::ErrorTerminated(
                "aggregate script data limit (string/array/map fuel)".into(),
                Position::NONE,
            )));
        }
        Ok(())
    }
}

fn data_weight(value: &Dynamic, depth: usize, left: usize) -> usize {
    // Curried closures can form cycles of shared values. Never recurse indefinitely
    // while accounting; exhausted/over-deep data terminates evaluation instead.
    if depth >= 32 || left == 0 {
        return left.saturating_add(1);
    }
    let mut weight = std::mem::size_of::<Dynamic>();
    let Some(value) = value.read_lock::<Dynamic>() else {
        // A method-call owns its captured function mutably until it returns. Do
        // not turn Rhai's own borrow/race check into a panic while inspecting the
        // scope; charge this reference, not the currently inaccessible contents.
        return weight;
    };
    if let Some(text) = value.read_lock::<ImmutableString>() {
        return weight.saturating_add(text.len());
    }
    let mut add = |value: &Dynamic| {
        if weight <= left {
            weight = weight.saturating_add(data_weight(value, depth + 1, left - weight));
        }
    };
    if let Some(array) = value.read_lock::<Array>() {
        for item in array.iter() {
            add(item);
        }
    } else if let Some(map) = value.read_lock::<Map>() {
        // Entry overhead is a fuel weight, not a claim about allocator/RSS bytes.
        for (key, item) in map.iter() {
            if weight > left {
                break;
            }
            weight = weight.saturating_add(key.len() + std::mem::size_of::<ImmutableString>() * 2);
            weight =
                weight.saturating_add(data_weight(item, depth + 1, left.saturating_sub(weight)));
        }
    } else if let Some(function) = value.read_lock::<FnPtr>() {
        for item in function.iter_curry() {
            add(item);
        }
    }
    weight
}
