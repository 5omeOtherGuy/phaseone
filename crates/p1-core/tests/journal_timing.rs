//! Implementer's tests for ADR-0121 request timing (issue #422).
//!
//! Determinism rules of the suite: fake clocks only, explicit timeouts, no sleeps,
//! no network, no filesystem.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use p1_contracts::{
    BoxFuture, CancellationToken, Clock, CommitError, CommitSink, JournalRecord, ModelOptions,
    Outcome, RecordBody, StopReason, StreamEvent, TurnEnd, Wait, WaitReason,
};
use p1_core::{Agent, AgentParts, project};
use p1_testkit::{
    PassthroughContext, RecordingEvents, RecordingJournal, ScriptedAuthorization, ScriptedProvider,
    Step, completed, origin, text_block,
};

const LIMIT: Duration = Duration::from_secs(5);

/// A core clock that hands out the scripted ticks in order; the test IS the
/// timeline, so an exhausted script is a test error.
#[derive(Debug)]
struct ScriptedClock {
    ticks: Mutex<VecDeque<u64>>,
}

impl ScriptedClock {
    fn new(ticks: &[u64]) -> Arc<Self> {
        Arc::new(Self {
            ticks: Mutex::new(ticks.iter().copied().collect()),
        })
    }
}

impl Clock for ScriptedClock {
    fn now_ms(&self) -> u64 {
        self.ticks
            .lock()
            .unwrap()
            .pop_front()
            .expect("the core read the clock more often than the script provides")
    }
}

/// Forwarding sink that declines `RequestTiming`, as a version-1/2 file does.
#[derive(Clone)]
struct NoTimingJournal(RecordingJournal);

impl CommitSink for NoTimingJournal {
    fn commit<'a>(&'a self, record: &'a JournalRecord) -> BoxFuture<'a, Result<(), CommitError>> {
        self.0.commit(record)
    }

    fn accepts_request_timing(&self) -> bool {
        false
    }
}

fn parts(journal: Arc<dyn CommitSink>) -> AgentParts {
    AgentParts {
        provider: Arc::new(ScriptedProvider::new(vec![example_script()])),
        tools: vec![],
        system_prompt: "timing".into(),
        options: ModelOptions::default(),
        context: Arc::new(PassthroughContext),
        authorization: Arc::new(ScriptedAuthorization::permit_all()),
        journal,
        events: Arc::new(RecordingEvents::new()),
    }
}

/// A provider script that yields exactly the ADR-0121 example's events: a wait
/// between send and the first event, then an `Activity` (first event), a
/// `TextDelta` (first output) and a `Finished` (end).
fn example_script() -> Step {
    Step::Events(vec![
        StreamEvent::Wait {
            reason: WaitReason::RateLimited,
            attempt: 1,
            delay_ms: 2000,
        },
        StreamEvent::Activity,
        StreamEvent::TextDelta {
            block: 0,
            text: "hello".into(),
        },
        StreamEvent::Finished(completed(
            vec![text_block("hello")],
            StopReason::EndTurn,
            None,
        )),
    ])
}

async fn run(agent: &mut Agent, input: &str) -> TurnEnd {
    tokio::time::timeout(
        LIMIT,
        agent.run_turn(input.into(), CancellationToken::new()),
    )
    .await
    .expect("run_turn hung")
}

/// DoD 2: the example in ADR-0121's brief is recorded exactly by the core.
#[tokio::test(flavor = "current_thread")]
async fn the_adr_0121_timing_example_is_recorded_exactly() {
    let recording = RecordingJournal::new();
    let mut agent = Agent::new(parts(Arc::new(recording.clone()))).unwrap();
    // 1000 at send, 1250 at the first event (Activity), 1400 at the first output
    // (TextDelta), 1900 at Finished. The `Wait` reads NO tick.
    agent.set_clock(ScriptedClock::new(&[1000, 1250, 1400, 1900]));

    assert_eq!(
        run(&mut agent, "hi").await,
        TurnEnd::Completed {
            stop: StopReason::EndTurn
        }
    );

    let records = recording.records();
    // The timing record sits immediately after its AssistantCompleted.
    let completed_at = records
        .iter()
        .position(|record| matches!(record.body, RecordBody::AssistantCompleted { .. }))
        .expect("no AssistantCompleted record");
    assert_eq!(
        records[completed_at + 1].body,
        RecordBody::RequestTiming {
            request_index: 0,
            sent_ms: 1000,
            first_event_ms: Some(1250),
            first_output_ms: Some(1400),
            ended_ms: 1900,
            waits: vec![Wait {
                reason: WaitReason::RateLimited,
                attempt: 1,
                delay_ms: 2000,
            }],
        }
    );
    // `seq` stays dense.
    assert_eq!(
        records.iter().map(|record| record.seq).collect::<Vec<_>>(),
        (0..records.len() as u64).collect::<Vec<_>>()
    );
}

