//! Lead acceptance: resuming a session on another route or model (ADR-0049, which
//! supersedes ADR-0033). The ORIGINAL rule — any changed origin is refused — is gone;
//! what replaces it is the provider's own `validate` over the projected history:
//! - a provider that accepts the transcript continues it, and the next turn commits
//!   the NEW environment before its input and sends the OLD history to the NEW route;
//! - a provider that refuses it fails the resume with its own reason, and nothing is
//!   committed (the guarantee ADR-0033 gave for every changed origin).

use std::sync::Arc;
use std::time::Duration;

use p1_contracts::{
    BoxFuture, CancellationToken, Item, ModelOptions, Origin, Provider, ProviderError,
    ProviderErrorKind, ProviderRequest, ProviderStream, RecordBody, RouteDescription, TurnEnd,
};
use p1_core::{Agent, AgentParts, BuildError, ResumeError};
use p1_testkit::{
    PassthroughContext, RecordingEvents, RecordingJournal, ScriptedAuthorization, ScriptedProvider,
    origin, text_response,
};

/// The scripted provider, describing itself as another origin, optionally refusing
/// any request whose history is not empty (a route that cannot carry the transcript).
struct Elsewhere {
    inner: Arc<ScriptedProvider>,
    origin: Origin,
    refuse_history: bool,
}

impl Provider for Elsewhere {
    fn describe(&self) -> RouteDescription {
        RouteDescription {
            origin: self.origin.clone(),
            ..self.inner.describe()
        }
    }
    fn validate(&self, request: &ProviderRequest) -> Result<(), ProviderError> {
        if self.refuse_history && !request.history.is_empty() {
            return Err(ProviderError::new(
                ProviderErrorKind::InvalidRequest,
                "the history holds an item this route cannot carry",
            ));
        }
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

async fn turn(agent: &mut Agent, input: &str) -> TurnEnd {
    tokio::time::timeout(
        Duration::from_secs(5),
        agent.run_turn(input.into(), CancellationToken::new()),
    )
    .await
    .unwrap()
}

async fn recorded_session() -> Vec<p1_contracts::JournalRecord> {
    let journal = Arc::new(RecordingJournal::new());
    let provider = Arc::new(ScriptedProvider::new(vec![text_response("hello")]));
    let mut agent = Agent::new(parts(provider, journal.clone())).unwrap();
    let end = turn(&mut agent, "hi").await;
    assert!(matches!(end, TurnEnd::Completed { .. }), "{end:?}");
    journal.records()
}

fn others() -> [Origin; 2] {
    [
        Origin {
            model: "another-model".into(),
            ..origin()
        },
        Origin {
            route: "another-route".into(),
            ..origin()
        },
    ]
}

#[tokio::test]
async fn another_origin_that_accepts_the_history_continues_it() {
    let records = recorded_session().await;
    for assembled in others() {
        let journal = Arc::new(RecordingJournal::new());
        let inner = Arc::new(ScriptedProvider::new(vec![text_response("again")]));
        let provider = Arc::new(Elsewhere {
            inner: inner.clone(),
            origin: assembled.clone(),
            refuse_history: false,
        });

        let (mut agent, report) =
            Agent::resume(parts(provider, journal.clone()), &records).expect("resumes");
        assert!(report.environment_changed, "a new origin is a change");
        assert!(
            journal.records().is_empty(),
            "resume itself commits nothing"
        );

        let end = turn(&mut agent, "go on").await;
        assert!(matches!(end, TurnEnd::Completed { .. }), "{end:?}");

        // The first record of the continued session is the NEW environment, at the
        // next dense sequence number, before the input.
        let written = journal.records();
        assert_eq!(written[0].seq, records.len() as u64);
        match &written[0].body {
            RecordBody::Environment { route, .. } => assert_eq!(route.origin, assembled),
            other => panic!("expected the new Environment first, got {other:?}"),
        }
        assert!(matches!(written[1].body, RecordBody::UserInput { .. }));

        // The new route is asked with the OLD transcript plus the new input.
        let requests = inner.requests();
        assert_eq!(requests.len(), 1);
        let history = &requests[0].history;
        assert!(
            matches!(&history[0], Item::User { text } if text == "hi"),
            "{history:?}"
        );
        assert!(matches!(history[1], Item::Assistant(_)), "{history:?}");
        assert!(
            matches!(&history[2], Item::User { text } if text == "go on"),
            "{history:?}"
        );
    }
}

#[tokio::test]
async fn another_origin_that_refuses_the_history_fails_with_its_reason_and_commits_nothing() {
    let records = recorded_session().await;
    for assembled in others() {
        let journal = Arc::new(RecordingJournal::new());
        let inner = Arc::new(ScriptedProvider::new(Vec::new()));
        let provider = Arc::new(Elsewhere {
            inner: inner.clone(),
            origin: assembled,
            refuse_history: true,
        });

        let error = Agent::resume(parts(provider, journal.clone()), &records)
            .map(|_| ())
            .unwrap_err();

        match &error {
            ResumeError::Build(BuildError::ProviderRejected(rejected)) => {
                assert_eq!(rejected.kind, ProviderErrorKind::InvalidRequest);
            }
            other => panic!("expected the provider's refusal, got {other:?}"),
        }
        assert!(
            error.to_string().contains("cannot carry"),
            "the provider's reason reaches the operator: {error}"
        );
        assert!(
            journal.records().is_empty(),
            "a refused resume commits nothing"
        );
        assert!(inner.requests().is_empty(), "and sends nothing");
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
