//! Seam tests for the host-stage gaps closed in `face.rs`/`transcript.rs`
//! (TUI-HANDOFF §6.9, §7.3, §7.7, §14): `ToolDescriber::result` receives the
//! call's elapsed; a `ResultFace` may override the frozen call target once
//! the result settles; an overflowing `Diff`/`Files` body registers its full
//! text under the fold handle `^O` opens.

use std::sync::{Arc, Mutex};

use p1_contracts::{AgentEvent, ToolCall, ToolInput, ToolResultItem, ToolStatus};
use p1_tui::face::{CallFace, FaceBody, ResultFace, TargetKind, ToolDescriber};
use p1_tui::render::diff::DiffRow;
use p1_tui::transcript::{Block, Transcript};

fn call(id: &str, name: &str) -> ToolCall {
    ToolCall {
        call_id: id.into(),
        name: name.into(),
        input: ToolInput::Json("{}".into()),
    }
}

fn result(id: &str, name: &str, status: ToolStatus, content: &str) -> ToolResultItem {
    ToolResultItem {
        call_id: id.into(),
        name: name.into(),
        status,
        content: content.into(),
    }
}

fn call_row(transcript: &Transcript, index: usize) -> &p1_tui::transcript::ToolRow {
    match &transcript.blocks[index] {
        Block::Call(row) => row,
        other => panic!("expected a call row at {index}, got {other:?}"),
    }
}

// ------------------------------------------------------------- elapsed reaches the face

/// A describer that records exactly what `settle()` gave it, so the test can
/// assert on the seam itself — not on any one host tool's formatting.
#[derive(Default)]
struct RecordingDescriber {
    elapsed_seen: Mutex<Vec<Option<u64>>>,
    /// The `ResultFace` to hand back, keyed by call name (one scripted face
    /// covers one test; `None` uses the default empty face).
    script: Mutex<Option<ResultFace>>,
}

impl ToolDescriber for RecordingDescriber {
    fn call(&self, call: &ToolCall) -> CallFace {
        CallFace {
            target: call.name.clone(),
            kind: TargetKind::Plain,
        }
    }

    fn result(
        &self,
        _call: &ToolCall,
        _result: &ToolResultItem,
        elapsed_ms: Option<u64>,
    ) -> ResultFace {
        self.elapsed_seen.lock().unwrap().push(elapsed_ms);
        self.script.lock().unwrap().clone().unwrap_or(ResultFace {
            outcome: None,
            body: FaceBody::None,
            meta: None,
            target: None,
        })
    }
}

#[test]
fn the_calls_own_elapsed_reaches_the_describer() {
    let describer = Arc::new(RecordingDescriber::default());
    let mut transcript = Transcript::with_describer(describer.clone());
    transcript.apply(
        &AgentEvent::ToolStarted {
            call: call("c1", "shell"),
        },
        Some(1_000),
    );
    transcript.apply(
        &AgentEvent::ToolFinished {
            result: result("c1", "shell", ToolStatus::Ok, "hi\n[exit code: 0]"),
        },
        Some(1_412),
    );
    // The call started at 1_000ms and settled at 1_412ms: 412ms elapsed, the
    // SAME clock `ToolRow.elapsed_ms` carries — the describer never keeps its
    // own.
    assert_eq!(
        describer.elapsed_seen.lock().unwrap().as_slice(),
        [Some(412)]
    );
    assert_eq!(call_row(&transcript, 0).elapsed_ms, Some(412));
}

#[test]
fn elapsed_is_unknown_without_a_started_stamp() {
    let describer = Arc::new(RecordingDescriber::default());
    let mut transcript = Transcript::with_describer(describer.clone());
    // No `ToolStarted` at all: an orphan result (SPEC §4.9) still renders as
    // a row, but with no captured `ToolCall` the describer has nothing to
    // describe FROM — it is never called (unchanged, pre-existing rule).
    transcript.apply(
        &AgentEvent::ToolFinished {
            result: result("c9", "shell", ToolStatus::Ok, "hi\n[exit code: 0]"),
        },
        Some(500),
    );
    assert_eq!(
        describer.elapsed_seen.lock().unwrap().as_slice(),
        Vec::<Option<u64>>::new()
    );
    assert!(matches!(transcript.blocks[0], Block::Call(_)));
}

// -------------------------------------------------------------- the target override

#[test]
fn a_result_time_target_replaces_the_frozen_call_target() {
    let describer = Arc::new(RecordingDescriber::default());
    *describer.script.lock().unwrap() = Some(ResultFace {
        outcome: Some("started · read, finish".into()),
        body: FaceBody::None,
        meta: None,
        target: Some("w1 · claude/sonnet".into()),
    });
    let mut transcript = Transcript::with_describer(describer);
    transcript.apply(
        &AgentEvent::ToolStarted {
            call: call("c1", "worker_start"),
        },
        Some(0),
    );
    // The frozen call-time target, before the result settles.
    assert_eq!(call_row(&transcript, 0).face.target, "worker_start");
    transcript.apply(
        &AgentEvent::ToolFinished {
            result: result(
                "c1",
                "worker_start",
                ToolStatus::Ok,
                "Started worker w1 ...",
            ),
        },
        Some(20),
    );
    // §7.3: `w1 · env/profile`, only known once the result settles.
    assert_eq!(call_row(&transcript, 0).face.target, "w1 · claude/sonnet");
}

