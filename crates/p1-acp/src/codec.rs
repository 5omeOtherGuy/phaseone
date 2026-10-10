//! Version selection and JSON boundary. Wire structs never leave this crate.

use crate::{
    capabilities::Capabilities,
    policy::{PermissionPrompt, PermissionReply},
    sink::Update,
    turn::{TurnError, TurnStop},
    wire,
};
use p1_contracts::frontend::{CommandInfo, ConfigChoice};
use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Version {
    V1,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Codec {
    version: Version,
}

impl Codec {
    /// ACP requires returning our latest supported version when the requested
    /// version is unsupported. The client decides whether to continue.
    pub fn negotiate(_requested: u16) -> Self {
        // V1 is both the only supported version and the unsupported fallback.
        let version = Version::V1;
        Self { version }
    }

    pub fn version(self) -> u16 {
        match self.version {
            Version::V1 => 1,
        }
    }

    pub fn encode_update(self, update: &Update) -> Value {
        match self.version {
            Version::V1 => wire::v1::update(update),
        }
    }

    pub fn encode_stop(self, stop: TurnStop) -> Value {
        match self.version {
            Version::V1 => wire::v1::stop(stop),
        }
    }

    pub fn encode_error(self, error: &TurnError) -> Value {
        match self.version {
            Version::V1 => wire::v1::error(error),
        }
    }

    pub fn encode_capabilities(self, capabilities: &Capabilities) -> Value {
        match self.version {
            Version::V1 => wire::v1::capabilities(capabilities),
        }
    }

    pub fn encode_permission(self, session: &str, prompt: &PermissionPrompt) -> Value {
        match self.version {
            Version::V1 => wire::v1::permission(session, prompt),
        }
    }

    /// The `configOptions` array of `session/new` and `session/set_config_option`.
    pub fn encode_config_options(self, choices: &[ConfigChoice]) -> Value {
        match self.version {
            Version::V1 => wire::v1::config::options(choices),
        }
    }

    /// The `config_option_update` a `session/update` carries.
    pub fn encode_config_update(self, choices: &[ConfigChoice]) -> Value {
        match self.version {
            Version::V1 => wire::v1::config::update(choices),
        }
    }

    /// The `available_commands_update` a `session/update` carries.
    pub fn encode_commands_update(self, commands: &[CommandInfo]) -> Value {
        match self.version {
            Version::V1 => wire::v1::commands::update(commands),
        }
    }

    /// Standard session metadata; needs no extension negotiation.
    pub fn encode_session_title(self, title: &str) -> Value {
        match self.version {
            Version::V1 => wire::v1::session_title(title),
        }
    }

    /// Malformed replies are errors; unknown option ids remain untrusted data
    /// and are denied by the policy, never interpreted as grants.
    pub fn decode_permission(self, reply: Value) -> Result<PermissionReply, serde_json::Error> {
        match self.version {
            Version::V1 => wire::v1::decode_permission(reply),
        }
    }
}
