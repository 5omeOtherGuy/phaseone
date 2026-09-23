//! Turning rhai's errors into the run's typed errors, with 1-based line and column.

use rhai::{EvalAltResult, ParseError, Position};

use crate::api::WorkflowError;

pub(crate) fn parse_error(error: &ParseError) -> WorkflowError {
    let (line, column) = line_column(error.position());
    WorkflowError::Parse {
        // The type alone: the position is carried by the fields, not repeated in the text.
        message: error.err_type().to_string(),
        line,
        column,
    }
}

/// `<message> [line L, column C]`, the one form a run's `error` takes for a script failure.
/// A failure without a position (a non-JSON return value) carries no bracket.
pub(crate) fn runtime_message(mut error: Box<EvalAltResult>) -> String {
    let position = error.take_position();
    located(&error.to_string(), position)
}

pub(crate) fn located(message: &str, position: Position) -> String {
    match line_column(position) {
        (0, _) => message.to_string(),
        (line, column) => format!("{message} [line {line}, column {column}]"),
    }
}

/// rhai positions are 1-based already; `Position::NONE` becomes 0.
fn line_column(position: Position) -> (u32, u32) {
    let line = position.line().unwrap_or(0);
    let column = position.position().unwrap_or(0);
    (
        u32::try_from(line).unwrap_or(u32::MAX),
        u32::try_from(column).unwrap_or(u32::MAX),
    )
}