#[test]
fn no_target_override_leaves_the_call_time_target_alone() {
    let describer = Arc::new(RecordingDescriber::default());
    let mut transcript = Transcript::with_describer(describer);
    transcript.apply(
        &AgentEvent::ToolStarted {
            call: call("c1", "read"),
        },
        Some(0),
    );
    transcript.apply(
        &AgentEvent::ToolFinished {
            result: result("c1", "read", ToolStatus::Ok, "one\ntwo"),
        },
        Some(10),
    );
    assert_eq!(call_row(&transcript, 0).face.target, "read");
}

// ---------------------------------------------------- Diff/Files overflow registers a fold

fn diff_rows(n: usize) -> Vec<DiffRow> {
    (0..n)
        .map(|i| DiffRow::Add {
            line: i as u32 + 1,
            text: format!("line {i}"),
        })
        .collect()
}

#[test]
fn a_cut_diff_body_registers_its_full_text_and_o_opens_it() {
    let describer = Arc::new(RecordingDescriber::default());
    let rows = diff_rows(14); // over §7.3's 12-row inline cap
    *describer.script.lock().unwrap() = Some(ResultFace {
        outcome: Some("+14 −0".into()),
        body: FaceBody::Diff(rows.clone()),
        meta: None,
        target: None,
    });
    let mut transcript = Transcript::with_describer(describer);
    transcript.apply(
        &AgentEvent::ToolStarted {
            call: call("c1", "edit"),
        },
        Some(0),
    );
    // The tool's own one-line success text — nothing like the true row count,
    // proving the cap reads the body's row count, not this.
    transcript.apply(
        &AgentEvent::ToolFinished {
            result: result("c1", "edit", ToolStatus::Ok, "Edited f.rs (1 replacement)."),
        },
        Some(5),
    );
    let row = call_row(&transcript, 0);
    assert_eq!(
        row.line_count, 14,
        "the row count drives the cap, not the raw content"
    );
    let id = row
        .fold
        .clone()
        .expect("an overflowing diff registers a fold");
    assert_eq!(
        transcript.latest_fold.as_ref(),
        Some(&id),
        "^O opens the most recent handle"
    );
    let opened = transcript
        .output(&id)
        .expect("^O opens the handle the row names");
    for text in ["line 0", "line 13"] {
        assert!(
            opened.contains(text),
            "the full diff is behind the handle: {opened}"
        );
    }
}

#[test]
fn a_diff_body_within_the_cap_registers_no_fold() {
    let describer = Arc::new(RecordingDescriber::default());
    *describer.script.lock().unwrap() = Some(ResultFace {
        outcome: Some("+3 −0".into()),
        body: FaceBody::Diff(diff_rows(3)),
        meta: None,
        target: None,
    });
    let mut transcript = Transcript::with_describer(describer);
    transcript.apply(
        &AgentEvent::ToolStarted {
            call: call("c1", "edit"),
        },
        Some(0),
    );
    transcript.apply(
        &AgentEvent::ToolFinished {
            result: result("c1", "edit", ToolStatus::Ok, "Edited f.rs (1 replacement)."),
        },
        Some(5),
    );
    // Within the cap, `render/block.rs` shows every row regardless of
    // `line_count`'s exact value (it only gates the cut itself): no fold is
    // ever registered for it.
    let row = call_row(&transcript, 0);
    assert_eq!(row.fold, None);
    assert_eq!(transcript.latest_fold, None);
}

#[test]
fn a_cut_files_body_registers_its_full_text_and_o_opens_it() {
    let describer = Arc::new(RecordingDescriber::default());
    let files: Vec<(String, String)> = (0..14)
        .map(|i| (format!("file{i}.rs"), "+1 −0".to_string()))
        .collect();
    *describer.script.lock().unwrap() = Some(ResultFace {
        outcome: Some("+14 · 14 files".into()),
        body: FaceBody::Files(files),
        meta: None,
        target: None,
    });
    let mut transcript = Transcript::with_describer(describer);
    transcript.apply(
        &AgentEvent::ToolStarted {
            call: call("c1", "apply_patch"),
        },
        Some(0),
    );
    transcript.apply(
        &AgentEvent::ToolFinished {
            result: result("c1", "apply_patch", ToolStatus::Ok, "M file0.rs\n..."),
        },
        Some(5),
    );
    let row = call_row(&transcript, 0);
    assert_eq!(row.line_count, 14);
    let id = row
        .fold
        .clone()
        .expect("an overflowing file list registers a fold");
    let opened = transcript
        .output(&id)
        .expect("^O opens the handle the row names");
    assert!(opened.contains("file0.rs"));
    assert!(opened.contains("file13.rs"));
}
