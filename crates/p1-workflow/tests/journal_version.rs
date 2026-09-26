//! The run journal's own format version (S6.9): every run writes it in its leading
//! `started` record, and a resume refuses a journal of a version this build does not read —
//! it never skips the record and replays what it cannot understand. A journal written
//! before the version existed resumes as version 1.

mod support;

use p1_workflow::{JournalRecord, RunId, RunOutcome, RunReport, StartRequest};
use serde_json::json;
use support::{Harness, request};

const TWO_STEPS: &str = r#"
    let a = agent("first", #{ label: "a" });
    let b = agent("second", #{ label: "b" });
    [a.value, b.value]
"#;

fn start(resume_from: Option<&str>) -> StartRequest {
    StartRequest {
        args: json!({}),
        resume_from: resume_from.map(|id| RunId(id.to_string())),
        ..request(TWO_STEPS)
    }
}

async fn run(harness: &Harness, start: StartRequest) -> RunReport {
    let id = harness.start_request(start).await;
    harness.wait(&id).await
}

/// The journal of the first run, and its text.
async fn first_run(harness: &Harness) -> (RunReport, std::path::PathBuf, String) {
    let first = run(harness, start(None)).await;
    assert_eq!(first.outcome, RunOutcome::Completed, "{first:?}");
    let path = first.run_dir.join("journal.jsonl");
    let text = std::fs::read_to_string(&path).unwrap();
    (first, path, text)
}

#[tokio::test(flavor = "multi_thread")]
async fn every_run_writes_the_version_in_its_leading_record() {
    let harness = Harness::new();
    let (first, _, text) = first_run(&harness).await;
    let leading = text.lines().next().unwrap();
    assert!(
        leading.starts_with(r#"{"kind":"started","p1_workflow_journal":1,"#),
        "{leading}"
    );
    let id = RunId(first.run_dir.file_name().unwrap().to_string_lossy().into());
    assert!(matches!(
        &harness.journal(&id)[0],
        JournalRecord::Started {
            journal_version: p1_workflow::WORKFLOW_JOURNAL_VERSION,
            ..
        }
    ));
}

#[tokio::test(flavor = "multi_thread")]
async fn resume_refuses_a_journal_of_an_unknown_version() {
    let harness = Harness::new();
    let (first, path, text) = first_run(&harness).await;
    let id = first
        .run_dir
        .file_name()
        .unwrap()
        .to_string_lossy()
        .into_owned();
    std::fs::write(
        &path,
        text.replacen(
            r#""p1_workflow_journal":1"#,
            r#""p1_workflow_journal":2"#,
            1,
        ),
    )
    .unwrap();
    let error = p1_workflow::WorkflowService::start(&*harness.service, start(Some(&id)))
        .await
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("workflow journal version 2 is not one this build reads"),
        "{error}"
    );
    assert_eq!(
        harness.runner.requests().len(),
        2,
        "nothing ran for the refused resume"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_journal_written_before_the_version_resumes_as_version_1() {
    let harness = Harness::new();
    let (first, path, text) = first_run(&harness).await;
    let id = first
        .run_dir
        .file_name()
        .unwrap()
        .to_string_lossy()
        .into_owned();
    // The journal as a build before the version wrote it: the same records, the leading
    // one without its version field.
    let unversioned = text.replacen(r#""p1_workflow_journal":1,"#, "", 1);
    assert_ne!(unversioned, text);
    std::fs::write(&path, unversioned).unwrap();

    let second = run(&harness, start(Some(&id))).await;
    assert_eq!(second.outcome, RunOutcome::Completed, "{second:?}");
    assert_eq!(second.value, first.value);
    assert_eq!(second.counts.replayed, 2);
    assert_eq!(
        harness.runner.requests().len(),
        2,
        "every step replayed from the old journal"
    );
}
