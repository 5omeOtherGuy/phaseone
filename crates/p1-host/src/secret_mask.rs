//! Issue #484: what a MODEL says never carries a credential p1 handles.
//!
//! A model can echo a credential it saw — in its answer, its reasoning or the arguments
//! of a tool call, and split between two deltas. The host wraps every provider the
//! catalog builds in [`masking`]: every registered value of the host's
//! [`SecretSet`] is replaced with its marker before `p1-core` sees the event, so the
//! screen, the partial text of an interrupted turn, the committed item, the journal and
//! every later request only ever hold the masked form.
//!
//! Only REGISTERED values are masked here ([`SecretSet::mask`]), never credential
//! shapes: a shape rule would corrupt ordinary model text and the arguments of tool
//! calls, and a registered value is exact. A delta that ends in what could be the start
//! of a registered value holds that suffix back until the next delta of the same block
//! (or the end of the stream) shows whether the value is there. A failure's message is
//! a diagnostic, so it is masked by shape too. Opaque continuation data (`ReplayData`)
//! is carried verbatim: the origin route needs it byte for byte.

use std::collections::VecDeque;
use std::sync::Arc;

use futures_util::StreamExt;
use p1_contracts::{
    AssistantBlock, AssistantItem, BoxFuture, CancellationToken, Outcome, Provider, ProviderError,
    ProviderRequest, ProviderStream, RouteDescription, StreamEvent, ToolInput,
};
use p1_redact::SecretSet;

/// `inner`, with every registered value of `secrets` masked in what it streams.
pub(crate) fn masking(inner: Arc<dyn Provider>, secrets: SecretSet) -> Arc<dyn Provider> {
    Arc::new(MaskingProvider { inner, secrets })
}

struct MaskingProvider {
    inner: Arc<dyn Provider>,
    secrets: SecretSet,
}

impl Provider for MaskingProvider {
    fn describe(&self) -> RouteDescription {
        self.inner.describe()
    }

    fn validate(&self, request: &ProviderRequest) -> Result<(), ProviderError> {
        self.inner
            .validate(request)
            .map_err(|error| mask_error(error, &self.secrets))
    }

    fn stream<'a>(
        &'a self,
        request: ProviderRequest,
        cancel: CancellationToken,
    ) -> BoxFuture<'a, Result<ProviderStream, ProviderError>> {
        Box::pin(async move {
            let inner = self
                .inner
                .stream(request, cancel)
                .await
                .map_err(|error| mask_error(error, &self.secrets))?;
            let masker = Masker {
                inner,
                secrets: self.secrets.clone(),
                held: Vec::new(),
                ready: VecDeque::new(),
                done: false,
            };
            let stream: ProviderStream = Box::pin(futures_util::stream::unfold(
                masker,
                |mut masker| async move { masker.next().await.map(|event| (event, masker)) },
            ));
            Ok(stream)
        })
    }
}

/// A provider error is a diagnostic: registered values and credential shapes are masked.
fn mask_error(error: ProviderError, secrets: &SecretSet) -> ProviderError {
    ProviderError {
        message: p1_redact::redact_with(&error.message, secrets).text,
        ..error
    }
}

/// The block a delta belongs to: a held suffix is only ever continued by the same one.
#[derive(Clone, PartialEq, Eq)]
enum Block {
    Text(usize),
    Reasoning(usize),
    ToolInput { call_id: String, name: String },
}

impl Block {
    fn delta(&self, text: String) -> StreamEvent {
        match self {
            Self::Text(block) => StreamEvent::TextDelta {
                block: *block,
                text,
            },
            Self::Reasoning(block) => StreamEvent::ReasoningDelta {
                block: *block,
                text,
            },
            Self::ToolInput { call_id, name } => StreamEvent::ToolInputDelta {
                call_id: call_id.clone(),
                name: name.clone(),
                text,
            },
        }
    }
}

struct Masker {
    inner: ProviderStream,
    secrets: SecretSet,
    /// Per block, the suffix held back because a registered value may start there.
    held: Vec<(Block, String)>,
    ready: VecDeque<StreamEvent>,
    done: bool,
}

impl Masker {
    async fn next(&mut self) -> Option<StreamEvent> {
        loop {
            if let Some(event) = self.ready.pop_front() {
                return Some(event);
            }
            if self.done {
                return None;
            }
            match self.inner.next().await {
                Some(event) => self.accept(event),
                // The end without a terminal event is `p1-core`'s to report; what was
                // held back is still text the model produced.
                None => {
                    self.flush();
                    self.done = true;
                }
            }
        }
    }

