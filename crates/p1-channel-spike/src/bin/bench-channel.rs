//! E1 and E2 of the channel spike, in the profile `bench-baseline` runs in (the workspace's
//! debug profile). Every measured call is checked to have produced its expected answer.
//!
//! E1: `tool/execute` with `echo:hi` (and a 1 KiB echo) over the channel, each call with its
//! own Store and instance, timed from the wire call's serialization to the decoded outcome,
//! as `bench-baseline` times `execute`. The same calls of the existing fixture's `execute`
//! export, through this crate's harness (same Store setup, imports trapping), separate the
//! WIT shape from the existing runtime's own machinery.
//!
//! E2: a synthetic history of 1 MiB and 32 MiB to `context/prepare`, (a) inline in the
//! payload and parsed by the guest (a0: handed over, not parsed), (b) by `open("history")` with only the last 4 KiB read,
//! (c) all of it read in 64 KiB chunks, (d) as (c) and parsed; and, for reference, the
//! existing fixture echoing the history through `execute` (in and back out, no parse).

use std::collections::HashMap;
use std::future::Future;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use p1_channel_spike::{
    BoxFuture, Calling, Capability, Grants, Handles, Plugin, bare_store, start_epoch_ticker,
};
use p1_module_protocol::{WireToolCall, WireToolInput, WireToolOutcome, WireToolStatus};
use serde_json::{Value, json};
use wasmtime::Engine;
use wasmtime::component::{Component, InstancePre, Linker, Val};

const WARM_CALLS: usize = 50;
const P95_CALLS: usize = 500;
const HISTORY_1M_CALLS: usize = 20;
const HISTORY_32M_CALLS: usize = 10;
const MIB: usize = 1 << 20;

struct NoProcess;

impl Capability for NoProcess {
    fn call<'a>(
        &'a self,
        _handles: &'a mut Handles,
        _method: &'a str,
        _payload: &'a str,
    ) -> BoxFuture<'a, Result<String, String>> {
        Box::pin(async { Err("the bench runs nothing".to_owned()) })
    }
}

fn main() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("a Tokio runtime");
    runtime.block_on(bench());
}

