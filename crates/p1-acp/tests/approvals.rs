use p1_acp::{
    codec::Codec,
    policy::{AcpPolicy, Answer, CANCEL_DENY, PermissionReply, USER_DENY},
};
use p1_contracts::*;
use serde_json::json;

fn call() -> ToolCall {
    ToolCall {
        call_id: "approval-5".into(),
        name: "shell".into(),
        input: ToolInput::Json(r#"{"command":"fixture"}"#.into()),
    }
}
fn identity() -> ToolIdentity {
    ToolIdentity {
        implementation: "fixture-shell".into(),
        variant: "default".into(),
    }
}
fn request<'a>(call: &'a ToolCall, identity: &'a ToolIdentity) -> AuthorizationRequest<'a> {
    AuthorizationRequest {
        call,
        identity,
        effect: Effect::Executes,
    }
}
fn selected(id: &str) -> PermissionReply {
    Codec::negotiate(1)
        .decode_permission(json!({"outcome":{"outcome":"selected","optionId":id}}))
        .unwrap()
}

#[tokio::test]
async fn choices_and_permission_wire() {
    for (id, expected) in [
        ("allow_once", Answer::Yes),
        ("allow_always", Answer::Always),
        ("reject_once", Answer::No),
        ("unoffered", Answer::No),
    ] {
        let (policy, mut rx) = AcpPolicy::new(CancellationToken::new());
        let (call, identity) = (call(), identity());
        let (answer, ()) = tokio::join!(policy.ask(request(&call, &identity)), async {
            let parked = rx.recv().await.unwrap();
            assert_eq!(parked.identity, identity);
            assert_eq!(parked.effect, Effect::Executes);
            let wire = Codec::negotiate(1).encode_permission("session-2", &parked.prompt());
            assert_eq!(wire["sessionId"], "session-2");
            assert_eq!(wire["toolCall"]["toolCallId"], "approval-5");
            assert_eq!(wire["toolCall"]["rawInput"], json!({"command":"fixture"}));
            assert_eq!(wire["toolCall"]["status"], "pending");
            assert_eq!(
                wire["options"],
                json!([
                    {"optionId":"allow_once","name":"Allow once","kind":"allow_once"},
                    {"optionId":"allow_always","name":"Always allow","kind":"allow_always"},
                    {"optionId":"reject_once","name":"Reject","kind":"reject_once"}
                ])
            );
            parked.answer(selected(id));
        });
        assert_eq!(answer, Some(expected));
    }
}

#[tokio::test]
async fn dropped_answer_denies() {
    let (policy, mut rx) = AcpPolicy::new(CancellationToken::new());
    let (call, identity) = (call(), identity());
    let (decision, ()) = tokio::join!(policy.authorize(request(&call, &identity)), async {
        drop(rx.recv().await.unwrap());
    });
    assert_eq!(
        decision,
        Decision::Deny {
            reason: USER_DENY.into()
        }
    );
}
#[tokio::test]
async fn gone_client_denies() {
    let (policy, rx) = AcpPolicy::new(CancellationToken::new());
    drop(rx);
    assert_eq!(
        policy.authorize(request(&call(), &identity())).await,
        Decision::Deny {
            reason: CANCEL_DENY.into()
        }
    );
}
#[tokio::test]
async fn turn_cancel_denies_every_parked_request_and_wins_over_ready_allow() {
    let (policy, mut rx) = AcpPolicy::new(CancellationToken::new());
    let turn = CancellationToken::new();
    policy.set_turn(Some(turn.clone()));
    let (call, identity) = (call(), identity());
    let (first, second, parked) = tokio::join!(
        policy.authorize(request(&call, &identity)),
        policy.authorize(request(&call, &identity)),
        async {
            let first = rx.recv().await.unwrap();
            let second = rx.recv().await.unwrap();
            first.answer(selected("allow_once"));
            turn.cancel();
            second
        }
    );
    assert_eq!(
        first,
        Decision::Deny {
            reason: CANCEL_DENY.into()
        }
    );
    assert_eq!(
        second,
        Decision::Deny {
            reason: CANCEL_DENY.into()
        }
    );
    assert!(parked.is_closed());
    policy.set_turn(Some(CancellationToken::new()));
    let (decision, ()) = tokio::join!(policy.authorize(request(&call, &identity)), async {
        rx.recv().await.unwrap().answer(selected("allow_once"));
    });
    assert_eq!(decision, Decision::Permit);
}
#[tokio::test]
async fn global_cancel_denies() {
    let cancel = CancellationToken::new();
    let (policy, mut rx) = AcpPolicy::new(cancel.clone());
    let (call, identity) = (call(), identity());
    let (decision, parked) = tokio::join!(policy.authorize(request(&call, &identity)), async {
        let parked = rx.recv().await.unwrap();
        cancel.cancel();
        parked
    });
    assert_eq!(
        decision,
        Decision::Deny {
            reason: CANCEL_DENY.into()
        }
    );
    assert!(parked.is_closed());
}

#[tokio::test]
async fn unpolled_authorization_keeps_its_original_turn() {
    let (policy, mut rx) = AcpPolicy::new(CancellationToken::new());
    let turn = CancellationToken::new();
    policy.set_turn(Some(turn.clone()));
    let (call, identity) = (call(), identity());
    let pending = policy.authorize(request(&call, &identity));
    turn.cancel();
    policy.set_turn(Some(CancellationToken::new()));
    let (decision, ()) = tokio::join!(pending, async {
        rx.recv().await.unwrap().answer(selected("allow_once"));
    });
    assert_eq!(
        decision,
        Decision::Deny {
            reason: CANCEL_DENY.into()
        }
    );
}

#[tokio::test]
async fn incoming_cancelled_denies() {
    let (policy, mut rx) = AcpPolicy::new(CancellationToken::new());
    let (call, identity) = (call(), identity());
    let (decision, ()) = tokio::join!(policy.authorize(request(&call, &identity)), async {
        rx.recv().await.unwrap().answer(
            Codec::negotiate(1)
                .decode_permission(json!({"outcome":{"outcome":"cancelled"}}))
                .unwrap(),
        );
    });
    assert_eq!(
        decision,
        Decision::Deny {
            reason: CANCEL_DENY.into()
        }
    );
}
#[tokio::test]
async fn authorization_decisions() {
    for (id, expected) in [
        ("allow_once", Decision::Permit),
        ("allow_always", Decision::Permit),
        (
            "reject_once",
            Decision::Deny {
                reason: USER_DENY.into(),
            },
        ),
    ] {
        let (policy, mut rx) = AcpPolicy::new(CancellationToken::new());
        let (call, identity) = (call(), identity());
        let (decision, ()) = tokio::join!(policy.authorize(request(&call, &identity)), async {
            rx.recv().await.unwrap().answer(selected(id));
        });
        assert_eq!(decision, expected);
    }
}
