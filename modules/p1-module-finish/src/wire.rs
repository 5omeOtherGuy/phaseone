//! Wire form of the values this component exchanges with the host: JSON text of the
//! protocol families named in `modules/wit/types.wit` (`p1:protocol/<family>/1`,
//! `docs/design/modules/protocol.md`). The host validates every value it receives, so these
//! types mirror only the fields `finish` reads or writes, spelled as `p1-module-protocol`
//! spells them.

use serde::{Deserialize, Serialize};

/// `tool-call`: one complete call.
#[derive(Deserialize)]
pub struct Call {
    pub name: String,
    pub input: Input,
}

/// A call's raw input, kept raw so the tool validates it.
#[derive(Deserialize)]
#[serde(tag = "kind", content = "raw", rename_all = "snake_case")]
pub enum Input {
    Json(String),
    Text(String),
}

impl Input {
    pub fn raw(&self) -> p1_finish_guest::RawInput<'_> {
        match self {
            Self::Json(raw) => p1_finish_guest::RawInput::Json(raw),
            Self::Text(raw) => p1_finish_guest::RawInput::Text(raw),
        }
    }
}

/// `history-item`, as far as `describe-result` reads it: a `tool_result` item's status and
/// the content the model was shown.
#[derive(Deserialize)]
pub struct ResultItem {
    pub status: String,
    pub content: String,
}

/// `tool-outcome`.
#[derive(Serialize)]
pub struct Outcome<'a> {
    pub status: &'static str,
    pub content: &'a str,
}

/// `call-description`.
#[derive(Serialize)]
pub struct CallDescription {
    pub verb: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target: Option<&'static str>,
    pub destructive: bool,
}

/// `result-description` with its optional `text` detail.
#[derive(Serialize)]
pub struct ResultDescription {
    pub summary: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<TextDetail>,
}

#[derive(Serialize)]
#[serde(tag = "kind", rename = "text")]
pub struct TextDetail {
    pub text: String,
}

/// Serialize a value the host validates. These types have no map keys and no custom
/// serializer, so `serde_json` cannot fail on them.
pub fn text<T: Serialize>(value: &T) -> String {
    serde_json::to_string(value).expect("wire values always serialize")
}

/// The tool-outcome text of an ok or error answer.
pub fn outcome(ok: bool, content: &str) -> String {
    text(&Outcome {
        status: if ok { "ok" } else { "error" },
        content,
    })
}
