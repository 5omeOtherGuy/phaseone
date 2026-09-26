//! Slice S5.8 (issue #332): replacing the provider of a running session keeps the replay data
//! the session recorded honest (ADR-0049, ADR-0018, ADR-0078 §4).
//!
//! `Agent::reconfigure` (S1.10, ADR-0084 items 3 and 6) validates the complete candidate
//! against the CURRENT history before it commits the candidate's `Environment` and only then
//! installs it. What that history carries is opaque provider-native continuation data:
//! `ReplayData { origin, version, payload }`. A provider DECLARES the layout versions of its
//! own origin's replay data it reads, and its `validate` decides whether a session may move to
//! it: another origin's replay data is dropped, whatever its version (ADR-0018), and the
//! provider's own data at a version it does not read is refused (ADR-0049) — never silently
//! dropped, because a session must not lose the reasoning it is continuing from.
//!
//! Where the shipped chat adapter declares this: `crates/p1-provider-openai-chat/src/request.rs`,
//! `lower_item` (`if data.version != 1`), which both `validate` and the request builder run, so
//! the acceptance of a switch and the request that follows it cannot disagree. The replacement
//! provider in these cases is [`ReplayReader`]: the shipped rule over a scripted stream, with
//! the versions it reads as the explicit field `reads` — so a case can hold a build that reads
//! the recorded layout and a build that reads only the next one side by side. The last case
//! runs the shipped adapter's own `validate` over the same recorded history, so the suite's
//! rule and the production declaration cannot drift apart.
//!
//! Explicit synchronization only: every switch is awaited, every record is read back from the
//! recording journal, and nothing sleeps, reaches a network or asserts on time.

use std::sync::{Arc, Mutex};

use p1_contracts::serde_json::json;
use p1_contracts::{
    AssistantBlock, AssistantItem, BoxFuture, CancellationToken, CompletedResponse, Item,
    JournalRecord, ModelOptions, Origin, Outcome, Provider, ProviderError, ProviderErrorKind,
    ProviderRequest, ProviderStream, RecordBody, ReplayData, RouteDescription, StopReason,
    StreamEvent, TurnEnd,
};
use p1_core::{Agent, AgentParts, BuildError, Reconfiguration, ReconfigureError};
use p1_model_profile::ModelProfile;
use p1_module_tests::within_deadline;
use p1_provider_openai_chat::{ChatRoute, validate_request};
use p1_testkit::{
    FakeTool, PassthroughContext, RecordingEvents, RecordingJournal, ScriptedAuthorization,
    ScriptedProvider, Step, origin, text_response,
};

/// The reasoning text the session's replay payload holds: it must travel back byte-exact.
const RECORDED_REASONING: &str = "the reasoning the session was recorded with";

/// The layout version the shipped chat adapter writes and reads. This is the recorded version
/// a compatible replacement declares (`crates/p1-provider-openai-chat/src/request.rs`,
/// `lower_item`: a same-origin replay at any other version is refused).
const RECORDED_VERSION: u32 = 1;

/// The next layout: a build that reads only this one cannot read what the session recorded.
const NEXT_VERSION: u32 = 2;