    fn accept(&mut self, event: StreamEvent) {
        match event {
            StreamEvent::TextDelta { block, text } => self.delta(Block::Text(block), text),
            StreamEvent::ReasoningDelta { block, text } => {
                self.delta(Block::Reasoning(block), text)
            }
            StreamEvent::ToolInputDelta {
                call_id,
                name,
                text,
            } => self.delta(Block::ToolInput { call_id, name }, text),
            StreamEvent::Finished(outcome) => {
                self.flush();
                let outcome = match outcome {
                    Outcome::Completed(mut response) => {
                        mask_item(&mut response.item, &self.secrets);
                        Outcome::Completed(response)
                    }
                    Outcome::Failed(error) => Outcome::Failed(mask_error(error, &self.secrets)),
                    Outcome::Cancelled => Outcome::Cancelled,
                };
                self.ready.push_back(StreamEvent::Finished(outcome));
                self.done = true;
            }
            other => self.ready.push_back(other),
        }
    }

    /// Mask `text` after what `block` held back, emitting all but a suffix that may
    /// still become a registered value.
    fn delta(&mut self, block: Block, text: String) {
        let index = match self.held.iter().position(|(known, _)| *known == block) {
            Some(index) => index,
            None => {
                self.held.push((block, String::new()));
                self.held.len() - 1
            }
        };
        let mut pending = std::mem::take(&mut self.held[index].1);
        pending.push_str(&text);
        let cut = self.secrets.shown_len(&pending);
        let shown = self.secrets.mask(&pending[..cut]).text;
        self.held[index].1 = pending[cut..].to_owned();
        if !shown.is_empty() {
            self.ready.push_back(self.held[index].0.delta(shown));
        }
    }

    /// Emit everything still held back: the stream ends, so no value can complete it.
    fn flush(&mut self) {
        for (block, text) in std::mem::take(&mut self.held) {
            if !text.is_empty() {
                let shown = self.secrets.mask(&text).text;
                self.ready.push_back(block.delta(shown));
            }
        }
    }
}

