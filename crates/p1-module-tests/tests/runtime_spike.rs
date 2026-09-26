//! The runtime spike (S0.5): the execution-model cases of the stop rule, each on a
//! current-thread and a multi-thread Tokio runtime, each under the deadlock guard.

use std::sync::Arc;

use p1_contracts::tool::ResultDescription;
use p1_contracts::{
    CancellationToken, DeclarationKind, Effect, Tool, ToolContext, ToolIdentity, ToolResultItem,
    ToolStatus,
};
use p1_module_runtime::{
    Digest, ExecutionLimits, ExitStatus, LoadError, ProcessEvent, Services, wasm_tool,
};
use p1_module_tests::{FIXTURE_NAME, Release, call, fake_processes};
use p1_redact::MaskCounter;

/// Generates the two flavours of case `$case` (an `async fn` below) as
/// `$case::current_thread` and `$case::multi_thread`.
macro_rules! on_both_flavours {
    ($($case:ident),* $(,)?) => {$(
        mod $case {
            #[tokio::test(flavor = "current_thread")]
            async fn current_thread() {
                p1_module_tests::within_deadline(
                    concat!(stringify!($case), " (current_thread)"),
                    super::$case(),
                )
                .await;
            }

            #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
            async fn multi_thread() {
                p1_module_tests::within_deadline(
                    concat!(stringify!($case), " (multi_thread)"),
                    super::$case(),
                )
                .await;
            }
        }
    )*};
}

on_both_flavours!(
    load_verifies_then_compiles_the_same_bytes,
    load_refuses_what_the_release_does_not_ship,
    inspection_is_synchronous_inside_a_running_task,
    execute_runs_each_call_on_a_fresh_instance,
    execute_awaits_an_async_host_import,
    concurrent_calls_all_complete,
    the_host_gets_the_redacting_wrapper,
);

/// The fixture tool with the fake process service, and the test's side of that service.
fn fixture_tool(release: &Release) -> (Arc<dyn Tool>, p1_module_tests::FakeProcesses) {
    let module = release
        .loader()
        .load(FIXTURE_NAME)
        .expect("load the fixture");
    let (process, processes) = fake_processes();
    let tool = wasm_tool(
        &module,
        Services {
            process: Some(process),
            summary: None,
        },
        ExecutionLimits::default(),
        &Arc::new(MaskCounter::new()),
    )
    .expect("the fixture is a tool");
    (tool, processes)
}

fn context() -> ToolContext {
    ToolContext {
        cancel: CancellationToken::new(),
    }
}

async fn load_verifies_then_compiles_the_same_bytes() {
    let mut release = Release::with_fixture();
    let bytes = release.fixture().wasm.clone();
    let published = release.fixture().manifest["digest"]
        .as_str()
        .expect("digest")
        .to_owned();

    // A copy with one byte changed, listed under the fixture's digest.
    let mut tampered = bytes.clone();
    let middle = tampered.len() / 2;
    tampered[middle] ^= 0x01;
    let entry = release.fixture_entry("p1/tampered");
    release.add(entry, &tampered);

    let loader = release.loader();
    let module = loader.load(FIXTURE_NAME).expect("load the fixture");
    // The identity is the digest of the bytes that were compiled, which is the digest the
    // build published and the manifest pins.
    assert_eq!(module.digest(), Digest::of(&bytes));
    assert_eq!(module.digest().to_string(), published);
    assert_eq!(
        module.identity(),
        &ToolIdentity {
            implementation: FIXTURE_NAME.to_owned(),
            variant: "default".to_owned(),
        }
    );

    match loader.load("p1/tampered") {
        Err(LoadError::DigestMismatch {
            name,
            expected,
            actual,
        }) => {
            assert_eq!(name, "p1/tampered");
            assert_eq!(expected.to_string(), published);
            assert_eq!(actual, Digest::of(&tampered));
        }
        Err(other) => panic!("expected a digest mismatch, got {other}"),
        Ok(_) => panic!("a tampered component loaded"),
    }

    // The compiled component is the fixture: the tool built from it declares the fixture.
    let tool = wasm_tool(
        &module,
        Services {
            process: Some(fake_processes().0),
            summary: None,
        },
        ExecutionLimits::default(),
        &Arc::new(MaskCounter::new()),
    )
    .expect("tool");
    assert_eq!(tool.declaration().name, "fixture");
    assert_eq!(
        tool.declaration().kind,
        DeclarationKind::Freeform { grammar: None }
    );
    assert_eq!(tool.identity().implementation, FIXTURE_NAME);
}