/// A model profile the chat dialect can express, as `profiles/<id>.toml` states one. The
/// adapter's `validate` reads the effort table; nothing here ever reaches a wire.
const PROFILE: &str = "\
id       = \"fake-model\"
revision = 1
model_id = \"fake-model\"
family   = \"fake\"
thinking = \"enabled\"
efforts  = [\"low\", \"high\"]
";

// ------------------------------------------------------------------ the recorded session

/// The blocks of one assistant item that reasoned: the displayed reasoning plus the opaque
/// replay data the build configured for `origin` wrote, at the layout version it wrote.
fn recorded_blocks(origin: &Origin, version: u32) -> Vec<AssistantBlock> {
    vec![
        AssistantBlock::Reasoning {
            text: "thinking, shown as the provider reported it".into(),
            replay: Some(ReplayData {
                origin: origin.clone(),
                version,
                payload: json!(RECORDED_REASONING),
            }),
        },
        AssistantBlock::Text {
            text: "hello".into(),
        },
    ]
}

/// The scripted answer of the build that records the session.
fn recorded_turn(origin: &Origin, version: u32) -> Step {
    Step::Events(vec![StreamEvent::Finished(Outcome::Completed(
        CompletedResponse {
            item: AssistantItem {
                origin: origin.clone(),
                blocks: recorded_blocks(origin, version),
            },
            stop: StopReason::EndTurn,
            usage: None,
        },
    ))])
}

/// The current history of a session that recorded one reasoning replay at `version`.
fn recorded_history(origin: &Origin, version: u32) -> Vec<Item> {
    vec![
        Item::User { text: "hi".into() },
        Item::Assistant(AssistantItem {
            origin: origin.clone(),
            blocks: recorded_blocks(origin, version),
        }),
    ]
}

// ------------------------------------------------------------------ the provider builds

/// What one build made of one recorded reasoning replay, for the test to read back.
#[derive(Debug, Clone, PartialEq)]
enum Reading {
    /// Its own origin's, at a version this build reads: carried byte-exact into the request.
    Carried { version: u32 },
    /// Another origin's: dropped entirely, never rendered as text and never refused (ADR-0018).
    Dropped { origin: Origin, version: u32 },
    /// Its own origin's, at a version this build does not read: refused (ADR-0049).
    Refused { version: u32 },
}

/// A provider that DECLARES the layout versions of its own origin's replay data it reads
/// (ADR-0049) over a scripted stream. `describe` reports the origin it is configured for, so a
/// case can hold a build of the session's own route and a build of another route; `validate`
/// runs the shipped rule and records what it made of every replay it was shown.
struct ReplayReader {
    inner: ScriptedProvider,
    /// The origin this build is configured for: `Origin { route, model }` (ADR-0018).
    origin: Origin,
    /// The layout versions of its own origin's replay data this build reads.
    reads: Vec<u32>,
    readings: Arc<Mutex<Vec<Reading>>>,
}

impl ReplayReader {
    fn new(inner: ScriptedProvider, origin: Origin, reads: Vec<u32>) -> Self {
        Self {
            inner,
            origin,
            reads,
            readings: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// Every recorded replay the last `validate` looked at, in the order it met them.
    fn readings(&self) -> Vec<Reading> {
        self.readings.lock().unwrap().clone()
    }

    /// Every request the scripted stream was asked to answer.
    fn requests(&self) -> Vec<ProviderRequest> {
        self.inner.requests()
    }

    /// Every request `validate` was asked about.
    fn validated(&self) -> Vec<ProviderRequest> {
        self.inner.validated()
    }

    /// The version text the refusal names, as this build declares it.
    fn reads_text(&self) -> String {
        self.reads
            .iter()
            .map(u32::to_string)
            .collect::<Vec<_>>()
            .join(", ")
    }

    /// The one replay rule, run over a history: another origin's data is dropped, its own at a
    /// version it does not read is refused with a sentence naming the item, its origin and both
    /// versions — the shape `lower_item` refuses in.
    fn read_replay(&self, history: &[Item]) -> Result<(), ProviderError> {
        let mut readings = Vec::new();
        for item in history {
            let Item::Assistant(item) = item else {
                continue;
            };
            for block in &item.blocks {
                let AssistantBlock::Reasoning {
                    replay: Some(data), ..
                } = block
                else {
                    continue;
                };
                if data.origin != self.origin {
                    readings.push(Reading::Dropped {
                        origin: data.origin.clone(),
                        version: data.version,
                    });
                    continue;
                }
                if !self.reads.contains(&data.version) {
                    readings.push(Reading::Refused {
                        version: data.version,
                    });
                    self.readings.lock().unwrap().extend(readings);
                    return Err(ProviderError::new(
                        ProviderErrorKind::InvalidRequest,
                        format!(
                            "cannot replay the reasoning block of the assistant item from {}/{}: \
                             its replay data is version {}, this route reads version {}",
                            data.origin.route,
                            data.origin.model,
                            data.version,
                            self.reads_text(),
                        ),
                    ));
                }
                readings.push(Reading::Carried {
                    version: data.version,
                });
            }
        }
        self.readings.lock().unwrap().extend(readings);
        Ok(())
    }
}

impl Provider for ReplayReader {
    fn describe(&self) -> RouteDescription {
        RouteDescription {
            origin: self.origin.clone(),
            ..self.inner.describe()
        }
    }

    fn validate(&self, request: &ProviderRequest) -> Result<(), ProviderError> {
        // The scripted half records what it was asked, as the testkit provider does.
        self.inner.validate(request)?;
        self.read_replay(&request.history)
    }

    fn stream<'a>(
        &'a self,
        request: ProviderRequest,
        cancel: CancellationToken,
    ) -> BoxFuture<'a, Result<ProviderStream, ProviderError>> {
        self.inner.stream(request, cancel)
    }
}

// ------------------------------------------------------------------ the harness

/// The session that recorded the replay data, still running on the build that recorded it.
struct Session {
    agent: Agent,
    journal: Arc<RecordingJournal>,
    /// The build that recorded the session: the provider the session runs.
    original: Arc<ReplayReader>,
    /// The history the switch has to keep honest, as the agent holds it.
    history: Vec<Item>,
}

/// One turn of a scripted build of `origin` whose answer carries a reasoning replay at
/// `version`, leaving the session with `Environment`, `UserInput` and `AssistantCompleted`.
async fn session(origin: &Origin, version: u32) -> Session {
    let journal = Arc::new(RecordingJournal::new());
    let original = Arc::new(ReplayReader::new(
        ScriptedProvider::new(vec![
            recorded_turn(origin, version),
            text_response("the session's own build still answers"),
        ]),
        origin.clone(),
        vec![version],
    ));
    let mut agent = Agent::new(AgentParts {
        provider: original.clone() as Arc<dyn Provider>,
        tools: Vec::new(),
        system_prompt: "prompt".into(),
        options: ModelOptions::default(),
        context: Arc::new(PassthroughContext),
        authorization: Arc::new(ScriptedAuthorization::permit_all()),
        journal: journal.clone(),
        events: Arc::new(RecordingEvents::new()),
    })
    .expect("the session assembles");
    let end = turn(&mut agent, "hi").await;
    assert!(matches!(end, TurnEnd::Completed { .. }), "{end:?}");
    assert_eq!(
        journal.records().len(),
        3,
        "the environment, the input and the answer"
    );
    let history = agent.history().to_vec();
    assert_eq!(history, recorded_history(origin, version));
    Session {
        agent,
        journal,
        original,
        history,
    }
}

/// The candidate a switch installs: another build of the session's route, with its own prompt
/// and tool set.
fn candidate(provider: Arc<dyn Provider>) -> Reconfiguration {
    Reconfiguration {
        provider,
        tools: vec![Arc::new(FakeTool::new("read"))],
        system_prompt: "the replacement's prompt".into(),
        options: ModelOptions::default(),
        context: Arc::new(PassthroughContext),
        authorization: None,
    }
}

/// One turn, bounded: a case that stopped making progress fails rather than hangs.
async fn turn(agent: &mut Agent, input: &str) -> TurnEnd {
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        agent.run_turn(input.into(), CancellationToken::new()),
    )
    .await
    .expect("the turn finished")
}