/// The committed item: text, reasoning text and tool-call input masked; replay data kept.
fn mask_item(item: &mut AssistantItem, secrets: &SecretSet) {
    if secrets.is_empty() {
        return;
    }
    for block in &mut item.blocks {
        match block {
            AssistantBlock::Text { text } | AssistantBlock::Reasoning { text, .. } => {
                *text = secrets.mask(text).text;
            }
            AssistantBlock::ToolCall(call) => {
                call.input = match &call.input {
                    ToolInput::Json(raw) => ToolInput::Json(secrets.mask(raw).text),
                    ToolInput::Text(raw) => ToolInput::Text(secrets.mask(raw).text),
                };
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use p1_contracts::{ProviderErrorKind, StopReason};
    use p1_testkit::{ScriptedProvider, Step, completed, json_call, text_block};

    /// A value no shape rule knows, built at run time.
    fn secret() -> String {
        format!("opaque{}", "-k3y".repeat(6))
    }

    fn request() -> ProviderRequest {
        ProviderRequest {
            system_prompt: String::new(),
            history: Vec::new(),
            tools: Vec::new(),
            options: Default::default(),
        }
    }

    async fn run(step: Step, secrets: &SecretSet) -> Vec<StreamEvent> {
        let provider = masking(Arc::new(ScriptedProvider::new(vec![step])), secrets.clone());
        match provider.stream(request(), CancellationToken::new()).await {
            Ok(stream) => stream.collect().await,
            Err(error) => vec![StreamEvent::Finished(Outcome::Failed(error))],
        }
    }

    fn deltas(events: &[StreamEvent]) -> String {
        events
            .iter()
            .filter_map(|event| match event {
                StreamEvent::TextDelta { text, .. }
                | StreamEvent::ReasoningDelta { text, .. }
                | StreamEvent::ToolInputDelta { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect()
    }

    /// Finding 11: a registered value split between deltas of text, reasoning and tool
    /// input, and whole in the committed item, reaches the consumer masked everywhere;
    /// the replay data is untouched.
    #[tokio::test]
    async fn an_echoed_credential_is_masked_in_every_delta_and_the_item() {
        let secrets = SecretSet::new();
        let value = secret();
        secrets.register(&value);
        let (head, tail) = value.split_at(9);
        let replay = p1_contracts::ReplayData {
            origin: p1_testkit::origin(),
            version: 1,
            payload: serde_json::json!({ "signature": value }),
        };
        let events = run(
            Step::Events(vec![
                StreamEvent::TextDelta {
                    block: 0,
                    text: format!("key is {head}"),
                },
                StreamEvent::ReasoningDelta {
                    block: 1,
                    text: format!("I saw {head}"),
                },
                StreamEvent::TextDelta {
                    block: 0,
                    text: format!("{tail} done"),
                },
                StreamEvent::ReasoningDelta {
                    block: 1,
                    text: tail.to_owned(),
                },
                StreamEvent::ToolInputDelta {
                    call_id: "c1".into(),
                    name: "shell".into(),
                    text: format!("{{\"command\":\"echo {value}\"}}"),
                },
                StreamEvent::Finished(completed(
                    vec![
                        text_block(&format!("key is {value} done")),
                        AssistantBlock::Reasoning {
                            text: format!("I saw {value}"),
                            replay: Some(replay.clone()),
                        },
                        AssistantBlock::ToolCall(json_call(
                            "c1",
                            "shell",
                            &format!("{{\"command\":\"echo {value}\"}}"),
                        )),
                    ],
                    StopReason::ToolUse,
                    None,
                )),
            ]),
            &secrets,
        )
        .await;

        let marker = format!("<redacted:secret:{} chars>", value.len());
        let shown = deltas(&events);
        assert!(!shown.contains(head), "{shown}");
        assert_eq!(
            shown,
            format!("key is I saw {marker} done{marker}{{\"command\":\"echo {marker}\"}}")
        );
        let Some(StreamEvent::Finished(Outcome::Completed(response))) = events.last() else {
            panic!("no completed outcome: {events:?}");
        };
        let item = serde_json::to_string(&response.item.blocks[..]).unwrap();
        assert_eq!(item.matches(&value).count(), 1, "only the replay keeps it");
        assert_eq!(
            response.item.blocks[1],
            AssistantBlock::Reasoning {
                text: format!("I saw {marker}"),
                replay: Some(replay),
            }
        );
        let AssistantBlock::ToolCall(call) = &response.item.blocks[2] else {
            panic!("the tool call moved");
        };
        let arguments: serde_json::Value = serde_json::from_str(call.input.raw()).unwrap();
        assert_eq!(arguments["command"], format!("echo {marker}"));
    }

    /// A value whose last byte equals its first is complete at the end of a delta, not
    /// the start of another occurrence: it is masked whole, never shown minus that byte.
    #[tokio::test]
    async fn a_value_ending_in_its_own_start_is_masked_within_one_delta() {
        let secrets = SecretSet::new();
        let value = format!("s{}s", "-k3y".repeat(6));
        secrets.register(&value);
        let events = run(
            Step::Events(vec![
                StreamEvent::TextDelta {
                    block: 0,
                    text: format!("here: {value}"),
                },
                StreamEvent::TextDelta {
                    block: 0,
                    text: " done".into(),
                },
                StreamEvent::Finished(Outcome::Cancelled),
            ]),
            &secrets,
        )
        .await;
        let marker = format!("<redacted:secret:{} chars>", value.len());
        let StreamEvent::TextDelta { text, .. } = &events[0] else {
            panic!("{events:?}");
        };
        assert_eq!(text, &format!("here: {marker}"));
        assert_eq!(deltas(&events), format!("here: {marker} done"));
    }

    /// A held-back suffix that never became a value is shown when the stream ends,
    /// so an interrupted turn keeps all of its text.
    #[tokio::test]
    async fn a_held_suffix_is_released_when_the_stream_ends() {
        let secrets = SecretSet::new();
        let value = secret();
        secrets.register(&value);
        let start = &value[..5];
        let events = run(
            Step::Events(vec![
                StreamEvent::TextDelta {
                    block: 0,
                    text: format!("almost {start}"),
                },
                StreamEvent::Finished(Outcome::Cancelled),
            ]),
            &secrets,
        )
        .await;
        assert_eq!(deltas(&events), format!("almost {start}"));
        assert!(matches!(
            events.last(),
            Some(StreamEvent::Finished(Outcome::Cancelled))
        ));
    }

    /// A failure's message, at setup or at the end of a stream, is masked by value
    /// and by shape.
    #[tokio::test]
    async fn failure_messages_are_masked() {
        let secrets = SecretSet::new();
        let value = secret();
        secrets.register(&value);
        let message = format!("echoed {value}");
        for step in [
            Step::SetupError(ProviderError::new(ProviderErrorKind::Transport, &message)),
            Step::Events(vec![StreamEvent::Finished(Outcome::Failed(
                ProviderError::new(ProviderErrorKind::Transport, &message),
            ))]),
        ] {
            let events = run(step, &secrets).await;
            let Some(StreamEvent::Finished(Outcome::Failed(error))) = events.last() else {
                panic!("no failure: {events:?}");
            };
            assert!(!error.message.contains(&value), "{}", error.message);
            assert!(error.message.contains("<redacted:secret:"));
        }
    }
}
