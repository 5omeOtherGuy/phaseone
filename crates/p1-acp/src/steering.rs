//! Ecosystem `_session/steering` options, pinned to claude-code-acp's wire.

use serde_json::Value;

use crate::driver::io::RpcError;

pub(crate) fn prompt_required(params: &Value) -> Result<bool, RpcError> {
    if params
        .get("prompt")
        .and_then(Value::as_array)
        .is_none_or(Vec::is_empty)
    {
        return Err(RpcError::new(
            -32602,
            "steering needs a non-empty prompt array",
        ));
    }
    match params.pointer("/_meta/steering/idleBehavior") {
        None => Ok(false),
        Some(Value::String(value)) if value == "promptRequired" => Ok(true),
        Some(_) => Err(RpcError::new(-32602, "unsupported steering idleBehavior")),
    }
}
