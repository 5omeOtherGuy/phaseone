//! Issue #2 (review 2026-09-20, departure D-B): a session continues only on the route
//! and model that produced it. A changed origin is REJECTED — before anything is
//! committed — instead of warned about and continued (seams.md §3, ADR-0033).

use std::sync::Arc;
use std::time::Duration;

use p1_contracts::{
    BoxFuture, CancellationToken, ModelOptions, Origin, Provider, ProviderError, ProviderRequest,
    ProviderStream, RouteDescription, TurnEnd,
};
use p1_core::{Agent, AgentParts, ResumeError};
use p1_testkit::{
    PassthroughContext, RecordingEvents, RecordingJournal, ScriptedAuthorization, ScriptedProvider,
    origin, text_response,
};

/// The scripted provider, describing itself as another origin.
struct Elsewhere {
    inner: ScriptedProvider,
    origin: Origin,
}

impl Provider for Elsewhere {
    fn describe(&self) -> RouteDescription {
        RouteDescription {
            origin: self.origin.clone(),
            ..self.inner.describe()
        }
    }
    fn validate(&self, request: &ProviderRequest) -> Result<(), ProviderError> {
        self.inner.validate(request)
    }
    fn stream<'a>(
        &'a self,
        request: ProviderRequest,
        cancel: CancellationToken,
    ) -> BoxFuture<'a, Result<ProviderStream, ProviderError>> {
        self.inner.stream(request, cancel)
    }
}

fn parts(provider: Arc<dyn Provider>, journal: Arc<RecordingJournal>) -> AgentParts {
    AgentParts {
        provider,
        tools: Vec::new(),
        system_prompt: "prompt".into(),
        options: ModelOptions::default(),
        context: Arc::new(PassthroughContext),
        authorization: Arc::new(ScriptedAuthorization::permit_all()),
        journal,
        events: Arc::new(RecordingEvents::new()),
    }
}

async fn recorded_session() -> Vec<p1_contracts::JournalRecord> {
    let journal = Arc::new(RecordingJournal::new());
    let provider = Arc::new(ScriptedProvider::new(vec![text_response("hello")]));
    let mut agent = Agent::new(parts(provider, journal.clone())).unwrap();
    let end = tokio::time::timeout(
        Duration::from_secs(5),
        agent.run_turn("hi".into(), CancellationToken::new()),
    )
    .await
    .unwrap();
    assert!(matches!(end, TurnEnd::Completed { .. }), "{end:?}");
    journal.records()
}

#[tokio::test]
async fn another_model_or_another_route_is_rejected_and_nothing_is_committed() {
    let records = recorded_session().await;
    let other_model = Origin {
        model: "another-model".into(),
        ..origin()
    };
    let other_route = Origin {
        route: "another-route".into(),
        ..origin()
    };

    for assembled in [other_model, other_route] {
        let journal = Arc::new(RecordingJournal::new());
        let provider = Arc::new(Elsewhere {
            inner: ScriptedProvider::new(Vec::new()),
            origin: assembled.clone(),
        });

        let error = Agent::resume(parts(provider, journal.clone()), &records)
            .map(|_| ())
            .unwrap_err();

        assert_eq!(
            error,
            ResumeError::RouteChanged {
                journalled: origin(),
                assembled: assembled.clone(),
            }
        );
        let message = error.to_string();
        assert!(message.contains("fake-route/fake-model"), "{message}");
        assert!(
            message.contains(&format!("{}/{}", assembled.route, assembled.model)),
            "{message}"
        );
        assert!(
            journal.records().is_empty(),
            "a rejected resume commits nothing"
        );
    }
}

#[tokio::test]
async fn the_same_origin_resumes_even_when_the_prompt_changed() {
    let records = recorded_session().await;
    let provider = Arc::new(ScriptedProvider::new(Vec::new()));
    let mut changed = parts(provider, Arc::new(RecordingJournal::new()));
    changed.system_prompt = "a better prompt".into();

    let (_, report) = Agent::resume(changed, &records).expect("same origin resumes");

    assert!(report.environment_changed);
}
