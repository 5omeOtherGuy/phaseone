//! What `configure` stores: the context table, validated as the native policy validates it.

use std::sync::OnceLock;

use p1_context::ContextConfig;
use p1_context::engine::Caps;
use p1_contracts::serde_json::{self, Map, Value};

/// The keys `configure` accepts. The table's own keys are `ContextConfig`'s field names;
/// `max_output_tokens` is the agent's own output limit, when its options carry one.
const TABLE_KEYS: [&str; 6] = [
    "window_tokens",
    "output_headroom_tokens",
    "summarize_at_tokens",
    "keep_recent_tokens",
    "user_verbatim_tokens",
    "tool_result_excerpt_chars",
];
const CAP_KEY: &str = "summary_output_tokens";
const AGENT_CAP_KEY: &str = "max_output_tokens";

pub(crate) struct Settings {
    pub(crate) config: ContextConfig,
    pub(crate) caps: Caps,
}

static SETTINGS: OnceLock<Settings> = OnceLock::new();

/// Keeps `settings` for every later call. The host calls `configure` once per instance.
pub(crate) fn store(settings: Settings) -> Result<(), String> {
    SETTINGS
        .set(settings)
        .map_err(|_| "configure was already called on this instance".to_string())
}

/// The stored settings; an export called before `configure` fails.
pub(crate) fn get() -> Result<&'static Settings, String> {
    SETTINGS
        .get()
        .ok_or_else(|| "the context policy was not configured".to_string())
}

impl Settings {
    /// A JSON object with every table key and `summary_output_tokens`, and optionally
    /// `max_output_tokens`. An unknown key, a missing one or a value that is not a
    /// non-negative integer is refused, never defaulted.
    pub(crate) fn parse(text: &str) -> Result<Self, String> {
        let value: Value = serde_json::from_str(text)
            .map_err(|error| format!("the context settings are not JSON: {error}"))?;
        let Value::Object(object) = value else {
            return Err("the context settings are not a JSON object".to_string());
        };
        if let Some(key) = object.keys().find(|key| {
            !TABLE_KEYS.contains(&key.as_str())
                && key.as_str() != CAP_KEY
                && key.as_str() != AGENT_CAP_KEY
        }) {
            return Err(format!("the context settings have an unknown key `{key}`"));
        }
        let config = ContextConfig {
            window_tokens: required(&object, "window_tokens")?,
            output_headroom_tokens: required(&object, "output_headroom_tokens")?,
            summarize_at_tokens: required(&object, "summarize_at_tokens")?,
            keep_recent_tokens: required(&object, "keep_recent_tokens")?,
            user_verbatim_tokens: required(&object, "user_verbatim_tokens")?,
            tool_result_excerpt_chars: usize::try_from(required(
                &object,
                "tool_result_excerpt_chars",
            )?)
            .map_err(|_| "tool_result_excerpt_chars does not fit this platform".to_string())?,
        };
        config.validate()?;
        let summary_output_tokens = required(&object, CAP_KEY)?;
        config.validate_summary_output_tokens(summary_output_tokens)?;
        let agent_max_output_tokens = match object.get(AGENT_CAP_KEY) {
            None => None,
            Some(value) => Some(
                value
                    .as_u64()
                    .and_then(|number| u32::try_from(number).ok())
                    .ok_or_else(|| format!("{AGENT_CAP_KEY} is not an unsigned 32-bit integer"))?,
            ),
        };
        Ok(Self {
            config,
            caps: Caps {
                summary_output_tokens,
                agent_max_output_tokens,
            },
        })
    }
}

fn required(object: &Map<String, Value>, key: &str) -> Result<u64, String> {
    object
        .get(key)
        .ok_or_else(|| format!("the context settings lack `{key}`"))?
        .as_u64()
        .ok_or_else(|| format!("`{key}` is not a non-negative integer"))
}