fn environments(records: &[JournalRecord]) -> Vec<&JournalRecord> {
    records
        .iter()
        .filter(|record| matches!(record.body, RecordBody::Environment { .. }))
        .collect()
}

fn assert_dense(records: &[JournalRecord]) {
    for (index, record) in records.iter().enumerate() {
        assert_eq!(record.seq, index as u64, "{records:#?}");
    }
}

// ------------------------------------------------------------------ the cases

/// A compatible upgrade: the replacement declares the recorded layout version (and the next
/// one) and its `validate` passes over the current history. The switch commits ONE
/// `Environment`, and the turn after it carries the recorded replay to the new build
/// byte-exact.
#[tokio::test]
async fn a_compatible_upgrade_is_accepted_and_its_next_request_carries_the_recorded_replay() {
    within_deadline(
        "a_compatible_upgrade_is_accepted_and_its_next_request_carries_the_recorded_replay",
        async {
            let mut session = session(&origin(), RECORDED_VERSION).await;
            let replacement = Arc::new(ReplayReader::new(
                ScriptedProvider::new(vec![text_response("two")]),
                origin(),
                vec![RECORDED_VERSION, NEXT_VERSION],
            ));
            session
                .agent
                .reconfigure(candidate(replacement.clone() as Arc<dyn Provider>))
                .await
                .expect("the replacement reads the version the session recorded");

            // The record exists when the call returns, at the next dense seq, and it is the
            // candidate's.
            let records = session.journal.records();
            assert_eq!(records.len(), 4, "{records:#?}");
            match &records[3].body {
                RecordBody::Environment {
                    route,
                    system_prompt,
                    tools,
                    options,
                } => {
                    assert_eq!(route.origin, origin());
                    assert_eq!(system_prompt, "the replacement's prompt");
                    assert_eq!(tools.len(), 1);
                    assert_eq!(*options, ModelOptions::default());
                }
                other => panic!("expected the candidate's Environment at seq 3, saw {other:?}"),
            }

            // The candidate was validated against the CURRENT history: the one carrying the
            // recorded replay, and it read that replay.
            let validated = replacement.validated();
            assert_eq!(validated.len(), 1);
            assert_eq!(validated[0].history, session.history);
            assert_eq!(
                replacement.readings(),
                vec![Reading::Carried {
                    version: RECORDED_VERSION
                }]
            );

            // The turn after the switch asks the new build, and its request still carries the
            // replay: same origin, same version, same payload.
            let end = turn(&mut session.agent, "more").await;
            assert!(matches!(end, TurnEnd::Completed { .. }), "{end:?}");
            let requests = replacement.requests();
            assert_eq!(requests.len(), 1, "the new build answered the turn");
            assert_eq!(
                requests[0].history[..session.history.len()],
                session.history[..],
                "the recorded replay travels verbatim"
            );
            assert_eq!(requests[0].history.len(), session.history.len() + 1);
            assert_eq!(
                session.original.requests().len(),
                1,
                "the replaced build got the first turn only"
            );

            let records = session.journal.records();
            assert_dense(&records);
            assert_eq!(
                environments(&records).len(),
                2,
                "the turn after the switch writes no second Environment"
            );
        },
    )
    .await;
}