/// A retried request's first event is the answer's, not the failed attempt's: the
/// adapter's `Notice` and back-off `Activity` before a `Wait` do not count.
#[tokio::test(flavor = "current_thread")]
async fn a_retry_wait_restarts_the_first_event() {
    let recording = RecordingJournal::new();
    let mut parts = parts(Arc::new(recording.clone()));
    parts.provider = Arc::new(ScriptedProvider::new(vec![Step::Events(vec![
        StreamEvent::Notice {
            text: "retrying".into(),
        },
        StreamEvent::Activity,
        StreamEvent::Wait {
            reason: WaitReason::RateLimited,
            attempt: 1,
            delay_ms: 2000,
        },
        StreamEvent::Activity,
        StreamEvent::TextDelta {
            block: 0,
            text: "hello".into(),
        },
        StreamEvent::Finished(completed(
            vec![text_block("hello")],
            StopReason::EndTurn,
            None,
        )),
    ])]));
    let mut agent = Agent::new(parts).unwrap();
    // send, back-off Activity, answer Activity, TextDelta, Finished. The Notice and
    // the Wait read no tick.
    agent.set_clock(ScriptedClock::new(&[1000, 1100, 3200, 3300, 3500]));
    run(&mut agent, "hi").await;

    let timing = recording
        .records()
        .into_iter()
        .find(|record| matches!(record.body, RecordBody::RequestTiming { .. }))
        .expect("no RequestTiming record");
    assert!(
        matches!(
            timing.body,
            RecordBody::RequestTiming {
                sent_ms: 1000,
                first_event_ms: Some(3200),
                first_output_ms: Some(3300),
                ended_ms: 3500,
                ..
            }
        ),
        "{:?}",
        timing.body
    );
}

/// DoD 2: `RequestTiming` is never history. A projection over records that
/// contain one yields the same items as without it, and still counts its seq.
#[tokio::test(flavor = "current_thread")]
async fn project_skips_request_timing() {
    let completed_item = match completed(vec![text_block("answer")], StopReason::EndTurn, None) {
        Outcome::Completed(response) => response.item,
        other => panic!("expected Completed, got {other:?}"),
    };
    let records = vec![
        JournalRecord {
            seq: 0,
            body: RecordBody::Environment {
                route: p1_contracts::RouteDescription {
                    origin: origin(),
                    supports_freeform_tools: true,
                    mandatory_prompt_prefix: None,
                    reports_cost: false,
                    cache_key: p1_contracts::CacheKeySupport::Unsupported,
                },
                system_prompt: "timing".into(),
                tools: Vec::new(),
                options: ModelOptions::default(),
            },
        },
        JournalRecord {
            seq: 1,
            body: RecordBody::UserInput { text: "hi".into() },
        },
        JournalRecord {
            seq: 2,
            body: RecordBody::AssistantCompleted {
                item: completed_item,
                stop: StopReason::EndTurn,
                usage: None,
            },
        },
        // ADR-0121: never history; the projection must skip it and keep counting seq.
        JournalRecord {
            seq: 3,
            body: RecordBody::RequestTiming {
                request_index: 0,
                sent_ms: 10,
                first_event_ms: Some(20),
                first_output_ms: Some(30),
                ended_ms: 40,
                waits: vec![],
            },
        },
    ];
    let projection = project(&records).unwrap();
    assert_eq!(
        projection.history.len(),
        2,
        "the timing record must add no history item"
    );
    assert_eq!(
        projection.next_seq, 4,
        "the timing record still counts its seq"
    );
    assert!(projection.unresolved_calls.is_empty());
}

/// DoD 2: a sink that declines `RequestTiming` (a version-1/2 file) gets none,
/// and `seq` stays dense.
#[tokio::test(flavor = "current_thread")]
async fn a_sink_that_declines_commits_no_request_timing() {
    let recording = RecordingJournal::new();
    let mut agent = Agent::new(parts(Arc::new(NoTimingJournal(recording.clone())))).unwrap();
    agent.set_clock(ScriptedClock::new(&[1000, 1250, 1400, 1900]));

    assert_eq!(
        run(&mut agent, "hi").await,
        TurnEnd::Completed {
            stop: StopReason::EndTurn
        }
    );
    let records = recording.records();
    assert!(
        records
            .iter()
            .all(|record| !matches!(record.body, RecordBody::RequestTiming { .. })),
        "a declining sink must receive no RequestTiming: {records:?}"
    );
    assert_eq!(
        records.iter().map(|record| record.seq).collect::<Vec<_>>(),
        (0..records.len() as u64).collect::<Vec<_>>(),
        "seq stays dense without the timing record"
    );
}
