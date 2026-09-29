//! Shared WebSocket one-shot reconnect rows for the host and native parity driver.

use p1_contracts::StreamEvent;
use p1_contracts::serde_json::{self, Value};

/// Three independent one-shot reconnect allowances per request.
#[derive(Default)]
pub struct OnceRows {
    reused_close: bool,
    error_event: [bool; 2],
}

/// Account for an inbound frame before JSON parsing or retaining it in either driver.
pub fn admit_frame(
    total: &mut usize,
    frame_len: usize,
    frame_limit: usize,
    response_limit: usize,
) -> bool {
    if frame_len > frame_limit || total.saturating_add(frame_len) > response_limit {
        return false;
    }
    *total += frame_len;
    true
}

/// Spend one transient retry, or report exhaustion without increasing the counter.
pub fn use_transient_retry(retries: &mut u32, max: u32) -> bool {
    if *retries >= max {
        return false;
    }
    *retries += 1;
    true
}

/// Which stream events make subsequent transport recovery unsafe.
pub fn is_visible(event: &StreamEvent) -> bool {
    matches!(
        event,
        StreamEvent::TextDelta { .. }
            | StreamEvent::ReasoningDelta { .. }
            | StreamEvent::ToolInputDelta { .. }
    )
}

/// Host action after a read failure, shared by native parity and component drivers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadRecovery {
    Reconnect,
    Terminal,
    Transient,
}

impl OnceRows {
    /// Consume a connection-error row only once, and never after visible output.
    pub fn reconnect_error(&mut self, row: Option<ReconnectRow>, visible: bool) -> bool {
        if visible {
            return false;
        }
        let Some(row) = row else {
            return false;
        };
        let slot = &mut self.error_event[row.slot()];
        if *slot {
            return false;
        }
        *slot = true;
        true
    }

    /// A reused connection failing before its first frame has one free reconnect.
    pub fn read_recovery(
        &mut self,
        first_frame: bool,
        reused: bool,
        visible: bool,
    ) -> ReadRecovery {
        if first_frame && reused && !visible && !self.reused_close {
            self.reused_close = true;
            ReadRecovery::Reconnect
        } else if visible {
            ReadRecovery::Terminal
        } else {
            ReadRecovery::Transient
        }
    }

    /// A reused socket whose send failed has the same one free reconnect.
    pub fn write_reconnect(&mut self, reused: bool) -> bool {
        if !reused || self.reused_close {
            return false;
        }
        self.reused_close = true;
        true
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReconnectRow {
    PreviousResponseNotFound,
    ConnectionLimitReached,
}

impl ReconnectRow {
    pub fn slot(self) -> usize {
        match self {
            Self::PreviousResponseNotFound => 0,
            Self::ConnectionLimitReached => 1,
        }
    }
}

/// A connection-level provider error that permits one reconnect, not a model error.
pub fn reconnect_row(text: &str) -> Option<ReconnectRow> {
    let value: Value = serde_json::from_str(text).ok()?;
    if !matches!(
        value.get("type").and_then(Value::as_str),
        Some("error") | Some("response.failed")
    ) {
        return None;
    }
    match frame_error_code(&value) {
        Some("previous_response_not_found") => Some(ReconnectRow::PreviousResponseNotFound),
        Some("websocket_connection_limit_reached") => Some(ReconnectRow::ConnectionLimitReached),
        _ => None,
    }
}

fn frame_error_code(value: &Value) -> Option<&str> {
    let error = value
        .get("response")
        .and_then(|response| response.get("error"))
        .or_else(|| value.get("error"));
    error
        .and_then(|error| {
            error
                .get("code")
                .and_then(Value::as_str)
                .or_else(|| error.get("type").and_then(Value::as_str))
        })
        .or_else(|| value.get("code").and_then(Value::as_str))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_accounting_stops_before_overflow_and_is_shared_by_both_drivers() {
        let mut total = 0;
        assert!(admit_frame(&mut total, 8, 8, 16));
        assert!(admit_frame(&mut total, 8, 8, 16));
        assert!(!admit_frame(&mut total, 1, 8, 16));
        assert!(!admit_frame(&mut total, usize::MAX, 8, 16));
        assert_eq!(total, 16);
    }

    #[test]
    fn transient_allowance_is_spent_once_per_retry_and_never_overflows() {
        let mut retries = 0;
        assert!(use_transient_retry(&mut retries, 1));
        assert!(!use_transient_retry(&mut retries, 1));
        assert_eq!(retries, 1);
        let mut exhausted = u32::MAX;
        assert!(!use_transient_retry(&mut exhausted, u32::MAX));
    }

    #[test]
    fn shared_read_policy_distinguishes_one_dead_reused_socket_and_visible_failure() {
        let mut once = OnceRows::default();
        assert_eq!(
            once.read_recovery(true, true, false),
            ReadRecovery::Reconnect
        );
        assert_eq!(
            once.read_recovery(true, true, false),
            ReadRecovery::Transient
        );
        assert_eq!(once.read_recovery(true, true, true), ReadRecovery::Terminal);
        assert_eq!(
            once.read_recovery(false, false, false),
            ReadRecovery::Transient
        );
        assert!(!once.write_reconnect(true));
        let mut once = OnceRows::default();
        assert!(!once.write_reconnect(false));
        assert!(once.write_reconnect(true));
        assert!(!once.write_reconnect(true));
    }

    #[test]
    fn reconnect_rows_are_distinct_and_unknown_errors_are_not_reconnected() {
        let mut once = OnceRows::default();
        for (code, slot) in [
            ("previous_response_not_found", 0),
            ("websocket_connection_limit_reached", 1),
        ] {
            let frame = format!(r#"{{"type":"error","error":{{"code":"{code}"}}}}"#);
            let row = reconnect_row(&frame).expect("connection error");
            assert_eq!(row.slot(), slot);
            assert!(!once.error_event[row.slot()]);
            once.error_event[row.slot()] = true;
            assert!(once.error_event[row.slot()]);
        }
        assert_eq!(
            reconnect_row(r#"{"type":"error","error":{"code":"unknown"}}"#),
            None
        );
    }
}
