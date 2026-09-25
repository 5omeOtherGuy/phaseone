//! The route parser seam, portable so a provider WebAssembly component can
//! implement it without the native driver (ADR-0071).

use p1_contracts::{Outcome, ProviderError, StreamEvent};

use crate::sse::SseEvent;

/// Turns route-native SSE events into contract stream events. Pure and
/// synchronous, so it can be unit-tested without a transport.
pub trait ResponseParser: Send {
    /// Feed one SSE event. Returned events are forwarded in order. A returned
    /// `StreamEvent::Finished` ends the stream.
    fn on_event(&mut self, event: SseEvent) -> Vec<StreamEvent>;

    /// The body ended. Return the terminal outcome (normally a Transport failure
    /// "stream ended without a terminal event" unless the parser already
    /// finished).
    fn on_end(&mut self) -> Outcome;

    /// Map a non-2xx response to an error. `body` is for CLASSIFICATION ONLY
    /// (e.g. spotting a context-window error type) and must never be copied into
    /// the message.
    fn on_http_error(
        &self,
        status: u16,
        headers: &[(String, String)],
        body: &[u8],
    ) -> ProviderError;
}