/// An explicit rejection: the replacement declares only the next layout, so the recorded
/// replay it cannot read fails activation. `Rejected` names the item and both versions, no
/// `Environment` is committed, and the session's own build keeps answering with the replay.
#[tokio::test]
async fn a_replacement_that_reads_only_the_next_layout_is_rejected_and_installs_nothing() {
    within_deadline(
        "a_replacement_that_reads_only_the_next_layout_is_rejected_and_installs_nothing",
        async {
            let mut session = session(&origin(), RECORDED_VERSION).await;
            let replacement = Arc::new(ReplayReader::new(
                ScriptedProvider::new(Vec::new()),
                origin(),
                vec![NEXT_VERSION],
            ));
            let error = session
                .agent
                .reconfigure(candidate(replacement.clone() as Arc<dyn Provider>))
                .await
                .expect_err("this build cannot read what the session recorded");
            match error {
                ReconfigureError::Rejected(BuildError::ProviderRejected(error)) => {
                    assert_eq!(error.kind, ProviderErrorKind::InvalidRequest);
                    for part in [
                        "cannot replay the reasoning block of the assistant item from \
                         fake-route/fake-model",
                        "its replay data is version 1",
                        "this route reads version 2",
                    ] {
                        assert!(error.message.contains(part), "{}: {part}", error.message);
                    }
                }
                other => panic!("expected the provider's refusal, saw {other:?}"),
            }
            assert_eq!(
                replacement.readings(),
                vec![Reading::Refused {
                    version: RECORDED_VERSION
                }]
            );
            assert!(
                replacement.requests().is_empty(),
                "a refused candidate is never asked for a response"
            );

            // Nothing was committed: no `Environment` for the candidate, no sequence number used.
            let records = session.journal.records();
            assert_eq!(records.len(), 3, "{records:#?}");
            assert_eq!(
                environments(&records).len(),
                1,
                "the environment never changed"
            );

            // The session's own build answers the next turn, with the recorded replay in its
            // request; the turn takes the next dense sequence number.
            let end = turn(&mut session.agent, "more").await;
            assert!(matches!(end, TurnEnd::Completed { .. }), "{end:?}");
            let requests = session.original.requests();
            assert_eq!(
                requests.len(),
                2,
                "the session's own build answered both turns"
            );
            assert_eq!(
                requests[1].history[..session.history.len()],
                session.history[..]
            );
            let records = session.journal.records();
            assert_dense(&records);
            assert_eq!(records.len(), 5, "{records:#?}");
            assert!(matches!(records[3].body, RecordBody::UserInput { .. }));
            assert_eq!(environments(&records).len(), 1);
        },
    )
    .await;
}