async fn load_refuses_what_the_release_does_not_ship() {
    let mut release = Release::with_fixture();
    let bytes = release.fixture().wasm.clone();

    // The fixture's own bytes under a name outside the reserved namespace.
    let entry = release.fixture_entry("thirdparty/fixture");
    release.add(entry, &bytes);
    // Granted less than it imports.
    let mut narrow = release.fixture_entry("p1/narrow");
    narrow["capabilities"] = serde_json_list(&["control", "clock"]);
    release.add(narrow, &bytes);
    // A world and a protocol major the runtime does not speak.
    let mut world = release.fixture_entry("p1/wrong-world");
    world["world"] = "p1:module/provider@1.0.0".into();
    release.add(world, &bytes);
    let mut protocol = release.fixture_entry("p1/next-protocol");
    protocol["protocol"] = "2.0".into();
    release.add(protocol, &bytes);
    let loader = release.loader();

    let refusal = |name: &str| match loader.load(name) {
        Err(error) => error,
        Ok(_) => panic!("{name} loaded"),
    };
    for name in ["thirdparty/fixture", "other/fixture", "fixture"] {
        let error = refusal(name);
        assert!(
            matches!(&error, LoadError::NotOfficial { name: refused } if refused == name),
            "{name}: {error}"
        );
        assert!(
            error.to_string().contains("reserved p1/ namespace"),
            "{error}"
        );
    }
    let error = refusal("p1/absent");
    assert!(
        matches!(&error, LoadError::NotInManifest { name } if name == "p1/absent"),
        "{error}"
    );
    assert!(error.to_string().contains("not in the release manifest"));
    let error = refusal("p1/narrow");
    assert!(
        matches!(&error, LoadError::UndeclaredImport { import, .. } if import == "p1:module/process@1.0.0"),
        "{error}"
    );
    let error = refusal("p1/wrong-world");
    assert!(matches!(error, LoadError::WorldMismatch { .. }), "{error}");
    let error = refusal("p1/next-protocol");
    assert!(
        matches!(error, LoadError::ProtocolMismatch { major: 1, .. }),
        "{error}"
    );
}

fn serde_json_list(items: &[&str]) -> p1_contracts::serde_json::Value {
    p1_contracts::serde_json::Value::Array(items.iter().map(|item| (*item).into()).collect())
}

async fn inspection_is_synchronous_inside_a_running_task() {
    let release = Release::with_fixture();
    let (tool, _processes) = fixture_tool(&release);

    // Inside a spawned task, on whichever worker runs it: effect and describe must answer
    // on the spot, never waiting on the executor or the runtime.
    let inspected = tokio::spawn(async move {
        let read_only = tool.effect(&call("echo:hi"));
        let executes = tool.effect(&call("stream:true"));
        let echo = tool.describe(&call("echo:hi"));
        let stream = tool.describe(&call("stream:true"));
        let import = tool.describe(&call("describe-import"));
        // The restricted instance recovers from that trap for the next call.
        let after = tool.describe(&call("echo:after"));
        let result = tool.describe_result(
            &call("echo:hi"),
            &ToolResultItem {
                call_id: "c1".to_owned(),
                name: "fixture".to_owned(),
                status: ToolStatus::Ok,
                content: "first line\nsecond".to_owned(),
            },
        );
        (read_only, executes, echo, stream, import, after, result)
    })
    .await
    .expect("the inspecting task");
    let (read_only, executes, echo, stream, import, after, result) = inspected;

    assert_eq!(read_only, Effect::ReadOnly);
    assert_eq!(executes, Effect::Executes);
    assert_eq!(echo.verb, "call");
    assert_eq!(echo.target.as_deref(), Some("echo:hi"));
    assert!(!echo.destructive);
    assert_eq!(stream.verb, "run");
    assert_eq!(stream.target.as_deref(), Some("stream:true"));
    assert!(stream.destructive);
    // `describe-import` calls an import; on the restricted path that traps, and the tool
    // answers the empty description instead of the module's.
    assert_eq!(import.verb, "call");
    assert_eq!(import.target, None);
    assert!(!import.destructive);
    assert_eq!(after.target.as_deref(), Some("echo:after"));
    assert_eq!(
        result,
        ResultDescription {
            summary: "first line".to_owned(),
            detail: None,
        }
    );
}

