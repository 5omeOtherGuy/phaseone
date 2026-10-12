//! The hook port (ADR-0213, seam #711): user commands run at named points of a session, whose
//! answers narrow what the session does.
//!
//! A hook never widens anything. Its answers reach the core only through the two decision
//! points [`crate::policy`] already names: a [`HookDecision`] for a tool call through
//! [`crate::AuthorizationPolicy`] (deny, or make the host ask), and context or a block for a
//! prompt through [`crate::ContextPolicy`]. The other events are named so a bridge can read
//! them; an event no decision point serves is read and not run, until a deliberate contract
//! change adds its interception point (`policy.rs`: "a new kind of interception is a
//! deliberate contract change").
//!
//! The port is [`HookRunner`]: given an event and its payload, run the matching commands and
//! merge their answers ([`HookOutput::merge`]). Which commands, how a payload is encoded and
//! how an answer is decoded is a bridge's, per harness (`p1-hooks-claude-code`); matching,
//! running and merging is the core's (`p1-hooks`).

use crate::{BoxFuture, CancellationToken};

/// A point of a session a hook can be attached to. The names are the ones the hook files of
/// Claude Code and Codex use.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum HookEvent {
    /// A session starts.
    SessionStart,
    /// The user submitted a prompt, before the model sees it.
    UserPromptSubmit,
    /// A tool call is about to run, before the session's authorization decides it.
    PreToolUse,
    /// A tool call finished.
    PostToolUse,
    /// The agent is about to end its turn.
    Stop,
    /// A delegated agent starts.
    SubagentStart,
    /// A delegated agent ended.
    SubagentStop,
}

impl HookEvent {
    /// Every event, in the order hook files list them.
    pub const ALL: [Self; 7] = [
        Self::SessionStart,
        Self::UserPromptSubmit,
        Self::PreToolUse,
        Self::PostToolUse,
        Self::Stop,
        Self::SubagentStart,
        Self::SubagentStop,
    ];

    /// The name a hook file spells.
    pub fn name(self) -> &'static str {
        match self {
            Self::SessionStart => "SessionStart",
            Self::UserPromptSubmit => "UserPromptSubmit",
            Self::PreToolUse => "PreToolUse",
            Self::PostToolUse => "PostToolUse",
            Self::Stop => "Stop",
            Self::SubagentStart => "SubagentStart",
            Self::SubagentStop => "SubagentStop",
        }
    }

    /// The event a hook file spells `name`.
    pub fn parse(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|event| event.name() == name)
    }

    /// Whether a decision point serves this event today: its hooks run and their answers
    /// count. The others are read and not run.
    pub fn is_served(self) -> bool {
        matches!(
            self,
            Self::SessionStart | Self::UserPromptSubmit | Self::PreToolUse
        )
    }
}

/// What a hook decides about a tool call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HookDecision {
    /// No objection. Never a grant: the session's own authorization still decides.
    Allow,
    /// The user is asked, whatever the session's authorization would have answered.
    Ask,
    /// The call does not run; `reason` is shown to the model.
    Deny {
        /// Why, in the hook's words.
        reason: String,
    },
}

/// The merged answer of the hooks of one event.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HookOutput {
    /// The strongest decision any hook gave: deny over ask over allow.
    pub decision: Option<HookDecision>,
    /// Text the hooks add to what the model sees, in the order they ran.
    pub additional_context: Option<String>,
    /// Why the session must stop, from the first hook that said so.
    pub stop: Option<String>,
}

impl HookOutput {
    /// Merges the answer of the next hook into this one: deny over ask over allow, the first
    /// stop sticks, context accumulates in order.
    pub fn merge(mut self, next: HookOutput) -> HookOutput {
        self.decision = match (self.decision.take(), next.decision) {
            (Some(HookDecision::Deny { reason }), _) | (_, Some(HookDecision::Deny { reason })) => {
                Some(HookDecision::Deny { reason })
            }
            (Some(HookDecision::Ask), _) | (_, Some(HookDecision::Ask)) => Some(HookDecision::Ask),
            (Some(HookDecision::Allow), _) | (_, Some(HookDecision::Allow)) => {
                Some(HookDecision::Allow)
            }
            (None, None) => None,
        };
        self.additional_context = match (self.additional_context.take(), next.additional_context) {
            (Some(first), Some(second)) => Some(format!("{first}\n\n{second}")),
            (first, second) => first.or(second),
        };
        self.stop = self.stop.or(next.stop);
        self
    }
}

/// Runs the hooks of one event: the commands that match it, their answers merged.
pub trait HookRunner: Send + Sync {
    /// Runs every hook of `event` that matches `payload`, in file order, and merges their
    /// answers. A hook that fails is reported by the runner and does not block; one that
    /// asks to block says so in its answer. `cancel` stops waiting for a hook.
    fn run<'a>(
        &'a self,
        event: HookEvent,
        payload: serde_json::Value,
        cancel: &'a CancellationToken,
    ) -> BoxFuture<'a, HookOutput>;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn deny(reason: &str) -> HookOutput {
        HookOutput {
            decision: Some(HookDecision::Deny {
                reason: reason.into(),
            }),
            ..HookOutput::default()
        }
    }

    #[test]
    fn deny_beats_ask_beats_allow_whatever_the_order_and_the_first_deny_names_it() {
        let allow = HookOutput {
            decision: Some(HookDecision::Allow),
            ..HookOutput::default()
        };
        let ask = HookOutput {
            decision: Some(HookDecision::Ask),
            ..HookOutput::default()
        };
        assert_eq!(
            allow.clone().merge(ask.clone()).decision,
            Some(HookDecision::Ask)
        );
        assert_eq!(
            ask.clone().merge(allow.clone()).decision,
            Some(HookDecision::Ask)
        );
        assert_eq!(
            ask.merge(deny("first")).merge(deny("second")).decision,
            Some(HookDecision::Deny {
                reason: "first".into()
            })
        );
        assert_eq!(
            HookOutput::default().merge(HookOutput::default()),
            HookOutput::default()
        );
    }

    #[test]
    fn context_accumulates_in_order_and_the_first_stop_sticks() {
        let said = |context: &str, stop: Option<&str>| HookOutput {
            additional_context: Some(context.into()),
            stop: stop.map(str::to_owned),
            ..HookOutput::default()
        };
        let merged = said("one", None)
            .merge(said("two", Some("first stop")))
            .merge(said("three", Some("second stop")));
        assert_eq!(
            merged.additional_context.as_deref(),
            Some("one\n\ntwo\n\nthree")
        );
        assert_eq!(merged.stop.as_deref(), Some("first stop"));
    }

    #[test]
    fn events_round_trip_their_file_names_and_three_are_served() {
        for event in HookEvent::ALL {
            assert_eq!(HookEvent::parse(event.name()), Some(event));
        }
        assert_eq!(HookEvent::parse("Notification"), None);
        let served: Vec<HookEvent> = HookEvent::ALL
            .into_iter()
            .filter(|event| event.is_served())
            .collect();
        assert_eq!(
            served,
            [
                HookEvent::SessionStart,
                HookEvent::UserPromptSubmit,
                HookEvent::PreToolUse
            ]
        );
    }
}