/// A foreign-origin replay never blocks a switch: only the build that wrote the data can be
/// asked to read it. It is dropped as today (ADR-0018), whatever its version.
#[tokio::test]
async fn a_foreign_origin_replay_is_dropped_and_never_blocks_the_switch() {
    within_deadline(
        "a_foreign_origin_replay_is_dropped_and_never_blocks_the_switch",
        async {
            // The session ran on another route and model and recorded its reasoning at a
            // layout version the replacement does not read either.
            let elsewhere = Origin {
                route: "other-route".into(),
                model: "other-model".into(),
            };
            let mut session = session(&elsewhere, 7).await;
            let replacement = Arc::new(ReplayReader::new(
                ScriptedProvider::new(vec![text_response("two")]),
                origin(),
                vec![RECORDED_VERSION],
            ));
            session
                .agent
                .reconfigure(candidate(replacement.clone() as Arc<dyn Provider>))
                .await
                .expect("another origin's replay never refuses a switch");
            let dropped = Reading::Dropped {
                origin: elsewhere,
                version: 7,
            };
            assert_eq!(replacement.readings(), vec![dropped.clone()]);

            let records = session.journal.records();
            assert_eq!(records.len(), 4, "{records:#?}");
            assert_eq!(environments(&records).len(), 2);

            // The core carries replay data verbatim, so the turned-away item is still in the
            // request; the replacement's own reading is what drops it.
            let end = turn(&mut session.agent, "more").await;
            assert!(matches!(end, TurnEnd::Completed { .. }), "{end:?}");
            let requests = replacement.requests();
            assert_eq!(requests.len(), 1);
            assert_eq!(
                requests[0].history[..session.history.len()],
                session.history[..]
            );
            assert_eq!(
                replacement.readings(),
                vec![dropped],
                "the switch was the only `validate` this build ran"
            );
        },
    )
    .await;
}

/// A switch back to the session's own build after a refused candidate succeeds: the refusal
/// changed nothing, so the session still carries the replay its own build wrote.
#[tokio::test]
async fn a_switch_back_to_the_sessions_own_build_after_a_refusal_succeeds() {
    within_deadline(
        "a_switch_back_to_the_sessions_own_build_after_a_refusal_succeeds",
        async {
            let mut session = session(&origin(), RECORDED_VERSION).await;
            let refusing = Arc::new(ReplayReader::new(
                ScriptedProvider::new(Vec::new()),
                origin(),
                vec![NEXT_VERSION],
            ));
            session
                .agent
                .reconfigure(candidate(refusing.clone() as Arc<dyn Provider>))
                .await
                .expect_err("this build cannot read the recorded layout");
            assert_eq!(
                session.journal.records().len(),
                3,
                "the refusal committed nothing"
            );

            // The build that wrote the recorded replay declares the version it wrote, so the
            // session may switch to it.
            let original = session.original.clone() as Arc<dyn Provider>;
            session
                .agent
                .reconfigure(candidate(original))
                .await
                .expect("the build that wrote the recorded replay reads it");
            let records = session.journal.records();
            assert_eq!(records.len(), 4, "{records:#?}");
            assert_eq!(environments(&records).len(), 2);
            assert!(refusing.requests().is_empty());

            let end = turn(&mut session.agent, "again").await;
            assert!(matches!(end, TurnEnd::Completed { .. }), "{end:?}");
            assert_eq!(
                session.original.requests().len(),
                2,
                "the session's own build answered the later turn"
            );
            assert_dense(&session.journal.records());
        },
    )
    .await;
}

/// The declaration the replacement builds above mirror, run for real over the same history:
/// the shipped chat adapter reads ONE layout version of its own origin's reasoning replay and
/// refuses any other, naming the item, its origin and both versions (ADR-0049). `validate`
/// runs that very lowering, so activation refuses exactly what the request builder would.
#[test]
fn the_shipped_chat_adapter_declares_the_recorded_version_and_refuses_another() {
    let route = ChatRoute {
        origin_route: origin().route,
        endpoint: "https://example.invalid/v1/chat/completions".into(),
        ..ChatRoute::default()
    };
    let profile = ModelProfile::from_toml("fake-model", PROFILE).expect("the profile parses");
    let request = |history: Vec<Item>| ProviderRequest {
        system_prompt: "prompt".into(),
        history,
        tools: Vec::new(),
        options: ModelOptions::default(),
    };

    validate_request(
        &route,
        "fake-model",
        &profile,
        &request(recorded_history(&origin(), RECORDED_VERSION)),
    )
    .expect("the shipped chat route reads the version the session recorded");

    let error = validate_request(
        &route,
        "fake-model",
        &profile,
        &request(recorded_history(&origin(), NEXT_VERSION)),
    )
    .expect_err("and refuses its own replay data at another version");
    assert_eq!(error.kind, ProviderErrorKind::InvalidRequest);
    for part in [
        "cannot replay the reasoning block of the assistant item from fake-route/fake-model",
        "its replay data is version 2",
        "this route reads version 1",
    ] {
        assert!(error.message.contains(part), "{}: {part}", error.message);
    }
}
