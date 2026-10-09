//! Approval bridge adapted from crates/p1-tui/src/runtime.rs (TuiPolicy).
//! The driver owns persistent Always grants; authorize alone permits this call.

use agent_client_protocol_schema::v1::{
    self as acp, PermissionOption, PermissionOptionKind, RequestPermissionOutcome,
    ToolCallUpdateFields,
};
use p1_contracts::{
    AuthorizationPolicy, AuthorizationRequest, BoxFuture, CancellationToken, Decision, Effect,
    ToolCall, ToolIdentity,
};
use std::future::{Future, poll_fn};
use std::sync::Mutex;
use std::task::Poll;
use tokio::sync::{mpsc, oneshot};

pub const USER_DENY: &str = "Denied by the user.";
pub const CANCEL_DENY: &str = "Cancelled while awaiting authorization.";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Answer {
    Yes,
    Always,
    No,
}

pub struct PermissionRequest {
    pub call: ToolCall,
    pub identity: ToolIdentity,
    pub effect: Effect,
    reply: oneshot::Sender<RequestPermissionOutcome>,
}

impl PermissionRequest {
    pub fn is_closed(&self) -> bool {
        self.reply.is_closed()
    }

    /// Driver can replace tool_call fields with AcpSink::tool_call's description.
    pub fn wire(&self, session: impl Into<acp::SessionId>) -> acp::RequestPermissionRequest {
        acp::RequestPermissionRequest::new(
            session,
            acp::ToolCallUpdate::new(
                self.call.call_id.clone(),
                ToolCallUpdateFields::new()
                    .title(self.call.name.clone())
                    .name(self.call.name.clone())
                    .status(acp::ToolCallStatus::Pending)
                    .raw_input(crate::sink::raw_input(&self.call)),
            ),
            vec![
                PermissionOption::new("allow_once", "Allow once", PermissionOptionKind::AllowOnce),
                PermissionOption::new(
                    "allow_always",
                    "Always allow",
                    PermissionOptionKind::AllowAlways,
                ),
                PermissionOption::new("reject_once", "Reject", PermissionOptionKind::RejectOnce),
            ],
        )
    }

    /// Unknown option ids deny rather than granting unintended permission.
    pub fn answer(self, outcome: RequestPermissionOutcome) {
        let _ = self.reply.send(outcome);
    }
}

pub struct AcpPolicy {
    cancel: CancellationToken,
    turn: Mutex<Option<CancellationToken>>,
    tx: mpsc::UnboundedSender<PermissionRequest>,
}

impl AcpPolicy {
    pub fn new(cancel: CancellationToken) -> (Self, mpsc::UnboundedReceiver<PermissionRequest>) {
        let (tx, rx) = mpsc::unbounded_channel();
        (
            Self {
                cancel,
                turn: Mutex::new(None),
                tx,
            },
            rx,
        )
    }

    pub fn set_turn(&self, token: Option<CancellationToken>) {
        *self.turn.lock().unwrap() = token;
    }

    /// None is cancellation/disconnection; a dropped answer is No. The driver
    /// uses Always to remember the grant before translating to Permit.
    pub fn ask<'a>(&'a self, request: AuthorizationRequest<'a>) -> BoxFuture<'a, Option<Answer>> {
        // Capture the turn before returning the future: a later set_turn must
        // not retarget an authorization belonging to the previous turn.
        let turn = self.turn.lock().unwrap().clone();
        Box::pin(async move {
            let (reply, answer) = oneshot::channel();
            let parked = PermissionRequest {
                call: request.call.clone(),
                identity: request.identity.clone(),
                effect: request.effect,
                reply,
            };
            if self.tx.send(parked).is_err() {
                return None;
            }
            let turn_cancelled = async move {
                match turn {
                    Some(turn) => turn.cancelled().await,
                    None => std::future::pending::<()>().await,
                }
            };
            let mut cancelled = std::pin::pin!(self.cancel.cancelled());
            let mut turn_cancelled = std::pin::pin!(turn_cancelled);
            let mut answer = std::pin::pin!(answer);
            // Biased cancellation, like the donor, without tokio's macros
            // feature in production (this crate needs only sync).
            poll_fn(|cx| {
                if cancelled.as_mut().poll(cx).is_ready()
                    || turn_cancelled.as_mut().poll(cx).is_ready()
                {
                    return Poll::Ready(None);
                }
                answer.as_mut().poll(cx).map(|outcome| match outcome {
                    Ok(RequestPermissionOutcome::Cancelled) => None,
                    Ok(RequestPermissionOutcome::Selected(selected)) => {
                        Some(match selected.option_id.0.as_ref() {
                            "allow_once" => Answer::Yes,
                            "allow_always" => Answer::Always,
                            _ => Answer::No,
                        })
                    }
                    _ => Some(Answer::No),
                })
            })
            .await
        })
    }
}

impl AuthorizationPolicy for AcpPolicy {
    fn authorize<'a>(&'a self, request: AuthorizationRequest<'a>) -> BoxFuture<'a, Decision> {
        let answer = self.ask(request);
        Box::pin(async move {
            match answer.await {
                Some(Answer::Yes | Answer::Always) => Decision::Permit,
                Some(Answer::No) => Decision::Deny {
                    reason: USER_DENY.into(),
                },
                None => Decision::Deny {
                    reason: CANCEL_DENY.into(),
                },
            }
        })
    }
}
