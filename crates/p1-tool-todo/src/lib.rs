//! Native tool adapter; production dispatch uses the component with the same logic.
use p1_contracts::{
    BoxFuture, DeclarationKind, Effect, Tool, ToolCall, ToolContext, ToolDeclaration, ToolIdentity,
    ToolInput, ToolOutcome, ToolStatus,
};
pub use p1_todo_guest::{DESCRIPTION, NAME, input_schema};

pub struct TodoTool {
    declaration: ToolDeclaration,
    identity: ToolIdentity,
}
impl Default for TodoTool {
    fn default() -> Self {
        Self {
            declaration: ToolDeclaration {
                name: NAME.into(),
                description: DESCRIPTION.into(),
                kind: DeclarationKind::Function {
                    input_schema: input_schema(),
                },
            },
            identity: ToolIdentity {
                implementation: env!("CARGO_PKG_NAME").into(),
                variant: "default".into(),
            },
        }
    }
}
impl Tool for TodoTool {
    fn declaration(&self) -> &ToolDeclaration {
        &self.declaration
    }
    fn identity(&self) -> &ToolIdentity {
        &self.identity
    }
    fn effect(&self, _: &ToolCall) -> Effect {
        Effect::ReadOnly
    }
    fn execute<'a>(
        &'a self,
        call: &'a ToolCall,
        context: ToolContext,
    ) -> BoxFuture<'a, ToolOutcome> {
        Box::pin(async move {
            if context.cancel.is_cancelled() {
                return ToolOutcome {
                    status: ToolStatus::Cancelled,
                    content: String::new(),
                };
            }
            let ToolInput::Json(raw) = &call.input else {
                return ToolOutcome::error("todo_write expects JSON arguments");
            };
            let outcome = p1_todo_guest::execute(raw);
            let content = outcome["content"].as_str().unwrap_or_default();
            if outcome["status"] == "ok" {
                ToolOutcome::ok(content)
            } else {
                ToolOutcome::error(content)
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use p1_contracts::{CancellationToken, Concurrency};

    #[tokio::test]
    async fn successful_redaction_expansion_is_published_and_replayed() {
        use p1_contracts::plan::PlanSource;
        use p1_contracts::{JournalRecord, RecordBody, ToolResultItem};
        use std::sync::Arc;
        let tool = p1_redact::redacted(
            Arc::new(TodoTool::default()),
            &Arc::new(p1_redact::MaskCounter::new()),
        );
        let text = format!("Bearer abcdefgh {}", "x".repeat(1984));
        assert_eq!(text.chars().count(), 2000);
        let call = ToolCall { call_id: "c1".into(), name: NAME.into(), input: ToolInput::Json(p1_contracts::serde_json::json!({"todos":[{"content":text,"status":"pending","priority":"high"}]}).to_string()) };
        let outcome = tool
            .execute(
                &call,
                ToolContext {
                    cancel: CancellationToken::new(),
                },
            )
            .await;
        assert_eq!(outcome.status, ToolStatus::Ok);
        let records = [
            JournalRecord {
                seq: 0,
                body: RecordBody::ToolStarted {
                    call_id: "c1".into(),
                    identity: tool.identity().clone(),
                },
            },
            JournalRecord {
                seq: 1,
                body: RecordBody::ToolFinished {
                    result: ToolResultItem {
                        call_id: "c1".into(),
                        name: NAME.into(),
                        status: outcome.status,
                        content: outcome.content,
                    },
                    exit_code: Some(None),
                },
            },
        ];
        for source in [
            p1_todo_session::SessionPlan::default(),
            p1_todo_session::SessionPlan::default(),
        ] {
            for record in &records {
                source.observe(record);
            }
            let entries = source
                .snapshot()
                .expect("redacted snapshot survives immediate observation and replay");
            assert_eq!(entries.len(), 1);
            assert!(entries[0].content.chars().count() > 2000);
            assert!(!entries[0].content.contains("abcdefgh"));
            assert!(entries[0].content.ends_with(&"x".repeat(1984)));
        }
    }

    #[tokio::test]
    async fn tool_validates_and_cancellation_never_returns_a_snapshot() {
        let tool = TodoTool::default();
        let mut call = ToolCall {
            call_id: "c1".into(),
            name: NAME.into(),
            input: ToolInput::Json(r#"{"todos":[]}"#.into()),
        };
        assert_eq!(tool.concurrency(&call), Concurrency::Exclusive);
        let result = tool
            .execute(
                &call,
                ToolContext {
                    cancel: CancellationToken::new(),
                },
            )
            .await;
        assert_eq!(result.status, ToolStatus::Ok);
        assert_eq!(p1_todo_guest::snapshot(&result.content).unwrap(), vec![]);
        call.input = ToolInput::Text("[]".into());
        assert_eq!(
            tool.execute(
                &call,
                ToolContext {
                    cancel: CancellationToken::new()
                }
            )
            .await
            .status,
            ToolStatus::Error
        );
        let cancel = CancellationToken::new();
        cancel.cancel();
        assert_eq!(
            tool.execute(&call, ToolContext { cancel }).await.status,
            ToolStatus::Cancelled
        );
    }
}
