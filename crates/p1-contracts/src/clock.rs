//! Wall-clock time seam (ADR-0121).
//!
//! One agent, one journal store: each carries a [`Clock`] and stamps the records it
//! writes with `now_ms` — Unix milliseconds. Tests supply a fake clock so timing is
//! deterministic; production uses [`SystemClock`].

/// A source of wall-clock milliseconds since the Unix epoch. `Debug` so a store
/// that holds one can still derive `Debug`.
pub trait Clock: Send + Sync + std::fmt::Debug {
    /// Milliseconds since 1970-01-01T00:00:00Z (UTC).
    fn now_ms(&self) -> u64;
}

/// The real clock: `SystemTime::now()` against the Unix epoch. A pre-epoch time
/// (only a wildly wrong system clock) yields 0 rather than a panic or a negative.
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now_ms(&self) -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|since| since.as_millis() as u64)
            .unwrap_or(0)
    }
}
