//! The closed verb vocabulary and the module failure mapping (freeze items 5 and 8).

use p1_contracts::{CallDescription, Outcome, ProviderError, ProviderErrorKind, ToolStatus};
use p1_module_protocol::{CALL_VERBS, ModuleFailure, WireCallDescription, call_verb};

#[test]
fn every_known_verb_maps_to_itself() {
    assert_eq!(
        CALL_VERBS,
        [
            "read", "edit", "run", "search", "finish", "worker", "workflow", "call"
        ]
    );
    for verb in CALL_VERBS {
        assert_eq!(call_verb(verb), verb);
    }
}

#[test]
fn an_unknown_verb_maps_to_call_and_is_not_kept() {
    for verb in ["delete", "Read", "read ", "", "rm -rf", "workers"] {
        assert_eq!(call_verb(verb), "call", "{verb:?}");
    }
    let description = CallDescription::from(WireCallDescription {
        verb: "launch".into(),
        target: Some("rocket".into()),
        edit: None,
        destructive: true,
    });
    assert_eq!(description.verb, "call");
    assert_eq!(description.target.as_deref(), Some("rocket"));
    assert!(
        !format!("{description:?}").contains("launch"),
        "the unknown verb must not survive anywhere"
    );
}

fn every_failure() -> Vec<ModuleFailure> {
    vec![
        ModuleFailure::Trap("wasm trap: unreachable".into()),
        ModuleFailure::Cancelled,
        ModuleFailure::DeadlineExceeded,
        ModuleFailure::FuelExhausted,
        ModuleFailure::InvalidOutput("missing field `status` at line 1".into()),
        ModuleFailure::Host(ProviderError::new(
            ProviderErrorKind::RateLimited,
            "rate limited by the route",
        )),
    ]
}

#[test]
fn every_failure_maps_into_a_tool_outcome() {
    for failure in every_failure() {
        let label = format!("{failure:?}");
        let outcome = failure.clone().into_tool_outcome();
        match failure {
            ModuleFailure::Cancelled => {
                assert_eq!(outcome.status, ToolStatus::Cancelled);
                assert_eq!(outcome.content, "");
            }
            ModuleFailure::Trap(message) | ModuleFailure::InvalidOutput(message) => {
                assert_eq!(outcome.status, ToolStatus::Error, "{label}");
                assert!(outcome.content.contains(&message), "{label}");
            }
            ModuleFailure::DeadlineExceeded => {
                assert_eq!(outcome.status, ToolStatus::Error);
                assert!(outcome.content.contains("time limit"));
            }
            ModuleFailure::FuelExhausted => {
                assert_eq!(outcome.status, ToolStatus::Error);
                assert!(outcome.content.contains("compute budget"));
            }
            ModuleFailure::Host(error) => {
                assert_eq!(outcome.status, ToolStatus::Error);
                assert!(outcome.content.contains("rate_limited"));
                assert!(outcome.content.contains(&error.message));
            }
        }
    }
}

#[test]
fn every_failure_maps_into_a_provider_outcome() {
    let kind = |outcome: Outcome| match outcome {
        Outcome::Failed(error) => Some(error.kind),
        Outcome::Cancelled => None,
        Outcome::Completed(_) => panic!("a failure never completes"),
    };
    for failure in every_failure() {
        let outcome = failure.clone().into_provider_outcome();
        match failure {
            ModuleFailure::Cancelled => assert_eq!(outcome, Outcome::Cancelled),
            ModuleFailure::Trap(_)
            | ModuleFailure::InvalidOutput(_)
            | ModuleFailure::FuelExhausted => {
                assert_eq!(kind(outcome), Some(ProviderErrorKind::Protocol));
            }
            ModuleFailure::DeadlineExceeded => {
                assert_eq!(kind(outcome), Some(ProviderErrorKind::Transport));
            }
            ModuleFailure::Host(error) => assert_eq!(outcome, Outcome::Failed(error)),
        }
    }
}

/// The mapping targets closed sets. These matches have no wildcard, so adding a
/// `ProviderErrorKind` or a `ToolStatus` stops this test from compiling: a new kind is a
/// contract change that must revisit the mapping, the schemas and the native retry rules.
#[test]
fn the_target_sets_stay_closed() {
    fn kind_name(kind: ProviderErrorKind) -> &'static str {
        match kind {
            ProviderErrorKind::InvalidRequest => "invalid_request",
            ProviderErrorKind::Authentication => "authentication",
            ProviderErrorKind::InsufficientBalance => "insufficient_balance",
            ProviderErrorKind::NotEntitled => "not_entitled",
            ProviderErrorKind::UsageLimitExhausted => "usage_limit_exhausted",
            ProviderErrorKind::RateLimited => "rate_limited",
            ProviderErrorKind::ContextWindowExceeded => "context_window_exceeded",
            ProviderErrorKind::Transport => "transport",
            ProviderErrorKind::Protocol => "protocol",
        }
    }
    fn status_name(status: ToolStatus) -> &'static str {
        match status {
            ToolStatus::Ok => "ok",
            ToolStatus::Error => "error",
            ToolStatus::Unavailable => "unavailable",
            ToolStatus::Denied => "denied",
            ToolStatus::Cancelled => "cancelled",
            ToolStatus::Unknown => "unknown",
        }
    }
    for failure in every_failure() {
        let status = failure.clone().into_tool_outcome().status;
        assert!(["error", "cancelled"].contains(&status_name(status)));
        if let Outcome::Failed(error) = failure.into_provider_outcome() {
            assert!(!kind_name(error.kind).is_empty());
        }
    }
}
