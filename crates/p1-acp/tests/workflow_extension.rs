//! The published eight-event extension fixture is also its serde contract.
use p1_acp::capabilities::initialize;
use p1_acp::extensions::workflow::WorkflowEvent;
use serde_json::{Value, json};

#[test]
fn workflow_events_round_trip_published_fixture() {
    let fixture = include_str!("../../../docs/acp/fixtures/workflow-run-p1dev.jsonl");
    let mut kinds = Vec::new();
    for line in fixture.lines() {
        let entry: Value = serde_json::from_str(line).unwrap();
        let wire = entry["msg"]["params"]["event"].clone();
        kinds.push(wire["type"].as_str().unwrap().to_string());
        let event: WorkflowEvent = serde_json::from_value(wire.clone()).unwrap();
        assert_eq!(serde_json::to_value(&event).unwrap(), wire);
        assert_eq!(
            serde_json::from_str::<WorkflowEvent>(&serde_json::to_string(&event).unwrap()).unwrap(),
            event
        );
    }
    assert_eq!(
        kinds,
        [
            "run_started",
            "phase",
            "log",
            "jobs_queued",
            "step_started",
            "step_ended",
            "thunk_failed",
            "run_ended"
        ]
    );
}

#[test]
fn workflow_capability_is_opt_in_and_deduplicated() {
    for (declaration, enabled) in [
        (
            json!({"version":1,"capabilities":["workflow_update","future","workflow_update"]}),
            true,
        ),
        (json!({"version":1,"capabilities":[]}), false),
        (json!({"version":1,"capabilities":["future"]}), false),
        (
            json!({"version":2,"capabilities":["workflow_update"]}),
            false,
        ),
        (
            json!({"version":1,"capabilities":["workflow_update",4]}),
            false,
        ),
    ] {
        let meta = json!({"p1.dev":declaration});
        let (codec, capabilities) = initialize(1, meta.as_object());
        assert_eq!(capabilities.workflow_update, enabled);
        let wire = codec.encode_capabilities(&capabilities);
        if enabled {
            assert_eq!(
                wire["agentCapabilities"]["_meta"]["p1.dev"]["extensions"],
                json!(["workflow_update"])
            );
        } else if capabilities.p1_extensions {
            assert_eq!(
                wire["agentCapabilities"]["_meta"]["p1.dev"]["extensions"],
                json!([])
            );
        } else {
            assert!(wire["agentCapabilities"].get("_meta").is_none());
        }
    }
}
