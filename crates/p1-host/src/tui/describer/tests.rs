//! Per-tool describer tests (handoff §7.3): every input JSON and every success
//! text below is copied from the matching `p1-tool-*` crate's own source
//! (schema shapes and `Ok(format!(...))`/error literals), not invented.

use super::*;
use p1_contracts::{ToolCall, ToolInput, ToolResultItem, ToolStatus};
use p1_tui::face::{FaceBody, TargetKind, ToolDescriber};

fn describer() -> HostDescriber {
    HostDescriber::new(PathBuf::from("/workspace"), "off".into())
}

fn call(name: &str, json: &str) -> ToolCall {
    ToolCall {
        call_id: "c1".into(),
        name: name.into(),
        input: ToolInput::Json(json.into()),
    }
}

fn text_call(name: &str, text: &str) -> ToolCall {
    ToolCall {
        call_id: "c1".into(),
        name: name.into(),
        input: ToolInput::Text(text.into()),
    }
}

fn result(name: &str, status: ToolStatus, content: &str) -> ToolResultItem {
    ToolResultItem {
        call_id: "c1".into(),
        name: name.into(),
        status,
        content: content.into(),
    }
}

// ------------------------------------------------------------------- read

#[test]
fn read_call_shows_the_path_and_the_range_only_when_offset_or_limit_was_given() {
    let d = describer();
    let plain = call("read", r#"{"file_path":"src/lib.rs"}"#);
    assert_eq!(
        d.call(&plain),
        CallFace {
            target: "src/lib.rs".into(),
            kind: TargetKind::Path
        }
    );
    let ranged = call(
        "read",
        r#"{"file_path":"src/lib.rs","offset":5,"limit":10}"#,
    );
    assert_eq!(d.call(&ranged).target, "src/lib.rs:5-14");
}

#[test]
fn read_ok_reports_lines_and_size_error_falls_back_to_the_message() {
    let d = describer();
    let ok = result("read", ToolStatus::Ok, "     1\tfn main() {}\n     2\t}");
    let face = d.result(&call("read", "{}"), &ok, None);
    assert_eq!(face.outcome.as_deref(), Some("2 lines · 0.0 kB"));
    assert_eq!(face.body, FaceBody::None);

    let error = result("read", ToolStatus::Error, "src/lib.rs does not exist.");
    let face = d.result(&call("read", "{}"), &error, None);
    assert_eq!(face.outcome.as_deref(), Some("src/lib.rs does not exist."));
}

// ------------------------------------------------------------------ write

#[test]
fn write_call_is_a_path_target() {
    let face = describer().call(&call("write", r#"{"file_path":"out.txt","content":"hi"}"#));
    assert_eq!(face.target, "out.txt");
    assert_eq!(face.kind, TargetKind::Path);
}

#[test]
fn write_ok_reports_lines_and_bytes_error_first_line_is_bounded_at_40_cells() {
    let d = describer();
    let call = call(
        "write",
        r#"{"file_path":"nested/dir/file.txt","content":"line1\nline2\n"}"#,
    );
    let ok = result(
        "write",
        ToolStatus::Ok,
        "Wrote nested/dir/file.txt (12 bytes).",
    );
    let face = d.result(&call, &ok, None);
    assert_eq!(face.outcome.as_deref(), Some("2 lines · 0.0 kB"));

    // The real read-before-mutate refusal (`p1-tool-write`'s own text), 41
    // cells: bounded to 40 with `…` (§7.1's generic Error rule).
    let error = result(
        "write",
        ToolStatus::Error,
        "You must read out.txt before changing it.",
    );
    let face = d.result(&call, &error, None);
    assert_eq!(
        face.outcome.as_deref(),
        Some("You must read out.txt before changing i…")
    );
}

// ------------------------------------------------------------------- edit

#[test]
fn edit_call_is_a_path_target() {
    let face = describer().call(&call(
        "edit",
        r#"{"file_path":"src/lib.rs","old_string":"a","new_string":"b"}"#,
    ));
    assert_eq!(face.target, "src/lib.rs");
    assert_eq!(face.kind, TargetKind::Path);
}

#[test]
fn edit_ok_counts_added_and_removed_lines_and_builds_the_hunk() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("f.rs"), "one\ntwo\nTHREE\nfour\n").unwrap();
    let d = HostDescriber::new(dir.path().to_path_buf(), "off".into());
    let call = call(
        "edit",
        r#"{"file_path":"f.rs","old_string":"old","new_string":"THREE"}"#,
    );
    let ok = result("edit", ToolStatus::Ok, "Edited f.rs (1 replacement).");
    let face = d.result(&call, &ok, None);
    assert_eq!(face.outcome.as_deref(), Some("+1 −1"));
    let FaceBody::Diff(rows) = face.body else {
        panic!("expected a diff body, got {:?}", face.body);
    };
    // One context line before (`two`) and after (`four`) the anchored change.
    assert_eq!(
        rows,
        vec![
            DiffRow::Context {
                line: 2,
                text: "two".into()
            },
            DiffRow::Del {
                line: 3,
                text: "old".into()
            },
            DiffRow::Add {
                line: 3,
                text: "THREE".into()
            },
            DiffRow::Context {
                line: 4,
                text: "four".into()
            },
        ]
    );
}

#[test]
fn edit_overflow_registers_the_full_diff_under_the_fold_it_names() {
    use p1_contracts::AgentEvent;
    use p1_tui::transcript::Transcript;

    let dir = tempfile::tempdir().unwrap();
    let old_lines: Vec<String> = (0..7).map(|n| format!("old{n}")).collect();
    let new_lines: Vec<String> = (0..7).map(|n| format!("new{n}")).collect();
    std::fs::write(
        dir.path().join("f.rs"),
        format!("{}\n", old_lines.join("\n")),
    )
    .unwrap();
    let describer = std::sync::Arc::new(HostDescriber::new(dir.path().to_path_buf(), "off".into()));
    let call = ToolCall {
        call_id: "c1".into(),
        name: "edit".into(),
        input: ToolInput::Json(
            serde_json::json!({
                "file_path": "f.rs",
                "old_string": old_lines.join("\n"),
                "new_string": new_lines.join("\n"),
            })
            .to_string(),
        ),
    };
    let result = ToolResultItem {
        call_id: "c1".into(),
        name: "edit".into(),
        status: ToolStatus::Ok,
        content: "Edited f.rs (1 replacement).".into(),
    };
    // 7 del + 7 add rows: over §7.3's 12-row inline cap.
    let face = describer.result(&call, &result, None);
    let FaceBody::Diff(rows) = &face.body else {
        panic!("expected a diff body, got {:?}", face.body);
    };
    assert_eq!(
        rows.len(),
        14,
        "the describer returns the full hunk, uncapped"
    );

    let mut transcript = Transcript::with_describer(describer);
    transcript.apply(&AgentEvent::ToolStarted { call }, Some(0));
    transcript.apply(&AgentEvent::ToolFinished { result }, Some(50));
    let p1_tui::transcript::Block::Call(row) = &transcript.blocks[0] else {
        panic!("expected a call row");
    };
    assert_eq!(
        row.line_count, 14,
        "the row/file count drives the cap, not the raw content"
    );
    let id = row
        .fold
        .clone()
        .expect("an overflowing diff registers a fold");
    let opened = transcript
        .output(&id)
        .expect("^O opens the handle the row names");
    assert!(
        opened.contains("new6"),
        "the full diff is behind the handle: {opened}"
    );
    assert!(
        opened.contains("old6"),
        "the full diff is behind the handle: {opened}"
    );
}

#[test]
fn edit_error_has_no_diff_body() {
    let d = describer();
    let call = call(
        "edit",
        r#"{"file_path":"src/lib.rs","old_string":"a","new_string":"b"}"#,
    );
    let error = result(
        "edit",
        ToolStatus::Error,
        "old_string was not found in src/lib.rs.",
    );
    let face = d.result(&call, &error, None);
    assert_eq!(
        face.outcome.as_deref(),
        Some("old_string was not found in src/lib.rs.")
    );
    assert_eq!(face.body, FaceBody::None);
}

// ------------------------------------------------------------ apply_patch

const TWO_FILE_PATCH: &str = "*** Begin Patch\n*** Add File: new.txt\n+line one\n+line two\n*** Delete File: old.txt\n*** End Patch\n";
const ONE_FILE_PATCH: &str = "*** Begin Patch\n*** Update File: src/lib.rs\n@@ fn main\n-old line\n+new line\n*** End Patch\n";

#[test]
fn apply_patch_call_names_the_one_file_or_the_count() {
    let d = describer();
    let one = d.call(&text_call("apply_patch", ONE_FILE_PATCH));
    assert_eq!(one.target, "src/lib.rs");
    assert_eq!(one.kind, TargetKind::Path);

    let two = d.call(&text_call("apply_patch", TWO_FILE_PATCH));
    assert_eq!(two.target, "2 files");
    assert_eq!(two.kind, TargetKind::Plain);

    // The function-face shape, `{"patch": "..."}`.
    let json = call(
        "apply_patch",
        &serde_json::json!({ "patch": ONE_FILE_PATCH }).to_string(),
    );
    assert_eq!(d.call(&json).target, "src/lib.rs");
}

#[test]
fn apply_patch_ok_counts_added_removed_per_file_delete_omits_removed() {
    let d = describer();
    let call = text_call("apply_patch", TWO_FILE_PATCH);
    // The real `Op::success_line` shape: one `A`/`M`/`D` line per file.
    let ok = result("apply_patch", ToolStatus::Ok, "A new.txt\nD old.txt");
    let face = d.result(&call, &ok, None);
    // The delete's removed count is unknown, so the TOTAL omits `−R` rather
    // than silently undercounting it.
    assert_eq!(face.outcome.as_deref(), Some("+2 · 2 files"));
    let FaceBody::Files(rows) = face.body else {
        panic!("expected a Files body, got {:?}", face.body);
    };
    assert_eq!(
        rows,
        vec![
            ("new.txt".to_string(), "+2 −0".to_string()),
            ("old.txt".to_string(), "D".to_string()),
        ]
    );
}

#[test]
fn apply_patch_single_update_file_reports_a_known_total() {
    let d = describer();
    let call = text_call("apply_patch", ONE_FILE_PATCH);
    let ok = result("apply_patch", ToolStatus::Ok, "M src/lib.rs");
    let face = d.result(&call, &ok, None);
    assert_eq!(face.outcome.as_deref(), Some("+1 −1 · 1 files"));
}

#[test]
fn apply_patch_rejection_is_the_bounded_first_line() {
    let d = describer();
    let call = text_call("apply_patch", ONE_FILE_PATCH);
    let error = result(
        "apply_patch",
        ToolStatus::Error,
        "Invalid patch: hunk did not match (line 3).",
    );
    let face = d.result(&call, &error, None);
    // 43 cells: bounded to 40 with `…` (§7.1's generic Error rule).
    assert_eq!(
        face.outcome.as_deref(),
        Some("Invalid patch: hunk did not match (line…")
    );
}

// --------------------------------------------------------------------- grep

#[test]
fn grep_call_joins_the_pattern_and_the_scope() {
    let d = describer();
    assert_eq!(
        d.call(&call("grep", r#"{"pattern":"beta","path":"src"}"#))
            .target,
        "beta src"
    );
    assert_eq!(
        d.call(&call("grep", r#"{"pattern":"beta"}"#)).target,
        "beta ."
    );
}

#[test]
fn grep_ok_counts_hits_and_files_from_the_rendered_blocks() {
    let d = describer();
    let call = call("grep", r#"{"pattern":"beta"}"#);
    let content = "a.rs\n1:beta line\n\nb.rs\n2:beta again\n3-context";
    let face = d.result(&call, &result("grep", ToolStatus::Ok, content), None);
    assert_eq!(face.outcome.as_deref(), Some("3 hits · 2 files"));

    let none = d.result(&call, &result("grep", ToolStatus::Ok, "No matches."), None);
    assert_eq!(none.outcome.as_deref(), Some("0 hits · 0 files"));
}

#[test]
fn grep_files_mode_omits_the_unknown_hit_count() {
    let d = describer();
    let call = call("grep", r#"{"pattern":"beta","mode":"files"}"#);
    let face = d.result(&call, &result("grep", ToolStatus::Ok, "a.rs\nb.rs"), None);
    assert_eq!(face.outcome.as_deref(), Some("2 files"));
}

#[test]
fn grep_error_is_the_bounded_first_line() {
    let d = describer();
    let call = call("grep", r#"{"pattern":"("}"#);
    let error = result(
        "grep",
        ToolStatus::Error,
        "invalid regex pattern: parse error",
    );
    let face = d.result(&call, &error, None);
    assert_eq!(
        face.outcome.as_deref(),
        Some("invalid regex pattern: parse error")
    );
}

// -------------------------------------------------------------------- shell

#[test]
fn shell_call_is_a_command_target() {
    let face = describer().call(&call("shell", r#"{"command":"cargo test"}"#));
    assert_eq!(face.target, "cargo test");
    assert_eq!(face.kind, TargetKind::Command);
}

#[test]
fn shell_ok_reports_exit_code_and_body_lines() {
    let d = describer();
    let call = call("shell", r#"{"command":"echo hi"}"#);
    // The real `render()` shape: body, then the `[exit code: N]` footer.
    let ok = result("shell", ToolStatus::Ok, "hi\n[exit code: 0]");
    // Without a stamp `D` is omitted, never guessed.
    let face = d.result(&call, &ok, None);
    assert_eq!(face.outcome.as_deref(), Some("exit 0 · 1 lines"));
    assert_eq!(face.meta, None);
}

#[test]
fn shell_ok_leads_with_elapsed_when_the_call_carried_a_stamp() {
    let d = describer();
    let call = call("shell", r#"{"command":"echo hi"}"#);
    let ok = result("shell", ToolStatus::Ok, "hi\n[exit code: 0]");
    let face = d.result(&call, &ok, Some(412));
    assert_eq!(face.outcome.as_deref(), Some("412ms · exit 0 · 1 lines"));
    let face = d.result(&call, &ok, Some(11_400));
    assert_eq!(face.outcome.as_deref(), Some("11.4s · exit 0 · 1 lines"));
}

#[test]
fn shell_sandboxed_meta_names_the_cwd_and_the_sandbox() {
    let d = HostDescriber::new(PathBuf::from("/work"), "bubblewrap".into());
    let call = call("shell", r#"{"command":"echo hi"}"#);
    let ok = result("shell", ToolStatus::Ok, "hi\n[exit code: 0]");
    let face = d.result(&call, &ok, None);
    assert_eq!(face.meta.as_deref(), Some("cwd /work · bubblewrap"));
}

#[test]
fn shell_timeout_is_an_error_with_the_timeout_footer_as_the_outcome() {
    let d = describer();
    let call = call("shell", r#"{"command":"sleep 30","timeout_seconds":1}"#);
    let error = result("shell", ToolStatus::Error, "[timed out after 1 s]");
    let face = d.result(&call, &error, None);
    assert_eq!(face.outcome.as_deref(), Some("[timed out after 1 s]"));
}

// ------------------------------------------------------------------- finish

#[test]
fn finish_call_names_the_status() {
    let face = describer().call(&call(
        "finish",
        r#"{"status":"done","summary":"did it","verification":["cargo test"]}"#,
    ));
    assert_eq!(face.target, "done");
    assert_eq!(face.kind, TargetKind::Plain);
}

#[test]
fn finish_done_lists_the_verification_commands() {
    let d = describer();
    let call = call(
        "finish",
        r#"{"status":"done","summary":"did it","verification":["cargo test"]}"#,
    );
    let ok = result("finish", ToolStatus::Ok, "Finished.");
    let face = d.result(&call, &ok, None);
    assert_eq!(face.outcome.as_deref(), Some("verified · cargo test"));
    assert_eq!(face.body, FaceBody::Lines(vec!["✓ cargo test".into()]));
}

#[test]
fn finish_blocked_shows_needs_and_the_summary_as_body() {
    let d = describer();
    let call = call(
        "finish",
        r#"{"status":"blocked","summary":"cannot write","needs":"edit tool"}"#,
    );
    let ok = result("finish", ToolStatus::Ok, "Recorded as blocked.");
    let face = d.result(&call, &ok, None);
    assert_eq!(face.outcome.as_deref(), Some("blocked · needs edit tool"));
    assert_eq!(face.body, FaceBody::Lines(vec!["cannot write".into()]));
}

#[test]
fn finish_rejection_is_prefixed_and_bounded() {
    let d = describer();
    let call = call(
        "finish",
        r#"{"status":"blocked","summary":"x","needs":"y"}"#,
    );
    let error = result(
        "finish",
        ToolStatus::Error,
        "Say what you need in \"needs\".",
    );
    let face = d.result(&call, &error, None);
    assert_eq!(
        face.outcome.as_deref(),
        Some("rejected · Say what you need in \"needs\".")
    );
}

// ------------------------------------------------------------- worker_start

#[test]
fn worker_start_call_names_the_requested_environment() {
    let face = describer().call(&call(
        "worker_start",
        r#"{"environment":"claude","task":"do X","tools":["read","edit"]}"#,
    ));
    assert_eq!(face.target, "claude");
    assert_eq!(face.kind, TargetKind::Plain);
}

#[test]
fn worker_start_ok_lists_the_task_and_the_grants() {
    let d = describer();
    let call = call(
        "worker_start",
        r#"{"environment":"claude","task":"do X\nmore detail","tools":["read","edit"]}"#,
    );
    let ok = result(
        "worker_start",
        ToolStatus::Ok,
        "Started worker w1 on claude/sonnet with tools: read, edit, finish. You will be \
         notified when it finishes.",
    );
    let face = d.result(&call, &ok, None);
    assert_eq!(
        face.outcome.as_deref(),
        Some("started · read, edit, finish")
    );
    assert_eq!(
        face.body,
        FaceBody::Lines(vec!["do X".into(), "grants  read edit finish".into()])
    );
    // §7.3's `w1 · env/profile`, only known once the service names them.
    assert_eq!(face.target.as_deref(), Some("w1 · claude/sonnet"));
}

#[test]
fn worker_start_error_is_bounded() {
    let d = describer();
    let call = call(
        "worker_start",
        r#"{"environment":"claude","task":"x","tools":["read"]}"#,
    );
    let error = result(
        "worker_start",
        ToolStatus::Error,
        "Cannot start worker: the worker service has shut down.",
    );
    let face = d.result(&call, &error, None);
    assert_eq!(
        face.outcome.as_deref(),
        Some("Cannot start worker: the worker service…")
    );
}

// ---------------------------------------------------------- worker_continue

#[test]
fn worker_continue_call_names_the_id_and_added_tools() {
    let d = describer();
    assert_eq!(
        d.call(&call(
            "worker_continue",
            r#"{"id":"w1","message":"go on","add_tools":["edit"]}"#
        ))
        .target,
        "w1 +edit"
    );
    assert_eq!(
        d.call(&call("worker_continue", r#"{"id":"w1","message":"go on"}"#))
            .target,
        "w1"
    );
}

#[test]
fn worker_continue_ok_names_added_tools_from_the_call() {
    let d = describer();
    let with_tools = call(
        "worker_continue",
        r#"{"id":"w1","message":"go on","add_tools":["edit"]}"#,
    );
    let ok = result(
        "worker_continue",
        ToolStatus::Ok,
        "Added tools: edit. Message sent to worker w1.",
    );
    assert_eq!(
        d.result(&with_tools, &ok, None).outcome.as_deref(),
        Some("resumed · +edit")
    );

    let plain = call("worker_continue", r#"{"id":"w1","message":"go on"}"#);
    let ok = result("worker_continue", ToolStatus::Ok, "Worker w1 continues.");
    assert_eq!(
        d.result(&plain, &ok, None).outcome.as_deref(),
        Some("resumed")
    );
}

#[test]
fn worker_continue_error_is_the_bounded_first_line() {
    let d = describer();
    let call = call("worker_continue", r#"{"id":"w9","message":"go on"}"#);
    let error = result("worker_continue", ToolStatus::Error, "No worker w9.");
    assert_eq!(
        d.result(&call, &error, None).outcome.as_deref(),
        Some("No worker w9.")
    );
}

// ----------------------------------------------------------- worker_result

#[test]
fn worker_result_call_names_the_id() {
    let face = describer().call(&call("worker_result", r#"{"id":"w1"}"#));
    assert_eq!(face.target, "w1");
    assert_eq!(face.kind, TargetKind::Plain);
}

#[test]
fn worker_result_running_and_cancelled() {
    let d = describer();
    let call = call("worker_result", r#"{"id":"w1"}"#);
    let running = result("worker_result", ToolStatus::Ok, "Worker w1: running");
    assert_eq!(
        d.result(&call, &running, None).outcome.as_deref(),
        Some("running · 1 lines")
    );
    let cancelled = result("worker_result", ToolStatus::Ok, "Worker w1: cancelled");
    assert_eq!(
        d.result(&call, &cancelled, None).outcome.as_deref(),
        Some("cancelled · 1 lines")
    );
}

#[test]
fn worker_result_finished_reads_the_finish_status_and_the_report_body() {
    let d = describer();
    let call = call("worker_result", r#"{"id":"w1"}"#);
    let content = "tools: read, finish\nfinish: done\n---\nWorker w1: finished\n\nAll good.";
    let ok = result("worker_result", ToolStatus::Ok, content);
    let face = d.result(&call, &ok, None);
    assert_eq!(face.outcome.as_deref(), Some("done · 6 lines"));
    assert_eq!(
        face.body,
        FaceBody::Lines(vec!["tools: read, finish".into(), "finish: done".into()])
    );
}

#[test]
fn worker_result_blocked_finish_status_stops_at_the_em_dash() {
    let d = describer();
    let call = call("worker_result", r#"{"id":"w1"}"#);
    let content = "tools: read\nfinish: blocked — needs: edit\n---\nWorker w1: finished\n\nstuck";
    let ok = result("worker_result", ToolStatus::Ok, content);
    assert_eq!(
        d.result(&call, &ok, None).outcome.as_deref(),
        Some("blocked · 6 lines")
    );
}

#[test]
fn worker_result_error_is_the_bounded_first_line() {
    let d = describer();
    let call = call("worker_result", r#"{"id":"w9"}"#);
    let error = result("worker_result", ToolStatus::Error, "No worker w9.");
    assert_eq!(
        d.result(&call, &error, None).outcome.as_deref(),
        Some("No worker w9.")
    );
}

// ----------------------------------------------------------- worker_cancel

#[test]
fn worker_cancel_call_names_the_id_ok_is_cancelled_error_is_bounded() {
    let d = describer();
    let face = d.call(&call("worker_cancel", r#"{"id":"w1"}"#));
    assert_eq!(face.target, "w1");
    let call = call("worker_cancel", r#"{"id":"w1"}"#);
    let ok = result("worker_cancel", ToolStatus::Ok, "Worker w1 cancelled.");
    assert_eq!(
        d.result(&call, &ok, None).outcome.as_deref(),
        Some("cancelled")
    );
    let error = result("worker_cancel", ToolStatus::Error, "No worker w9.");
    assert_eq!(
        d.result(&call, &error, None).outcome.as_deref(),
        Some("No worker w9.")
    );
}

// -------------------------------------------------------------- unknown tool

#[test]
fn an_unknown_tool_falls_back_to_the_generic_describer() {
    let d = describer();
    let generic = p1_tui::face::GenericDescriber;
    let call = call("some_future_tool", r#"{"x":"y"}"#);
    assert_eq!(d.call(&call), generic.call(&call));
    let ok = result("some_future_tool", ToolStatus::Ok, "did the thing");
    assert_eq!(d.result(&call, &ok, None), generic.result(&call, &ok, None));
}

// ------------------------------------------------------ statuses with no facts

#[test]
fn denied_cancelled_unknown_unavailable_carry_no_host_fact() {
    let d = describer();
    let call = call("shell", r#"{"command":"rm -rf /"}"#);
    for status in [
        ToolStatus::Denied,
        ToolStatus::Cancelled,
        ToolStatus::Unknown,
        ToolStatus::Unavailable,
    ] {
        let face = d.result(&call, &result("shell", status, ""), None);
        assert_eq!(
            face,
            ResultFace {
                outcome: None,
                body: FaceBody::None,
                meta: None,
                target: None,
            }
        );
    }
}