async fn bench() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let engine = p1_module_runtime::engine().expect("engine");
    start_epoch_ticker(&engine);
    println!(
        "profile: {}",
        if cfg!(debug_assertions) {
            "dev"
        } else {
            "release"
        }
    );
    println!("tokio_runtime: current_thread");

    let grants: Grants = [(
        "process".to_owned(),
        Arc::new(NoProcess) as Arc<dyn Capability>,
    )]
    .into();
    let channel_bytes = read(&root.join("modules/target/p1-channel/channel-fixture-default.wasm"));
    let (channel, cold) = time_async(Plugin::load(&engine, &channel_bytes, &grants)).await;
    let channel = channel.expect("the channel fixture loads");
    println!(
        "channel_cold_load_incl_compile_and_manifest: {}",
        millis(cold)
    );
    let existing = Existing::load(
        &engine,
        &read(&root.join("modules/target/p1-modules/p1-module-fixture/p1-module-fixture.wasm")),
    );

    // E1.
    let kib = "x".repeat(1024);
    for (label, text) in [("echo_hi", "hi"), ("echo_1kib", kib.as_str())] {
        let raw = format!("echo:{text}");
        println!(
            "payload_{label}: {} bytes of call JSON",
            wire_call(&raw).len()
        );
        for calls in [WARM_CALLS, P95_CALLS] {
            for calling in [Calling::Typed, Calling::Dynamic] {
                let name = format!("e1_channel_{calling:?}_{label}").to_lowercase();
                let samples = series(calls, || async {
                    let answer = channel
                        .emit_with(calling, "tool/execute", &wire_call(&raw), HashMap::new())
                        .await
                        .expect("the call runs")
                        .expect("an answer");
                    check_outcome(&answer, text);
                })
                .await;
                report(&name, samples);
            }
            let name = format!("e1_existing_fixture_same_harness_{label}");
            let samples = series(calls, || async {
                let answer = existing.execute(wire_call(&raw)).await;
                check_outcome(&answer, text);
            })
            .await;
            report(&name, samples);
        }
    }

    // E2.
    for (label, bytes, calls) in [
        ("1mib", MIB, HISTORY_1M_CALLS),
        ("32mib", 32 * MIB, HISTORY_32M_CALLS),
    ] {
        let (history, items) = synthetic_history(bytes);
        println!("history_{label}: {} items, {} bytes", items, history.len());
        let shared: Arc<[u8]> = Arc::from(history.as_bytes());
        let data = || HashMap::from([("history".to_owned(), shared.clone())]);

        let inline = format!(r#"{{"mode":"inline","history":{history}}}"#);
        let samples = series(calls, || async {
            let answer = channel
                .emit("context/prepare", &inline, HashMap::new())
                .await
                .expect("runs")
                .expect("an answer");
            assert_eq!(field(&answer, "items"), items as u64, "{answer}");
        })
        .await;
        report(&format!("e2a_inline_payload_parsed_{label}"), samples);

        let unparsed = format!(r#"{{"mode":"inline-unparsed","history":{history}}}"#);
        let samples = series(calls, || async {
            let answer = channel
                .emit("context/prepare", &unparsed, HashMap::new())
                .await
                .expect("runs")
                .expect("an answer");
            assert_eq!(field(&answer, "len"), unparsed.len() as u64, "{answer}");
        })
        .await;
        report(&format!("e2a0_inline_payload_unparsed_{label}"), samples);

        for (mode, key, expected) in [
            ("tail", "read", 4096),
            ("all", "read", history.len() as u64),
            ("all-parse", "items", items as u64),
        ] {
            let payload = json!({ "mode": mode }).to_string();
            let samples = series(calls, || async {
                let answer = channel
                    .emit("context/prepare", &payload, data())
                    .await
                    .expect("runs")
                    .expect("an answer");
                assert_eq!(field(&answer, key), expected, "{answer}");
            })
            .await;
            let name = match mode {
                "tail" => "e2b_handle_last_4kib",
                "all" => "e2c_handle_all_64kib_chunks",
                _ => "e2d_handle_all_64kib_chunks_parsed",
            };
            report(&format!("{name}_{label}"), samples);
        }

        let echo = wire_call(&format!("echo:{history}"));
        let samples = series(calls, || async {
            let answer = existing.execute(echo.clone()).await;
            assert!(answer.len() >= history.len(), "the echo came back short");
        })
        .await;
        report(
            &format!("e2_ref_existing_fixture_echo_in_and_out_{label}"),
            samples,
        );
    }
}

/// The existing fixture's `execute` through this crate's harness: the same Store setup as a
/// channel call, a fresh instance per call, the export called with `Val`s as the existing
/// runtime calls it, every import a trap (the echo path calls none).
struct Existing {
    pre: InstancePre<p1_channel_spike::CallState>,
}

impl Existing {
    fn load(engine: &Engine, bytes: &[u8]) -> Self {
        let component = Component::from_binary(engine, bytes).expect("the fixture compiles");
        let mut linker = Linker::new(engine);
        linker
            .define_unknown_imports_as_traps(&component)
            .expect("trap imports");
        Self {
            pre: linker.instantiate_pre(&component).expect("links"),
        }
    }

    async fn execute(&self, call: String) -> String {
        let mut store = bare_store(self.pre.engine());
        let instance = self
            .pre
            .instantiate_async(&mut store)
            .await
            .expect("instance");
        let execute = instance.get_func(&mut store, "execute").expect("execute");
        let mut results = [Val::Bool(false)];
        execute
            .call_async(&mut store, &[Val::String(call)], &mut results)
            .await
            .expect("execute runs");
        match results {
            [Val::String(text)] => text,
            _ => panic!("execute returned no text"),
        }
    }
}

fn wire_call(raw: &str) -> String {
    serde_json::to_string(&WireToolCall {
        call_id: "c1".to_owned(),
        name: "fixture".to_owned(),
        input: WireToolInput::Text(raw.to_owned()),
    })
    .expect("a call serializes")
}

fn check_outcome(answer: &str, content: &str) {
    let outcome: WireToolOutcome = serde_json::from_str(answer).expect("a wire tool outcome");
    assert_eq!(outcome.status, WireToolStatus::Ok, "{}", outcome.content);
    assert_eq!(outcome.content, content);
}

fn field(answer: &str, key: &str) -> u64 {
    let value: Value = serde_json::from_str(answer).expect("JSON");
    value[key]
        .as_u64()
        .unwrap_or_else(|| panic!("no {key} in {answer}"))
}

/// A JSON array of history items in p1's wire shape (user, assistant with a tool call, tool
/// result), at least `bytes` long, and its item count.
fn synthetic_history(bytes: usize) -> (String, usize) {
    let mut items = Vec::new();
    let mut size = 2;
    let mut turn = 0;
    while size < bytes {
        let call_id = format!("call-{turn}");
        for item in [
            json!({"item": "user", "text": text(512, turn)}),
            json!({"item": "assistant", "origin": {"route": "fake/route", "model": "fake-model"},
                   "blocks": [{"block": "text", "text": text(1024, turn + 1)},
                              {"block": "tool_call", "call_id": call_id, "name": "read",
                               "input": {"kind": "json", "raw": format!(r#"{{"path":"src/module_{turn}.rs"}}"#)}}]}),
            json!({"item": "tool_result", "call_id": call_id, "status": "ok", "content": text(2048, turn + 2)}),
        ] {
            size += item.to_string().len() + 1;
            items.push(item);
        }
        turn += 1;
    }
    let count = items.len();
    (Value::Array(items).to_string(), count)
}

fn text(len: usize, seed: usize) -> String {
    const WORDS: [&str; 8] = [
        "alpha",
        "beta",
        "gamma",
        "delta",
        "\"quoted\"",
        "line\nbreak",
        "tab\t",
        "ünï",
    ];
    let mut out = String::with_capacity(len + 16);
    let mut i = seed;
    while out.len() < len {
        out.push_str(WORDS[i % WORDS.len()]);
        out.push(' ');
        i += 1;
    }
    out
}

/// One warm-up call (not recorded), then `calls` timed calls.
async fn series<F, Fut>(calls: usize, mut body: F) -> Vec<Duration>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = ()>,
{
    body().await;
    let mut samples = Vec::with_capacity(calls);
    for _ in 0..calls {
        let ((), took) = time_async(body()).await;
        samples.push(took);
    }
    samples
}

fn report(name: &str, mut samples: Vec<Duration>) {
    samples.sort_unstable();
    let n = samples.len();
    let median = if n.is_multiple_of(2) {
        (samples[n / 2 - 1] + samples[n / 2]) / 2
    } else {
        samples[n / 2]
    };
    let p95 = samples[(n * 95).div_ceil(100) - 1];
    println!(
        "{name} (n={n}): median {} p95 {} max {}",
        millis(median),
        millis(p95),
        millis(samples[n - 1])
    );
}

async fn time_async<T>(body: impl Future<Output = T>) -> (T, Duration) {
    let started = Instant::now();
    let value = body.await;
    (value, started.elapsed())
}

fn millis(duration: Duration) -> String {
    format!("{:.3} ms", duration.as_secs_f64() * 1000.0)
}

fn read(path: &Path) -> Vec<u8> {
    std::fs::read(path).unwrap_or_else(|error| {
        panic!("{}: {error}; run crates/p1-channel-spike/build-guest.sh and scripts/build-modules.sh --package p1-module-fixture", path.display())
    })
}
