//! The sandbox (ADR-0053 item 3): nothing outside the workflow surface is reachable, and
//! every engine limit holds. The escape vectors are the spike's.

mod support;

use p1_workflow::{RunOutcome, WorkflowError, WorkflowService};
use support::{Harness, request};

// (13)
#[tokio::test(flavor = "multi_thread")]
async fn every_escape_vector_fails_on_its_own() {
    let harness = Harness::new();
    let vectors: &[(&str, &str)] = &[
        ("file read", r#"read_file("/etc/passwd")"#),
        ("file write", r#"write_file("/tmp/rhai-escape.txt", "x")"#),
        ("network", r#"http_get("http://example.com/")"#),
        ("socket", r#"connect("127.0.0.1", 80)"#),
        ("env", r#"env("PATH")"#),
        ("process", r#"exec("id")"#),
        ("time", "timestamp()"),
        ("now", "now()"),
        ("random", "rand(1, 6)"),
        ("random 2", "random()"),
        (
            "module by path",
            r#"import "/etc/passwd" as stolen; stolen"#,
        ),
        ("module by name", r#"import "std" as stdlib; stdlib"#),
        ("sleep", "sleep(30)"),
        ("sleep float", "sleep(0.5)"),
        (
            "function pointer to a missing name",
            r#"Fn("read_file").call("/etc/passwd")"#,
        ),
    ];
    for (name, source) in vectors {
        let report = harness.run(source).await;
        assert_eq!(report.outcome, RunOutcome::Failed, "{name} must not work");
        let error = report.error.unwrap();
        assert!(
            error.contains("not found") || error.contains("not available"),
            "{name}: unexpected error {error}"
        );
        assert!(error.contains("[line 1, column"), "{name}: {error}");
    }
    assert!(harness.runner.requests().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn eval_is_a_parse_error() {
    let harness = Harness::new();
    match harness
        .service
        .start(request("let x = 1;\neval(\"x + 1\")"))
        .await
    {
        Err(WorkflowError::Parse {
            message,
            line,
            column,
        }) => {
            assert!(message.contains("eval"), "{message}");
            assert_eq!((line, column), (2, 1));
        }
        other => panic!("expected a parse error, got {other:?}"),
    }
}

// (14)
#[tokio::test(flavor = "multi_thread")]
async fn every_engine_limit_holds() {
    let harness = Harness::new();
    let cases: &[(&str, &str)] = &[
        ("operations", "let x = 0; loop { x += 1; }"),
        ("recursion", "fn down(n) { down(n + 1) } down(0)"),
        ("string", r#"let s = "x"; loop { s += s; }"#),
        (
            "array",
            "let a = []; for i in 0..5000 { a.push(i); } a.len()",
        ),
        (
            "map",
            "let m = #{}; for i in 0..5000 { m[`k${i}`] = i; } m.len()",
        ),
    ];
    let mut errors = Vec::new();
    for (name, source) in cases {
        let report = harness.run(source).await;
        assert_eq!(report.outcome, RunOutcome::Failed, "{name}: {report:?}");
        errors.push((name, report.error.unwrap()));
    }
    let expect = |name: &str, needle: &str| {
        let (_, error) = errors.iter().find(|(n, _)| **n == name).unwrap();
        assert!(
            error.to_lowercase().contains(needle),
            "{name}: expected {needle:?} in {error}"
        );
    };
    expect("operations", "too many operations");
    expect("recursion", "stack overflow");
    expect("string", "string");
    expect("array", "array");
    expect("map", "map");

    let deep = format!("{}1{}", "(".repeat(40), ")".repeat(40));
    assert!(matches!(
        harness.service.start(request(&deep)).await,
        Err(WorkflowError::Parse { .. })
    ));
    let shallow = format!("{}1{}", "(".repeat(10), ")".repeat(10));
    assert_eq!(harness.run(&shallow).await.value, 1);
}