async fn execute_runs_each_call_on_a_fresh_instance() {
    let release = Release::with_fixture();
    let (tool, _processes) = fixture_tool(&release);

    let echo = tool.execute(&call("echo:hi"), context()).await;
    assert_eq!(echo.status, ToolStatus::Ok, "{}", echo.content);
    assert_eq!(echo.content, "hi");

    let clock = tool.execute(&call("clock"), context()).await;
    assert_eq!(clock.status, ToolStatus::Ok, "{}", clock.content);
    assert_eq!(clock.content, "monotonic ok");

    let trap = tool.execute(&call("trap"), context()).await;
    assert_eq!(trap.status, ToolStatus::Error);
    assert!(trap.content.contains("trapped"), "{}", trap.content);
    assert!(trap.content.contains("unreachable"), "{}", trap.content);

    // The trap poisoned nothing: the next call runs on a fresh instance.
    let after = tool.execute(&call("echo:after the trap"), context()).await;
    assert_eq!(after.status, ToolStatus::Ok, "{}", after.content);
    assert_eq!(after.content, "after the trap");

    // A call cancelled before it starts is answered as cancelled.
    let cancel = CancellationToken::new();
    cancel.cancel();
    let cancelled = tool
        .execute(&call("echo:never"), ToolContext { cancel })
        .await;
    assert_eq!(cancelled.status, ToolStatus::Cancelled);
}

async fn execute_awaits_an_async_host_import() {
    let release = Release::with_fixture();
    let (tool, mut processes) = fixture_tool(&release);

    let running = {
        let tool = tool.clone();
        tokio::spawn(async move { tool.execute(&call("stream:make it"), context()).await })
    };
    // The module is now blocked in `process.next`, whose host future stays pending until
    // the test sends an event.
    let spawned = processes.next_spawn().await;
    assert_eq!(spawned.command.script, "make it");
    spawned
        .events
        .send(ProcessEvent::Output(b"built".to_vec()))
        .expect("send output");
    spawned
        .events
        .send(ProcessEvent::Exited(ExitStatus::Code(0)))
        .expect("send exit");

    let outcome = running.await.expect("the executing task");
    assert_eq!(outcome.status, ToolStatus::Ok, "{}", outcome.content);
    assert_eq!(outcome.content, "built\nexit: code(0)");
    // The call's Store is gone, and with it the process handle.
    spawned
        .dropped
        .await
        .expect("the process handle was dropped");
}

async fn concurrent_calls_all_complete() {
    let release = Release::with_fixture();
    let (tool, _processes) = fixture_tool(&release);

    let calls: Vec<_> = (0..8)
        .map(|index| {
            let tool = tool.clone();
            tokio::spawn(async move {
                let outcome = tool
                    .execute(&call(&format!("echo:call {index}")), context())
                    .await;
                (index, outcome)
            })
        })
        .collect();
    for task in calls {
        let (index, outcome) = task.await.expect("a calling task");
        assert_eq!(outcome.status, ToolStatus::Ok, "{}", outcome.content);
        assert_eq!(outcome.content, format!("call {index}"));
    }
}

async fn the_host_gets_the_redacting_wrapper() {
    let release = Release::with_fixture();
    let module = release.loader().load(FIXTURE_NAME).expect("load");
    let counter = Arc::new(MaskCounter::new());
    let tool = wasm_tool(
        &module,
        Services {
            process: Some(fake_processes().0),
            summary: None,
        },
        ExecutionLimits::default(),
        &counter,
    )
    .expect("tool");

    // A key-shaped value built at runtime, the shape p1-redact's own tests use.
    let secret = format!("sk-{}", "A".repeat(24));
    let outcome = tool
        .execute(&call(&format!("echo:{secret}")), context())
        .await;
    assert_eq!(outcome.status, ToolStatus::Ok, "{}", outcome.content);
    assert!(!outcome.content.contains(&secret));
    assert_eq!(outcome.content, "<redacted:sk-:24 chars>");
    assert_eq!(counter.take(), 1);
}
