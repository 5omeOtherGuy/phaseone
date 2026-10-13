//! E3: the channel fails closed. Needs the guests built by `build-guest.sh`.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use p1_channel_spike::{BoxFuture, Capability, Grants, Handles, LoadError, Plugin};
use p1_module_protocol::{WireToolCall, WireToolInput, WireToolOutcome, WireToolStatus};
use wasmtime::Engine;

fn guest(variant: &str) -> Vec<u8> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../modules/target/p1-channel")
        .join(format!("channel-fixture-{variant}.wasm"));
    std::fs::read(&path).unwrap_or_else(|error| {
        panic!(
            "{}: {error}; run crates/p1-channel-spike/build-guest.sh",
            path.display()
        )
    })
}

fn engine() -> Engine {
    p1_module_runtime::engine().expect("engine")
}

/// A process service that only counts what it was asked to run.
#[derive(Default)]
struct CountingProcess {
    spawned: AtomicUsize,
}

impl Capability for CountingProcess {
    fn call<'a>(
        &'a self,
        _handles: &'a mut Handles,
        method: &'a str,
        _payload: &'a str,
    ) -> BoxFuture<'a, Result<String, String>> {
        assert_eq!(method, "spawn");
        self.spawned.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { Ok("spawned".to_owned()) })
    }
}

fn call(raw: &str) -> String {
    serde_json::to_string(&WireToolCall {
        call_id: "c1".to_owned(),
        name: "fixture".to_owned(),
        input: WireToolInput::Text(raw.to_owned()),
    })
    .expect("a call serializes")
}

async fn execute(plugin: &Plugin, raw: &str) -> WireToolOutcome {
    let answer = plugin
        .emit("tool/execute", &call(raw), HashMap::new())
        .await
        .expect("the call runs")
        .expect("the plugin answers tool/execute");
    serde_json::from_str(&answer).expect("a wire tool outcome")
}

#[tokio::test(flavor = "current_thread")]
async fn an_ungranted_capability_is_refused_and_runs_nothing() {
    let engine = engine();
    let process = Arc::new(CountingProcess::default());
    // The manifest asks for "process"; the environment grants nothing.
    let plugin = Plugin::load(&engine, &guest("default"), &Grants::new())
        .await
        .expect("loads");
    assert!(plugin.manifest().capabilities.contains("process"));
    assert_eq!(plugin.capabilities().count(), 0);

    let outcome = execute(&plugin, "spawn:rm -rf /").await;
    assert_eq!(outcome.status, WireToolStatus::Error);
    assert!(
        outcome.content.contains("not granted"),
        "{}",
        outcome.content
    );
    assert_eq!(process.spawned.load(Ordering::SeqCst), 0);

    // The same plugin with "process" granted does reach the service: the refusal above was
    // the grant, not a broken path.
    let grants: Grants = [("process".to_owned(), process.clone() as Arc<dyn Capability>)].into();
    let granted = Plugin::load(&engine, &guest("default"), &grants)
        .await
        .expect("loads");
    let outcome = execute(&granted, "spawn:true").await;
    assert_eq!(outcome.status, WireToolStatus::Ok, "{}", outcome.content);
    assert_eq!(process.spawned.load(Ordering::SeqCst), 1);
}

#[tokio::test(flavor = "current_thread")]
async fn a_granted_capability_not_in_the_manifest_is_not_reachable() {
    let engine = engine();
    let clock: Arc<dyn Capability> = Arc::new(CountingProcess::default());
    let grants: Grants = [("clock".to_owned(), clock)].into();
    let plugin = Plugin::load(&engine, &guest("default"), &grants)
        .await
        .expect("loads");
    // The environment granted "clock", but the manifest did not ask for it.
    assert_eq!(plugin.capabilities().count(), 0);
}

#[tokio::test(flavor = "current_thread")]
async fn a_capability_the_catalog_lacks_is_refused_at_load() {
    let process: Arc<dyn Capability> = Arc::new(CountingProcess::default());
    let grants: Grants = [("process".to_owned(), process)].into();
    match Plugin::load(&engine(), &guest("unknown-capability"), &grants).await {
        Err(LoadError::UnknownCapability(name)) => assert_eq!(name, "teleport"),
        Err(other) => panic!("refused for the wrong reason: {other}"),
        Ok(_) => panic!("a manifest naming an unknown capability loaded"),
    }
}

#[tokio::test(flavor = "current_thread")]
async fn an_event_the_manifest_does_not_list_is_never_delivered() {
    let plugin = Plugin::load(&engine(), &guest("default"), &Grants::new())
        .await
        .expect("loads");
    assert!(!plugin.manifest().events.contains_key("session/start"));
    for _ in 0..3 {
        let answer = plugin
            .emit("session/start", "{}", HashMap::new())
            .await
            .expect("not an error");
        assert_eq!(answer, None);
    }
    assert_eq!(plugin.delivered(), 0);

    // A listed event is delivered and counted.
    execute(&plugin, "echo:hi").await;
    assert_eq!(plugin.delivered(), 1);
}

/// E4: `tool/post-execute` is a catalog row, this emit and a match arm in the guest; the WIT
/// world is unchanged.
#[tokio::test(flavor = "current_thread")]
async fn a_new_event_is_delivered_without_a_wit_change() {
    let plugin = Plugin::load(&engine(), &guest("default"), &Grants::new())
        .await
        .expect("loads");
    let outcome = execute(&plugin, "echo:hi").await;
    let payload = format!(
        r#"{{"call":{},"outcome":{}}}"#,
        call("echo:hi"),
        serde_json::to_string(&outcome).expect("serializes")
    );
    let answer = plugin
        .emit("tool/post-execute", &payload, HashMap::new())
        .await
        .expect("runs");
    assert_eq!(answer.as_deref(), Some(r#"{"note":"c1 ended ok"}"#));
    assert_eq!(plugin.delivered(), 2);
}
